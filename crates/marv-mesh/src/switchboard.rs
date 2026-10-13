//! The switchboard: one per Marv process, on the host, outside every sandbox.
//! New in marv-rs (docs/MESH.md).
//!
//! It owns the wiring table, hands out plugs, and routes every message. It is
//! the only part of the mesh that touches other processes or the network.
//! Every message goes through [`Switchboard::admit`], which applies the rules
//! in order and logs the outcome to the trajectory either way:
//!
//! 1. the receptacle exists and supports the mode;
//! 2. a wire allows it ([`WireTable::allows`]: default deny, by name only);
//! 3. the message fits the wire's size limit (refused, never cut);
//! 4. the wire's rate limit has a token.
//!
//! The sender is the plug whose socket the message arrived on: identity comes
//! from the connection, never from the message.
//!
//! Serving: an MCP server (`marv_mcp::jsonrpc`) on each plug's socket. Each
//! wired receptacle is a tool in `tools/list`; `tools/call` routes; `watch`
//! wires get notifications. v2 also listens on
//! `$XDG_RUNTIME_DIR/marv/mesh/<session>.sock` ([`crate::discovery`]).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use marv_mcp::jsonrpc::{Request, Response};
use serde::Serialize;
use serde_json::Value;

use crate::MeshError;
use crate::limits::RateLimiter;
use crate::message::Envelope;
use crate::plug::{Plug, PlugId, Reachable};
use crate::receptacle::{Receptacle, ReceptacleName};
use crate::wire::{Wire, WireMode, WireTable};

/// What happened to one message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    Delivered,
    Refused { reason: String },
}

/// One trajectory record (`type: "mesh"`): sender, receptacle, size, never the body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MeshRecord {
    pub at_ms: u64,
    pub id: Option<String>,
    pub sender: String,
    pub receptacle: String,
    pub mode: WireMode,
    pub size: usize,
    #[serde(flatten)]
    pub outcome: Outcome,
}

/// Where the switchboard logs every message (`marv-session`'s trajectory writer).
pub trait TrajectorySink: Send + Sync {
    fn mesh(&self, record: MeshRecord);
}

/// A sink that keeps nothing (tests, and runs without trajectories).
pub struct NoTrajectory;
impl TrajectorySink for NoTrajectory {
    fn mesh(&self, _record: MeshRecord) {}
}

pub struct Switchboard {
    /// This Marv's session id: message ids and the v2 discovery socket name.
    pub session: String,
    table: WireTable,
    receptacles: HashMap<ReceptacleName, Receptacle>,
    plugs: HashMap<PlugId, Plug>,
    limiters: HashMap<(PlugId, ReceptacleName, WireMode), RateLimiter>,
    trajectory: Arc<dyn TrajectorySink>,
    seq: u64,
    /// The private 0700 folder holding plug sockets.
    runtime_dir: PathBuf,
}

impl Switchboard {
    pub fn new(session: impl Into<String>, runtime_dir: PathBuf, trajectory: Arc<dyn TrajectorySink>) -> Self {
        Self {
            session: session.into(),
            table: WireTable::new(),
            receptacles: HashMap::new(),
            plugs: HashMap::new(),
            limiters: HashMap::new(),
            trajectory,
            seq: 0,
            runtime_dir,
        }
    }

    pub fn table(&self) -> &WireTable {
        &self.table
    }

    /// Offers a receptacle (an agent's inbox, a Herdr pane, a remote service).
    pub fn offer(&mut self, receptacle: Receptacle) {
        self.receptacles.insert(receptacle.name.clone(), receptacle);
    }

    pub fn receptacle(&self, name: &ReceptacleName) -> Option<&Receptacle> {
        self.receptacles.get(name)
    }

    /// A plug for a new agent and its inbox; its socket lives in the runtime folder.
    /// The agent starts with no wires.
    pub fn plug(&mut self, id: PlugId) -> Plug {
        let plug = Plug { host_socket: self.runtime_dir.join(format!("{}.sock", id.as_str())), id: id.clone() };
        self.offer(Receptacle::inbox(id.clone()));
        self.plugs.insert(id, plug.clone());
        plug
    }

    /// Adds a wire the user approved (see [`crate::approval`]).
    pub fn wire(&mut self, wire: Wire) -> crate::Result<()> {
        let owner = self.receptacles.get(&wire.to).and_then(|r| r.owner.clone());
        self.table.add(wire, owner.as_ref())
    }

    pub fn unwire(&mut self, from: &PlugId, to: &ReceptacleName, mode: WireMode) -> bool {
        self.limiters.remove(&(from.clone(), to.clone(), mode));
        self.table.remove(from, to, mode)
    }

    /// An agent ended: its plug, inbox and wires go.
    pub fn unplug(&mut self, id: &PlugId) {
        self.table.unplug(id);
        self.plugs.remove(id);
        self.receptacles.remove(&ReceptacleName::inbox(id));
        self.limiters.retain(|(from, to, _), _| from != id && to.inbox_owner() != Some(id.as_str()));
    }

    /// What `mesh_list` shows a plug: only what it's wired to.
    pub fn reachable(&self, from: &PlugId) -> Vec<Reachable> {
        let mut out: Vec<Reachable> = Vec::new();
        for w in self.table.from_plug(from) {
            let Some(r) = self.receptacles.get(&w.to) else { continue };
            match out.iter_mut().find(|x| x.receptacle == w.to) {
                Some(x) => x.modes.push(w.mode),
                None => out.push(Reachable { receptacle: w.to.clone(), modes: vec![w.mode], description: r.description.clone() }),
            }
        }
        out
    }

    /// The policy check every message goes through (module docs), logged either way.
    pub fn admit(
        &mut self,
        from: &PlugId,
        receptacle: &ReceptacleName,
        mode: WireMode,
        body: Value,
        now_ms: u64,
    ) -> crate::Result<Envelope> {
        self.seq += 1;
        let envelope =
            Envelope::new(format!("{}:{}", self.session, self.seq), from.clone(), receptacle.clone(), mode, body, now_ms);
        let result = self.check(&envelope);
        self.trajectory.mesh(MeshRecord {
            at_ms: now_ms,
            id: result.is_ok().then(|| envelope.id.clone()),
            sender: from.to_string(),
            receptacle: receptacle.to_string(),
            mode,
            size: envelope.size,
            outcome: match &result {
                Ok(()) => Outcome::Delivered,
                Err(e) => Outcome::Refused { reason: e.to_string() },
            },
        });
        result.map(|()| envelope)
    }

    fn check(&mut self, e: &Envelope) -> crate::Result<()> {
        let r = self.receptacles.get(&e.receptacle).ok_or_else(|| MeshError::NoReceptacle(e.receptacle.to_string()))?;
        let not_wired = || MeshError::NotWired { from: e.sender.to_string(), receptacle: e.receptacle.to_string(), mode: e.mode };
        if !r.supports(e.mode) {
            return Err(not_wired());
        }
        let wire = self.table.check(&e.sender, &e.receptacle, e.mode)?;
        let limits = wire.limits;
        limits.check_size(e.receptacle.as_str(), e.size)?;
        self.limiters
            .entry((e.sender.clone(), e.receptacle.clone(), e.mode))
            .or_insert_with(|| RateLimiter::new(limits, e.at_ms))
            .take(e.receptacle.as_str(), e.at_ms)
    }

    /// Delivers an admitted message: into an inbox, to a peer switchboard, a
    /// Herdr pane or the AgentMesh bridge. `call` waits for the response.
    pub async fn route(&mut self, _envelope: Envelope) -> crate::Result<Option<Value>> {
        todo!("port: mesh v1: route by ReceptacleKind (Inbox queue with DEFAULT_INBOX_DEPTH; Peer/HerdrPane v2; Remote v3)")
    }

    /// One MCP request from `from`'s socket: `initialize`, `tools/list`
    /// (= [`Self::reachable`]), `tools/call` (= admit + route).
    pub async fn handle(&mut self, _from: &PlugId, _request: Request) -> Response {
        todo!("port: mesh v1: MCP server dispatch on the plug socket")
    }

    /// Listens on every plug's socket (0600, in the 0700 runtime folder) until cancelled.
    pub async fn serve(&mut self, _cancel: tokio_util::sync::CancellationToken) -> crate::Result<()> {
        todo!("port: mesh v1: tokio UnixListener per plug, newline-delimited JSON-RPC, identity = which socket")
    }

    /// The inbox's unread messages for an agent's next turn (oldest first).
    pub fn drain_inbox(&mut self, _agent: &PlugId) -> Vec<Envelope> {
        todo!("port: mesh v1: inbox queue")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::WireLimits;
    use crate::wire::{Approver, Grant};
    use std::sync::Mutex;

    #[derive(Default)]
    struct Log(Mutex<Vec<MeshRecord>>);
    impl TrajectorySink for Log {
        fn mesh(&self, record: MeshRecord) {
            self.0.lock().unwrap().push(record);
        }
    }

    fn id(s: &str) -> PlugId {
        PlugId::new(s).unwrap()
    }

    fn board() -> (Switchboard, Arc<Log>) {
        let log = Arc::new(Log::default());
        let mut sb = Switchboard::new("s1", "/tmp/marv-mesh-test".into(), log.clone());
        sb.plug(id("parent"));
        sb.plug(id("worker-1"));
        (sb, log)
    }

    #[test]
    fn unwired_messages_are_refused_and_logged() {
        let (mut sb, log) = board();
        let inbox = ReceptacleName::inbox(&id("parent"));
        let err = sb.admit(&id("worker-1"), &inbox, WireMode::Send, Value::from("hi"), 0).unwrap_err();
        assert!(matches!(err, MeshError::NotWired { .. }));
        let records = log.0.lock().unwrap();
        assert_eq!(records.len(), 1);
        assert!(matches!(records[0].outcome, Outcome::Refused { .. }));
        assert_eq!(records[0].size, 4);
        assert_eq!(records[0].id, None);
    }

    #[test]
    fn a_wired_message_is_admitted_as_mesh_text() {
        let (mut sb, log) = board();
        let inbox = ReceptacleName::inbox(&id("parent"));
        sb.wire(Wire::new(id("worker-1"), inbox.clone(), WireMode::Send, Grant::local(Approver::Tui))).unwrap();
        let e = sb.admit(&id("worker-1"), &inbox, WireMode::Send, Value::from("done"), 5).unwrap();
        assert_eq!(e.provenance, marv_core::Provenance::Mesh);
        assert_eq!(e.id, "s1:1");
        assert_eq!(log.0.lock().unwrap()[0].outcome, Outcome::Delivered);
        // An inbox takes no calls, wired or not.
        assert!(sb.admit(&id("worker-1"), &inbox, WireMode::Call, Value::Null, 5).is_err());
        assert_eq!(sb.reachable(&id("worker-1")).len(), 1);
        assert!(sb.reachable(&id("parent")).is_empty());
    }

    #[test]
    fn agents_cant_wire_their_own_inbox() {
        let (mut sb, _) = board();
        let own = ReceptacleName::inbox(&id("worker-1"));
        assert!(matches!(
            sb.wire(Wire::new(id("worker-1"), own, WireMode::Send, Grant::local(Approver::Cli))),
            Err(MeshError::SelfWire(_))
        ));
    }

    #[test]
    fn per_wire_limits_apply() {
        let (mut sb, _) = board();
        let inbox = ReceptacleName::inbox(&id("parent"));
        let mut w = Wire::new(id("worker-1"), inbox.clone(), WireMode::Send, Grant::local(Approver::Tui));
        w.limits = WireLimits { max_message_bytes: 8, per_minute: 1, burst: 1 };
        sb.wire(w).unwrap();
        assert!(matches!(
            sb.admit(&id("worker-1"), &inbox, WireMode::Send, Value::from("x".repeat(20)), 0),
            Err(MeshError::TooLarge { .. })
        ));
        sb.admit(&id("worker-1"), &inbox, WireMode::Send, Value::from("a"), 0).unwrap();
        assert!(matches!(
            sb.admit(&id("worker-1"), &inbox, WireMode::Send, Value::from("b"), 1),
            Err(MeshError::RateLimited { .. })
        ));
    }

    #[test]
    fn unplugging_removes_wires_both_ways() {
        let (mut sb, _) = board();
        let to_parent = ReceptacleName::inbox(&id("parent"));
        let to_worker = ReceptacleName::inbox(&id("worker-1"));
        sb.wire(Wire::new(id("worker-1"), to_parent, WireMode::Send, Grant::local(Approver::Tui))).unwrap();
        sb.wire(Wire::new(id("parent"), to_worker.clone(), WireMode::Send, Grant::local(Approver::Tui))).unwrap();
        sb.unplug(&id("worker-1"));
        assert!(sb.table().all().is_empty());
        assert!(sb.receptacle(&to_worker).is_none());
    }
}
