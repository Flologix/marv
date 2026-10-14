//! `docs/SECURITY.md`, "Sandbox" (`.git` read-only) for worktree subagents: inside the sandbox a worktree subagent
//! can change only its worktree's files; the repository's shared `.git` is read-only, so it can't commit, move
//! branches or plant hooks or config that git would later run *outside* the sandbox. **Marv commits for it**
//! (`marv_session::worktree::finish_worktree`), through a git dir of its own, so nothing the worktree says is trusted.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use marv_guards::{Recorder, TempDir, call, require_bwrap, sandbox_with_home};
use marv_session::subagent::subagent_context;
use marv_session::worktree::{create_worktree, finish_worktree};
use marv_tools::{Decision, ToolContext, builtin_tools, run_tool};
use serde_json::json;

fn git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git").args(args).current_dir(cwd).output().expect("git runs");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A repository with one commit, outside `/tmp` (which the sandbox replaces with its own).
fn repo() -> TempDir {
    let repo = TempDir::outside_tmp("wt-repo");
    repo.write("a.txt", "one\n");
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["config", "user.email", "t@example.com"],
        &["config", "user.name", "T"],
        &["config", "commit.gpgSign", "false"],
        &["add", "-A"],
        &["commit", "-q", "-m", "first"],
    ] {
        git(repo.path(), args);
    }
    repo
}

/// **A worktree subagent's `.git` is read-only, and Marv commits for it.** The subagent's bash runs as
/// `run_subagent` runs it: `subagent_context` in a sandboxed worktree, where its approver says yes without asking.
/// Its own `git commit`, a new branch and a planted hook all fail on the read-only `.git`; when it ends, Marv's
/// commit puts its change on its branch, and the main checkout and other refs are untouched.
///
/// Proven: emptied `read_only` in `subagent_context` → this test failed (the auto-approved, unconfined command
/// committed and planted the hook); made `finish_worktree` skip `commit_changes` → this test failed (no commit on
/// the branch).
#[tokio::test]
async fn a_worktree_subagents_git_is_read_only_and_marv_commits_for_it() {
    require_bwrap!();
    let (repo, trees, home) = (repo(), TempDir::outside_tmp("wt-trees"), TempDir::outside_tmp("wt-home"));
    let wt = create_worktree(repo.path(), trees.path(), "guard").await.expect("a worktree");
    let refs = git(repo.path(), &["for-each-ref"]);

    let mut parent = ToolContext::new(repo.path());
    parent.sandbox = Arc::new(sandbox_with_home(home.path()));
    let approver = Recorder::new(Decision::No);
    let ctx = subagent_context(&parent, "general-purpose · guard", Some(&wt), Some(approver.clone()));
    let hook = wt.git_dir.join("hooks").join("pre-commit");

    let write = run_tool(&call("write_file", json!({"path": "b.txt", "content": "bee\n"})), &ctx, builtin_tools()).await;
    assert!(!write.result.is_error, "{}", write.result.output);
    for command in [
        "git add -A && git -c user.email=s@x -c user.name=S commit -qm sub".to_string(),
        "git branch other".to_string(),
        format!("mkdir -p {0} && echo 'touch /tmp/pwned' > {0}/pre-commit", hook.parent().unwrap().display()),
    ] {
        let ran = run_tool(&call("bash", json!({"command": command})), &ctx, builtin_tools()).await;
        assert!(ran.result.output.contains("Read-only file system"), "{command}: {}", ran.result.output);
    }
    assert!(approver.requests().is_empty(), "a sandboxed worktree's changes run without asking");
    assert!(!hook.exists(), "a hook was planted");
    assert_eq!(git(repo.path(), &["for-each-ref"]), refs, "the subagent moved a ref");

    let line = finish_worktree(&wt, "guard", false).await;
    assert!(line.contains(&format!("Branch {}: 1 commit on {}", wt.branch, wt.base)), "{line}");
    assert_eq!(git(repo.path(), &["log", "-1", "--format=%s", &wt.branch]), "marv: guard");
    assert_eq!(git(repo.path(), &["show", &format!("{}:b.txt", wt.branch)]), "bee");
    assert_eq!(git(repo.path(), &["log", "--format=%s", "main"]), "first");
    assert!(!repo.path().join("b.txt").exists(), "the main checkout is untouched");
}

/// **Marv's commit trusts nothing the worktree says** (the second layer, for a `.git` that was writable after all):
/// a `.git` file pointing at a fake repository, a redirected HEAD and `commondir` in the worktree's record, and a
/// hook in the shared `.git`. The commit still lands on the worktree's branch, and no program runs.
///
/// Proven: made `commit_changes` pin `GIT_DIR` to the worktree's record (`admin_dir`) instead of `commit_git_dir`
/// → this test failed; removed `NO_PROGRAMS` from `run_git` → this test failed (the hook ran).
#[tokio::test]
async fn marvs_commit_trusts_nothing_the_worktree_says() {
    let (repo, trees) = (repo(), TempDir::outside_tmp("wt-trees2"));
    let wt = create_worktree(repo.path(), trees.path(), "planted").await.expect("a worktree");
    let pwned = trees.path().join("PWNED");
    let hook = trees.write("hook.sh", &format!("#!/bin/sh\ntouch {}\n", pwned.display()));
    std::fs::create_dir_all(wt.git_dir.join("hooks")).unwrap();
    std::fs::copy(&hook, wt.git_dir.join("hooks").join("pre-commit")).unwrap();
    Command::new("chmod").arg("+x").arg(wt.git_dir.join("hooks").join("pre-commit")).status().unwrap();
    let fake = trees.path().join("fake.git");
    git(trees.path(), &["init", "-q", "--bare", &fake.to_string_lossy()]);
    git(&fake, &["config", "core.fsmonitor", &format!("touch {}", pwned.display())]);
    std::fs::write(wt.dir.join(".git"), format!("gitdir: {}\n", fake.display())).unwrap();
    std::fs::write(wt.admin_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(wt.dir.join("b.txt"), "bee\n").unwrap();

    let line = finish_worktree(&wt, "planted", false).await;
    assert!(line.contains("1 commit"), "{line}");
    assert!(!pwned.exists(), "a planted program ran");
    assert_eq!(git(repo.path(), &["show", &format!("{}:b.txt", wt.branch)]), "bee");
    assert_eq!(git(repo.path(), &["log", "--format=%s", "main"]), "first", "the commit moved to main");
}
