//! Git worktrees for subagents: a second checkout on its own branch, so agents working in parallel don't edit the
//! same files. Ports Marv's `src/worktree.ts`.
//!
//! ```text
//! git worktree add -b marv/<task>-<id> ~/.marv/worktrees/<project>/<task>-<id> HEAD
//! ```
//!
//! Inside the sandbox a subagent can change only its worktree; the repository's `.git` is read-only, so it can't plant
//! hooks or config that git would later run *outside* the sandbox. When it ends (however it ends) **Marv commits for
//! it** ([`finish_worktree`]): one commit `marv: <description>` (`(interrupted)` unless it ended with an answer),
//! through a temporary git dir Marv writes itself (HEAD → the branch, `commondir` → the real `.git`, a copy of the
//! worktree's index, without which force-added or skip-worktree files would be dropped), so nothing the worktree's
//! own `.git` file or record says is trusted. It refuses to commit if the worktree holds another `.git` outside
//! ignored folders (git would use that repository's config). Then it deletes only that worktree's folder and admin
//! dir (never `git worktree prune`, which would drop the user's other worktrees) and keeps the branch (deleted if it
//! has no commits). Commits are unsigned: a pinentry prompt can't show in the TUI and would block every subagent.
//!
//! Known limits: split-index and reftable repositories fail safe (git errors, the folder is kept).

use std::path::{Path, PathBuf};

pub const NOT_A_REPO: &str =
    "isolation: \"worktree\" needs a git repository with at least one commit. Start the agent without isolation instead.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub dir: PathBuf,
    pub branch: String,
    /// Short hash of the commit it started from.
    pub base: String,
    /// The main checkout (the project root).
    pub repo: PathBuf,
    /// The repository's shared `.git` (the sandbox shows it read-only).
    pub git_dir: PathBuf,
    /// This worktree's own folder inside it (`.git/worktrees/<id>`): its HEAD and index.
    pub admin_dir: PathBuf,
    /// Where the parent's project sits in the repository ("pkg/", or "" at the top).
    pub prefix: String,
    /// Where the subagent works: `dir` + `prefix` (a session in `repo/pkg` gives its subagent `pkg`).
    pub work_dir: PathBuf,
}

/// Where git must look for a worktree a subagent has used: never its `.git` file, which was within the subagent's
/// reach and could point at a fake repository with its own config.
pub fn worktree_env(wt: &Worktree) -> Vec<(String, String)> {
    vec![
        ("GIT_DIR".into(), wt.admin_dir.to_string_lossy().into_owned()),
        ("GIT_COMMON_DIR".into(), wt.git_dir.to_string_lossy().into_owned()),
        ("GIT_WORK_TREE".into(), wt.dir.to_string_lossy().into_owned()),
    ]
}

/// "Task 2: Parser errors" → "marv/task-2-parser-errors-ab12".
pub fn branch_name(description: &str, id: &str) -> String {
    let mut slug = String::new();
    for c in description.to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            slug.push(c);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    let slug: String = slug.chars().take(40).collect();
    let slug = slug.trim_end_matches('-');
    format!("marv/{}-{id}", if slug.is_empty() { "task" } else { slug })
}

/// What a worktree would start from, how many uncommitted changes the whole repository has, and where `root` sits
/// in it. `None` outside a repository with a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoState {
    pub base: String,
    pub dirty: usize,
    pub prefix: String,
}

pub async fn inspect_repo(_root: &Path) -> Option<RepoState> {
    todo!("port: inspectRepo (rev-parse --short HEAD, --show-prefix, status --porcelain via git::run_git)")
}

/// Creates the worktree and branch (`ToolError(NOT_A_REPO)` outside a repository).
pub async fn create_worktree(_root: &Path, _base_dir: &Path, _description: &str) -> Result<Worktree, marv_tools::ToolError> {
    todo!("port: createWorktree (git worktree add -q -b <branch> <dir> HEAD; git-common-dir, git-dir; mkdir work_dir)")
}

/// Commits the subagent's changes, removes the worktree, keeps the branch. Returns the line the parent reads.
pub async fn finish_worktree(_wt: &Worktree, _description: &str, _interrupted: bool) -> String {
    todo!(
        "port: finishWorktree (commitGitDir temp git dir, nestedRepo walk skipping ignored folders, unsigned commit, rev-list --count, remove dir + admin dir)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_names_are_slugs() {
        assert_eq!(branch_name("Task 2: Parser errors", "ab12"), "marv/task-2-parser-errors-ab12");
        assert_eq!(branch_name("!!!", "ab12"), "marv/task-ab12");
        let long = branch_name(&"word ".repeat(20), "x");
        assert!(!long.contains("--") && long.len() <= "marv/".len() + 40 + 2);
    }
}
