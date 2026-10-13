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
/// `herdr.pane.<pane>.status` reads at most this many lines of the pane's output.
pub const MAX_READ_LINES: u64 = 200;
/// Between the text and its Enter: an agent's TUI may still be taking the text in (Herdr's own `agent prompt` waits
/// too), and an Enter that lands first submits half a prompt.
const ENTER_DELAY: Duration = Duration::from_millis(150);

/// What a pane receptacle does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneAction {
    /// `herdr.pane.<pane>.send`: types a prompt into the pane and presses Enter.
    Send,
    /// `herdr.pane.<pane>.status`: the pane's agent and state, optionally its last lines.
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
/// - Commands run from the switchboard, on the host, through the `herdr` CLI (`pane get`, `pane send-text` then
///   `pane send-keys enter`, since Herdr 0.8.2 refuses `agent prompt` for Marv; `pane read`), never from a sandbox.
///   A pane's state and output are the other side's text: answers are `Provenance::Mesh`, fenced like any mesh text.
pub struct HerdrAdapter {
    /// The `herdr` program (tests: a fake script).
    pub program: PathBuf,
    /// The pane Marv runs in (`HERDR_PANE_ID`): never offered.
    pub own_pane: Option<String>,
}

impl HerdrAdapter {
    pub fn new(program: impl Into<PathBuf>, own_pane: Option<String>) -> Self {
        Self { program: program.into(), own_pane }
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
                    "Types a prompt into the {who} in Herdr pane {} and presses Enter, as if the user typed it",
                    pane.pane_id
                ),
                json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}),
            ),
            PaneAction::Status => (
                format!("The state of the {who} in Herdr pane {} (and its last lines, with lines: N)", pane.pane_id),
                json!({"type": "object", "properties": {"lines": {"type": "integer", "minimum": 0, "maximum": MAX_READ_LINES}}}),
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

    /// Every receptacle the adapter offers right now: two per agent pane, Marv's own left out.
    pub async fn receptacles(&self) -> crate::Result<Vec<Receptacle>> {
        let panes = self.agents().await?;
        Ok(panes
            .iter()
            .filter(|p| self.offerable(p))
            .flat_map(|p| [PaneAction::Status, PaneAction::Send].into_iter().filter_map(|a| Self::receptacle(p, a)))
            .collect())
    }

    /// `herdr.pane.<pane>.send`: the text, then Enter, into an agent that's idle or done.
    pub async fn send_text(&self, pane: &str, text: &str) -> crate::Result<String> {
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
        let text: String =
            text.chars().map(|c| if c == '\n' || c == '\t' || c == '\r' { ' ' } else { c }).filter(|c| !c.is_control()).collect();
        let text = text.trim();
        if text.is_empty() {
            return Err(MeshError::BadName("herdr.pane.send needs some text".into()));
        }
        self.run(&["pane", "send-text", pane, text]).await?;
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
            PaneAction::Send => self.adapter.send_text(&self.pane, &envelope.text()).await?,
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
            let get = |p: &Value| format!("'{}') echo '{}';;", p["pane_id"].as_str().unwrap(), json!({"result": {"pane": p}}));
            let script = format!(
                "#!/bin/sh\necho \"$@\" >> '{log}'\ncase \"$1 $2\" in\n\
                 'agent list') echo '{agents}';;\n\
                 'pane get') case \"$3\" in\n{gets}\n*) echo '{{\"error\":{{\"code\":\"pane_not_found\",\"message\":\"pane $3 not found\"}}}}'; exit 1;;\nesac;;\n\
                 'pane read') echo '{{\"result\":{{\"text\":\"line1\\nline2\"}}}}';;\n\
                 *) echo '{{\"result\":{{}}}}';;\nesac\n",
                log = log.display(),
                gets = panes.iter().map(get).collect::<Vec<_>>().join("\n"),
            );
            let program = dir.0.join("herdr");
            std::fs::write(&program, script).unwrap();
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
            Self { dir }
        }
        fn adapter(&self) -> Arc<HerdrAdapter> {
            Arc::new(HerdrAdapter::new(self.dir.0.join("herdr"), Some("w1:p1".into())))
        }
        fn log(&self) -> String {
            std::fs::read_to_string(self.dir.0.join("log")).unwrap_or_default()
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

    #[tokio::test]
    async fn herdr_errors_name_the_command_not_herdrs_words() {
        let fake = FakeHerdr::new();
        let err = fake.adapter().status("w9:p9", None).await.unwrap_err();
        assert_eq!(err.to_string(), "herdr pane get: pane_not_found");
        let missing = HerdrAdapter::new("/nonexistent/herdr", None);
        assert!(missing.agents().await.unwrap_err().to_string().starts_with("herdr agent list: "));
    }
}
