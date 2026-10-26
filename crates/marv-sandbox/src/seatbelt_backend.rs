//! The Seatbelt backend (#166, `docs/specs/2026-10-17-macos-sandbox.md` §6.10, §6.11): macOS's `sandbox-exec`, with
//! the profile of [`crate::seatbelt::profile`], running `marv __guard` around each command. New in marv-rs (Marv had
//! only bubblewrap).
//!
//! One command is:
//!
//! ```text
//! /usr/bin/sandbox-exec -p <profile> -D KEY=VALUE… -- <marv> __guard <marv's pid> -- bash -c <script>
//! ```
//!
//! spawned with the plan's environment only (`sandbox-exec` passes its own on), in the project.
//!
//! - **A private `TMPDIR` per command**, made with `mkdtemp` (0700) in the user's own temporary folder (`…/T/`, found
//!   with `getconf DARWIN_USER_TEMP_DIR`, never from `TMPDIR`, which the environment could point anywhere), and
//!   removed after the command ([`SandboxBackend::after`]). The profile refuses any other `TMPDIR`. `/tmp` is shared
//!   with the host on macOS, so it's closed (spec §11.2).
//! - **The guard** (`marv_tools::guard`) is what `sandbox-exec` execs, so the pid Marv spawned is the guard's, which
//!   leads a process group of its own. [`SandboxBackend::kill`] is `killpg(pid, SIGSTOP)`, then `SIGKILL`: the guard,
//!   the command and everything left in the group (stopped first, as macOS signals a group one member at a time).
//!   [`SandboxBackend::terminate`] is SIGTERM to the guard alone, which passes it on to the group (a
//!   `killpg(SIGTERM)` would reach the command twice). There's no PID namespace: after the guard has
//!   exited (before it's reaped, so its pid can't be reused) the caller sweeps the group ([`SandboxBackend::sweep`]:
//!   killed again and again), for what the command left running in the background (bwrap's namespace took those),
//!   and, for seconds after a timeout or a stop, for a process that was being created as that kill landed (#247).
//! - **Availability** is probed once per process ([`Seatbelt::available`]): the real base rules on `sandbox-exec`, and
//!   the guard echoing a line back. With no `sandbox-exec`, a profile the kernel no longer accepts, a Marv that's
//!   already inside a Seatbelt sandbox, or a binary that doesn't answer `__guard` (a test harness), commands run
//!   unsandboxed, with the warning, as on Linux without bwrap (spec §11.7).
//! - **The process door isn't offered** ([`SandboxBackend::processes`], #152); a plan with the relay's binds is
//!   refused by the profile builder.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::SandboxError;
use crate::backend::{SandboxBackend, SandboxCommand};
use crate::fs::FsProbe;
use crate::home::TOOLCHAINS;
use crate::plan::{Bind, Plan};
use crate::seatbelt::{Invocation, Profile, SANDBOX_EXEC, SHARED_TMP, probe_profile, profile};

/// The guard's hidden subcommand, as `sandbox-exec` runs it (`marv __guard <parent-pid> -- <command…>`).
pub const GUARD_SUBCOMMAND: &str = "__guard";

/// The private `TMPDIR`'s name in the user's `T/` folder (`mkdtemp` fills in the `X`s).
pub const TMP_PREFIX: &str = "marv-cmd-";

/// A profile longer than this goes to `sandbox-exec` as a file (`-f`) in the private `TMPDIR`, not as `-p`: one rule
/// per protected path can grow, and an argument list is limited (`ARG_MAX`, 1 MB). A 300 KB `-p` profile still
/// worked on macOS 14 (#166 probe), so this leaves room.
pub const PROFILE_FILE_OVER: usize = 256 * 1024;

/// The binary that answers [`GUARD_SUBCOMMAND`], when it isn't this one (tests: a fixture binary).
static GUARD_EXE: OnceLock<PathBuf> = OnceLock::new();

/// Tests: the binary that answers `__guard` instead of the test binary itself. Call it before anything asks whether
/// the sandbox is available; the first call wins.
pub fn set_guard_exe(path: impl Into<PathBuf>) {
    let path = path.into();
    let _ = GUARD_EXE.set(std::fs::canonicalize(&path).unwrap_or(path));
}

/// The binary `sandbox-exec` runs as the guard, by its real path: [`set_guard_exe`]'s, else Marv's own, resolved once
/// per run. So the answer can't change mid-session (Marv's binary replaced or removed by an upgrade), and a command's
/// preview and its run agree on whether it's sandboxed (#166 review).
pub fn guard_exe() -> Option<PathBuf> {
    static OWN: OnceLock<Option<PathBuf>> = OnceLock::new();
    GUARD_EXE
        .get()
        .cloned()
        .or_else(|| OWN.get_or_init(|| std::env::current_exe().ok().and_then(|e| std::fs::canonicalize(e).ok())).clone())
}

/// The user's own temporary folder, `/private/var/folders/<xx>/<id>/T`, by its real path: `confstr`'s
/// `_CS_DARWIN_USER_TEMP_DIR`, through `getconf` (no `unsafe`), asked once. Not `TMPDIR`: Marv's environment can name
/// any folder, and the profile closes this one and opens only the private `TMPDIR` in it. None where there's none
/// (not macOS, or an answer of another shape).
pub fn user_temp_dir() -> Option<PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let out = std::process::Command::new("/usr/bin/getconf")
            .arg("DARWIN_USER_TEMP_DIR")
            .env_clear()
            .stdin(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())?;
        let dir = std::fs::canonicalize(String::from_utf8(out.stdout).ok()?.trim()).ok()?;
        user_temp_shape(&dir).then_some(dir)
    })
    .clone()
}

/// `/private/var/folders/<xx>/<id>/T`, exactly.
fn user_temp_shape(dir: &Path) -> bool {
    dir.starts_with("/private/var/folders") && dir.components().count() == 7 && dir.file_name().is_some_and(|n| n == "T")
}

/// A new private `TMPDIR` in `t` (the user's `T/` folder): `mkdtemp`, so it's new, ours and 0700.
pub fn make_private_tmp(t: &Path) -> std::io::Result<PathBuf> {
    nix::unistd::mkdtemp(&t.join(format!("{TMP_PREFIX}XXXXXX"))).map_err(std::io::Error::from)
}

/// Whether `path` is a private `TMPDIR` this backend made: a `marv-cmd-*` folder right in the user's `T/`. Only such
/// a folder is ever removed by [`SandboxBackend::after`].
fn is_private_tmp(path: &Path, t: &Path) -> bool {
    path.parent() == Some(t) && path.file_name().is_some_and(|n| n.to_string_lossy().starts_with(TMP_PREFIX))
}

/// `sandbox-exec`'s whole argv: the profile (`-p`, or `-f` with `file` when it's in a file), its parameters, then the
/// guard around `inner`.
pub fn argv(profile: &Profile, file: Option<&Path>, exe: &Path, parent: u32, inner: &[OsString]) -> Vec<OsString> {
    let mut argv: Vec<OsString> = vec![SANDBOX_EXEC.into()];
    let mut args = profile.args();
    if let Some(file) = file {
        args.splice(0..2, ["-f".into(), file.as_os_str().to_os_string()]);
    }
    argv.extend(args);
    argv.extend(["--".into(), exe.as_os_str().to_os_string(), GUARD_SUBCOMMAND.into(), parent.to_string().into(), "--".into()]);
    argv.extend(inner.iter().cloned());
    argv
}

/// The caches a command's tools would keep under the home, which Seatbelt can't make writable (bwrap's stand-in home
/// was): each in the command's own `TMPDIR`, gone with it. Not shared across commands, for the reason bun's isn't
/// (`Plan::env`): one command must not plant packages for another.
pub const CACHE_VARS: &[(&str, &str)] = &[
    ("XDG_CACHE_HOME", "cache"),
    ("GOCACHE", "go-cache"),
    ("PIP_CACHE_DIR", "pip-cache"),
    ("npm_config_cache", "npm-cache"),
    // pnpm's store, `~/Library/pnpm/store` by default (#166 probe).
    ("npm_config_store_dir", "pnpm-store"),
];

/// The Seatbelt backend: see the module doc.
#[derive(Debug)]
pub struct Seatbelt;

impl Seatbelt {
    /// The availability check itself (uncached) for the guard binary `exe`: see the module doc.
    fn probe(exe: &Path) -> bool {
        if !Path::new(SANDBOX_EXEC).exists() {
            return false;
        }
        const TOKEN: &str = "marv-seatbelt-ok";
        let out = std::process::Command::new(SANDBOX_EXEC)
            .args(["-p", &probe_profile(), "--"])
            .arg(exe)
            .args([GUARD_SUBCOMMAND, &std::process::id().to_string(), "--", "/bin/echo", TOKEN])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .current_dir("/")
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output();
        out.is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == TOKEN)
    }
}

impl SandboxBackend for Seatbelt {
    fn name(&self) -> &'static str {
        "Seatbelt"
    }

    /// Probed once per guard binary: a test that sets the guards' fixture ([`set_guard_exe`]) after something else
    /// asked gets its own answer, not the test harness's.
    fn available(&self) -> bool {
        static AVAILABLE: std::sync::Mutex<Vec<(PathBuf, bool)>> = std::sync::Mutex::new(Vec::new());
        let Some(exe) = guard_exe() else { return false };
        let mut known = AVAILABLE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, ok)) = known.iter().find(|(e, _)| *e == exe) {
            return *ok;
        }
        let ok = Self::probe(&exe);
        known.push((exe, ok));
        ok
    }

    fn command(&self, plan: &Plan, inner: &[OsString], fs: &dyn FsProbe) -> Result<SandboxCommand, SandboxError> {
        let t = user_temp_dir().ok_or_else(|| {
            SandboxError::Mount("Couldn't find the user's temporary folder, so the macOS sandbox can't start.".into())
        })?;
        let user_temp = t.parent().map(Path::to_path_buf);
        let exe = guard_exe().ok_or_else(|| SandboxError::Mount("Couldn't find Marv's own binary for the sandbox.".into()))?;
        let tmp = make_private_tmp(&t)
            .map_err(|e| SandboxError::Mount(format!("Couldn't make the command's temporary folder in {}: {e}", t.display())))?;
        let built = (|| {
            let run = Invocation { tmp: tmp.clone(), user_temp, exe: Some(exe.clone()) };
            let profile = profile(plan, &run, fs)?;
            let file = if profile.text.len() > PROFILE_FILE_OVER {
                let file = tmp.join("sandbox.sb");
                std::fs::write(&file, &profile.text)?;
                Some(file)
            } else {
                None
            };
            Ok::<_, SandboxError>(argv(&profile, file.as_deref(), &exe, std::process::id(), inner))
        })();
        let argv = match built {
            Ok(argv) => argv,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&tmp);
                return Err(e);
            }
        };
        let env = plan.env(&tmp.to_string_lossy(), self).into_iter().map(|(k, v)| (k.into(), v.into())).collect();
        Ok(SandboxCommand { argv, env, leftovers: vec![tmp] })
    }

    /// Seatbelt can't mount: a path is where it is on the host.
    fn guest_path(&self, bind: &Bind) -> PathBuf {
        bind.source.clone()
    }

    /// What every profile closes for reads and Unix-socket connects, whatever the project: the shared `/private/tmp`
    /// and the user's temporary folder (`/private/var/folders/<xx>/<id>`), and in the home its hidden top-level entries
    /// (but the toolchains, shown again) and `~/Library`, which even a home-folder project doesn't show.
    fn hides(&self, dir: &Path, home: &Path) -> bool {
        seatbelt_hides(dir, home, user_temp_dir().as_deref().and_then(Path::parent))
    }

    /// The guard's group killed ([`crate::backend::kill_group`]: stopped first, so no member can run on while the
    /// kill goes round, #247), then `kill(pid, SIGKILL)`. The group exists only once the guard has called `setsid`;
    /// before that (while `sandbox-exec` compiles the profile and execs the guard) a `killpg` finds nothing, and the
    /// guard would go on to start the command: the pid kill ends it. The group goes first so the guard doesn't die
    /// before the rest is stopped (its group, orphaned, would get SIGCONT). `pid` is an unreaped child of Marv's, so
    /// no signal can reach another process.
    fn kill(&self, pid: u32) {
        crate::backend::kill_group(pid);
        crate::backend::send(pid, nix::sys::signal::Signal::SIGKILL);
    }

    /// The guard has exited (a zombie, so its group's id is still its own): its group is killed again and again
    /// ([`crate::backend::kill_group_for`]), for [`crate::backend::GROUP_SWEEP`], or [`crate::backend::GROUP_LINGER`]
    /// after a kill (#247): a process being forked or spawned as the kill landed (the command, by the guard; a job, by
    /// the command's shell) is created after its parent has died, and only a group kill sent after that reaches it.
    fn sweep(&self, pid: u32, killed: bool, held: &std::sync::atomic::AtomicBool) {
        let window = if killed { crate::backend::GROUP_LINGER } else { crate::backend::GROUP_SWEEP };
        crate::backend::kill_group_for(pid, window, held);
    }

    fn lingers(&self) -> bool {
        true
    }

    /// SIGTERM to the guard alone: it passes it on to its group (sent to the group, the command would get it twice).
    fn terminate(&self, pid: u32) {
        crate::backend::send(pid, nix::sys::signal::Signal::SIGTERM);
    }

    /// Removes the private `TMPDIR` (only a folder this backend made: `marv-cmd-*` right in the user's `T/`).
    fn after(&self, command: &SandboxCommand) {
        let Some(t) = user_temp_dir() else { return };
        for path in command.leftovers.iter().filter(|p| is_private_tmp(p, &t)) {
            let _ = std::fs::remove_dir_all(path);
        }
    }

    fn extra_env(&self, tmp: &str) -> Vec<(String, String)> {
        CACHE_VARS.iter().map(|(k, dir)| (k.to_string(), format!("{tmp}/{dir}"))).collect()
    }

    fn processes(&self) -> bool {
        false
    }
}

/// [`Seatbelt::hides`], with the user's temporary folder given (None: not on macOS).
fn seatbelt_hides(dir: &Path, home: &Path, user_temp: Option<&Path>) -> bool {
    let dir = crate::real_or_planned(dir);
    if !dir.is_absolute() || dir.components().any(|c| c == std::path::Component::ParentDir) {
        return false;
    }
    let home = crate::real_or_planned(home);
    let under = |base: &Path| dir.starts_with(base) && dir != base;
    if under(Path::new(SHARED_TMP)) || user_temp.is_some_and(under) {
        return true;
    }
    if home == Path::new("/") {
        return false;
    }
    let first = dir
        .strip_prefix(&home)
        .ok()
        .and_then(|rel| rel.components().next())
        .map(|c| c.as_os_str().to_string_lossy().into_owned());
    match first {
        Some(first) if first == "Library" => true,
        Some(first) if first.starts_with('.') => !TOOLCHAINS.iter().any(|t| dir.starts_with(home.join(t))),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_guard_runs_inside_sandbox_exec_around_the_command() {
        let profile = Profile { text: "(version 1)".into(), params: vec![("TMP".into(), "/t".into())] };
        let inner: Vec<OsString> = vec!["bash".into(), "-c".into(), "echo hi".into()];
        let argv = argv(&profile, None, Path::new("/opt/marv"), 4242, &inner);
        let argv: Vec<&str> = argv.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            argv,
            [
                SANDBOX_EXEC,
                "-p",
                "(version 1)",
                "-D",
                "TMP=/t",
                "--",
                "/opt/marv",
                "__guard",
                "4242",
                "--",
                "bash",
                "-c",
                "echo hi"
            ]
        );
        // A long profile goes in a file, its parameters still on the command line.
        let argv = argv_strings(&profile, Some(Path::new("/t/sandbox.sb")));
        assert_eq!(&argv[..5], [SANDBOX_EXEC, "-f", "/t/sandbox.sb", "-D", "TMP=/t"]);
    }

    fn argv_strings(profile: &Profile, file: Option<&Path>) -> Vec<String> {
        argv(profile, file, Path::new("/opt/marv"), 1, &[]).iter().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn only_a_private_tmpdir_it_made_is_removed() {
        let t = Path::new("/private/var/folders/ab/xyz/T");
        assert!(user_temp_shape(t));
        assert!(!user_temp_shape(Path::new("/private/var/folders/ab/xyz/C")));
        assert!(!user_temp_shape(Path::new("/tmp/T")));
        assert!(is_private_tmp(&t.join("marv-cmd-a1B2c3"), t));
        for not_ours in ["/private/var/folders/ab/xyz/T/other", "/private/var/folders/ab/xyz/T", "/Users/me/marv-cmd-1"] {
            assert!(!is_private_tmp(Path::new(not_ours), t), "{not_ours}");
        }
        assert!(!is_private_tmp(&t.join("marv-cmd-1/deeper"), t));
    }

    #[test]
    fn the_caches_go_to_the_private_tmpdir() {
        let env = Seatbelt.extra_env("/private/var/folders/ab/xyz/T/marv-cmd-1");
        for (key, _) in CACHE_VARS {
            let (_, value) = env.iter().find(|(k, _)| k == key).unwrap();
            assert!(value.starts_with("/private/var/folders/ab/xyz/T/marv-cmd-1/"), "{key}={value}");
        }
        let plan = Plan { root: "/Users/me/p".into(), home: "/Users/me".into(), ..Default::default() };
        let all = plan.env("/t", &Seatbelt);
        assert!(all.contains(&("XDG_CACHE_HOME".into(), "/t/cache".into())) && all.contains(&("TMPDIR".into(), "/t".into())));
    }

    #[test]
    fn it_hides_what_every_profile_closes() {
        let home = Path::new("/Users/me");
        let ut = Some(Path::new("/private/var/folders/ab/xyz"));
        for hidden in
            ["/private/tmp/marv-501", "/private/var/folders/ab/xyz/T/x", "/Users/me/.run", "/Users/me/Library/Caches/marv"]
        {
            assert!(seatbelt_hides(Path::new(hidden), home, ut), "{hidden}");
        }
        // /run isn't macOS's; a toolchain and the visible home show in some sandbox; the folders themselves aren't in.
        for shown in ["/run/user/501", "/Users/me/.cargo/run", "/Users/me/run", "/private/tmp", "/Users/me", "/srv/x", "rel"] {
            assert!(!seatbelt_hides(Path::new(shown), home, ut), "{shown}");
        }
        assert!(!seatbelt_hides(Path::new("/x/.y"), Path::new("/"), ut), "a home of / hides nothing");
        assert!(!seatbelt_hides(Path::new("/private/tmp/a/../../etc"), home, ut));
    }

    #[test]
    fn no_process_door_on_macos_yet() {
        assert!(!Seatbelt.processes());
        assert_eq!(Seatbelt.guest_path(&Bind { source: "/a".into(), dest: "/run/b".into(), writable: false }), Path::new("/a"));
    }
}
