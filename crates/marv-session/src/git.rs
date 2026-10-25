//! Marv's own git calls. Ports Marv's `src/git.ts`.
//!
//! Worktrees and the file listing behind glob and grep run git *outside* the sandbox, without approval, in folders an
//! agent could have tampered with. So they never run a repository's programs (hooks, fsmonitor), never wait for
//! input, and never wait forever.
//!
//! Port note: Marv's `spawnSync` blocked the whole UI (ctrl+c included) until git exited; this is async
//! (`tokio::process`), with the child killed on timeout or drop. `run_git` is `run_git_async` (Marv #16's addition,
//! for checkpoints: stdout's bytes, Marv's own stdin, a stop) with `runGit`'s result.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

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
    let r = run_git_async(cwd, args, GitOptions { env: pinned, timeout, ..Default::default() }).await;
    GitResult { ok: r.ok, out: r.out, timed_out: r.timed_out }
}

/// What [`run_git_async`] takes beyond the arguments.
#[derive(Clone, Copy)]
pub struct GitOptions<'a> {
    /// Set after the inherited `GIT_*` are dropped (`GIT_DIR`, `GIT_INDEX_FILE`…).
    pub env: &'a [(String, String)],
    pub timeout: Duration,
    /// A stop kills git too.
    pub cancel: Option<&'a CancellationToken>,
    /// Marv's own stdin for git (`--stdin-paths`, `--batch-check`); without it stdin is closed.
    pub input: Option<&'a [u8]>,
}

impl Default for GitOptions<'_> {
    fn default() -> Self {
        Self { env: &[], timeout: GIT_TIMEOUT, cancel: None, input: None }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitAsyncResult {
    pub ok: bool,
    /// stdout when it succeeded, stderr when it failed; not trimmed.
    pub out: String,
    /// stdout as bytes (a blob from `cat-file`), whatever the exit code.
    pub bytes: Vec<u8>,
    pub timed_out: bool,
}

/// Kills a git's whole process group when it's dropped before git finished: a timeout, a stop, or Marv exiting with
/// the task still running (the runtime drops it). Killing git alone left what it started running: `git gc`'s
/// `git repack` went on writing into the checkpoint git dir after Marv had gone, and recreated a removed folder.
struct GitGroup {
    pgid: Option<nix::unistd::Pid>,
}

/// The groups of the gits running now, for [`kill_running`].
static RUNNING: std::sync::Mutex<Vec<nix::unistd::Pid>> = std::sync::Mutex::new(Vec::new());

fn running() -> std::sync::MutexGuard<'static, Vec<nix::unistd::Pid>> {
    RUNNING.lock().unwrap_or_else(|e| e.into_inner())
}

impl GitGroup {
    fn new(pgid: Option<nix::unistd::Pid>) -> Self {
        running().extend(pgid);
        Self { pgid }
    }

    /// git exited (and was reaped): its group is gone or no longer its own to kill.
    fn finished(&mut self) {
        if let Some(pgid) = self.pgid.take() {
            running().retain(|&p| p != pgid);
        }
    }
}

impl Drop for GitGroup {
    fn drop(&mut self) {
        if let Some(pgid) = self.pgid {
            let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL);
            running().retain(|&p| p != pgid);
        }
    }
}

/// SIGKILLs the process group of every git still running: for an exit that can't wait for the runtime to drop their
/// tasks (#196: the forced exit after a quit signal, when the way out is stuck).
pub fn kill_running() {
    for pgid in running().drain(..) {
        let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL);
    }
}

/// Marv's `runGitAsync` (checkpoints): `run_git`'s rules (no hooks or fsmonitor, no inherited `GIT_*`, no stdin but
/// Marv's own `input`, a timeout), and a stop kills it too. A git that's killed isn't waited for: a child it started
/// (an alias, a filter) may hold its pipes open. git runs in a process group of its own, killed whole when git is
/// ([`GitGroup`]), so nothing it started outlives it.
pub async fn run_git_async(cwd: &Path, args: &[&str], options: GitOptions<'_>) -> GitAsyncResult {
    let killed = GitAsyncResult { ok: false, out: String::new(), bytes: Vec::new(), timed_out: false };
    if options.cancel.is_some_and(|c| c.is_cancelled()) {
        return killed;
    }
    let env = git_environment(std::env::vars(), options.env);
    let spawned = tokio::process::Command::new("git")
        .args(NO_PROGRAMS)
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(env)
        // nothing may wait for input: there's no one to type it
        .stdin(if options.input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true) // a timeout or a stop drops the child: SIGKILL, and its pipes aren't waited for
        .process_group(0)
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(err) => return GitAsyncResult { out: err.to_string(), ..killed },
    };
    let mut group = GitGroup::new(child.id().map(|pid| nix::unistd::Pid::from_raw(pid as i32)));
    // Written beside the reads: a large input and a large output would otherwise wait for each other.
    if let (Some(input), Some(mut stdin)) = (options.input, child.stdin.take()) {
        let input = input.to_vec();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(&input).await;
        });
    }
    let never = CancellationToken::new();
    let cancel = options.cancel.unwrap_or(&never);
    tokio::select! {
        output = child.wait_with_output() => match output {
            Err(err) => GitAsyncResult { out: err.to_string(), ..killed },
            Ok(output) => {
                group.finished();
                let ok = output.status.success();
                let out = String::from_utf8_lossy(if ok { &output.stdout } else { &output.stderr }).into_owned();
                GitAsyncResult { ok, out, bytes: output.stdout, timed_out: false }
            }
        },
        _ = tokio::time::sleep(options.timeout) => GitAsyncResult { timed_out: true, ..killed },
        _ = cancel.cancelled() => killed,
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

    /// What git started dies with it: a `git` dropped mid-run (Marv exiting with a checkpoint's `git gc` still going)
    /// left `gc`'s `git repack` writing into a folder after it was removed.
    #[tokio::test]
    async fn a_git_stopped_midway_takes_what_it_started_with_it() {
        let dir = Temp::new("git-group");
        let marker = dir.0.join("written-after-git-stopped");
        let alias = format!("alias.later=!sleep 0.4; touch '{}'", marker.display());
        let args = ["-c", alias.as_str(), "later"];
        // Dropped from outside (the runtime going away), and stopped by its own timeout.
        let dropped = tokio::time::timeout(Duration::from_millis(100), run_git_async(&dir.0, &args, GitOptions::default()));
        assert!(dropped.await.is_err());
        let timed = run_git_async(&dir.0, &args, GitOptions { timeout: Duration::from_millis(100), ..Default::default() }).await;
        assert!(timed.timed_out);
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert!(!marker.exists(), "a child of a stopped git ran on");
        // Left to finish, it does.
        assert!(run_git_async(&dir.0, &args, GitOptions::default()).await.ok);
        assert!(marker.exists());
    }

    // Ports tests/git.test.ts (runGitAsync).
    use crate::worktree::tests::{Temp, init_repo, sh_git};

    fn repo() -> Temp {
        let dir = Temp::new("git");
        init_repo(&dir.0);
        dir
    }

    #[tokio::test]
    async fn run_git_async_has_run_gits_result_shape() {
        let dir = repo();
        let sync = run_git(&dir.0, &["rev-parse", "HEAD"], &[], GIT_TIMEOUT).await;
        let r = run_git_async(&dir.0, &["rev-parse", "HEAD"], GitOptions::default()).await;
        assert!(r.ok);
        assert_eq!(r.out, sync.out);
        assert!(!r.timed_out);
        let failed = run_git_async(&dir.0, &["rev-parse", "no-such-ref"], GitOptions::default()).await;
        assert!(!failed.ok);
        assert!(failed.out.contains("no-such-ref"), "{}", failed.out);
    }

    #[tokio::test]
    async fn run_git_async_keeps_stdouts_bytes_and_takes_input() {
        let dir = repo();
        let bytes = [0u8, 255, 10, 1];
        std::fs::write(dir.0.join("bin"), bytes).unwrap();
        let hashed = run_git_async(&dir.0, &["hash-object", "-w", "bin"], GitOptions::default()).await;
        let blob = run_git_async(&dir.0, &["cat-file", "blob", hashed.out.trim()], GitOptions::default()).await;
        assert_eq!(blob.bytes, bytes);
        let input = format!("{}\n", hashed.out.trim());
        let checked = run_git_async(
            &dir.0,
            &["cat-file", "--batch-check=%(objectname)"],
            GitOptions { input: Some(input.as_bytes()), ..Default::default() },
        )
        .await;
        assert_eq!(checked.out.trim(), hashed.out.trim());
    }

    #[tokio::test]
    async fn run_git_async_never_runs_hooks_or_fsmonitor() {
        use marv_tools::github::fake::write_executable;
        let dir = repo();
        let marker = dir.0.join("ran");
        let script = dir.0.join("touch.sh");
        let body = format!("#!/bin/sh\ntouch {}\n", marker.display());
        write_executable(&script, &body);
        let hook = dir.0.join(".git/hooks/pre-commit");
        write_executable(&hook, &body);
        sh_git(&dir.0, &["config", "core.fsmonitor", &script.to_string_lossy()]);
        std::fs::write(dir.0.join("a.txt"), "changed\n").unwrap();
        run_git_async(&dir.0, &["status", "--porcelain"], GitOptions::default()).await;
        run_git_async(&dir.0, &["commit", "-q", "-am", "x"], GitOptions::default()).await;
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn run_git_async_ignores_an_inherited_git_dir() {
        // The inherited environment is what git_environment filters; checked there without touching this process's.
        let dir = repo();
        let other = repo();
        let env = git_environment([("GIT_DIR".to_string(), other.0.join(".git").to_string_lossy().into_owned())], &[]);
        assert!(!env.contains_key("GIT_DIR"));
        let top = run_git_async(&dir.0, &["rev-parse", "--show-toplevel"], GitOptions::default()).await;
        assert_eq!(top.out.trim(), sh_git(&dir.0, &["rev-parse", "--show-toplevel"]));
    }

    #[tokio::test]
    async fn a_timeout_kills_git() {
        let dir = repo();
        // An alias that runs a shell command for a long time stands in for a git that hangs.
        let started = std::time::Instant::now();
        let r = run_git_async(
            &dir.0,
            &["-c", "alias.wait=!sleep 5", "wait"],
            GitOptions { timeout: Duration::from_millis(200), ..Default::default() },
        )
        .await;
        assert!(r.timed_out);
        assert!(!r.ok);
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn a_stop_kills_git() {
        let dir = repo();
        let stop = tokio_util::sync::CancellationToken::new();
        let later = stop.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            later.cancel();
        });
        let started = std::time::Instant::now();
        let r = run_git_async(
            &dir.0,
            &["-c", "alias.wait=!sleep 5", "wait"],
            GitOptions { cancel: Some(&stop), ..Default::default() },
        )
        .await;
        assert!(!r.ok);
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
