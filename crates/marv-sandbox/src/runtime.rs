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
//! The override's parent must exist; it's judged by its real path, and nothing on the way may be a symlink (but the
//! root's `/tmp`) or changeable by another user ([`private_folder`]).
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
/// default is checked as before (hidden in every sandbox) and made where it's used.
pub fn runtime_dir(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    let home = env.get("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
    if let Some(dir) = env.get(RUNTIME_DIR_VAR).filter(|d| !d.is_empty()) {
        return check_override(Path::new(dir), &home).ok();
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
    let dir = env.get(RUNTIME_DIR_VAR).filter(|d| !d.is_empty())?;
    let home = env.get("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
    check_override(Path::new(dir), &home).err().map(|why| format!("{RUNTIME_DIR_VAR} {dir} isn't used: {why}"))
}

/// `MARV_RUNTIME_DIR`, checked like the default: absolute, short enough, its parent existing and judged by its real
/// path (macOS's `/tmp` is `/private/tmp`), hidden in every sandbox, then [`private_folder`]. Sockets are bound at the
/// path as given, which `private_folder` checks leads nowhere else.
fn check_override(dir: &Path, home: &Path) -> Result<PathBuf, String> {
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
    private_folder(dir).map_err(|e| e.to_string())?;
    Ok(dir.to_path_buf())
}

/// Makes `dir` this user's private folder, or refuses it. `dir` is absolute and plain (no `.` or `..`); its parent
/// exists, and nothing on the way is a symlink but the root's `/tmp` (macOS's link to `/private/tmp`): through a
/// linked parent a folder that looks hidden could be one sandboxes see. Every folder on the way belongs to this user or
/// root and isn't writable by others unless sticky (`/tmp`): nobody else can swap a parent before a socket is bound.
/// Only the last part is made, 0700 and not recursively; then it must be a folder, not a symlink, of this user's (an
/// existing one is made 0700 again).
fn private_folder(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    use std::path::Component;
    let refuse = |why: &str| Err(std::io::Error::other(why.to_string()));
    let uid = nix::unistd::geteuid().as_raw();
    let mut parts = dir.components();
    if parts.next() != Some(Component::RootDir) || !parts.all(|c| matches!(c, Component::Normal(_))) {
        return refuse("not an absolute, plain path");
    }
    for ancestor in dir.ancestors().skip(1) {
        let mut meta = std::fs::symlink_metadata(ancestor)?;
        if meta.file_type().is_symlink() {
            if ancestor != Path::new("/tmp") {
                // Not followed, not even a system's own (Fedora Silverblue's /home -> var/home): through it, a folder
                // that looks hidden could be one sandboxes see. The real path works.
                let real = std::fs::canonicalize(dir.parent().unwrap_or(dir))
                    .map(|p| format!(" (here {})", p.join(dir.file_name().unwrap_or_default()).display()))
                    .unwrap_or_default();
                return refuse(&format!("a symlink on the way: {}; use a path without symlinks{real}", ancestor.display()));
            }
            meta = std::fs::metadata(ancestor)?;
        }
        let mode = meta.mode();
        if !meta.is_dir() || (meta.uid() != uid && meta.uid() != 0) || (mode & 0o022 != 0 && mode & 0o1000 == 0) {
            return refuse(&format!("{} could be changed by another user", ancestor.display()));
        }
    }
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
}
