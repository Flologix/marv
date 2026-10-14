//! `docs/MESH.md` rules, end to end (#15): the real switchboard, serving a subagent's plug, reached from inside real
//! bwrap through `/run/marv/mesh.sock` by a small MCP client (python3), the way anything in the sandbox would.

use std::sync::{Arc, Mutex};

use marv_guards::{TempDir, require_bwrap, run_sandboxed, sandbox_with_home};
use marv_mesh::switchboard::{MeshRecord, Outcome, Switchboard, TrajectorySink};
use marv_mesh::wire::{Approver, Grant, Wire, WireMode};
use marv_mesh::{PlugId, ReceptacleName};
use marv_sandbox::CommandMounts;
use marv_session::mesh::{MeshAccess, SubagentPlug, parent_id};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Log(Mutex<Vec<MeshRecord>>);
impl TrajectorySink for Log {
    fn mesh(&self, record: MeshRecord) {
        self.0.lock().unwrap().push(record);
    }
}

/// A served switchboard with the parent plugged in, a second worker, and the subagent's plug.
struct Mesh {
    mesh: Arc<Switchboard>,
    log: Arc<Log>,
    sub: SubagentPlug,
    cancel: CancellationToken,
    _runtime: TempDir,
}

impl Drop for Mesh {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn mesh() -> Mesh {
    let runtime = TempDir::new("mesh-rt");
    let log = Arc::new(Log::default());
    let mesh = Arc::new(Switchboard::new("g", runtime.path().join("plugs"), log.clone()));
    let cancel = CancellationToken::new();
    tokio::spawn(Arc::clone(&mesh).serve(cancel.clone()));
    mesh.plug(parent_id()).unwrap();
    // The subagent under test is worker-1; worker-2 is another agent the parent is wired to.
    let sub = MeshAccess { mesh: mesh.clone(), approver: None }.plug_subagent().unwrap();
    assert_eq!(sub.plug.id.as_str(), "worker-1");
    mesh.plug(PlugId::new("worker-2").unwrap()).unwrap();
    mesh.wire(send_wire("parent", "agent.worker-2.inbox")).unwrap();
    Mesh { mesh, log, sub, cancel, _runtime: runtime }
}

fn send_wire(from: &str, to: &str) -> Wire {
    Wire::new(PlugId::new(from).unwrap(), ReceptacleName::new(to).unwrap(), WireMode::Send, Grant::local(Approver::Tui))
}

/// An MCP client in python3: `initialize`, then each `(label, method, params)`, printing `<label> <response>` lines.
const CLIENT: &str = r#"
import json, socket, sys
def connect(path):
    s = socket.socket(socket.AF_UNIX)
    s.connect(path)
    return s.makefile("rw")
f = connect("/run/marv/mesh.sock")
n = 0
def rpc(method, params):
    global n
    n += 1
    f.write(json.dumps({"jsonrpc": "2.0", "id": n, "method": method, "params": params}) + "\n")
    f.flush()
    while True:
        r = json.loads(f.readline())
        if r.get("id") == n:
            return r
rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "guard", "version": "0"}})
for label, method, params in json.loads(sys.argv[1]):
    print(label, json.dumps(rpc(method, params)))
for path in sys.argv[2:]:
    try:
        connect(path)
        print("reached", path)
    except OSError:
        print("unreachable", path)
"#;

/// Runs [`CLIENT`] in the subagent's sandbox (its plug mounted) and returns each label's response.
async fn from_inside(m: &Mesh, calls: Value, other_sockets: &[&std::path::Path]) -> (Vec<(String, Value)>, String) {
    let (project, home) = (TempDir::new("mesh-proj"), TempDir::outside_tmp("mesh-home"));
    let sandbox = m.sub.sandbox(&sandbox_with_home(home.path()));
    let others: Vec<String> = other_sockets.iter().map(|p| format!("'{}'", p.display())).collect();
    let script = format!("python3 - '{}' {} <<'PY'\n{CLIENT}\nPY", calls, others.join(" "));
    // bwrap blocks; the switchboard answering it runs on the runtime meanwhile.
    let ran = tokio::task::spawn_blocking(move || {
        let ran = run_sandboxed(&sandbox, project.path(), &CommandMounts::default(), &script, &[]);
        (ran.output, ran.code)
    })
    .await
    .unwrap();
    let parsed = ran
        .0
        .lines()
        .filter_map(|l| l.split_once(' '))
        .filter_map(|(label, json)| serde_json::from_str(json).ok().map(|v| (label.to_string(), v)))
        .collect();
    (parsed, ran.0)
}

fn send(label: &str, to: &str, extra_meta: Value) -> Value {
    let mut meta = serde_json::json!({"marv/mode": "send"});
    meta.as_object_mut().unwrap().extend(extra_meta.as_object().cloned().unwrap_or_default());
    serde_json::json!([label, "tools/call", {"name": to, "arguments": {"text": label}, "_meta": meta}])
}

fn result_text(v: &Value) -> (String, bool) {
    let r = &v["result"];
    (r["content"][0]["text"].as_str().unwrap_or_default().to_string(), r["isError"].as_bool().unwrap_or(false))
}

fn python() -> bool {
    std::process::Command::new("python3").arg("-c").arg("").status().is_ok_and(|s| s.success())
}

/// **Nothing is wired by default; an unwired call is refused and logged** (docs/MESH.md rules 1 and 3). From inside
/// its sandbox, a subagent's plug lists nothing, and a send to the parent's inbox is refused, never queued, and
/// written to the trajectory as refused, with its sender and size.
///
/// Proven: made `Board::check` let a message through without a wire (`table.check` ignored) → this test failed (the
/// message was delivered); made `Switchboard::admit` skip its `MeshRecord` → failed (nothing logged).
#[tokio::test]
async fn an_unwired_call_from_the_sandbox_is_refused_and_logged() {
    require_bwrap!();
    if !python() {
        eprintln!("skipped: no python3 for the in-sandbox client");
        return;
    }
    let m = mesh();
    let calls = serde_json::json!([["list", "tools/list", {}], send("hello", "agent.parent.inbox", Value::Null)]);
    let (got, output) = from_inside(&m, calls, &[]).await;
    assert_eq!(got.len(), 2, "{output}");
    assert_eq!(got[0].1["result"]["tools"], serde_json::json!([]), "{output}");
    let (text, is_error) = result_text(&got[1].1);
    assert!(is_error, "{output}");
    assert!(text.starts_with("not wired: worker-1 has no send wire to agent.parent.inbox"), "{text}");
    assert!(m.mesh.drain_inbox(&parent_id()).is_empty());
    let log = m.log.0.lock().unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!((log[0].sender.as_str(), log[0].receptacle.as_str()), ("worker-1", "agent.parent.inbox"));
    assert!(matches!(log[0].outcome, Outcome::Refused { .. }));
    assert_eq!(log[0].size, r#"{"text":"hello"}"#.len());
}

/// **A sandboxed subagent reaches the parent only over its wire** (rule 5, no ambient authority). Wired
/// `worker-1 -> agent.parent.inbox` (send), from inside its sandbox:
/// - it sees and can use only that wire: not another agent's inbox the parent is wired to, not a `call`;
/// - it can't speak as anyone else: a `sender` in the message is ignored, the sender is the socket's plug;
/// - there's no method to wire anything from a plug;
/// - nothing but its own plug is reachable: the parent's socket isn't in the sandbox.
///
/// Proven: dropped the `w.from == from` check in `WireTable::find` → this test failed (worker-1 reached
/// agent.worker-2.inbox over the parent's wire); took the sender from `_meta.sender` in `Switchboard::handle` →
/// failed (the posing message went out as the parent's); bound the plug's folder into the sandbox next to its socket
/// (marv-sandbox's bwrap builder) → failed (the parent's socket was reachable).
#[tokio::test]
async fn a_sandboxed_subagent_reaches_the_parent_only_over_its_wire() {
    require_bwrap!();
    if !python() {
        eprintln!("skipped: no python3 for the in-sandbox client");
        return;
    }
    let m = mesh();
    m.mesh.wire(send_wire("worker-1", "agent.parent.inbox")).unwrap();
    let parent_socket = m.mesh.plug(parent_id()).unwrap().host_socket;
    let calls = serde_json::json!([
        ["list", "tools/list", {}],
        send("posing", "agent.parent.inbox", serde_json::json!({"sender": "parent", "from": "parent"})),
        send("sideways", "agent.worker-2.inbox", Value::Null),
        ["call", "tools/call", {"name": "agent.parent.inbox", "arguments": {"text": "call"}}],
        ["wire", "mesh/wire", {"from": "worker-1", "to": "agent.worker-2.inbox", "mode": "send"}],
    ]);
    let (got, output) = from_inside(&m, calls, &[&parent_socket]).await;
    let by = |label: &str| got.iter().find(|(l, _)| l == label).map(|(_, v)| v.clone()).unwrap_or_else(|| panic!("{output}"));

    let tools = by("list")["result"]["tools"].clone();
    assert_eq!(tools.as_array().unwrap().len(), 1, "{output}");
    assert_eq!(tools[0]["name"], "agent.parent.inbox");
    assert_eq!(result_text(&by("posing")), ("Delivered to agent.parent.inbox.".to_string(), false));
    let (sideways, refused) = result_text(&by("sideways"));
    assert!(refused && sideways.starts_with("not wired: worker-1"), "{sideways}");
    assert!(result_text(&by("call")).1, "a send wire grants no call");
    assert!(by("wire")["error"].is_object(), "no wiring from a plug: {output}");
    assert!(output.contains(&format!("unreachable {}", parent_socket.display())), "{output}");

    let delivered = m.mesh.drain_inbox(&parent_id());
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].sender.as_str(), "worker-1", "identity is the socket, never the message");
    assert!(m.mesh.drain_inbox(&PlugId::new("worker-2").unwrap()).is_empty());
    assert_eq!(m.mesh.table().all().len(), 2, "nothing wired from inside");
}

/// **Marv's runtime folder is empty in every sandbox, wherever `XDG_RUNTIME_DIR` points.** A sandboxed command that
/// could see a plug's socket could connect to it and act with that plug's identity and wires (a read-only mount
/// doesn't stop `connect()`). The mesh also refuses a runtime dir outside `/run`, `/tmp` and the home folder
/// (`marv_sandbox::hidden_in_sandbox`); this holds even where one is inside the project, which is mounted writable.
///
/// Proven: removed the `--tmpfs` over `SandboxOptions::hidden` in `sandbox_args` → this test failed (the plug listed).
#[test]
fn marvs_runtime_folder_is_empty_in_the_sandbox_even_inside_the_project() {
    marv_guards::require_bwrap!();
    let (project, home) = (marv_guards::TempDir::new("rt-proj"), marv_guards::TempDir::outside_tmp("rt-home"));
    let outside = marv_guards::TempDir::outside_tmp("rt-xdg");
    project.write("xdg/marv/plugs/s/parent.sock", "the parent's plug");
    outside.write("marv/plugs/s/parent.sock", "the parent's plug");
    let sandbox = marv_sandbox::Sandbox {
        hidden: vec![project.path().join("xdg/marv"), outside.path().join("marv")],
        ..marv_guards::sandbox_with_home(home.path())
    };
    let ran = marv_guards::run_sandboxed(
        &sandbox,
        project.path(),
        &marv_sandbox::CommandMounts::default(),
        &format!(
            "ls -A {}/xdg/marv | wc -l; ls -A {}/marv | wc -l; cat {}/xdg/marv/plugs/s/parent.sock {}/marv/plugs/s/parent.sock 2>/dev/null; echo end",
            project.path().display(),
            outside.path().display(),
            project.path().display(),
            outside.path().display()
        ),
        &[],
    );
    let counts: Vec<&str> = ran.output.lines().take(2).map(str::trim).collect();
    assert_eq!(counts, ["0", "0"], "{}", ran.output);
    assert!(!ran.output.contains("the parent's plug"), "{}", ran.output);
    assert!(ran.output.contains("end"), "{}", ran.output);
    // On the host the files are still there: only the sandbox's view is empty.
    assert!(project.path().join("xdg/marv/plugs/s/parent.sock").exists());
}

/// Answers every approval with its decision, keeping what it was asked.
struct Person(marv_tools::Decision, Mutex<Vec<marv_tools::ApprovalRequest>>);
#[async_trait::async_trait]
impl marv_tools::Approver for Person {
    async fn approve(&self, request: marv_tools::ApprovalRequest) -> marv_tools::Decision {
        self.1.lock().unwrap().push(request);
        self.0
    }
}

/// **A new wire is granted only by a person, in one prompt that never offers "always"** (rule 1; #77). A `mesh_send`
/// that needs a new wire is one prompt for the message and the wire. In yolo, with "always" already given for that
/// receptacle's calls and for the wire's scope, the person is still asked, once, yes or no only (`once`, `must_ask`),
/// and only their yes wires it: an "always" answer is one yes and remembered for nothing, a no wires and sends nothing.
/// Without anyone to ask (headless, no `--approve mesh:<receptacle>`) it's refused.
///
/// Proven: set `once` and `must_ask` false in `call_wire_approval` → this test failed (the prompt could offer
/// "always" and be answered from it); made `MeshTool::approve_itself` grant the wire unasked in yolo → failed (nobody
/// was asked, the wire was there).
#[tokio::test]
async fn the_one_prompt_for_a_call_and_its_new_wire_is_a_persons_yes_or_no() {
    use marv_session::mesh::{HeadlessWires, MESH_WIRE_CALL, MeshTool, WireApprover, wire_approver};
    use marv_session::runner::{AlwaysScopes, SessionApprover};
    use marv_tools::{Approval, Decision, ToolRef};
    let runtime = TempDir::new("mesh-77");
    let mesh = Arc::new(Switchboard::new("g", runtime.path().join("plugs"), Arc::new(Log::default())));
    let cancel = CancellationToken::new();
    tokio::spawn(Arc::clone(&mesh).serve(cancel.clone()));
    mesh.plug(parent_id()).unwrap();
    let inbox = ReceptacleName::new("agent.parent.inbox").unwrap();
    let call = marv_core::ToolCall {
        id: "c1".into(),
        name: "mesh_send".into(),
        arguments: r#"{"receptacle": "agent.parent.inbox", "text": "found it"}"#.into(),
    };
    let tools = |plug: &marv_mesh::plug::Plug, wires: Option<Arc<dyn WireApprover>>| -> Vec<ToolRef> {
        MeshTool::all(plug, &mesh, wires).into_iter().map(|t| Arc::new(t) as ToolRef).collect()
    };

    for (answer, wired) in [(Decision::Always, true), (Decision::No, false)] {
        let worker = mesh.plug_next("worker").unwrap();
        let person = Arc::new(Person(answer, Mutex::default()));
        let always = AlwaysScopes::default();
        always.add("mesh:agent.parent.inbox:send");
        always.add(format!("mesh-wire:{}:agent.parent.inbox:send", worker.id));
        let mut ctx = marv_tools::ToolContext::new(std::env::temp_dir());
        ctx.yolo = true;
        ctx.approver = Some(Arc::new(SessionApprover { inner: person.clone(), always, cancel: CancellationToken::new() }));
        let wires = wire_approver(None, Some(&(person.clone() as Arc<dyn marv_tools::Approver>)));
        let out = marv_tools::run_tool(&call, &ctx, &tools(&worker, wires)).await;

        let asked = person.1.lock().unwrap().clone();
        assert_eq!(asked.len(), 1, "one prompt, asked of the person: {asked:?}");
        assert_eq!(asked[0].tool, MESH_WIRE_CALL);
        assert!(asked[0].once && asked[0].must_ask, "yes or no, and a person answers");
        assert_eq!(asked[0].preview.text.as_deref(), Some("found it"), "the payload is on it");
        assert_eq!(mesh.table().allows(&worker.id, &inbox, WireMode::Send), wired, "{answer:?}: {}", out.result.output);
        assert_eq!(mesh.drain_inbox(&parent_id()).len(), usize::from(wired));
        if wired {
            let by = mesh.table().all().iter().find(|w| w.from == worker.id).map(|w| w.grant.local);
            assert_eq!(by, Some(Approver::Tui));
            assert_eq!(out.result.approval, Some(Approval::Decided(Decision::Yes)), "always counts as one yes");
        } else {
            assert!(out.result.declined, "{}", out.result.output);
        }
    }

    // Headless without `--approve mesh:<receptacle>`: nobody to ask, refused, nothing wired.
    let worker = mesh.plug_next("worker").unwrap();
    let ctx = marv_tools::ToolContext::new(std::env::temp_dir());
    let out = marv_tools::run_tool(&call, &ctx, &tools(&worker, Some(Arc::new(HeadlessWires(Vec::new()))))).await;
    assert!(out.result.is_error, "{}", out.result.output);
    assert!(!mesh.table().allows(&worker.id, &inbox, WireMode::Send));
    assert!(mesh.drain_inbox(&parent_id()).is_empty());
    cancel.cancel();
}
