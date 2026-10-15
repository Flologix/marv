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
//! - when the project is the home folder or a parent of it (Marv started in `~`), the stand-in can't hide the home
//!   without hiding the project, so the project is shown **read-only** instead, and on top of it every hidden
//!   top-level entry of the home (`~/.<anything>`: Marv's config, credentials, agent and tool state, the sockets they
//!   keep) is an empty tmpfs or `/dev/null`, the toolchain folders are shown again read-only, [`HOME_MASKS`] are
//!   masked once more, and every Unix socket or FIFO a bounded scan finds in the visible part of the home is
//!   `/dev/null` (a read-only mount doesn't stop a `connect()` or a write into a FIFO). See `home_masks`;
//! - Marv's config folder (`MARV_CONFIG_DIR`) is an empty tmpfs wherever it is, except inside the stand-in home,
//!   which hides it already;
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
use crate::home::{CREDENTIALS, HOME_MASKS, TOOLCHAINS, contains, credential_masks};
use crate::protected::SKIPPED_DIRS;

/// Where the mesh plug's socket appears inside the sandbox: the one thing in the otherwise empty `/run`.
pub const MESH_SOCKET: &str = "/run/marv/mesh.sock";

/// What a path itself is, not following a final symlink (`lstatKind`). Folders and special files are `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    File,
    Symlink,
}

/// What a folder entry is, not following a symlink: what [`FsProbe::entries`] reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EntryKind {
    Dir,
    File,
    Symlink,
    /// A Unix socket or a FIFO: a way to talk to a process outside the sandbox, which a read-only mount doesn't close.
    Channel,
    /// A device or anything else.
    Other,
}

/// How deep below the home folder the socket scan of a home-folder project looks, and how many entries it reads at
/// most (breadth first, so the shallow ones are seen first). The home's hidden entries, where services keep their
/// sockets, are masked whole without a scan; this is for the visible folders.
pub const SOCKET_SCAN_DEPTH: usize = 6;
pub const SOCKET_SCAN_LIMIT: usize = 20_000;

/// The file system, injectable for tests.
pub trait FsProbe {
    /// A folder's entries with what each is (not following symlinks).
    fn entries(&self, dir: &Path) -> io::Result<Vec<(std::ffi::OsString, EntryKind)>>;
    /// What a path itself is (not following a final symlink); None if it doesn't exist.
    fn entry_kind(&self, path: &Path) -> Option<EntryKind>;
    /// Whether a path exists (following symlinks).
    fn exists(&self, path: &Path) -> bool;
    /// What the path itself is.
    fn kind(&self, path: &Path) -> Option<FileKind>;
    /// Where it really leads (an error if it leads nowhere).
    fn realpath(&self, path: &Path) -> io::Result<PathBuf>;
}

/// The real file system.
pub struct RealFs;

fn entry_kind_of(file_type: std::fs::FileType) -> EntryKind {
    use std::os::unix::fs::FileTypeExt;
    if file_type.is_symlink() {
        EntryKind::Symlink
    } else if file_type.is_dir() {
        EntryKind::Dir
    } else if file_type.is_file() {
        EntryKind::File
    } else if file_type.is_socket() || file_type.is_fifo() {
        EntryKind::Channel
    } else {
        EntryKind::Other
    }
}

impl FsProbe for RealFs {
    fn entries(&self, dir: &Path) -> io::Result<Vec<(OsString, EntryKind)>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            out.push((entry.file_name(), entry.file_type().map(entry_kind_of)?));
        }
        out.sort();
        Ok(out)
    }
    fn entry_kind(&self, path: &Path) -> Option<EntryKind> {
        std::fs::symlink_metadata(path).ok().map(|m| entry_kind_of(m.file_type()))
    }
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
    /// Files or folders mounted at a path of their own, after `/run` is emptied (so a destination under `/run`
    /// works): the process door's relay (Marv #51) needs Marv's own binary at `/run/marv-bin` (read-only) and its
    /// socket folder at `/run/marv` (writable for the process that serves it, read-only for a command that only
    /// connects).
    pub binds: Vec<Bind>,
    /// Marv's own runtime folders (`$XDG_RUNTIME_DIR/marv`: the mesh's plug and discovery sockets, the process
    /// door's relays), each an empty tmpfs in the sandbox wherever it is (marv-rs).
    pub hidden: Vec<PathBuf>,
    /// Marv's config folder (`~/.marv` or `MARV_CONFIG_DIR`): an empty tmpfs in the sandbox wherever it is, its
    /// `skills/` shown again read-only (marv-rs). Inside the stand-in home it's hidden already.
    pub config_dir: Option<PathBuf>,
}

/// One of [`SandboxOptions::binds`]: `source` on the host, at `dest` in the sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bind {
    pub source: PathBuf,
    pub dest: PathBuf,
    pub writable: bool,
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
    let real_home = fs.realpath(&opts.home).unwrap_or_else(|_| opts.home.clone());
    let real_root = fs.realpath(&opts.root).unwrap_or_else(|_| opts.root.clone());
    // The project is the home folder or a parent of it (marv-rs): mounting it writable would put the whole real home
    // back over the stand-in, so it's read-only, with what's private in the home masked on top (`home_masks`). At /
    // the first mount already shows it read-only, and binding / again would cover /tmp, /run and the stand-in home:
    // the home stays a stand-in then.
    let home_project = contains(&real_root, &real_home);
    let bind_root = real_root != Path::new("/");
    let home_shown = home_project && bind_root;
    let read_only = opts.project_read_only || home_project;
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
    for bind in &opts.binds {
        let flag = if bind.writable { "--bind" } else { "--ro-bind" };
        args.extend([flag.into(), bind.source.clone().into_os_string(), bind.dest.clone().into_os_string()]);
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
    if bind_root {
        args.extend([if read_only { "--ro-bind" } else { "--bind" }.into(), root.to_os_string(), root.to_os_string()]);
    }
    for (dir, real, _) in extras.iter().filter(|e| !e.2) {
        args.extend(["--ro-bind".into(), real.clone().into_os_string(), dir.as_os_str().to_os_string()]);
    }
    for dir in &opts.writable {
        let (real, _) = resolve_extra(dir, &opts.home, &opts.root, fs)?;
        args.extend(["--bind".into(), real.into_os_string(), dir.as_os_str().to_os_string()]);
    }
    if !read_only {
        // Protected paths (marv-rs): read-only on top of the writable project.
        for path in &opts.protected {
            args.extend(["--ro-bind".into(), path.clone().into_os_string(), path.clone().into_os_string()]);
        }
        // A read-only project can't get the empty mount point bwrap creates, and nothing in it can create a .git.
        for path in &opts.placeholders {
            args.extend(["--tmpfs".into(), path.clone().into_os_string(), "--remount-ro".into(), path.clone().into_os_string()]);
        }
    }
    // Marv's runtime folder, empty wherever $XDG_RUNTIME_DIR points (marv-rs): a socket a command could see is one it
    // could connect to (a read-only mount doesn't stop connect()), and a plug's socket carries that plug's identity
    // and wires. After the project, toolchain and PATH mounts, so none of them shows it again. The relay and mesh
    // binds have their own destinations under /run/marv, which this never covers. Under /run it's already gone unless
    // something is mounted again over the empty /run around it: the project (a drive under /run/media) or a PATH
    // folder kept there. Otherwise a mount would only make bwrap create its mount point in that empty /run.
    let shown_again = |real: &Path| {
        real.starts_with(&opts.root)
            || opts.read_only.iter().chain(&opts.writable).any(|d| real.starts_with(d))
            || opts.path.split(':').any(|d| d.starts_with("/run/") && real.starts_with(d))
    };
    for dir in &opts.hidden {
        if let Ok(real) = fs.realpath(dir)
            && fs.exists(&real)
            && real.is_absolute()
            && (!real.starts_with("/run") || shown_again(&real))
        {
            args.extend(["--tmpfs".into(), real.into_os_string()]);
        }
    }
    // Marv's config folder (marv-rs): its config.json holds the API key and the session-start hooks, which run outside
    // the sandbox. Inside the stand-in home it's gone already (a tmpfs there would only hide ~/.marv/skills, mounted
    // above); elsewhere `/` shows it, and in a home-folder project the project does. Never one that holds the
    // project: that would hide the project.
    let mut config_hidden = None;
    if let Some(dir) = &opts.config_dir
        && let Ok(real) = fs.realpath(dir)
        && fs.entry_kind(&real) == Some(EntryKind::Dir)
        && !contains(&real, &real_root)
        && (home_shown || !real.starts_with(&real_home))
    {
        args.extend(["--tmpfs".into(), real.clone().into_os_string()]);
        let skills = real.join("skills");
        if !home_shown && fs.entry_kind(&skills) == Some(EntryKind::Dir) {
            args.extend(["--ro-bind".into(), skills.clone().into_os_string(), skills.into_os_string()]);
        }
        config_hidden = Some(real);
    }
    if home_shown {
        args.extend(home_masks(opts, &real_home, config_hidden.as_deref(), fs)?);
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

/// What hides `path` (by what it is, not following a symlink): a folder becomes an empty tmpfs, anything else
/// (a file, a socket, a FIFO) `/dev/null`. A symlink hides what it leads to, if that's inside the home and not the
/// home itself or a parent of it (outside the home, it's what any sandbox shows; a link to the home only leads back
/// to what's masked anyway). Nothing for a path that doesn't exist.
fn mask(path: &Path, real_home: &Path, fs: &dyn FsProbe) -> Option<(Vec<OsString>, PathBuf)> {
    let (path, kind) = match fs.entry_kind(path)? {
        EntryKind::Symlink => {
            let target = fs.realpath(path).ok()?;
            if !target.starts_with(real_home) || contains(&target, real_home) {
                return None;
            }
            let kind = fs.entry_kind(&target)?;
            (target, kind)
        }
        kind => (path.to_path_buf(), kind),
    };
    let args = match kind {
        EntryKind::Dir => vec!["--tmpfs".into(), path.clone().into_os_string()],
        EntryKind::Symlink => return None, // realpath leads to no symlink
        _ => vec!["--ro-bind".into(), "/dev/null".into(), path.clone().into_os_string()],
    };
    Some((args, path))
}

/// The masks over a home folder the project shows (read-only), in this order:
/// 1. every hidden top-level entry (`~/.<anything>`) but the toolchains named whole in [`TOOLCHAINS`] (`.bun`,
///    `.cargo`, `.gitconfig`…): Marv's config, credentials (`.ssh`, `.gnupg`, cloud CLI folders), agent state
///    (`.claude`), and `.config`, `.local`, `.cache` where tools keep their sockets (Herdr's `.config/herdr`). This is
///    what the stand-in home hides in any other project. If the home can't be listed, the command doesn't run;
/// 2. the toolchain folders inside those (`.local/bin`, `.config/git`, `.marv/skills`…), shown again read-only;
/// 3. [`HOME_MASKS`], wherever step 2 (or a future toolchain) would show one again;
/// 4. every Unix socket and FIFO in the visible part of the home, found by a breadth-first scan of at most
///    [`SOCKET_SCAN_LIMIT`] entries, [`SOCKET_SCAN_DEPTH`] levels deep, not following symlinks and skipping
///    `node_modules`, `target` and `.git/objects`: `/dev/null` on top of each. A read-only mount doesn't stop a
///    `connect()` on a socket or a write into a FIFO, and a service listening there may do anything its client asks.
///
/// The credential masks ([`CREDENTIALS`]) still come after all of these.
fn home_masks(
    opts: &SandboxOptions,
    real_home: &Path,
    config_hidden: Option<&Path>,
    fs: &dyn FsProbe,
) -> Result<Vec<OsString>, SandboxError> {
    let entries = fs.entries(real_home).map_err(|e| {
        SandboxError::Mount(format!(
            "Can't list the home folder {} to hide what's private in it ({e}), so no command runs there in the sandbox. Start Marv in a project folder.",
            real_home.display()
        ))
    })?;
    let mut args: Vec<OsString> = Vec::new();
    let mut masked: Vec<PathBuf> = config_hidden.map(Path::to_path_buf).into_iter().collect();
    // 1. Hidden top-level entries.
    for (name, _) in &entries {
        let text = name.to_string_lossy();
        if !text.starts_with('.') || TOOLCHAINS.contains(&&*text) {
            continue;
        }
        if let Some((more, path)) = mask(&real_home.join(name), real_home, fs)
            && !masked.contains(&path)
        {
            args.extend(more);
            masked.push(path);
        }
    }
    // 2. Toolchains under them, shown again (bwrap makes their mount points in the tmpfs).
    let shown_again: Vec<PathBuf> = TOOLCHAINS
        .iter()
        .map(|dir| real_home.join(dir))
        .filter(|full| fs.exists(full) && masked.iter().any(|m| full.starts_with(m) && full != m))
        .collect();
    for full in &shown_again {
        args.extend(["--ro-bind".into(), full.clone().into_os_string(), full.clone().into_os_string()]);
    }
    let visible = |path: &Path, masked: &[PathBuf]| {
        !masked.iter().any(|m| path.starts_with(m)) || shown_again.iter().any(|t| path.starts_with(t))
    };
    // 3. The named ones, wherever a toolchain shows them again.
    for name in HOME_MASKS {
        let path = real_home.join(name);
        if visible(&path, &masked)
            && let Some((more, path)) = mask(&path, real_home, fs)
        {
            args.extend(more);
            masked.push(path);
        }
    }
    // 4. Sockets and FIFOs in what's still visible.
    let mut queue: std::collections::VecDeque<(PathBuf, usize)> = std::collections::VecDeque::new();
    let mut seen = 0usize;
    let mut channels: Vec<PathBuf> = Vec::new();
    let mut visit =
        |dir: &Path, depth: usize, list: &[(OsString, EntryKind)], queue: &mut std::collections::VecDeque<(PathBuf, usize)>| {
            for (name, kind) in list {
                let path = dir.join(name);
                // The toolchains left whole at the top (`.cargo`, `.rustup`) are shown as in any project's sandbox, unscanned.
                let skipped = SKIPPED_DIRS.iter().any(|s| *s != ".git" && name == *s)
                    || (name == "objects" && dir.file_name().is_some_and(|n| n == ".git"))
                    || (depth == 1 && name.to_string_lossy().starts_with('.'));
                match kind {
                    EntryKind::Channel if visible(&path, &masked) => channels.push(path),
                    EntryKind::Dir if depth < SOCKET_SCAN_DEPTH && !skipped && visible(&path, &masked) => {
                        queue.push_back((path, depth + 1))
                    }
                    _ => {}
                }
            }
        };
    visit(real_home, 1, &entries, &mut queue);
    while let Some((dir, depth)) = queue.pop_front() {
        if opts.hidden.iter().any(|h| dir.starts_with(h)) {
            continue;
        }
        let Ok(list) = fs.entries(&dir) else { continue };
        seen += list.len();
        visit(&dir, depth, &list, &mut queue);
        if seen >= SOCKET_SCAN_LIMIT {
            break;
        }
    }
    for path in channels {
        args.extend(["--ro-bind".into(), "/dev/null".into(), path.into_os_string()]);
    }
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
        /// Folder listings (a folder not listed is empty), and what's in them.
        dirs: HashMap<String, Vec<(String, EntryKind)>>,
        unlistable: bool,
    }
    impl Fake {
        fn new(exists: impl Fn(&str) -> bool + 'static) -> Self {
            Self {
                exists: Box::new(exists),
                kinds: HashMap::new(),
                links: HashMap::new(),
                dangling: false,
                dirs: HashMap::new(),
                unlistable: false,
            }
        }
        /// A file system from listings alone: every listed entry exists, as what it's listed as.
        fn listed(dirs: &[(&str, &[(&str, EntryKind)])]) -> Self {
            let mut all: HashMap<String, EntryKind> = HashMap::new();
            for (dir, list) in dirs {
                all.insert((*dir).into(), EntryKind::Dir);
                for (name, kind) in *list {
                    all.insert(format!("{dir}/{name}"), *kind);
                }
            }
            let names: Vec<String> = all.keys().cloned().collect();
            let mut fs = Fake::new(move |p| names.iter().any(|n| n == p));
            for (path, kind) in &all {
                match kind {
                    EntryKind::File => drop(fs.kinds.insert(path.clone(), FileKind::File)),
                    EntryKind::Symlink => drop(fs.kinds.insert(path.clone(), FileKind::Symlink)),
                    _ => {}
                }
            }
            fs.dirs = dirs.iter().map(|(d, l)| ((*d).into(), l.iter().map(|(n, k)| ((*n).into(), *k)).collect())).collect();
            fs
        }
        fn listed_kind(&self, path: &str) -> Option<EntryKind> {
            let (dir, name) = path.rsplit_once('/')?;
            let dir = if dir.is_empty() { "/" } else { dir };
            self.dirs.get(dir)?.iter().find(|(n, _)| n == name).map(|(_, k)| *k)
        }
    }
    impl FsProbe for Fake {
        fn entries(&self, dir: &Path) -> io::Result<Vec<(OsString, EntryKind)>> {
            if self.unlistable {
                return Err(io::Error::from(io::ErrorKind::PermissionDenied));
            }
            Ok(self
                .dirs
                .get(&*dir.to_string_lossy())
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|(n, k)| (n.into(), k))
                .collect())
        }
        fn entry_kind(&self, path: &Path) -> Option<EntryKind> {
            let p = path.to_string_lossy();
            if let Some(kind) = self.listed_kind(&p) {
                return Some(kind);
            }
            match self.kinds.get(&*p) {
                Some(FileKind::File) => Some(EntryKind::File),
                Some(FileKind::Symlink) => Some(EntryKind::Symlink),
                None => self.exists(path).then_some(EntryKind::Dir),
            }
        }
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
    fn marvs_runtime_folder_is_an_empty_tmpfs_after_the_project_and_toolchains() {
        // Under the project (which is mounted writable) and under a folder outside the hidden ones: hidden all the same.
        let fs = Fake::new(|p| p.ends_with(".bun") || p.contains("marv"));
        let opts = SandboxOptions {
            hidden: vec!["/home/me/proj/xdg/marv".into(), "/var/tmp/xdg/marv".into(), "/run/user/1000/marv".into()],
            ..base()
        };
        let args = joined(&opts, &fs);
        let project = args.find("--bind /home/me/proj /home/me/proj").unwrap();
        let toolchain = args.find("--ro-bind /home/me/.bun").unwrap();
        let hidden = args.find("--tmpfs /home/me/proj/xdg/marv").expect("hidden in the project");
        assert!(hidden > project && hidden > toolchain, "{args}");
        assert!(args.contains("--tmpfs /var/tmp/xdg/marv"), "{args}");
        assert!(!args.contains("--tmpfs /run/user/1000/marv"), "already hidden by the empty /run: {args}");
        // Under /run only where the project (a drive under /run/media) or a kept PATH folder shows it again.
        let media = SandboxOptions {
            root: "/run/media/me/usb/proj".into(),
            hidden: vec!["/run/media/me/usb/proj/xdg/marv".into(), "/run/user/1000/marv".into()],
            ..base()
        };
        let args = joined(&media, &fs);
        assert!(args.contains("--tmpfs /run/media/me/usb/proj/xdg/marv"), "{args}");
        assert!(!args.contains("--tmpfs /run/user/1000/marv"), "{args}");
        // A folder that doesn't exist yet gets nothing (bwrap can't create a mount point on the read-only root).
        let none = joined(&SandboxOptions { hidden: vec!["/run/user/1000/marv".into()], ..base() }, &default_fs());
        assert!(!none.contains("/run/user/1000/marv"), "{none}");
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
    fn binds_go_after_the_empty_run_read_only_unless_writable() {
        let opts = SandboxOptions {
            binds: vec![
                Bind { source: "/opt/marv".into(), dest: "/run/marv-bin".into(), writable: false },
                Bind { source: "/run/user/1000/marv/1-ab/p1".into(), dest: "/run/marv".into(), writable: true },
            ],
            ..base()
        };
        let args = joined(&opts, &default_fs());
        let run = args.find("--tmpfs /run").unwrap();
        assert!(run < args.find("--ro-bind /opt/marv /run/marv-bin").unwrap());
        assert!(run < args.find("--bind /run/user/1000/marv/1-ab/p1 /run/marv").unwrap());
        assert!(!args.contains("--ro-bind /run/user/1000/marv/1-ab/p1"));
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

    use EntryKind::{Channel, Dir, File as F, Symlink as L};

    /// A home folder with Marv's config, credentials, agent state, sockets in hidden and visible folders, toolchains.
    fn a_home() -> Fake {
        let mut fs = Fake::listed(&[
            (
                "/home/me",
                &[
                    (".bashrc", F),
                    (".cargo", Dir),
                    (".claude", Dir),
                    (".config", Dir),
                    (".gitconfig", F),
                    (".local", Dir),
                    (".marv", Dir),
                    (".ssh", Dir),
                    (".agent.sock", Channel),
                    (".dots", L),
                    ("Projects", Dir),
                    ("notes.txt", F),
                ],
            ),
            ("/home/me/.config", &[("herdr", Dir), ("git", Dir)]),
            ("/home/me/.config/herdr", &[("herdr.sock", Channel)]),
            ("/home/me/.local", &[("share", Dir), ("bin", Dir)]),
            ("/home/me/.local/share", &[("mise", Dir), ("keyrings", Dir)]),
            ("/home/me/.marv", &[("config.json", F), ("skills", Dir)]),
            ("/home/me/Projects", &[("run.sock", Channel), ("app", Dir), ("dotfiles", Dir)]),
            ("/home/me/Projects/app", &[("node_modules", Dir), ("pipe", Channel)]),
            ("/home/me/Projects/app/node_modules", &[("x.sock", Channel)]),
            ("/home/me/Projects/dotfiles", &[("secret", Dir)]),
        ]);
        fs.links.insert("/home/me/.dots".into(), "/home/me/Projects/dotfiles/secret".into());
        fs
    }
    fn at_home() -> SandboxOptions {
        SandboxOptions {
            root: "/home/me".into(),
            home: "/home/me".into(),
            path: "/usr/bin".into(),
            config_dir: Some("/home/me/.marv".into()),
            ..Default::default()
        }
    }
    /// Where `needle` last appears in the joined args.
    fn last(args: &str, needle: &str) -> usize {
        args.rfind(needle).unwrap_or_else(|| panic!("{needle} missing from {args}"))
    }

    #[test]
    fn a_home_folder_project_is_read_only_and_its_private_parts_are_masked_after_it() {
        let fs = a_home();
        let opts =
            SandboxOptions { protected: vec!["/home/me/.envrc".into()], placeholders: vec!["/home/me/.git".into()], ..at_home() };
        let args = joined(&opts, &fs);
        let project = last(&args, "--ro-bind /home/me /home/me");
        assert_eq!(args.matches("--tmpfs /home/me/.marv ").count(), 1, "the config folder once: {args}");
        assert!(!args.contains("--bind /home/me /home/me"), "never writable: {args}");
        // Every hidden top-level entry, after the read-only home.
        for mask in [
            "--tmpfs /home/me/.marv",
            "--tmpfs /home/me/.ssh",
            "--tmpfs /home/me/.claude",
            "--tmpfs /home/me/.config",
            "--tmpfs /home/me/.local",
            "--ro-bind /dev/null /home/me/.bashrc",
            "--ro-bind /dev/null /home/me/.agent.sock",
            // A hidden symlink hides what it leads to.
            "--tmpfs /home/me/Projects/dotfiles/secret",
            // Sockets and FIFOs in the visible part.
            "--ro-bind /dev/null /home/me/Projects/run.sock",
            "--ro-bind /dev/null /home/me/Projects/app/pipe",
        ] {
            assert!(last(&args, mask) > project, "{mask} must come after the home: {args}");
        }
        // Toolchains: left whole, or shown again after their hidden parent.
        assert!(!args.contains("--tmpfs /home/me/.cargo") && !args.contains("/dev/null /home/me/.gitconfig"), "{args}");
        for (parent, toolchain) in
            [(".local", ".local/share/mise"), (".local", ".local/bin"), (".config", ".config/git"), (".marv", ".marv/skills")]
        {
            let shown = last(&args, &format!("--ro-bind /home/me/{toolchain} /home/me/{toolchain}"));
            assert!(shown > last(&args, &format!("--tmpfs /home/me/{parent} ")), "{toolchain}: {args}");
        }
        // Not node_modules (skipped), and nothing protected on top of a read-only project.
        assert!(!args.contains("x.sock") && !args.contains(".envrc") && !args.contains(".git "), "{args}");
    }

    #[test]
    fn a_parent_of_the_home_is_read_only_too_and_the_root_keeps_the_stand_in() {
        let fs = a_home();
        let args = joined(&SandboxOptions { root: "/home".into(), ..at_home() }, &fs);
        let project = last(&args, "--ro-bind /home /home");
        assert!(!args.contains("--bind /home /home"));
        assert!(last(&args, "--tmpfs /home/me/.config") > project, "{args}");
        assert!(last(&args, "--ro-bind /dev/null /home/me/Projects/run.sock") > project, "{args}");
        // At / the first mount shows the project read-only already; binding / again would cover /tmp, /run and
        // the stand-in home.
        let args = strings(&SandboxOptions { root: "/".into(), ..at_home() }, &fs);
        assert_eq!(args.windows(3).filter(|w| w[1] == "/" && w[2] == "/").count(), 1, "{args:?}");
        assert!(!args.iter().any(|a| a == "--bind"), "{args:?}");
        assert!(args.join(" ").contains("--tmpfs /home/me"));
    }

    #[test]
    fn a_home_that_cant_be_listed_runs_nothing() {
        let mut fs = a_home();
        fs.unlistable = true;
        assert!(sandbox_args(&at_home(), &fs).is_err());
        // Elsewhere the home isn't listed at all.
        assert!(sandbox_args(&SandboxOptions { root: "/home/me/Projects/app".into(), ..at_home() }, &fs).is_ok());
    }

    #[test]
    fn a_subfolder_project_is_unchanged() {
        let fs = a_home();
        let args = joined(&SandboxOptions { root: "/home/me/Projects/app".into(), ..at_home() }, &fs);
        assert!(args.contains("--bind /home/me/Projects/app /home/me/Projects/app"), "{args}");
        // The stand-in hides the config folder already (a tmpfs on it would hide ~/.marv/skills).
        assert!(!args.contains("--tmpfs /home/me/.marv") && !args.contains("--tmpfs /home/me/.ssh"), "{args}");
        assert!(!args.contains("/dev/null /home/me/Projects/run.sock"), "{args}");
    }

    #[test]
    fn a_config_folder_outside_the_home_is_hidden_with_its_skills_shown() {
        let fs = Fake::listed(&[("/srv/marv-config", &[("config.json", F), ("skills", Dir)]), ("/home/me", &[])]);
        let args = joined(&SandboxOptions { config_dir: Some("/srv/marv-config".into()), ..base() }, &fs);
        let hidden = last(&args, "--tmpfs /srv/marv-config");
        assert!(hidden > last(&args, "--bind /home/me/proj"), "{args}");
        assert!(last(&args, "--ro-bind /srv/marv-config/skills /srv/marv-config/skills") > hidden, "{args}");
        // Never one that holds the project.
        let args = joined(&SandboxOptions { config_dir: Some("/home/me".into()), ..base() }, &fs);
        assert_eq!(args.matches("--tmpfs /home/me ").count(), 1, "only the stand-in: {args}");
    }
}
