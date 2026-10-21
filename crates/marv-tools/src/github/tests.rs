//! The github tool (#47) against a fake gh on PATH (`fake`): reads run unasked, writes ask as the github door, gh is
//! spawned without a shell and without Marv's keys, tokens are redacted. Ports Marv's `tests/github-tool.test.ts`
//! (its session and `marv -p` parts are in marv-session and marv-cli) and the ghEnv part of `tests/host-env.test.ts`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use marv_core::ToolCall;

use super::fake::{FakeGhCall, fake_gh_calls, install_fake_gh};
use super::*;
use crate::registry::{ToolRef, run_tool};
use crate::test_support::{TempDir, git};
use crate::types::{Approval, ApprovalRequest, Approver, Decision, Doors};

/// A project with an origin on github.com, a fake gh first on PATH, and the variables Marv's tests set.
struct Fixture {
    root: TempDir,
    bin: TempDir,
    log: PathBuf,
    env: Arc<Mutex<Env>>,
}

impl Fixture {
    fn new() -> Self {
        let root = TempDir::new("gh-tool");
        let bin = TempDir::new("gh-bin");
        install_fake_gh(bin.path());
        git(root.path(), &["init", "-q"]);
        git(root.path(), &["remote", "add", "origin", "https://github.com/o/r.git"]);
        let log = bin.path().join("calls.log");
        let path = format!("{}:{}", bin.path().display(), std::env::var("PATH").unwrap_or_default());
        let env = vec![
            ("PATH".into(), path),
            ("HOME".into(), std::env::var("HOME").unwrap_or_default()),
            ("FAKE_GH_LOG".into(), log.display().to_string()),
            ("FAKE_GH_LOGGED_IN".into(), "1".into()),
            ("OPENROUTER_API_KEY".into(), "sk-or-secret".into()),
            ("MARV_SECRET".into(), "marv-secret".into()),
            ("GH_DEBUG".into(), "api".into()),
        ];
        Self { root, bin, log, env: Arc::new(Mutex::new(env)) }
    }
    fn root(&self) -> &Path {
        self.root.path()
    }
    fn set(&self, name: &str, value: &str) {
        let mut env = self.env.lock().unwrap();
        env.retain(|(k, _)| k != name);
        env.push((name.into(), value.into()));
    }
    fn env(&self) -> Env {
        self.env.lock().unwrap().clone()
    }
    fn tools(&self) -> Vec<ToolRef> {
        let env = self.env.clone();
        vec![Arc::new(Github::with_env(move || env.lock().unwrap().clone()))]
    }
    fn calls(&self) -> Vec<FakeGhCall> {
        fake_gh_calls(&self.log)
    }
    fn args(&self) -> Vec<Vec<String>> {
        self.calls().into_iter().map(|c| c.args).collect()
    }
    fn ctx(&self, approver: Option<Arc<Recorder>>, yolo: bool, github: bool) -> ToolContext {
        let mut ctx = ToolContext::new(self.root());
        ctx.approver = approver.map(|a| a as Arc<dyn Approver>);
        ctx.yolo = yolo;
        ctx.doors = Doors { github, ..Doors::default() };
        ctx
    }
    async fn run(&self, args: &[&str], ctx: &ToolContext) -> crate::ToolOutcome {
        run_tool(&call(args, "g1"), ctx, &self.tools()).await
    }
}

fn call(args: &[&str], id: &str) -> ToolCall {
    ToolCall { id: id.into(), name: "github".into(), arguments: json!({ "args": args }).to_string() }
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| a.to_string()).collect()
}

struct Recorder(Decision, Mutex<Vec<ApprovalRequest>>);
impl Recorder {
    fn new(decision: Decision) -> Arc<Self> {
        Arc::new(Self(decision, Mutex::default()))
    }
    fn asked(&self) -> Vec<ApprovalRequest> {
        self.1.lock().unwrap().clone()
    }
}
#[async_trait]
impl Approver for Recorder {
    async fn approve(&self, request: ApprovalRequest) -> Decision {
        self.1.lock().unwrap().push(request);
        self.0.clone()
    }
}

#[tokio::test]
async fn a_read_runs_without_asking_yolo_or_not_as_gh_with_the_project_as_cwd() {
    for yolo in [true, false] {
        let f = Fixture::new();
        let approver = Recorder::new(Decision::Yes);
        let out = f.run(&["pr", "list"], &f.ctx(Some(approver.clone()), yolo, true)).await;
        assert!(approver.asked().is_empty());
        assert_eq!(out.label, "gh pr list");
        assert_eq!(out.result.door, Some(Door::Github));
        assert!(out.result.output.contains("fake gh pr list"), "{}", out.result.output);
        assert!(out.result.output.contains("[exit code 0]"));
        let calls = f.calls();
        assert_eq!(calls.iter().map(|c| c.args.clone()).collect::<Vec<_>>(), [strings(&["pr", "list"])]);
        assert_eq!(calls[0].cwd, f.root());
    }
}

#[tokio::test]
async fn a_write_asks_as_the_github_door_scoped_by_command_and_names_the_repository_for_a_merge() {
    let f = Fixture::new();
    let approver = Recorder::new(Decision::Yes);
    let out = f.run(&["pr", "merge", "12", "--squash"], &f.ctx(Some(approver.clone()), true, true)).await;
    let asked = approver.asked();
    assert_eq!(asked.len(), 1);
    let r = &asked[0];
    assert_eq!((r.tool.as_str(), r.door, r.network, r.once), ("github", Some(Door::Github), true, false));
    assert_eq!((r.scope.key.as_str(), r.scope.description.as_str()), ("github:pr merge", "gh pr merge"));
    assert_eq!(r.preview.command.as_deref(), Some("gh pr merge 12 --squash"));
    assert_eq!(r.preview.warning.as_deref(), Some("Visible to other people on github.com/o/r."));
    assert_eq!(out.result.approval, Some(Approval::Decided(Decision::Yes)));
    assert_eq!(f.args().last().unwrap(), &strings(&["pr", "merge", "12", "--squash"]));
}

#[tokio::test]
async fn a_merge_whose_repository_cant_be_told_says_so_rather_than_naming_none() {
    let f = Fixture::new();
    f.set("FAKE_GH_NO_REPO", "1");
    let approver = Recorder::new(Decision::No);
    f.run(&["pr", "merge", "12"], &f.ctx(Some(approver.clone()), true, true)).await;
    assert_eq!(
        approver.asked()[0].preview.warning.as_deref(),
        Some("Couldn't tell which repository: gh picks it from this folder.")
    );
}

#[tokio::test]
async fn a_declined_write_never_reaches_gh() {
    let f = Fixture::new();
    let out = f.run(&["issue", "comment", "3", "--body", "hi"], &f.ctx(Some(Recorder::new(Decision::No)), false, true)).await;
    assert!(out.result.declined);
    assert!(f.calls().is_empty());
}

#[tokio::test]
async fn a_sensitive_api_read_asks_once_and_a_field_that_reads_a_file_says_so() {
    let f = Fixture::new();
    let approver = Recorder::new(Decision::No);
    let ctx = f.ctx(Some(approver.clone()), false, true);
    f.run(&["api", "repos/o/r/hooks"], &ctx).await;
    run_tool(&call(&["api", "repos/o/r/issues", "-F", "body=@/etc/hostname"], "g2"), &ctx, &f.tools()).await;
    let asked = approver.asked();
    assert_eq!(asked.iter().map(|r| r.scope.key.as_str()).collect::<Vec<_>>(), ["github:api sensitive", "github:api POST"]);
    assert!(asked[0].preview.warning.as_deref().unwrap().contains("webhook"));
    assert!(asked[1].preview.warning.as_deref().unwrap().contains("Reads /etc/hostname on your machine"));
    // One sensitive path approved "always" must not cover the others (they share a key): each is asked every time.
    assert!(asked[0].once);
    assert!(!asked[1].once);
}

#[tokio::test]
async fn gh_is_spawned_without_a_shell_an_argument_arrives_as_written() {
    let f = Fixture::new();
    let marker = f.root().join("x");
    let arg = format!("$(touch {})", marker.display());
    let out = f.run(&["search", "issues", &arg], &f.ctx(None, false, true)).await;
    assert!(!out.result.is_error, "{}", out.result.output);
    assert_eq!(f.calls()[0].args, strings(&["search", "issues", &arg]));
    assert!(!marker.exists());
}

#[tokio::test]
async fn its_environment_has_no_provider_keys_or_debug_switches_never_prompts_and_git_runs_no_programs() {
    let f = Fixture::new();
    f.run(&["pr", "status"], &f.ctx(None, false, true)).await;
    let env = &f.calls()[0].env;
    for gone in ["OPENROUTER_API_KEY", "MARV_SECRET", "GH_DEBUG"] {
        assert!(!env.contains_key(gone), "{gone}: {env:?}");
    }
    let want = [
        ("GH_PROMPT_DISABLED", "1"),
        ("GH_PAGER", "cat"),
        ("NO_COLOR", "1"),
        ("GIT_CONFIG_COUNT", "2"),
        ("GIT_CONFIG_KEY_0", "core.hooksPath"),
        ("GIT_CONFIG_VALUE_0", "/dev/null"),
        ("GIT_CONFIG_KEY_1", "core.fsmonitor"),
        ("GIT_CONFIG_VALUE_1", "false"),
    ];
    for (name, value) in want {
        assert_eq!(env.get(name).map(String::as_str), Some(value), "{name}");
    }
}

#[tokio::test]
async fn gh_missing_while_the_repository_is_looked_up_isnt_remembered() {
    let f = Fixture::new();
    let approver = Recorder::new(Decision::No);
    let ctx = f.ctx(Some(approver.clone()), true, true);
    // git still there (the project's host comes from it), gh not: looking up the repository fails to spawn.
    let git_only = f.root().join(".git-only-bin");
    std::fs::create_dir(&git_only).unwrap();
    let real_git = which("git", std::env::var("PATH").ok().as_deref(), &[]).unwrap();
    std::os::unix::fs::symlink(real_git, git_only.join("git")).unwrap();
    let with_gh = f.env().into_iter().find(|(k, _)| k == "PATH").unwrap().1;
    f.set("PATH", &git_only.display().to_string());
    f.run(&["pr", "merge", "12"], &ctx).await;
    f.set("PATH", &with_gh);
    run_tool(&call(&["pr", "merge", "12"], "g2"), &ctx, &f.tools()).await;
    assert_eq!(approver.asked().last().unwrap().preview.warning.as_deref(), Some("Visible to other people on github.com/o/r."));
}

#[tokio::test]
async fn gh_talks_to_the_projects_host_whatever_gh_host_says() {
    let f = Fixture::new();
    f.set("GH_HOST", "ghe.example.com");
    f.run(&["pr", "view", "1", "-R", "someone/else"], &f.ctx(None, false, true)).await;
    assert_eq!(f.calls().last().unwrap().env.get("GH_HOST").map(String::as_str), Some("github.com"));
}

#[tokio::test]
async fn a_project_whose_origin_names_no_host_gets_github_com_never_the_users_gh_host() {
    let f = Fixture::new();
    f.set("GH_HOST", "ghe.example.com");
    git(f.root(), &["remote", "set-url", "origin", "/srv/repos/local.git"]);
    f.run(&["pr", "view", "1", "-R", "someone/else"], &f.ctx(None, false, true)).await;
    assert_eq!(f.calls().last().unwrap().env.get("GH_HOST").map(String::as_str), Some("github.com"));
}

#[tokio::test]
async fn tokens_in_ghs_output_and_errors_are_redacted_and_the_result_says_how_many() {
    let f = Fixture::new();
    f.set("FAKE_GH_OUTPUT", &format!("body: ghp_{}", "A".repeat(36)));
    f.set("FAKE_GH_STDERR", &format!("warning: github_pat_{}", "b".repeat(30)));
    let out = f.run(&["issue", "view", "1"], &f.ctx(None, false, true)).await.result;
    assert!(!out.output.contains("ghp_A"));
    assert!(!out.output.contains("github_pat_b"));
    assert!(out.output.contains("body: [redacted token]"));
    assert!(out.output.contains("Marv: 2 tokens were redacted from this output."));
    assert!(out.summary.contains("2 tokens redacted"), "{}", out.summary);
}

#[tokio::test]
async fn a_failing_gh_reports_its_exit_code() {
    let f = Fixture::new();
    f.set("FAKE_GH_EXIT", "4");
    f.set("FAKE_GH_STDERR", "not found");
    let out = f.run(&["pr", "view", "999"], &f.ctx(None, false, true)).await.result;
    assert!(out.output.contains("not found"));
    assert!(out.output.contains("[exit code 4]"), "{}", out.output);
}

#[tokio::test]
async fn refused_calls_fail_before_gh_runs_with_the_reason() {
    let f = Fixture::new();
    let out = f.run(&["auth", "token"], &f.ctx(Some(Recorder::new(Decision::Yes)), false, true)).await.result;
    assert!(out.is_error);
    assert!(out.output.contains("Marv keeps the token away from the conversation"));
    assert!(f.calls().is_empty());
}

#[tokio::test]
async fn a_read_naming_another_host_is_refused_in_run() {
    let f = Fixture::new();
    let out = f.run(&["pr", "list", "-R", "ghe.corp/o/r"], &f.ctx(None, false, true)).await.result;
    assert!(out.is_error);
    assert!(out.output.contains("ghe.corp"));
    assert!(f.calls().is_empty());
}

#[tokio::test]
async fn on_github_enterprise_the_projects_own_host_is_allowed() {
    let f = Fixture::new();
    git(f.root(), &["remote", "set-url", "origin", "git@ghe.corp:o/r.git"]);
    assert_eq!(project_host(f.root(), &f.env()).await.as_deref(), Some("ghe.corp"));
    let ctx = f.ctx(None, false, true);
    let ok = f.run(&["api", "--hostname", "ghe.corp", "repos/o/r"], &ctx).await.result;
    assert!(!ok.is_error, "{}", ok.output);
    let other = f.run(&["api", "--hostname", "github.com", "repos/o/r"], &ctx).await.result;
    assert!(other.is_error);
}

#[tokio::test]
async fn outside_a_repository_the_host_is_github_com() {
    let plain = TempDir::new("gh-plain");
    assert_eq!(project_host(plain.path(), &process_env()).await.as_deref(), Some("github.com"));
}

#[tokio::test]
async fn without_the_github_door_a_subagent_writes_and_sensitive_reads_are_refused_reads_run() {
    let f = Fixture::new();
    let approver = Recorder::new(Decision::Yes);
    let ctx = f.ctx(Some(approver.clone()), false, false);
    let write = f.run(&["pr", "comment", "1", "--body", "x"], &ctx).await.result;
    let sensitive = run_tool(&call(&["api", "user/emails"], "g2"), &ctx, &f.tools()).await.result;
    let read = run_tool(&call(&["pr", "view", "1"], "g3"), &ctx, &f.tools()).await.result;
    assert!(write.output.contains("Only the main agent can change GitHub"));
    assert!(sensitive.output.contains("Only the main agent"));
    assert!(!read.is_error);
    assert!(approver.asked().is_empty());
    assert_eq!(f.args(), [strings(&["pr", "view", "1"])]);
}

#[test]
fn the_label_quotes_what_a_shell_would_split() {
    assert_eq!(
        gh_line(&strings(&["issue", "create", "--title", "it's broken", "--body", "a b"])),
        r"gh issue create --title 'it'\''s broken' --body 'a b'"
    );
}

#[tokio::test]
async fn an_argument_with_a_nul_or_more_than_40_is_invalid() {
    let f = Fixture::new();
    let ctx = f.ctx(None, false, true);
    assert!(f.run(&["pr", "view\0x"], &ctx).await.result.output.contains("Invalid input"));
    let many = vec!["x"; 41];
    assert!(f.run(&many, &ctx).await.result.output.contains("Invalid input"));
    assert!(f.run(&[], &ctx).await.result.output.contains("Invalid input"));
}

#[tokio::test]
async fn offered_when_gh_is_on_path_and_logged_in_not_when_logged_out_or_missing() {
    let f = Fixture::new();
    assert!(github_available(&f.env(), &[], AUTH_CHECK).await);
    f.set("FAKE_GH_LOGGED_IN", "0");
    assert!(!github_available(&f.env(), &[], AUTH_CHECK).await);
    assert!(!github_available(&[("PATH".into(), "/nonexistent".into())], &[], AUTH_CHECK).await);
}

#[tokio::test]
async fn when_gh_auth_status_doesnt_answer_in_time_ghs_hosts_yml_decides() {
    let f = Fixture::new();
    f.set("FAKE_GH_HANG", "1");
    let config = f.bin.path().join("gh-config");
    std::fs::create_dir(&config).unwrap();
    f.set("GH_CONFIG_DIR", &config.display().to_string());
    let quick = Duration::from_millis(200);
    assert!(!github_available(&f.env(), &[], quick).await);
    std::fs::write(config.join("hosts.yml"), "github.com:\n  user: me\n").unwrap();
    assert!(github_available(&f.env(), &[], quick).await);
}

#[test]
fn marv_github_off_hides_it() {
    assert!(github_wanted(None));
    for off in ["off", "0", "false", "NO", " no "] {
        assert!(!github_wanted(Some(off)), "{off}");
    }
    assert!(github_wanted(Some("on")));
}

/// Marv's `tests/host-env.test.ts`: gh gets its own tokens back, and nothing else secret.
#[test]
fn gh_env_gets_ghs_own_tokens_back_and_nothing_else_secret() {
    let env: Env = [
        ("PATH", "/usr/bin"),
        ("GITHUB_TOKEN", "ghp_x"),
        ("GH_TOKEN", "ghp_y"),
        ("ANTHROPIC_API_KEY", "sk-ant"),
        ("AWS_SECRET_ACCESS_KEY", "aws"),
        ("NPM_TOKEN", "npm"),
        ("DATABASE_URL", "postgres://user:hunter2@db:5432/app"),
        ("OPENROUTER_API_KEY", "sk-or"),
        ("GIT_DIR", "/elsewhere"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let got = gh_env(&env);
    assert_eq!(got.get("GH_TOKEN").map(String::as_str), Some("ghp_y"));
    assert_eq!(got.get("GITHUB_TOKEN").map(String::as_str), Some("ghp_x"));
    for name in ["ANTHROPIC_API_KEY", "AWS_SECRET_ACCESS_KEY", "NPM_TOKEN", "DATABASE_URL", "OPENROUTER_API_KEY", "GIT_DIR"] {
        assert!(!got.contains_key(name), "{name}");
    }
}
