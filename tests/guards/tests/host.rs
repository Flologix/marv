//! `docs/SECURITY.md`, "The doors out of the sandbox": bash with `sandbox: false` (Marv #52). Ports Marv's
//! `tests/guards/host.test.ts`.
//!
//! A host command runs on the user's machine, so it's asked every time and only where someone approves each command
//! at a terminal. It asks in yolo; "always" never covers a second call; it's refused, before any ask, in an SDK
//! session without `host_commands` (whatever its approver would say), in `marv -p` (with every `--approve` name there
//! is), and for a subagent. Each effect is a marker file outside the project and outside the sandbox's own /tmp
//! (`TempDir::outside_tmp`), so it shows up only if the command really ran on the host; the controls show it does
//! when allowed.

use std::path::Path;
use std::sync::Arc;

use marv_guards::{Recorder, Scripted, TempDir, say, sdk_turn, tool_call, use_tools};
use marv_tools::{Decision, ToolContext, ToolRef, run_tool};
use serde_json::json;

/// A command that leaves a marker per run in `outside`.
fn host_command(outside: &Path) -> String {
    format!("touch {}/ran-$(date +%s%N)", outside.display())
}
fn runs(outside: &Path) -> usize {
    std::fs::read_dir(outside).unwrap().count()
}
fn host_call(id: &str, command: &str) -> marv_core::ToolCall {
    tool_call(id, "bash", json!({ "command": command, "sandbox": false }))
}

/// Asks in yolo, with the host door and `once` (yolo never runs it unasked); a yes runs it on the host.
///
/// Proven: dropped `input.sandbox != Some(false)` from `Bash::auto_safe` → yolo ran it unasked, on the host.
#[tokio::test]
async fn asks_in_yolo_with_the_host_door_and_once() {
    marv_guards::require_sandbox!();
    let (root, outside) = (TempDir::new("host"), TempDir::outside_tmp("host-outside"));
    let tools: Vec<ToolRef> = vec![Arc::new(marv_tools::bash::Bash)];
    let call = host_call("h1", &host_command(outside.path()));
    let no = Recorder::new(Decision::No);
    let mut ctx = ToolContext::new(root.path());
    (ctx.yolo, ctx.doors.host) = (true, true);
    ctx.approver = Some(no.clone());
    let out = run_tool(&call, &ctx, &tools).await;
    let asked = no.requests();
    assert_eq!(asked.len(), 1);
    assert_eq!((asked[0].door, asked[0].once), (Some(marv_core::Door::Host), true));
    assert!(out.result.declined);
    assert_eq!(runs(outside.path()), 0);
    // Control: a yes really runs it on the host.
    ctx.approver = Some(Recorder::new(Decision::Yes));
    run_tool(&call, &ctx, &tools).await;
    assert_eq!(runs(outside.path()), 1);
}

/// Never uses "always": two calls, two asks, even when the client answers "always".
///
/// Proven: made `Bash::once` return false → the second call was answered from "always" (one ask).
#[tokio::test]
async fn never_uses_always_two_calls_two_asks() {
    marv_guards::require_sandbox!();
    let (root, config, outside) =
        (TempDir::new("host-always"), TempDir::new("host-always-config"), TempDir::outside_tmp("host-always-out"));
    let command = host_command(outside.path());
    let approver = Recorder::new(Decision::Always);
    let model =
        Scripted::new(vec![use_tools(vec![host_call("h1", &command)]), use_tools(vec![host_call("h2", &command)]), say("Done.")]);
    let run = sdk_turn(root.path(), config.path(), model, |o| {
        o.host_commands = true;
        o.approve = Some(approver.clone());
    })
    .await;
    let keys: Vec<String> = approver.requests().iter().map(|r| r.scope.key.clone()).collect();
    assert_eq!(keys, [format!("bash:host:{command}"), format!("bash:host:{command}")]);
    assert!(run.results.iter().all(|r| !r.is_error), "{:?}", run.results);
    assert_eq!(runs(outside.path()), 2);
}

/// An SDK session without `host_commands` refuses it before its approver is asked, even one that says yes to
/// everything (`marv -p`'s, given every `--approve` name, is such a session).
///
/// Proven: removed the `ctx.doors.host` check from `Bash::preview` and `Bash::run` → the approver was asked and the
/// command ran.
#[tokio::test]
async fn a_session_without_host_commands_refuses_it_before_its_approver_is_asked() {
    marv_guards::require_sandbox!();
    let (root, config, outside) =
        (TempDir::new("host-sdk"), TempDir::new("host-sdk-config"), TempDir::outside_tmp("host-sdk-out"));
    let approver = Recorder::new(Decision::Yes);
    let model = Scripted::new(vec![use_tools(vec![host_call("h1", &host_command(outside.path()))]), say("Done.")]);
    let run = sdk_turn(root.path(), config.path(), model, |o| o.approve = Some(approver.clone())).await;
    assert!(run.results[0].is_error);
    assert_eq!(run.results[0].output, marv_tools::bash::NO_HOST_DOOR);
    assert!(approver.requests().is_empty());
    assert_eq!(runs(outside.path()), 0);
}

/// A subagent can't: refused, even though the session has the door and its approver says yes.
///
/// Proven: gave the subagent's context the parent's doors in `subagent_context` → the subagent's command was asked
/// and ran.
#[tokio::test]
async fn a_subagent_cant_even_where_the_session_has_the_door() {
    marv_guards::require_sandbox!();
    let (root, config, outside) =
        (TempDir::new("host-sub"), TempDir::new("host-sub-config"), TempDir::outside_tmp("host-sub-out"));
    let approver = Recorder::new(Decision::Yes);
    let model = Scripted::new(vec![
        use_tools(vec![tool_call("a1", "agent", json!({"description": "set up", "prompt": "run it on the host"}))]),
        use_tools(vec![host_call("s1", &host_command(outside.path()))]),
        say("Tried."),
        say("Done."),
    ]);
    let run = sdk_turn(root.path(), config.path(), model, |o| {
        o.host_commands = true;
        o.approve = Some(approver.clone());
    })
    .await;
    assert!(run.results[0].output.contains("Tried."), "{}", run.results[0].output);
    assert!(approver.requests().iter().all(|r| r.door != Some(marv_core::Door::Host)));
    assert_eq!(runs(outside.path()), 0);
}

/// `marv -p`: refused (exit 3) with every `--approve` name there is, and nothing runs.
///
/// Two layers hold here: no `--approve` name matches `bash:host:`, and the session has no host door. Proven: set
/// `host_commands` in `run_headless` and let `--approve network` match `bash:host:` → the command ran (either alone:
/// still refused).
#[test]
fn headless_refuses_it_with_every_approve_name() {
    marv_guards::require_sandbox!();
    let (root, config, outside) = (TempDir::new("host-p"), TempDir::new("host-p-config"), TempDir::outside_tmp("host-p-out"));
    let call = marv_oracle::script::ScriptCall {
        id: Some("h1".into()),
        name: "bash".into(),
        arguments: json!({ "command": host_command(outside.path()), "sandbox": false }),
    };
    let names = ["git_write", "network", "web", "mcp", "files", "memory", "agent_worktree", "github"];
    let path = std::env::var("PATH").unwrap_or_default();
    let out =
        marv_guards::headless(root.path(), config.path(), vec![call], &names, &[("PATH", path.as_str()), ("MARV_GITHUB", "off")]);
    assert_eq!(out.code, Some(3), "{}", out.result);
    assert_eq!(out.result["refused"][0]["tool"], "bash");
    assert_eq!(runs(outside.path()), 0);
}
