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
