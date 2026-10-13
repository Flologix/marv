//! Path rules: confining every path a tool is given to its root, after `realpath`.
//!
//! Ports the path half of Marv's `src/tools/files.ts` (`resolveInProject`, `insideGitDir`, `touchesGit`,
//! `refuseGit`, `projectPath`) and fixes the skill-file read (`src/tools/skill.ts`), which checked only the
//! lexical path and so followed a symlink out of the skill's folder (a real vulnerability in Marv 0.3.0).
//!
//! - A leading `~/` is the home folder, as in a shell: taken literally, `~/.config/x` became `<project>/~/.config/x`,
//!   and a model told to create it would make a stray `~` folder. `~src` is not `~user`: it stays literal.
//! - A symlink inside the project can still point outside it. For a path that doesn't exist yet (a new file), the
//!   nearest folder that does is checked: a new file in a symlinked folder would land wherever that folder points.
//! - `.git` is judged through symlinks too: git runs its hooks and config outside any sandbox, so yolo never changes
//!   it without asking, and the check is repeated at apply time (`refuse_git`) since a parallel command could point a
//!   symlink into it meanwhile.
//! - Known limit (TOCTOU): a path checked here can be swapped for a symlink before it's opened. Planned: open with
//!   `openat2(RESOLVE_BENEATH)` where the kernel has it.

use std::path::{Component, Path, PathBuf};

use crate::SandboxError;

/// Lexical normalisation, like Node's `path.resolve` on an absolute path: `.` dropped, `..` pops (never above `/`).
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(name) => out.push(name),
        }
    }
    out
}

/// Whether `path` is `root` or inside it, lexically (both absolute and normalised).
pub fn is_inside(root: &Path, path: &Path) -> bool {
    path.starts_with(root)
}

/// The nearest path at or above `path` that exists (following symlinks, like `existsSync`).
pub fn nearest_existing(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    while !existing.exists() {
        match existing.parent() {
            Some(parent) => existing = parent.to_path_buf(),
            None => break,
        }
    }
    existing
}

/// Where a path really leads, for one that may not exist yet: the real path of its nearest existing ancestor, plus
/// the rest as written.
pub fn real_with_missing(path: &Path) -> std::io::Result<PathBuf> {
    let existing = nearest_existing(path);
    let rest = path.strip_prefix(&existing).unwrap_or(Path::new(""));
    Ok(std::fs::canonicalize(&existing)?.join(rest))
}

/// `~` and `~/x` expanded to the home folder; anything else as written.
pub fn expand_home(path: &str, home: &Path) -> PathBuf {
    if path == "~" {
        home.to_path_buf()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(path)
    }
}

/// Resolves a path the model gave (relative to the project, absolute, or starting with `~/`) and refuses anything
/// outside the project, including via symlinks. Returns the absolute (lexical) path.
pub fn resolve_in_project(root: &Path, path: &str, home: &Path) -> Result<PathBuf, SandboxError> {
    let expanded = expand_home(path, home);
    let absolute = if expanded.is_absolute() { normalize(&expanded) } else { normalize(&root.join(expanded)) };
    let outside = || SandboxError::OutsideProject { path: path.to_string(), root: root.display().to_string() };
    if !is_inside(root, &absolute) {
        return Err(outside());
    }
    let real_root = std::fs::canonicalize(root)?;
    let real = std::fs::canonicalize(nearest_existing(&absolute))?;
    if !is_inside(&real_root, &real) {
        return Err(outside());
    }
    Ok(absolute)
}

/// A file inside `dir` (a skill's folder), resolved through symlinks: refused when it is `dir` itself, climbs out
/// lexically, or *leads* out through a symlink. Returns the real path, which is what should be opened.
///
/// marv-rs fix (docs/SECURITY.md, "every path is checked after realpath"): Marv's `skill` tool compared only
/// `resolve(dir, file)` with the folder, so `ln -s ~/.ssh/id_ed25519 .marv/skills/x/key` let a model read the key,
/// outside the sandbox.
pub fn resolve_within(dir: &Path, file: &str) -> Result<PathBuf, SandboxError> {
    let outside = || SandboxError::OutsideDir { path: file.to_string(), within: dir.display().to_string() };
    let dir = normalize(dir);
    let lexical = normalize(&dir.join(file));
    if !is_inside(&dir, &lexical) || lexical == dir {
        return Err(outside());
    }
    let real_dir = std::fs::canonicalize(&dir)?;
    let real = match std::fs::canonicalize(&lexical) {
        Ok(real) => real,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Err(SandboxError::Missing(file.to_string())),
        Err(err) => return Err(err.into()),
    };
    if !is_inside(&real_dir, &real) || real == real_dir {
        return Err(outside());
    }
    Ok(real)
}

/// Whether an absolute project path is (or is inside) the project's `.git`, also through a symlink. A path that
/// doesn't exist yet is judged by the nearest folder that does.
pub fn inside_git_dir(root: &Path, absolute: &Path) -> std::io::Result<bool> {
    let real = real_with_missing(absolute)?;
    let real_root = std::fs::canonicalize(root)?;
    Ok(real
        .strip_prefix(&real_root)
        .ok()
        .and_then(|rel| rel.components().next())
        .is_some_and(|first| first.as_os_str() == ".git"))
}

/// For `auto_safe`: whether a path the model gave leads into `.git` (a path outside the project counts too: it
/// asks, and fails).
pub fn touches_git(root: &Path, path: &str, home: &Path) -> bool {
    match resolve_in_project(root, path, home) {
        Ok(absolute) => inside_git_dir(root, &absolute).unwrap_or(true),
        Err(_) => true,
    }
}

/// Checked again when a change that runs without asking is applied: a command running meanwhile (a parallel
/// subagent) could have pointed a symlink into `.git`.
pub fn refuse_git(root: &Path, absolute: &Path) -> Result<(), SandboxError> {
    if inside_git_dir(root, absolute).unwrap_or(true) {
        return Err(SandboxError::IntoGit(project_path(root, absolute)));
    }
    Ok(())
}

/// `src/app.ts` for display and for the model; `.` for the root itself.
pub fn project_path(root: &Path, absolute: &Path) -> String {
    match absolute.strip_prefix(root) {
        Ok(rel) if rel.as_os_str().is_empty() => ".".into(),
        Ok(rel) => rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/"),
        Err(_) => absolute.display().to_string(),
    }
}

#[cfg(test)]
pub(crate) mod test_dir {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A fresh temp folder, removed on drop (no tempfile dependency for a skeleton).
    pub struct TempDir(pub PathBuf);
    impl TempDir {
        pub fn new(tag: &str) -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("marv-sandbox-{tag}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(std::fs::canonicalize(dir).unwrap())
        }
        pub fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_dir::TempDir;
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    #[test]
    fn normalize_resolves_dots_lexically() {
        assert_eq!(normalize(Path::new("/a/b/../c/./d")), PathBuf::from("/a/c/d"));
        assert_eq!(normalize(Path::new("/../x")), PathBuf::from("/x"));
    }

    #[test]
    fn resolves_relative_and_absolute_paths_inside_the_project() {
        let root = TempDir::new("resolve");
        let r = root.path();
        assert_eq!(resolve_in_project(r, "src/app.ts", Path::new("/nowhere")).unwrap(), r.join("src/app.ts"));
        assert_eq!(resolve_in_project(r, &r.join("a.ts").display().to_string(), Path::new("/nowhere")).unwrap(), r.join("a.ts"));
        assert_eq!(resolve_in_project(r, ".", Path::new("/nowhere")).unwrap(), r);
    }

    #[test]
    fn refuses_paths_outside_the_project() {
        let root = TempDir::new("outside");
        let err = resolve_in_project(root.path(), "../secret.txt", Path::new("/nowhere")).unwrap_err();
        assert!(err.to_string().contains("is outside the project"));
        assert!(resolve_in_project(root.path(), "/etc/hosts", Path::new("/nowhere")).is_err());
    }

    #[test]
    fn wont_follow_a_symlink_out_of_the_project() {
        let root = TempDir::new("link-in");
        let outside = TempDir::new("link-out");
        fs::write(outside.path().join("secret.txt"), "top secret\n").unwrap();
        symlink(outside.path().join("secret.txt"), root.path().join("link.txt")).unwrap();
        symlink(outside.path(), root.path().join("dir")).unwrap();
        assert!(resolve_in_project(root.path(), "link.txt", Path::new("/nowhere")).is_err());
        // A new file in a symlinked folder would land outside: judged by the nearest existing folder.
        assert!(resolve_in_project(root.path(), "dir/new.txt", Path::new("/nowhere")).is_err());
    }

    #[test]
    fn tilde_is_the_home_folder_and_tilde_name_is_literal() {
        let root = TempDir::new("tilde");
        let r = root.path();
        assert_eq!(resolve_in_project(r, "~/src/app.ts", r).unwrap(), r.join("src/app.ts"));
        assert_eq!(resolve_in_project(r, "~", r).unwrap(), r);
        assert_eq!(resolve_in_project(r, "~src/app.ts", r).unwrap(), r.join("~src/app.ts"));
    }

    #[test]
    fn touches_git_directly_and_through_a_symlink() {
        let root = TempDir::new("git");
        let r = root.path();
        fs::create_dir_all(r.join(".git/hooks")).unwrap();
        symlink(r.join(".git/hooks"), r.join("hooks")).unwrap();
        let home = Path::new("/nowhere");
        assert!(touches_git(r, ".git/config", home));
        assert!(touches_git(r, "hooks/pre-commit", home));
        assert!(touches_git(r, "../elsewhere", home)); // outside counts: it asks, and fails
        assert!(!touches_git(r, "src/a.ts", home));
        assert!(refuse_git(r, &r.join("hooks/pre-commit")).is_err());
        assert!(refuse_git(r, &r.join("src/a.ts")).is_ok());
    }

    #[test]
    fn project_path_is_slash_separated_and_dot_for_the_root() {
        assert_eq!(project_path(Path::new("/p"), Path::new("/p/src/a.ts")), "src/a.ts");
        assert_eq!(project_path(Path::new("/p"), Path::new("/p")), ".");
    }

    #[test]
    fn a_skill_file_is_confined_to_its_folder_after_realpath() {
        let skill = TempDir::new("skill");
        let outside = TempDir::new("skill-out");
        fs::write(skill.path().join("reference.md"), "ref").unwrap();
        fs::write(outside.path().join("id_ed25519"), "PRIVATE KEY").unwrap();
        symlink(outside.path().join("id_ed25519"), skill.path().join("key")).unwrap();
        fs::create_dir(skill.path().join("scripts")).unwrap();
        symlink(outside.path(), skill.path().join("scripts/out")).unwrap();

        assert_eq!(resolve_within(skill.path(), "reference.md").unwrap(), skill.path().join("reference.md"));
        // The vulnerability: lexically inside, really outside.
        assert!(matches!(resolve_within(skill.path(), "key"), Err(SandboxError::OutsideDir { .. })));
        assert!(matches!(resolve_within(skill.path(), "scripts/out/id_ed25519"), Err(SandboxError::OutsideDir { .. })));
        // Lexical escapes and the folder itself.
        assert!(matches!(resolve_within(skill.path(), "../../../config.json"), Err(SandboxError::OutsideDir { .. })));
        assert!(matches!(resolve_within(skill.path(), "/etc/passwd"), Err(SandboxError::OutsideDir { .. })));
        assert!(matches!(resolve_within(skill.path(), "."), Err(SandboxError::OutsideDir { .. })));
        assert!(matches!(resolve_within(skill.path(), "missing.md"), Err(SandboxError::Missing(_))));
    }
}
