//! Shared file helpers for the tools: confining paths to the project, listing the project's files, and refusing
//! special files.
//!
//! Ports Marv's `src/tools/files.ts`. The path rules themselves (`resolveInProject`, `touchesGit`, `refuseGit`,
//! `projectPath`) moved to `marv_sandbox::paths`; these wrappers turn their errors into [`ToolError`]s.
//!
//! - Files are listed with `git ls-files -z --cached --others --exclude-standard` (so `.gitignore` applies) through
//!   `runGit` (a 20 s timeout, no hooks or fsmonitor, inherited `GIT_*` dropped, `ToolContext::git_env` pins a worktree
//!   subagent's git paths). A timeout is an error, never the unfiltered walk (it would return node_modules); `--cached`
//!   includes deleted-but-staged files, so each is checked to exist. Outside a repo: a walk that skips
//!   [`IGNORED_DIRS`] and special files.
//! - `read_file`, `edit_file`, `write_file` and `grep` refuse special files (FIFOs, devices): reading one would block
//!   forever, and Esc can't cancel it. A missing path is called missing, not special.
//! - Binary: a NUL byte in the first 8 KB (crude but standard).

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::types::{ToolContext, ToolError};

pub use marv_sandbox::paths::{project_path, refuse_git};

/// Skipped when the project isn't a git repo (inside one, `.gitignore` decides).
pub const IGNORED_DIRS: &[&str] = &[".git", "node_modules", "dist", "build", "out", "coverage", ".next", ".cache", "target", ".venv", "__pycache__"];

/// Long enough for a big repository, short enough that a blocked git (a FIFO named `.gitignore`) gives up.
pub const LIST_TIMEOUT: Duration = Duration::from_secs(20);

/// A path the model gave, resolved inside the project (see `marv_sandbox::resolve_in_project`).
pub fn resolve_in_project(ctx: &ToolContext, path: &str) -> Result<PathBuf, ToolError> {
    Ok(marv_sandbox::resolve_in_project(&ctx.root, path, &ctx.home)?)
}

/// For `auto_safe`: whether a path leads into `.git` (outside the project counts too).
pub fn touches_git(ctx: &ToolContext, path: &str) -> bool {
    marv_sandbox::touches_git(&ctx.root, path, &ctx.home)
}

/// Every file in the project, project-relative and sorted (`git ls-files`, or the walk outside a repo).
pub async fn list_project_files(_root: &Path, _git_env: &[(String, String)], _timeout: Duration) -> Result<Vec<String>, ToolError> {
    todo!("port: listProjectFiles via runGit (marv-session's git module or a local run_git) with the walk fallback")
}

/// Files under `dir` (project-relative, `.` for everything).
pub async fn files_under(root: &Path, dir: &str, git_env: &[(String, String)]) -> Result<Vec<String>, ToolError> {
    let all = list_project_files(root, git_env, LIST_TIMEOUT).await?;
    if dir == "." {
        return Ok(all);
    }
    let prefix = format!("{dir}/");
    Ok(all.into_iter().filter(|p| p.starts_with(&prefix) || p == dir).collect())
}

/// Whether a path is a regular file (following a symlink).
pub fn is_regular_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file())
}

pub fn require_regular_file(path: &Path, shown: &str) -> Result<(), ToolError> {
    if !path.exists() {
        return Err(ToolError(format!("\"{shown}\" doesn't exist.")));
    }
    if !is_regular_file(path) {
        return Err(ToolError(format!("\"{shown}\" is not a regular file (a pipe, socket or device), so it can't be read.")));
    }
    Ok(())
}

pub fn is_directory(path: &Path) -> bool {
    path.is_dir()
}

/// Crude but standard: a NUL byte in the first 8 KB means binary.
pub fn looks_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(8192)].contains(&0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_is_a_nul_in_the_first_8k() {
        assert!(looks_binary(b"\x89PNG\0\0"));
        assert!(!looks_binary(b"plain text"));
        let mut late = vec![b'a'; 9000];
        late[8500] = 0;
        assert!(!looks_binary(&late));
    }

    #[test]
    fn a_missing_path_is_missing_not_special() {
        let err = require_regular_file(Path::new("/nonexistent/x"), "x").unwrap_err();
        assert_eq!(err.0, "\"x\" doesn't exist.");
        assert!(require_regular_file(Path::new("/dev/null"), "null").unwrap_err().0.contains("not a regular file"));
    }
}
