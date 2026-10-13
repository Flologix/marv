//! Trajectories: a record of what every agent run did, for measuring Marv over time and, later, training data.
//! Ports Marv's `src/trajectory.ts`.
//!
//! One append-only JSONL file per session, `~/.marv/trajectories/<project>/<session id>.jsonl` (0600: it holds code
//! and prompts). Each line is one [`TrajectoryRecord`], stamped with the format version, the time and the session. A
//! turn is one user message and everything the agents did until they stopped; subagents' records carry their own
//! agent id (`<turn>.<n>`). Separate from sessions on purpose: a session is the latest state, rewritten after every
//! turn; a trajectory is the history, only appended to, with timings, approvals and subagents' steps.
//!
//! Logging must never break a run: writes are queued in order on a writer thread, the first failure is reported once
//! ([`Trajectory::on_error`]) and the rest are dropped quietly. A crash can leave a half-written last line, so the file
//! is started on a fresh line before the first record of a process.
//!
//! marv-rs additions: `mesh` records (every mesh message, delivered or refused: `marv_mesh::switchboard::MeshRecord`)
//! and `taint` records (what tainted the session, `docs/SECURITY.md`).

use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};

use marv_agent::LoopEvent;
use marv_agent::agent::EmptyNext;
use marv_core::{Provenance, ToolCall, Usage};
use marv_mcp::deferred::ModeChoice;
use marv_mesh::switchboard::{MeshRecord, TrajectorySink};
use serde::Serialize;

use crate::paths::project_key;
use crate::runner::ApprovalRecord;

pub const FORMAT_VERSION: u32 = 1;

/// Who a record is about: the turn, and "main" or a subagent's id (`<turn>.<n>`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Who {
    pub turn: String,
    pub agent: String,
}

/// A request's usage, in Marv's field names (`bun run stats` reads `usage.promptTokens`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageRecord {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
}

impl From<&Usage> for UsageRecord {
    fn from(u: &Usage) -> Self {
        Self {
            prompt_tokens: u.prompt_tokens,
            completion_tokens: u.completion_tokens,
            cached_tokens: u.cached_tokens,
            cost: u.cost,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Done,
    Failed,
    Off,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum TrajectoryRecord {
    /// Once per session file per process (again on resume): what the runs that follow ran with.
    Session {
        root: String,
        marv: String,
        provider: String,
        model: String,
        system: String,
        tools: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        git: Option<String>,
    },
    /// The user's message; `for_model` when the model got something else (a /skill's instructions).
    TurnStart {
        turn: String,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        for_model: Option<String>,
        provider: String,
        model: String,
        yolo: bool,
        sandbox: bool,
        diagnostics: bool,
        checks: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        mcp_tools: Option<ModeChoice>,
    },
    /// Deferred MCP tools (#14): the note at the start of the turn's message.
    McpNote { turn: String, text: String },
    /// One model request: its tokens and how long since the previous step ended.
    Request {
        #[serde(flatten)]
        who: Who,
        step: u32,
        usage: UsageRecord,
        ms: u64,
    },
    /// A finished reply, with the reasoning before it.
    Assistant {
        #[serde(flatten)]
        who: Who,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        thinking: Option<String>,
    },
    /// A tool call and exactly what the model got back. `ms` of a write includes waiting for the typecheck baseline.
    Tool {
        #[serde(flatten)]
        who: Who,
        call: ToolCall,
        output: String,
        summary: String,
        is_error: bool,
        declined: bool,
        approval: ApprovalRecord,
        ms: u64,
    },
    Error {
        #[serde(flatten)]
        who: Who,
        message: String,
    },
    StepLimit {
        #[serde(flatten)]
        who: Who,
        steps: u32,
        continued: bool,
    },
    CutOff {
        #[serde(flatten)]
        who: Who,
        continued: bool,
    },
    TextLoop {
        #[serde(flatten)]
        who: Who,
        continued: bool,
    },
    ToolLoop {
        #[serde(flatten)]
        who: Who,
        continued: bool,
    },
    /// `next`: "retry", "nudge" or "stop".
    EmptyReply {
        #[serde(flatten)]
        who: Who,
        next: &'static str,
    },
    /// A check after a step's file changes: the typecheck, or with `source: "checks"` the project's checks.
    Check {
        #[serde(flatten)]
        who: Who,
        #[serde(skip_serializing_if = "Option::is_none")]
        source: Option<&'static str>,
        status: CheckStatus,
        #[serde(skip_serializing_if = "Option::is_none")]
        ms: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        before: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        errors: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        added: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// Exactly what the model was told.
        #[serde(skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    SubagentStart {
        #[serde(flatten)]
        who: Who,
        subagent: String,
        call: String,
        agent_type: String,
        description: String,
        prompt: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        isolation: Option<String>,
    },
    /// How an agent's run ended (for "main", the turn).
    AgentEnd {
        #[serde(flatten)]
        who: Who,
        reason: String,
        steps: u32,
        tools: u32,
        ms: u64,
    },
    /// The conversation was replaced by a summary.
    Compact {
        #[serde(skip_serializing_if = "Option::is_none")]
        turn: Option<String>,
        summary: String,
    },
    /// 1 good, -1 bad, 0 just labeled.
    Feedback {
        turn: String,
        score: i8,
        source: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        labels: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        phrase: Option<String>,
    },
    /// marv-rs: a mesh message, delivered or refused (never its body).
    Mesh(MeshRecord),
    /// marv-rs: the session became tainted (web, MCP, mesh or image content entered its context).
    Taint {
        #[serde(skip_serializing_if = "Option::is_none")]
        turn: Option<String>,
        source: Provenance,
    },
}

/// Opens trajectories under `dir`.
#[derive(Debug, Clone)]
pub struct TrajectoryStore {
    pub dir: PathBuf,
}

impl TrajectoryStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn open(&self, root: &Path, session: &str) -> Trajectory {
        Trajectory::new(self.dir.join(project_key(root)).join(format!("{session}.jsonl")), session)
    }
}

enum Job {
    Line(String),
    Flush(tokio::sync::oneshot::Sender<()>),
}

pub type OnError = Arc<dyn Fn(String) + Send + Sync>;

/// One session's log. Writes go to a writer thread in order; [`Trajectory::flush`] waits for them.
pub struct Trajectory {
    pub path: PathBuf,
    pub session: String,
    tx: Mutex<Option<mpsc::Sender<Job>>>,
    on_error: Arc<Mutex<Option<OnError>>>,
}

impl Trajectory {
    pub fn new(path: PathBuf, session: &str) -> Self {
        Self { path, session: session.to_string(), tx: Mutex::new(None), on_error: Arc::default() }
    }

    /// Where the first failure is reported (once); later ones are dropped quietly.
    pub fn on_error(&self, report: OnError) {
        *self.on_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(report);
    }

    fn sender(&self) -> mpsc::Sender<Job> {
        let mut tx = self.tx.lock().unwrap_or_else(|e| e.into_inner());
        tx.get_or_insert_with(|| {
            let (tx, rx) = mpsc::channel::<Job>();
            let path = self.path.clone();
            let on_error = self.on_error.clone();
            std::thread::spawn(move || writer(path, rx, on_error));
            tx
        })
        .clone()
    }

    /// Appends a record, stamped `{v, t, session}`.
    pub fn write(&self, record: &TrajectoryRecord) {
        let Ok(serde_json::Value::Object(fields)) = serde_json::to_value(record) else {
            return;
        };
        let mut line = serde_json::Map::new();
        line.insert("v".into(), FORMAT_VERSION.into());
        line.insert("t".into(), crate::time::iso(crate::time::now_ms()).into());
        line.insert("session".into(), self.session.clone().into());
        line.extend(fields);
        let _ = self.sender().send(Job::Line(format!("{}\n", serde_json::Value::Object(line))));
    }

    /// Resolves once everything written so far is on disk.
    pub async fn flush(&self) {
        let (done, wait) = tokio::sync::oneshot::channel();
        if self.sender().send(Job::Flush(done)).is_ok() {
            let _ = wait.await;
        }
    }
}

fn writer(path: PathBuf, rx: mpsc::Receiver<Job>, on_error: Arc<Mutex<Option<OnError>>>) {
    let mut file: Option<std::fs::File> = None;
    let mut failed = false;
    for job in rx {
        match job {
            Job::Flush(done) => {
                let _ = done.send(());
            }
            Job::Line(_) if failed => {}
            Job::Line(line) => {
                let result = (|| -> std::io::Result<()> {
                    if file.is_none() {
                        file = Some(open_log(&path)?);
                    }
                    file.as_mut().expect("opened").write_all(line.as_bytes())
                })();
                if let Err(err) = result {
                    failed = true;
                    if let Some(report) = on_error.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                        report(format!("Couldn't write the trajectory log ({err}); this session won't be logged."));
                    }
                }
            }
        }
    }
}

/// Opens for append, 0600 in a 0700 folder, starting on a fresh line if a crash left a half-written one.
fn open_log(path: &Path) -> std::io::Result<std::fs::File> {
    use std::io::{Read, Seek, SeekFrom};
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    let mut file = std::fs::OpenOptions::new().read(true).append(true).create(true).mode(0o600).open(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    let len = file.metadata()?.len();
    if len > 0 {
        let mut last = [0u8; 1];
        file.seek(SeekFrom::Start(len - 1))?;
        file.read_exact(&mut last)?;
        if last[0] != b'\n' {
            file.write_all(b"\n")?;
        }
    }
    Ok(file)
}

/// The switchboard logs every mesh message here (docs/MESH.md rule 3).
pub struct MeshSink(pub Arc<Trajectory>);

impl TrajectorySink for MeshSink {
    fn mesh(&self, record: MeshRecord) {
        self.0.write(&TrajectoryRecord::Mesh(record));
    }
}

/// Turns one agent's loop events into records, plus timings. One per agent per turn.
pub struct AgentRecorder {
    sink: Box<dyn Fn(TrajectoryRecord) + Send + Sync>,
    who: Who,
    now: Box<dyn Fn() -> u64 + Send + Sync>,
    started: u64,
    step_start: u64,
    thinking: String,
    steps: u32,
    tools: u32,
    tool_starts: std::collections::HashMap<String, u64>,
    ended: bool,
}

impl AgentRecorder {
    pub fn new(
        sink: impl Fn(TrajectoryRecord) + Send + Sync + 'static,
        who: Who,
        now: impl Fn() -> u64 + Send + Sync + 'static,
    ) -> Self {
        let start = now();
        Self {
            sink: Box::new(sink),
            who,
            now: Box::new(now),
            started: start,
            step_start: start,
            thinking: String::new(),
            steps: 0,
            tools: 0,
            tool_starts: Default::default(),
            ended: false,
        }
    }

    /// The step-end typecheck isn't the model's time: the next request's clock starts after it.
    pub fn restart_clock(&mut self) {
        self.step_start = (self.now)();
    }

    fn take_thinking(&mut self) -> Option<String> {
        (!self.thinking.is_empty()).then(|| std::mem::take(&mut self.thinking))
    }

    pub fn event(&mut self, event: &LoopEvent) {
        if self.ended {
            return;
        }
        let who = self.who.clone();
        let now = (self.now)();
        match event {
            LoopEvent::ThinkingDelta(text) => self.thinking.push_str(text),
            LoopEvent::Usage(usage) => {
                self.steps += 1;
                (self.sink)(TrajectoryRecord::Request { who, step: self.steps, usage: usage.into(), ms: now - self.step_start });
            }
            LoopEvent::Assistant { text } => {
                let thinking = self.take_thinking();
                (self.sink)(TrajectoryRecord::Assistant { who, text: text.clone(), thinking });
            }
            LoopEvent::ToolStart { call, .. } => {
                // Reasoning that led straight to a tool call, with no text.
                if let Some(thinking) = self.take_thinking() {
                    (self.sink)(TrajectoryRecord::Assistant { who, text: String::new(), thinking: Some(thinking) });
                }
                self.tools += 1;
                self.tool_starts.insert(call.id.clone(), now);
            }
            LoopEvent::ToolEnd { call, run } => {
                let started = self.tool_starts.remove(&call.id).unwrap_or(now);
                let approval = run.approval;
                (self.sink)(TrajectoryRecord::Tool {
                    who,
                    call: call.clone(),
                    output: run.result.output.clone(),
                    summary: run.result.summary.clone().unwrap_or_default(),
                    is_error: run.result.is_error,
                    declined: run.declined,
                    approval,
                    ms: now - started,
                });
                // The next request goes out once the last tool is done.
                self.step_start = now;
            }
            // Never ran, but the model got this as its result: a trajectory must hold every result it saw.
            LoopEvent::ToolSkipped { call, output } => (self.sink)(TrajectoryRecord::Tool {
                who,
                call: call.clone(),
                output: output.clone(),
                summary: "not run".into(),
                is_error: false,
                declined: true,
                approval: ApprovalRecord::None,
                ms: 0,
            }),
            LoopEvent::Error(message) => (self.sink)(TrajectoryRecord::Error { who, message: message.clone() }),
            LoopEvent::StepLimit { steps, continued } => {
                (self.sink)(TrajectoryRecord::StepLimit { who, steps: *steps, continued: *continued })
            }
            LoopEvent::CutOff { continued } => (self.sink)(TrajectoryRecord::CutOff { who, continued: *continued }),
            LoopEvent::TextLoop { continued } => (self.sink)(TrajectoryRecord::TextLoop { who, continued: *continued }),
            LoopEvent::ToolLoop { continued } => (self.sink)(TrajectoryRecord::ToolLoop { who, continued: *continued }),
            LoopEvent::EmptyReply { next } => {
                let next = match next {
                    EmptyNext::Retry => "retry",
                    EmptyNext::Nudge => "nudge",
                    EmptyNext::Stop => "stop",
                };
                (self.sink)(TrajectoryRecord::EmptyReply { who, next });
            }
            LoopEvent::Done(reason) => {
                let reason = serde_json::to_value(reason).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
                self.finish(&reason);
            }
            // The whole reply comes with Assistant; deltas, retries and compactions aren't records.
            LoopEvent::TextDelta(_) | LoopEvent::ToolCallDelta { .. } | LoopEvent::Compacted | LoopEvent::Retry { .. } => {}
        }
    }

    /// Records how the run ended (only the first call counts: a run that threw never sends Done).
    pub fn finish(&mut self, reason: &str) {
        if self.ended {
            return;
        }
        self.ended = true;
        if let Some(thinking) = self.take_thinking() {
            (self.sink)(TrajectoryRecord::Assistant { who: self.who.clone(), text: String::new(), thinking: Some(thinking) });
        }
        let ms = (self.now)() - self.started;
        (self.sink)(TrajectoryRecord::AgentEnd {
            who: self.who.clone(),
            reason: reason.into(),
            steps: self.steps,
            tools: self.tools,
            ms,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marv_core::{StopReason, ToolResult};

    fn recorder() -> (AgentRecorder, Arc<Mutex<Vec<TrajectoryRecord>>>) {
        let out = Arc::new(Mutex::new(Vec::new()));
        let sink = out.clone();
        let rec = AgentRecorder::new(move |r| sink.lock().unwrap().push(r), Who { turn: "t".into(), agent: "main".into() }, || 0);
        (rec, out)
    }

    #[test]
    fn records_requests_replies_tools_and_the_end() {
        let (mut rec, out) = recorder();
        let call = ToolCall { id: "c1".into(), name: "read_file".into(), arguments: "{}".into() };
        rec.event(&LoopEvent::ThinkingDelta("hmm".into()));
        rec.event(&LoopEvent::Usage(Usage { prompt_tokens: 10, completion_tokens: 2, cached_tokens: None, cost: None }));
        rec.event(&LoopEvent::ToolStart { call: call.clone(), label: "a".into() });
        rec.event(&LoopEvent::ToolEnd {
            call,
            run: marv_agent::ToolRun::new(
                ToolResult { output: "x".into(), summary: Some("1 line".into()), is_error: false },
                "a",
            ),
        });
        rec.event(&LoopEvent::Done(StopReason::End));
        rec.event(&LoopEvent::Error("late".into())); // after the end: ignored
        let records = out.lock().unwrap();
        let types: Vec<String> =
            records.iter().map(|r| serde_json::to_value(r).unwrap()["type"].as_str().unwrap().to_string()).collect();
        assert_eq!(types, vec!["request", "assistant", "tool", "agent_end"]);
        let tool = serde_json::to_value(&records[2]).unwrap();
        assert_eq!(tool["approval"], "none");
        assert_eq!(tool["isError"], false);
        assert_eq!(tool["agent"], "main");
        assert_eq!(serde_json::to_value(&records[3]).unwrap()["reason"], "end");
    }

    #[tokio::test]
    async fn writes_stamped_lines_privately() {
        let dir = std::env::temp_dir().join(format!("marv-traj-{}-{}", std::process::id(), marv_fs::random_hex()));
        let log = TrajectoryStore::new(&dir).open(Path::new("/p"), "s1");
        std::fs::create_dir_all(log.path.parent().unwrap()).unwrap();
        std::fs::write(&log.path, "{\"half").unwrap(); // a crash's leftovers
        log.write(&TrajectoryRecord::McpNote { turn: "t".into(), text: "x".into() });
        log.flush().await;
        let text = std::fs::read_to_string(&log.path).unwrap();
        let last: serde_json::Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(
            (last["v"].as_u64(), last["session"].as_str(), last["type"].as_str()),
            (Some(1), Some("s1"), Some("mcp_note"))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
