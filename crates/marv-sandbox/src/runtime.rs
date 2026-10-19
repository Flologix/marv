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
//! Inside it: `mesh/<name>.sock` (doors), `p/<pid>/<plug>.sock` (one process's plugs) and `<pid>-<hex>/p<n>/` (the
//! process door's sockets). The plug folder is named after the process, not the session: the session name is up to
//! 43 bytes and the plug id up to 48, which together overflowed `sun_path` on both platforms.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The override's variable.
pub const RUNTIME_DIR_VAR: &str = "MARV_RUNTIME_DIR";

/// `sizeof(sockaddr_un.sun_path)`: a socket's path must be shorter (it ends in a NUL).
#[cfg(target_os = "macos")]
pub const SUN_PATH: usize = 104;
#[cfg(not(target_os = "macos"))]
pub const SUN_PATH: usize = 108;

/// What the longest socket adds to the runtime folder: `/p/` + a pid (7 digits) + `/` + a plug id (48) + `.sock`.
const LONGEST_SOCKET_TAIL: usize = 3 + 7 + 1 + 48 + 5;

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

/// Where the rule puts the runtime folder, unchecked: what the sandbox hides and the writable check refuses (more
/// hidden is harmless). `None` only on Linux without an absolute `XDG_RUNTIME_DIR`.
pub fn runtime_path(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    if let Some(dir) = env.get(RUNTIME_DIR_VAR).filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir)).filter(|d| d.is_absolute());
    }
    default_path(env)
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
/// processes' sockets are off. The macOS default and the override are made here (0700) and must then be this user's
/// own folder; the Linux default is checked as before (hidden in every sandbox) and made where it's used.
pub fn runtime_dir(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    let home = env.get("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
    if let Some(dir) = env.get(RUNTIME_DIR_VAR).filter(|d| !d.is_empty()) {
        let dir = PathBuf::from(dir);
        let ok = dir.is_absolute()
            && dir.as_os_str().len() <= MAX_RUNTIME_DIR
            && crate::hidden_in_sandbox(&dir, &home)
            && private_folder(&dir).is_ok();
        return ok.then_some(dir);
    }
    if cfg!(target_os = "macos") {
        let dir = default_path(env)?;
        return private_folder(&dir).is_ok().then_some(dir);
    }
    let base = env.get("XDG_RUNTIME_DIR").filter(|d| crate::hidden_in_sandbox(Path::new(d), &home))?;
    Some(Path::new(base).join("marv"))
}

/// Makes `dir` (0700) and checks it's a folder, not a symlink, of this user's; then 0700 again (an existing one).
fn private_folder(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    match std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.is_dir() || meta.uid() != nix::unistd::geteuid().as_raw() {
        return Err(std::io::Error::other(format!("{} isn't a folder of this user's", dir.display())));
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
        let plug = format!("{dir}/p/4194304/{}.sock", "x".repeat(48));
        assert_eq!(plug.len(), SUN_PATH - 1);
        assert!(check_socket_path(Path::new(&plug)).is_ok());
        let over = format!("{dir}x/p/4194304/{}.sock", "x".repeat(48));
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
        assert_eq!(runtime_path(&env(&[("XDG_RUNTIME_DIR", "/var/tmp/x")])), Some("/var/tmp/x/marv".into()));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_uses_a_private_tmp_folder_named_for_the_user() {
        let uid = nix::unistd::geteuid().as_raw();
        let want = PathBuf::from(format!("/tmp/marv-{uid}"));
        assert_eq!(runtime_path(&env(&[("XDG_RUNTIME_DIR", "/run/user/1")])), Some(want.clone()));
        assert_eq!(runtime_dir(&env(&[])), Some(want));
    }

    #[test]
    fn the_override_is_made_private_and_checked_like_the_default() {
        use std::os::unix::fs::PermissionsExt;
        let dir = short_temp("ov");
        let value = dir.join("marv");
        let got = runtime_dir(&env(&[(RUNTIME_DIR_VAR, value.to_str().unwrap()), ("XDG_RUNTIME_DIR", "/run/user/1")]));
        assert_eq!(got, Some(value.clone()));
        assert_eq!(std::fs::metadata(&value).unwrap().permissions().mode() & 0o777, 0o700);
        // Relative, too long for the longest socket, or a symlink: refused, never another folder in its place.
        assert_eq!(runtime_dir(&env(&[(RUNTIME_DIR_VAR, "rel/marv")])), None);
        let long = format!("/tmp/{}", "l".repeat(MAX_RUNTIME_DIR));
        assert_eq!(runtime_dir(&env(&[(RUNTIME_DIR_VAR, &long)])), None);
        assert!(!Path::new(&long).exists());
        let link = dir.join("link");
        std::os::unix::fs::symlink(&value, &link).unwrap();
        assert_eq!(runtime_dir(&env(&[(RUNTIME_DIR_VAR, link.to_str().unwrap())])), None);
        // One a sandboxed command could see (not under /tmp, /run or a hidden home entry): refused, and not made.
        assert_eq!(runtime_dir(&env(&[(RUNTIME_DIR_VAR, "/var/marv-mx-nope")])), None);
        assert!(!Path::new("/var/marv-mx-nope").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
