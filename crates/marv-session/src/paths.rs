//! Display paths, the home-folder guard and per-project keys. Ports Marv's `src/paths.ts`.
//!
//! - [`project_key`]: per-project files (sessions, project memory, trajectories, MCP trust, worktrees) live under
//!   `<readable path>-<10-char hash of the full path>`. The readable part alone collided (`/a-b/c` vs `/a/b-c`, any
//!   two non-ASCII names of equal length), which made projects share memory, sessions and trusted MCP servers.
//! - [`legacy_project_key`]: the old key, still searched (sessions check their recorded root; memory is moved).
//! - [`home_or_above`]: a project there would put every dotfile within reach of yolo's unasked edits (the SDK refuses
//!   yolo there). Compared as real paths, so a symlink to either is caught too.
//!
//! Port note: Marv's legacy key replaced each UTF-16 unit; here each `char` becomes `len_utf16()` dashes, so keys
//! are identical, astral characters included.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// The readable part keeps at most this many characters (the end, which names the project).
pub const MAX_SLUG: usize = 64;

/// The user's home folder (`os.homedir()`).
pub fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

/// `/home/me/projects/x` → `~/projects/x`, for display only.
pub fn shorten_home(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// Whether `folder` is the home folder or a folder above it (`/home`, `/`), compared as real paths.
pub fn home_or_above(folder: &Path, home: &Path) -> bool {
    // A path that doesn't exist (a home folder that was never made) is compared as given.
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let target = real(folder);
    let h = real(home);
    target == h || target == Path::new("/") || h.starts_with(&target)
}

/// The key before it had a hash. Not unique: never use it to decide anything.
pub fn legacy_project_key(root: &Path) -> String {
    root.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_string() } else { "-".repeat(c.len_utf16()) })
        .collect()
}

/// `/home/me/proj` → `-home-me-proj-1a2b3c4d5e`.
pub fn project_key(root: &Path) -> String {
    let digest = Sha256::digest(root.as_os_str().as_encoded_bytes());
    let hash: String = digest.iter().map(|b| format!("{b:02x}")).collect::<String>()[..10].to_string();
    let legacy = legacy_project_key(root);
    let slug = &legacy[legacy.len().saturating_sub(MAX_SLUG)..]; // ASCII only by now
    format!("{slug}-{hash}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn different_folders_never_share_a_project_key() {
        let pairs = [
            ("/home/me/a-b/c", "/home/me/a/b-c"),
            ("/home/me/my_app", "/home/me/my-app"),
            ("/home/me/проект", "/home/me/работа"),
            ("/home/me/项目一", "/home/me/客户端"),
        ];
        for (a, b) in pairs {
            assert_eq!(legacy_project_key(Path::new(a)), legacy_project_key(Path::new(b)));
            assert_ne!(project_key(Path::new(a)), project_key(Path::new(b)));
        }
    }

    #[test]
    fn a_key_is_stable_readable_and_safe_as_a_file_name() {
        let key = project_key(Path::new("/home/me/Projects/marv"));
        assert_eq!(key, project_key(Path::new("/home/me/Projects/marv")));
        let re = regex::Regex::new(r"^-home-me-Projects-marv-[0-9a-f]{10}$").unwrap();
        assert!(re.is_match(&key), "{key}");
        let long = project_key(Path::new(&format!("/home/me/{}project", "deep/".repeat(60))));
        assert!(long.len() <= 80);
        assert!(regex::Regex::new(r"project-[0-9a-f]{10}$").unwrap().is_match(&long));
    }

    #[test]
    fn home_is_shortened_for_display() {
        let home = Path::new("/home/me");
        assert_eq!(shorten_home(Path::new("/home/me/x/y"), home), "~/x/y");
        assert_eq!(shorten_home(home, home), "~");
        assert_eq!(shorten_home(Path::new("/home/meme"), home), "/home/meme");
    }

    #[test]
    fn home_and_above_are_refused() {
        let home = Path::new("/nonexistent-home/me");
        assert!(home_or_above(home, home));
        assert!(home_or_above(Path::new("/nonexistent-home"), home));
        assert!(home_or_above(Path::new("/"), home));
        assert!(!home_or_above(Path::new("/nonexistent-home/me/project"), home));
    }
}
