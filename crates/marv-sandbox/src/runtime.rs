//! Marv's runtime folder: where the mesh's doors and plugs and the process door's sockets live (marv-rs; #150).
//!
//! One rule, used by everything that makes or hides those sockets (discovery, the session's switchboard, the process
//! manager, the sandbox's hidden folders, the writable-folder check):
//! - `MARV_RUNTIME_DIR` from the real environment, when set: the folder itself, checked like the default (absolute,
//!   short enough for the longest socket, hidden in every sandbox, and a private folder of this user's).
//! - Linux: `$XDG_RUNTIME_DIR/marv`, when every sandbox hides `XDG_RUNTIME_DIR` ([`crate::hidden_in_sandbox`]).
//!   Without one there's no runtime folder: Marv never falls back to a folder a sandboxed command could see.
//! - macOS: `/tmp/marv-<uid>` (there's no `XDG_RUNTIME_DIR`). The path string `/tmp/…` is what sockets are bound at:
//!   it's shorter than `/private/tmp/…`, and `sun_path` is only 104 bytes there. Made 0700; then it must be a folder
//!   (not a symlink) this user owns. Another user who made ours first turns the mesh off, nothing more: `/tmp`'s
//!   sticky bit stops them replacing it.
//!
//! The override's parent must exist; it's judged by its real path, and it may not be inside the session's project
//! (#168: a sandboxed command writes there, and could make the folder, or what's in it, before Marv does). Nothing on
//! the way may be changeable by another user, nor a symlink but the root's `/tmp` and a root-owned link in a folder
//! only root can change (Fedora Silverblue's `/home -> var/home`), whose target's folders are walked too
//! ([`private_folder`]). Every folder on the way must be this user's or root's: in a container whose `/` belongs to a
//! third uid every override is refused (that uid could swap anything under `/`); there the Linux default
//! (`XDG_RUNTIME_DIR` under `/run`) still works.
//!
//! Inside it: `mesh/<name>.sock` (doors), `p/<pid>-<hex>/<plug>.sock` (one switchboard's plugs; a process can have
//! several, Marv desktop's chats) and `<pid>-<hex>/p<n>/` (the process door's sockets). The plug folder isn't named
//! after the session: the session name is up to 43 bytes and the plug id up to 48, which together overflowed
//! `sun_path` on both platforms.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The override's variable.
pub const RUNTIME_DIR_VAR: &str = "MARV_RUNTIME_DIR";

/// `sizeof(sockaddr_un.sun_path)`: a socket's path must be shorter (it ends in a NUL).
#[cfg(target_os = "macos")]
pub const SUN_PATH: usize = 104;
#[cfg(not(target_os = "macos"))]
pub const SUN_PATH: usize = 108;

/// What the longest socket adds to the runtime folder: `/p/` + a pid (7 digits) + `-` + 4 hex digits (one folder per
/// switchboard) + `/` + a plug id (48) + `.sock`.
const LONGEST_SOCKET_TAIL: usize = 3 + 7 + 1 + 4 + 1 + 48 + 5;

/// The longest runtime folder whose every socket fits `sun_path`.
pub const MAX_RUNTIME_DIR: usize = SUN_PATH - 1 - LONGEST_SOCKET_TAIL;

/// The rule as messages name it ("the mesh needs …").
#[cfg(target_os = "macos")]
pub const RUNTIME_DIR_RULE: &str = "a private runtime folder (/tmp/marv-<uid>, or MARV_RUNTIME_DIR)";
#[cfg(not(target_os = "macos"))]
pub const RUNTIME_DIR_RULE: &str = "XDG_RUNTIME_DIR under /run, /tmp or your home folder (or MARV_RUNTIME_DIR)";

/// The variables [`runtime_dir`] reads, from this process's real environment.
pub fn runtime_env() -> BTreeMap<String, String> {
    ["HOME", "XDG_RUNTIME_DIR", RUNTIME_DIR_VAR]
        .into_iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect()
}

/// Every folder the rule could put the runtime folder in, unchecked: what the sandbox hides and the writable check
/// refuses (more hidden is harmless). The platform's default and, when set, `MARV_RUNTIME_DIR` (a relative one taken
/// from the current folder, though [`runtime_dir`] never uses it).
pub fn runtime_paths(env: &BTreeMap<String, String>) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = default_path(env).into_iter().collect();
    if let Some(dir) = env.get(RUNTIME_DIR_VAR).filter(|d| !d.is_empty()).map(PathBuf::from) {
        let dir = if dir.is_absolute() {
            dir
        } else {
            match std::env::current_dir() {
                Ok(cwd) => cwd.join(dir),
                Err(_) => return paths,
            }
        };
        if !paths.contains(&dir) {
            paths.push(dir);
        }
    }
    paths
}

#[cfg(target_os = "macos")]
fn default_path(_env: &BTreeMap<String, String>) -> Option<PathBuf> {
    Some(PathBuf::from(format!("/tmp/marv-{}", nix::unistd::geteuid().as_raw())))
}

#[cfg(not(target_os = "macos"))]
fn default_path(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    env.get("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|d| d.is_absolute()).map(|d| d.join("marv"))
}

/// The runtime folder, checked (see the module doc); `None` when there's none Marv may use, and the mesh and the
/// processes' sockets are off. The macOS default and the override are made here ([`private_folder`]); the Linux
/// default is checked as before (hidden in every sandbox) and made where it's used. For a session, use
/// [`runtime_dir_for`] with its project, which refuses an override inside it.
pub fn runtime_dir(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    runtime_dir_for(env, None)
}

/// [`runtime_dir`] for a session in `project` (its root): an override inside the project, or holding it, is refused
/// (#168). `None` where no project is known (`marv mesh ls`), checked as before.
pub fn runtime_dir_for(env: &BTreeMap<String, String>, project: Option<&Path>) -> Option<PathBuf> {
    let home = env.get("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
    if let Some(dir) = env.get(RUNTIME_DIR_VAR).filter(|d| !d.is_empty()) {
        return check_override(Path::new(dir), &home, project).ok();
    }
    if cfg!(target_os = "macos") {
        let dir = default_path(env)?;
        return private_folder(&dir).is_ok().then_some(dir);
    }
    let base = env.get("XDG_RUNTIME_DIR").filter(|d| crate::hidden_in_sandbox(Path::new(d), &home))?;
    Some(Path::new(base).join("marv"))
}

/// Why `MARV_RUNTIME_DIR` was refused, for the message that says the mesh is off; `None` when it's unset or accepted.
pub fn override_refusal(env: &BTreeMap<String, String>) -> Option<String> {
    override_refusal_for(env, None)
}

/// [`override_refusal`] for a session in `project` ([`runtime_dir_for`]).
pub fn override_refusal_for(env: &BTreeMap<String, String>, project: Option<&Path>) -> Option<String> {
    let dir = env.get(RUNTIME_DIR_VAR).filter(|d| !d.is_empty())?;
    let home = env.get("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
    check_override(Path::new(dir), &home, project).err().map(|why| format!("{RUNTIME_DIR_VAR} {dir} isn't used: {why}"))
}

/// `MARV_RUNTIME_DIR`, checked like the default: absolute, short enough, its parent existing and judged by its real
/// path (macOS's `/tmp` is `/private/tmp`), hidden in every sandbox, not inside the project nor holding it, then
/// [`private_folder`]. Sockets are bound at the path as given, which `private_folder` checks leads nowhere else.
fn check_override(dir: &Path, home: &Path, project: Option<&Path>) -> Result<PathBuf, String> {
    if !dir.is_absolute() {
        return Err("it isn't an absolute path".into());
    }
    if dir.as_os_str().len() > MAX_RUNTIME_DIR {
        return Err(format!("it's longer than {MAX_RUNTIME_DIR} bytes, too long for the mesh's sockets"));
    }
    let (Some(parent), Some(name)) = (dir.parent(), dir.file_name()) else {
        return Err("it names no folder".into());
    };
    let real = std::fs::canonicalize(parent).map_err(|e| format!("its parent {}: {e}", parent.display()))?.join(name);
    if !crate::hidden_in_sandbox(&real, home) {
        return Err("sandboxed commands could see it: use a folder under /tmp, /run or a hidden folder of your home".into());
    }
    // A sandboxed command runs as this user and writes in the project: it could make the folder, or its `mesh` and
    // `p` folders, before Marv does (#168). Judged both as given and by real paths; a project whose real path can't
    // be read is judged as given.
    // The folder's own real path too, when it exists: on a case-insensitive volume (macOS's APFS) `/tmp/Proj` is
    // the project `/private/tmp/proj`, which the byte comparison above misses, and `private_folder` would make that
    // project 0700. Checked again once it's made.
    let own_real = || std::fs::canonicalize(dir).ok();
    if let Some(project) = project {
        clear_of_project(&[dir, &real], own_real().as_deref(), project)?;
    }
    private_folder(dir).map_err(|e| e.to_string())?;
    if let Some(project) = project {
        clear_of_project(&[dir, &real], own_real().as_deref(), project)?;
    }
    Ok(dir.to_path_buf())
}

/// Refuses a runtime folder (`paths`: as given and with its parent's real path; `own_real`: its own real path, when
/// it exists) inside `project` or holding it, comparing given and real paths of both.
fn clear_of_project(paths: &[&Path], own_real: Option<&Path>, project: &Path) -> Result<(), String> {
    let real_project = std::fs::canonicalize(project).unwrap_or_else(|_| project.to_path_buf());
    let paths: Vec<&Path> = paths.iter().copied().chain(own_real).collect();
    let projects = [project, real_project.as_path()];
    if paths.iter().any(|d| projects.iter().any(|p| d.starts_with(p))) {
        return Err(format!(
            "it's inside the project ({}), where sandboxed commands could make it first: use a folder outside it",
            project.display()
        ));
    }
    if paths.iter().any(|d| projects.iter().any(|p| p.starts_with(d))) {
        return Err(format!("it holds the project ({}): use a folder outside it", project.display()));
    }
    Ok(())
}

/// Makes `dir` this user's private folder, or refuses it. `dir` is absolute and plain (no `.` or `..`); its parent
/// exists, and the folders on the way pass [`check_ancestors`]. Only the last part is made, 0700 and not recursively;
/// then it must be a folder, not a symlink, of this user's (an existing one is made 0700 again). Also for the
/// processes' fallback folder (`marv_session::processes`), one level at a time.
pub fn private_folder(dir: &Path) -> std::io::Result<()> {
    private_folder_as(dir, nix::unistd::geteuid().as_raw())
}

/// [`private_folder`] for `uid` (tests pass another uid: a folder of someone else's can't be made without root).
fn private_folder_as(dir: &Path, uid: u32) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    use std::path::Component;
    let refuse = |why: &str| Err(std::io::Error::other(why.to_string()));
    let mut parts = dir.components();
    if parts.next() != Some(Component::RootDir) || !parts.all(|c| matches!(c, Component::Normal(_))) {
        return refuse("not an absolute, plain path");
    }
    check_ancestors(dir, uid, &RealFs)?;
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.is_dir() || meta.uid() != uid {
        return refuse(&format!("{} isn't a folder of this user's", dir.display()));
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// What [`check_ancestors`] reads about one path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Node {
    symlink: bool,
    dir: bool,
    uid: u32,
    mode: u32,
}

/// The file system as [`check_ancestors`] sees it: the real one, or a test's.
trait Probe {
    /// The path itself (a symlink isn't followed).
    fn lstat(&self, path: &Path) -> std::io::Result<Node>;
    /// Where it leads (symlinks followed).
    fn stat(&self, path: &Path) -> std::io::Result<Node>;
    fn realpath(&self, path: &Path) -> std::io::Result<PathBuf>;
    /// A link's own target, as stored (one hop).
    fn read_link(&self, path: &Path) -> std::io::Result<PathBuf>;
}

struct RealFs;

impl RealFs {
    fn node(meta: std::fs::Metadata) -> Node {
        use std::os::unix::fs::MetadataExt;
        Node { symlink: meta.file_type().is_symlink(), dir: meta.is_dir(), uid: meta.uid(), mode: meta.mode() }
    }
}

impl Probe for RealFs {
    fn lstat(&self, path: &Path) -> std::io::Result<Node> {
        std::fs::symlink_metadata(path).map(Self::node)
    }
    fn stat(&self, path: &Path) -> std::io::Result<Node> {
        std::fs::metadata(path).map(Self::node)
    }
    fn realpath(&self, path: &Path) -> std::io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }
    fn read_link(&self, path: &Path) -> std::io::Result<PathBuf> {
        std::fs::read_link(path)
    }
}

/// Every folder on the way to `dir` (not `dir` itself) belongs to this user (`uid`) or root and isn't writable by
/// others unless sticky (`/tmp`): nobody else can swap a parent before a socket is bound. A third uid is refused even
/// when it owns `/` (a container's): it could rename anything under `/`, and Marv can't tell an image's build user
/// from another person (#168).
///
/// Nothing on the way may be a symlink, with two exceptions: the root's `/tmp` (macOS's link to `/private/tmp`), and
/// a link root owns in a folder only root can change (not even sticky-writable by others), such as Fedora
/// Silverblue's `/home -> var/home` (#168). Nobody but root can change where that one leads. It's followed one hop
/// at a time (its stored target, never the fully resolved path): the target and each of its parents are walked the
/// same way, so a further link in a chain, and the folder it sits in, get the same checks. Any other symlink is
/// refused, not followed: through it, a folder that looks hidden could be one sandboxes see.
fn check_ancestors(dir: &Path, uid: u32, fs: &dyn Probe) -> std::io::Result<()> {
    match dir.parent() {
        Some(parent) => check_chain(parent, dir, uid, fs, 0),
        None => Ok(()),
    }
}

/// How many root-owned links one walk follows (a loop is refused, not followed forever).
const MAX_LINK_HOPS: usize = 8;

/// `path` and each of its parents, for [`check_ancestors`] (`dir`: the runtime folder, for messages).
fn check_chain(path: &Path, dir: &Path, uid: u32, fs: &dyn Probe, hops: usize) -> std::io::Result<()> {
    let refuse = |why: String| Err(std::io::Error::other(why));
    for ancestor in path.ancestors() {
        let mut node = fs.lstat(ancestor)?;
        if node.symlink {
            if ancestor == Path::new("/tmp") {
                node = fs.stat(ancestor)?;
            } else if root_only_link(ancestor, node, fs)? {
                if hops >= MAX_LINK_HOPS {
                    return refuse(format!("too many symlinks on the way at {}", ancestor.display()));
                }
                let target = link_target(ancestor, fs)?;
                check_chain(&target, dir, uid, fs, hops + 1)?;
                // The link's own folders: the rest of this walk.
                continue;
            } else {
                // The real path works, when it's one Marv accepts.
                let real = fs
                    .realpath(dir.parent().unwrap_or(dir))
                    .map(|p| format!(" (here {})", p.join(dir.file_name().unwrap_or_default()).display()))
                    .unwrap_or_default();
                return refuse(format!("a symlink on the way: {}; use a path without symlinks{real}", ancestor.display()));
            }
        }
        folder_ok(ancestor, node, uid)?;
    }
    Ok(())
}

/// Whether `link` is a link only root could have made or change: root owns it, and its folder is a root-owned folder
/// nobody else may write in (no sticky-bit exception: a sticky folder others write in could hold a link root made for
/// someone else's purpose).
fn root_only_link(link: &Path, node: Node, fs: &dyn Probe) -> std::io::Result<bool> {
    let Some(parent) = link.parent() else { return Ok(false) };
    let folder = fs.stat(parent)?;
    Ok(node.uid == 0 && folder.dir && folder.uid == 0 && folder.mode & 0o022 == 0)
}

/// `link`'s stored target, absolute (a relative one is joined to the link's folder) and plain: one with `.` or `..`
/// in it is refused rather than resolved, since `..` after a further link climbs out of where that link leads.
fn link_target(link: &Path, fs: &dyn Probe) -> std::io::Result<PathBuf> {
    use std::path::Component;
    let stored = fs.read_link(link)?;
    let target = if stored.is_absolute() { stored } else { link.parent().unwrap_or(Path::new("/")).join(stored) };
    let mut parts = target.components();
    if parts.next() != Some(Component::RootDir) || !parts.all(|c| matches!(c, Component::Normal(_))) {
        return Err(std::io::Error::other(format!(
            "{} leads to {}, not a plain path; use a path without symlinks",
            link.display(),
            target.display()
        )));
    }
    Ok(target)
}

/// One folder on the way: a folder, this user's or root's, not writable by others unless sticky.
fn folder_ok(path: &Path, node: Node, uid: u32) -> std::io::Result<()> {
    let refuse = |why: String| Err(std::io::Error::other(why));
    if !node.dir {
        return refuse(format!("{} isn't a folder", path.display()));
    }
    if node.uid != uid && node.uid != 0 {
        // Linux's default isn't walked: it's judged by `hidden_in_sandbox` alone.
        let instead = if cfg!(target_os = "macos") { "" } else { "; use XDG_RUNTIME_DIR under /run instead" };
        return refuse(format!(
            "{} belongs to uid {}, neither you nor root, who could change it{instead}",
            path.display(),
            node.uid
        ));
    }
    if node.mode & 0o022 != 0 && node.mode & 0o1000 == 0 {
        return refuse(format!("{} could be changed by another user", path.display()));
    }
    Ok(())
}

/// Refuses a socket path `bind` would reject (`EINVAL`) or cut, with a message that says what to do.
pub fn check_socket_path(path: &Path) -> Result<(), String> {
    let len = path.as_os_str().len();
    if len >= SUN_PATH {
        return Err(format!(
            "{} is {len} bytes, too long for a socket here (at most {}): set {RUNTIME_DIR_VAR} to a shorter folder",
            path.display(),
            SUN_PATH - 1
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn short_temp(tag: &str) -> PathBuf {
        let dir = PathBuf::from(format!("/tmp/mx-rt-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_longest_plug_socket_fits_in_the_longest_runtime_folder() {
        let dir = "/".to_string() + &"r".repeat(MAX_RUNTIME_DIR - 1);
        let plug = format!("{dir}/p/4194304-ffff/{}.sock", "x".repeat(48));
        assert_eq!(plug.len(), SUN_PATH - 1);
        assert!(check_socket_path(Path::new(&plug)).is_ok());
        let over = format!("{dir}x/p/4194304-ffff/{}.sock", "x".repeat(48));
        let err = check_socket_path(Path::new(&over)).unwrap_err();
        assert!(err.contains("too long") && err.contains(RUNTIME_DIR_VAR), "{err}");
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn linux_uses_xdg_runtime_dir_marv_when_the_sandbox_hides_it() {
        assert_eq!(runtime_dir(&env(&[("XDG_RUNTIME_DIR", "/run/user/1000")])), Some("/run/user/1000/marv".into()));
        assert_eq!(runtime_dir(&env(&[("XDG_RUNTIME_DIR", "relative")])), None);
        assert_eq!(runtime_dir(&env(&[("XDG_RUNTIME_DIR", "/var/tmp/x")])), None);
        assert_eq!(runtime_dir(&env(&[])), None);
        assert_eq!(runtime_paths(&env(&[("XDG_RUNTIME_DIR", "/var/tmp/x")])), [PathBuf::from("/var/tmp/x/marv")]);
        // The default and the override are both hidden.
        let both = runtime_paths(&env(&[("XDG_RUNTIME_DIR", "/run/user/1"), (RUNTIME_DIR_VAR, "/tmp/o")]));
        assert_eq!(both, [PathBuf::from("/run/user/1/marv"), PathBuf::from("/tmp/o")]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_uses_a_private_tmp_folder_named_for_the_user() {
        let uid = nix::unistd::geteuid().as_raw();
        let want = PathBuf::from(format!("/tmp/marv-{uid}"));
        assert_eq!(runtime_paths(&env(&[("XDG_RUNTIME_DIR", "/run/user/1")])), std::slice::from_ref(&want));
        assert_eq!(runtime_paths(&env(&[(RUNTIME_DIR_VAR, "/tmp/o")])), [want.clone(), PathBuf::from("/tmp/o")]);
        assert_eq!(runtime_dir(&env(&[])), Some(want));
    }

    #[test]
    fn the_override_is_made_private_and_checked_like_the_default() {
        use std::os::unix::fs::PermissionsExt;
        let dir = short_temp("ov");
        // Its parent must exist.
        let value = dir.join("marv");
        assert_eq!(runtime_dir(&env(&[(RUNTIME_DIR_VAR, value.to_str().unwrap())])), None);
        std::fs::create_dir(&dir).unwrap();
        let got = runtime_dir(&env(&[(RUNTIME_DIR_VAR, value.to_str().unwrap()), ("XDG_RUNTIME_DIR", "/run/user/1")]));
        assert_eq!(got, Some(value.clone()));
        assert_eq!(override_refusal(&env(&[(RUNTIME_DIR_VAR, value.to_str().unwrap())])), None);
        assert_eq!(std::fs::metadata(&value).unwrap().permissions().mode() & 0o777, 0o700);
        // Relative, too long for the longest socket, or a symlink: refused, never another folder in its place.
        assert_eq!(runtime_dir(&env(&[(RUNTIME_DIR_VAR, "rel/marv")])), None);
        let long = format!("/tmp/{}", "l".repeat(MAX_RUNTIME_DIR));
        assert_eq!(runtime_dir(&env(&[(RUNTIME_DIR_VAR, &long)])), None);
        assert!(!Path::new(&long).exists());
        let link = dir.join("link");
        std::os::unix::fs::symlink(&value, &link).unwrap();
        assert_eq!(runtime_dir(&env(&[(RUNTIME_DIR_VAR, link.to_str().unwrap())])), None);
        // A parent that's a symlink: the folder could be made wherever it leads (here outside /tmp).
        let out = PathBuf::from(format!("/var/tmp/mx-rt-out-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        if std::fs::create_dir(&out).is_ok() {
            std::os::unix::fs::symlink(&out, dir.join("up")).unwrap();
            assert_eq!(runtime_dir(&env(&[(RUNTIME_DIR_VAR, dir.join("up/marv").to_str().unwrap())])), None);
            assert!(!out.join("marv").exists());
            let _ = std::fs::remove_dir_all(&out);
        }
        // A symlinked parent that leads somewhere hidden too (Fedora Silverblue's /home -> var/home): still refused,
        // and the refusal says why and what to use instead.
        std::fs::create_dir(dir.join("real")).unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("alias")).unwrap();
        let via_link = env(&[(RUNTIME_DIR_VAR, dir.join("alias/marv").to_str().unwrap())]);
        assert_eq!(runtime_dir(&via_link), None);
        let why = override_refusal(&via_link).unwrap();
        assert!(why.contains(&format!("a symlink on the way: {}", dir.join("alias").display())), "{why}");
        assert!(why.contains("without symlinks") && why.contains("real/marv"), "{why}");
        // Not plain.
        let dotted = format!("{}/../marv", dir.display());
        assert_eq!(runtime_dir(&env(&[(RUNTIME_DIR_VAR, &dotted)])), None);
        // One a sandboxed command could see (not under /tmp, /run or a hidden home entry): refused, and not made.
        assert_eq!(runtime_dir(&env(&[(RUNTIME_DIR_VAR, "/var/marv-mx-nope")])), None);
        assert!(!Path::new("/var/marv-mx-nope").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_override_inside_the_project_or_holding_it_is_refused() {
        // #168: a sandboxed command writes in the project, and could make the folder (or its `mesh`) first.
        let dir = short_temp("proj");
        std::fs::create_dir_all(dir.join("proj/run")).unwrap();
        std::fs::create_dir_all(dir.join("other")).unwrap();
        let project = dir.join("proj");
        let inside = env(&[(RUNTIME_DIR_VAR, dir.join("proj/run/marv").to_str().unwrap())]);
        assert_eq!(runtime_dir_for(&inside, Some(&project)), None);
        assert!(!dir.join("proj/run/marv").exists(), "not made");
        let why = override_refusal_for(&inside, Some(&project)).unwrap();
        assert!(why.contains("inside the project") && why.contains(&project.display().to_string()), "{why}");
        // Holding the project: the project's folder is in the runtime folder.
        let holding = env(&[(RUNTIME_DIR_VAR, dir.to_str().unwrap())]);
        let why = override_refusal_for(&holding, Some(&project)).unwrap();
        assert!(why.contains("holds the project"), "{why}");
        // Through a symlink to the project: judged by real paths too.
        std::os::unix::fs::symlink(&project, dir.join("p2")).unwrap();
        let via = env(&[(RUNTIME_DIR_VAR, dir.join("proj/run/marv").to_str().unwrap())]);
        assert_eq!(runtime_dir_for(&via, Some(&dir.join("p2"))), None);
        // Outside it: used. Without a project (`marv mesh ls`) the same override is judged as before.
        let outside = env(&[(RUNTIME_DIR_VAR, dir.join("other/marv").to_str().unwrap())]);
        assert_eq!(runtime_dir_for(&outside, Some(&project)), Some(dir.join("other/marv")));
        assert_eq!(runtime_dir(&inside), Some(dir.join("proj/run/marv")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_folders_own_real_path_is_compared_with_the_project_too() {
        // macOS's case-insensitive APFS: `/tmp/Proj` is the project `/private/tmp/proj`, though no byte comparison of
        // the given path (nor its parent's real path joined to the name as given) says so. The folder's own real path
        // (`canonicalize`, which returns the stored case) does.
        let project = Path::new("/mx-no-such/private/tmp/proj");
        let given = [Path::new("/mx-no-such/tmp/Proj"), Path::new("/mx-no-such/private/tmp/Proj")];
        assert_eq!(clear_of_project(&given, None, project), Ok(()));
        let why = clear_of_project(&given, Some(project), project).unwrap_err();
        assert!(why.contains("inside the project"), "{why}");
        let why = clear_of_project(&given, Some(Path::new("/mx-no-such/private/tmp")), project).unwrap_err();
        assert!(why.contains("holds the project"), "{why}");
    }

    #[test]
    fn a_private_folder_is_made_0700_and_refused_when_it_isnt_this_users_folder() {
        use std::os::unix::fs::PermissionsExt;
        let dir = short_temp("pf");
        // Made 0700; an existing one with a wider mode is made 0700 again.
        private_folder(&dir).unwrap();
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        private_folder(&dir).unwrap();
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        // Owned by another uid: refused, its mode left alone.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let uid = nix::unistd::geteuid().as_raw();
        let why = private_folder_as(&dir, uid + 1).unwrap_err().to_string();
        assert!(why.contains("isn't a folder of this user's"), "{why}");
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o755);
        // A symlink to a folder of this user's, or a file: refused.
        let link = PathBuf::from(format!("{}-l", dir.display()));
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert!(private_folder(&link).is_err());
        std::fs::write(dir.join("f"), "").unwrap();
        assert!(private_folder(&dir.join("f")).is_err());
        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file system for [`check_ancestors`]: folders and links, every other path missing.
    #[derive(Default)]
    struct Fake {
        nodes: BTreeMap<PathBuf, Node>,
        links: BTreeMap<PathBuf, PathBuf>,
    }

    impl Fake {
        fn dir(mut self, path: &str, uid: u32, mode: u32) -> Self {
            self.nodes.insert(path.into(), Node { symlink: false, dir: true, uid, mode });
            self
        }
        fn link(mut self, path: &str, uid: u32, target: &str) -> Self {
            self.nodes.insert(path.into(), Node { symlink: true, dir: false, uid, mode: 0o777 });
            self.links.insert(path.into(), target.into());
            self
        }
        /// A plain Linux root: `/` and `/var` root's, 0755.
        fn root() -> Self {
            Fake::default().dir("/", 0, 0o755).dir("/var", 0, 0o755)
        }
    }

    impl Probe for Fake {
        /// Like `lstat(2)`: links on the way to the last part are followed, the last part's isn't.
        fn lstat(&self, path: &Path) -> std::io::Result<Node> {
            let at = match (path.parent(), path.file_name()) {
                (Some(parent), Some(name)) => self.realpath(parent)?.join(name),
                _ => path.to_path_buf(),
            };
            self.raw(&at)
        }
        fn stat(&self, path: &Path) -> std::io::Result<Node> {
            self.lstat(&self.realpath(path)?)
        }
        fn realpath(&self, path: &Path) -> std::io::Result<PathBuf> {
            let mut out = PathBuf::from("/");
            for part in path.components().skip(1) {
                out.push(part);
                for _ in 0..8 {
                    match self.links.get(&out) {
                        Some(target) => {
                            let joined = out.parent().unwrap_or(Path::new("/")).join(target);
                            // Lexically, as the kernel does once the folders on the way are real ones.
                            out = PathBuf::from("/");
                            for c in joined.components().skip(1) {
                                match c {
                                    std::path::Component::ParentDir => {
                                        out.pop();
                                    }
                                    std::path::Component::CurDir => {}
                                    c => out.push(c),
                                }
                            }
                        }
                        None => break,
                    }
                }
                self.raw(&out)?;
            }
            Ok(out)
        }
        fn read_link(&self, path: &Path) -> std::io::Result<PathBuf> {
            let at = match (path.parent(), path.file_name()) {
                (Some(parent), Some(name)) => self.realpath(parent)?.join(name),
                _ => path.to_path_buf(),
            };
            self.read_link_raw(&at)
        }
    }

    impl Fake {
        fn read_link_raw(&self, path: &Path) -> std::io::Result<PathBuf> {
            self.links.get(path).cloned().ok_or_else(|| std::io::ErrorKind::InvalidInput.into())
        }
        fn raw(&self, path: &Path) -> std::io::Result<Node> {
            self.nodes.get(path).copied().ok_or_else(|| std::io::ErrorKind::NotFound.into())
        }
    }

    fn walk(fs: &Fake, dir: &str) -> Result<(), String> {
        check_ancestors(Path::new(dir), 1000, fs).map_err(|e| e.to_string())
    }

    /// Fedora Silverblue: `/home -> var/home`, root's, in `/`.
    fn silverblue() -> Fake {
        Fake::root().dir("/var/home", 0, 0o755).dir("/var/home/u", 1000, 0o700).dir("/var/home/u/.c", 1000, 0o700)
    }

    #[test]
    fn a_root_owned_symlink_in_a_folder_only_root_changes_is_followed_and_its_target_walked() {
        // #168: Silverblue's /home -> var/home no longer refuses an override in the home's hidden folders.
        let fs = silverblue().link("/home", 0, "/var/home");
        assert_eq!(walk(&fs, "/home/u/.c/marv"), Ok(()));
        // The link's own folders are still walked: a home folder another user could change.
        let fs = silverblue().link("/home", 0, "/var/home").dir("/var/home/u/.c", 1000, 0o777);
        assert!(walk(&fs, "/home/u/.c/marv").unwrap_err().contains("could be changed by another user"));
        // The target's parents are walked too: a /var another user owns, or one others may write in.
        let fs = silverblue().link("/home", 0, "/var/home").dir("/var", 2000, 0o755);
        assert!(walk(&fs, "/home/u/.c/marv").unwrap_err().contains("/var belongs to uid 2000"));
        let fs = silverblue().link("/home", 0, "/var/home").dir("/var", 0, 0o777);
        assert!(walk(&fs, "/home/u/.c/marv").unwrap_err().contains("/var could be changed"));
        // A link that leads to something that isn't a folder.
        let fs = silverblue().link("/home", 0, "/var/home").link("/var/home", 0, "/var/f");
        let mut fs = fs;
        fs.nodes.insert("/var/f".into(), Node { symlink: false, dir: false, uid: 0, mode: 0o644 });
        assert!(walk(&fs, "/home/u/.c/marv").is_err());
    }

    #[test]
    fn a_chain_of_links_is_followed_one_hop_at_a_time() {
        // The review's case: root's /srv/l -> /opt/a/l2, and /opt/a/l2 (another uid's, in a folder that uid may
        // write) -> /var/home. Resolving the whole chain at once only walked /var/home: that uid could repoint l2.
        let chain = || silverblue().dir("/srv", 0, 0o755).dir("/opt", 0, 0o755).link("/srv/l", 0, "/opt/a/l2");
        let fs = chain().dir("/opt/a", 2000, 0o777).link("/opt/a/l2", 2000, "/var/home");
        assert!(walk(&fs, "/srv/l/u/.c/marv").unwrap_err().contains("a symlink on the way: /opt/a/l2"));
        // A root-owned second link, but in a folder others may write in (or a third uid's): refused too.
        let fs = chain().dir("/opt/a", 0, 0o777).link("/opt/a/l2", 0, "/var/home");
        assert!(walk(&fs, "/srv/l/u/.c/marv").unwrap_err().contains("a symlink on the way: /opt/a/l2"));
        let fs = chain().dir("/opt/a", 2000, 0o755).link("/opt/a/l2", 0, "/var/home");
        assert!(walk(&fs, "/srv/l/u/.c/marv").is_err());
        // Two links only root can change, each in a root-only folder: followed.
        let fs = chain().dir("/opt/a", 0, 0o755).link("/opt/a/l2", 0, "/var/home");
        assert_eq!(walk(&fs, "/srv/l/u/.c/marv"), Ok(()));
        // A relative target (Silverblue's own is `var/home`) is joined to the link's folder.
        let fs = silverblue().link("/home", 0, "var/home");
        assert_eq!(walk(&fs, "/home/u/.c/marv"), Ok(()));
        // A target with `..` in it is refused, not resolved; a loop is refused, not followed forever.
        let fs = silverblue().dir("/srv", 0, 0o755).link("/home", 0, "/srv/../var/home");
        assert!(walk(&fs, "/home/u/.c/marv").unwrap_err().contains("not a plain path"));
        let fs = Fake::root().link("/a", 0, "/b").link("/b", 0, "/a");
        assert!(walk(&fs, "/a/marv").is_err());
    }

    #[test]
    fn any_other_symlink_on_the_way_is_still_refused() {
        // A link the user owns (anyone running as the user could repoint it: a sandboxed command can't reach /home,
        // but the rule doesn't depend on that).
        let fs = silverblue().link("/home", 1000, "/var/home");
        assert!(walk(&fs, "/home/u/.c/marv").unwrap_err().contains("a symlink on the way: /home"));
        // Root's link, in a folder others may write in, even sticky (/tmp-like): someone else could have put it there.
        let fs = silverblue().dir("/srv", 0, 0o1777).link("/srv/h", 0, "/var/home");
        assert!(walk(&fs, "/srv/h/u/.c/marv").unwrap_err().contains("a symlink on the way: /srv/h"));
        // Root's link, in a folder a third uid owns.
        let fs = silverblue().dir("/srv", 2000, 0o755).link("/srv/h", 0, "/var/home");
        assert!(walk(&fs, "/srv/h/u/.c/marv").is_err());
        // Root's link, in a root folder its group may write in.
        let fs = silverblue().dir("/srv", 0, 0o775).link("/srv/h", 0, "/var/home");
        assert!(walk(&fs, "/srv/h/u/.c/marv").unwrap_err().contains("a symlink on the way: /srv/h"));
    }

    #[test]
    fn a_root_folder_owned_by_a_third_uid_refuses_every_override_and_says_why() {
        // #168: a container whose `/` belongs to neither root nor the user. That uid could rename anything under `/`,
        // so it's refused, with the reason (and, on Linux, what to use instead).
        let fs = Fake::default().dir("/", 5000, 0o755).dir("/tmp", 0, 0o1777);
        let why = walk(&fs, "/tmp/marv").unwrap_err();
        assert!(why.contains("/ belongs to uid 5000, neither you nor root"), "{why}");
        #[cfg(not(target_os = "macos"))]
        assert!(why.contains("XDG_RUNTIME_DIR under /run"), "{why}");
        // Root's or the user's: accepted.
        assert_eq!(walk(&Fake::default().dir("/", 0, 0o755).dir("/tmp", 0, 0o1777), "/tmp/marv"), Ok(()));
        assert_eq!(walk(&Fake::default().dir("/", 1000, 0o755).dir("/tmp", 0, 0o1777), "/tmp/marv"), Ok(()));
    }

    #[test]
    fn the_systems_own_root_symlinks_are_followed_on_this_machine() {
        // The real file system: a root-owned link in `/` (Arch's and Ubuntu's /bin -> usr/bin, macOS's /var and
        // /etc), walked like Silverblue's /home.
        use std::os::unix::fs::MetadataExt;
        let root_link = ["/bin", "/sbin", "/lib", "/var", "/etc"].into_iter().map(Path::new).find(|p| {
            std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink() && m.uid() == 0)
                && std::fs::metadata(p).is_ok_and(|m| m.is_dir())
        });
        let Some(link) = root_link else {
            eprintln!("no root-owned symlink in / here: skipped");
            return;
        };
        let uid = nix::unistd::geteuid().as_raw();
        let got = check_ancestors(&link.join("marv"), uid, &RealFs);
        assert!(got.is_ok(), "{}: {got:?}", link.display());
    }
}
