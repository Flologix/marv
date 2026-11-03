//! How a command that runs without asking is kept away from the project's `.git`: read-only if it's there, an empty
//! read-only placeholder if it isn't (so it can't create a repository whose config `git status` would run).
//!
//! Ports `gitGuard`, `worktreeGit` and `worktreeGitDirs` from Marv's `src/tools/bash.ts`.
//!
//! marv-rs: [`git_write_guard`], the mounts for a `git_write` command that runs without asking (bubblewrap): `.git`
//! writable, with what git runs programs from read-only on top.
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
            Err(_) if is_inside(&real_root, &target) || is_inside(root, &target) => {
                GitGuard { placeholders: vec![target], ..Default::default() }
            }
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

/// What stays read-only in a `.git` that a command changes without asking (`git_write` with yolo or
/// `approveSandboxed`; bubblewrap only): everything git runs a program from, or follows to find one. `config` (hooksPath,
/// fsmonitor, aliases, filter and diff drivers, sshCommand), `hooks/`, `info/` (attributes pick drivers), `modules/` and
/// `worktrees/` (other repositories' config and hooks), `config.worktree` (read with `extensions.worktreeConfig`) and
/// `commondir` (where git looks for all of those). The rest (objects, refs, the index, HEAD, logs, a merge's or rebase's
/// state) is what commits, branches and merges write.
pub const GIT_PROGRAM_DIRS: [&str; 4] = ["hooks", "info", "modules", "worktrees"];
/// The files among them, with what stands in for one that isn't there (see [`git_write_guard`]). `config` must exist.
pub const GIT_PROGRAM_FILES: [(&str, Option<&str>); 3] =
    [("config", None), ("config.worktree", Some("")), ("commondir", Some(".\n"))];

/// The mounts for a command that may change the repository without asking, while it runs: `.git` writable and itself
/// a mount point (so it can't be renamed away and replaced by one with a planted config), and what
/// [`GIT_PROGRAM_DIRS`] and [`GIT_PROGRAM_FILES`] name read-only on top. A missing folder gets an empty read-only
/// placeholder (removed afterwards, like any placeholder). A missing file is created first with content that changes
/// nothing (an empty `config.worktree`; `commondir` = `.`, the `.git` itself: git dies on an empty one), mounted
/// read-only, and removed when this is dropped: created afterwards instead, git outside the sandbox (an editor's
/// `git status` every few seconds) would follow it while the command runs. Holds an exclusive lock on `.git` until
/// dropped, so two such commands (subagents, another Marv) never share the files: the first one's cleanup would take
/// the second one's mount away.
#[derive(Debug)]
pub struct GitWrite {
    pub writable: PathBuf,
    pub read_only_on_top: Vec<PathBuf>,
    pub placeholders: Vec<PathBuf>,
    created: Vec<(PathBuf, &'static str)>,
    _lock: std::fs::File,
}

/// Why a [`GitWrite`] couldn't be made.
#[derive(Debug, PartialEq, Eq)]
pub enum GitWriteBlocked {
    /// Another such command holds `.git`: wait.
    Busy,
    /// Not a plain repository folder (no `.git`, a worktree's `.git` file, a symlink, a symlinked `config`…): the
    /// command is asked, as before.
    Unsupported,
}

/// Whether [`git_write_guard`] can work for this project (it may still be [`GitWriteBlocked::Busy`]).
pub fn git_write_supported(root: &Path) -> bool {
    plan_git_write(root).is_some()
}

/// What [`git_write_guard`] mounts and creates.
struct GitWritePlan {
    git: PathBuf,
    read_only: Vec<PathBuf>,
    placeholders: Vec<PathBuf>,
    create: Vec<(PathBuf, &'static str)>,
}

/// What to mount and what to create, without changing anything.
fn plan_git_write(root: &Path) -> Option<GitWritePlan> {
    let git = root.join(".git");
    if !std::fs::symlink_metadata(&git).ok()?.is_dir() {
        return None;
    }
    let (mut read_only, mut placeholders, mut create) = (Vec::new(), Vec::new(), Vec::new());
    for name in GIT_PROGRAM_DIRS {
        let path = git.join(name);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => read_only.push(path),
            Ok(_) => return None,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => placeholders.push(path),
            Err(_) => return None,
        }
    }
    for (name, stand_in) in GIT_PROGRAM_FILES {
        let path = git.join(name);
        match (std::fs::symlink_metadata(&path), stand_in) {
            (Ok(meta), _) if meta.is_file() => read_only.push(path),
            (Err(e), Some(content)) if e.kind() == std::io::ErrorKind::NotFound => create.push((path, content)),
            _ => return None,
        }
    }
    Some(GitWritePlan { git, read_only, placeholders, create })
}

/// The [`GitWrite`] for this project: locks `.git` and creates the stand-ins.
pub fn git_write_guard(root: &Path) -> Result<GitWrite, GitWriteBlocked> {
    let GitWritePlan { git, mut read_only, placeholders, create } = plan_git_write(root).ok_or(GitWriteBlocked::Unsupported)?;
    let lock = std::fs::File::open(&git).map_err(|_| GitWriteBlocked::Unsupported)?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Err(GitWriteBlocked::Busy),
        Err(_) => return Err(GitWriteBlocked::Unsupported),
    }
    let mut guard = GitWrite { writable: git, read_only_on_top: Vec::new(), placeholders, created: Vec::new(), _lock: lock };
    for (path, content) in create {
        // Locked, and nothing runs yet: it's still missing (create_new fails rather than write over anything).
        let made = std::fs::OpenOptions::new().write(true).create_new(true).open(&path).and_then(|mut f| {
            use std::io::Write;
            f.write_all(content.as_bytes())
        });
        if made.is_err() {
            return Err(GitWriteBlocked::Unsupported); // Dropping the guard removes what was made so far.
        }
        guard.created.push((path.clone(), content));
        read_only.push(path);
    }
    guard.read_only_on_top = read_only;
    Ok(guard)
}

impl Drop for GitWrite {
    /// Removes the stand-ins it made, still as made (they were read-only to the command), then the lock goes.
    fn drop(&mut self) {
        for (path, content) in &self.created {
            if std::fs::read_to_string(path).is_ok_and(|now| now == *content) {
                let _ = std::fs::remove_file(path);
            }
        }
    }
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
    fn a_git_write_guard_locks_makes_its_stand_ins_and_removes_them() {
        let root = TempDir::new("guard-write");
        let git = root.path().join(".git");
        fs::create_dir_all(git.join("hooks")).unwrap();
        fs::write(git.join("config"), "[core]\n").unwrap();
        let guard = git_write_guard(root.path()).unwrap();
        assert_eq!(guard.writable, git);
        assert_eq!(guard.placeholders, ["info", "modules", "worktrees"].map(|d| git.join(d)));
        assert_eq!(guard.read_only_on_top, ["hooks", "config", "config.worktree", "commondir"].map(|f| git.join(f)));
        assert_eq!(fs::read_to_string(git.join("commondir")).unwrap(), ".\n");
        assert_eq!(fs::read_to_string(git.join("config.worktree")).unwrap(), "");
        // A second one waits for the first.
        assert_eq!(git_write_guard(root.path()).unwrap_err(), GitWriteBlocked::Busy);
        drop(guard);
        assert!(!git.join("commondir").exists() && !git.join("config.worktree").exists());
        assert_eq!(fs::read_to_string(git.join("config")).unwrap(), "[core]\n");
        drop(git_write_guard(root.path()).unwrap());
    }

    #[test]
    fn a_git_write_guard_needs_a_plain_repository() {
        let root = TempDir::new("guard-write-plain");
        assert!(!git_write_supported(root.path()));
        fs::write(root.path().join(".git"), "gitdir: /elsewhere\n").unwrap();
        assert_eq!(git_write_guard(root.path()).unwrap_err(), GitWriteBlocked::Unsupported);
        fs::remove_file(root.path().join(".git")).unwrap();
        fs::create_dir(root.path().join(".git")).unwrap();
        assert!(!git_write_supported(root.path()), "no config");
        fs::write(root.path().join("elsewhere"), "").unwrap();
        std::os::unix::fs::symlink(root.path().join("elsewhere"), root.path().join(".git/config")).unwrap();
        assert!(!git_write_supported(root.path()), "a symlinked config can't be mounted on");
        fs::remove_file(root.path().join(".git/config")).unwrap();
        fs::write(root.path().join(".git/config"), "").unwrap();
        assert!(git_write_supported(root.path()));
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
