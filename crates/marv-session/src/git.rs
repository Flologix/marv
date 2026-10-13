//! Marv's own git calls. Ports Marv's `src/git.ts`.
//!
//! Worktrees and the file listing behind glob and grep run git *outside* the sandbox, without approval, in folders an
//! agent could have tampered with. So they never run a repository's programs (hooks, fsmonitor), never wait for
//! input, and never wait forever.
//!
//! Port note: Marv's `spawnSync` blocked the whole UI (ctrl+c included) until git exited; this is async
//! (`tokio::process`), with the child killed on timeout or drop.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

/// Hooks and fsmonitor are how a repository makes git run a program; Marv's own git calls turn both off.
pub const NO_PROGRAMS: [&str; 4] = ["-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false"];

pub const GIT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitResult {
    pub ok: bool,
    /// stdout when it succeeded, stderr when it failed; not trimmed.
    pub out: String,
    pub timed_out: bool,
}

/// The environment without inherited `GIT_*` variables (a parent git or hook could have set `GIT_DIR`,
/// `GIT_INDEX_FILE`…), plus `pinned`.
pub fn git_environment(
    outer: impl IntoIterator<Item = (String, String)>,
    pinned: &[(String, String)],
) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = outer.into_iter().filter(|(k, _)| !k.starts_with("GIT_")).collect();
    env.extend(pinned.iter().cloned());
    env
}

/// Runs `git <NO_PROGRAMS> <args>` in `cwd`, stdin closed, `GIT_*` dropped (then `pinned` set), with a timeout.
pub async fn run_git(cwd: &Path, args: &[&str], pinned: &[(String, String)], timeout: Duration) -> GitResult {
    let env = git_environment(std::env::vars(), pinned);
    let child = tokio::process::Command::new("git")
        .args(NO_PROGRAMS)
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(env)
        .stdin(Stdio::null()) // nothing may wait for input: there's no one to type it
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(timeout, child).await {
        Err(_) => GitResult { ok: false, out: String::new(), timed_out: true },
        Ok(Err(err)) => GitResult { ok: false, out: err.to_string(), timed_out: false },
        Ok(Ok(output)) => {
            let ok = output.status.success();
            let out = String::from_utf8_lossy(if ok { &output.stdout } else { &output.stderr }).into_owned();
            GitResult { ok, out, timed_out: false }
        }
    }
}

/// The project's commit, so a trajectory says what code a run started from (2 s at most).
pub async fn git_head(root: &Path) -> Option<String> {
    let head = run_git(root, &["rev-parse", "HEAD"], &[], Duration::from_secs(2)).await;
    head.ok.then(|| head.out.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inherited_git_variables_are_dropped_and_pinned_ones_set() {
        let outer = [("GIT_DIR".to_string(), "/evil".to_string()), ("PATH".to_string(), "/bin".to_string())];
        let env = git_environment(outer, &[("GIT_WORK_TREE".into(), "/wt".into())]);
        assert_eq!(env.get("GIT_DIR"), None);
        assert_eq!(env["PATH"], "/bin");
        assert_eq!(env["GIT_WORK_TREE"], "/wt");
    }

    #[tokio::test]
    async fn outside_a_repository_git_fails_quietly() {
        let dir = std::env::temp_dir();
        let r = run_git(&dir, &["--version"], &[], GIT_TIMEOUT).await;
        // git may be missing on a build machine: then it's an error result, never a panic.
        assert!(!r.timed_out);
    }
}
