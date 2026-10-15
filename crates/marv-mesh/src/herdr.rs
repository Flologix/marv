//! Herdr: pane status reporting (ports Marv's `src/herdr.ts`) and, for mesh
//! v2, the adapter that offers Herdr panes as receptacles (new, [`HerdrAdapter`]).
//!
//! Herdr is a terminal multiplexer for coding agents. When Marv runs in one of
//! its panes, it tells Herdr what it's doing, so Herdr's sidebar shows working /
//! blocked / done, and `herdr agent list|get|wait|read` see Marv.
//!
//! - Herdr puts `HERDR_ENV=1`, `HERDR_SOCKET_PATH` and `HERDR_PANE_ID` into every
//!   pane's environment ([`herdr_target`]). The sandbox's empty environment keeps
//!   them from commands.
//! - A report is one line of JSON-RPC on that Unix socket (`pane.report_agent`,
//!   `pane.report_agent_session`, `pane.release_agent`), source `herdr:marv`,
//!   agent `marv`, the protocol of Herdr's own integrations.
//! - A rising `seq` (microseconds since the epoch, then +1 per report): Herdr
//!   drops a report whose seq isn't newer, so a slow, older report can't
//!   overwrite a newer state.
//! - **Best effort**: repeats are skipped, only the newest state is sent when
//!   several pile up, a silent Herdr gets one retry (500 ms, then 1500 ms) and is
//!   let go, nothing fails. Herdr not answering must never slow Marv down.
//! - As of Herdr 0.8.2 it accepts Marv's session report but doesn't store it,
//!   and `herdr agent prompt|send-keys` refuse Marv ("not an active named
//!   agent"; `herdr pane send-text|send-keys` work).
//!
//! The App reports `blocked` while an approval waits (message: the asking
//! subagent and the request's label), `working` while busy, `idle` otherwise;
//! the CLI sends `release` on the way out (at most 1 s).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::watch;

use crate::MeshError;
use crate::message::Envelope;
use crate::receptacle::{Receptacle, ReceptacleKind, ReceptacleName, valid_segment};
use crate::switchboard::{Resolve, Service};
use crate::wire::WireMode;

const AGENT: &str = "marv";
const SOURCE: &str = "herdr:marv";
pub const QUICK_TRY: Duration = Duration::from_millis(500);
pub const PATIENT_TRY: Duration = Duration::from_millis(1500);

/// What Herdr shows for an agent. `idle` becomes `done` in Herdr when the pane's tab wasn't being looked at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HerdrState {
    Idle,
    Working,
    Blocked,
}

impl HerdrState {
    fn as_str(self) -> &'static str {
        match self {
            HerdrState::Idle => "idle",
            HerdrState::Working => "working",
            HerdrState::Blocked => "blocked",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HerdrTarget {
    pub socket_path: String,
    pub pane_id: String,
}

/// The pane Marv runs in, or `None` outside Herdr.
pub fn herdr_target(env: &marv_mcp::Env) -> Option<HerdrTarget> {
    let socket_path = env.get("HERDR_SOCKET_PATH").filter(|s| !s.is_empty())?;
    let pane_id = env.get("HERDR_PANE_ID").filter(|s| !s.is_empty())?;
    (env.get("HERDR_ENV").map(String::as_str) == Some("1"))
        .then(|| HerdrTarget { socket_path: socket_path.clone(), pane_id: pane_id.clone() })
}

/// Sends one request; true once Herdr answered. Never fails.
#[async_trait]
pub trait HerdrSend: Send + Sync {
    async fn send(&self, request: &Value, timeout: Duration) -> bool;
}

/// The real sender: one connection per request, one JSON line, any answer counts.
pub struct SocketSender {
    pub socket_path: String,
}

#[async_trait]
impl HerdrSend for SocketSender {
    async fn send(&self, request: &Value, timeout: Duration) -> bool {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let attempt = async {
            let mut stream = tokio::net::UnixStream::connect(&self.socket_path).await.ok()?;
            stream.write_all(format!("{request}\n").as_bytes()).await.ok()?;
            let mut buf = [0u8; 1];
            (stream.read(&mut buf).await.ok()? > 0).then_some(())
        };
        matches!(tokio::time::timeout(timeout, attempt).await, Ok(Some(())))
    }
}

/// One request in Herdr's protocol.
pub fn build_request(target: &HerdrTarget, method: &str, params: Map<String, Value>, seq: u64) -> Value {
    let mut p = Map::new();
    p.insert("pane_id".into(), target.pane_id.clone().into());
    p.insert("source".into(), SOURCE.into());
    p.insert("agent".into(), AGENT.into());
    p.insert("seq".into(), seq.into());
    p.extend(params);
    json!({"id": format!("{SOURCE}:{seq}"), "method": method, "params": p})
}

struct Inner {
    seq: u64,
    session_ref: Map<String, Value>,
    last: Option<String>,
    /// Only the newest state matters: a newer one replaces whatever was waiting.
    queued: Option<(HerdrState, Option<String>, u64)>,
}

/// Reports Marv's state to the pane it runs in.
#[derive(Clone)]
pub struct HerdrReporter {
    target: HerdrTarget,
    send: Arc<dyn HerdrSend>,
    inner: Arc<Mutex<Inner>>,
    /// Whether a drain is running (for `settled`).
    sending: Arc<watch::Sender<bool>>,
}

impl HerdrReporter {
    pub fn new(target: HerdrTarget, send: Option<Arc<dyn HerdrSend>>) -> Self {
        let send = send.unwrap_or_else(|| Arc::new(SocketSender { socket_path: target.socket_path.clone() }));
        let now_us = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_micros() as u64);
        let (sending, _) = watch::channel(false);
        Self {
            target,
            send,
            inner: Arc::new(Mutex::new(Inner { seq: now_us, session_ref: Map::new(), last: None, queued: None })),
            sending: Arc::new(sending),
        }
    }

    fn next_seq(inner: &mut Inner) -> u64 {
        inner.seq += 1;
        inner.seq
    }

    /// Reports Marv's state (repeats of the same state are skipped). Needs a Tokio runtime.
    pub fn state(&self, state: HerdrState, message: Option<&str>) {
        {
            let mut inner = self.inner.lock().expect("herdr lock");
            let key = format!("{}\n{}", state.as_str(), message.unwrap_or(""));
            if inner.last.as_deref() == Some(&key) {
                return;
            }
            inner.last = Some(key);
            let seq = Self::next_seq(&mut inner);
            inner.queued = Some((state, message.map(str::to_string), seq));
        }
        if !*self.sending.borrow() {
            self.sending.send_replace(true);
            let me = self.clone();
            tokio::spawn(async move { me.drain().await });
        }
    }

    async fn drain(&self) {
        loop {
            let next = {
                let mut inner = self.inner.lock().expect("herdr lock");
                match inner.queued.take() {
                    Some((state, message, seq)) => {
                        let mut params = Map::new();
                        params.insert("state".into(), state.as_str().into());
                        if let Some(m) = message.filter(|m| !m.is_empty()) {
                            params.insert("message".into(), m.into());
                        }
                        params.extend(inner.session_ref.clone());
                        Some((params, seq))
                    }
                    None => {
                        self.sending.send_replace(false);
                        None
                    }
                }
            };
            let Some((params, seq)) = next else { return };
            self.request("pane.report_agent", params, seq).await;
        }
    }

    /// Tells Herdr which saved session the pane holds (again after /clear or a resume).
    pub async fn session(&self, id: &str, path: Option<&str>) {
        let (params, seq) = {
            let mut inner = self.inner.lock().expect("herdr lock");
            let mut r = Map::new();
            r.insert("agent_session_id".into(), id.into());
            if let Some(p) = path.filter(|p| !p.is_empty()) {
                r.insert("agent_session_path".into(), p.into());
            }
            inner.session_ref = r.clone();
            (r, Self::next_seq(&mut inner))
        };
        self.request("pane.report_agent_session", params, seq).await;
    }

    /// Marv is leaving the pane. Waits for what's still being sent first.
    pub async fn release(&self) {
        self.inner.lock().expect("herdr lock").queued = None;
        self.settled().await;
        let seq = Self::next_seq(&mut self.inner.lock().expect("herdr lock"));
        self.request("pane.release_agent", Map::new(), seq).await;
    }

    /// Waits until the latest state has been sent.
    pub async fn settled(&self) {
        let mut rx = self.sending.subscribe();
        let _ = rx.wait_for(|sending| !sending).await;
    }

    /// One quick try, then one patient one (Herdr may be busy redrawing), then give up quietly.
    async fn request(&self, method: &str, params: Map<String, Value>, seq: u64) {
        let request = build_request(&self.target, method, params, seq);
        if self.send.send(&request, QUICK_TRY).await {
            return;
        }
        self.send.send(&request, PATIENT_TRY).await;
    }
}

/// The `herdr` CLI gets this long per command; Herdr not answering is an error the agent reads, never a hang.
pub const HERDR_TIMEOUT: Duration = Duration::from_secs(5);
/// `mesh_list` waits this long for `herdr agent list`, then lists no panes: a slow or missing Herdr never holds the
/// list up.
pub const LIST_TIMEOUT: Duration = Duration::from_millis(1500);
/// `herdr.pane.<pane>.status` reads at most this many lines of the pane's output.
pub const MAX_READ_LINES: u64 = 200;
/// Between the text and its Enter: an agent's TUI may still be taking the text in (Herdr's own `agent prompt` waits
/// too), and an Enter that lands first submits half a prompt.
const ENTER_DELAY: Duration = Duration::from_millis(150);
/// After a send, the same pane takes no other for this long: Herdr may still call its agent idle while the prompt
/// just typed is being taken in, and a second send's text and Enter would land in whatever that agent shows next.
pub const SEND_GAP: Duration = Duration::from_secs(5);

/// What a pane receptacle does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneAction {
    /// `herdr.pane.<pane>.send`: types a prompt into the pane and presses Enter.
    Send,
    /// `herdr.pane.<pane>.status`: the pane's agent and state and, with `lines: N`, its last N lines: how a pane is
    /// read (#123: the model missed that; kept as one receptacle, so one wire reads both).
    Status,
}

impl PaneAction {
    fn as_str(self) -> &'static str {
        match self {
            PaneAction::Send => "send",
            PaneAction::Status => "status",
        }
    }
}

/// A Herdr pane id (`w2T:p1`) as one receptacle-name segment (`w2_t-p1`): an uppercase letter becomes `_` and the
/// letter, `:` becomes `-`. Ids with anything else (or with `_`/`-`, which would read back wrong) aren't offered.
pub fn pane_segment(pane: &str) -> Option<String> {
    let mut out = String::new();
    for c in pane.chars() {
        match c {
            'a'..='z' | '0'..='9' => out.push(c),
            'A'..='Z' => {
                out.push('_');
                out.push(c.to_ascii_lowercase());
            }
            ':' => out.push('-'),
            _ => return None,
        }
    }
    valid_segment(&out).then_some(out)
}

/// The pane id a [`pane_segment`] came from.
pub fn pane_from_segment(segment: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = segment.chars();
    while let Some(c) = chars.next() {
        match c {
            '_' => out.push(chars.next().filter(char::is_ascii_lowercase)?.to_ascii_uppercase()),
            '-' => out.push(':'),
            c => out.push(c),
        }
    }
    Some(out)
}

/// `herdr.pane.<segment>.send` or `.status`.
pub fn pane_receptacle(pane: &str, action: PaneAction) -> Option<ReceptacleName> {
    ReceptacleName::new(format!("herdr.pane.{}.{}", pane_segment(pane)?, action.as_str())).ok()
}

/// The pane and action a `herdr.pane.<segment>.<action>` name stands for.
pub fn parse_pane_receptacle(name: &ReceptacleName) -> Option<(String, PaneAction)> {
    let (segment, action) = name.as_str().strip_prefix("herdr.pane.")?.split_once('.')?;
    let action = match action {
        "send" => PaneAction::Send,
        "status" => PaneAction::Status,
        _ => return None,
    };
    Some((pane_from_segment(segment)?, action))
}

/// A pane as `herdr agent list` / `pane get` describe it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct HerdrPane {
    pub pane_id: String,
    /// The agent Herdr detected in it (`claude`, `marv`), if any.
    #[serde(default)]
    pub agent: Option<String>,
    /// The name it was given (`herdr agent rename`).
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, rename = "agent_status")]
    pub status: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

/// Mesh v2: Herdr panes as receptacles, `herdr.pane.<pane>.send` and `herdr.pane.<pane>.status` (pane ids as
/// [`pane_segment`]s). Replaces hand-typed orchestration with wires the user approves, while each worker stays
/// sandboxed.
///
/// - **One receptacle per pane and action**, not one `herdr.pane.send` with the pane in the message: a grant is
///   decided on names only (docs/MESH.md), so approving a wire approves typing into that one pane.
/// - Only panes where Herdr sees an agent are offered, never a plain shell (typing there would run commands outside
///   any sandbox), and never Marv's own pane (an agent can't wire itself).
/// - A send goes only to an agent that's `idle` or `done`: one waiting at an approval would take the Enter as an
///   answer (Herdr's `agent prompt` refuses a blocked agent for the same reason). Control characters are dropped and
///   newlines become spaces, so the text can't drive the TUI or submit early. What arrives is typed as if by the user:
///   the wire approval says so.
/// - A send is a prompt, never a command ([`pane_prompt`]): text starting with `/`, `!` or `#` is refused, so a wire
///   to a pane asks its agent for work and never reaches that agent's own commands or settings.
/// - Commands run from the switchboard, on the host, through the `herdr` CLI (`pane get`, `pane send-text` then
///   `pane send-keys enter`, since Herdr 0.8.2 refuses `agent prompt` for Marv; `pane read`), never from a sandbox.
///   A pane's state and output are the other side's text: answers are `Provenance::Mesh`, fenced like any mesh text.
pub struct HerdrAdapter {
    /// The `herdr` program (tests: a fake script).
    pub program: PathBuf,
    /// The pane Marv runs in (`HERDR_PANE_ID`): never offered.
    pub own_pane: Option<String>,
    /// When each pane was last typed into ([`SEND_GAP`]). Held for a whole send, so sends one at a time.
    sent: tokio::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
}

impl HerdrAdapter {
    pub fn new(program: impl Into<PathBuf>, own_pane: Option<String>) -> Self {
        Self { program: program.into(), own_pane, sent: Default::default() }
    }

    /// Runs `herdr <args>` and returns its JSON `result`. Herdr's own words never reach the agent: an error is named
    /// by the command, plus Herdr's error code when it's a plain identifier (`pane_not_found`).
    async fn run(&self, args: &[&str]) -> crate::Result<Value> {
        let what = format!("herdr {}", args.iter().take(2).copied().collect::<Vec<_>>().join(" "));
        let mut command = tokio::process::Command::new(&self.program);
        command
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let output = match tokio::time::timeout(HERDR_TIMEOUT, command.output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => return Err(MeshError::Io(format!("{what}: {e}"))),
            Err(_) => return Err(MeshError::Io(format!("{what}: Herdr didn't answer in {} s", HERDR_TIMEOUT.as_secs()))),
        };
        let answer: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
        if let Some(code) = answer.pointer("/error/code").and_then(Value::as_str) {
            let plain = code.chars().all(|c| c.is_ascii_lowercase() || c == '_') && code.len() <= 40;
            return Err(MeshError::Io(if plain { format!("{what}: {code}") } else { format!("{what} failed") }));
        }
        if !output.status.success() {
            return Err(MeshError::Io(format!("{what} failed ({})", output.status)));
        }
        Ok(answer.get("result").cloned().unwrap_or(Value::Null))
    }

    /// The panes Herdr sees an agent in (`herdr agent list`).
    pub async fn agents(&self) -> crate::Result<Vec<HerdrPane>> {
        let result = self.run(&["agent", "list"]).await?;
        Ok(serde_json::from_value(result.get("agents").cloned().unwrap_or_default()).unwrap_or_default())
    }

    /// One pane (`herdr pane get`).
    pub async fn pane(&self, pane: &str) -> crate::Result<HerdrPane> {
        let result = self.run(&["pane", "get", pane]).await?;
        serde_json::from_value(result.get("pane").cloned().unwrap_or_default())
            .map_err(|_| MeshError::Io("herdr pane get: an answer without a pane".into()))
    }

    /// Whether a pane can be offered: an agent's, and not Marv's own.
    fn offerable(&self, pane: &HerdrPane) -> bool {
        pane.agent.is_some() && self.own_pane.as_deref() != Some(pane.pane_id.as_str()) && pane_segment(&pane.pane_id).is_some()
    }

    /// The receptacle for one of a pane's actions. Descriptions are Marv's words plus Herdr's ids.
    pub fn receptacle(pane: &HerdrPane, action: PaneAction) -> Option<Receptacle> {
        let name = pane_receptacle(&pane.pane_id, action)?;
        let who = pane.agent.as_deref().unwrap_or("agent");
        let (description, schema) = match action {
            PaneAction::Send => (
                format!(
                    "Types a prompt into the {who} in Herdr pane {} and presses Enter, as if the user typed it; a prompt, never \
                     a command: text starting with / ! or # is refused. Arguments: {{\"text\": \"...\"}}, nothing \
                     else. It can't answer you: read the pane with .status and lines: N{}",
                    pane.pane_id,
                    if who == AGENT {
                        ", or, when mesh_list names this Marv, mesh_send to its inbox (peer.<marv>.agent.parent.inbox) for a reply"
                    } else {
                        ""
                    }
                ),
                json!({
                    "type": "object",
                    "properties": {"text": {"type": "string", "description": "The prompt to type: a prompt, not a command (no leading / ! or #)."}},
                    "required": ["text"],
                    "additionalProperties": false
                }),
            ),
            PaneAction::Status => (
                format!(
                    "Reads Herdr pane {}: the {who}'s state and, with lines: N (up to {MAX_READ_LINES}), the pane's last N \
                     lines, which is what the agent shows (its chat)",
                    pane.pane_id
                ),
                json!({"type": "object", "properties": {"lines": {
                    "type": "integer", "minimum": 0, "maximum": MAX_READ_LINES,
                    "description": "Also return the pane's last N lines (its chat)."
                }}}),
            ),
        };
        Some(Receptacle {
            name,
            owner: None,
            kind: ReceptacleKind::HerdrPane,
            description,
            modes: vec![WireMode::Call],
            input_schema: schema,
        })
    }

    /// The panes `mesh_list` shows as wireable: the agents' panes, Marv's own left out, within [`LIST_TIMEOUT`]. A
    /// slow, failing or missing Herdr is no panes (the `herdr` it started is killed with the dropped command).
    pub async fn wireable_panes(&self) -> Vec<HerdrPane> {
        match tokio::time::timeout(LIST_TIMEOUT, self.agents()).await {
            Ok(Ok(panes)) => panes.into_iter().filter(|p| self.offerable(p)).collect(),
            _ => Vec::new(),
        }
    }

    /// Every receptacle the adapter offers right now: two per agent pane, Marv's own left out.
    pub async fn receptacles(&self) -> crate::Result<Vec<Receptacle>> {
        let panes = self.agents().await?;
        Ok(panes
            .iter()
            .filter(|p| self.offerable(p))
            .flat_map(|p| [PaneAction::Status, PaneAction::Send].into_iter().filter_map(|a| Self::receptacle(p, a)))
            .collect())
    }

    /// `herdr.pane.<pane>.send`: the text, then Enter, into an agent that's idle or done. The text is a prompt, never
    /// a command ([`pane_prompt`]).
    pub async fn send_text(&self, pane: &str, text: &str) -> crate::Result<String> {
        let text = pane_prompt(text)?;
        // One send at a time, and not again into a pane typed into within SEND_GAP: its state may not show it yet.
        let mut sent = self.sent.lock().await;
        sent.retain(|_, at| at.elapsed() < SEND_GAP);
        if sent.get(pane).is_some_and(|at| at.elapsed() < SEND_GAP) {
            return Err(MeshError::Io(format!(
                "Herdr pane {pane} was just sent a prompt: it takes another once its agent is idle or done again, try again later"
            )));
        }
        let target = self.pane(pane).await?;
        if !self.offerable(&target) {
            return Err(MeshError::NoReceptacle(format!("an agent in Herdr pane {pane}")));
        }
        let state = target.status.as_deref().unwrap_or("unknown");
        if !matches!(state, "idle" | "done") {
            return Err(MeshError::Io(format!(
                "the agent in Herdr pane {pane} is {state}: it takes a prompt only when idle or done, try again later"
            )));
        }
        sent.insert(pane.to_string(), std::time::Instant::now());
        self.run(&["pane", "send-text", pane, &text]).await?;
        tokio::time::sleep(ENTER_DELAY).await;
        self.run(&["pane", "send-keys", pane, "enter"]).await?;
        Ok(format!("Typed into Herdr pane {pane} and pressed Enter."))
    }

    /// `herdr.pane.<pane>.status`: the pane's agent and state as JSON, with its last `lines` lines when asked.
    pub async fn status(&self, pane: &str, lines: Option<u64>) -> crate::Result<String> {
        let target = self.pane(pane).await?;
        let mut status = json!({"pane": target.pane_id, "agent": target.agent, "name": target.name, "state": target.status});
        if let Some(n) = lines.filter(|n| *n > 0) {
            let n = n.min(MAX_READ_LINES).to_string();
            let read = self.run(&["pane", "read", pane, "--source", "recent", "--lines", &n, "--format", "text"]).await?;
            let text = read.get("text").or_else(|| read.get("output")).cloned().unwrap_or(read);
            status["output"] = match text {
                Value::String(s) => Value::String(s),
                other => Value::String(other.to_string()),
            };
        }
        Ok(status.to_string())
    }
}

/// What the Herdr door lists (#74): `herdr workspace list`, `tab list`, `pane list`, `agent list`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HerdrList {
    Workspaces,
    Tabs,
    Panes,
    Agents,
}

/// Where the Herdr door opens a pane (#74): split from Marv's own pane, or a new tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    Right,
    Down,
    Tab,
}

impl Place {
    pub fn as_str(self) -> &'static str {
        match self {
            Place::Right => "right",
            Place::Down => "down",
            Place::Tab => "tab",
        }
    }
}

/// How long a spawned Marv gets to show its prompt (its splash, setup, a resume) before the door gives up on typing
/// its task (#74).
pub const READY_TIMEOUT: Duration = Duration::from_secs(60);
/// How often the door looks at a starting pane.
const READY_POLL: Duration = Duration::from_millis(500);

/// An id Herdr handed back (`w1`, `w1:t2`, `w1:p3`): opaque, but only ever these characters, so it's Marv's to show
/// and pass on, never text another program chose.
pub fn plain_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '-'))
}

/// The Herdr door's commands (#74), on the same `herdr` CLI as the pane receptacles, run in Marv's own process (the
/// sandbox can't reach Herdr's socket: #130). What comes back from a read is the other panes' text: the door fences it
/// and taints the session with it. Errors name the command, never Herdr's words (`run`).
impl HerdrAdapter {
    /// `herdr <args>`'s JSON `result` (the door's commands).
    pub async fn call(&self, args: &[&str]) -> crate::Result<Value> {
        self.run(args).await
    }

    /// A listing, as Herdr's JSON (`workspace list`, `tab list [--workspace w]`, `pane list [--workspace w]`, `agent
    /// list`).
    pub async fn list(&self, what: HerdrList, workspace: Option<&str>) -> crate::Result<Value> {
        let mut args = match what {
            HerdrList::Workspaces => vec!["workspace", "list"],
            HerdrList::Tabs => vec!["tab", "list"],
            HerdrList::Panes => vec!["pane", "list"],
            HerdrList::Agents => vec!["agent", "list"],
        };
        if let Some(w) = workspace.filter(|_| matches!(what, HerdrList::Tabs | HerdrList::Panes)) {
            args.extend(["--workspace", w]);
        }
        self.run(&args).await
    }

    /// A pane's last `lines` lines (at most [`MAX_READ_LINES`]), soft wraps joined, as plain text.
    pub async fn read(&self, pane: &str, lines: u64) -> crate::Result<String> {
        let n = lines.clamp(1, MAX_READ_LINES).to_string();
        let read = self.run(&["pane", "read", pane, "--source", "recent-unwrapped", "--lines", &n, "--format", "text"]).await?;
        Ok(match read.get("text").or_else(|| read.get("output")).cloned().unwrap_or(read) {
            Value::String(s) => s,
            other => other.to_string(),
        })
    }

    /// Opens a pane in `cwd`: split from `from` (Marv's own pane) to the right or below, or a new tab. Focus stays
    /// where the user has it. Returns the new pane's id.
    pub async fn open_pane(&self, place: Place, from: Option<&str>, cwd: &str, label: Option<&str>) -> crate::Result<String> {
        let result = match place {
            Place::Tab => {
                let mut args = vec!["tab", "create", "--cwd", cwd, "--no-focus"];
                if let Some(label) = label {
                    args.extend(["--label", label]);
                }
                self.run(&args).await?
            }
            Place::Right | Place::Down => {
                let Some(from) = from else {
                    return Err(MeshError::Io("herdr pane split: Marv doesn't know its own pane (HERDR_PANE_ID)".into()));
                };
                self.run(&["pane", "split", from, "--direction", place.as_str(), "--cwd", cwd, "--no-focus"]).await?
            }
        };
        new_pane_id(&result).ok_or_else(|| MeshError::Io("herdr: an answer without the new pane's id".into()))
    }

    /// A new git worktree on `branch` (from `base`), opened by Herdr in a workspace of its own. Returns its first pane.
    pub async fn worktree(&self, repo: &str, branch: &str, base: &str, label: Option<&str>) -> crate::Result<String> {
        let mut args = vec!["worktree", "create", "--cwd", repo, "--branch", branch, "--base", base, "--no-focus"];
        if let Some(label) = label {
            args.extend(["--label", label]);
        }
        let result = self.run(&args).await?;
        new_pane_id(&result).ok_or_else(|| MeshError::Io("herdr worktree create: an answer without the new pane's id".into()))
    }

    /// Types `command` into a pane's shell and presses Enter (`herdr pane run`).
    pub async fn pane_run(&self, pane: &str, command: &str) -> crate::Result<()> {
        self.run(&["pane", "run", pane, command]).await.map(drop)
    }

    /// Starts a Herdr-supported agent `kind` named `name` in a pane at its shell prompt (`herdr agent start`), with no
    /// arguments of its own: it runs with its own defaults, never with flags a model chose.
    pub async fn agent_start(&self, name: &str, kind: &str, pane: &str) -> crate::Result<()> {
        self.run(&["agent", "start", name, "--kind", kind, "--pane", pane]).await.map(drop)
    }

    /// Waits until the pane holds a `agent` that's idle or done (a Marv past its splash, at an empty prompt), at most
    /// `timeout`; false when it never got there (or `cancel` fired).
    pub async fn wait_ready(
        &self,
        pane: &str,
        agent: &str,
        timeout: Duration,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Ok(p) = self.pane(pane).await
                && p.agent.as_deref() == Some(agent)
                && matches!(p.status.as_deref(), Some("idle" | "done"))
            {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::select! {
                _ = tokio::time::sleep(READY_POLL) => {}
                _ = cancel.cancelled() => return false,
            }
        }
    }

    /// Focuses a tab (`w1:t2`), a workspace (`w1`) or the agent in a pane (`w1:p3`).
    pub async fn focus(&self, target: &str) -> crate::Result<()> {
        let args: [&str; 3] = match target_kind(target) {
            Some(TargetKind::Pane) => ["agent", "focus", target],
            Some(TargetKind::Tab) => ["tab", "focus", target],
            Some(TargetKind::Workspace) => ["workspace", "focus", target],
            None => return Err(MeshError::BadName(format!("not a Herdr pane, tab or workspace id: {target}"))),
        };
        self.run(&args).await.map(drop)
    }

    /// Renames a pane or a tab.
    pub async fn rename(&self, target: &str, name: &str) -> crate::Result<()> {
        let args: [&str; 4] = match target_kind(target) {
            Some(TargetKind::Pane) => ["pane", "rename", target, name],
            Some(TargetKind::Tab) => ["tab", "rename", target, name],
            _ => return Err(MeshError::BadName(format!("not a Herdr pane or tab id: {target}"))),
        };
        self.run(&args).await.map(drop)
    }

    /// The user's `/peers stop` (#74), never the model's: asks the worker to leave as its user would (`/exit` for a
    /// Marv; ctrl+c twice for another agent, which is how Claude Code quits), then closes the pane. Typed past the
    /// prompts-only rule ([`pane_prompt`]) because a person asked for it, not a model.
    pub async fn stop(&self, pane: &str, marv: bool) -> crate::Result<()> {
        if marv {
            let _ = self.run(&["pane", "send-text", pane, "/exit"]).await;
            tokio::time::sleep(ENTER_DELAY).await;
            let _ = self.run(&["pane", "send-keys", pane, "enter"]).await;
        } else {
            for _ in 0..2 {
                let _ = self.run(&["pane", "send-keys", pane, "ctrl+c"]).await;
                tokio::time::sleep(ENTER_DELAY).await;
            }
        }
        tokio::time::sleep(STOP_GRACE).await;
        self.run(&["pane", "close", pane]).await.map(drop)
    }
}

/// How long a stopped worker gets to leave before its pane is closed.
const STOP_GRACE: Duration = Duration::from_millis(1500);

/// What an id names, by its shape: `w1` a workspace, `w1:t2` a tab, `w1:p3` a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    Workspace,
    Tab,
    Pane,
}

pub fn target_kind(id: &str) -> Option<TargetKind> {
    if !plain_id(id) {
        return None;
    }
    match id.split_once(':') {
        None => Some(TargetKind::Workspace),
        Some((_, rest)) if rest.starts_with('t') => Some(TargetKind::Tab),
        Some((_, rest)) if rest.starts_with('p') => Some(TargetKind::Pane),
        _ => None,
    }
}

/// The new pane's id in a creation answer (`pane split`: `.pane`; `tab create`, `worktree create`: `.root_pane`).
fn new_pane_id(result: &Value) -> Option<String> {
    ["/pane/pane_id", "/root_pane/pane_id", "/workspace/root_pane/pane_id"]
        .iter()
        .find_map(|p| result.pointer(p).and_then(Value::as_str))
        .filter(|id| plain_id(id))
        .map(str::to_string)
}

/// The characters that make an agent's input box read a line as a command rather than a prompt: `/` (a slash
/// command, in Marv and Claude Code alike), `!` (Claude Code's shell mode), `#` (Claude Code's memory).
const COMMAND_PREFIXES: [char; 3] = ['/', '!', '#'];

/// The text a pane send types, or a refusal. Newlines, tabs and returns become spaces, other control characters go
/// (no escape sequences, no early Enter), and the ends are trimmed. A pane send is a prompt, not a command: text
/// whose first visible character is `/`, `!` or `#` is refused whole, not stripped, so nothing is typed. Checked
/// by [`HerdrAdapter::send_text`] and, before any wire is asked for, by `tools::check`.
pub fn pane_prompt(text: &str) -> crate::Result<String> {
    let text: String =
        text.chars().map(|c| if c == '\n' || c == '\t' || c == '\r' { ' ' } else { c }).filter(|c| !c.is_control()).collect();
    let text = text.trim();
    // Zero-width and other invisible characters don't hide a command: the first visible one decides.
    let invisible = |c: &char| {
        c.is_whitespace() || matches!(c, '\u{00AD}' | '\u{180E}' | '\u{200B}'..='\u{200F}' | '\u{2060}'..='\u{2064}' | '\u{FEFF}')
    };
    let Some(first) = text.chars().find(|c| !invisible(c)) else {
        return Err(MeshError::BadName("herdr.pane.send needs some text".into()));
    };
    if COMMAND_PREFIXES.contains(&first) {
        return Err(MeshError::BadName(
            "herdr.pane.send types a prompt, not a command: text starting with / ! or # is refused; nothing was typed".into(),
        ));
    }
    Ok(text.to_string())
}

/// Finds `herdr.pane.<pane>.<action>` receptacles on demand (a pane opened after Marv started).
pub struct HerdrPanes(pub Arc<HerdrAdapter>);

#[async_trait]
impl Resolve for HerdrPanes {
    async fn resolve(&self, name: &ReceptacleName) -> Option<(Receptacle, Arc<dyn Service>)> {
        let (pane, action) = parse_pane_receptacle(name)?;
        let found = self.0.pane(&pane).await.ok()?;
        if !self.0.offerable(&found) {
            return None;
        }
        let receptacle = HerdrAdapter::receptacle(&found, action)?;
        Some((receptacle, Arc::new(PaneService { adapter: self.0.clone(), pane, action })))
    }
}

/// Serves one pane receptacle.
pub struct PaneService {
    pub adapter: Arc<HerdrAdapter>,
    pub pane: String,
    pub action: PaneAction,
}

#[async_trait]
impl Service for PaneService {
    async fn deliver(&self, envelope: &Envelope) -> crate::Result<Option<Value>> {
        let answer = match self.action {
            PaneAction::Send => self.adapter.send_text(&self.pane, envelope.send_text()?).await?,
            PaneAction::Status => self.adapter.status(&self.pane, envelope.body.get("lines").and_then(Value::as_u64)).await?,
        };
        Ok(Some(Value::String(answer)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Fake {
        sent: Mutex<Vec<Value>>,
        answer: bool,
    }
    #[async_trait]
    impl HerdrSend for Fake {
        async fn send(&self, request: &Value, _timeout: Duration) -> bool {
            self.sent.lock().unwrap().push(request.clone());
            self.answer
        }
    }

    fn target() -> HerdrTarget {
        HerdrTarget { socket_path: "/x".into(), pane_id: "p1".into() }
    }

    #[test]
    fn target_needs_all_three_variables() {
        let env = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<marv_mcp::Env>();
        assert_eq!(
            herdr_target(&env(&[("HERDR_ENV", "1"), ("HERDR_SOCKET_PATH", "/s"), ("HERDR_PANE_ID", "p")])).map(|t| t.pane_id),
            Some("p".into())
        );
        assert!(herdr_target(&env(&[("HERDR_ENV", "0"), ("HERDR_SOCKET_PATH", "/s"), ("HERDR_PANE_ID", "p")])).is_none());
        assert!(herdr_target(&env(&[("HERDR_ENV", "1"), ("HERDR_PANE_ID", "p")])).is_none());
    }

    #[test]
    fn requests_speak_herdrs_protocol() {
        let r =
            build_request(&target(), "pane.report_agent", [("state".to_string(), Value::from("idle"))].into_iter().collect(), 7);
        assert_eq!(
            r,
            json!({"id": "herdr:marv:7", "method": "pane.report_agent", "params": {"pane_id": "p1", "source": "herdr:marv", "agent": "marv", "seq": 7, "state": "idle"}})
        );
    }

    #[tokio::test]
    async fn repeats_are_skipped_and_a_silent_herdr_gets_one_retry() {
        let fake = Arc::new(Fake { answer: true, ..Default::default() });
        let r = HerdrReporter::new(target(), Some(fake.clone()));
        r.state(HerdrState::Working, None);
        r.settled().await;
        r.state(HerdrState::Working, None);
        r.settled().await;
        assert_eq!(fake.sent.lock().unwrap().len(), 1);
        r.session("abc", Some("/s.json")).await;
        r.state(HerdrState::Blocked, Some("approve bash"));
        r.settled().await;
        let sent = fake.sent.lock().unwrap().clone();
        let last = &sent.last().unwrap()["params"];
        assert_eq!(last["state"], "blocked");
        assert_eq!(last["agent_session_id"], "abc");
        let seqs: Vec<u64> = sent.iter().map(|s| s["params"]["seq"].as_u64().unwrap()).collect();
        assert!(seqs.windows(2).all(|w| w[0] < w[1]), "seq rises");

        let silent = Arc::new(Fake::default());
        let r = HerdrReporter::new(target(), Some(silent.clone()));
        r.release().await;
        assert_eq!(silent.sent.lock().unwrap().len(), 2);
    }

    /// A fake `herdr` on disk: canned answers, every command line appended to `log`.
    struct FakeHerdr {
        dir: crate::switchboard::tests::Runtime,
    }
    impl FakeHerdr {
        fn new() -> Self {
            use std::os::unix::fs::PermissionsExt;
            let dir = crate::switchboard::tests::Runtime::new();
            std::fs::create_dir_all(&dir.0).unwrap();
            let log = dir.0.join("log");
            let pane = |id: &str, agent: Option<&str>, status: &str| {
                let mut p = json!({"pane_id": id, "agent_status": status, "cwd": "/w"});
                if let Some(a) = agent {
                    p["agent"] = a.into();
                }
                p
            };
            let panes = [
                pane("w1:p1", Some("marv"), "working"),
                pane("w1:p2", Some("claude"), "idle"),
                pane("w1:p3", Some("marv"), "blocked"),
                pane("w1:p4", None, "unknown"),
            ];
            let agents = json!({"result": {"agents": panes.iter().filter(|p| p.get("agent").is_some()).collect::<Vec<_>>()}});
            let get = |p: &Value| {
                format!("'{}') printf '%s\\n' '{}';;", p["pane_id"].as_str().unwrap(), json!({"result": {"pane": p}}))
            };
            // printf, not echo: dash (Ubuntu's sh) turns the \n in the JSON into a newline.
            let script = format!(
                "#!/bin/sh\n[ \"$1\" = --ready ] && exit 0\necho \"$@\" >> '{log}'\ncase \"$1 $2\" in\n\
                 'agent list') printf '%s\\n' '{agents}';;\n\
                 'pane get') case \"$3\" in\n{gets}\n*) printf '%s\\n' '{{\"error\":{{\"code\":\"pane_not_found\",\"message\":\"pane $3 not found\"}}}}'; exit 1;;\nesac;;\n\
                 'pane read') printf '%s\\n' '{{\"result\":{{\"text\":\"line1\\nline2\"}}}}';;\n\
                 *) printf '%s\\n' '{{\"result\":{{}}}}';;\nesac\n",
                log = log.display(),
                gets = panes.iter().map(get).collect::<Vec<_>>().join("\n"),
            );
            let program = dir.0.join("herdr");
            std::fs::write(&program, script).unwrap();
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
            wait_until_runnable(&program);
            Self { dir }
        }
        fn adapter(&self) -> Arc<HerdrAdapter> {
            Arc::new(HerdrAdapter::new(self.dir.0.join("herdr"), Some("w1:p1".into())))
        }
        fn log(&self) -> String {
            std::fs::read_to_string(self.dir.0.join("log")).unwrap_or_default()
        }
    }

    /// Waits until a script just written can be run (#92). While `std::fs::write` had it open, another test's thread
    /// may have forked to spawn its own child; that child holds a copy of the write handle until it execs, and Linux
    /// refuses to run a file open for writing ("Text file busy", ETXTBSY). Under load that gap is long enough for the
    /// adapter's first `herdr` to fail. No new holder can appear once the write is closed, so the first run that
    /// starts means every later one will too.
    fn wait_until_runnable(program: &std::path::Path) {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            match std::process::Command::new(program).arg("--ready").status() {
                Ok(status) => return assert!(status.success(), "fake herdr --ready: {status}"),
                Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(e) => panic!("fake herdr didn't run: {e}"),
            }
        }
    }

    #[test]
    fn pane_ids_become_name_segments_and_back() {
        assert_eq!(pane_segment("w2T:p1").as_deref(), Some("w2_t-p1"));
        assert_eq!(pane_segment("wZ:p1").as_deref(), Some("w_z-p1"));
        for id in ["w2T:p1", "wZ:p1", "w1:p12"] {
            assert_eq!(pane_from_segment(&pane_segment(id).unwrap()).as_deref(), Some(id));
        }
        for bad in ["a-b", "a_b", "w1 p2", "", "w1:p2/x"] {
            assert_eq!(pane_segment(bad), None, "{bad}");
        }
        let send = pane_receptacle("w2T:p1", PaneAction::Send).unwrap();
        assert_eq!(send.as_str(), "herdr.pane.w2_t-p1.send");
        assert_eq!(parse_pane_receptacle(&send), Some(("w2T:p1".to_string(), PaneAction::Send)));
        assert_eq!(parse_pane_receptacle(&ReceptacleName::new("herdr.pane.w1-p2.kill").unwrap()), None);
    }

    #[tokio::test]
    async fn agent_panes_are_offered_but_never_marvs_own_or_a_shell() {
        let fake = FakeHerdr::new();
        let names: Vec<String> = fake.adapter().receptacles().await.unwrap().into_iter().map(|r| r.name.to_string()).collect();
        assert_eq!(
            names,
            ["herdr.pane.w1-p2.status", "herdr.pane.w1-p2.send", "herdr.pane.w1-p3.status", "herdr.pane.w1-p3.send"]
        );
        let panes = HerdrPanes(fake.adapter());
        let name = |n: &str| ReceptacleName::new(n).unwrap();
        assert!(panes.resolve(&name("herdr.pane.w1-p2.send")).await.is_some());
        for absent in ["herdr.pane.w1-p1.send", "herdr.pane.w1-p4.send", "herdr.pane.w9-p9.status"] {
            assert!(panes.resolve(&name(absent)).await.is_none(), "{absent}");
        }
        assert!(!fake.log().contains("send-text"), "finding panes types nothing");
        let listed: Vec<String> = fake.adapter().wireable_panes().await.into_iter().map(|p| p.pane_id).collect();
        assert_eq!(listed, ["w1:p2", "w1:p3"]);
    }

    /// Runs a fake `herdr` just written (`herdr probe`) until Linux lets it run: another test's forked child can hold
    /// its write handle for a moment, and running it then fails with "Text file busy" (#92).
    fn wait_to_run(program: &std::path::Path) {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            match std::process::Command::new(program).arg("probe").output() {
                Ok(_) => return,
                Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(e) => panic!("fake herdr didn't run: {e}"),
            }
        }
    }

    #[tokio::test]
    async fn a_slow_or_missing_herdr_lists_no_panes_in_bounded_time() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::switchboard::tests::Runtime::new();
        std::fs::create_dir_all(&dir.0).unwrap();
        let program = dir.0.join("herdr");
        std::fs::write(&program, "#!/bin/sh\n[ \"$1\" = probe ] && exit 0\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        wait_to_run(&program);
        let started = std::time::Instant::now();
        assert!(HerdrAdapter::new(&program, None).wireable_panes().await.is_empty());
        let took = started.elapsed();
        assert!(took >= LIST_TIMEOUT && took < LIST_TIMEOUT + Duration::from_secs(2), "{took:?}");
        assert!(HerdrAdapter::new("/nonexistent/herdr", None).wireable_panes().await.is_empty());
    }

    #[tokio::test]
    async fn a_switchboard_given_herdr_finds_panes_and_lists_them() {
        use crate::switchboard::{NoTrajectory, Switchboard};
        let fake = FakeHerdr::new();
        wait_to_run(&fake.dir.0.join("herdr"));
        let runtime = crate::switchboard::tests::Runtime::new();
        let sb = Switchboard::new("s", runtime.0.clone(), Arc::new(NoTrajectory));
        assert!(sb.herdr().is_none());
        sb.set_herdr(fake.adapter());
        assert!(sb.lookup(&ReceptacleName::new("herdr.pane.w1-p2.status").unwrap()).await.is_some());
        assert_eq!(sb.herdr().unwrap().wireable_panes().await.len(), 2);
    }

    #[tokio::test]
    async fn a_wired_send_types_clean_text_into_an_idle_agent_only() {
        use crate::switchboard::{NoTrajectory, Switchboard};
        use crate::wire::{Approver, Grant, Wire};
        let fake = FakeHerdr::new();
        let runtime = crate::switchboard::tests::Runtime::new();
        let sb = Arc::new(Switchboard::new("s", runtime.0.clone(), Arc::new(NoTrajectory)));
        sb.add_resolver(Arc::new(HerdrPanes(fake.adapter())));
        let worker = crate::PlugId::new("worker-1").unwrap();
        sb.plug(worker.clone()).unwrap();
        let send = ReceptacleName::new("herdr.pane.w1-p2.send").unwrap();
        assert_eq!(sb.lookup(&send).await.unwrap().modes, [WireMode::Call]);
        // Unwired: refused before Herdr hears anything.
        assert!(sb.deliver(&worker, send.as_str(), WireMode::Call, json!({"text": "hi"})).await.is_err());
        assert!(!fake.log().contains("send-text"));
        sb.wire(Wire::new(worker.clone(), send.clone(), WireMode::Call, Grant::local(Approver::Tui))).unwrap();
        let answer = sb.deliver(&worker, send.as_str(), WireMode::Call, json!({"text": "fix it\nnow\u{1b}[2J"})).await.unwrap();
        assert_eq!(answer, Some(Value::from("Typed into Herdr pane w1:p2 and pressed Enter.")));
        let log = fake.log();
        assert!(log.contains("pane send-text w1:p2 fix it now[2J\n"), "{log}");
        assert!(log.contains("pane send-keys w1:p2 enter\n"), "{log}");
        // A blocked agent would take the Enter as an approval's answer: nothing is typed.
        let blocked = ReceptacleName::new("herdr.pane.w1-p3.send").unwrap();
        sb.lookup(&blocked).await.unwrap();
        sb.wire(Wire::new(worker.clone(), blocked.clone(), WireMode::Call, Grant::local(Approver::Tui))).unwrap();
        let err = sb.deliver(&worker, blocked.as_str(), WireMode::Call, json!({"text": "y"})).await.unwrap_err();
        assert!(err.to_string().contains("is blocked"), "{err}");
        assert!(!fake.log().contains("send-text w1:p3"));
        // Status, with the pane's last lines.
        let status = ReceptacleName::new("herdr.pane.w1-p3.status").unwrap();
        sb.lookup(&status).await.unwrap();
        sb.wire(Wire::new(worker.clone(), status.clone(), WireMode::Call, Grant::local(Approver::Tui))).unwrap();
        let answer = sb.deliver(&worker, status.as_str(), WireMode::Call, json!({"lines": 5})).await.unwrap().unwrap();
        let answer: Value = serde_json::from_str(answer.as_str().unwrap()).unwrap();
        assert_eq!((answer["agent"].as_str(), answer["state"].as_str()), (Some("marv"), Some("blocked")));
        assert_eq!(answer["output"], "line1\nline2");
        assert!(fake.log().contains("pane read w1:p3 --source recent --lines 5 --format text"));
    }

    /// #123: a model sent `{"prompt": …}` and the pane got the JSON typed in. A body that isn't text is refused, with
    /// the shape it needs, and nothing reaches the pane.
    #[tokio::test]
    async fn a_send_that_isnt_text_is_refused_and_nothing_is_typed() {
        use crate::switchboard::{NoTrajectory, Switchboard};
        use crate::wire::{Approver, Grant, Wire};
        let fake = FakeHerdr::new();
        let runtime = crate::switchboard::tests::Runtime::new();
        let sb = Arc::new(Switchboard::new("s", runtime.0.clone(), Arc::new(NoTrajectory)));
        sb.add_resolver(Arc::new(HerdrPanes(fake.adapter())));
        let worker = crate::PlugId::new("worker-1").unwrap();
        sb.plug(worker.clone()).unwrap();
        let send = ReceptacleName::new("herdr.pane.w1-p2.send").unwrap();
        sb.lookup(&send).await.unwrap();
        sb.wire(Wire::new(worker.clone(), send.clone(), WireMode::Call, Grant::local(Approver::Tui))).unwrap();
        for body in [
            json!({"prompt": "Please start hack.py"}),
            json!({"text": "hi", "prompt": "and this"}),
            json!({"text": 5}),
            json!({}),
            json!(["hi"]),
            json!(7),
        ] {
            let err = sb.deliver(&worker, send.as_str(), WireMode::Call, body.clone()).await.unwrap_err().to_string();
            assert!(err.contains(r#"expected {"text": "..."}"#), "{body}: {err}");
            assert!(!err.contains("hack.py") && !err.contains("and this"), "the body isn't echoed: {err}");
        }
        assert!(!fake.log().contains("send-text"), "nothing typed: {}", fake.log());
        // A plain string or {"text": "…"} is typed.
        sb.deliver(&worker, send.as_str(), WireMode::Call, json!("hello")).await.unwrap();
        assert!(fake.log().contains("pane send-text w1:p2 hello\n"), "{}", fake.log());
    }

    /// A pane send is a prompt, not a command: text whose first visible character is `/`, `!` or `#` is refused
    /// whole (not stripped), and nothing reaches the pane; a prompt with those characters later on is typed.
    #[tokio::test]
    async fn a_send_that_starts_like_a_command_is_refused_and_nothing_is_typed() {
        use crate::switchboard::{NoTrajectory, Switchboard};
        use crate::wire::{Approver, Grant, Wire};
        let fake = FakeHerdr::new();
        let runtime = crate::switchboard::tests::Runtime::new();
        let sb = Arc::new(Switchboard::new("s", runtime.0.clone(), Arc::new(NoTrajectory)));
        sb.add_resolver(Arc::new(HerdrPanes(fake.adapter())));
        let worker = crate::PlugId::new("worker-1").unwrap();
        sb.plug(worker.clone()).unwrap();
        let send = ReceptacleName::new("herdr.pane.w1-p2.send").unwrap();
        sb.lookup(&send).await.unwrap();
        sb.wire(Wire::new(worker.clone(), send.clone(), WireMode::Call, Grant::local(Approver::Tui))).unwrap();
        for text in ["/help", "!ls", "#remember this", "   /help", "\n\t !ls", "\r# x", "\u{1b}/help", "\u{FEFF}/help"] {
            let err = sb.deliver(&worker, send.as_str(), WireMode::Call, json!({"text": text})).await.unwrap_err();
            assert_eq!(
                err.to_string(),
                "herdr.pane.send types a prompt, not a command: text starting with / ! or # is refused; nothing was typed",
                "{text:?}"
            );
        }
        // The adapter refuses it too, whoever calls it.
        assert!(fake.adapter().send_text("w1:p2", "/help").await.is_err());
        assert!(!fake.log().contains("send-text"), "nothing typed: {}", fake.log());
        assert!(!fake.log().contains("send-keys"), "no Enter pressed: {}", fake.log());
        sb.deliver(&worker, send.as_str(), WireMode::Call, json!({"text": "  rerun the tests # then report"})).await.unwrap();
        assert!(fake.log().contains("pane send-text w1:p2 rerun the tests # then report\n"), "{}", fake.log());
    }

    /// Two sends at once, or one right after another, into the same pane: one is typed, the other is refused before
    /// anything is typed (Herdr may still call the agent idle while it takes the first in).
    #[tokio::test]
    async fn a_pane_just_sent_a_prompt_takes_no_other_for_a_while() {
        let fake = FakeHerdr::new();
        let adapter = fake.adapter();
        let (a, b) = tokio::join!(adapter.send_text("w1:p2", "first"), adapter.send_text("w1:p2", "second"));
        assert_eq!(a.is_ok() as u8 + b.is_ok() as u8, 1, "{a:?} {b:?}");
        let err = a.err().or(b.err()).unwrap().to_string();
        assert!(err.contains("was just sent a prompt") && !err.contains("first") && !err.contains("second"), "{err}");
        assert!(adapter.send_text("w1:p2", "third").await.is_err());
        let log = fake.log();
        assert_eq!(log.matches("send-text").count(), 1, "{log}");
        assert_eq!(log.matches("send-keys").count(), 1, "{log}");
        assert!(!log.contains("third"), "{log}");
    }

    #[test]
    fn a_pane_prompt_is_cleaned_and_a_command_or_nothing_is_refused() {
        assert_eq!(pane_prompt(" fix it\nnow\u{1b}[2J ").unwrap(), "fix it now[2J");
        assert_eq!(pane_prompt("a/b !c #d").unwrap(), "a/b !c #d");
        for command in ["/x", "!x", "#x", " \t/x", "\u{200B}#x"] {
            assert!(pane_prompt(command).unwrap_err().to_string().contains("not a command"), "{command:?}");
        }
        for empty in ["", "  \n", "\u{200B}"] {
            assert_eq!(pane_prompt(empty).unwrap_err().to_string(), "herdr.pane.send needs some text", "{empty:?}");
        }
    }

    #[test]
    fn pane_descriptions_say_status_reads_the_pane_and_a_marv_has_an_inbox() {
        let pane = |agent: &str| HerdrPane { pane_id: "w1:p2".into(), agent: Some(agent.into()), ..HerdrPane::default() };
        let status = HerdrAdapter::receptacle(&pane("claude"), PaneAction::Status).unwrap();
        assert!(status.description.contains("with lines: N"), "{}", status.description);
        assert!(status.description.contains("last N lines"), "{}", status.description);
        let send = HerdrAdapter::receptacle(&pane("claude"), PaneAction::Send).unwrap();
        assert!(send.description.contains(r#"{"text": "..."}"#) && !send.description.contains("inbox"), "{}", send.description);
        assert!(send.description.contains("never a command: text starting with / ! or # is refused"), "{}", send.description);
        assert_eq!(send.input_schema["additionalProperties"], json!(false));
        let marv = HerdrAdapter::receptacle(&pane("marv"), PaneAction::Send).unwrap();
        assert!(marv.description.contains("peer.<marv>.agent.parent.inbox"), "{}", marv.description);
    }

    #[tokio::test]
    async fn herdr_errors_name_the_command_not_herdrs_words() {
        let fake = FakeHerdr::new();
        let err = fake.adapter().status("w9:p9", None).await.unwrap_err();
        assert_eq!(err.to_string(), "herdr pane get: pane_not_found");
        let missing = HerdrAdapter::new("/nonexistent/herdr", None);
        assert!(missing.agents().await.unwrap_err().to_string().starts_with("herdr agent list: "));
    }
}
