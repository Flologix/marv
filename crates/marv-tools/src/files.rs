//! Shared file helpers for the tools: confining paths to the project, listing the project's files, and refusing
//! special files.
//!
//! Ports Marv's `src/tools/files.ts`. The path rules themselves (`resolveInProject`, `touchesGit`, `refuseGit`,
//! `projectPath`) moved to `marv_sandbox::paths`; these wrappers turn their errors into [`ToolError`]s.
//!
//! - Files are listed with `git ls-files -z --cached --others --exclude-standard` (so `.gitignore` applies) through
//!   `runGit` (a 20 s timeout, no hooks or fsmonitor, inherited `GIT_*` dropped, `ToolContext::git_env` pins a worktree
//!   subagent's git paths). A timeout is an error, never the unfiltered walk (it would return node_modules); `--cached`
//!   includes deleted-but-staged files, so each is checked to exist. Outside a repo: a walk (ripgrep's `ignore`
//!   walker, with its own filters off: [`IGNORED_DIRS`] decides, not a stray `.gitignore`) that skips special files.
//! - Port note: `runGit` lives in marv-session's git module in the port, which marv-tools can't depend on, so the
//!   listing has its own [`run_git`] with the same rules (`NO_PROGRAMS`, no inherited `GIT_*`, stdin closed).
//! - `read_file`, `edit_file`, `write_file` and `grep` refuse special files (FIFOs, devices): reading one would block
//!   forever, and Esc can't cancel it. A missing path is called missing, not special.
//! - Binary: a NUL byte in the first 8 KB (crude but standard).
//! - #61: `read_file` and `grep` also read the session's saved tool outputs ([`resolve_readable`]), and only them
//!   outside the project. bash can't (its sandbox has its own home), and the writing tools keep `resolve_in_project`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::types::{ToolContext, ToolError, ToolOutput};

pub use marv_sandbox::paths::{project_path, refuse_git};

/// Skipped when the project isn't a git repo (inside one, `.gitignore` decides).
pub const IGNORED_DIRS: &[&str] =
    &[".git", "node_modules", "dist", "build", "out", "coverage", ".next", ".cache", "target", ".venv", "__pycache__"];

/// Long enough for a big repository, short enough that a blocked git (a FIFO named `.gitignore`) gives up.
pub const LIST_TIMEOUT: Duration = Duration::from_secs(20);

/// A path the model gave, resolved inside the project (see `marv_sandbox::resolve_in_project`).
pub fn resolve_in_project(ctx: &ToolContext, path: &str) -> Result<PathBuf, ToolError> {
    Ok(marv_sandbox::resolve_in_project(&ctx.root, path, &ctx.home)?)
}

/// For the reading tools (`read_file`, `grep`): a path inside the project, or a file in this session's saved tool
/// outputs (#61, `ToolContext::tool_output`). Exactly that folder's files, checked after realpath like the project's
/// (`resolve_within`); nothing else outside the project, and the writing tools never get here.
pub fn resolve_readable(ctx: &ToolContext, path: &str) -> Result<PathBuf, ToolError> {
    if let Some(dir) = &ctx.tool_output {
        let dir = marv_sandbox::paths::normalize(dir);
        let absolute = marv_sandbox::paths::normalize(&marv_sandbox::paths::expand_home(path, &ctx.home));
        if absolute.is_absolute()
            && let Ok(file) = absolute.strip_prefix(&dir)
            && !file.as_os_str().is_empty()
        {
            return marv_sandbox::resolve_within(&dir, &file.to_string_lossy()).map_err(|e| match e {
                marv_sandbox::SandboxError::Missing(_) => ToolError(format!("\"{path}\" doesn't exist.")),
                e => e.into(),
            });
        }
    }
    resolve_in_project(ctx, path)
}

/// Whether `path` (as [`resolve_readable`] returned it) is one of the session's saved tool outputs.
pub fn is_tool_output(ctx: &ToolContext, path: &Path) -> bool {
    let real = |p: &Path| std::fs::canonicalize(p).ok();
    ctx.tool_output.as_deref().and_then(real).is_some_and(|dir| real(path).is_some_and(|p| p.starts_with(dir)))
}

/// A reading tool's result from `path`: when that's a saved tool output, it keeps the source the output came from
/// (`output::saved_provenance`), so an MCP server's text read back is fenced and taints as it did the first time.
pub fn with_saved_source(ctx: &ToolContext, path: &Path, mut out: ToolOutput) -> ToolOutput {
    if is_tool_output(ctx, path) {
        out.provenance = crate::output::saved_provenance(path);
    }
    out
}

/// For `auto_safe`: whether a path leads into `.git` (outside the project counts too).
pub fn touches_git(ctx: &ToolContext, path: &str) -> bool {
    marv_sandbox::touches_git(&ctx.root, path, &ctx.home)
}

/// Hooks and fsmonitor are how a repository makes git run a program; Marv's own git calls turn both off.
pub const NO_PROGRAMS: &[&str] = &["-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false"];

/// How one of Marv's own git calls ended (`GitResult`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitResult {
    pub ok: bool,
    /// stdout when it succeeded, stderr when it failed; not trimmed.
    pub out: String,
    pub timed_out: bool,
}

/// Runs git outside the sandbox, in a folder an agent could have tampered with: it never runs the repository's
/// programs (hooks, fsmonitor), never waits for input, never waits forever, and doesn't inherit `GIT_*` variables (a
/// parent git or hook could have set `GIT_DIR`, `GIT_INDEX_FILE`…); `pinned` is added on top.
pub async fn run_git(cwd: &Path, args: &[&str], pinned: &[(String, String)], timeout: Duration) -> GitResult {
    let mut command = tokio::process::Command::new("git");
    command.args(NO_PROGRAMS).args(args).current_dir(cwd);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
    command.envs(pinned.iter().map(|(k, v)| (k, v)));
    // Nothing may wait for input: there's no one to type it.
    command.stdin(std::process::Stdio::null()).kill_on_drop(true);
    match tokio::time::timeout(timeout, command.output()).await {
        Err(_) => GitResult { ok: false, out: String::new(), timed_out: true },
        Ok(Err(e)) => GitResult { ok: false, out: e.to_string(), timed_out: false },
        Ok(Ok(output)) => {
            let ok = output.status.success();
            let out = String::from_utf8_lossy(if ok { &output.stdout } else { &output.stderr }).into_owned();
            GitResult { ok, out, timed_out: false }
        }
    }
}

/// Every file in the project, project-relative and sorted (`git ls-files`, or the walk outside a repo).
pub async fn list_project_files(root: &Path, git_env: &[(String, String)], timeout: Duration) -> Result<Vec<String>, ToolError> {
    let git = run_git(root, &["ls-files", "-z", "--cached", "--others", "--exclude-standard"], git_env, timeout).await;
    // No fallback to the walk: it would return node_modules and other ignored folders unfiltered.
    if git.timed_out {
        return Err(ToolError::new("Listing the project's files timed out (a special file such as a FIFO may be blocking git)."));
    }
    let mut paths: Vec<String> = if git.ok {
        // --cached includes deleted-but-staged files.
        git.out.split('\0').filter(|p| !p.is_empty() && root.join(p).exists()).map(str::to_string).collect()
    } else {
        let root = root.to_path_buf();
        tokio::task::spawn_blocking(move || walk(&root)).await.unwrap_or_default()
    };
    paths.sort();
    Ok(paths)
}

/// The project's files outside a git repo: everything but [`IGNORED_DIRS`] and special files.
fn walk(root: &Path) -> Vec<String> {
    let walker = ignore::WalkBuilder::new(root)
        .standard_filters(false)
        .filter_entry(|entry| entry.depth() == 0 || !IGNORED_DIRS.iter().any(|d| entry.file_name() == *d))
        .build();
    walker
        .filter_map(Result::ok)
        .filter(|entry| entry.depth() > 0 && !entry.file_type().is_some_and(|t| t.is_dir()) && is_regular_file(entry.path()))
        .filter_map(|entry| entry.path().strip_prefix(root).ok().map(|rel| rel.to_string_lossy().into_owned()))
        .collect()
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
    use crate::test_support::{TempDir, git, mkfifo, project};

    async fn list(root: &Path, git_env: &[(String, String)]) -> Vec<String> {
        list_project_files(root, git_env, LIST_TIMEOUT).await.unwrap()
    }

    #[tokio::test]
    async fn outside_git_the_walk_skips_ignored_folders_and_special_files() {
        let root = project("list-walk");
        root.write("dist/out.js", "x");
        root.write(".env", "x");
        mkfifo(&root.path().join("src/pipe"));
        assert_eq!(
            list(root.path(), &[]).await,
            [".env", "logo.png", "package.json", "src/app.ts", "src/greet.ts", "src/ui/view.tsx"]
        );
    }

    #[tokio::test]
    async fn inside_git_gitignore_decides_and_deleted_but_staged_files_are_left_out() {
        let root = project("list-git");
        git(root.path(), &["init", "-q"]);
        root.write(".gitignore", "node_modules/\nsrc/ui/\n");
        git(root.path(), &["add", "src/app.ts"]);
        std::fs::remove_file(root.path().join("src/app.ts")).unwrap();
        assert_eq!(list(root.path(), &[]).await, [".gitignore", "logo.png", "package.json", "src/greet.ts"]);
    }

    #[tokio::test]
    async fn core_fsmonitor_in_the_projects_own_config_isnt_run() {
        let root = TempDir::new("list-fsmonitor");
        let outside = TempDir::new("list-fsmonitor-out");
        let pwned = outside.path().join("PWNED");
        git(root.path(), &["init", "-q"]);
        git(root.path(), &["config", "core.fsmonitor", &format!("touch {}", pwned.display())]);
        root.write("a.txt", "a\n");
        // Negative control: plain git, without Marv's protections, does run the program.
        let _ = std::process::Command::new("git").args(["ls-files", "--others"]).current_dir(root.path()).output();
        assert!(pwned.exists());
        std::fs::remove_file(&pwned).unwrap();
        // (git passes the hook its version and a token, so the control's `touch` also made two files here.)
        assert!(list(root.path(), &[]).await.contains(&"a.txt".to_string()));
        assert!(!pwned.exists());
    }

    #[tokio::test]
    async fn core_fsmonitor_in_a_per_worktree_config_isnt_run() {
        let root = TempDir::new("list-worktree-config");
        let outside = TempDir::new("list-worktree-config-out");
        let pwned = outside.path().join("PWNED");
        git(root.path(), &["init", "-q"]);
        git(root.path(), &["config", "extensions.worktreeConfig", "true"]);
        root.write(".git/config.worktree", format!("[core]\n\tfsmonitor = touch {}\n", pwned.display()));
        root.write("w.txt", "w\n");
        assert_eq!(list(root.path(), &[]).await, ["w.txt"]);
        assert!(!pwned.exists());
    }

    #[tokio::test]
    async fn with_git_env_a_redirected_git_file_is_ignored() {
        let root = TempDir::new("list-git-env");
        let outside = TempDir::new("list-git-env-out");
        let pwned = outside.path().join("PWNED");
        // Two usable repositories outside the work folder. Only the fake one hides hidden.txt.
        let real = outside.path().join("real.git");
        let fake = outside.path().join("fake.git");
        for dir in [&real, &fake] {
            git(outside.path(), &["init", "-q", "--bare", &dir.display().to_string()]);
            git(dir, &["config", "core.bare", "false"]);
        }
        git(&fake, &["config", "core.fsmonitor", &format!("touch {}", pwned.display())]);
        std::fs::write(fake.join("info/exclude"), "hidden.txt\n").unwrap();
        // The .git file in the work folder points to the fake one.
        root.write(".git", format!("gitdir: {}\n", fake.display()));
        root.write("shown.txt", "s\n");
        root.write("hidden.txt", "h\n");

        // Unpinned, git follows the .git file: the fake repository's view.
        let followed = list(root.path(), &[]).await;
        assert!(followed.contains(&"shown.txt".to_string()) && !followed.contains(&"hidden.txt".to_string()));

        // Pinned to the real repository: its view (hidden.txt isn't excluded there).
        let pin = |k: &str, v: &Path| (k.to_string(), v.display().to_string());
        let pinned =
            list(root.path(), &[pin("GIT_DIR", &real), pin("GIT_COMMON_DIR", &real), pin("GIT_WORK_TREE", root.path())]).await;
        assert!(pinned.contains(&"shown.txt".to_string()) && pinned.contains(&"hidden.txt".to_string()));
        assert!(!pwned.exists());
    }

    #[tokio::test]
    async fn a_fifo_named_gitignore_makes_the_listing_time_out_with_a_tool_error_quickly() {
        let root = TempDir::new("list-fifo");
        git(root.path(), &["init", "-q"]);
        mkfifo(&root.path().join(".gitignore"));
        let started = std::time::Instant::now();
        let err = list_project_files(root.path(), &[], Duration::from_millis(300)).await.unwrap_err();
        assert!(err.0.contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn files_under_a_folder() {
        let root = project("list-under");
        assert_eq!(files_under(root.path(), "src/ui", &[]).await.unwrap(), ["src/ui/view.tsx"]);
        assert_eq!(files_under(root.path(), "src", &[]).await.unwrap().len(), 3);
    }

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
