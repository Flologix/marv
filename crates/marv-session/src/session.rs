//! The headless Marv session: everything that turns the agent loop into Marv, with no UI. Ports Marv's
//! `src/session.ts` (`MarvSession`, which implements the SDK's `Session`).
//!
//! It holds the conversation (what the model sees), builds the system prompt and tool list once per conversation,
//! waits for MCP servers, compacts when the context is nearly full, starts subagents, records trajectories, saves the
//! session, and hands each turn back as a stream of [`SessionEvent`]s. The TUI is one client; headless `marv -p` and
//! the SDK ([`crate::sdk`]) are others. It never imports the TUI.
//!
//! Rules carried over from Marv, each a bug that was really hit:
//! - **Prompt cache:** the history is only appended to (compaction is the one exception), and the system prompt and
//!   tool specs stay the same for a whole conversation.
//! - **One turn at a time:** `send()` and `clear()` fail at once during a turn; `resume()` and `compact()` too.
//! - **A turn doesn't depend on being read:** its work runs on its own task and pushes into an [`EventQueue`]; one
//!   nobody reads still runs to the end. `TurnStart` first and `TurnEnd` last, exactly once however it ends
//!   (`Interrupted` when stopped before the loop began: waiting for MCP, or an automatic compaction).
//! - **Dropping a [`Turn`] interrupts it** (Marv: leaving the `for await` early); [`Turn::finish`] also waits for
//!   its cleanup, so every tool call has a result before the client goes on.
//! - The session is free (`busy() == false`) right before `TurnEnd`, so a client can send the next message as soon as
//!   it sees it; events after `TurnEnd` are dropped.
//! - A turn reads all its settings when it starts; `configure()` applies from the next turn and is all or nothing.
//! - Saved 200 ms after each turn, and by `save()`/`flush()`; `clear()`/`resume()` save the previous one first.
//!
//! marv-rs additions (docs/SECURITY.md, docs/MESH.md):
//! - The session owns a [`TaintState`]: shared with every tool context; [`SessionEvent::Tainted`] tells the client
//!   (status bar) and a `taint` trajectory record logs it. Tainted egress asks even in yolo and even after "always"
//!   ([`crate::runner::SessionApprover`]).
//! - It owns the process's MCP manager ([`McpHandle`], when `owns_mcp`) and mesh switchboard ([`MeshHandle`]).
//! - The typecheck's and project checks' notes are fenced as project data, never in Marv's voice.

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures::Stream;
use marv_agent::LoopEvent;
use marv_core::{ChatTurn, Provenance, StopReason, Usage};
use marv_mcp::deferred::{DEFERRED_NAMES, McpToolsSetting};
use marv_provider::{ModelInfo, Provider, ProviderFactory, ProviderOption};
use marv_sandbox::TaintState;
use marv_tools::{AgentProgress, Approver, ToolRef};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_util::sync::CancellationToken;

use crate::agents::{AgentType, general_purpose};
use crate::checks::{ProjectCheckResult, Violation};
use crate::diagnostics::turn::Checker;
use crate::diagnostics::{CheckResult, TsError};
use crate::event_queue::EventQueue;
use crate::mcp_tools::McpHandle;
use crate::memory::{Memories, MemoryPaths};
use crate::mesh::{MeshHandle, WireApprover};
use crate::prompt::{Listed, PromptInput, system_prompt};
use crate::runner::AlwaysScopes;
use crate::sessions::{McpListing, SavedSession, SessionStore, new_session};
use crate::skills::LoadedSkill;
use crate::trajectory::{Trajectory, TrajectoryRecord, TrajectoryStore};
use crate::types::{Message, Role};
use crate::usage::{Prices, Totals, add_usage};

/// Saved this long after a turn ends (and again on flush), so a burst of turns is one write.
pub const SAVE_DELAY: std::time::Duration = std::time::Duration::from_millis(200);
pub const BUSY: &str = "Marv is working on a turn: wait for it to end, or interrupt() it, first.";
pub const CLOSED: &str = "This session is closed.";

/// What a turn is doing between the model's replies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnStatus {
    /// MCP servers are still starting: the tool list can't change after the first request (prompt cache).
    WaitingForMcp,
    Compacting,
    /// A step's typecheck or project checks are running.
    Checking,
    /// The message is sent (again after each `Checking`).
    Running,
}

/// How a turn ended: how the agent loop ended, or `Interrupted` when it was stopped before the loop began.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnEndReason {
    Done(StopReason),
    Interrupted,
}

impl TurnEndReason {
    /// Marv's names: "end", "max_steps", …, "interrupted".
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Interrupted => "interrupted",
            Self::Done(StopReason::End) => "end",
            Self::Done(StopReason::Length) => "length",
            Self::Done(StopReason::Aborted) => "aborted",
            Self::Done(StopReason::Declined) => "declined",
            Self::Done(StopReason::MaxSteps) => "max_steps",
            Self::Done(StopReason::Error) => "error",
            Self::Done(StopReason::Loop) => "loop",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactFailure {
    /// Nothing to compact, or the context isn't nearly full.
    Empty,
    Stopped,
    Failed,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CompactResult {
    Compacted { summary: String, tokens_before: Option<u64> },
    NotCompacted { reason: CompactFailure, error: String },
}

/// What a turn yields: the UI-facing event type the TUI and headless consume.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// Always first.
    TurnStart {
        turn: String,
    },
    Status(TurnStatus),
    /// The main agent, exactly as `marv_agent::run_agent` yields them.
    Loop(LoopEvent),
    /// A subagent's loop event; `call_id` is the `agent` tool call that started it.
    Subagent {
        call_id: String,
        event: LoopEvent,
    },
    SubagentProgress {
        call_id: String,
        progress: AgentProgress,
    },
    /// An automatic compaction (the context was nearly full), and how it went.
    Compaction(CompactResult),
    /// The typecheck after a step's file changes: new errors, a failure, or that it's off.
    Check(CheckResult),
    /// The project's own checks after a step's file changes.
    Checks(ProjectCheckResult),
    /// marv-rs: the session just became tainted by this source (show it; egress asks from now on, even in yolo).
    Tainted {
        source: Provenance,
    },
    /// Always last, exactly once, however the turn ended.
    TurnEnd {
        turn: String,
        reason: TurnEndReason,
    },
}

/// A saved session, brought back.
#[derive(Debug, Clone, PartialEq)]
pub struct Resumed {
    pub id: String,
    pub updated_at: u64,
    pub model: String,
    /// What the user saw (the client's transcript when it gave one, otherwise the messages and replies).
    pub transcript: Vec<Message>,
    pub totals: Totals,
}

/// A built-in provider by option, or a factory (the TUI makes one from its config: `ConfigFactory`).
#[derive(Clone)]
pub enum ProviderChoice {
    Factory(Arc<dyn ProviderFactory>),
    Option(ProviderOption),
}

/// Replaces Marv's system prompt, or appends to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemPrompt {
    Replace(String),
    Append(String),
}

pub type TranscriptFn = Arc<dyn Fn() -> Vec<Message> + Send + Sync>;
pub type Notify = Arc<dyn Fn() + Send + Sync>;
pub type Warn = Arc<dyn Fn(String) + Send + Sync>;

/// How to make a session (Marv's `SessionInit`). [`SessionInit::new`] gives Marv's defaults.
pub struct SessionInit {
    /// Absolute project root: the tools can't reach outside it.
    pub root: PathBuf,
    /// Shown to the model ("~/Projects/app"); default: the root, home shortened.
    pub cwd: Option<String>,
    pub provider: ProviderChoice,
    /// For a `ProviderOption`: let thinking models reason first (a factory has it built in).
    pub thinking: bool,
    /// Marv's version, for trajectories.
    pub version: String,
    /// The project's AGENTS.md, as it goes into the prompt (`prompt::read_instructions`).
    pub instructions: Option<String>,
    pub skills: Vec<LoadedSkill>,
    /// Agent types the agent tool can start (default: general-purpose).
    pub agents: Vec<AgentType>,
    pub memory: Option<(MemoryPaths, Memories)>,
    /// MCP servers, already starting; their tools join once they're ready.
    pub mcp: Option<McpHandle>,
    /// `close()` closes the MCP servers (the session started them; the CLI closes its own).
    pub owns_mcp: bool,
    /// The process's switchboard (mesh v1); the main agent gets a plug, subagents an unwired one.
    pub mesh: Option<MeshHandle>,
    pub wire_approver: Option<Arc<dyn WireApprover>>,
    /// Extra tools for the main agent, after the built-in ones; names must be their own.
    pub tools: Vec<ToolRef>,
    pub system_prompt: Option<SystemPrompt>,
    /// Asked for every call that needs a yes; without it only what yolo vouches for runs. Wrapped by the session
    /// (`SessionApprover`): "always" scopes, `must_ask`, a stop answers "no".
    pub approve: Option<Arc<dyn Approver>>,
    /// The main agent's step limit (default 25; 0 never asks and never stops it). Subagents keep their own 50.
    pub max_steps: Option<u32>,
    pub diagnostics: bool,
    pub checks: bool,
    pub mcp_tools: McpToolsSetting,
    pub sandbox: bool,
    pub yolo: bool,
    /// Yolo isn't allowed here, and why (the project is the home folder or above it).
    pub no_yolo: Option<String>,
    pub sessions: Option<Arc<SessionStore>>,
    pub trajectories: Option<Arc<TrajectoryStore>>,
    pub log_trajectories: bool,
    /// `~/.marv/worktrees/<project>`; without it, no worktrees.
    pub worktrees_dir: Option<PathBuf>,
    /// What the user saw, saved with the session (the TUI's transcript); default: the messages and replies.
    pub transcript: Option<TranscriptFn>,
    /// Something a UI shows changed outside a turn (the model's info arrived, memory reloaded).
    pub on_change: Option<Notify>,
    /// Something the user should hear about (a trajectory that can't be written).
    pub on_warning: Option<Warn>,
    /// Config files that couldn't be read, for the client to show.
    pub problems: Vec<String>,
}

impl SessionInit {
    pub fn new(root: impl Into<PathBuf>, provider: ProviderChoice) -> Self {
        Self {
            root: root.into(),
            cwd: None,
            provider,
            thinking: false,
            version: env!("CARGO_PKG_VERSION").into(),
            instructions: None,
            skills: Vec::new(),
            agents: vec![general_purpose()],
            memory: None,
            mcp: None,
            owns_mcp: false,
            mesh: None,
            wire_approver: None,
            tools: Vec::new(),
            system_prompt: None,
            approve: None,
            max_steps: None,
            diagnostics: true,
            checks: true,
            mcp_tools: McpToolsSetting::Auto,
            sandbox: true,
            yolo: true,
            no_yolo: None,
            sessions: None,
            trajectories: None,
            log_trajectories: true,
            worktrees_dir: None,
            transcript: None,
            on_change: None,
            on_warning: None,
            problems: Vec::new(),
        }
    }
}

/// New settings; each turn reads them when it starts.
#[derive(Clone, Default)]
pub struct Configure {
    pub provider: Option<ProviderChoice>,
    pub thinking: Option<bool>,
    pub sandbox: Option<bool>,
    pub yolo: Option<bool>,
    pub trajectories: Option<bool>,
    pub diagnostics: Option<bool>,
    pub checks: Option<bool>,
    /// The main agent's step limit (0 never asks).
    pub max_steps: Option<u32>,
    /// Applies from the next conversation that decides (after `clear()`).
    pub mcp_tools: Option<McpToolsSetting>,
}

#[derive(Clone, Default)]
pub struct SendOptions {
    /// What the model gets when it differs from what the user typed (a /skill's instructions).
    pub for_model: Option<String>,
    /// Stops the turn like `interrupt()`.
    pub cancel: Option<CancellationToken>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UsageSnapshot {
    /// The last request's tokens (how full the context is).
    pub last: Option<Usage>,
    /// The whole session's (survives `clear()`).
    pub totals: Totals,
    pub context_length: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Feedback {
    /// 1 good, -1 bad, 0 labels only.
    pub score: i8,
    pub note: Option<String>,
    pub labels: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateOutcome {
    Rated,
    /// Logging is off.
    Off,
    /// No logged turn to rate.
    Nothing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeTarget {
    Latest,
    Id(String),
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("{BUSY}")]
    Busy,
    #[error("{CLOSED}")]
    Closed,
    /// A bad option (a taken tool name, yolo where it isn't allowed), as Marv's sentence.
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// One running turn's events. Dropping it interrupts the turn (like breaking out of Marv's `for await`).
pub struct Turn {
    events: UnboundedReceiver<SessionEvent>,
    stop: CancellationToken,
    work: Option<tokio::task::JoinHandle<()>>,
}

impl Turn {
    /// Interrupts the turn, then waits for its cleanup (every tool call answered, the session free again).
    pub async fn finish(mut self) {
        self.stop.cancel();
        if let Some(work) = self.work.take() {
            let _ = work.await;
        }
    }

    /// Stops the turn (Esc) without waiting.
    pub fn interrupt(&self) {
        self.stop.cancel();
    }
}

impl Stream for Turn {
    type Item = SessionEvent;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<SessionEvent>> {
        self.events.poll_recv(cx)
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        // The client left before the end: that's an interrupt. A no-op after a normal end, and it's this turn's own
        // token, so it can't stop a newer turn the client started at TurnEnd.
        self.stop.cancel();
    }
}

/// What changes during a session (behind one lock; never held across an `.await`).
// Several fields are read only by `run_turn`, which is still to port.
#[allow(dead_code)]
struct State {
    factory: Arc<dyn ProviderFactory>,
    provider: Arc<dyn Provider>,
    option: ProviderChoice,
    thinking: bool,
    /// What the model list says about the model, once it has answered.
    info: Option<ModelInfo>,
    lookups: u64,
    /// What the model sees. Shared with the running loop (`marv_agent::History`).
    conversation: marv_agent::History,
    saved: SavedSession,
    totals: Totals,
    last: Option<Usage>,
    /// A mid-turn summary already failed this turn: don't ask for another (each would be another paid request).
    mid_turn_compact_failed: bool,
    current: Option<CancellationToken>,
    running: bool,
    closed: bool,
    memories: Option<Memories>,
    system: String,
    sandbox: bool,
    yolo: bool,
    diagnostics: bool,
    checks: bool,
    max_steps: Option<u32>,
    logging: bool,
    mcp_tools: McpToolsSetting,
    /// This conversation's MCP mode and what it was told (#14); `None` until a turn has MCP tools to decide on.
    listing: Option<McpListing>,
    log: Option<Arc<Trajectory>>,
    session_logged: Option<String>,
    /// The latest logged turn (turn id, session id): what `rate()` and the next message's tone rate.
    last_turn: Option<(String, String)>,
    /// The default transcript: what was asked, and the replies.
    messages: Vec<Message>,
    save_timer: Option<tokio::task::JoinHandle<()>>,
    working: Option<tokio::task::JoinHandle<()>>,
}

struct Inner {
    init: SessionInit,
    /// Built once: every request sends byte-identical tool definitions.
    specs: Vec<marv_core::ToolSpec>,
    always: AlwaysScopes,
    taint: Arc<TaintState>,
    typecheck: Arc<Checker<TsError>>,
    project_checks: Arc<Checker<Violation>>,
    state: Mutex<State>,
}

/// The engine. Cheap to clone (an `Arc`): the TUI keeps one for `interrupt()` while a [`Turn`] streams.
#[derive(Clone)]
pub struct MarvSession {
    inner: Arc<Inner>,
}

/// Tools of the caller's own must have names of their own: `run_tool` runs the first tool with the call's name, so a
/// custom "bash" would be offered next to the built-in one while every call ran the built-in; `mcp__` names are MCP
/// servers'; `tool_search`/`mcp_call` are a deferred conversation's.
pub fn check_tool_names(custom: &[ToolRef]) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for tool in custom {
        let name = tool.name();
        if marv_tools::builtin_tools().iter().any(|t| t.name() == name)
            || DEFERRED_NAMES.contains(&name)
            || [marv_mesh::tools::MESH_LIST, marv_mesh::tools::MESH_CALL, marv_mesh::tools::MESH_SEND].contains(&name)
        {
            return Err(format!("A tool named \"{name}\" is already built in: give yours another name."));
        }
        if name.starts_with("mcp__") {
            return Err("Tool names starting with \"mcp__\" are for MCP servers' tools.".into());
        }
        if !seen.insert(name.to_string()) {
            return Err(format!("Two tools are named \"{name}\"."));
        }
    }
    Ok(())
}

fn factory_of(choice: &ProviderChoice, thinking: bool) -> Arc<dyn ProviderFactory> {
    match choice {
        ProviderChoice::Factory(f) => f.clone(),
        ProviderChoice::Option(o) => crate::config::provider_factory(o.clone(), thinking),
    }
}

impl MarvSession {
    /// Builds a session. Fails (and changes nothing) for a taken tool name, or yolo where `no_yolo` forbids it.
    pub fn new(init: SessionInit) -> Result<Self, SessionError> {
        check_tool_names(&init.tools).map_err(SessionError::Invalid)?;
        if let Some(why) = &init.no_yolo
            && init.yolo
        {
            return Err(SessionError::Invalid(why.clone()));
        }
        let factory = factory_of(&init.provider, init.thinking);
        let provider = factory.make(None, None);
        let has_skills = !init.skills.is_empty();
        let mut specs = marv_tools::tool_specs_for(has_skills);
        specs.extend(init.tools.iter().map(|t| t.spec()));
        if init.mesh.is_some() {
            specs.extend(marv_mesh::tools::mesh_specs());
        }
        let saved = new_session(&init.root, factory.id(), factory.model(), crate::time::now_ms());
        let memories = init.memory.as_ref().map(|(_, m)| m.clone());
        let state = State {
            factory,
            provider,
            option: init.provider.clone(),
            thinking: init.thinking,
            info: None,
            lookups: 0,
            conversation: Arc::default(),
            saved,
            totals: Totals::default(),
            last: None,
            mid_turn_compact_failed: false,
            current: None,
            running: false,
            closed: false,
            memories,
            system: String::new(),
            sandbox: init.sandbox,
            yolo: init.yolo,
            diagnostics: init.diagnostics,
            checks: init.checks,
            max_steps: init.max_steps,
            logging: init.log_trajectories,
            mcp_tools: init.mcp_tools,
            listing: None,
            log: None,
            session_logged: None,
            last_turn: None,
            messages: Vec::new(),
            save_timer: None,
            working: None,
        };
        let inner = Inner {
            specs,
            always: AlwaysScopes::default(),
            taint: Arc::new(TaintState::new()),
            typecheck: Arc::new(Checker::new(Arc::new(crate::diagnostics::turn::Typecheck), None)),
            project_checks: Arc::new(Checker::new(Arc::new(crate::checks::kind::ProjectChecksKind), None)),
            state: Mutex::new(state),
            init,
        };
        let session = Self { inner: Arc::new(inner) };
        let system = session.build_system();
        session.state().system = system;
        session.lookup();
        Ok(session)
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The session's id: its file name under `sessions/`, and what `marv -r` and `resume` take.
    pub fn id(&self) -> String {
        self.state().saved.id.clone()
    }
    /// A turn (or a compaction) is running.
    pub fn busy(&self) -> bool {
        self.state().running
    }
    /// "openrouter · x/y", for a status bar.
    pub fn provider_name(&self) -> String {
        self.state().provider.name().to_string()
    }
    /// The model's context window in tokens, when known (Ollama's num_ctx, or OpenRouter's model list).
    pub fn context_length(&self) -> Option<u64> {
        let s = self.state();
        s.provider.context_length().or_else(|| s.info.as_ref().and_then(|i| i.context))
    }
    pub fn model_info(&self) -> Option<ModelInfo> {
        self.state().info.clone()
    }
    /// Memory as of the start of this conversation.
    pub fn memory(&self) -> Option<Memories> {
        self.state().memories.clone()
    }
    /// Files and servers that couldn't be read, and why.
    pub fn problems(&self) -> &[String] {
        &self.inner.init.problems
    }
    /// What tainted this session, in order (empty: clean).
    pub fn tainted(&self) -> Vec<Provenance> {
        self.inner.taint.sources()
    }
    /// The MCP manager (for `/mcp`), if the session has one.
    pub fn mcp(&self) -> Option<McpHandle> {
        self.inner.init.mcp.clone()
    }
    /// The mesh switchboard (for `marv mesh wire` and the TUI's wire approvals), if any.
    pub fn mesh(&self) -> Option<MeshHandle> {
        self.inner.init.mesh.clone()
    }
    /// The system prompt this conversation sends (byte-stable until `clear()`).
    pub fn system_prompt(&self) -> String {
        self.state().system.clone()
    }

    pub fn usage(&self) -> UsageSnapshot {
        let context_length = self.context_length();
        let s = self.state();
        UsageSnapshot { last: s.last, totals: s.totals.clone(), context_length }
    }

    /// Refuses new work after `close()` and during a turn.
    fn idle(&self, s: &State) -> Result<(), SessionError> {
        if s.closed {
            return Err(SessionError::Closed);
        }
        if s.running {
            return Err(SessionError::Busy);
        }
        Ok(())
    }

    /// Runs one turn: the user's message and everything the agent does until it stops. The turn starts at once on its
    /// own task (it must be inside a Tokio runtime); read the [`Turn`] for its events. Fails during a turn, or after
    /// `close()`.
    pub fn send(&self, text: &str, options: SendOptions) -> Result<Turn, SessionError> {
        let stop = CancellationToken::new();
        {
            let mut s = self.state();
            self.idle(&s)?;
            // Sites the user pasted are theirs to fetch: only what they typed counts (not a /skill's instructions,
            // a file or a page: links found there are where a planted instruction would send the model).
            for key in marv_tools::web_fetch::pasted_scopes(text) {
                self.inner.always.add(key);
            }
            s.running = true;
            s.current = Some(stop.clone());
        }
        if let Some(outer) = options.cancel.clone() {
            let stop = stop.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = outer.cancelled() => stop.cancel(),
                    _ = stop.cancelled() => {}
                }
            });
        }
        let queue = Arc::new(EventQueue::new());
        let events = queue.reader().map_err(|e| SessionError::Invalid(e.into()))?;
        let session = self.clone();
        let (text, for_model, turn_stop) =
            (text.to_string(), options.for_model.unwrap_or_else(|| text.to_string()), stop.clone());
        let work = tokio::spawn(async move {
            let push = queue.clone();
            session.run_turn(text, for_model, turn_stop, move |e| push.push(e)).await;
            queue.close();
        });
        Ok(Turn { events, stop, work: Some(work) })
    }

    /// One turn's work (Marv's `runTurn`). Never fails: whatever happens ends in `TurnEnd`.
    ///
    /// Order: `TurnStart` → wait for `memory reload` and MCP `ready()` (`WaitingForMcp`; Esc → `Interrupted`) →
    /// stopped before sending? `Interrupted`, nothing logged → read the turn's settings → decide the MCP mode (#14) →
    /// trajectory `session`/`turn_start` (+ implicit feedback on the previous turn) → compact first if nearly full →
    /// MCP announcement note at the start of the user message → `Running` → the checks' `TurnChecks` → the
    /// [`crate::runner::SessionAgentHost`] → `marv_agent::run_agent` with the [`crate::runner::SessionToolRunner`],
    /// `rewrite_call` (deferred MCP), `on_full` (mid-turn compaction), `on_limit` (the step-limit approval),
    /// `after_step` (checks), `wrap_up_at_limit` → forward `Loop` events (usage → totals; assistant → transcript;
    /// agent `ToolStart` → `subagent_start` record) → drain mesh inbox between steps → finally: reset
    /// `mid_turn_compact_failed`, finish recorders, cancel the stop, free the session, schedule the save, `TurnEnd`.
    async fn run_turn(
        &self,
        _text: String,
        _for_model: String,
        _stop: CancellationToken,
        _push: impl Fn(SessionEvent) + Send + Sync + 'static,
    ) {
        todo!("port: runTurn (src/session.ts) as documented above")
    }

    /// Stops the running turn or compaction (Esc). Approvals still waiting are answered "no".
    pub fn interrupt(&self) {
        if let Some(stop) = &self.state().current {
            stop.cancel();
        }
    }

    /// `/compact [focus]`: summarizes the conversation now and continues from the summary. Fails during a turn.
    pub async fn compact(&self, _focus: Option<&str>) -> Result<CompactResult, SessionError> {
        {
            let s = self.state();
            self.idle(&s)?;
        }
        todo!("port: compact (summarizeInto with the session's provider; running/current set and reset; schedule save)")
    }

    /// `/clear`: a new conversation (and session file), with memory reloaded for the new system prompt. Totals stay.
    /// Saves the previous conversation first.
    pub async fn clear(&self) -> Result<(), SessionError> {
        {
            let mut s = self.state();
            self.idle(&s)?;
            self.save_now(&mut s);
            s.saved = new_session(&self.inner.init.root, s.factory.id(), s.factory.model(), crate::time::now_ms());
            s.conversation = Arc::default();
            s.messages.clear();
            s.last = None;
            s.listing = None; // a new conversation decides its MCP mode again
        }
        if let Some((paths, _)) = &self.inner.init.memory
            && let Ok(memories) = crate::memory::load_memory(paths).await
        {
            self.state().memories = Some(memories);
        }
        // Servers removed with /mcp remove kept their tools listed for the old conversation's sake; not for this one.
        if let Some(mcp) = &self.inner.init.mcp {
            mcp.forget_removed();
        }
        let system = self.build_system();
        self.state().system = system;
        self.changed();
        Ok(())
    }

    /// Brings back a saved session (its id, or the latest here); it keeps saving to the same file. `None` when
    /// there's none. Fails during a turn.
    pub async fn resume(&self, target: ResumeTarget) -> Result<Option<Resumed>, SessionError> {
        {
            let s = self.state();
            self.idle(&s)?;
        }
        let Some(store) = self.inner.init.sessions.clone() else {
            return Ok(None);
        };
        let root = &self.inner.init.root;
        let saved = match &target {
            ResumeTarget::Latest => store.latest(root).await,
            ResumeTarget::Id(id) => store.load(root, id).await,
        };
        let Some(saved) = saved else { return Ok(None) };
        let mut s = self.state();
        // A turn may have started while it loaded: swapping the conversation under it would mix the two.
        self.idle(&s)?;
        self.save_now(&mut s);
        s.conversation = Arc::new(Mutex::new(saved.conversation.clone()));
        s.messages = saved.transcript.clone();
        s.totals = saved.totals.clone();
        s.last = None;
        s.listing = saved.mcp_listing();
        let resumed = Resumed {
            id: saved.id.clone(),
            updated_at: saved.updated_at,
            model: saved.model.clone(),
            transcript: saved.transcript.clone(),
            totals: saved.totals.clone(),
        };
        s.saved = saved;
        Ok(Some(resumed))
    }

    /// New settings, from the next turn. All or nothing: the new provider is made first; yolo where `no_yolo`
    /// forbids it fails and changes nothing.
    pub fn configure(&self, changes: Configure) -> Result<(), SessionError> {
        if changes.yolo == Some(true)
            && let Some(why) = &self.inner.init.no_yolo
        {
            return Err(SessionError::Invalid(why.clone()));
        }
        let mut s = self.state();
        let remade = if changes.provider.is_some() || changes.thinking.is_some() {
            let option = changes.provider.clone().unwrap_or_else(|| s.option.clone());
            let thinking = changes.thinking.unwrap_or(s.thinking);
            let factory = factory_of(&option, thinking);
            // Same model (a /think or /yolo): what the list said still holds, so the provider keeps its switch.
            let same = factory.id() == s.factory.id() && factory.model() == s.factory.model();
            let info = if same { s.info.clone() } else { None };
            let provider = factory.make(None, info.as_ref().and_then(|i| i.reasoning));
            Some((option, thinking, factory, provider, info))
        } else {
            None
        };
        if let Some(v) = changes.sandbox {
            s.sandbox = v;
        }
        if let Some(v) = changes.yolo {
            s.yolo = v;
        }
        if let Some(v) = changes.trajectories {
            s.logging = v;
        }
        if let Some(v) = changes.diagnostics {
            // Turned back on: forget what made checking give up, so /diagnostics on really does.
            if v && !s.diagnostics {
                self.inner.typecheck.reset();
            }
            s.diagnostics = v;
        }
        if let Some(v) = changes.checks {
            if v && !s.checks {
                self.inner.project_checks.reset();
            }
            s.checks = v;
        }
        if let Some(v) = changes.max_steps {
            s.max_steps = Some(v);
        }
        if let Some(v) = changes.mcp_tools {
            s.mcp_tools = v;
        }
        let Some((option, thinking, factory, provider, info)) = remade else {
            return Ok(());
        };
        (s.option, s.thinking, s.factory, s.provider, s.info) = (option, thinking, factory, provider, info);
        drop(s);
        self.lookup();
        Ok(())
    }

    /// `/good`, `/bad`, `/label`: feedback on the last turn, in its trajectory.
    pub fn rate(&self, feedback: Feedback) -> RateOutcome {
        let Some(log) = self.trajectory() else {
            return RateOutcome::Off;
        };
        let Some((turn, session)) = self.state().last_turn.clone() else {
            return RateOutcome::Nothing;
        };
        if session != log.session {
            return RateOutcome::Nothing;
        }
        log.write(&TrajectoryRecord::Feedback {
            turn,
            score: feedback.score,
            source: "explicit",
            labels: feedback.labels,
            note: feedback.note,
            phrase: None,
        });
        RateOutcome::Rated
    }

    /// Saves the conversation now (it's also saved shortly after every turn).
    pub async fn save(&self) -> Result<(), SessionError> {
        let Some(store) = self.inner.init.sessions.clone() else {
            return Ok(());
        };
        let saved = self.snapshot();
        store.save(&saved).await?;
        Ok(())
    }

    /// What `save()` writes: every tool call answered (a run winding down may still have unanswered calls).
    fn snapshot(&self) -> SavedSession {
        let transcript = self.inner.init.transcript.as_ref().map(|f| f());
        let mut s = self.state();
        let conversation = marv_agent::agent::answer_all_calls(&s.conversation.lock().unwrap_or_else(|e| e.into_inner()));
        let mut saved = s.saved.clone();
        saved.provider = s.factory.id().into();
        saved.model = s.factory.model().into();
        saved.conversation = conversation;
        saved.transcript = transcript.unwrap_or_else(|| s.messages.clone());
        saved.totals = s.totals.clone();
        saved.mcp = s.listing.as_ref().and_then(|l| serde_json::to_value(l).ok());
        s.saved = saved.clone();
        saved
    }

    /// The waiting save, now: before the conversation is swapped (clear, resume), or the timer would save the new one.
    fn save_now(&self, s: &mut State) {
        if let Some(timer) = s.save_timer.take() {
            timer.abort();
            todo!("port: saveNow (start the save of the current snapshot now; flush() waits for it)")
        }
    }

    /// Before exiting: the pending save, and the trajectory's queued records, on disk.
    pub async fn flush(&self) {
        let (timer, log) = {
            let mut s = self.state();
            (s.save_timer.take(), s.log.clone())
        };
        if let Some(t) = timer {
            t.abort();
        }
        let _ = self.save().await;
        if let Some(log) = log {
            log.flush().await;
        }
    }

    /// Stops what's running, waits for it to wind down, flushes, and closes the MCP servers the session started.
    /// After it, `send()`, `compact()`, `clear()` and `resume()` fail; calling it again is fine.
    pub async fn close(&self) {
        let working = {
            let mut s = self.state();
            s.closed = true;
            if let Some(stop) = &s.current {
                stop.cancel();
            }
            s.working.take()
        };
        if let Some(w) = working {
            let _ = w.await;
        }
        self.flush().await;
        if self.inner.init.owns_mcp
            && let Some(mcp) = &self.inner.init.mcp
        {
            mcp.close().await;
        }
    }

    fn cwd(&self) -> String {
        self.inner
            .init
            .cwd
            .clone()
            .unwrap_or_else(|| crate::paths::shorten_home(&self.inner.init.root, &crate::paths::home_dir()))
    }

    fn build_system(&self) -> String {
        let init = &self.inner.init;
        if let Some(SystemPrompt::Replace(text)) = &init.system_prompt {
            return text.clone();
        }
        let memory = self.state().memories.clone();
        let base = system_prompt(&PromptInput {
            cwd: self.cwd(),
            tools: self.inner.specs.iter().map(|s| s.name.clone()).collect(),
            instructions: init.instructions.clone(),
            skills: init
                .skills
                .iter()
                .map(|s| Listed { name: s.skill.name.clone(), description: s.skill.description.clone() })
                .collect(),
            memory,
            agents: init.agents.iter().map(|a| Listed { name: a.name.clone(), description: a.description.clone() }).collect(),
            // Whenever there's a manager, even an empty one: a server added mid-conversation is then covered without
            // changing the system prompt (the prompt cache).
            mcp: init.mcp.is_some(),
            date: crate::time::iso_date(crate::time::now_ms()),
        });
        match &init.system_prompt {
            Some(SystemPrompt::Append(more)) => format!("{base}\n\n{more}"),
            _ => base,
        }
    }

    /// Reads the model list in the background (context window, prices, reasoning switch). Needs a Tokio runtime;
    /// outside one it's skipped (no estimates, that's all).
    fn lookup(&self) {
        let (factory, ticket) = {
            let mut s = self.state();
            s.lookups += 1;
            (s.factory.clone(), s.lookups)
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let Some(lookup) = factory.lookup() else {
            return;
        };
        let session = self.clone();
        handle.spawn(async move {
            let Some(info) = lookup.await else { return };
            {
                let mut s = session.state();
                if s.lookups != ticket {
                    return; // the provider changed meanwhile
                }
                if info.reasoning.is_some() {
                    s.provider = factory.make(None, info.reasoning);
                }
                s.info = Some(info);
            }
            session.changed();
        });
    }

    /// Tells the client something changed. Its callback failing is the client's problem.
    fn changed(&self) {
        if let Some(f) = &self.inner.init.on_change {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f()));
        }
    }

    /// Adds a request's tokens and cost to the totals. `factory` made the provider; `info` is what the list said
    /// about its model when the turn started (another model's prices would make the estimate wrong).
    #[allow(dead_code)] // used by run_turn, still to port
    pub(crate) fn count(&self, usage: &Usage, factory: &dyn ProviderFactory, info: Option<&ModelInfo>) {
        let mut s = self.state();
        let same = factory.id() == s.factory.id() && factory.model() == s.factory.model();
        let prices = info.or(if same { s.info.as_ref() } else { None }).map(Prices::from).unwrap_or_default();
        let local = (s.totals.requests == 0 || s.totals.local == Some(true)) && factory.local();
        let mut totals = add_usage(&s.totals, usage, &prices);
        totals.local = Some(local);
        s.totals = totals;
    }

    /// Whether the last request used ≥85% of the context window.
    #[allow(dead_code)] // used by run_turn, still to port
    pub(crate) fn nearly_full(&self) -> bool {
        let context = self.context_length();
        let s = self.state();
        match (context, s.last) {
            (Some(c), Some(last)) => {
                (last.prompt_tokens + last.completion_tokens) as f64 >= c as f64 * marv_agent::compact::COMPACT_AT
            }
            _ => false,
        }
    }

    /// The default transcript's next entry.
    #[allow(dead_code)] // used by run_turn, still to port
    pub(crate) fn note(&self, role: Role, text: &str) {
        let mut s = self.state();
        let id = s.messages.last().map_or(0, |m| m.id) + 1;
        s.messages.push(Message::new(id, role, text));
    }

    /// This session's trajectory, or `None` when logging is off.
    fn trajectory(&self) -> Option<Arc<Trajectory>> {
        let store = self.inner.init.trajectories.clone()?;
        let mut s = self.state();
        if !s.logging {
            return None;
        }
        if s.log.as_ref().is_none_or(|l| l.session != s.saved.id) {
            let log = Arc::new(store.open(&self.inner.init.root, &s.saved.id));
            if let Some(warn) = self.inner.init.on_warning.clone() {
                log.on_error(Arc::new(move |t| warn(t)));
            }
            s.log = Some(log);
        }
        s.log.clone()
    }

    /// The conversation as the model sees it now (a copy).
    pub fn conversation(&self) -> Vec<ChatTurn> {
        self.state().conversation.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marv_core::ToolSpec;
    use marv_provider::EventStream;

    struct Silent;
    impl Provider for Silent {
        fn name(&self) -> &str {
            "silent"
        }
        fn context_length(&self) -> Option<u64> {
            Some(1000)
        }
        fn stream(&self, _history: &[ChatTurn], _options: marv_provider::StreamOptions) -> EventStream {
            Box::pin(futures::stream::empty())
        }
    }

    fn session(edit: impl FnOnce(&mut SessionInit)) -> Result<MarvSession, SessionError> {
        let mut init =
            SessionInit::new("/tmp/marv-session-test", ProviderChoice::Option(ProviderOption::Custom(Arc::new(Silent))));
        init.cwd = Some("~/p".into());
        edit(&mut init);
        MarvSession::new(init)
    }

    #[test]
    fn a_custom_provider_is_used_and_named() {
        let s = session(|_| {}).unwrap();
        assert_eq!(s.provider_name(), "silent");
        assert_eq!(s.context_length(), Some(1000));
        assert!(!s.busy());
        assert!(s.system_prompt().contains("Working directory: ~/p"));
    }

    #[test]
    fn yolo_where_it_isnt_allowed_fails_and_configure_changes_nothing() {
        let err = session(|i| i.no_yolo = Some("home".into())).err().unwrap();
        assert_eq!(err.to_string(), "home");
        let s = session(|i| {
            i.no_yolo = Some("home".into());
            i.yolo = false;
        })
        .unwrap();
        assert!(s.configure(Configure { yolo: Some(true), ..Configure::default() }).is_err());
        assert!(!s.state().yolo);
    }

    #[test]
    fn system_prompt_can_be_replaced_or_appended() {
        let s = session(|i| i.system_prompt = Some(SystemPrompt::Replace("Be brief.".into()))).unwrap();
        assert_eq!(s.system_prompt(), "Be brief.");
        let s = session(|i| i.system_prompt = Some(SystemPrompt::Append("Extra.".into()))).unwrap();
        assert!(s.system_prompt().ends_with("\n\nExtra."));
    }

    struct Named(&'static str);
    #[async_trait::async_trait]
    impl marv_tools::Tool for Named {
        type Input = serde_json::Value;
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "x"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn label(&self, _input: &Self::Input) -> String {
            String::new()
        }
        async fn run(
            &self,
            _input: &Self::Input,
            _ctx: &marv_tools::ToolContext,
        ) -> Result<marv_tools::ToolOutput, marv_tools::ToolError> {
            Ok(marv_tools::ToolOutput::ok("", ""))
        }
    }

    #[test]
    fn custom_tool_names_must_be_their_own() {
        let tools = |names: &[&'static str]| names.iter().map(|n| Arc::new(Named(n)) as ToolRef).collect::<Vec<_>>();
        assert!(check_tool_names(&tools(&["deploy"])).is_ok());
        assert!(check_tool_names(&tools(&["bash"])).unwrap_err().contains("already built in"));
        assert!(check_tool_names(&tools(&["mcp_call"])).unwrap_err().contains("already built in"));
        assert!(check_tool_names(&tools(&["mcp__x__y"])).unwrap_err().contains("mcp__"));
        assert!(check_tool_names(&tools(&["a", "a"])).unwrap_err().contains("Two tools"));
        let s = session(|i| i.tools = tools(&["deploy"])).unwrap();
        let _: &[ToolSpec] = &s.inner.specs;
        assert_eq!(s.inner.specs.last().unwrap().name, "deploy");
    }

    #[test]
    fn totals_count_requests_and_local_models() {
        let s = session(|_| {}).unwrap();
        let factory = s.state().factory.clone();
        s.count(
            &Usage { prompt_tokens: 900, completion_tokens: 10, cached_tokens: None, cost: Some(0.01) },
            factory.as_ref(),
            None,
        );
        assert_eq!(s.usage().totals.requests, 1);
        s.state().last = Some(Usage { prompt_tokens: 900, completion_tokens: 10, cached_tokens: None, cost: None });
        assert!(s.nearly_full());
    }

    #[test]
    fn turn_end_reasons_use_marvs_names() {
        assert_eq!(TurnEndReason::Done(StopReason::MaxSteps).as_str(), "max_steps");
        assert_eq!(TurnEndReason::Interrupted.as_str(), "interrupted");
    }
}
