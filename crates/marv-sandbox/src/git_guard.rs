//! How a command that runs without asking is kept away from the project's `.git`: read-only if it's there, an empty
//! read-only placeholder if it isn't (so it can't create a repository whose config `git status` would run).
//!
//! Ports `gitGuard`, `worktreeGit` and `worktreeGitDirs` from Marv's `src/tools/bash.ts`.
//!
//! - bwrap can't mount on a symlink, so for a `.git` that is one, what it points to is mounted, when that's inside
//!   the project; outside, the sandbox already shows it read-only (or hides it, in the home folder).
//! - In a git worktree `.git` is a file (`gitdir: <repo>/.git/worktrees/<name>`) and the repository's data lives
//!   outside the project, usually in the hidden home. The worktree's own folder and the common `.git` are shown
//!   read-only, and the `.git` file too, so a command can't repoint it. The project's files say where those are, so
//!   they're only trusted when they check out the way git checks them: the admin folder sits in
//!   `<common>/worktrees/`, and its `gitdir` file points back at this `.git`. Otherwise a planted
//!   `gitdir: /home/me/.ssh` would show that folder to every command that runs without asking.

use std::path::{Path, PathBuf};

use crate::paths::{is_inside, normalize};

/// Extra mounts for a confined command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitGuard {
    pub read_only: Vec<PathBuf>,
    pub placeholders: Vec<PathBuf>,
}

/// The guard for a project root.
pub fn git_guard(root: &Path) -> GitGuard {
    let git_dir = root.join(".git");
    let Ok(meta) = std::fs::symlink_metadata(&git_dir) else {
        return GitGuard { placeholders: vec![git_dir], ..Default::default() };
    };
    if meta.file_type().is_symlink() {
        let real_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let target = std::fs::read_link(&git_dir).map(|t| normalize(&root.join(t))).unwrap_or_else(|_| git_dir.clone());
        return match std::fs::canonicalize(&git_dir) {
            Ok(real) if is_inside(&real_root, &real) => GitGuard { read_only: vec![real], ..Default::default() },
            Ok(_) => GitGuard::default(),
            // It leads nowhere yet: inside the project, keep that spot empty too.
            Err(_) if is_inside(&real_root, &target) || is_inside(root, &target) => GitGuard { placeholders: vec![target], ..Default::default() },
            Err(_) => GitGuard::default(),
        };
    }
    if meta.is_file() {
        let mut read_only = vec![git_dir.clone()];
        read_only.extend(worktree_git_dirs(&git_dir));
        return GitGuard { read_only, ..Default::default() };
    }
    GitGuard { read_only: vec![git_dir], ..Default::default() }
}

/// For a project whose `.git` is a worktree's file: its trusted git folders; else none.
pub fn worktree_git(root: &Path) -> Vec<PathBuf> {
    let git_file = root.join(".git");
    match std::fs::symlink_metadata(&git_file) {
        Ok(meta) if meta.is_file() => worktree_git_dirs(&git_file),
        _ => vec![],
    }
}

/// A worktree's own git folder and the repository's common `.git`, when its `.git` file checks out; else none
/// (missing or unreadable records: show nothing more, and git fails, as before).
pub fn worktree_git_dirs(git_file: &Path) -> Vec<PathBuf> {
    let check = || -> Option<Vec<PathBuf>> {
        let text = std::fs::read_to_string(git_file).ok()?;
        let target = text.lines().find_map(|l| l.strip_prefix("gitdir:")).map(str::trim)?;
        let parent = git_file.parent()?;
        let admin = std::fs::canonicalize(parent.join(target)).ok()?;
        // git's own check: the worktree's record points back at this .git file.
        let back = std::fs::read_to_string(admin.join("gitdir")).ok()?;
        if std::fs::canonicalize(admin.join(back.trim())).ok()? != std::fs::canonicalize(git_file).ok()? {
            return None;
        }
        let common_rel = std::fs::read_to_string(admin.join("commondir")).ok()?;
        let common = std::fs::canonicalize(admin.join(common_rel.trim())).ok()?;
        // And it's one of that repository's worktrees.
        if admin.parent()? != common.join("worktrees") {
            return None;
        }
        Some(vec![admin, common])
    };
    check().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::test_dir::TempDir;
    use std::fs;

    #[test]
    fn no_git_gets_a_placeholder_and_a_git_folder_is_read_only() {
        let root = TempDir::new("guard");
        assert_eq!(git_guard(root.path()).placeholders, vec![root.path().join(".git")]);
        fs::create_dir(root.path().join(".git")).unwrap();
        assert_eq!(git_guard(root.path()).read_only, vec![root.path().join(".git")]);
    }

    #[test]
    fn a_worktree_record_is_trusted_only_when_it_points_back() {
        let repo = TempDir::new("guard-repo");
        let tree = TempDir::new("guard-tree");
        let admin = repo.path().join(".git/worktrees/w1");
        fs::create_dir_all(&admin).unwrap();
        fs::write(tree.path().join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
        fs::write(admin.join("commondir"), "../..\n").unwrap();
        fs::write(admin.join("gitdir"), format!("{}\n", tree.path().join(".git").display())).unwrap();
        assert_eq!(worktree_git(tree.path()), vec![admin.clone(), repo.path().join(".git")]);
        // A planted gitdir that doesn't point back: nothing.
        fs::write(admin.join("gitdir"), "/somewhere/else/.git\n").unwrap();
        assert!(worktree_git(tree.path()).is_empty());
        assert_eq!(git_guard(tree.path()).read_only, vec![tree.path().join(".git")]);
    }
}
