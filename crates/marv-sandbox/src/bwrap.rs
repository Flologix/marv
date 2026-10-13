//! The bubblewrap (bwrap) argument builder: the same Linux kernel features containers use (namespaces), without a
//! container runtime.
//!
//! Ports `sandboxArgs`, `runMounts`, `resolveExtra` and `sandboxAvailable` from Marv's `src/sandbox.ts`, plus two
//! marv-rs additions: protected paths mounted read-only (`docs/SECURITY.md`) and the mesh plug at
//! [`MESH_SOCKET`] (`docs/MESH.md`).
//!
//! Inside it:
//! - the whole system is visible but read-only; `/run` is an empty folder (it holds the host's sockets: the user's
//!   D-Bus let `systemd-run --user` start programs outside the sandbox, plus the GPG/SSH agents, keyring, display and
//!   docker; a read-only mount doesn't stop a `connect()`). Only `PATH` folders under `/run` (NixOS, fnm) and, with
//!   network, the DNS config `/etc/resolv.conf` leads to are shown again, read-only;
//! - the home folder is a stand-in (see [`crate::home`]);
//! - the project is the only writable place (plus a private, RAM-backed `/tmp`), with protected paths read-only;
//! - bun's download cache is a throwaway one in that `/tmp` (`BUN_INSTALL_CACHE_DIR=/tmp/bun-cache`): a shared or
//!   host cache would let one sandboxed command (an install's lifecycle scripts) plant packages for another context;
//! - no network unless the call asked (`--unshare-net`); known limit: with network the sandbox shares the host's
//!   network namespace, so abstract Unix sockets (X11's) are reachable, which is why such calls always ask;
//! - a cleared environment (no API keys), its own PID namespace (killing it kills everything it started),
//!   `--new-session` (it can't inject keystrokes into our terminal), `--die-with-parent`.
//!
//! Order matters: later mounts sit on top of earlier ones.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::SandboxError;
use crate::home::{CREDENTIALS, TOOLCHAINS, contains, credential_masks};

/// Where the mesh plug's socket appears inside the sandbox: the one thing in the otherwise empty `/run`.
pub const MESH_SOCKET: &str = "/run/marv/mesh.sock";

/// What a path itself is, not following a final symlink (`lstatKind`). Folders and special files are `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    File,
    Symlink,
}

/// The file system, injectable for tests.
pub trait FsProbe {
    /// Whether a path exists (following symlinks).
    fn exists(&self, path: &Path) -> bool;
    /// What the path itself is.
    fn kind(&self, path: &Path) -> Option<FileKind>;
    /// Where it really leads (an error if it leads nowhere).
    fn realpath(&self, path: &Path) -> io::Result<PathBuf>;
}

/// The real file system.
pub struct RealFs;

impl FsProbe for RealFs {
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
    fn kind(&self, path: &Path) -> Option<FileKind> {
        let meta = std::fs::symlink_metadata(path).ok()?;
        if meta.file_type().is_symlink() {
            Some(FileKind::Symlink)
        } else if meta.is_file() {
            Some(FileKind::File)
        } else {
            None
        }
    }
    fn realpath(&self, path: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }
}

/// One bwrap invocation's mounts.
#[derive(Debug, Clone, Default)]
pub struct SandboxOptions {
    pub root: PathBuf,
    pub home: PathBuf,
    pub network: bool,
    /// PATH inside the sandbox.
    pub path: String,
    /// LANG inside the sandbox (`C.UTF-8` when empty).
    pub lang: String,
    /// Extra folders the command may read but not change: a worktree's shared `.git` (so git status/diff/log work),
    /// or the worktree around a subagent working in one of its subfolders (mounted *before* the project, which stays
    /// writable on top).
    pub read_only: Vec<PathBuf>,
    /// Extra folders outside the project the command may change, for an approved command: a git worktree's record
    /// and its repository's `.git` (in the hidden home), so an approved `git commit` works there.
    pub writable: Vec<PathBuf>,
    /// Paths inside the project that don't exist yet and must stay that way: each gets an empty read-only folder
    /// (a project without `.git`, so a command can't create a repository whose config git would run outside). bwrap
    /// leaves the empty mount point behind; the caller removes it.
    pub placeholders: Vec<PathBuf>,
    /// The project read-only too: Marv's own checks, which run the project's code without asking.
    pub project_read_only: bool,
    /// Protected paths inside the project, mounted read-only on top of it (marv-rs).
    pub protected: Vec<PathBuf>,
    /// The mesh plug's host socket, bound at [`MESH_SOCKET`] (marv-rs).
    pub mesh_socket: Option<PathBuf>,
}

/// The real path of an extra folder after checking that mounting it is safe, and whether it holds the project (then
/// it's mounted before the project). bwrap follows symlinks, so the check must too.
fn resolve_extra(dir: &Path, home: &Path, root: &Path, fs: &dyn FsProbe) -> Result<(PathBuf, bool), SandboxError> {
    if !dir.is_absolute() {
        return Err(SandboxError::Mount(format!("Extra sandbox folder {} must be an absolute path.", dir.display())));
    }
    let real =
        fs.realpath(dir).map_err(|_| SandboxError::Mount(format!("Extra sandbox folder {} doesn't exist.", dir.display())))?;
    let real_home = fs.realpath(home).unwrap_or_else(|_| home.to_path_buf());
    let real_root = fs.realpath(root).unwrap_or_else(|_| root.to_path_buf());
    // Mounting the home folder or a parent of it (even read-only) would expose the hidden home, with the API key in it.
    if real == Path::new("/") || contains(&real, &real_home) {
        return Err(SandboxError::Mount(format!(
            "Refusing to make {} visible in the sandbox: it is, or leads to, the home folder or one of its parents.",
            dir.display()
        )));
    }
    // Read-only over the project itself would leave nothing writable.
    if real == real_root {
        return Err(SandboxError::Mount(format!(
            "Refusing to make {} read-only in the sandbox: it is the project folder.",
            dir.display()
        )));
    }
    let around = contains(&real, &real_root);
    Ok((real, around))
}

/// What the sandbox keeps of `/run`: an empty tmpfs, then PATH folders under it (never `/run`, `/run/user` or a
/// user's whole folder, even if PATH names one: that's the sockets again) and, with network, the DNS config.
fn run_mounts(path: &str, network: bool, fs: &dyn FsProbe) -> Vec<OsString> {
    let is_socket_root = |dir: &str| {
        let parts: Vec<&str> = dir.trim_end_matches('/').split('/').collect();
        // ["", "run"], ["", "run", "user"], ["", "run", "user", "<uid>"]
        parts.len() == 2 || (parts.get(2) == Some(&"user") && parts.len() <= 4)
    };
    let mut keep: Vec<PathBuf> = path
        .split(':')
        .filter(|dir| dir.starts_with("/run/") && !is_socket_root(dir) && fs.exists(Path::new(dir)))
        .map(PathBuf::from)
        .collect();
    if network
        && let Ok(dns) = fs.realpath(Path::new("/etc/resolv.conf"))
        && dns.starts_with("/run/")
    {
        keep.push(dns);
    }
    let mut seen = Vec::new();
    let mut args: Vec<OsString> = vec!["--tmpfs".into(), "/run".into()];
    for p in keep {
        if !seen.contains(&p) {
            args.extend(["--ro-bind".into(), p.clone().into(), p.clone().into()]);
            seen.push(p);
        }
    }
    args
}

/// bwrap's arguments (everything before `-- command`).
pub fn sandbox_args(opts: &SandboxOptions, fs: &dyn FsProbe) -> Result<Vec<OsString>, SandboxError> {
    let root = opts.root.as_os_str();
    let home = opts.home.as_os_str();
    let mut args: Vec<OsString> = Vec::new();
    let mut push = |items: &[&std::ffi::OsStr]| args.extend(items.iter().map(|s| s.to_os_string()));
    push(&[
        "--ro-bind".as_ref(),
        "/".as_ref(),
        "/".as_ref(),
        "--dev".as_ref(),
        "/dev".as_ref(),
        "--proc".as_ref(),
        "/proc".as_ref(),
    ]);
    push(&["--tmpfs".as_ref(), "/tmp".as_ref(), "--tmpfs".as_ref(), home]);
    args.extend(run_mounts(&opts.path, opts.network, fs));
    // The mesh plug (marv-rs): after the empty /run, which would hide it otherwise. A read-only bind wouldn't stop
    // connect() anyway, and the plug only speaks the switchboard's protocol (docs/MESH.md).
    if let Some(socket) = &opts.mesh_socket {
        args.extend(["--bind".into(), socket.clone().into_os_string(), MESH_SOCKET.into()]);
    }
    let mounts: Vec<PathBuf> = TOOLCHAINS.iter().map(|dir| opts.home.join(dir)).filter(|full| fs.exists(full)).collect();
    for full in &mounts {
        args.extend(["--ro-bind".into(), full.clone().into_os_string(), full.clone().into_os_string()]);
    }
    // Bind the resolved path (what was checked), at the path the caller gave. A folder around the project goes
    // first: mounted after it, it would cover the project and silently turn it read-only.
    let extras = opts
        .read_only
        .iter()
        .map(|dir| resolve_extra(dir, &opts.home, &opts.root, fs).map(|(real, around)| (dir, real, around)))
        .collect::<Result<Vec<_>, _>>()?;
    for (dir, real, _) in extras.iter().filter(|e| e.2) {
        args.extend(["--ro-bind".into(), real.clone().into_os_string(), dir.as_os_str().to_os_string()]);
    }
    args.extend([if opts.project_read_only { "--ro-bind" } else { "--bind" }.into(), root.to_os_string(), root.to_os_string()]);
    for (dir, real, _) in extras.iter().filter(|e| !e.2) {
        args.extend(["--ro-bind".into(), real.clone().into_os_string(), dir.as_os_str().to_os_string()]);
    }
    for dir in &opts.writable {
        let (real, _) = resolve_extra(dir, &opts.home, &opts.root, fs)?;
        args.extend(["--bind".into(), real.into_os_string(), dir.as_os_str().to_os_string()]);
    }
    if !opts.project_read_only {
        // Protected paths (marv-rs): read-only on top of the writable project.
        for path in &opts.protected {
            args.extend(["--ro-bind".into(), path.clone().into_os_string(), path.clone().into_os_string()]);
        }
        // A read-only project can't get the empty mount point bwrap creates, and nothing in it can create a .git.
        for path in &opts.placeholders {
            args.extend(["--tmpfs".into(), path.clone().into_os_string(), "--remount-ro".into(), path.clone().into_os_string()]);
        }
    }
    // Last, on top of every other mount, so the empty file hides the real one and no later mount shows it again.
    let mut masks: Vec<PathBuf> = Vec::new();
    for file in CREDENTIALS {
        for mask in credential_masks(&opts.home.join(file), &mounts, fs) {
            if !masks.contains(&mask) {
                masks.push(mask);
            }
        }
    }
    for mask in masks {
        args.extend(["--ro-bind".into(), "/dev/null".into(), mask.into_os_string()]);
    }
    if !opts.network {
        args.push("--unshare-net".into());
    }
    let lang = if opts.lang.is_empty() { "C.UTF-8" } else { &opts.lang };
    let flags: [&std::ffi::OsStr; 4] =
        ["--unshare-pid".as_ref(), "--die-with-parent".as_ref(), "--new-session".as_ref(), "--clearenv".as_ref()];
    args.extend(flags.iter().map(|s| s.to_os_string()));
    for (key, value) in [
        ("PATH", opts.path.as_str()),
        ("HOME", &opts.home.to_string_lossy()),
        ("LANG", lang),
        ("TERM", "dumb"),
        ("TMPDIR", "/tmp"),
        // `bun install` needs a writable cache; not the user's, nor any cache that outlives the command (see above).
        ("BUN_INSTALL_CACHE_DIR", "/tmp/bun-cache"),
    ] {
        args.extend(["--setenv".into(), key.into(), value.into()]);
    }
    args.extend(["--chdir".into(), root.to_os_string()]);
    Ok(args)
}

/// Whether bwrap is installed and allowed to create namespaces here (checked once per process).
pub fn sandbox_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        std::process::Command::new("bwrap")
            .args(["--ro-bind", "/", "/", "--unshare-net", "--unshare-pid", "--", "true"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A fake file system: `exists` by predicate, `kind` by table, `realpath` by table (identity otherwise).
    struct Fake {
        exists: Box<dyn Fn(&str) -> bool>,
        kinds: HashMap<String, FileKind>,
        links: HashMap<String, String>,
        dangling: bool,
    }
    impl Fake {
        fn new(exists: impl Fn(&str) -> bool + 'static) -> Self {
            Self { exists: Box::new(exists), kinds: HashMap::new(), links: HashMap::new(), dangling: false }
        }
    }
    impl FsProbe for Fake {
        fn exists(&self, path: &Path) -> bool {
            (self.exists)(&path.to_string_lossy())
        }
        fn kind(&self, path: &Path) -> Option<FileKind> {
            self.kinds.get(&*path.to_string_lossy()).copied()
        }
        fn realpath(&self, path: &Path) -> io::Result<PathBuf> {
            let p = path.to_string_lossy();
            if self.dangling && self.kinds.get(&*p) == Some(&FileKind::Symlink) {
                return Err(io::Error::from(io::ErrorKind::NotFound));
            }
            Ok(PathBuf::from(self.links.get(&*p).cloned().unwrap_or_else(|| p.into_owned())))
        }
    }

    fn base() -> SandboxOptions {
        SandboxOptions { root: "/home/me/proj".into(), home: "/home/me".into(), path: "/usr/bin".into(), ..Default::default() }
    }
    fn default_fs() -> Fake {
        Fake::new(|p| p.ends_with(".bun"))
    }
    fn joined(opts: &SandboxOptions, fs: &dyn FsProbe) -> String {
        strings(opts, fs).join(" ")
    }
    fn strings(opts: &SandboxOptions, fs: &dyn FsProbe) -> Vec<String> {
        sandbox_args(opts, fs).unwrap().into_iter().map(|s| s.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn read_only_system_hidden_home_writable_project_no_network_clean_env() {
        let args = joined(&base(), &default_fs());
        assert!(args.contains("--ro-bind / /"));
        assert!(args.contains("--tmpfs /home/me")); // home hidden…
        assert!(args.contains("--ro-bind /home/me/.bun /home/me/.bun")); // …except toolchains, read-only
        assert!(!args.contains(".cargo")); // only the ones that exist
        assert!(args.contains("--bind /home/me/proj /home/me/proj"));
        assert!(args.contains("--unshare-net"));
        assert!(args.contains("--clearenv"));
        assert!(args.contains("--chdir /home/me/proj"));
    }

    #[test]
    fn run_is_empty_with_path_folders_and_dns_shown_again() {
        let mut fs = Fake::new(|p| p.starts_with("/run/") || p.ends_with(".bun"));
        fs.links.insert("/etc/resolv.conf".into(), "/run/systemd/resolve/stub-resolv.conf".into());
        let path = "/run/current-system/sw/bin:/usr/bin:/run/user/1000/fnm_multishells/42/bin:/run/user/1000";
        let offline = joined(&SandboxOptions { path: path.into(), ..base() }, &fs);
        assert!(offline.contains("--tmpfs /run"));
        assert!(offline.contains("--ro-bind /run/current-system/sw/bin /run/current-system/sw/bin"));
        assert!(offline.contains("--ro-bind /run/user/1000/fnm_multishells/42/bin /run/user/1000/fnm_multishells/42/bin"));
        assert!(!offline.contains("--ro-bind /run/user/1000 ")); // a user's whole folder: the sockets again
        assert!(!offline.contains("resolv")); // no network, no DNS
        let online = joined(&SandboxOptions { path: path.into(), network: true, ..base() }, &fs);
        assert!(online.contains("--ro-bind /run/systemd/resolve/stub-resolv.conf /run/systemd/resolve/stub-resolv.conf"));
        // The empty /run comes first: mounted after them, it would hide what it's meant to let through.
        assert!(online.find("--tmpfs /run").unwrap() < online.find("--ro-bind /run/systemd").unwrap());
    }

    #[test]
    fn project_read_only_and_placeholders() {
        let ro = joined(
            &SandboxOptions { project_read_only: true, placeholders: vec!["/home/me/proj/.git".into()], ..base() },
            &default_fs(),
        );
        assert!(ro.contains("--ro-bind /home/me/proj /home/me/proj"));
        assert!(!ro.contains("--bind /home/me/proj /home/me/proj"));
        assert!(!ro.contains("--tmpfs /home/me/proj/.git")); // bwrap can't create them in a read-only project
        let rw = joined(&SandboxOptions { placeholders: vec!["/home/me/proj/.git".into()], ..base() }, &default_fs());
        assert!(rw.contains("--tmpfs /home/me/proj/.git --remount-ro /home/me/proj/.git"));
    }

    #[test]
    fn personal_skills_and_mise_are_mounted_the_rest_of_marv_is_not() {
        let args = joined(&base(), &Fake::new(|p| p.ends_with(".marv/skills") || p.ends_with(".local/share/mise")));
        assert!(args.contains("--ro-bind /home/me/.marv/skills /home/me/.marv/skills"));
        assert!(args.contains("--ro-bind /home/me/.local/share/mise /home/me/.local/share/mise"));
        assert!(!args.contains("/home/me/.marv /home/me/.marv"));
    }

    #[test]
    fn network_per_command_and_a_throwaway_bun_cache() {
        for network in [false, true] {
            let args = strings(&SandboxOptions { network, ..base() }, &default_fs());
            assert_eq!(args.contains(&"--unshare-net".to_string()), !network);
            assert!(args.join(" ").contains("--setenv BUN_INSTALL_CACHE_DIR /tmp/bun-cache"));
        }
    }

    #[test]
    fn credential_files_are_masked_last() {
        let secrets = ["/home/me/.cargo/credentials", "/home/me/.cargo/credentials.toml", "/home/me/.config/git/credentials"];
        let mut fs = Fake::new(move |p| p.ends_with(".cargo") || p.ends_with(".config/git") || secrets.contains(&p));
        for s in secrets {
            fs.kinds.insert(s.into(), FileKind::File);
        }
        let args = strings(&base(), &fs);
        let joined = args.join(" ");
        let project = args.iter().position(|a| a == "/home/me/proj").unwrap();
        for secret in secrets {
            assert!(joined.contains(&format!("--ro-bind /dev/null {secret}")));
            assert!(args.iter().rposition(|a| a == secret).unwrap() > project);
        }
        // Ones that don't exist aren't mounted (bwrap would have to create them).
        assert!(!self::joined(&base(), &Fake::new(|p| p.ends_with(".cargo"))).contains("/dev/null"));
    }

    #[test]
    fn symlinked_credentials_mask_the_target_where_a_toolchain_shows_it() {
        let exists = |p: &str| p.ends_with(".cargo") || p.ends_with(".config/git");
        // Pointing inside a mounted toolchain folder: the target is masked.
        let mut fs = Fake::new(exists);
        fs.kinds.insert("/home/me/.cargo/credentials".into(), FileKind::Symlink);
        fs.kinds.insert("/home/me/.cargo/credentials.toml".into(), FileKind::File);
        fs.links.insert("/home/me/.cargo/credentials".into(), "/home/me/.cargo/credentials.toml".into());
        let args = joined(&base(), &fs);
        assert!(args.contains("--ro-bind /dev/null /home/me/.cargo/credentials.toml"));
        assert!(!args.contains("/dev/null /home/me/.cargo/credentials "));
        // Into a toolchain folder that is itself a symlink: masked where the sandbox mounts it.
        let mut fs = Fake::new(exists);
        fs.kinds.insert("/home/me/.config/git/credentials".into(), FileKind::Symlink);
        fs.kinds.insert("/home/me/dotfiles/git/secret".into(), FileKind::File);
        fs.links.insert("/home/me/.config/git".into(), "/home/me/dotfiles/git".into());
        fs.links.insert("/home/me/.config/git/credentials".into(), "/home/me/dotfiles/git/secret".into());
        assert!(joined(&base(), &fs).contains("--ro-bind /dev/null /home/me/.config/git/secret"));
        // Pointing outside the mounted folders: skipped.
        let mut fs = Fake::new(exists);
        fs.kinds.insert("/home/me/.cargo/credentials.toml".into(), FileKind::Symlink);
        fs.kinds.insert("/home/me/dotfiles/cargo/credentials.toml".into(), FileKind::File);
        fs.links.insert("/home/me/.cargo/credentials.toml".into(), "/home/me/dotfiles/cargo/credentials.toml".into());
        assert!(!joined(&base(), &fs).contains("/dev/null"));
        // Dangling: skipped.
        let mut fs = Fake::new(exists);
        fs.kinds.insert("/home/me/.cargo/credentials".into(), FileKind::Symlink);
        fs.dangling = true;
        assert!(!joined(&base(), &fs).contains("/dev/null"));
    }

    #[test]
    fn extra_read_only_folders() {
        let fs = default_fs();
        // Mounted after the root, read-only.
        let args = joined(&SandboxOptions { read_only: vec!["/srv/repo/.git".into()], ..base() }, &fs);
        assert!(args.find("--bind /home/me/proj").unwrap() < args.find("--ro-bind /srv/repo/.git /srv/repo/.git").unwrap());
        // Around the project: before it, so the project stays writable.
        let args = joined(&SandboxOptions { root: "/srv/tree/pkg".into(), read_only: vec!["/srv/tree".into()], ..base() }, &fs);
        assert!(args.find("--ro-bind /srv/tree /srv/tree").unwrap() < args.find("--bind /srv/tree/pkg").unwrap());
        // The home folder, a parent of it, /, or the project itself: refused.
        for bad in ["/home/me", "/home", "/", "/home/me/proj"] {
            assert!(sandbox_args(&SandboxOptions { read_only: vec![bad.into()], ..base() }, &fs).is_err(), "{bad}");
        }
        assert!(sandbox_args(&SandboxOptions { read_only: vec!["relative".into()], ..base() }, &fs).is_err());
    }

    #[test]
    fn protected_paths_are_read_only_on_top_of_the_project_and_the_mesh_plug_is_in_run() {
        let opts = SandboxOptions {
            protected: vec!["/home/me/proj/.envrc".into()],
            mesh_socket: Some("/run/user/1000/marv/mesh-1.sock".into()),
            ..base()
        };
        let args = joined(&opts, &default_fs());
        assert!(args.find("--bind /home/me/proj").unwrap() < args.find("--ro-bind /home/me/proj/.envrc").unwrap());
        assert!(
            args.find("--tmpfs /run").unwrap()
                < args.find(&format!("--bind /run/user/1000/marv/mesh-1.sock {MESH_SOCKET}")).unwrap()
        );
    }
}
