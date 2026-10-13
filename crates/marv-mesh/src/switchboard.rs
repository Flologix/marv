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
//! wires get notifications (routing them comes last). v2 also listens on
//! `$XDG_RUNTIME_DIR/marv/mesh/<session>.sock` ([`crate::discovery`]).
//!
//! Inboxes (v1): a queue per agent, at most `DEFAULT_INBOX_DEPTH` unread (the
//! oldest are kept, new ones refused, so a looping sender can't push out what
//! the reader hasn't seen). The agent's next step reads them with
//! [`Switchboard::drain_inbox`]; nothing is pushed into a running reply.
//!
//! Shared, not locked by its users: the switchboard lives in an `Arc` and every method takes `&self`; `serve` and
//! `route` take `Arc<Self>` because they outlive the call (the socket loop, a `call` waiting for its answer). Its
//! tables sit behind one short synchronous lock, never held across an await, so serving never blocks wiring,
//! `mesh_list` or the next message.

use std::collections::{HashMap, VecDeque};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use marv_mcp::jsonrpc::{Message, Request, Response, code, decode_line, encode_line, method};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::sync::CancellationToken;

use crate::MeshError;
use crate::limits::{DEFAULT_INBOX_DEPTH, RateLimiter};
use crate::message::Envelope;
use crate::plug::{MODE_META, MODES_META, Plug, PlugId, Reachable};
use crate::receptacle::{Receptacle, ReceptacleKind, ReceptacleName};
use crate::wire::{Wire, WireMode, WireTable};

/// One JSON-RPC line, at most. Far above any wire's message limit; a client that sends more without a newline is
/// cut off rather than buffered without end.
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

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
    board: Mutex<Board>,
    /// Swapped by the session when its conversation (and so its trajectory file) changes.
    trajectory: Mutex<Arc<dyn TrajectorySink>>,
    /// The private 0700 folder holding plug sockets.
    runtime_dir: PathBuf,
    /// A plug was added while `serve` runs: it has a new socket to listen on.
    wake: tokio::sync::Notify,
}

/// What changes: one short lock, never held across an await.
#[derive(Default)]
struct Board {
    table: WireTable,
    receptacles: HashMap<ReceptacleName, Receptacle>,
    plugs: HashMap<PlugId, Plug>,
    limiters: HashMap<(PlugId, ReceptacleName, WireMode), RateLimiter>,
    seq: u64,
    /// Unread messages per agent, oldest first.
    inboxes: HashMap<PlugId, VecDeque<Envelope>>,
    /// Sockets bound by `plug` that `serve` hasn't picked up yet.
    unserved: Vec<(PlugId, StdUnixListener)>,
    /// Stops a plug's listener and its connections when it's unplugged.
    stops: HashMap<PlugId, CancellationToken>,
    /// The last `worker-N` handed out ([`Switchboard::plug_next`]); never reused within a process.
    workers: u64,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn io(context: &str, err: std::io::Error) -> MeshError {
    MeshError::Io(format!("{context}: {err}"))
}

impl Switchboard {
    pub fn new(session: impl Into<String>, runtime_dir: PathBuf, trajectory: Arc<dyn TrajectorySink>) -> Self {
        Self {
            session: session.into(),
            board: Mutex::default(),
            trajectory: Mutex::new(trajectory),
            runtime_dir,
            wake: tokio::sync::Notify::new(),
        }
    }

    fn board(&self) -> MutexGuard<'_, Board> {
        self.board.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn log(&self, record: MeshRecord) {
        let sink = self.trajectory.lock().unwrap_or_else(|e| e.into_inner()).clone();
        sink.mesh(record);
    }

    /// Where every message is logged from now on (the session's current trajectory).
    pub fn set_trajectory(&self, trajectory: Arc<dyn TrajectorySink>) {
        *self.trajectory.lock().unwrap_or_else(|e| e.into_inner()) = trajectory;
    }

    /// A snapshot of the wiring table.
    pub fn table(&self) -> WireTable {
        self.board().table.clone()
    }

    /// Offers a receptacle (an agent's inbox, a Herdr pane, a remote service).
    pub fn offer(&self, receptacle: Receptacle) {
        self.board().receptacles.insert(receptacle.name.clone(), receptacle);
    }

    pub fn receptacle(&self, name: &ReceptacleName) -> Option<Receptacle> {
        self.board().receptacles.get(name).cloned()
    }

    /// The private runtime folder, made 0700 (it holds every plug's socket).
    fn runtime_folder(&self) -> crate::Result<()> {
        let dir = &self.runtime_dir;
        std::fs::create_dir_all(dir).map_err(|e| io(&format!("can't make {}", dir.display()), e))?;
        let meta = std::fs::symlink_metadata(dir).map_err(|e| io(&dir.display().to_string(), e))?;
        if !meta.is_dir() {
            return Err(MeshError::Io(format!("{} isn't a folder", dir.display())));
        }
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| io(&dir.display().to_string(), e))
    }

    /// A plug for a new agent and its inbox; its socket (0600) is bound at once, so a sandbox can mount it before
    /// `serve` picks it up. The agent starts with no wires. Plugging an agent that has a plug returns that one.
    pub fn plug(&self, id: PlugId) -> crate::Result<Plug> {
        if let Some(plug) = self.board().plugs.get(&id) {
            return Ok(plug.clone());
        }
        self.runtime_folder()?;
        let host_socket = self.runtime_dir.join(format!("{}.sock", id.as_str()));
        // A socket left by an earlier plug of this name (the folder is this session's own): bind would fail on it.
        let _ = std::fs::remove_file(&host_socket);
        let listener =
            StdUnixListener::bind(&host_socket).map_err(|e| io(&format!("can't listen on {}", host_socket.display()), e))?;
        std::fs::set_permissions(&host_socket, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| io(&host_socket.display().to_string(), e))?;
        let plug = Plug { host_socket, id: id.clone() };
        {
            let mut board = self.board();
            let inbox = Receptacle::inbox(id.clone());
            board.receptacles.insert(inbox.name.clone(), inbox);
            board.plugs.insert(id.clone(), plug.clone());
            board.inboxes.entry(id.clone()).or_default();
            board.stops.insert(id.clone(), CancellationToken::new());
            board.unserved.push((id, listener));
        }
        self.wake.notify_one();
        Ok(plug)
    }

    /// A plug named `<prefix>-<n>` with the next unused n (`worker-1`, `worker-2`, …): a subagent's.
    pub fn plug_next(&self, prefix: &str) -> crate::Result<Plug> {
        let id = {
            let mut board = self.board();
            loop {
                board.workers += 1;
                let id = PlugId::new(format!("{prefix}-{}", board.workers))?;
                if !board.plugs.contains_key(&id) {
                    break id;
                }
            }
        };
        self.plug(id)
    }

    /// Adds a wire the user approved (see [`crate::approval`]).
    pub fn wire(&self, wire: Wire) -> crate::Result<()> {
        let mut board = self.board();
        let owner = board.receptacles.get(&wire.to).and_then(|r| r.owner.clone());
        board.table.add(wire, owner.as_ref())
    }

    pub fn unwire(&self, from: &PlugId, to: &ReceptacleName, mode: WireMode) -> bool {
        let mut board = self.board();
        board.limiters.remove(&(from.clone(), to.clone(), mode));
        board.table.remove(from, to, mode)
    }

    /// An agent ended: its plug, inbox, wires, socket and connections go.
    pub fn unplug(&self, id: &PlugId) {
        let plug = {
            let mut board = self.board();
            board.table.unplug(id);
            board.receptacles.remove(&ReceptacleName::inbox(id));
            board.limiters.retain(|(from, to, _), _| from != id && to.inbox_owner() != Some(id.as_str()));
            board.inboxes.remove(id);
            board.unserved.retain(|(p, _)| p != id);
            if let Some(stop) = board.stops.remove(id) {
                stop.cancel();
            }
            board.plugs.remove(id)
        };
        if let Some(plug) = plug {
            let _ = std::fs::remove_file(&plug.host_socket);
        }
    }

    /// What `mesh_list` shows a plug: only what it's wired to.
    pub fn reachable(&self, from: &PlugId) -> Vec<Reachable> {
        let board = self.board();
        let mut out: Vec<Reachable> = Vec::new();
        for w in board.table.from_plug(from) {
            let Some(r) = board.receptacles.get(&w.to) else { continue };
            match out.iter_mut().find(|x| x.receptacle == w.to) {
                Some(x) => x.modes.push(w.mode),
                None => out.push(Reachable { receptacle: w.to.clone(), modes: vec![w.mode], description: r.description.clone() }),
            }
        }
        out
    }

    /// The policy check every message goes through (module docs), logged either way.
    pub fn admit(
        &self,
        from: &PlugId,
        receptacle: &ReceptacleName,
        mode: WireMode,
        body: Value,
        now_ms: u64,
    ) -> crate::Result<Envelope> {
        let (envelope, result) = {
            let mut board = self.board();
            board.seq += 1;
            let id = format!("{}:{}", self.session, board.seq);
            let envelope = Envelope::new(id, from.clone(), receptacle.clone(), mode, body, now_ms);
            let result = board.check(&envelope);
            (envelope, result)
        };
        // Logged outside the lock: the sink is the session's trajectory writer.
        self.log(MeshRecord {
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

    /// Delivers an admitted message: into an inbox (v1); to a peer switchboard, a Herdr pane or the AgentMesh bridge
    /// later (v2/v3). `call` waits for the response. A message that can't be delivered after all (its inbox filled
    /// since `admit`, its agent ended) is logged again, refused, under the same id.
    pub async fn route(self: Arc<Self>, envelope: Envelope) -> crate::Result<Option<Value>> {
        let result = {
            let mut board = self.board();
            let kind = board.receptacles.get(&envelope.receptacle).map(|r| (r.kind.clone(), r.owner.clone()));
            match kind {
                None => Err(MeshError::NoReceptacle(envelope.receptacle.to_string())),
                Some((ReceptacleKind::Inbox, Some(owner))) if envelope.mode == WireMode::Send => {
                    match board.inboxes.get_mut(&owner) {
                        Some(queue) if queue.len() < DEFAULT_INBOX_DEPTH => {
                            queue.push_back(envelope.clone());
                            Ok(None)
                        }
                        Some(_) => Err(MeshError::InboxFull(envelope.receptacle.to_string())),
                        None => Err(MeshError::NoReceptacle(envelope.receptacle.to_string())),
                    }
                }
                // `watch` routing, Herdr panes, peers and remote services: later milestones.
                Some(_) => Err(MeshError::NotServed { receptacle: envelope.receptacle.to_string(), mode: envelope.mode }),
            }
        };
        if let Err(err) = &result {
            self.log(MeshRecord {
                at_ms: envelope.at_ms,
                id: Some(envelope.id.clone()),
                sender: envelope.sender.to_string(),
                receptacle: envelope.receptacle.to_string(),
                mode: envelope.mode,
                size: envelope.size,
                outcome: Outcome::Refused { reason: err.to_string() },
            });
        }
        result
    }

    /// One message from `from`: [`Self::admit`], then [`Self::route`]. What `tools/call` runs.
    pub async fn deliver(
        self: &Arc<Self>,
        from: &PlugId,
        receptacle: &str,
        mode: WireMode,
        body: Value,
    ) -> crate::Result<Option<Value>> {
        let receptacle = ReceptacleName::new(receptacle)?;
        let envelope = self.admit(from, &receptacle, mode, body, now_ms())?;
        Arc::clone(self).route(envelope).await
    }

    /// One MCP request from `from`'s socket: `initialize`, `ping`, `tools/list` (= [`Self::reachable`]),
    /// `tools/call` (= [`Self::deliver`]). The mode rides in `_meta` ([`MODE_META`], default `call`). A refusal is
    /// a tool result with `isError`, so the agent reads it; the request's own fields are never echoed back.
    pub async fn handle(self: &Arc<Self>, from: &PlugId, request: Request) -> Response {
        let id = request.id.clone();
        match request.method.as_str() {
            method::INITIALIZE => Response::ok(
                id,
                json!({
                    "protocolVersion": marv_mcp::jsonrpc::PROTOCOL_VERSION,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "marv-switchboard", "version": env!("CARGO_PKG_VERSION")},
                }),
            ),
            method::PING => Response::ok(id, json!({})),
            method::TOOLS_LIST => {
                let tools: Vec<Value> = self
                    .reachable(from)
                    .into_iter()
                    .map(|r| {
                        let schema = self.receptacle(&r.receptacle).map_or_else(|| json!({"type": "object"}), |x| x.input_schema);
                        json!({
                            "name": r.receptacle,
                            "description": r.description,
                            "inputSchema": schema,
                            "_meta": {MODES_META: r.modes},
                        })
                    })
                    .collect();
                Response::ok(id, json!({"tools": tools}))
            }
            method::TOOLS_CALL => {
                let params = request.params.unwrap_or(Value::Null);
                let Some(name) = params.get("name").and_then(Value::as_str) else {
                    return Response::err(id, code::INVALID_PARAMS, "tools/call needs a name");
                };
                let mode = match params.get("_meta").and_then(|m| m.get(MODE_META)) {
                    None => Ok(WireMode::Call),
                    Some(m) => serde_json::from_value::<WireMode>(m.clone()).map_err(|_| ()),
                };
                let Ok(mode) = mode else {
                    return Response::err(id, code::INVALID_PARAMS, "the mode is call, send or watch");
                };
                let body = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
                let (text, is_error) = match self.deliver(from, name, mode, body).await {
                    Ok(Some(Value::String(answer))) => (answer, false),
                    Ok(Some(answer)) => (answer.to_string(), false),
                    Ok(None) => (format!("Delivered to {name}."), false),
                    Err(err) => (err.to_string(), true),
                };
                Response::ok(id, json!({"content": [{"type": "text", "text": text}], "isError": is_error}))
            }
            _ => Response::err(id, code::METHOD_NOT_FOUND, "the switchboard doesn't know that method"),
        }
    }

    /// Listens on every plug's socket (0600, in the 0700 runtime folder) until cancelled, including plugs added
    /// later. Each connection's task holds its own clone of the `Arc`; who sent a message is the plug whose socket it
    /// arrived on.
    pub async fn serve(self: Arc<Self>, cancel: CancellationToken) -> crate::Result<()> {
        loop {
            let unserved = std::mem::take(&mut self.board().unserved);
            for (id, listener) in unserved {
                let stop = self.board().stops.get(&id).cloned().unwrap_or_default();
                listener.set_nonblocking(true).map_err(|e| io("listener", e))?;
                let listener = UnixListener::from_std(listener).map_err(|e| io("listener", e))?;
                tokio::spawn(Arc::clone(&self).accept(id, listener, stop, cancel.clone()));
            }
            tokio::select! {
                _ = self.wake.notified() => {}
                _ = cancel.cancelled() => return Ok(()),
            }
        }
    }

    async fn accept(self: Arc<Self>, id: PlugId, listener: UnixListener, stop: CancellationToken, cancel: CancellationToken) {
        loop {
            tokio::select! {
                _ = stop.cancelled() => return,
                _ = cancel.cancelled() => return,
                conn = listener.accept() => match conn {
                    Ok((stream, _)) => {
                        tokio::spawn(Arc::clone(&self).connection(id.clone(), stream, stop.clone(), cancel.clone()));
                    }
                    // Out of file descriptors and the like: try again shortly rather than spin.
                    Err(_) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
                },
            }
        }
    }

    /// One client on a plug's socket: newline-delimited JSON-RPC, one request at a time.
    async fn connection(self: Arc<Self>, id: PlugId, stream: UnixStream, stop: CancellationToken, cancel: CancellationToken) {
        let (read, mut write) = stream.into_split();
        let mut read = BufReader::new(read);
        loop {
            let mut line = Vec::new();
            let mut limited = (&mut read).take(MAX_LINE_BYTES as u64 + 1);
            let n = tokio::select! {
                _ = stop.cancelled() => return,
                _ = cancel.cancelled() => return,
                n = limited.read_until(b'\n', &mut line) => n,
            };
            match n {
                Ok(0) | Err(_) => return,
                Ok(_) if line.len() > MAX_LINE_BYTES => return,
                Ok(_) => {}
            }
            let Ok(text) = std::str::from_utf8(&line) else { return };
            // Notifications (`initialized`) and anything unparsable get no answer.
            let Ok(Message::Request(request)) = decode_line(text) else { continue };
            let response = self.handle(&id, request).await;
            if write.write_all(encode_line(&Message::Response(response)).as_bytes()).await.is_err() {
                return;
            }
        }
    }

    /// The inbox's unread messages for an agent's next turn (oldest first).
    pub fn drain_inbox(&self, agent: &PlugId) -> Vec<Envelope> {
        self.board().inboxes.get_mut(agent).map(|q| q.drain(..).collect()).unwrap_or_default()
    }
}

impl Drop for Switchboard {
    fn drop(&mut self) {
        for plug in self.board().plugs.values() {
            let _ = std::fs::remove_file(&plug.host_socket);
        }
    }
}

impl Board {
    fn check(&mut self, e: &Envelope) -> crate::Result<()> {
        let r = self.receptacles.get(&e.receptacle).ok_or_else(|| MeshError::NoReceptacle(e.receptacle.to_string()))?;
        let not_wired = || MeshError::NotWired { from: e.sender.to_string(), receptacle: e.receptacle.to_string(), mode: e.mode };
        if !r.supports(e.mode) {
            return Err(not_wired());
        }
        let full = r.kind == ReceptacleKind::Inbox
            && r.owner.as_ref().and_then(|o| self.inboxes.get(o)).is_some_and(|q| q.len() >= DEFAULT_INBOX_DEPTH);
        let wire = self.table.check(&e.sender, &e.receptacle, e.mode)?;
        let limits = wire.limits;
        limits.check_size(e.receptacle.as_str(), e.size)?;
        if full {
            return Err(MeshError::InboxFull(e.receptacle.to_string()));
        }
        self.limiters
            .entry((e.sender.clone(), e.receptacle.clone(), e.mode))
            .or_insert_with(|| RateLimiter::new(limits, e.at_ms))
            .take(e.receptacle.as_str(), e.at_ms)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::limits::WireLimits;
    use crate::plug::PlugClient;
    use crate::wire::{Approver, Grant};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    pub(crate) struct Log(pub Mutex<Vec<MeshRecord>>);
    impl TrajectorySink for Log {
        fn mesh(&self, record: MeshRecord) {
            self.0.lock().unwrap().push(record);
        }
    }

    /// A private runtime folder, removed on drop. Short, so socket paths stay under the 108-byte limit.
    pub(crate) struct Runtime(pub PathBuf);
    impl Runtime {
        pub(crate) fn new() -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            Self(std::env::temp_dir().join(format!("mm-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst))))
        }
    }
    impl Drop for Runtime {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn id(s: &str) -> PlugId {
        PlugId::new(s).unwrap()
    }

    fn board() -> (Switchboard, Arc<Log>, Runtime) {
        let log = Arc::new(Log::default());
        let runtime = Runtime::new();
        let sb = Switchboard::new("s1", runtime.0.clone(), log.clone());
        sb.plug(id("parent")).unwrap();
        sb.plug(id("worker-1")).unwrap();
        (sb, log, runtime)
    }

    fn tui(from: &str, to: &ReceptacleName) -> Wire {
        Wire::new(id(from), to.clone(), WireMode::Send, Grant::local(Approver::Tui))
    }

    #[test]
    fn shared_without_a_lock_around_it() {
        fn shared<T: Send + Sync>(_: &T) {}
        let (sb, _, _rt) = board();
        let sb = Arc::new(sb);
        shared(&sb);
        let inbox = ReceptacleName::inbox(&id("parent"));
        let other = sb.clone();
        std::thread::spawn(move || {
            other.wire(Wire::new(id("worker-1"), inbox, WireMode::Send, Grant::local(Approver::Tui))).unwrap();
        })
        .join()
        .unwrap();
        assert_eq!(sb.reachable(&id("worker-1")).len(), 1);
    }

    #[test]
    fn unwired_messages_are_refused_and_logged() {
        let (sb, log, _rt) = board();
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
        let (sb, log, _rt) = board();
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
        let (sb, _, _rt) = board();
        let own = ReceptacleName::inbox(&id("worker-1"));
        assert!(matches!(
            sb.wire(Wire::new(id("worker-1"), own, WireMode::Send, Grant::local(Approver::Cli))),
            Err(MeshError::SelfWire(_))
        ));
    }

    #[test]
    fn per_wire_limits_apply() {
        let (sb, _, _rt) = board();
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
    fn unplugging_removes_wires_both_ways_and_the_socket() {
        let (sb, _, _rt) = board();
        let to_parent = ReceptacleName::inbox(&id("parent"));
        let to_worker = ReceptacleName::inbox(&id("worker-1"));
        sb.wire(tui("worker-1", &to_parent)).unwrap();
        sb.wire(tui("parent", &to_worker)).unwrap();
        let socket = sb.plug(id("worker-1")).unwrap().host_socket;
        assert!(socket.exists());
        sb.unplug(&id("worker-1"));
        assert!(sb.table().all().is_empty());
        assert!(sb.receptacle(&to_worker).is_none());
        assert!(!socket.exists());
    }

    #[test]
    fn plugs_get_private_sockets_and_workers_are_numbered() {
        use std::os::unix::fs::PermissionsExt;
        let (sb, _, rt) = board();
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&rt.0), 0o700);
        let w = sb.plug_next("worker").unwrap();
        // worker-1 is taken already: the next free number.
        assert_eq!(w.id.as_str(), "worker-2");
        assert_eq!(mode(&w.host_socket), 0o600);
        assert_eq!(sb.plug_next("worker").unwrap().id.as_str(), "worker-3");
        // Plugging again hands back the same plug.
        assert_eq!(sb.plug(id("worker-2")).unwrap(), w);
    }

    #[tokio::test]
    async fn routed_sends_wait_in_the_inbox_until_drained() {
        let (sb, log, _rt) = board();
        let sb = Arc::new(sb);
        let inbox = ReceptacleName::inbox(&id("parent"));
        sb.wire(tui("worker-1", &inbox)).unwrap();
        for text in ["one", "two"] {
            sb.deliver(&id("worker-1"), inbox.as_str(), WireMode::Send, json!({"text": text})).await.unwrap();
        }
        let got = sb.drain_inbox(&id("parent"));
        assert_eq!(got.iter().map(Envelope::text).collect::<Vec<_>>(), ["one", "two"]);
        assert!(got.iter().all(|e| e.sender == id("worker-1")));
        assert!(sb.drain_inbox(&id("parent")).is_empty());
        assert!(sb.drain_inbox(&id("worker-1")).is_empty());
        assert_eq!(log.0.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_full_inbox_keeps_the_oldest_and_refuses_new_ones() {
        let (sb, log, _rt) = board();
        let sb = Arc::new(sb);
        let inbox = ReceptacleName::inbox(&id("parent"));
        let mut w = tui("worker-1", &inbox);
        w.limits.burst = 1000;
        sb.wire(w).unwrap();
        for i in 0..DEFAULT_INBOX_DEPTH {
            sb.deliver(&id("worker-1"), inbox.as_str(), WireMode::Send, Value::from(i)).await.unwrap();
        }
        let err = sb.deliver(&id("worker-1"), inbox.as_str(), WireMode::Send, Value::from("late")).await.unwrap_err();
        assert!(matches!(err, MeshError::InboxFull(_)));
        assert!(matches!(log.0.lock().unwrap().last().unwrap().outcome, Outcome::Refused { .. }));
        let kept = sb.drain_inbox(&id("parent"));
        assert_eq!(kept.len(), DEFAULT_INBOX_DEPTH);
        assert_eq!(kept[0].text(), "0");
    }

    #[tokio::test]
    async fn over_the_socket_identity_is_the_socket_and_only_wires_count() {
        let (sb, log, _rt) = board();
        let sb = Arc::new(sb);
        let cancel = CancellationToken::new();
        tokio::spawn(Arc::clone(&sb).serve(cancel.clone()));
        let to_parent = ReceptacleName::inbox(&id("parent"));
        let worker = sb.plug(id("worker-1")).unwrap();
        let mut client = PlugClient::connect(&worker.host_socket).await.unwrap();
        // Nothing wired: nothing listed, and a send is refused (and logged).
        assert!(client.list().await.unwrap().is_empty());
        let err = client.send(&to_parent, "hi").await.unwrap_err();
        assert!(err.to_string().starts_with("not wired: worker-1 has no send wire to agent.parent.inbox"), "{err}");
        assert!(sb.drain_inbox(&id("parent")).is_empty());
        // Wired by the user: listed, delivered, and the sender is the socket's plug.
        sb.wire(tui("worker-1", &to_parent)).unwrap();
        let listed = client.list().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!((listed[0].receptacle.clone(), listed[0].modes.clone()), (to_parent.clone(), vec![WireMode::Send]));
        client.send(&to_parent, "done").await.unwrap();
        let got = sb.drain_inbox(&id("parent"));
        assert_eq!((got[0].sender.as_str(), got[0].text().as_str()), ("worker-1", "done"));
        // A plug added after serve started is served too.
        let late = sb.plug(id("worker-9")).unwrap();
        let mut late = PlugClient::connect(&late.host_socket).await.unwrap();
        assert!(late.send(&to_parent, "x").await.is_err());
        let outcomes: Vec<bool> = log.0.lock().unwrap().iter().map(|r| r.outcome == Outcome::Delivered).collect();
        assert_eq!(outcomes, [false, true, false]);
        // Unplugged: its connections close.
        sb.unplug(&id("worker-1"));
        assert!(client.list().await.is_err());
        cancel.cancel();
    }
}
