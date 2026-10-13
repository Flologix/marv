//! Herdr: pane status reporting (ports Marv's `src/herdr.ts`) and, for mesh
//! v2, the adapter that offers Herdr panes as receptacles (new).
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

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::watch;

use crate::plug::PlugId;
use crate::receptacle::Receptacle;

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

/// Mesh v2: Herdr panes as receptacles (`herdr.pane.send`, `herdr.pane.status`).
/// Maps pane ids to Marv sessions, replacing hand-typed orchestration with wired
/// inboxes while each worker stays sandboxed. Calls go to Herdr's socket from
/// the switchboard (host side), never from a sandbox.
pub struct HerdrAdapter {
    pub socket_path: String,
    /// Pane id → the Marv session in it (from `herdr agent list` and session reports).
    pub panes: Vec<(String, String)>,
}

impl HerdrAdapter {
    /// The receptacles it offers on the switchboard.
    pub fn receptacles(&self) -> Vec<Receptacle> {
        todo!("port: mesh v2: herdr.pane.send (call: pane, text) and herdr.pane.status (call/watch: pane)")
    }

    /// Refreshes the pane → session map.
    pub async fn refresh(&mut self) -> crate::Result<()> {
        todo!("port: mesh v2: herdr agent list over the socket")
    }

    /// `herdr.pane.send`: text to a pane (`pane send-text`; Herdr refuses `agent prompt` for Marv as of 0.8.2).
    pub async fn send_text(&self, _from: &PlugId, _pane: &str, _text: &str) -> crate::Result<()> {
        todo!("port: mesh v2: pane.send_text")
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
}
