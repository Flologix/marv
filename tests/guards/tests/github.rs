//! `docs/SECURITY.md`, "The doors out of the sandbox": the github tool (Marv #47). Ports Marv's
//! `tests/guards/github.test.ts`.
//!
//! The github tool runs gh outside the sandbox with the user's login, so what it can never do must never reach gh,
//! whatever the approver says: every form of auth, config, extensions, aliases (an unknown word), gh api's --input,
//! --web, another host. Writes, other API methods, graphql and sensitive reads ask (yolo or not); tokens in gh's output
//! never reach the model, the events, the saved session or the trajectory; a subagent can't write. gh is a fake on
//! PATH (`marv_tools::github::fake`) that logs every argv: an empty log means nothing ran. The controls (reads, an
//! approved write) show the log does fill when a call is allowed.
//!
//! Each test runs in a child process with the fake gh first on PATH (`in_child_with_env`): the session's github tool
//! reads the real environment, as Marv's does.

use std::path::{Path, PathBuf};

use marv_guards::{Recorder, Scripted, TempDir, in_child_with_env, say, sdk_turn, tool_call, use_tools};
use marv_tools::Decision;
use marv_tools::github::fake::{fake_gh_calls, install_fake_gh};
use serde_json::json;

const TOKEN: &str = "ghp_ZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ";

/// The fake gh, a project with an origin on github.com, and Marv's folder. The parent makes them and passes their
/// paths to the child, which runs the test.
struct Setup {
    bin: PathBuf,
    root: PathBuf,
    config: PathBuf,
    _dirs: Option<TempDir>,
}

impl Setup {
    /// In the parent: the folders, and the child's environment. In the child: the folders it was given.
    fn new(test: &str, extra: &[(&str, &str)]) -> Option<Self> {
        if let Ok(base) = std::env::var("GUARD_DIR") {
            let base = PathBuf::from(base);
            return Some(Self { bin: base.join("bin"), root: base.join("root"), config: base.join("config"), _dirs: None });
        }
        let dirs = TempDir::new("gh");
        let (bin, root, config) = (dirs.path().join("bin"), dirs.path().join("root"), dirs.path().join("config"));
        install_fake_gh(&bin);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&config).unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["remote", "add", "origin", "https://github.com/o/r.git"]);
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
        let log = bin.join("calls.log").display().to_string();
        let base = dirs.path().display().to_string();
        let mut env = vec![
            ("PATH", path.as_str()),
            ("FAKE_GH_LOG", log.as_str()),
            ("FAKE_GH_LOGGED_IN", "1"),
            ("GUARD_DIR", base.as_str()),
        ];
        env.extend_from_slice(extra);
        let ran_here = in_child_with_env(test, &env);
        assert!(!ran_here);
        None
    }
    /// gh's calls that weren't Marv's own availability check.
    fn gh_runs(&self) -> Vec<Vec<String>> {
        fake_gh_calls(&self.bin.join("calls.log"))
            .into_iter()
            .map(|c| c.args)
            .filter(|a| a != &["auth", "status", "--active"])
            .collect()
    }
}

fn git(cwd: &Path, args: &[&str]) {
    assert!(std::process::Command::new("git").args(args).current_dir(cwd).status().unwrap().success());
}

fn gh(id: &str, args: &[&str]) -> marv_core::ToolCall {
    tool_call(id, "github", json!({ "args": args }))
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| a.to_string()).collect()
}

const NEVER: &[&[&str]] = &[
    &["auth", "token"],
    &["auth", "status", "--show-token"],
    &["auth", "status", "--show-token", "--hostname", "github.com"],
    &["-h", "auth", "token"],
    &["--hostname", "x", "auth", "token"],
    &["auth", "login", "--with-token"],
    &["config", "get", "oauth_token"],
    &["extension", "exec", "evil"],
    &["myalias"],
    &["myalias", "run"],
    &["alias", "set", "co", "pr checkout"],
    &["secret", "list"],
    &["api", "--input", "/etc/passwd", "repos/o/r/issues"],
    &["api", "--verbose", "user"],
    &["pr", "view", "1", "--web"],
    &["pr", "view", "1", "-w"],
    &["pr", "list", "-R", "evil.example/o/r"],
    &["api", "--hostname", "evil.example", "user"],
    &["pr", "view", "https://evil.example/o/r/pull/1"],
    &["pr", "checkout", "1"],
    &["repo", "clone", "o/r"],
    &["run", "download", "1"],
];

/// What can never run never reaches gh, even with an approver that says yes to everything, and isn't asked.
///
/// Proven: reverted `allow::never`'s "auth" arm (so `auth` fell through to the unknown-word refusal) → the reason
/// check failed.
#[tokio::test]
async fn what_can_never_run_never_reaches_gh_and_isnt_asked() {
    let Some(s) = Setup::new("what_can_never_run_never_reaches_gh_and_isnt_asked", &[]) else { return };
    let calls = NEVER.iter().enumerate().map(|(i, a)| gh(&format!("n{i}"), a)).collect();
    let approver = Recorder::new(Decision::Yes);
    let run = sdk_turn(&s.root, &s.config, Scripted::new(vec![use_tools(calls), say("Done.")]), |o| {
        o.github = true;
        o.approve = Some(approver.clone());
    })
    .await;
    assert_eq!(run.results.len(), NEVER.len());
    for r in &run.results {
        assert!(r.is_error, "{}", r.output);
    }
    // Each refused for its own rule, said so the model learns it (not only by the catch-all for unknown words).
    let reason = |args: &[&str]| run.results[NEVER.iter().position(|a| *a == args).unwrap()].output.clone();
    assert!(reason(&["auth", "token"]).contains("Marv keeps the token away from the conversation"));
    assert!(reason(&["config", "get", "oauth_token"]).contains("gh config … is never run"));
    assert!(reason(&["extension", "exec", "evil"]).contains("an extension is a program"));
    assert!(reason(&["-h", "auth", "token"]).contains("Put the command first"));
    assert!(reason(&["--hostname", "x", "auth", "token"]).contains("Put the command first"));
    assert!(reason(&["pr", "list", "-R", "evil.example/o/r"]).contains("only reaches the project's own GitHub host"));
    assert!(approver.requests().is_empty());
    assert!(s.gh_runs().is_empty(), "{:?}", s.gh_runs());
}

/// Writes, other API methods, fields, graphql and sensitive reads ask in yolo; with a no, nothing runs.
///
/// Proven: made `Github::needs_approval` say no for everything → the writes ran unasked (asked was empty, the log
/// filled).
#[tokio::test]
async fn writes_and_sensitive_reads_ask_in_yolo_and_a_no_runs_nothing() {
    let Some(s) = Setup::new("writes_and_sensitive_reads_ask_in_yolo_and_a_no_runs_nothing", &[]) else { return };
    let asks: &[&[&str]] = &[
        &["pr", "comment", "1", "--body", "hi"],
        &["pr", "merge", "1"],
        &["issue", "close", "2"],
        &["api", "-X", "POST", "repos/o/r/issues"],
        &["api", "repos/o/r/issues", "-f", "title=x"],
        &["api", "graphql", "-f", "query=mutation{x}"],
        &["api", "repos/o/r/actions/secrets"],
        &["api", "repos/o/r/hooks"],
    ];
    // A "no" stops the run at the first: it's asked, and the rest are "not run".
    let approver = Recorder::new(Decision::No);
    let calls = asks.iter().enumerate().map(|(i, a)| gh(&format!("w{i}"), a)).collect();
    sdk_turn(&s.root, &s.config, Scripted::new(vec![use_tools(calls), say("Done.")]), |o| {
        o.github = true;
        o.approve = Some(approver.clone());
    })
    .await;
    let asked = approver.requests();
    assert_eq!(asked.len(), 1);
    assert_eq!(
        (asked[0].tool.as_str(), asked[0].door, asked[0].scope.key.as_str()),
        ("github", Some(marv_core::Door::Github), "github:pr comment")
    );
    for (i, args) in asks.iter().enumerate() {
        let one = Recorder::new(Decision::No);
        sdk_turn(&s.root, &s.config, Scripted::new(vec![use_tools(vec![gh(&format!("a{i}"), args)]), say("Done.")]), |o| {
            o.github = true;
            o.approve = Some(one.clone());
        })
        .await;
        assert_eq!(one.requests().iter().map(|r| r.door).collect::<Vec<_>>(), [Some(marv_core::Door::Github)], "{args:?}");
    }
    // gh ran only for the repository name the merge and close approvals show.
    let others: Vec<_> = s.gh_runs().into_iter().filter(|a| a[0] != "repo").collect();
    assert!(others.is_empty(), "{others:?}");
}

/// Control: reads run without asking in yolo, and an approved write runs.
#[tokio::test]
async fn control_reads_run_unasked_and_an_approved_write_runs() {
    let Some(s) = Setup::new("control_reads_run_unasked_and_an_approved_write_runs", &[]) else { return };
    let no = Recorder::new(Decision::No);
    let reads =
        vec![gh("r1", &["pr", "list"]), gh("r2", &["run", "view", "5", "--log-failed"]), gh("r3", &["api", "repos/o/r/pulls"])];
    sdk_turn(&s.root, &s.config, Scripted::new(vec![use_tools(reads), say("Done.")]), |o| {
        o.github = true;
        o.approve = Some(no.clone());
    })
    .await;
    assert!(no.requests().is_empty());
    let yes = Recorder::new(Decision::Yes);
    let write = vec![gh("w1", &["pr", "comment", "1", "--body", "hi"])];
    sdk_turn(&s.root, &s.config, Scripted::new(vec![use_tools(write), say("Done.")]), |o| {
        o.github = true;
        o.approve = Some(yes.clone());
    })
    .await;
    assert_eq!(yes.requests().len(), 1);
    // The reads run together (marv-rs #58), so in any order; the write after them.
    let mut runs = s.gh_runs();
    let write = runs.pop();
    runs.sort();
    assert_eq!(
        runs,
        [strings(&["api", "repos/o/r/pulls"]), strings(&["pr", "list"]), strings(&["run", "view", "5", "--log-failed"])]
    );
    assert_eq!(write, Some(strings(&["pr", "comment", "1", "--body", "hi"])));
}

/// A token in gh's output never reaches the model, the events, the saved session or the trajectory.
///
/// Proven: replaced `redact(&format_output(..))` with the plain output in `Github::run` → the token showed up in the
/// events, the requests and the files.
#[tokio::test]
async fn a_token_in_ghs_output_never_reaches_the_model_the_events_the_saved_session_or_the_trajectory() {
    let output = format!("issue body: here's my token {TOKEN}");
    let name = "a_token_in_ghs_output_never_reaches_the_model_the_events_the_saved_session_or_the_trajectory";
    let Some(s) = Setup::new(name, &[("FAKE_GH_OUTPUT", output.as_str())]) else { return };
    let model = Scripted::new(vec![use_tools(vec![gh("r1", &["issue", "view", "1"])]), say("Done.")]);
    let run = sdk_turn(&s.root, &s.config, model.clone(), |o| {
        o.github = true;
        o.persist = marv_session::sdk::Store::Default;
        o.trajectories = marv_session::sdk::Store::Default;
    })
    .await;
    assert!(run.results[0].output.contains("issue body: here's my token [redacted token]"), "{}", run.results[0].output);
    assert!(!format!("{:?}", run.events).contains(TOKEN));
    let requests = format!("{:?}", model.requests.lock().unwrap());
    assert!(!requests.contains(TOKEN));
    assert!(requests.contains("[redacted token]"));
    let files = walk(&s.config);
    assert!(files.iter().any(|f| f.extension().is_some_and(|e| e == "jsonl") && f.to_string_lossy().contains("trajectories")));
    assert!(files.iter().any(|f| f.to_string_lossy().contains("sessions")));
    for file in &files {
        let text = std::fs::read_to_string(file).unwrap_or_default();
        assert!(!text.contains(TOKEN), "{}", file.display());
    }
    // The control: gh really printed it.
    assert_eq!(s.gh_runs(), [strings(&["issue", "view", "1"])]);
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(walk(&path));
        } else {
            files.push(path);
        }
    }
    files
}

/// A subagent reads, but its write is refused, even with the session's approver saying yes.
///
/// Proven: removed the `ctx.doors.github` check in `Github::judge` → the subagent's comment reached the approver and
/// gh.
#[tokio::test]
async fn a_subagent_reads_but_its_write_is_refused_even_with_the_approver_saying_yes() {
    let Some(s) = Setup::new("a_subagent_reads_but_its_write_is_refused_even_with_the_approver_saying_yes", &[]) else {
        return;
    };
    let approver = Recorder::new(Decision::Yes);
    let model = Scripted::new(vec![
        use_tools(vec![tool_call("a1", "agent", json!({"description": "review", "prompt": "comment on the PR"}))]),
        use_tools(vec![gh("s1", &["pr", "view", "1"]), gh("s2", &["pr", "comment", "1", "--body", "LGTM"])]),
        say("Tried."),
        say("Done."),
    ]);
    let run = sdk_turn(&s.root, &s.config, model, |o| {
        o.github = true;
        o.approve = Some(approver.clone());
    })
    .await;
    assert!(run.results[0].output.contains("Tried."), "{}", run.results[0].output);
    assert!(approver.requests().is_empty());
    assert_eq!(s.gh_runs(), [strings(&["pr", "view", "1"])]);
}

/// Not offered where gh is logged out: the model can't call it, nor can its subagents.
///
/// Proven: offered the github tool in `MarvSession::offered` whatever `init.github` says → the call ran.
#[tokio::test]
async fn not_offered_where_gh_is_logged_out_not_even_to_subagents() {
    let name = "not_offered_where_gh_is_logged_out_not_even_to_subagents";
    let Some(s) = Setup::new(name, &[("FAKE_GH_LOGGED_IN", "0")]) else { return };
    let yes = Recorder::new(Decision::Yes);
    let run = sdk_turn(&s.root, &s.config, Scripted::new(vec![use_tools(vec![gh("r1", &["pr", "list"])]), say("Done.")]), |o| {
        o.github = true;
        o.approve = Some(yes.clone());
    })
    .await;
    assert!(run.results[0].output.contains("Unknown tool \"github\""), "{}", run.results[0].output);
    let model = Scripted::new(vec![
        use_tools(vec![tool_call("a1", "agent", json!({"description": "look", "prompt": "list PRs"}))]),
        use_tools(vec![gh("s1", &["pr", "list"])]),
        say("Tried."),
        say("Done."),
    ]);
    sdk_turn(&s.root, &s.config, model, |o| {
        o.github = true;
        o.approve = Some(yes.clone());
    })
    .await;
    assert!(s.gh_runs().is_empty(), "{:?}", s.gh_runs());
}

// ---- marv -p ----

fn script_call(args: &[&str]) -> marv_oracle::script::ScriptCall {
    marv_oracle::script::ScriptCall { id: Some("g1".into()), name: "github".into(), arguments: json!({ "args": args }) }
}

/// The real CLI with the fake gh on PATH (and nothing else from this environment).
fn headless_gh(s: &HeadlessSetup, args: &[&str], approve: &[&str]) -> marv_guards::Headless {
    let path = format!("{}:{}", s.bin.display(), std::env::var("PATH").unwrap_or_default());
    let log = s.bin.join("calls.log").display().to_string();
    let env = [("PATH", path.as_str()), ("FAKE_GH_LOG", log.as_str()), ("FAKE_GH_LOGGED_IN", "1")];
    marv_guards::headless(&s.root, &s.dirs.path().join("config"), vec![script_call(args)], approve, &env)
}

struct HeadlessSetup {
    dirs: TempDir,
    bin: PathBuf,
    root: PathBuf,
}

impl HeadlessSetup {
    fn new() -> Self {
        let dirs = TempDir::new("gh-p");
        let (bin, root) = (dirs.path().join("bin"), dirs.path().join("root"));
        install_fake_gh(&bin);
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["remote", "add", "origin", "https://github.com/o/r.git"]);
        Self { dirs, bin, root }
    }
    fn gh_runs(&self) -> Vec<Vec<String>> {
        fake_gh_calls(&self.bin.join("calls.log"))
            .into_iter()
            .map(|c| c.args)
            .filter(|a| a != &["auth", "status", "--active"])
            .collect()
    }
}

/// `marv -p`: a write is refused (exit 3) without `--approve github`, whatever else is approved, and nothing runs.
///
/// Proven: added "github:" to `network`'s prefixes in `APPROVE_PREFIXES` → the write ran (exit 0).
#[test]
fn headless_a_write_is_refused_without_approve_github() {
    let s = HeadlessSetup::new();
    let all_but = ["git_write", "network", "web", "mcp", "files", "memory", "agent_worktree"];
    let out = headless_gh(&s, &["pr", "comment", "1", "--body", "x"], &all_but);
    assert_eq!(out.code, Some(3), "{}", out.result);
    assert_eq!(out.result["refused"][0]["tool"], "github");
    assert!(s.gh_runs().is_empty());
}

/// `marv -p`: auth is refused even with `--approve github`.
#[test]
fn headless_auth_is_refused_even_with_approve_github() {
    let s = HeadlessSetup::new();
    let out = headless_gh(&s, &["auth", "token"], &["github"]);
    assert_eq!(out.result["reply"], "Done.");
    assert!(s.gh_runs().is_empty());
}

/// Control: `marv -p` runs reads with nothing approved, and a write with `--approve github`.
#[test]
fn headless_control_reads_run_and_a_write_with_approve_github() {
    let s = HeadlessSetup::new();
    assert_eq!(headless_gh(&s, &["pr", "list"], &[]).code, Some(0));
    assert_eq!(headless_gh(&s, &["pr", "comment", "1", "--body", "x"], &["github"]).code, Some(0));
    assert_eq!(s.gh_runs(), [strings(&["pr", "list"]), strings(&["pr", "comment", "1", "--body", "x"])]);
}
