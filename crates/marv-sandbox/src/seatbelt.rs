//! The Seatbelt profile builder (#164, `docs/specs/2026-10-17-macos-sandbox.md` §6): one [`Plan`] rendered as an SBPL
//! profile for macOS's `/usr/bin/sandbox-exec -p <profile> -D KEY=VALUE… -- command…`.
//!
//! Pure: a function of the plan, the per-command [`Invocation`] and a [`FsProbe`], so its tests run on Linux CI too.
//! Nothing here calls macOS, and it isn't the active backend yet: wiring it in (the private `TMPDIR`, `marv __guard`,
//! the availability probe) is #166, and [`crate::backend()`] still returns bubblewrap.
//!
//! Seatbelt has no mounts. Where bwrap *shows something else at a path* (an empty tmpfs, `/dev/null`, a read-only
//! bind), Seatbelt can only *refuse the path*. The profile is deny-default (§6.1: on macOS the dangerous doors are
//! Mach and XPC services, `open`, `osascript`, `launchctl`, the pasteboard, the keychain, and one `(deny default)`
//! closes them all), then reads are allowed for the whole disk and narrowed again, writes are allowed only where the
//! plan says, and the network is closed unless the call asked. The last matching rule wins, so the sections come in a
//! fixed order (§6.2), each overriding the ones before it:
//!
//! 1. **base**: `(deny default)`, fork and exec, signals and process info for this sandbox only, a short `sysctl-read`
//!    list without `kern.proc.all` (G20), two Mach services, POSIX semaphores, Unix and IP sockets may be created
//!    (connecting and binding are separate rules), the harmless devices. No `pseudo-tty` (G22);
//! 2. **reads, broad**: `file-read*` and `file-map-executable` everywhere (G1);
//! 3. **closed**: the home (only its own entry's metadata stays readable, so `cd ~` works), or in a home-folder
//!    project every hidden top-level entry of it by one regex, and `~/Library` (G6, G16); `/private/tmp` and the user's
//!    `/private/var/folders/<xx>/<id>` (G2);
//! 4. **shown again**: the project, the toolchains (G7), the read-only and writable extras (G8, G10), the private
//!    `TMPDIR`, Marv's binary;
//! 5. **closed again**: `HOME_MASKS` (home project), Marv's config folder (G15, its `skills/` shown again after it),
//!    Marv's runtime folders (G14), terminals (G22), each credential file as written and where it leads (G17);
//! 6. **writes**: the project unless read-only (G9), not the read-only extras inside it, the writable extras (G10),
//!    the private `TMPDIR`, `/dev/null`;
//! 7. **writes closed again**: the runtime and config folders, credentials, each protected path and placeholder as
//!    written and where it leads (G11, G12), the protected names at any depth as regexes, which also stop a command
//!    *creating* `.envrc` or `pkg/AGENTS.md` (G13), and unlinking an ancestor of a protected path (a rename of the
//!    parent can't carry the protected entry away); and no hard link to anything denied (bwrap's `EXDEV`);
//! 8. **network** (G18): nothing without it but the forwarded process ports on `localhost` and the mesh plug; with
//!    it, IP, DNS and the TLS-trust services. Unix sockets stay closed either way, except the plug and sockets in the
//!    private `TMPDIR`;
//! 9. **last**: no XPC service by any name, and two `fcntl`s that change a file through a read-only descriptor.
//!
//! **Paths.** Every path in a rule is canonical ([`canonical`]: `/tmp` → `/private/tmp`, `/var` → `/private/var`, the
//! user's own symlinks followed, a missing rest kept as written), because Seatbelt matches the resolved path. Paths
//! never enter the profile text: each is a `-D KEY=VALUE` parameter, read by `(param "KEY")`. Only the regexes need a
//! path in the text (`regex` takes no parameter): the project's and the home's, escaped for SBPL and the regex, and a
//! path with a control character or that isn't UTF-8 is refused (`SandboxError::Mount`), so such a command never
//! starts (G24).
//!
//! The rule shapes follow OpenAI Codex's Seatbelt policy (`codex-rs/sandboxing`, Apache-2.0), as the spec describes it:
//! deny-default base, full-disk reads narrowed, `literal` and `subpath` for protected paths, protected ancestors
//! unlinkable, XPC denied last. What Seatbelt can't match (§6.12) is listed in `docs/SECURITY.md`.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use crate::SandboxError;
use crate::fs::{EntryKind, FileKind, FsProbe};
#[cfg(test)]
use crate::home::HOME_MASKS;
use crate::home::{CREDENTIALS, contains};
use crate::plan::Plan;

/// `sandbox-exec`, always by its full path on the system volume, never from PATH.
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// The shared temporary folder every macOS process sees: denied, each command gets a private `TMPDIR` (§11.2).
pub const SHARED_TMP: &str = "/private/tmp";

/// Folders the protected-name rules don't reach into, as the after-scan doesn't (`protected::SKIPPED_DIRS`): an
/// install writes `node_modules/pkg/AGENTS.md` and that must work. Matched case-sensitively, as the walker matches
/// them: `Node_Modules/AGENTS.md` is still denied, which is stricter than the scan and intended.
const SKIPPED: &str = "(node_modules|target|\\.git)";

/// The sysctls any process reads (Codex's list, from Chromium's). Not `kern.proc.all` or `kern.proc.pid.`: listing or
/// inspecting the user's other processes is what G20 keeps out.
const SYSCTLS: &[&str] = &[
    "hw.activecpu",
    "hw.busfrequency_compat",
    "hw.byteorder",
    "hw.cacheconfig",
    "hw.cachelinesize_compat",
    "hw.cpufamily",
    "hw.cpufrequency_compat",
    "hw.cputype",
    "hw.l1dcachesize_compat",
    "hw.l1icachesize_compat",
    "hw.l2cachesize_compat",
    "hw.l3cachesize_compat",
    "hw.logicalcpu_max",
    "hw.machine",
    "hw.memsize",
    "hw.model",
    "hw.ncpu",
    "hw.nperflevels",
    "hw.packages",
    "hw.pagesize_compat",
    "hw.physicalcpu_max",
    "hw.tbfrequency_compat",
    "hw.vectorunit",
    "kern.hostname",
    "kern.maxfilesperproc",
    "kern.ngroups",
    "kern.osproductversion",
    "kern.osrelease",
    "kern.ostype",
    "kern.osvariant_status",
    "kern.osversion",
    "kern.secure_kernel",
    "kern.usrstack64",
    "kern.version",
    // Node's `os.cpus()` is empty without it (#166 probe), and `os.cpus().length` sizes worker pools.
    "machdep.cpu.brand_string",
];
/// `sysctl.` is the sysctl tree's own metadata (`name2oid`, `oidfmt`, `next`): the `sysctl -n hw.ncpu` that build
/// scripts run needs it to look a name up; the value is still filtered by the list above.
const SYSCTL_PREFIXES: &[&str] = &["hw.optional.arm.", "hw.optional.armv8_", "hw.perflevel", "sysctl."];

/// The interface list (`getifaddrs`: node's `os.networkInterfaces()`, Go's `net.Interfaces()`, Python's
/// `if_nameindex()`), with network only: it names the machine's addresses, as a networked command on Linux sees the
/// host's interfaces and one without network sees only `lo`. Without it those calls fail on macOS (#166 probe).
const NETWORK_SYSCTL_PREFIX: &str = "net.routetable.";

/// The Mach services every command may look up: user and group lookups (git, ssh) and the system log. Each one added
/// later is a review item, with a failing tool and a test. `opendirectoryd.libinfo` answers `getpwuid`/`getgrgid`
/// and friends: a command can learn the machine's user and group names and home folders (as `/etc/passwd` shows on
/// Linux), nothing more.
pub const MACH_SERVICES: &[&str] = &["com.apple.system.opendirectoryd.libinfo", "com.apple.system.logger"];

/// The services a command with network needs on top: DNS configuration and TLS trust (Codex's network policy, without
/// `com.apple.SecurityServer`: securityd answers for keychain items and raises authorization prompts; #166 review).
pub const NETWORK_MACH_SERVICES: &[&str] = &[
    "com.apple.SystemConfiguration.configd",
    "com.apple.SystemConfiguration.DNSConfiguration",
    "com.apple.trustd.agent",
    "com.apple.networkd",
    "com.apple.ocspd",
];

/// The DNS resolver's socket, reachable with network only.
const DNS_SOCKET: &str = "/private/var/run/mDNSResponder";

/// The operation that makes a hard link. A hard link is a second name for a file that no path rule names: a protected
/// file linked into the project or the private `TMPDIR` could be written through it, a credential read through it.
/// bwrap stops this with `EXDEV` (a link can't cross a mount); here each denied path is also denied as a link source.
/// The name is right, and `link()` is checked against **both** paths, the source and the new name: a rule naming
/// either refuses it (#166 probe on macOS 14: with `(deny file-link (subpath A))`, A→B, B→A and A→A are refused, B→B
/// isn't; with `(deny file-link)` then `(allow file-link (subpath A))` only A→A works).
const HARD_LINK_OP: &str = "file-link";

/// Whether hard links are allowed in the project and the private `TMPDIR` (rustc's incremental cache, `ln`). On since
/// the BLOCKING probe of #166 (spec §9.2): because `file-link` checks the source too, an allow for the project doesn't
/// let `ln ~/.ssh/known_hosts ./x` through (the home's deny names the source), nor `ln ~/.ssh/known_hosts $TMPDIR/x`,
/// nor `ln .envrc y` (the protected path's deny), nor a link from a readable file outside every allowed folder
/// (`/Users/Shared/x`, no allow names it). Both ends must be in the project or the private TMPDIR. (A link from the
/// project into the TMPDIR is refused too, for no reason the rules show; the other three directions work.)
const ALLOW_LINKS_IN_PROJECT: bool = true;

// A setuid or setgid program (`sudo`, `su`, `ps`) doesn't run in the sandbox at all: `sandbox-exec`'s exec of one
// fails with EPERM (#166 probe on macOS 14: `sudo -n true` and `/bin/ps` both "Operation not permitted"), so no rule
// is needed for it. The guards check it stays so.

/// What one command's profile needs beyond the [`Plan`]: things the macOS backend makes or finds per command (#166).
#[derive(Debug, Clone, Default)]
pub struct Invocation {
    /// The private `TMPDIR` the backend made for this command (0700, removed after it): readable and writable, and
    /// the one place a command may bind and connect Unix sockets.
    pub tmp: PathBuf,
    /// The user's own temporary folder, `/private/var/folders/<xx>/<id>` (`getconf DARWIN_USER_DIR`'s parent), denied
    /// like `/private/tmp`. None: taken from [`Invocation::tmp`] when that is inside it; otherwise the profile is
    /// refused rather than leaving it readable.
    pub user_temp: Option<PathBuf>,
    /// Marv's own binary, which `sandbox-exec` runs as `marv __guard` around the command: readable and executable
    /// even under the closed home.
    pub exe: Option<PathBuf>,
    /// The ports of the user's background processes a command without network may still reach, on `localhost`.
    pub ports: Vec<u16>,
}

/// An SBPL profile and the parameters its `(param "KEY")`s read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Profile {
    pub text: String,
    pub params: Vec<(String, PathBuf)>,
}

impl Profile {
    /// `sandbox-exec`'s arguments before `-- command…`: `-p <profile>` and a `-D KEY=VALUE` per parameter.
    pub fn args(&self) -> Vec<OsString> {
        let mut args: Vec<OsString> = vec!["-p".into(), self.text.clone().into()];
        for (key, value) in &self.params {
            let mut pair = OsString::from(format!("{key}="));
            pair.push(value.as_os_str());
            args.extend(["-D".into(), pair]);
        }
        args
    }

    /// The value of one parameter.
    pub fn param(&self, key: &str) -> Option<&Path> {
        self.params.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_path())
    }
}

/// Where `path` really leads, as Seatbelt sees it: each existing prefix resolved by `fs` (so `/tmp/x` is
/// `/private/tmp/x` on macOS, and a symlinked folder is where it leads), the missing rest as written (a placeholder
/// that doesn't exist yet). `..` after a symlink goes up from where it led, as the kernel does.
pub fn canonical(path: &Path, fs: &dyn FsProbe) -> PathBuf {
    let mut out = PathBuf::from("/");
    let mut missing = false;
    for part in path.components() {
        match part {
            Component::Normal(name) => {
                let next = out.join(name);
                out = match (missing, fs.realpath(&next)) {
                    (false, Ok(real)) if real.is_absolute() => real,
                    _ => {
                        missing = true;
                        next
                    }
                };
            }
            Component::ParentDir => {
                out.pop();
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    out
}

/// A constant (never a path from the plan) as an SBPL string.
fn quoted(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A canonical path as the prefix of a regex inside `#"…"`, every regex metacharacter escaped with one backslash.
/// Refused when it can't be written there safely: not UTF-8, a control character (a newline would end more than the
/// string), or `"` or `\`. Whether SBPL's `#"…"` reader treats a backslash as an escape isn't known yet: if it doesn't,
/// a `"` in the path would end the regex and the rest of the path would become profile text (`x"))) (allow default)`).
/// Relax this only after a macOS probe (#166) proves how `\"` and `\\` are read. A trailing `/` is dropped, so `/`
/// gives the empty prefix.
fn regex_prefix(path: &Path, what: &str) -> Result<String, SandboxError> {
    let refuse =
        || SandboxError::Mount(format!("The {what} path has a character the macOS sandbox can't express: {}", path.display()));
    let text = path.to_str().ok_or_else(refuse)?;
    if text.chars().any(|c| c.is_control() || c == '"' || c == '\\') {
        return Err(refuse());
    }
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.trim_end_matches('/').chars() {
        if matches!(c, '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|') {
            out.push('\\');
        }
        out.push(c);
    }
    Ok(out)
}

/// An ASCII name as a case-insensitive regex (APFS is case-insensitive by default, SBPL regexes have no `(?i)`):
/// `[Aa][Gg][Ee][Nn][Tt][Ss]\.[Mm][Dd]`. `*` stands for `[^/]*`.
fn any_case(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        match c {
            c if c.is_ascii_alphabetic() => {
                out.push('[');
                out.push(c.to_ascii_uppercase());
                out.push(c.to_ascii_lowercase());
                out.push(']');
            }
            '*' => out.push_str("[^/]*"),
            '.' => out.push_str("\\."),
            c => out.push(c),
        }
    }
    out
}

/// The protected names (`protected::rule_for_relative`), as regex alternatives matched against a whole path
/// component, case-insensitively. `.git` is separate: an approved command may change it.
const PROTECTED_NAMES: &[&str] = &[".env*", "bunfig.toml", ".mcp.json", "AGENTS.md", "CLAUDE.md", ".marv", ".vscode", ".idea"];

/// The profile text under construction, with its parameters.
struct Builder {
    text: String,
    params: Vec<(String, PathBuf)>,
    /// Parameters of paths that may not be the source of a hard link (see [`HARD_LINK_OP`]).
    no_links: Vec<String>,
}

impl Builder {
    fn line(&mut self, line: &str) {
        self.text.push_str(line);
        self.text.push('\n');
    }
    fn section(&mut self, title: &str) {
        self.line(&format!("; {title}"));
    }
    /// `(param "KEY")` for `value`; a key used again (a folder's read rule, then its write rule) keeps its value.
    fn param(&mut self, key: String, value: &Path) -> String {
        let text = format!("(param {})", quoted(&key));
        match self.params.iter().find(|(k, _)| *k == key) {
            Some((_, v)) => debug_assert_eq!(v, value, "{key}"),
            None => self.params.push((key, value.to_path_buf())),
        }
        text
    }
    /// `(rule op (subpath …) (literal …))` for each path, as `key`, `key_REAL` when where it leads differs.
    fn paths(&mut self, rule: &str, filters: &[&str], key: &str, given: &Path, real: Option<&Path>) {
        let mut forms = vec![(key.to_string(), given.to_path_buf())];
        if let Some(real) = real.filter(|r| *r != given) {
            forms.push((format!("{key}_REAL"), real.to_path_buf()));
        }
        for (key, path) in forms {
            if rule.starts_with("deny") && !self.no_links.contains(&key) {
                self.no_links.push(key.clone());
            }
            let p = self.param(key, &path);
            let filters: Vec<String> = filters.iter().map(|f| format!("({f} {p})")).collect();
            self.line(&format!("({rule} {})", filters.join(" ")));
        }
    }
}

/// The profile for `plan`: see the module doc for its sections. An error means a rule can't be written safely, and
/// the command must not start (G24).
pub fn profile(plan: &Plan, run: &Invocation, fs: &dyn FsProbe) -> Result<Profile, SandboxError> {
    build_profile(plan, run, fs, ALLOW_LINKS_IN_PROJECT)
}

/// [`profile`] with hard links allowed in the project and the private TMPDIR, whatever [`ALLOW_LINKS_IN_PROJECT`] says:
/// only for the macOS probe that decides it (`tests/seatbelt_probe.rs`, spec §9.2), never for a command.
#[doc(hidden)]
pub fn profile_allowing_links(plan: &Plan, run: &Invocation, fs: &dyn FsProbe) -> Result<Profile, SandboxError> {
    build_profile(plan, run, fs, true)
}

/// [`profile`], with hard links in the project and the private TMPDIR allowed or not ([`ALLOW_LINKS_IN_PROJECT`]).
fn build_profile(plan: &Plan, run: &Invocation, fs: &dyn FsProbe, allow_links: bool) -> Result<Profile, SandboxError> {
    for (path, what) in [(&plan.root, "project"), (&plan.home, "home folder")] {
        if !path.is_absolute() {
            return Err(SandboxError::Mount(format!("The {what} path {} isn't absolute.", path.display())));
        }
    }
    // The process door's relay (§8): off on macOS until #152, so a plan that needs it doesn't start.
    if !plan.binds.is_empty() {
        return Err(SandboxError::Mount(
            "Background processes aren't available in the macOS sandbox yet, so this command can't run in one.".into(),
        ));
    }
    let canon = |p: &Path| canonical(p, fs);
    let root = canon(&plan.root);
    let home = canon(&plan.home);
    let home_project = contains(&root, &home);
    let read_only = plan.project_read_only || home_project;
    let toolchains: Vec<PathBuf> = plan.toolchains(fs);
    let ro_extras = plan.read_only_extras(fs)?;
    let rw_extras = plan.writable_extras(fs)?;
    let config = plan.config_dir_hidden(fs).map(|c| canon(&c));
    // Every runtime folder, there yet or not: one Marv makes later is hidden from the start.
    let runtime: Vec<PathBuf> = plan.hidden.iter().filter(|d| d.is_absolute()).map(|d| canon(d)).collect();
    let tmp = canon(&run.tmp);
    let user_temp = user_temp(run, &tmp, fs)?;
    // The private TMPDIR is readable, writable and may hold sockets: a fresh folder of its own (the backend makes it,
    // #166) at least two levels inside the user's temporary folder (in `T/`, never `T` or `C` themselves), and
    // neither inside nor around anything the profile hides or limits.
    let others: Vec<PathBuf> = [home.clone(), root.clone(), PathBuf::from(SHARED_TMP)]
        .into_iter()
        .chain(runtime.iter().cloned())
        .chain(config.clone())
        .chain(ro_extras.iter().chain(&rw_extras).map(|e| canon(&e.real)))
        .collect();
    let deep_enough = tmp.strip_prefix(&user_temp).is_ok_and(|rest| rest.components().count() >= 2);
    if !run.tmp.is_absolute() || !deep_enough || others.iter().any(|o| contains(o, &tmp) || contains(&tmp, o)) {
        return Err(SandboxError::Mount(format!(
            "The sandbox's temporary folder {} must be a new folder of its own inside the user's temporary folder.",
            run.tmp.display()
        )));
    }
    let mut b = Builder { text: String::new(), params: Vec::new(), no_links: Vec::new() };

    // 1. Base.
    base(&mut b);

    // 2. Reads, broad.
    b.section("2. reads, broad");
    b.line("(allow file-read* file-map-executable)");

    // 3. Closed: the home, the shared temporary folders.
    b.section("3. closed");
    if home_project {
        // Every hidden top-level entry, listed or not: no listing needed, no socket scan (no unix-socket rule covers
        // the home, so nothing there can be connected to at any depth).
        let prefix = regex_prefix(&home, "home folder")?;
        b.line(&format!("(deny file-read* (regex #\"^{prefix}/\\.[^/]+(/.*)?$\"))"));
        b.line(&format!("(deny {HARD_LINK_OP} (regex #\"^{prefix}/\\.[^/]+(/.*)?$\"))"));
        // macOS keeps what Linux keeps in dotfolders in ~/Library: keychains, cookies, mail, messages, browser
        // profiles, app containers and their data. No TOOLCHAINS entry is there; a read-only or writable extra
        // under it is shown again in section 4, as the plan asks.
        b.paths("deny file-read*", &["subpath"], "HOME_LIBRARY", &home.join("Library"), None);
    } else {
        let p = b.param("HOME".into(), &home);
        b.line(&format!("(deny file-read* (subpath {p}))"));
        b.line(&format!("(allow file-read-metadata (literal {p}))"));
    }
    b.line(&format!("(deny file-read* (subpath {}))", quoted(SHARED_TMP)));
    let p = b.param("USER_TEMP".into(), &user_temp);
    b.line(&format!("(deny file-read* (subpath {p}))"));

    // 4. Reads, shown again.
    b.section("4. reads, shown again");
    let read = "allow file-read* file-map-executable";
    if !home_project {
        // In a home-folder project reads are open already, and the project would show the hidden entries again.
        b.paths(read, &["subpath"], "PROJECT", &root, None);
    }
    for (i, dir) in toolchains.iter().enumerate() {
        b.paths(read, &["subpath"], &format!("TOOLCHAIN_{i}"), &canon(dir), None);
    }
    for (i, extra) in ro_extras.iter().enumerate() {
        b.paths(read, &["subpath"], &format!("READ_ONLY_{i}"), &canon(&extra.real), None);
    }
    for (i, extra) in rw_extras.iter().enumerate() {
        b.paths(read, &["subpath"], &format!("WRITABLE_{i}"), &canon(&extra.real), None);
    }
    b.paths(read, &["subpath"], "TMP", &tmp, None);
    let exe = run.exe.as_ref().map(|e| canon(e));
    if let Some(exe) = &exe {
        b.paths(read, &["literal"], "EXE", exe, None);
    }
    // The folders above each of those, by their own entry's metadata only (`stat`, not a listing): tools walk up a
    // path with `lstat` (npm's `mkdir -p` of its cache in the TMPDIR stops at the user's temporary folder with EPERM,
    // #166 probe), as `cd ~` needs the home's. Section 5 closes again what it must.
    let mut shown: Vec<PathBuf> = vec![tmp.clone()];
    if !home_project {
        shown.push(root.clone());
    }
    shown.extend(toolchains.iter().map(|d| canon(d)));
    shown.extend(ro_extras.iter().chain(&rw_extras).map(|e| canon(&e.real)));
    shown.extend(exe.iter().cloned());
    let mut parents: Vec<PathBuf> = Vec::new();
    for path in &shown {
        // Only where section 3 closed them: elsewhere reads are open already.
        let closed = |d: &Path| d.starts_with(&home) || d.starts_with(&user_temp) || d.starts_with(SHARED_TMP);
        for dir in path.ancestors().skip(1).filter(|d| closed(d)) {
            if !parents.iter().any(|p| p == dir) {
                parents.push(dir.to_path_buf());
            }
        }
    }
    let filters: Vec<String> =
        parents.iter().enumerate().map(|(i, dir)| format!("(literal {})", b.param(format!("PARENT_{i}"), dir))).collect();
    if !filters.is_empty() {
        b.line(&format!("(allow file-read-metadata {})", filters.join(" ")));
    }

    // 5. Reads, closed again.
    b.section("5. reads, closed again");
    let masks: Vec<(PathBuf, Option<PathBuf>)> = plan.home_masks(fs).iter().map(|m| written_and_real(m, fs)).collect();
    for (i, (given, real)) in masks.iter().enumerate() {
        b.paths("deny file-read*", &["literal", "subpath"], &format!("HOME_MASK_{i}"), given, real.as_deref());
    }
    if let Some(config) = &config {
        b.paths("deny file-read*", &["subpath"], "CONFIG", config, None);
        let skills = config.join("skills");
        if fs.entry_kind(&skills) == Some(EntryKind::Dir) {
            b.paths(read, &["subpath"], "SKILLS", &skills, None);
        }
    }
    for (i, dir) in runtime.iter().enumerate() {
        b.paths("deny file-read*", &["subpath"], &format!("RUNTIME_{i}"), dir, None);
    }
    // Terminals: nothing to read keystrokes from or write into (G22).
    b.line("(deny file-read* file-write* (regex #\"^/dev/(tty|pty|ptmx)\"))");
    let credentials = credentials(plan, fs);
    for (i, (given, real)) in credentials.iter().enumerate() {
        b.paths("deny file-read*", &["literal"], &format!("CREDENTIAL_{i}"), given, real.as_deref());
    }

    // 6. Writes.
    b.section("6. writes");
    let write = "allow file-write*";
    if !read_only {
        b.paths(write, &["subpath"], "PROJECT", &root, None);
    }
    // A read-only folder inside the project or a writable one (a worktree's `.git`): read-only on top of it, as
    // bwrap mounts it. One around the project has no write rule to undo. A writable extra inside a read-only one is
    // allowed again after this, as bwrap's later writable bind sits on top of the read-only one.
    for (i, extra) in ro_extras.iter().enumerate().filter(|(_, e)| !e.around_project) {
        b.paths("deny file-write*", &["literal", "subpath"], &format!("READ_ONLY_{i}"), &canon(&extra.real), None);
    }
    for (i, extra) in rw_extras.iter().enumerate() {
        b.paths(write, &["subpath"], &format!("WRITABLE_{i}"), &canon(&extra.real), None);
    }
    b.paths(write, &["subpath"], "TMP", &tmp, None);
    // Hard links (see HARD_LINK_OP): none out of the home or the shared temporary folders; then, only with
    // ALLOW_LINKS_IN_PROJECT, allowed in the project and the private TMPDIR, which may sit inside those; what in them
    // may not be linked is denied again in section 7.
    let home_key = if home_project { None } else { Some("(param \"HOME\")".to_string()) };
    let broad: Vec<String> = home_key
        .into_iter()
        .chain([quoted(SHARED_TMP), "(param \"USER_TEMP\")".to_string()])
        .map(|p| format!("(subpath {p})"))
        .collect();
    b.line(&format!("(deny {HARD_LINK_OP} {})", broad.join(" ")));
    if allow_links {
        let mut linkable = vec![format!("(subpath {})", b.param("TMP".into(), &tmp))];
        if !read_only {
            linkable.insert(0, format!("(subpath {})", b.param("PROJECT".into(), &root)));
        }
        b.line(&format!("(allow {HARD_LINK_OP} {})", linkable.join(" ")));
    }
    b.line("(allow file-write-data (literal \"/dev/null\") (literal \"/dev/dtracehelper\") (regex #\"^/dev/fd/[0-9]+$\"))");

    // 7. Writes, closed again.
    b.section("7. writes, closed again");
    for (i, dir) in runtime.iter().enumerate() {
        b.paths("deny file-write*", &["subpath"], &format!("RUNTIME_{i}"), dir, None);
    }
    if let Some(config) = &config {
        b.paths("deny file-write*", &["subpath"], "CONFIG", config, None);
    }
    for (i, (given, real)) in masks.iter().enumerate() {
        b.paths("deny file-write*", &["literal", "subpath"], &format!("HOME_MASK_{i}"), given, real.as_deref());
    }
    for (i, (given, real)) in credentials.iter().enumerate() {
        b.paths("deny file-write*", &["literal"], &format!("CREDENTIAL_{i}"), given, real.as_deref());
    }
    if read_only {
        // Nothing to write to in the project, but a protected file linked into the private TMPDIR could be written.
        for (i, path) in plan.protected.iter().enumerate() {
            let (given, real) = written_and_real(path, fs);
            for (key, value) in [(format!("PROTECTED_{i}"), Some(given)), (format!("PROTECTED_{i}_REAL"), real)] {
                if let Some(value) = value {
                    b.param(key.clone(), &value);
                    b.no_links.push(key);
                }
            }
        }
    } else {
        // Protected paths that exist (and the hooks folder), as written and where they lead.
        let mut ancestors: Vec<PathBuf> = vec![root.clone()];
        for (i, path) in plan.protected.iter().enumerate() {
            let (given, real) = written_and_real(path, fs);
            b.paths("deny file-write*", &["literal", "subpath"], &format!("PROTECTED_{i}"), &given, real.as_deref());
            for dir in given.ancestors().skip(1).take_while(|d| d.starts_with(&root)) {
                if !ancestors.iter().any(|a| a == dir) {
                    ancestors.push(dir.to_path_buf());
                }
            }
        }
        // Paths that must stay absent (a project without `.git`): no entry can be made there (G12).
        for (i, path) in plan.placeholders.iter().enumerate() {
            let (given, real) = written_and_real(path, fs);
            b.paths("deny file-write*", &["literal", "subpath"], &format!("PLACEHOLDER_{i}"), &given, real.as_deref());
        }
        // Protected names at any depth, existing or new, outside node_modules, target and .git (G13).
        let prefix = regex_prefix(&root, "project")?;
        let names: Vec<String> = PROTECTED_NAMES.iter().map(|n| any_case(n)).collect();
        b.line(&name_rule("file-write*", &prefix, &names.join("|"), true));
        b.line(&name_rule(HARD_LINK_OP, &prefix, &names.join("|"), true));
        // `.git` at any depth for a command that runs without asking (its project `.git` is read-only or a
        // placeholder in the plan, as bash's git guard makes it); an approved command may change a repository.
        if git_confined(plan, &root, fs) {
            b.line(&name_rule("file-write*", &prefix, &any_case(".git"), false));
            b.line(&name_rule(HARD_LINK_OP, &prefix, &any_case(".git"), false));
        }
        // A protected entry can't be carried away by renaming or removing a folder above it.
        for (i, dir) in ancestors.iter().enumerate() {
            let p = b.param(format!("ANCESTOR_{i}"), dir);
            b.line(&format!("(deny file-write-unlink (literal {p}))"));
        }
    }
    // No hard link to anything denied above, even inside the project or the private TMPDIR: Library, the masks,
    // Marv's folders, credentials, protected paths, read-only extras (see HARD_LINK_OP).
    for key in std::mem::take(&mut b.no_links) {
        b.line(&format!("(deny {HARD_LINK_OP} (literal (param {k})) (subpath (param {k})))", k = quoted(&key)));
    }

    // 8. Network.
    b.section("8. network");
    if plan.network {
        b.line("(allow network-outbound (remote ip))");
        b.line("(allow network-bind (local ip))");
        b.line("(allow network-inbound (local ip \"localhost:*\"))");
        b.line(&format!("(allow network-outbound (remote unix-socket (path-literal {})))", quoted(DNS_SOCKET)));
        let services: Vec<String> = NETWORK_MACH_SERVICES.iter().map(|s| format!("(global-name {})", quoted(s))).collect();
        b.line(&format!("(allow mach-lookup {})", services.join(" ")));
        b.line(&format!("(allow sysctl-read (sysctl-name-prefix {}))", quoted(NETWORK_SYSCTL_PREFIX)));
    } else {
        // The host's loopback is the user's (databases, dev servers, a local proxy): closed, but for the forwarded
        // ports of the user's own background processes.
        for port in &run.ports {
            b.line(&format!("(allow network-outbound (remote ip \"localhost:{port}\"))"));
        }
    }
    // Unix sockets: the private TMPDIR is the command's own; nothing else on the machine is reachable.
    let p = b.param("TMP".into(), &tmp);
    b.line(&format!("(allow network-bind (local unix-socket (subpath {p})))"));
    b.line(&format!("(allow network-outbound (remote unix-socket (subpath {p})))"));
    // Marv's runtime folders (default and override, by their real paths): no socket there is reachable, nor can one be
    // bound there, whatever an allow above names. Deny-default closes them already; this keeps them closed. Then the
    // one plug.
    for i in 0..runtime.len() {
        let p = format!("(param \"RUNTIME_{i}\")");
        b.line(&format!("(deny network-outbound (remote unix-socket (subpath {p})))"));
        b.line(&format!("(deny network-bind (local unix-socket (subpath {p})))"));
    }
    if let Some(plug) = &plan.mesh_socket {
        let p = b.param("PLUG".into(), &canon(plug));
        b.line(&format!("(allow network-outbound (remote unix-socket (path-literal {p})))"));
    }

    // 9. Last.
    last(&mut b);
    Ok(Profile { text: b.text, params: b.params })
}

/// Section 1: closed by default, then what any process needs (see the module doc).
fn base(b: &mut Builder) {
    b.section("1. base");
    b.line("(version 1)");
    b.line("(deny default)");
    b.line("(allow process-fork)");
    b.line("(allow process-exec)");
    b.line("(allow signal (target same-sandbox))");
    b.line("(allow process-info* (target same-sandbox))");
    let mut sysctls: Vec<String> = SYSCTLS.iter().map(|s| format!("(sysctl-name {})", quoted(s))).collect();
    sysctls.extend(SYSCTL_PREFIXES.iter().map(|s| format!("(sysctl-name-prefix {})", quoted(s))));
    b.line(&format!("(allow sysctl-read {})", sysctls.join(" ")));
    let services: Vec<String> = MACH_SERVICES.iter().map(|s| format!("(global-name {})", quoted(s))).collect();
    b.line(&format!("(allow mach-lookup {})", services.join(" ")));
    b.line("(allow ipc-posix-sem)");
    // Creating a Unix or IP socket reaches nothing; connecting and binding are the network section's. The symbolic
    // domain names are accepted (#166 probe).
    b.line("(allow system-socket (socket-domain AF_UNIX) (socket-domain AF_INET) (socket-domain AF_INET6))");
    b.line("(allow file-ioctl (literal \"/dev/dtracehelper\"))");
}

/// Section 9: no XPC service by any name, no `fcntl` that changes a file through a read-only descriptor.
fn last(b: &mut Builder) {
    b.section("9. last");
    b.line("(deny mach-lookup (xpc-service-name-prefix \"\"))");
    b.line("(deny system-fcntl (fcntl-command 80) (fcntl-command 110))");
}

/// The profile [`crate::seatbelt_backend::Seatbelt`]'s availability check runs: the base and last sections of every
/// command's profile with reads open, and nothing else. If `sandbox-exec` is gone, or the kernel no longer accepts
/// these rules, or Marv already runs inside a Seatbelt sandbox (a nested one can't start), it fails.
pub fn probe_profile() -> String {
    let mut b = Builder { text: String::new(), params: Vec::new(), no_links: Vec::new() };
    base(&mut b);
    b.section("2. reads, broad");
    b.line("(allow file-read* file-map-executable)");
    b.line("(allow file-write-data (literal \"/dev/null\"))");
    last(&mut b);
    b.text
}

/// The user's temporary folder, `/private/var/folders/<xx>/<id>` exactly: as given or the one the private TMPDIR is
/// in. Either way the TMPDIR must be in its `T/` (never `C/` or `0/`, nor elsewhere under a given folder).
fn user_temp(run: &Invocation, tmp: &Path, fs: &dyn FsProbe) -> Result<PathBuf, SandboxError> {
    let shape = |p: &Path| p.starts_with("/private/var/folders") && p.components().count() == 6;
    let found = match &run.user_temp {
        Some(given) => Some(canonical(given, fs)).filter(|g| shape(g)),
        None => {
            let parts: Vec<Component> = tmp.components().collect();
            (parts.len() > 6).then(|| parts[..6].iter().collect::<PathBuf>()).filter(|g| shape(g))
        }
    };
    match found {
        Some(user_temp) if tmp.strip_prefix(&user_temp).is_ok_and(|rest| rest.starts_with("T")) => Ok(user_temp),
        _ => Err(SandboxError::Mount(format!(
            "Can't tell the user's temporary folder from {}, so the macOS sandbox can't close it.",
            tmp.display()
        ))),
    }
}

/// A path as written (its folder canonical, its own name kept: a symlinked credential or protected file is denied
/// by its name) and where it leads when that's somewhere else.
fn written_and_real(path: &Path, fs: &dyn FsProbe) -> (PathBuf, Option<PathBuf>) {
    let written = match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => canonical(parent, fs).join(name),
        _ => canonical(path, fs),
    };
    let real = canonical(path, fs);
    let real = (real != written).then_some(real);
    (written, real)
}

/// The [`CREDENTIALS`] files under the home that exist (a file, or a symlink: denied by name and where it leads).
fn credentials(plan: &Plan, fs: &dyn FsProbe) -> Vec<(PathBuf, Option<PathBuf>)> {
    CREDENTIALS
        .iter()
        .map(|file| plan.home.join(file))
        .filter(|file| matches!(fs.kind(file), Some(FileKind::File | FileKind::Symlink)))
        .map(|file| written_and_real(&file, fs))
        .collect()
}

/// A deny of `op` (writes, or hard links) for protected names under the project: `names` as a whole path component
/// at any depth, and what's in it, but not inside node_modules or target (nor `.git`, with `skip_git`).
fn name_rule(op: &str, prefix: &str, names: &str, skip_git: bool) -> String {
    let skipped = if skip_git { SKIPPED.to_string() } else { "(node_modules|target)".to_string() };
    format!(
        "(deny {op} (require-all (regex #\"^{prefix}/(.*/)?({names})(/.*)?$\") (require-not (regex #\"^{prefix}/(.*/)?{skipped}/\"))))"
    )
}

/// Whether the plan keeps the project's `.git` from a command that runs without asking: bash's git guard made it
/// read-only or a placeholder (by its path or, for a symlinked `.git`, where it leads).
fn git_confined(plan: &Plan, root: &Path, fs: &dyn FsProbe) -> bool {
    let dot_git = root.join(".git");
    let real = canonical(&plan.root.join(".git"), fs);
    plan.read_only.iter().chain(&plan.placeholders).map(|p| canonical(p, fs)).any(|p| p == dot_git || p == real)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fs::Fake;

    /// A macOS-like file system: `/tmp` and `/var` lead into `/private`, the home has `.cargo` and `.bun`.
    fn mac_fs(exists: impl Fn(&str) -> bool + 'static) -> Fake {
        let mut fs = Fake::new(move |p| p.ends_with(".cargo") || p.ends_with(".bun") || exists(p));
        fs.links.insert("/tmp".into(), "/private/tmp".into());
        fs.links.insert("/var".into(), "/private/var".into());
        fs
    }
    fn base() -> Plan {
        Plan { root: "/Users/me/proj".into(), home: "/Users/me".into(), path: "/usr/bin".into(), ..Default::default() }
    }
    fn run() -> Invocation {
        Invocation {
            tmp: "/var/folders/ab/xyz/T/marv-1".into(),
            user_temp: Some("/var/folders/ab/xyz".into()),
            exe: Some("/Users/me/.local/bin/marv".into()),
            ports: vec![],
        }
    }
    fn build(plan: &Plan, fs: &Fake) -> Profile {
        profile(plan, &run(), fs).unwrap()
    }
    /// With hard links allowed in the project and the TMPDIR, as once the #166 probe allows it.
    fn build_linking(plan: &Plan, fs: &Fake) -> Profile {
        build_profile(plan, &run(), fs, true).unwrap()
    }
    /// Where `needle` first is in the text (it must be there).
    fn at(text: &str, needle: &str) -> usize {
        text.find(needle).unwrap_or_else(|| panic!("{needle:?} not in:\n{text}"))
    }
    fn last_at(text: &str, needle: &str) -> usize {
        text.rfind(needle).unwrap_or_else(|| panic!("{needle:?} not in:\n{text}"))
    }
    /// The rule lines that read parameter `key`, but its hard-link deny (`nothing_denied_can_be_hard_linked`).
    fn rules<'a>(text: &'a str, key: &str) -> Vec<&'a str> {
        let p = format!("(param \"{key}\")");
        text.lines().filter(|l| l.contains(&p) && !l.starts_with(&format!("(deny {HARD_LINK_OP}"))).collect()
    }
    /// Every `#"…"` regex in the profile, as the regex sees it.
    fn regexes(text: &str) -> Vec<regex::Regex> {
        let mut out = Vec::new();
        let mut rest = text;
        while let Some(start) = rest.find("#\"") {
            let body = &rest[start + 2..];
            let mut end = None;
            let mut escaped = false;
            for (i, c) in body.char_indices() {
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    end = Some(i);
                    break;
                }
            }
            let end = end.unwrap_or_else(|| panic!("an unclosed regex in: {rest}"));
            out.push(regex::Regex::new(&body[..end]).unwrap_or_else(|e| panic!("{}: {e}", &body[..end])));
            rest = &body[end + 1..];
        }
        out
    }
    /// Whether a write to `path` is denied by the name rules (both regexes of a require-all/require-not pair).
    fn name_denied(text: &str, path: &str) -> bool {
        name_denied_for(text, "file-write*", path)
    }
    fn name_denied_for(text: &str, op: &str, path: &str) -> bool {
        text.lines().filter(|l| l.starts_with(&format!("(deny {op} (require-all"))).any(|line| {
            let r = regexes(line);
            r[0].is_match(path) && !r[1].is_match(path)
        })
    }

    #[test]
    fn the_base_is_closed_by_default_and_ends_with_the_xpc_and_fcntl_denies() {
        let text = build(&base(), &mac_fs(|_| false)).text;
        let lines: Vec<&str> = text.lines().filter(|l| !l.starts_with(';')).collect();
        assert_eq!(&lines[..2], ["(version 1)", "(deny default)"]);
        assert_eq!(
            &lines[lines.len() - 2..],
            ["(deny mach-lookup (xpc-service-name-prefix \"\"))", "(deny system-fcntl (fcntl-command 80) (fcntl-command 110))"]
        );
        for absent in ["(allow default)", "pseudo-tty", "kern.proc", "user-preference", "/dev/ptmx\")"] {
            assert!(!text.contains(absent), "{absent}");
        }
        // Only the reviewed Mach services, and without network none of the network ones.
        let looked_up: Vec<&str> = text.lines().filter(|l| l.contains("global-name")).collect();
        assert_eq!(looked_up.len(), 1);
        for service in MACH_SERVICES {
            assert!(looked_up[0].contains(service));
        }
        assert_eq!(looked_up[0].matches("global-name").count(), MACH_SERVICES.len());
        // Signals and process info reach this sandbox only.
        assert!(
            text.contains("(allow signal (target same-sandbox))") && text.contains("(allow process-info* (target same-sandbox))")
        );
        // Section order.
        let mut last = 0;
        for n in 1..=9 {
            let pos = at(&text, &format!("; {n}. "));
            assert!(pos > last || n == 1);
            last = pos;
        }
    }

    #[test]
    fn the_home_is_closed_toolchains_shown_again_read_only_credentials_denied_after_them() {
        let mut fs = mac_fs(|_| false);
        fs.kinds.insert("/Users/me/.cargo/credentials.toml".into(), FileKind::File);
        let p = build(&base(), &fs);
        let t = &p.text;
        assert_eq!(
            rules(t, "HOME"),
            ["(deny file-read* (subpath (param \"HOME\")))", "(allow file-read-metadata (literal (param \"HOME\")))"]
        );
        let cargo = (0..2).find(|i| p.param(&format!("TOOLCHAIN_{i}")) == Some(Path::new("/Users/me/.cargo"))).unwrap();
        let toolchain = format!("TOOLCHAIN_{cargo}");
        assert!(at(t, &format!("(param \"{toolchain}\")")) > at(t, "(param \"HOME\")"), "toolchains after the home deny");
        assert!(rules(t, &toolchain).iter().all(|l| l.starts_with("(allow file-read*")), "read-only: no write rule");
        assert_eq!(p.param("CREDENTIAL_0"), Some(Path::new("/Users/me/.cargo/credentials.toml")));
        assert!(at(t, "(deny file-read* (literal (param \"CREDENTIAL_0\")))") > last_at(t, &format!("(param \"{toolchain}\")")));
        assert!(at(t, "(deny file-write* (literal (param \"CREDENTIAL_0\")))") > at(t, "; 6."));
        assert!(p.param("CREDENTIAL_0_REAL").is_none());
    }

    #[test]
    fn a_symlinked_credential_is_denied_by_name_and_where_it_leads() {
        let mut fs = mac_fs(|_| false);
        fs.kinds.insert("/Users/me/.cargo/credentials.toml".into(), FileKind::Symlink);
        fs.links.insert("/Users/me/.cargo/credentials.toml".into(), "/Users/me/dotfiles/cargo-credentials".into());
        let p = build(&base(), &fs);
        assert_eq!(p.param("CREDENTIAL_0"), Some(Path::new("/Users/me/.cargo/credentials.toml")));
        assert_eq!(p.param("CREDENTIAL_0_REAL"), Some(Path::new("/Users/me/dotfiles/cargo-credentials")));
        assert_eq!(rules(&p.text, "CREDENTIAL_0_REAL").len(), 2, "read and write denied");
        // Whatever the plan's bwrap masks are, each is denied here, by one form or the other.
        let plan = base();
        for mask in plan.credentials(&plan.toolchains(&fs), &fs) {
            assert!(p.params.iter().any(|(k, v)| k.starts_with("CREDENTIAL_") && *v == mask), "{}", mask.display());
        }
    }

    #[test]
    fn marvs_config_is_closed_after_the_toolchains_and_its_skills_shown_again_after_that() {
        let fs = mac_fs(|p| p.contains("/.marv"));
        let plan = Plan { config_dir: Some("/Users/me/.marv".into()), ..base() };
        let p = build(&plan, &fs);
        let t = &p.text;
        let skills_toolchain =
            (0..5).find(|i| p.param(&format!("TOOLCHAIN_{i}")) == Some(Path::new("/Users/me/.marv/skills"))).unwrap();
        let config_read = at(t, "(deny file-read* (subpath (param \"CONFIG\")))");
        assert!(config_read > at(t, &format!("(param \"TOOLCHAIN_{skills_toolchain}\")")));
        assert!(at(t, "(allow file-read* file-map-executable (subpath (param \"SKILLS\")))") > config_read);
        assert!(at(t, "(deny file-write* (subpath (param \"CONFIG\")))") > at(t, "; 7."));
        assert!(!rules(t, "SKILLS").iter().any(|l| l.contains("file-write")), "skills read-only");
        // A config folder holding the project isn't hidden (it would hide the project).
        let holding = Plan { config_dir: Some("/Users/me".into()), ..base() };
        assert!(build(&holding, &fs).param("CONFIG").is_none());
    }

    #[test]
    fn the_runtime_folder_is_closed_after_the_project_even_inside_it_and_before_it_exists() {
        let fs = mac_fs(|_| false);
        let plan = Plan { hidden: vec!["/Users/me/proj/run/marv".into(), "/tmp/marv-501".into()], ..base() };
        let p = build(&plan, &fs);
        let t = &p.text;
        assert_eq!(p.param("RUNTIME_0"), Some(Path::new("/Users/me/proj/run/marv")));
        assert_eq!(p.param("RUNTIME_1"), Some(Path::new("/private/tmp/marv-501")), "canonical, though missing");
        for i in 0..2 {
            let key = format!("(param \"RUNTIME_{i}\")");
            assert!(at(t, &format!("(deny file-read* (subpath {key}))")) > at(t, "(subpath (param \"PROJECT\"))"));
            assert!(at(t, &format!("(deny file-write* (subpath {key}))")) > last_at(t, "(subpath (param \"PROJECT\"))"));
        }
        // The shared /tmp is closed on its own too, before anything is shown again.
        assert!(at(t, "(deny file-read* (subpath \"/private/tmp\"))") < at(t, "; 4."));
    }

    #[test]
    fn the_shared_temporary_folders_are_closed_and_the_private_tmpdir_open_with_its_sockets() {
        let p = build(&base(), &mac_fs(|_| false));
        let t = &p.text;
        assert_eq!(p.param("USER_TEMP"), Some(Path::new("/private/var/folders/ab/xyz")));
        assert_eq!(p.param("TMP"), Some(Path::new("/private/var/folders/ab/xyz/T/marv-1")));
        assert!(
            at(t, "(deny file-read* (subpath (param \"USER_TEMP\")))")
                < at(t, "(allow file-read* file-map-executable (subpath (param \"TMP\")))")
        );
        assert!(t.contains("(allow file-write* (subpath (param \"TMP\")))"));
        assert!(t.contains("(allow network-bind (local unix-socket (subpath (param \"TMP\"))))"));
        assert!(t.contains("(allow network-outbound (remote unix-socket (subpath (param \"TMP\"))))"));
        // No other Unix socket: not the home's, not /private/tmp's (the SSH agent), not the runtime folder's.
        assert_eq!(t.matches("unix-socket").count(), 2);
        // A project under /tmp is shown again over the closed /private/tmp.
        let in_tmp = build(&Plan { root: "/tmp/proj".into(), ..base() }, &mac_fs(|_| false));
        assert_eq!(in_tmp.param("PROJECT"), Some(Path::new("/private/tmp/proj")));
        assert!(
            at(&in_tmp.text, "(subpath (param \"PROJECT\"))") > at(&in_tmp.text, "(deny file-read* (subpath \"/private/tmp\"))")
        );
    }

    #[test]
    fn every_parameter_is_canonical_and_no_plan_path_is_in_the_text() {
        let mut fs = mac_fs(|p| p.contains("marv") || p.starts_with("/var/work"));
        fs.links.insert("/Users/me/code".into(), "/Volumes/work/code".into());
        let plan = Plan {
            root: "/Users/me/code/proj".into(),
            read_only: vec!["/var/work/shared".into()],
            writable: vec!["/tmp/out".into()],
            protected: vec!["/Users/me/code/proj/.envrc".into()],
            placeholders: vec!["/Users/me/code/proj/.git".into()],
            hidden: vec!["/var/run/marv".into()],
            config_dir: Some("/var/marv-config".into()),
            mesh_socket: Some("/tmp/marv-501/p/1/a.sock".into()),
            ..base()
        };
        let p = build(&plan, &fs);
        for (key, value) in &p.params {
            let text = value.to_string_lossy();
            assert!(!text.starts_with("/tmp") && !text.starts_with("/var/"), "{key}={text}");
            assert!(!text.starts_with("/Users/me/code"), "{key}={text}");
            // Only the regexes carry a path, escaped (`/private/tmp`, a parent of an extra in it, is also a constant).
            assert!(
                text == SHARED_TMP || !p.text.lines().filter(|l| !l.contains("#\"")).any(|l| l.contains(&*text)),
                "{key}'s value is in the text"
            );
        }
        assert_eq!(p.param("PROJECT"), Some(Path::new("/Volumes/work/code/proj")));
        assert_eq!(p.param("PLUG"), Some(Path::new("/private/tmp/marv-501/p/1/a.sock")));
        assert_eq!(p.param("WRITABLE_0"), Some(Path::new("/private/tmp/out")));
        // The parameters become -D KEY=VALUE after -p.
        let args = p.args();
        assert_eq!(args[0], "-p");
        assert_eq!(args[1], OsString::from(&p.text));
        assert!(args.windows(2).any(|w| w[0] == "-D" && w[1] == "PROJECT=/Volumes/work/code/proj"));
        assert_eq!(args.len(), 2 + 2 * p.params.len());
    }

    #[test]
    fn protected_paths_are_denied_as_written_and_where_they_lead_after_the_projects_write_allow() {
        let mut fs = mac_fs(|_| false);
        fs.links.insert("/Users/me/proj/pkg/AGENTS.md".into(), "/Users/me/proj/docs/agents.md".into());
        let plan = Plan { protected: vec!["/Users/me/proj/.envrc".into(), "/Users/me/proj/pkg/AGENTS.md".into()], ..base() };
        let p = build(&plan, &fs);
        let t = &p.text;
        let project_write = at(t, "(allow file-write* (subpath (param \"PROJECT\")))");
        assert_eq!(
            rules(t, "PROTECTED_0"),
            ["(deny file-write* (literal (param \"PROTECTED_0\")) (subpath (param \"PROTECTED_0\")))"]
        );
        assert!(at(t, "(param \"PROTECTED_0\")") > project_write);
        assert_eq!(p.param("PROTECTED_1"), Some(Path::new("/Users/me/proj/pkg/AGENTS.md")));
        assert_eq!(p.param("PROTECTED_1_REAL"), Some(Path::new("/Users/me/proj/docs/agents.md")));
        // The project root and every folder above a protected path can't be unlinked (or renamed away).
        let ancestors: Vec<&Path> =
            p.params.iter().filter(|(k, _)| k.starts_with("ANCESTOR_")).map(|(_, v)| v.as_path()).collect();
        assert_eq!(ancestors, [Path::new("/Users/me/proj"), Path::new("/Users/me/proj/pkg")]);
        assert!(t.contains("(deny file-write-unlink (literal (param \"ANCESTOR_1\")))"));
    }

    #[test]
    fn protected_names_are_denied_at_any_depth_case_insensitively_but_not_in_node_modules() {
        let t = build(&base(), &mac_fs(|_| false)).text;
        let root = "/Users/me/proj";
        for denied in [
            ".envrc",
            ".env",
            ".env.local",
            "pkg/AGENTS.md",
            "agents.md",
            "Agents.MD",
            "pkg/deep/claude.md",
            "bunfig.toml",
            "a/.mcp.json",
            ".marv/x",
            ".Marv",
            ".vscode/tasks.json",
            "web/.idea",
        ] {
            assert!(name_denied(&t, &format!("{root}/{denied}")), "{denied}");
        }
        for allowed in
            ["AGENTS.md.bak", "src/main.rs", "my.envrc", "env", "node_modules/pkg/AGENTS.md", "a/target/.env", "docs/.gitignore"]
        {
            assert!(!name_denied(&t, &format!("{root}/{allowed}")), "{allowed}");
        }
        assert!(!name_denied(&t, "/Users/me/other/.envrc"), "only under the project");
    }

    #[test]
    fn git_rules_only_for_a_confined_command() {
        let fs = mac_fs(|_| false);
        // An approved command: `.git` is the user's to change (bwrap leaves it writable too).
        let approved = build(&base(), &fs).text;
        assert!(!name_denied(&approved, "/Users/me/proj/.git/config"));
        assert!(!name_denied(&approved, "/Users/me/proj/vendor/x/.git/hooks/pre-commit"));
        // Confined, no .git yet: a placeholder, and no .git can be made at any depth (bwrap reverts it after).
        let confined = Plan { placeholders: vec!["/Users/me/proj/.git".into()], ..base() };
        let p = build(&confined, &fs);
        assert_eq!(rules(&p.text, "PLACEHOLDER_0").len(), 1);
        assert!(at(&p.text, "(param \"PLACEHOLDER_0\")") > at(&p.text, "(allow file-write* (subpath (param \"PROJECT\")))"));
        for path in [".git", ".git/config", "vendor/x/.git/hooks/pre-commit", ".GIT"] {
            assert!(name_denied(&p.text, &format!("/Users/me/proj/{path}")), "{path}");
        }
        assert!(!name_denied(&p.text, "/Users/me/proj/node_modules/x/.git/config"));
        assert!(!name_denied(&p.text, "/Users/me/proj/.github/workflows/ci.yml"));
        // Confined with a .git: read-only, so its writes are denied after the project's allow.
        let read_only = Plan { read_only: vec!["/Users/me/proj/.git".into()], ..base() };
        let t = build(&read_only, &fs).text;
        assert!(name_denied(&t, "/Users/me/proj/.git/hooks/pre-commit"));
        assert!(
            at(&t, "(deny file-write* (literal (param \"READ_ONLY_0\")) (subpath (param \"READ_ONLY_0\")))")
                > at(&t, "(allow file-write* (subpath (param \"PROJECT\")))")
        );
    }

    #[test]
    fn a_read_only_project_has_no_write_rule_for_it_and_needs_no_protected_rules() {
        let plan = Plan {
            project_read_only: true,
            protected: vec!["/Users/me/proj/.envrc".into()],
            placeholders: vec!["/Users/me/proj/.git".into()],
            ..base()
        };
        let p = build(&plan, &mac_fs(|_| false));
        assert!(rules(&p.text, "PROJECT").iter().all(|l| !l.contains("file-write")));
        assert!(p.param("PLACEHOLDER_0").is_none() && !p.text.contains("require-all"));
        // The protected file still can't be hard-linked into the private TMPDIR and written there.
        let protected: Vec<&str> = p.text.lines().filter(|l| l.contains("(param \"PROTECTED_0\")")).collect();
        assert_eq!(
            protected,
            [format!("(deny {HARD_LINK_OP} (literal (param \"PROTECTED_0\")) (subpath (param \"PROTECTED_0\")))")]
        );
        // Only the private TMPDIR (and /dev) is writable.
        let writes: Vec<&str> = p.text.lines().filter(|l| l.starts_with("(allow file-write")).collect();
        assert_eq!(
            writes,
            [
                "(allow file-write* (subpath (param \"TMP\")))",
                "(allow file-write-data (literal \"/dev/null\") (literal \"/dev/dtracehelper\") (regex #\"^/dev/fd/[0-9]+$\"))"
            ]
        );
    }

    #[test]
    fn a_home_folder_project_hides_every_hidden_entry_by_one_regex_and_is_read_only() {
        let mut fs = mac_fs(|p| p.contains(".ssh") || p.contains(".marv"));
        fs.kinds.insert("/Users/me/.cargo/credentials".into(), FileKind::File);
        let plan = Plan { root: "/Users/me".into(), config_dir: Some("/Users/me/.marv".into()), ..base() };
        let p = build(&plan, &fs);
        let t = &p.text;
        assert!(p.param("HOME").is_none() && p.param("PROJECT").is_none());
        let hidden: Vec<&str> =
            t.lines().filter(|l| l.starts_with("(deny file-read*") && l.contains("^/Users/me/\\.[^/]+")).collect();
        assert_eq!(hidden, ["(deny file-read* (regex #\"^/Users/me/\\.[^/]+(/.*)?$\"))"]);
        let r = &regexes(hidden[0])[0];
        assert!(r.is_match("/Users/me/.ssh/id_ed25519") && r.is_match("/Users/me/.zshrc"));
        assert!(!r.is_match("/Users/me/notes.txt") && !r.is_match("/Users/me/src/.envrc"));
        // Toolchains after it, then the masks, the config and the credentials.
        let regex_at = at(t, "(regex #\"^/Users/me/");
        assert!(at(t, "(param \"TOOLCHAIN_0\")") > regex_at);
        let ssh =
            (0..HOME_MASKS.len()).find(|i| p.param(&format!("HOME_MASK_{i}")) == Some(Path::new("/Users/me/.ssh"))).unwrap();
        assert!(at(t, &format!("(param \"HOME_MASK_{ssh}\")")) > last_at(t, "(param \"TOOLCHAIN_"));
        assert!(at(t, "(param \"CONFIG\")") > last_at(t, "(param \"TOOLCHAIN_"));
        assert!(at(t, "(param \"CREDENTIAL_0\")") > last_at(t, "(param \"TOOLCHAIN_"));
        // Nothing in the home is writable, and no socket scan is needed: no unix-socket rule covers the home.
        assert!(!t.lines().any(|l| l.starts_with("(allow file-write* (subpath (param \"PROJECT")));
        assert!(!t.lines().any(|l| l.contains("unix-socket") && l.contains("HOME")));
        // A home that can't be listed doesn't stop it: the regex needs no listing.
        let mut unlistable = mac_fs(|_| false);
        unlistable.unlistable = true;
        assert!(profile(&plan, &run(), &unlistable).is_ok());
    }

    #[test]
    fn extras_read_only_and_writable() {
        let fs = mac_fs(|p| p.starts_with("/srv"));
        let plan = Plan {
            root: "/srv/tree/pkg".into(),
            read_only: vec!["/srv/tree".into(), "/srv/shared".into()],
            writable: vec!["/srv/out".into()],
            ..base()
        };
        let p = build(&plan, &fs);
        let t = &p.text;
        // Around the project: readable, and no write deny that would cover the project.
        assert_eq!(rules(t, "READ_ONLY_0"), ["(allow file-read* file-map-executable (subpath (param \"READ_ONLY_0\")))"]);
        assert_eq!(rules(t, "READ_ONLY_1").len(), 2);
        assert_eq!(
            rules(t, "WRITABLE_0"),
            [
                "(allow file-read* file-map-executable (subpath (param \"WRITABLE_0\")))",
                "(allow file-write* (subpath (param \"WRITABLE_0\")))"
            ]
        );
        // The plan's refusals hold: the home is never an extra.
        assert!(profile(&Plan { read_only: vec!["/Users".into()], ..base() }, &run(), &fs).is_err());
        assert!(profile(&Plan { writable: vec!["/Users/me".into()], ..base() }, &run(), &fs).is_err());
    }

    #[test]
    fn network_off_closes_ip_and_loopback_but_for_forwarded_ports() {
        let fs = mac_fs(|_| false);
        let t = build(&base(), &fs).text;
        assert!(!t.contains("remote ip") && !t.contains("local ip") && !t.contains("mDNSResponder"));
        let forwarded = profile(&base(), &Invocation { ports: vec![3000, 5173], ..run() }, &fs).unwrap().text;
        let ip: Vec<&str> = forwarded.lines().filter(|l| l.contains(" ip ")).collect();
        assert_eq!(
            ip,
            [
                "(allow network-outbound (remote ip \"localhost:3000\"))",
                "(allow network-outbound (remote ip \"localhost:5173\"))"
            ]
        );
    }

    #[test]
    fn network_on_opens_ip_dns_and_tls_but_no_unix_socket() {
        let fs = mac_fs(|_| false);
        let t = profile(&Plan { network: true, ..base() }, &Invocation { ports: vec![3000], ..run() }, &fs).unwrap().text;
        assert!(t.contains("(allow network-outbound (remote ip))"));
        assert!(t.contains("(allow network-inbound (local ip \"localhost:*\"))"));
        assert!(t.contains("(path-literal \"/private/var/run/mDNSResponder\")"));
        for service in NETWORK_MACH_SERVICES {
            assert!(t.contains(service), "{service}");
        }
        assert!(!t.contains("localhost:3000"), "every port is open anyway");
        // Unix sockets: the DNS socket and the private TMPDIR's, nothing else.
        assert_eq!(t.matches("unix-socket").count(), 3);
        assert!(at(&t, "(allow network-outbound (remote ip))") > at(&t, "; 8."));
        assert!(at(&t, "(allow network-outbound (remote ip))") < at(&t, "(xpc-service-name-prefix"));
    }

    /// The folders above what's shown again have their own metadata readable, nothing more, and before section 5.
    #[test]
    fn the_folders_above_whats_shown_can_be_stat_ed_only() {
        let fs = mac_fs(|_| false);
        let p = build(&Plan { root: "/Users/me/code/proj".into(), ..base() }, &fs);
        let parents: Vec<&Path> = p.params.iter().filter(|(k, _)| k.starts_with("PARENT_")).map(|(_, v)| v.as_path()).collect();
        // Above the TMPDIR, the project, the toolchains and Marv's binary (in ~/.local/bin), where section 3 closed them.
        for dir in [
            "/private/var/folders/ab/xyz/T",
            "/private/var/folders/ab/xyz",
            "/Users/me/code",
            "/Users/me",
            "/Users/me/.local/bin",
            "/Users/me/.local",
        ] {
            assert!(parents.contains(&Path::new(dir)), "{dir}: {parents:?}");
        }
        for open in ["/", "/Users", "/private", "/private/var/folders"] {
            assert!(!parents.contains(&Path::new(open)), "{open}: readable already");
        }
        let line = p.text.lines().find(|l| l.contains("(param \"PARENT_0\")")).unwrap();
        assert!(line.starts_with("(allow file-read-metadata (literal") && !line.contains("subpath"), "{line}");
        assert!(at(&p.text, line) > at(&p.text, "; 4.") && at(&p.text, line) < at(&p.text, "; 5."));
    }

    /// Every runtime folder's sockets are denied by name, after the TMPDIR's allow and before the plug's.
    #[test]
    fn runtime_sockets_are_denied_but_the_plug() {
        let fs = mac_fs(|p| p.contains("marv"));
        let plan = Plan {
            hidden: vec!["/tmp/marv-501".into(), "/Users/me/.run/marv".into()],
            mesh_socket: Some("/tmp/marv-501/p/1/a.sock".into()),
            ..base()
        };
        let p = build(&plan, &fs);
        assert_eq!(p.param("RUNTIME_0"), Some(Path::new("/private/tmp/marv-501")));
        let tmp_allow = at(&p.text, "(allow network-outbound (remote unix-socket (subpath (param \"TMP\"))))");
        let plug = at(&p.text, "(allow network-outbound (remote unix-socket (path-literal (param \"PLUG\"))))");
        for i in 0..2 {
            for rule in ["network-outbound (remote", "network-bind (local"] {
                let line = format!("(deny {rule} unix-socket (subpath (param \"RUNTIME_{i}\"))))");
                let pos = at(&p.text, &line);
                assert!(tmp_allow < pos && pos < plug, "{line}");
            }
        }
    }

    /// The interface list names the machine's addresses: with network only, as Linux shows the host's only then.
    #[test]
    fn the_interface_list_only_with_network() {
        let fs = mac_fs(|_| false);
        let rule = format!("(sysctl-name-prefix \"{NETWORK_SYSCTL_PREFIX}\")");
        assert!(!build(&base(), &fs).text.contains(&rule));
        assert!(build(&Plan { network: true, ..base() }, &fs).text.contains(&rule));
        // securityd (keychain items, authorization prompts) is never reachable, with network or not.
        assert!(!build(&Plan { network: true, ..base() }, &fs).text.contains("com.apple.SecurityServer"));
        // What `sysctl -n` and node's `os.cpus()` need, always.
        let t = build(&base(), &fs).text;
        assert!(t.contains("(sysctl-name-prefix \"sysctl.\")") && t.contains("(sysctl-name \"machdep.cpu.brand_string\")"));
    }

    #[test]
    fn the_plug_is_the_one_socket_in_the_runtime_folder() {
        let fs = mac_fs(|_| false);
        let plan =
            Plan { mesh_socket: Some("/tmp/marv-501/p/9/peer-a.sock".into()), hidden: vec!["/tmp/marv-501".into()], ..base() };
        let p = build(&plan, &fs);
        assert_eq!(rules(&p.text, "PLUG"), ["(allow network-outbound (remote unix-socket (path-literal (param \"PLUG\"))))"]);
        assert_eq!(p.param("PLUG"), Some(Path::new("/private/tmp/marv-501/p/9/peer-a.sock")));
        assert!(rules(&p.text, "RUNTIME_0").iter().all(|l| l.starts_with("(deny")));
    }

    #[test]
    fn binds_are_refused_until_the_process_door_comes_to_macos() {
        let plan = Plan {
            binds: vec![crate::plan::Bind { source: "/usr/bin/true".into(), dest: "/run/marv-bin".into(), writable: false }],
            ..base()
        };
        let err = profile(&plan, &run(), &mac_fs(|_| false)).unwrap_err().to_string();
        assert!(err.contains("macOS sandbox"), "{err}");
    }

    #[test]
    fn terminals_are_closed() {
        let t = build(&base(), &mac_fs(|_| false)).text;
        let line = t.lines().find(|l| l.contains("/dev/(tty")).unwrap();
        assert!(line.starts_with("(deny file-read* file-write*"));
        let r = &regexes(line)[0];
        for dev in ["/dev/tty", "/dev/ttys000", "/dev/ptmx", "/dev/ptyp0"] {
            assert!(r.is_match(dev), "{dev}");
        }
        assert!(!r.is_match("/dev/null"));
        assert!(at(&t, "/dev/(tty") > at(&t, "(allow file-read* file-map-executable)"));
    }

    #[test]
    fn a_path_with_regex_and_sbpl_characters_matches_only_itself() {
        let root = "/Users/me/weird (dir).*+?[x]{1}^$| ü";
        let p = build(&Plan { root: root.into(), ..base() }, &mac_fs(|_| false));
        assert!(name_denied(&p.text, &format!("{root}/.envrc")));
        // The same path with any metacharacter read as a pattern doesn't match.
        for other in [
            "/Users/me/weird dir.*+?[x]{1}^$| ü",
            "/Users/me/weird (dir)x*+?[x]{1}^$| ü",
            "/Users/me/weird (dir).*+?x{1}^$| ü",
            "/Users/me/weird (dir).*+?[x]{1}| ü",
        ] {
            assert!(!name_denied(&p.text, &format!("{other}/.envrc")), "{other}");
        }
        // The path itself is a parameter: never in the text but escaped in the regexes.
        assert!(!p.text.contains(root));
        assert_eq!(p.param("PROJECT"), Some(Path::new(root)));
    }

    #[test]
    fn a_path_the_profile_cant_express_stops_the_command() {
        let fs = mac_fs(|_| false);
        // `"` and `\` too, until a macOS probe shows how SBPL's `#"…"` reads a backslash: a project folder named
        // `x"))) (allow default) ;` must not become profile text.
        for root in
            ["/Users/me/a\nb", "/Users/me/a\tb", "/Users/me/a\u{7f}b", "/Users/me/x\"))) (allow default) ;", "/Users/me/a\\b"]
        {
            let err = profile(&Plan { root: root.into(), ..base() }, &run(), &fs).unwrap_err().to_string();
            assert!(err.contains("can't express"), "{err}");
        }
        // A read-only project writes no regex for it, but the home project's regex has the same check.
        let err = profile(&Plan { root: "/Users/m\ne".into(), home: "/Users/m\ne".into(), ..base() }, &run(), &fs).unwrap_err();
        assert!(err.to_string().contains("home folder"));
        assert!(profile(&Plan { root: "relative".into(), ..base() }, &run(), &fs).is_err());
    }

    #[test]
    fn a_home_folder_project_closes_library_too() {
        let plan = Plan { root: "/Users/me".into(), ..base() };
        let p = build(&plan, &mac_fs(|_| false));
        assert_eq!(p.param("HOME_LIBRARY"), Some(Path::new("/Users/me/Library")));
        let rules = rules(&p.text, "HOME_LIBRARY");
        assert!(rules.contains(&"(deny file-read* (subpath (param \"HOME_LIBRARY\")))"));
        // Nothing reads it again later (no toolchain lives there).
        let deny = at(&p.text, "(deny file-read* (subpath (param \"HOME_LIBRARY\")))");
        assert!(!p.text[deny..].lines().any(|l| l.starts_with("(allow file-read") && l.contains("HOME_LIBRARY")));
        // In any other project the whole home is closed already.
        assert!(build(&base(), &mac_fs(|_| false)).param("HOME_LIBRARY").is_none());
    }

    #[test]
    fn nothing_denied_can_be_hard_linked() {
        let mut fs = mac_fs(|p| p.contains("marv"));
        fs.kinds.insert("/Users/me/.cargo/credentials.toml".into(), FileKind::File);
        let plan = Plan {
            protected: vec!["/Users/me/proj/.envrc".into()],
            read_only: vec!["/Users/me/proj/.git".into()],
            hidden: vec!["/tmp/marv-501".into()],
            config_dir: Some("/Users/me/.marv".into()),
            ..base()
        };
        // As shipped: links allowed in the project and the TMPDIR, since the #166 probe showed `file-link` checks the
        // source too (ALLOW_LINKS_IN_PROJECT).
        assert_eq!(build(&plan, &fs), build_linking(&plan, &fs));
        // Without the allow: no link anywhere (deny default), the denies all the same.
        let off = build_profile(&plan, &run(), &fs, false).unwrap().text;
        assert!(!off.lines().any(|l| l.starts_with(&format!("(allow {HARD_LINK_OP}"))));
        assert!(off.contains(&format!("(deny {HARD_LINK_OP} (literal (param \"CREDENTIAL_0\"))")));
        // With links allowed in the project and the TMPDIR:
        let t = build_linking(&plan, &fs).text;
        for key in ["CREDENTIAL_0", "PROTECTED_0", "READ_ONLY_0", "RUNTIME_0", "CONFIG"] {
            let line = format!("(deny {HARD_LINK_OP} (literal (param \"{key}\")) (subpath (param \"{key}\")))");
            assert!(at(&t, &line) > at(&t, "; 7."), "{key}");
        }
        // Not out of the home or the shared temporary folders; then allowed in the project and the private TMPDIR,
        // which sit inside those, before the denies above.
        let broad = at(
            &t,
            &format!(
                "(deny {HARD_LINK_OP} (subpath (param \"HOME\")) (subpath \"/private/tmp\") (subpath (param \"USER_TEMP\")))"
            ),
        );
        let allow = at(&t, &format!("(allow {HARD_LINK_OP} (subpath (param \"PROJECT\")) (subpath (param \"TMP\")))"));
        assert!(at(&t, "; 6.") < broad && broad < allow && allow < at(&t, "; 7."));
        for key in ["TMP", "PROJECT", "USER_TEMP", "HOME"] {
            assert!(!t.contains(&format!("(deny {HARD_LINK_OP} (literal (param \"{key}\"))")), "{key}");
        }
        // Protected names at any depth can't be linked (and written through) either; nor a nested .git when confined.
        for path in ["sub/.env", "a/b/AGENTS.md", "sub/.git/config"] {
            assert!(name_denied_for(&t, HARD_LINK_OP, &format!("/Users/me/proj/{path}")), "{path}");
        }
        assert!(!name_denied_for(&t, HARD_LINK_OP, "/Users/me/proj/src/main.rs"));
        // A read-only project: links only in the private TMPDIR.
        let ro = build_linking(&Plan { project_read_only: true, ..plan.clone() }, &fs).text;
        assert!(ro.contains(&format!("(allow {HARD_LINK_OP} (subpath (param \"TMP\")))")));
        // A home-folder project: the hidden entries by the same regex as their reads.
        let home = build(&Plan { root: "/Users/me".into(), ..base() }, &fs).text;
        assert!(home.contains(&format!("(deny {HARD_LINK_OP} (regex #\"^/Users/me/\\.[^/]+(/.*)?$\"))")));
        assert!(home.contains(&format!("(deny {HARD_LINK_OP} (literal (param \"HOME_LIBRARY\"))")));
    }

    #[test]
    fn the_users_temporary_folder_is_found_from_the_tmpdir_or_the_profile_is_refused() {
        let fs = mac_fs(|_| false);
        let derived = profile(&base(), &Invocation { user_temp: None, ..run() }, &fs).unwrap();
        assert_eq!(derived.param("USER_TEMP"), Some(Path::new("/private/var/folders/ab/xyz")));
        let elsewhere = Invocation { tmp: "/Volumes/scratch/marv-1".into(), user_temp: None, ..run() };
        assert!(profile(&base(), &elsewhere, &fs).unwrap_err().to_string().contains("temporary folder"));
        // Given, it must have the shape /private/var/folders/<xx>/<id>, and the TMPDIR must be in its T/.
        let given =
            Invocation { tmp: "/var/folders/cd/uvw/T/marv-1".into(), user_temp: Some("/var/folders/cd/uvw".into()), ..run() };
        assert_eq!(profile(&base(), &given, &fs).unwrap().param("USER_TEMP"), Some(Path::new("/private/var/folders/cd/uvw")));
        for (tmp, user_temp) in [
            ("/opt/homebrew/x/y", Some("/")),
            ("/Volumes/scratch/T/marv-1", Some("/Volumes/scratch")),
            ("/var/folders/cd/uvw/T/marv-1", Some("/var/folders/cd")),
            ("/var/folders/cd/uvw/T/marv-1", Some("/var/folders/cd/uvw/T")),
            ("/var/folders/cd/uvw/C/marv-1", Some("/var/folders/cd/uvw")),
            ("/var/folders/cd/uvw/0/marv-1", None),
            ("/var/folders/cd/uvw/C/marv-1", None),
            ("/var/folders/ab/xyz/T/marv-1", Some("/var/folders/cd/uvw")),
        ] {
            let run = Invocation { tmp: tmp.into(), user_temp: user_temp.map(PathBuf::from), ..run() };
            assert!(profile(&base(), &run, &fs).is_err(), "{tmp} in {user_temp:?}");
        }
    }

    #[test]
    fn the_private_tmpdir_must_be_a_folder_of_its_own() {
        let fs = mac_fs(|_| false);
        for tmp in [
            "",
            "relative",
            "/",
            "/Users",
            "/Users/me",
            "/Users/me/proj",
            "/Users/me/Documents",
            "/Users/me/.ssh",
            "/opt/homebrew",
            "/tmp",
            "/tmp/marv-501",
            "/tmp/com.apple.launchd.x/Listeners",
            "/var/folders",
            "/var/folders/ab/xyz",
            "/var/folders/ab/xyz/T",
            "/var/folders/ab/xyz/C",
            "/var/folders/ab/xyz/T/../../xyz/T",
        ] {
            let run = Invocation { tmp: tmp.into(), ..run() };
            assert!(profile(&base(), &run, &fs).is_err(), "{tmp:?}");
        }
        // Inside a read-only project, still refused.
        let ro = Plan { project_read_only: true, ..base() };
        assert!(profile(&ro, &Invocation { tmp: "/Users/me/proj/sub".into(), ..run() }, &fs).is_err());
        // Inside or around a runtime folder, the config folder or an extra, even in the user's temporary folder.
        let t = "/var/folders/ab/xyz/T";
        let marv_fs = mac_fs(|p| p.contains("/T/"));
        for plan in [
            Plan { hidden: vec![format!("{t}/marv-run").into()], ..base() },
            Plan { config_dir: Some(format!("{t}/marv-run").into()), ..base() },
            Plan { writable: vec![format!("{t}/marv-run").into()], ..base() },
            Plan { read_only: vec![format!("{t}/marv-run").into()], ..base() },
        ] {
            for tmp in [format!("{t}/marv-run/x"), format!("{t}/marv-run")] {
                assert!(profile(&plan, &Invocation { tmp: tmp.clone().into(), ..run() }, &marv_fs).is_err(), "{tmp} {plan:?}");
            }
            let around = Plan { hidden: vec![format!("{t}/a/marv-run").into()], ..base() };
            assert!(profile(&around, &Invocation { tmp: format!("{t}/a").into(), ..run() }, &marv_fs).is_err());
        }
        // The usual one is fine.
        assert!(profile(&base(), &run(), &fs).is_ok());
    }

    #[test]
    fn only_unix_and_ip_sockets_can_be_made() {
        let t = build(&base(), &mac_fs(|_| false)).text;
        let line = t.lines().find(|l| l.contains("system-socket")).unwrap();
        assert_eq!(line, "(allow system-socket (socket-domain AF_UNIX) (socket-domain AF_INET) (socket-domain AF_INET6))");
    }

    /// For every credential, home mask, runtime and config deny, no later allow of the same operation names a path
    /// equal to it, inside it or around it (`SKILLS` after the config's read deny is the one intended exception).
    #[test]
    fn no_allow_reopens_a_hidden_path() {
        let mut fs = mac_fs(|p| p.contains("marv") || p.starts_with("/srv"));
        fs.kinds.insert("/Users/me/.cargo/credentials.toml".into(), FileKind::File);
        fs.kinds.insert("/Users/me/.cargo/credentials".into(), FileKind::Symlink);
        fs.links.insert("/Users/me/.cargo/credentials".into(), "/Users/me/proj/creds".into());
        let scenarios = [
            Plan {
                hidden: vec!["/Users/me/proj/run/marv".into(), "/tmp/marv-501".into()],
                config_dir: Some("/Users/me/.marv".into()),
                protected: vec!["/Users/me/proj/.envrc".into()],
                placeholders: vec!["/Users/me/proj/.git".into()],
                read_only: vec!["/srv/ro".into()],
                writable: vec!["/srv/rw".into()],
                mesh_socket: Some("/tmp/marv-501/p/1/a.sock".into()),
                network: true,
                ..base()
            },
            Plan {
                root: "/Users/me".into(),
                config_dir: Some("/Users/me/.marv".into()),
                hidden: vec!["/tmp/marv-501".into()],
                ..base()
            },
            Plan {
                root: "/srv/proj".into(),
                config_dir: Some("/srv/proj/marv-config".into()),
                hidden: vec!["/srv/proj/marv-run".into()],
                ..base()
            },
        ];
        for (plan, allow_links) in scenarios.iter().flat_map(|p| [(p, false), (p, true)]) {
            let p = build_profile(plan, &run(), &fs, allow_links).unwrap();
            let lines: Vec<&str> = p.text.lines().collect();
            let keys_in = |line: &str| -> Vec<String> {
                line.split("(param \"").skip(1).filter_map(|rest| rest.split('"').next()).map(str::to_string).collect()
            };
            let hidden = |k: &str| {
                [
                    "CREDENTIAL_",
                    "HOME_MASK_",
                    "HOME_LIBRARY",
                    "RUNTIME_",
                    "CONFIG",
                    "PROTECTED_",
                    "PLACEHOLDER_",
                    "READ_ONLY_",
                    "USER_TEMP",
                ]
                .iter()
                .any(|h| k.starts_with(h))
            };
            // No socket rule for a path around or inside Marv's runtime or config folders, by parameter or constant,
            // `subpath` or `path-literal`; the plug's own literal, inside a runtime folder, is the one exception.
            for line in lines.iter().filter(|l| l.starts_with("(allow network")) {
                for (filter, allowed) in socket_paths(line, &p) {
                    if filter == "path-literal" && Some(allowed.as_path()) == p.param("PLUG") {
                        continue;
                    }
                    for (k, v) in p.params.iter().filter(|(k, _)| k.starts_with("RUNTIME_") || k == "CONFIG") {
                        assert!(!v.starts_with(&allowed) && !allowed.starts_with(v), "{k} reachable by sockets in: {line}");
                    }
                }
            }
            for (i, line) in lines.iter().enumerate() {
                for op in ["file-read", "file-write", HARD_LINK_OP] {
                    if !line.starts_with(&format!("(deny {op}")) && !line.starts_with(&format!("(deny file-read* {op}")) {
                        continue;
                    }
                    for key in keys_in(line).into_iter().filter(|k| hidden(k)) {
                        let denied = p.param(&key).unwrap();
                        for later in &lines[i + 1..] {
                            if !later.starts_with(&format!("(allow {op}")) {
                                continue;
                            }
                            for other in keys_in(later) {
                                // Intended: skills shown again inside the config; the private TMPDIR inside the user's.
                                // And the folders above what's shown, by their own metadata only.
                                if (key == "CONFIG" && other == "SKILLS" && op == "file-read")
                                    || (key == "USER_TEMP" && other == "TMP")
                                    || (other.starts_with("PARENT_") && later.starts_with("(allow file-read-metadata (literal"))
                                {
                                    continue;
                                }
                                let allowed = p.param(&other).unwrap();
                                assert!(
                                    !denied.starts_with(allowed) && !allowed.starts_with(denied),
                                    "{key} ({}) reopened by {other} in: {later}",
                                    denied.display()
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// The paths a socket rule names: `(subpath …)`, `(path-literal …)` or `(literal …)` of a parameter or a constant.
    fn socket_paths(line: &str, p: &Profile) -> Vec<(&'static str, PathBuf)> {
        let mut out = Vec::new();
        for filter in ["subpath", "path-literal", "literal"] {
            for rest in line.split(&format!("({filter} ")).skip(1) {
                let path = if let Some(key) = rest.strip_prefix("(param \"") {
                    p.param(key.split('"').next().unwrap()).unwrap().to_path_buf()
                } else if let Some(text) = rest.strip_prefix('"') {
                    PathBuf::from(text.split('"').next().unwrap())
                } else {
                    panic!("an unparsed socket filter in: {line}");
                };
                out.push((filter, path));
            }
        }
        out
    }

    #[test]
    fn the_socket_check_sees_constants_and_literals() {
        let p = Profile { text: String::new(), params: vec![("TMP".into(), "/x/t".into())] };
        let line = "(allow network-outbound (remote unix-socket (subpath \"/private/tmp\")) (path-literal (param \"TMP\")))";
        assert_eq!(socket_paths(line, &p), [("subpath", PathBuf::from("/private/tmp")), ("path-literal", PathBuf::from("/x/t"))]);
    }

    /// Parity: every field of the plan is rendered by both backends. Destructured without `..`, so a new field fails
    /// to compile here until both renderers (and this test) handle it.
    #[test]
    fn every_plan_field_reaches_both_renderers() {
        let mut fs = mac_fs(|p| p.starts_with("/srv") || p.contains("marv"));
        fs.kinds.insert("/Users/me/.cargo/credentials".into(), FileKind::File);
        let plan = Plan {
            root: "/Users/me/proj".into(),
            home: "/Users/me".into(),
            network: false,
            path: "/usr/bin".into(),
            lang: "en_US.UTF-8".into(),
            read_only: vec!["/srv/ro".into()],
            writable: vec!["/srv/rw".into()],
            placeholders: vec!["/Users/me/proj/.git".into()],
            project_read_only: false,
            protected: vec!["/Users/me/proj/.envrc".into()],
            mesh_socket: Some("/srv/marv-run/p/1/a.sock".into()),
            binds: vec![],
            hidden: vec!["/srv/marv-run".into()],
            config_dir: Some("/srv/marv-config".into()),
        };
        let Plan {
            root,
            home,
            network,
            path: _, // the environment, set on the spawn (#166), not in a profile
            lang: _, // the same
            read_only,
            writable,
            placeholders,
            project_read_only,
            protected,
            mesh_socket,
            binds, // refused on macOS until #152 (binds_are_refused_until_the_process_door_comes_to_macos)
            hidden,
            config_dir,
        } = &plan;
        assert!(binds.is_empty());
        let bwrap = crate::bwrap::sandbox_args(&plan, &fs)
            .unwrap()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        let p = build(&plan, &fs);
        let has = |key: &str, value: &Path| p.param(key) == Some(value);
        for (key, value) in [
            ("PROJECT", root),
            ("HOME", home),
            ("READ_ONLY_0", &read_only[0]),
            ("WRITABLE_0", &writable[0]),
            ("PLACEHOLDER_0", &placeholders[0]),
            ("PROTECTED_0", &protected[0]),
            ("PLUG", mesh_socket.as_ref().unwrap()),
            ("RUNTIME_0", &hidden[0]),
            ("CONFIG", config_dir.as_ref().unwrap()),
            ("CREDENTIAL_0", &home.join(".cargo/credentials")),
        ] {
            assert!(has(key, value), "{key}");
            assert!(bwrap.contains(&*value.to_string_lossy()), "bwrap renders {}", value.display());
        }
        assert!(!network && bwrap.contains("--unshare-net") && !p.text.contains("remote ip"));
        assert!(!project_read_only && p.text.contains("(allow file-write* (subpath (param \"PROJECT\")))"));
    }
}
