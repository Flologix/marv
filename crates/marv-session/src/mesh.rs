//! The session's side of the mesh (new in marv-rs; `docs/MESH.md`, `docs/subsystems/mesh.md` v1).
//!
//! - The session (one per process: the TUI's, or `marv -p`'s) owns the [`MeshHandle`]: one `Switchboard`, on the
//!   host, outside every sandbox ([`start`] makes one and serves it). Its trajectory sink is
//!   [`crate::trajectory::MeshSink`] on the conversation's current log ([`log_to`]; rule 3: every message logged).
//! - The main agent's plug is [`PARENT`]. Each subagent gets a plug of its own ([`SubagentPlug`], `worker-N`) with
//!   no wires, mounted in its sandbox at `/run/marv/mesh.sock`, and unplugged when it ends (its wires and unread
//!   messages go with it). It never inherits the parent's wires or approvals.
//! - The model reaches the mesh only through [`MeshTool`] (`mesh_list` Read, `mesh_call`/`mesh_send` Egress), over
//!   the agent's `PlugClient`, so every message goes through the switchboard's checks and log like any other plug's.
//! - Wires are granted only by a person: [`WireApprover`] ([`PersonWires`]: the TUI's approval, always asked,
//!   `once` and `must_ask`; [`HeadlessWires`]: `--approve mesh:<receptacle>`). Yolo never wires: the approver
//!   wrapped is the client's own, never the session's yolo or "always" layer, and nothing here lets an agent wire
//!   itself (a wire to its own inbox is never even asked for).
//! - Inbox messages are drained before the turn's request and between steps ([`inbox_note`], [`with_inbox`]) and
//!   go in fenced (`Envelope::fenced`), marking the session tainted with `Provenance::Mesh`.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use marv_agent::agent::AfterStep;
use marv_core::{Provenance, ToolKind};
use marv_mesh::approval::{WireRequest, parse_approve_flag};
use marv_mesh::limits::WireLimits;
use marv_mesh::plug::{Plug, PlugClient};
use marv_mesh::switchboard::{Switchboard, TrajectorySink};
use marv_mesh::tools::{MESH_CALL, MESH_LIST, MESH_SEND, kind_of, mesh_specs};
use marv_mesh::wire::Approver as WireBy;
use marv_mesh::{PlugId, ReceptacleName, WireMode};
use marv_sandbox::{Sandbox, TaintState};
use marv_tools::{ApprovalRequest, Approver, Decision, Preview, Scope, Tool, ToolContext, ToolError, ToolOutput, ToolRef};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::trajectory::{MeshSink, Trajectory};

/// The process's switchboard. It synchronizes itself (`&self`; `serve`/`route` take the `Arc`), so serving never
/// blocks wiring or `mesh_list`.
pub type MeshHandle = Arc<Switchboard>;

/// The main agent's plug: `agent.parent.inbox` is its inbox (docs/MESH.md, topology 1).
pub const PARENT: &str = "parent";
/// Subagents' plugs are `worker-1`, `worker-2`, …
pub const WORKER: &str = "worker";

pub fn parent_id() -> PlugId {
    PlugId::new(PARENT).expect("a valid plug id")
}

/// A switchboard for session `session`, its plug sockets in `$XDG_RUNTIME_DIR/marv/plugs/<session>` (0700), served
/// until `cancel`. `None` without a runtime dir: plugs never fall back to a world-readable `/tmp`.
pub fn start(session: &str, env: &marv_mcp::Env, cancel: CancellationToken) -> Option<MeshHandle> {
    let base = env.get("XDG_RUNTIME_DIR").filter(|d| std::path::Path::new(d).is_absolute())?;
    let dir = PathBuf::from(base).join("marv").join("plugs").join(session);
    let mesh = Arc::new(Switchboard::new(session, dir, Arc::new(marv_mesh::switchboard::NoTrajectory)));
    tokio::spawn(Arc::clone(&mesh).serve(cancel));
    Some(mesh)
}

/// Logs the switchboard's records to this conversation's trajectory from now on.
pub fn log_to(mesh: &MeshHandle, log: Option<&Arc<Trajectory>>) {
    let sink: Arc<dyn TrajectorySink> = match log {
        Some(log) => Arc::new(MeshSink(log.clone())),
        None => Arc::new(marv_mesh::switchboard::NoTrajectory),
    };
    mesh.set_trajectory(sink);
}

/// Asks a person for a wire: who granted it, or `None`. Never answered by yolo.
#[async_trait]
pub trait WireApprover: Send + Sync {
    async fn approve_wire(&self, request: WireRequest) -> Option<WireBy>;
}

/// What a wire approval shows: the full request (who, to what, which mode, the limits). Yes or no only, and a
/// person must answer it (no "always" layer, no yolo).
pub fn wire_approval(request: &WireRequest) -> ApprovalRequest {
    let limits = WireLimits::default();
    let (from, to, mode) = (&request.from, &request.to, request.mode);
    ApprovalRequest {
        tool: "mesh_wire".into(),
        label: request.title(),
        preview: Preview {
            title: request.title(),
            text: Some(format!(
                "{from} asks to {mode} to {to}.\nLimits: {} bytes a message, {} a minute (bursts of {}).",
                limits.max_message_bytes, limits.per_minute, limits.burst
            )),
            note: Some("The wire lasts until either agent ends.".into()),
            warning: Some("What arrives through it is data from another agent: the receiving session counts as tainted.".into()),
            ..Preview::default()
        },
        scope: Scope { key: format!("mesh-wire:{from}:{to}:{mode}"), description: format!("{from} -> {to}") },
        agent: (from.as_str() != PARENT).then(|| from.to_string()),
        network: false,
        once: true,
        must_ask: true,
        protected: None,
    }
}

/// A person approves wires through the client's approver (the TUI's approval queue).
pub struct PersonWires {
    /// The client's own approver, never the session's wrapper: "always" scopes and yolo don't answer for a wire.
    pub inner: Arc<dyn Approver>,
    pub by: WireBy,
}

#[async_trait]
impl WireApprover for PersonWires {
    async fn approve_wire(&self, request: WireRequest) -> Option<WireBy> {
        match self.inner.approve(wire_approval(&request)).await {
            Decision::Yes | Decision::Always => Some(self.by),
            Decision::No | Decision::Unavailable => None,
        }
    }
}

/// `marv -p --approve mesh:<receptacle>`: wires to the listed receptacles only.
pub struct HeadlessWires(pub Vec<ReceptacleName>);

#[async_trait]
impl WireApprover for HeadlessWires {
    async fn approve_wire(&self, request: WireRequest) -> Option<WireBy> {
        self.0.contains(&request.to).then_some(WireBy::Headless)
    }
}

/// The session's wire approver: the client's own, else a person through the client's tool approver.
pub fn wire_approver(own: Option<Arc<dyn WireApprover>>, approve: Option<&Arc<dyn Approver>>) -> Option<Arc<dyn WireApprover>> {
    own.or_else(|| approve.map(|inner| Arc::new(PersonWires { inner: inner.clone(), by: WireBy::Tui }) as Arc<dyn WireApprover>))
}

/// `--approve mesh:<receptacle>` flags → the receptacles pre-approved for headless `send`s, and the other flags (for
/// `buildApprover`). A malformed receptacle name is an error naming the flag.
pub fn headless_grants(flags: &[String]) -> Result<(Vec<ReceptacleName>, Vec<String>), String> {
    let (mut receptacles, mut rest) = (Vec::new(), Vec::new());
    for flag in flags {
        match parse_approve_flag(flag) {
            Some(Ok(name)) => receptacles.push(name),
            Some(Err(err)) => return Err(format!("--approve {flag}: {err}")),
            None => rest.push(flag.clone()),
        }
    }
    Ok((receptacles, rest))
}

/// The unread messages in `agent`'s inbox, fenced, as one note for the model; marks the session tainted (Mesh).
pub fn inbox_note(mesh: &Switchboard, agent: &PlugId, taint: &TaintState) -> Option<String> {
    let messages = mesh.drain_inbox(agent);
    if messages.is_empty() {
        return None;
    }
    taint.mark(Provenance::Mesh);
    Some(messages.iter().map(|m| m.fenced()).collect::<Vec<_>>().join("\n\n"))
}

/// `after_step` that also reads the agent's inbox: its messages follow the step's own note (the checks').
pub fn with_inbox(inner: Option<AfterStep>, mesh: MeshHandle, agent: PlugId, taint: Arc<TaintState>) -> AfterStep {
    Arc::new(move |runs| {
        let (inner, mesh, agent, taint) = (inner.clone(), mesh.clone(), agent.clone(), taint.clone());
        Box::pin(async move {
            let note = match inner {
                Some(after) => after(runs).await,
                None => None,
            };
            match (note, inbox_note(&mesh, &agent, &taint)) {
                (Some(note), Some(inbox)) => Some(format!("{note}\n\n{inbox}")),
                (note, inbox) => note.or(inbox),
            }
        })
    })
}

/// The switchboard and who approves wires: what the agent tool needs to give a subagent a plug.
#[derive(Clone)]
pub struct MeshAccess {
    pub mesh: MeshHandle,
    pub approver: Option<Arc<dyn WireApprover>>,
}

impl MeshAccess {
    /// A new subagent's plug (`worker-N`, no wires).
    pub fn plug_subagent(&self) -> Result<SubagentPlug, ToolError> {
        let plug = self.mesh.plug_next(WORKER).map_err(|e| ToolError::new(format!("The mesh: {e}")))?;
        Ok(SubagentPlug { plug, access: self.clone() })
    }
}

/// A subagent's plug while it runs. Dropping it unplugs the subagent: its wires, inbox and socket go.
pub struct SubagentPlug {
    pub plug: Plug,
    access: MeshAccess,
}

impl SubagentPlug {
    /// Its three mesh tools.
    pub fn tools(&self) -> Vec<ToolRef> {
        MeshTool::all(&self.plug, &self.access.mesh, self.access.approver.clone())
            .into_iter()
            .map(|t| Arc::new(t) as ToolRef)
            .collect()
    }

    /// Its sandbox: the parent's, with this plug's socket at `/run/marv/mesh.sock`.
    pub fn sandbox(&self, parent: &Sandbox) -> Arc<Sandbox> {
        Arc::new(Sandbox { mesh_socket: Some(self.plug.host_socket.clone()), ..parent.clone() })
    }

    /// Its `after_step`: reads its inbox between steps.
    pub fn after_step(&self, inner: Option<AfterStep>, taint: Arc<TaintState>) -> AfterStep {
        with_inbox(inner, self.access.mesh.clone(), self.plug.id.clone(), taint)
    }
}

impl Drop for SubagentPlug {
    fn drop(&mut self) {
        self.access.mesh.unplug(&self.plug.id);
    }
}

/// One of the three mesh tools for an agent's plug.
pub struct MeshTool {
    /// `mesh_list`, `mesh_call` or `mesh_send`.
    pub name: &'static str,
    pub plug: PlugId,
    /// The plug's host socket: the tool speaks to the switchboard as this plug, like anything inside its sandbox.
    pub socket: PathBuf,
    pub mesh: MeshHandle,
    pub approver: Option<Arc<dyn WireApprover>>,
}

impl MeshTool {
    /// The three, in `mesh_specs()` order.
    pub fn all(plug: &Plug, mesh: &MeshHandle, approver: Option<Arc<dyn WireApprover>>) -> Vec<Self> {
        [MESH_LIST, MESH_CALL, MESH_SEND]
            .into_iter()
            .map(|name| Self {
                name,
                plug: plug.id.clone(),
                socket: plug.host_socket.clone(),
                mesh: mesh.clone(),
                approver: approver.clone(),
            })
            .collect()
    }

    fn target(&self, input: &serde_json::Map<String, Value>) -> Option<(ReceptacleName, WireMode)> {
        marv_mesh::tools::target(self.name, input).ok().flatten()
    }

    fn wired(&self, input: &serde_json::Map<String, Value>) -> bool {
        self.target(input).is_some_and(|(r, m)| self.mesh.table().allows(&self.plug, &r, m))
    }

    /// No wire yet: asks a person for one, when one could be granted (the receptacle exists, takes the mode, and
    /// isn't this agent's own). Whatever the answer, the message then goes to the switchboard, which refuses and
    /// logs it if there's still no wire.
    async fn ask_for_wire(&self, receptacle: &ReceptacleName, mode: WireMode) {
        if self.mesh.table().allows(&self.plug, receptacle, mode) {
            return;
        }
        let Some(r) = self.mesh.receptacle(receptacle) else { return };
        let own = r.owner.as_ref() == Some(&self.plug) || receptacle.inbox_owner() == Some(self.plug.as_str());
        let Some(approver) = self.approver.as_ref().filter(|_| r.supports(mode) && !own) else { return };
        let request = WireRequest { from: self.plug.clone(), to: receptacle.clone(), mode, asked_by: self.plug.clone() };
        if let Some(by) = approver.approve_wire(request.clone()).await {
            // A refusal here (a self-wire) leaves it unwired: the switchboard says so.
            let _ = self.mesh.wire(request.grant(by));
        }
    }
}

#[async_trait]
impl Tool for MeshTool {
    type Input = serde_json::Map<String, Value>;
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        match self.name {
            MESH_LIST => marv_mesh::tools::LIST_DESCRIPTION,
            MESH_CALL => marv_mesh::tools::CALL_DESCRIPTION,
            _ => marv_mesh::tools::SEND_DESCRIPTION,
        }
    }
    fn schema(&self) -> Value {
        mesh_specs().into_iter().find(|s| s.name == self.name).map(|s| s.parameters).unwrap_or(Value::Null)
    }
    fn kind(&self) -> ToolKind {
        kind_of(self.name).unwrap_or(ToolKind::Egress)
    }
    fn provenance(&self) -> Provenance {
        Provenance::Mesh
    }
    fn check(&self, input: &Self::Input) -> Result<(), String> {
        marv_mesh::tools::target(self.name, input).map(drop).map_err(|e| e.to_string())
    }
    fn label(&self, input: &Self::Input) -> String {
        input.get("receptacle").and_then(Value::as_str).unwrap_or(self.name).to_string()
    }
    fn uses_network(&self, _input: &Self::Input) -> bool {
        self.name != MESH_LIST
    }
    async fn preview(&self, input: &Self::Input, _ctx: &ToolContext) -> Result<Preview, ToolError> {
        let Some((receptacle, mode)) = self.target(input) else {
            return Ok(Preview { title: self.name.into(), ..Preview::default() });
        };
        let (title, text) = match mode {
            WireMode::Send => (
                format!("Send a message to {receptacle}"),
                input.get("text").and_then(Value::as_str).unwrap_or_default().to_string(),
            ),
            _ => (format!("Call {receptacle}"), input.get("arguments").map(Value::to_string).unwrap_or_default()),
        };
        let note = if self.wired(input) {
            format!("over {}'s {mode} wire", self.plug)
        } else {
            format!("{} has no {mode} wire to it yet: you'll be asked to wire it", self.plug)
        };
        Ok(Preview { title, text: Some(text), note: Some(note), ..Preview::default() })
    }
    fn scope(&self, input: &Self::Input) -> Option<Scope> {
        let (receptacle, mode) = self.target(input)?;
        Some(Scope { key: format!("mesh:{receptacle}:{mode}"), description: format!("{mode}s to {receptacle}") })
    }
    /// Yolo may use a wire the user already granted (tainted sessions still ask: `must_ask`); it never wires.
    fn auto_safe(&self, input: &Self::Input, _ctx: &ToolContext) -> bool {
        self.wired(input)
    }
    async fn run(&self, input: &Self::Input, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let target = self.target(input);
        if let Some((receptacle, mode)) = &target {
            self.ask_for_wire(receptacle, *mode).await;
        }
        let result = match PlugClient::connect(&self.socket).await {
            Ok(mut client) => marv_mesh::tools::run(&mut client, self.name, input).await,
            Err(err) => Err(err),
        };
        Ok(match result {
            // A send's acknowledgement is Marv's sentence; a list or a call's answer comes from the mesh (taints).
            Ok(text) if self.name == MESH_SEND => ToolOutput::ok(text, "sent"),
            Ok(text) => ToolOutput {
                provenance: Provenance::Mesh,
                ..ToolOutput::ok(text, if self.name == MESH_LIST { "listed" } else { "answered" })
            },
            Err(err) => ToolOutput::error(err.to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marv_mesh::switchboard::{MeshRecord, Outcome};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn mesh_flags_are_split_from_the_rest() {
        let (grants, rest) = headless_grants(&["git_write".into(), "mesh:agent.parent.inbox".into()]).unwrap();
        assert_eq!(grants.len(), 1);
        assert_eq!(rest, vec!["git_write"]);
        assert!(headless_grants(&["mesh:Not A Name".into()]).is_err());
    }

    #[derive(Default)]
    struct Log(Mutex<Vec<MeshRecord>>);
    impl TrajectorySink for Log {
        fn mesh(&self, record: MeshRecord) {
            self.0.lock().unwrap().push(record);
        }
    }

    /// Answers every approval with `decision`, keeping what it was asked.
    struct Person(Decision, Mutex<Vec<ApprovalRequest>>);
    #[async_trait]
    impl Approver for Person {
        async fn approve(&self, request: ApprovalRequest) -> Decision {
            self.1.lock().unwrap().push(request);
            self.0
        }
    }

    struct Runtime(PathBuf);
    impl Drop for Runtime {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn switchboard() -> (MeshHandle, Arc<Log>, Runtime, CancellationToken) {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!("ms-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        let log = Arc::new(Log::default());
        let mesh = Arc::new(Switchboard::new("s", dir.clone(), log.clone()));
        let cancel = CancellationToken::new();
        tokio::spawn(Arc::clone(&mesh).serve(cancel.clone()));
        (mesh, log, Runtime(dir), cancel)
    }

    fn input(v: Value) -> serde_json::Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    fn tool(name: &str, plug: &Plug, mesh: &MeshHandle, approver: Option<Arc<dyn WireApprover>>) -> MeshTool {
        MeshTool::all(plug, mesh, approver).into_iter().find(|t| t.name == name).unwrap()
    }

    #[tokio::test]
    async fn a_send_asks_a_person_for_the_wire_once_then_delivers() {
        let (mesh, log, _rt, cancel) = switchboard();
        let parent = mesh.plug(parent_id()).unwrap();
        let worker = mesh.plug_next(WORKER).unwrap();
        let person = Arc::new(Person(Decision::Yes, Mutex::default()));
        let approver = wire_approver(None, Some(&(person.clone() as Arc<dyn Approver>)));
        let send = tool(MESH_SEND, &worker, &mesh, approver);
        let ctx = ToolContext::new(std::env::temp_dir());
        let args = input(serde_json::json!({"receptacle": "agent.parent.inbox", "text": "found it"}));
        assert!(!send.auto_safe(&args, &ctx), "unwired: yolo doesn't send");
        let out = send.run(&args, &ctx).await.unwrap();
        assert!(!out.is_error, "{}", out.output);
        assert_eq!(out.provenance, Provenance::Project);
        // The approval showed the whole request, a person had to answer, and only yes or no.
        let asked = person.1.lock().unwrap().clone();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].preview.title, "Wire worker-1 -> agent.parent.inbox (send)");
        assert!(asked[0].preview.text.as_ref().unwrap().contains("32768 bytes a message"));
        assert!(asked[0].must_ask && asked[0].once);
        assert_eq!(asked[0].agent.as_deref(), Some("worker-1"));
        assert!(send.auto_safe(&args, &ctx), "wired: yolo may use it");
        send.run(&args, &ctx).await.unwrap();
        assert_eq!(person.1.lock().unwrap().len(), 1, "asked once");
        // The parent reads both, fenced, and its session is tainted.
        let taint = TaintState::default();
        let note = inbox_note(&mesh, &parent.id, &taint).unwrap();
        assert_eq!(note.matches("<mesh-message from=\"worker-1\"").count(), 2);
        assert!(taint.sources().contains(&Provenance::Mesh));
        assert!(inbox_note(&mesh, &parent.id, &taint).is_none());
        assert_eq!(log.0.lock().unwrap().len(), 2);
        cancel.cancel();
    }

    #[tokio::test]
    async fn declined_or_unasked_wires_are_refused_and_logged() {
        let (mesh, log, _rt, cancel) = switchboard();
        mesh.plug(parent_id()).unwrap();
        let worker = mesh.plug_next(WORKER).unwrap();
        let ctx = ToolContext::new(std::env::temp_dir());
        let args = input(serde_json::json!({"receptacle": "agent.parent.inbox", "text": "hi"}));
        let no = Arc::new(Person(Decision::No, Mutex::default()));
        for approver in [None, wire_approver(None, Some(&(no.clone() as Arc<dyn Approver>)))] {
            let out = tool(MESH_SEND, &worker, &mesh, approver).run(&args, &ctx).await.unwrap();
            assert!(out.is_error);
            assert!(out.output.starts_with("not wired: worker-1 has no send wire"), "{}", out.output);
        }
        assert_eq!(no.1.lock().unwrap().len(), 1);
        let records = log.0.lock().unwrap();
        assert_eq!(records.len(), 2);
        assert!(records.iter().all(|r| matches!(r.outcome, Outcome::Refused { .. })));
        assert!(mesh.drain_inbox(&parent_id()).is_empty());
        cancel.cancel();
    }

    #[tokio::test]
    async fn an_agent_is_never_asked_about_wiring_itself() {
        let (mesh, _log, _rt, cancel) = switchboard();
        let worker = mesh.plug_next(WORKER).unwrap();
        let person = Arc::new(Person(Decision::Yes, Mutex::default()));
        let approver = wire_approver(None, Some(&(person.clone() as Arc<dyn Approver>)));
        let args = input(serde_json::json!({"receptacle": "agent.worker-1.inbox", "text": "hi"}));
        let out = tool(MESH_SEND, &worker, &mesh, approver).run(&args, &ToolContext::new(std::env::temp_dir())).await.unwrap();
        assert!(out.is_error);
        assert!(person.1.lock().unwrap().is_empty());
        assert!(mesh.table().all().is_empty());
        cancel.cancel();
    }

    #[tokio::test]
    async fn headless_wires_only_what_approve_mesh_names() {
        let wires = HeadlessWires(vec![ReceptacleName::new("agent.parent.inbox").unwrap()]);
        let ask = |to: &str| WireRequest {
            from: PlugId::new("worker-1").unwrap(),
            to: ReceptacleName::new(to).unwrap(),
            mode: WireMode::Send,
            asked_by: PlugId::new("worker-1").unwrap(),
        };
        assert_eq!(wires.approve_wire(ask("agent.parent.inbox")).await, Some(WireBy::Headless));
        assert_eq!(wires.approve_wire(ask("agent.worker-2.inbox")).await, None);
    }

    #[tokio::test]
    async fn a_subagent_plug_is_its_own_mounted_and_gone_when_it_ends() {
        let (mesh, _log, _rt, cancel) = switchboard();
        let access = MeshAccess { mesh: mesh.clone(), approver: None };
        let sub = access.plug_subagent().unwrap();
        assert_eq!(sub.plug.id.as_str(), "worker-1");
        let sandbox = sub.sandbox(&Sandbox::from_env(true));
        assert_eq!(sandbox.mesh_socket.as_ref(), Some(&sub.plug.host_socket));
        assert_eq!(sub.tools().iter().map(|t| t.name().to_string()).collect::<Vec<_>>(), [MESH_LIST, MESH_CALL, MESH_SEND]);
        let socket = sub.plug.host_socket.clone();
        drop(sub);
        assert!(!socket.exists());
        assert!(mesh.receptacle(&ReceptacleName::new("agent.worker-1.inbox").unwrap()).is_none());
        // Never reused: the next subagent is worker-2.
        assert_eq!(access.plug_subagent().unwrap().plug.id.as_str(), "worker-2");
        cancel.cancel();
    }
}
