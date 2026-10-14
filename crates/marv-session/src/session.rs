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

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher, Hasher};
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures::{FutureExt, Stream, StreamExt};
use marv_agent::compact::{MID_TURN_NOTE, SummarizeOptions, compacted_history};
use marv_agent::{AgentOptions, LimitAnswer, LoopEvent};
use marv_core::{ChatTurn, Provenance, StopReason, ToolCall, ToolSpec, Usage};
use marv_mcp::deferred::{
    DEFERRED_NAMES, McpMode, McpToolsSetting, ModeChoice, announcement, as_mcp_call, choose_mode, deferred_specs,
};
use marv_provider::{ModelInfo, Provider, ProviderFactory, ProviderOption};
use marv_sandbox::TaintState;
use marv_tools::{AgentProgress, Approver, Decision, PlanHost, PlanItem, ProcessInfo, ToolContext, ToolRef};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_util::sync::CancellationToken;

use crate::agents::{AgentType, general_purpose};
use crate::checkpoints::store::{CheckpointKind, CheckpointStore, SnapshotResult, StoreOptions};
use crate::checkpoints::turn::CheckpointTurn;
use crate::checkpoints::undo::{UndoPlan, UndoRefusal, UndoResult, apply_undo, plan_undo, undo_note};
use crate::checks::{ProjectCheckResult, Violation};
use crate::diagnostics::turn::{CheckOutcome, Checker, FailReason, OffReason, TurnChecks, TurnOptions, combine_checks};
use crate::diagnostics::{CheckResult, TsError};
use crate::event_queue::EventQueue;
use crate::feedback::classify_reply;
use crate::mcp_tools::{McpCallTool, McpHandle, McpToolAdapter, ToolSearchTool};
use crate::memory::{Memories, MemoryPaths};
use crate::mesh::{MeshAccess, MeshHandle, MeshTool, WireApprover};
use crate::processes::{ProcessManager, ProcessManagerOptions};
use crate::prompt::{Listed, PromptInput, system_prompt};
use crate::runner::{AlwaysScopes, SessionAgentHost, SessionApprover, SessionToolRunner, SubagentSink, step_limit_request};
use crate::sessions::{McpListing, SavedCheckpoints, SavedSession, SessionStore, new_session};
use crate::skills::LoadedSkill;
use crate::trajectory::{AgentRecorder, CheckStatus, Trajectory, TrajectoryRecord, TrajectoryStore, Who};
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
    /// The turn's checkpoint snapshot is taking longer than 300 ms (#16): "Saving a checkpoint…".
    Checkpointing,
    /// The message is sent (again after each `Checking` or `Checkpointing`).
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
    /// The turn's checkpoint (#16) couldn't be saved: the turn goes on, but /undo can't take it back. Once per turn.
    CheckpointFailed {
        message: String,
    },
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
    /// The plan tool's list as it was saved (#15), or `None`.
    pub plan: Option<Vec<PlanItem>>,
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
    /// The instruction files, as they go into the prompt (`instructions::load_instructions`, which gives each its
    /// heading); plain text of your own goes under "# Project instructions (from AGENTS.md)".
    pub instructions: Option<String>,
    /// Append a folder's own `AGENTS.md`/`CLAUDE.md` (below the root) to the first read_file, edit_file or write_file
    /// result there, once per conversation (#62; `crate::instructions::NestedInstructions`). Default false: an SDK
    /// session reads the project's files only when it asks for the `Project` source; the CLI sets it.
    pub nested_instructions: bool,
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
    /// Offer the plan tool (#15): a checklist the main agent keeps for a long task. Default true. Fixed for the
    /// session's life, since it changes the tool list (prompt cache).
    pub plan: bool,
    pub diagnostics: bool,
    pub checks: bool,
    /// Snapshot the project before each turn's first change and after the turn (git commits under a hidden ref, #16),
    /// so `undo_plan()`/`undo()` can put a turn's files back. Default true.
    pub checkpoints: bool,
    /// Marv's own git dir for a project outside a git repository (`~/.marv/checkpoints/<project>`); without it, such
    /// a project gets no checkpoints.
    pub checkpoints_dir: Option<PathBuf>,
    /// The checkpoints' store; tests pass one.
    pub checkpoint_store: Option<Arc<CheckpointStore>>,
    pub mcp_tools: McpToolsSetting,
    pub sandbox: bool,
    /// Let bash run a command on the host (`sandbox: false`, the host door, Marv #52), asked every time. Only where
    /// someone approves each command at a terminal (the TUI sets it); default false: there it's refused before any
    /// ask, whatever the approver would say. The main agent only: subagents never get it.
    pub host_commands: bool,
    /// Offer the github tool (#47): gh with the user's login, outside the sandbox (reads run, writes ask). The caller
    /// checks once that gh is there and logged in (`marv_tools::github::github_available`) before the session
    /// exists, since the tool list is fixed for the session (the prompt cache). Default false. Subagents get it for
    /// reads only.
    pub github: bool,
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
    /// Offer the process tool (Marv #51): servers and watchers that keep running in their own sandbox, their ports
    /// forwarded to the user's 127.0.0.1, each start asked. Default true; offered only where bubblewrap works. Fixed
    /// for the session's life (it changes the tool list and the prompt). The main agent only.
    pub processes: bool,
    /// The background processes' manager (tests pass one with a fake home); default: one made with the session.
    pub process_manager: Option<ProcessManager>,
    /// Pre-approved scopes (#98): the config's `"approve"` (and the TUI's `--approve`) scope names. They seed the
    /// session's "always" scopes and are listed in the prompt; a subagent inherits neither (`NOT_INHERITED`).
    pub pre_approved: Vec<String>,
    /// Extra writable sandbox folders (#98): the config's `"writable"`, resolved (`writable::resolve`) and
    /// canonical, bound read-write into every command's sandbox. Empty: the project is the only writable place.
    pub writable: Vec<PathBuf>,
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
            nested_instructions: false,
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
            plan: true,
            diagnostics: true,
            checks: true,
            checkpoints: true,
            checkpoints_dir: None,
            checkpoint_store: None,
            mcp_tools: McpToolsSetting::Auto,
            sandbox: true,
            host_commands: false,
            github: false,
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
            processes: true,
            process_manager: None,
            pre_approved: Vec::new(),
            writable: Vec::new(),
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
    /// Checkpoints before each turn's changes (#16), for `undo()`.
    pub checkpoints: Option<bool>,
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
    /// An undo that couldn't be applied (a stale plan, a failed snapshot first), as Marv's sentence.
    #[error("{0}")]
    Undo(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// For /checkpoints: why the last checkpoint failed, or why checkpoints are off for this project.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckpointProblem {
    pub failure: Option<String>,
    pub off: Option<String>,
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
struct State {
    factory: Arc<dyn ProviderFactory>,
    provider: Arc<dyn Provider>,
    option: ProviderChoice,
    thinking: bool,
    /// What the model list says about the model, once it has answered.
    info: Option<ModelInfo>,
    /// Bumped by each lookup: an answer for a provider replaced meanwhile is ignored.
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
    /// Writes for a conversation the session has moved on from (its last save, its trajectory): flush() waits for them.
    flushing: Vec<tokio::task::JoinHandle<()>>,
    /// The running turn's or compaction's work, until it has fully ended (cancelled then): close() waits for it.
    working: Option<CancellationToken>,
    /// The running turn's MCP specs: what a mid-turn compaction announces again.
    turn_mcp_specs: Arc<Vec<ToolSpec>>,
    /// The plan tool's list (#15): set only by its calls, saved with the conversation, re-injected after compaction.
    plan: Option<Vec<PlanItem>>,
    /// A turn has waited for the MCP servers (see `turn_body`): only the first turn always does.
    mcp_waited: bool,
    checkpoints: bool,
    /// This conversation's turns, counted: a checkpoint's turn number ("Undo turn 3").
    turn_number: u32,
    /// The paths each turn's tools said they changed (by turn number): an undo lists the ones git ignores.
    reported: std::collections::BTreeMap<u32, Vec<String>>,
    /// Why the last checkpoint failed, for `undo_plan()`'s answer; cleared by one that worked.
    checkpoint_failure: Option<String>,
    /// What the model hasn't been told about an undo yet: it starts the next user message.
    pending_note: Option<String>,
    /// The checkpoint chain this conversation's turns go on: its id, unless a resume couldn't read that chain. Saved.
    chain_id: String,
    /// Old chains are pruned once per session, at its first turn.
    pruned: bool,
}

impl State {
    /// A conversation was swapped (clear, resume): its own chain, numbering and undo note.
    fn new_chain(&mut self, store: &CheckpointStore) {
        self.chain_id = self.saved.id.clone();
        store.set_session(&self.chain_id);
        self.turn_number = 0;
        self.reported.clear();
        self.pending_note = None;
        self.checkpoint_failure = None;
    }

    /// A path a turn's tool reported changing (`ToolOutput::changed`), main agent or subagent.
    fn note_changed(&mut self, turn: u32, event: &LoopEvent) {
        if let LoopEvent::ToolEnd { run, .. } = event
            && let Some(changed) = &run.changed
        {
            let paths = self.reported.entry(turn).or_default();
            if !paths.contains(changed) {
                paths.push(changed.clone());
            }
        }
    }
}

struct Inner {
    init: SessionInit,
    /// Built once: every request sends byte-identical tool definitions.
    specs: Vec<marv_core::ToolSpec>,
    always: AlwaysScopes,
    taint: Arc<TaintState>,
    /// The skills as the `skill` tool takes them.
    skills: Arc<Vec<marv_tools::Skill>>,
    /// MCP tools whose schema this conversation has seen (tool_search, or a rejected call): their errors don't repeat it.
    loaded: Arc<Mutex<HashSet<String>>>,
    typecheck: Arc<Checker<TsError>>,
    project_checks: Arc<Checker<Violation>>,
    /// Whether this session offers the process tool (Marv #51; decided at construction: it changes the tool list and
    /// the prompt).
    process_on: bool,
    /// The background processes. They belong to the session, not the conversation: they outlive turns, `clear()` and
    /// `resume()`, and stop on `close()`.
    processes: Option<ProcessManager>,
    /// The project's checkpoints (#16); its ref follows the conversation (`State::chain_id`).
    checkpoints: Arc<CheckpointStore>,
    /// Nested instruction files the main agent's conversation has had (#62); emptied with it (`clear`, `resume`) and
    /// by a compaction (the summary replaces the results that carried them).
    nested: Arc<crate::instructions::NestedInstructions>,
    state: Mutex<State>,
}

/// The engine. Cheap to clone (an `Arc`): the TUI keeps one for `interrupt()` while a [`Turn`] streams.
#[derive(Clone)]
pub struct MarvSession {
    inner: Arc<Inner>,
}

/// Tools of the caller's own must have names of their own: `run_tool` runs the first tool with the call's name, so a
/// custom "bash" would be offered next to the built-in one while every call ran the built-in; `mcp__` names are MCP
/// servers'; `tool_search`/`mcp_call` are a deferred conversation's. `plan` and `github` are built in only where the
/// session offers them (`plan`, `github`): elsewhere the name is free.
pub fn check_tool_names(custom: &[ToolRef], plan: bool, github: bool) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for tool in custom {
        let name = tool.name();
        if marv_tools::builtin_tools()
            .iter()
            .any(|t| t.name() == name && (plan || name != "plan") && (github || name != "github"))
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

/// The running turn's checkpoint pair, finished by `run_turn` however the turn ends.
type Pair = Arc<Mutex<Option<Arc<CheckpointTurn>>>>;

/// Where a turn's events go (the queue), dropped after `TurnEnd`.
type Emit = Arc<dyn Fn(SessionEvent) + Send + Sync>;
/// Writes a trajectory record (nothing when logging is off).
type Record = Arc<dyn Fn(TrajectoryRecord) + Send + Sync>;
type Recorder = Arc<Mutex<Option<AgentRecorder>>>;
type Recorders = Arc<Mutex<HashMap<String, AgentRecorder>>>;

/// A lock that a panic elsewhere doesn't make unusable.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Marv's turn id: the time in base 36, a dash, four random base-36 digits.
fn turn_id() -> String {
    fn base36(mut n: u64) -> String {
        let mut digits = Vec::new();
        loop {
            digits.push(char::from_digit((n % 36) as u32, 36).unwrap_or('0'));
            n /= 36;
            if n == 0 {
                break;
            }
        }
        digits.iter().rev().collect()
    }
    let random = std::collections::hash_map::RandomState::new().build_hasher().finish();
    let tail: String = base36(random).chars().rev().take(4).collect();
    format!("{}-{tail}", base36(crate::time::now_ms()))
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "the turn panicked".into())
}

fn recorder(record: &Record, turn: &str, agent: &str) -> AgentRecorder {
    let record = record.clone();
    AgentRecorder::new(move |r| record(r), Who { turn: turn.into(), agent: agent.into() }, crate::time::now_ms)
}

/// A check's trajectory record: `note` is exactly what the model was told.
fn check_record<E>(
    turn: &str,
    source: Option<&'static str>,
    result: &CheckOutcome<E>,
    note: impl Fn(&[E]) -> String,
) -> TrajectoryRecord {
    let who = Who { turn: turn.into(), agent: "main".into() };
    let (status, ms, before, errors, added, reason, told) = match result {
        CheckOutcome::Done { ms, before, errors, added } => (
            CheckStatus::Done,
            Some(*ms),
            Some(*before),
            Some(*errors),
            Some(added.len()),
            None,
            (!added.is_empty()).then(|| note(added)),
        ),
        CheckOutcome::Failed { ms, reason, .. } => {
            let reason = match reason {
                FailReason::Timeout => "timeout",
                FailReason::Unreadable => "unreadable",
                FailReason::MissingInterpreter => "missing-interpreter",
            };
            (CheckStatus::Failed, Some(*ms), None, None, None, Some(reason.to_string()), None)
        }
        CheckOutcome::Off { reason } => {
            let reason = match reason {
                OffReason::NoSandbox => "no-sandbox",
                OffReason::Timeouts => "timeouts",
            };
            (CheckStatus::Off, None, None, None, None, Some(reason.to_string()), None)
        }
    };
    TrajectoryRecord::Check { who, source, status, ms, before, errors, added, reason, note: told }
}

/// A checkpoint snapshot's trajectory record.
fn checkpoint_record(turn: &str, kind: CheckpointKind, result: &SnapshotResult) -> TrajectoryRecord {
    let (ok, sha, files, message) = match result {
        SnapshotResult::Ok { sha, files, .. } => (true, Some(sha.clone()), Some(*files), None),
        SnapshotResult::Failed { message, .. } => (false, None, None, Some(message.clone())),
    };
    TrajectoryRecord::Checkpoint { turn: turn.into(), kind: kind.as_str(), ok, sha, files, ms: result.ms(), message }
}

/// Frees the session when a `compact()` ends, however it ends.
struct Free<'a> {
    session: &'a MarvSession,
    done: CancellationToken,
}

impl Drop for Free<'_> {
    fn drop(&mut self) {
        {
            let mut s = self.session.state();
            s.current = None;
            s.running = false;
        }
        self.done.cancel();
        self.session.schedule_save();
    }
}

/// The main agent's plan tool writes into the session's state (#15).
struct SessionPlan(MarvSession);

impl PlanHost for SessionPlan {
    fn set(&self, items: Vec<PlanItem>) {
        self.0.state().plan = (!items.is_empty()).then_some(items);
    }
}

/// Where a turn's subagents report: their events and progress become `SessionEvent`s, their usage the totals.
struct TurnSink {
    session: MarvSession,
    /// The turn's factory and model info: subagents are counted and priced as the turn's model.
    factory: Arc<dyn ProviderFactory>,
    info: Option<ModelInfo>,
    emit: Emit,
    subs: Recorders,
    /// The turn's checkpoint number: what shared-folder subagents' changes are reported under.
    turn_number: u32,
}

impl SubagentSink for TurnSink {
    fn usage(&self, usage: &Usage) {
        self.session.count(usage, self.factory.as_ref(), self.info.as_ref());
    }
    fn progress(&self, call_id: &str, progress: AgentProgress) {
        (self.emit)(SessionEvent::SubagentProgress { call_id: call_id.into(), progress });
    }
    fn event(&self, call_id: &str, event: &LoopEvent) {
        self.session.state().note_changed(self.turn_number, event);
        if let Some(sub) = lock(&self.subs).get_mut(call_id) {
            sub.event(event);
        }
        (self.emit)(SessionEvent::Subagent { call_id: call_id.into(), event: event.clone() });
    }
}

impl MarvSession {
    /// Builds a session. Fails (and changes nothing) for a taken tool name, or yolo where `no_yolo` forbids it.
    pub fn new(mut init: SessionInit) -> Result<Self, SessionError> {
        let process_on = init.processes && marv_sandbox::sandbox_available();
        // The process tool is built in only where the session offers it: elsewhere the name is free (Marv).
        let custom: Vec<ToolRef> = init.tools.iter().filter(|t| process_on || t.name() != "process").cloned().collect();
        check_tool_names(&custom, init.plan, init.github).map_err(SessionError::Invalid)?;
        // The main agent's plug (`agent.parent.inbox`), bound now so it's there before the first turn.
        if let Some(mesh) = &init.mesh {
            mesh.plug(crate::mesh::parent_id()).map_err(|e| SessionError::Invalid(format!("The mesh: {e}")))?;
        }
        if let Some(why) = &init.no_yolo
            && init.yolo
        {
            return Err(SessionError::Invalid(why.clone()));
        }
        let factory = factory_of(&init.provider, init.thinking);
        let provider = factory.make(None, None);
        let has_skills = !init.skills.is_empty();
        // Built once, so every request sends byte-identical tool definitions (the skill tool only when there are
        // skills, the plan tool unless it's off, github only where gh is logged in).
        let mut specs = marv_tools::tool_specs_for(has_skills, init.plan, init.github);
        specs.retain(|s| process_on || s.name != "process");
        specs.extend(init.tools.iter().map(|t| t.spec()));
        if init.mesh.is_some() {
            specs.extend(marv_mesh::tools::mesh_specs());
        }
        let saved = new_session(&init.root, factory.id(), factory.model(), crate::time::now_ms());
        let checkpoints = init.checkpoint_store.clone().unwrap_or_else(|| {
            Arc::new(CheckpointStore::new(StoreOptions::new(&init.root, init.checkpoints_dir.clone(), saved.id.clone())))
        });
        checkpoints.set_session(&saved.id);
        let chain_id = saved.id.clone();
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
            flushing: Vec::new(),
            working: None,
            turn_mcp_specs: Arc::default(),
            mcp_waited: false,
            plan: None,
            checkpoints: init.checkpoints,
            turn_number: 0,
            reported: Default::default(),
            checkpoint_failure: None,
            pending_note: None,
            chain_id,
            pruned: false,
        };
        // The config's pre-approved scopes (#98) seed the "always" layer: they mean "always, for this session",
        // through the same path a person's "always" takes, so must_ask and taint still win. An unknown name is a
        // problem (shown), not a silent approval.
        let always = AlwaysScopes::default();
        match crate::approvals::seed_prefixes(&init.pre_approved) {
            Ok(prefixes) => always.seed(&prefixes),
            Err(problem) => init.problems.push(problem),
        }
        let inner = Inner {
            specs,
            always,
            taint: Arc::new(TaintState::new()),
            skills: Arc::new(init.skills.iter().map(|s| s.skill.clone()).collect()),
            loaded: Arc::default(),
            nested: Arc::default(),
            typecheck: Arc::new(Checker::new(Arc::new(crate::diagnostics::turn::Typecheck), None)),
            project_checks: Arc::new(Checker::new(Arc::new(crate::checks::kind::ProjectChecksKind), None)),
            process_on,
            processes: init.process_manager.clone().or_else(|| {
                process_on.then(|| {
                    let mut options = ProcessManagerOptions::new(&init.root);
                    options.on_change = init.on_change.clone();
                    ProcessManager::new(options)
                })
            }),
            checkpoints,
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
    /// The plan tool's list (#15), or `None` when there's none; each change also arrives as a tool end's
    /// `result.plan`.
    pub fn plan(&self) -> Option<Vec<PlanItem>> {
        self.state().plan.clone()
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
    /// Who grants wires: `wire_approver`, else a person through the client's approver (never yolo).
    fn wire_approver(&self) -> Option<Arc<dyn WireApprover>> {
        crate::mesh::wire_approver(self.inner.init.wire_approver.clone(), self.inner.init.approve.as_ref())
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
        let done = CancellationToken::new();
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
            s.working = Some(done.clone());
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
        // Started here, not when the client first reads: the turn's end is what frees the session, so it must not
        // depend on the client reading. Events wait in the queue until it does.
        let work = tokio::spawn(async move {
            let push = queue.clone();
            session.run_turn(text, for_model, turn_stop, move |e| push.push(e)).await;
            queue.close();
            done.cancel();
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
    /// marv-rs: the parent's mesh inbox is read before the user message (its messages go in first, fenced) and
    /// after each step (`crate::mesh::with_inbox`).
    async fn run_turn(
        &self,
        text: String,
        for_model: String,
        stop: CancellationToken,
        push: impl Fn(SessionEvent) + Send + Sync + 'static,
    ) {
        let turn = turn_id();
        // Nothing after TurnEnd: a subagent's callback arriving late (after the turn ended) is dropped.
        let ended = Arc::new(AtomicBool::new(false));
        let emit: Emit = {
            let ended = ended.clone();
            Arc::new(move |event| {
                if !ended.load(Ordering::SeqCst) {
                    push(event);
                }
            })
        };
        emit(SessionEvent::TurnStart { turn: turn.clone() });
        let main: Recorder = Arc::default();
        // Subagents' recorders, by call id; ids are unique within the turn.
        let subs: Recorders = Arc::default();
        let pair: Pair = Arc::default();
        let work = AssertUnwindSafe(self.turn_body(&turn, &text, &for_model, &stop, &emit, &main, &subs, &pair));
        let reason = match work.catch_unwind().await {
            Ok(reason) => reason,
            Err(panic) => {
                // A provider (or a tool's own code) that panics instead of reporting an error: the turn ends, the
                // session lives on.
                emit(SessionEvent::Loop(LoopEvent::Error(format!("Error: {}", panic_message(panic.as_ref())))));
                TurnEndReason::Done(StopReason::Error)
            }
        };
        self.state().mid_turn_compact_failed = false; // the next turn tries compaction again
        for (_, mut sub) in lock(&subs).drain() {
            sub.finish("interrupted");
        }
        if let Some(main) = lock(&main).as_mut() {
            main.finish("error"); // only if the loop panicked: otherwise its Done already ended the turn
        }
        // Stop anything still running (a no-op after a normal end): subagents still running in a parallel group would
        // otherwise carry on unseen, and could still ask for approvals.
        stop.cancel();
        // The after snapshot, however the turn ended, once every call is answered (a pair that changed nothing goes).
        let pair = lock(&pair).take();
        if let Some(pair) = pair {
            pair.finish().await;
        }
        // The session is free before TurnEnd goes out, so a client can send the next message as soon as it sees it,
        // and a turn nobody reads frees it too. (A newer turn can't have started yet: send() refuses while running.)
        {
            let mut s = self.state();
            s.current = None;
            s.running = false;
        }
        self.schedule_save();
        emit(SessionEvent::TurnEnd { turn, reason });
        ended.store(true, Ordering::SeqCst);
    }

    /// `run_turn` up to the end of the loop: how the turn ended.
    #[allow(clippy::too_many_arguments)]
    async fn turn_body(
        &self,
        turn: &str,
        text: &str,
        for_model: &str,
        stop: &CancellationToken,
        emit: &Emit,
        main: &Recorder,
        subs: &Recorders,
        pair: &Pair,
    ) -> TurnEndReason {
        let init = &self.inner.init;
        // MCP servers still starting: their tools must be in place before the first request (the list can't change
        // after it, or the prompt cache would start over). Interrupting the wait ends the turn. A failed start still
        // settles: the turn goes on with whatever tools there are.
        //
        // marv-rs: the first turn with MCP servers always goes through the wait (and says so), settled or not. In
        // Marv the first turn's check always came before startup could finish (connecting takes I/O, the check
        // doesn't); here startup runs on another thread, and a server that connects in a few milliseconds raced the
        // check, so `waiting_for_mcp` came and went from run to run. A wait on settled servers returns at once.
        if let Some(mcp) = &init.mcp {
            let first = !self.state().mcp_waited && !mcp.status().is_empty();
            if first || !mcp.settled() {
                emit(SessionEvent::Status(TurnStatus::WaitingForMcp));
                tokio::select! {
                    _ = mcp.ready() => {}
                    _ = stop.cancelled() => return TurnEndReason::Interrupted,
                }
            }
            self.state().mcp_waited = true;
        }
        // Stopped before the message was even sent (an already-cancelled token, or interrupt() during the wait): it
        // isn't sent, so the conversation stays as it was, and nothing is logged (a turn the model never saw would
        // otherwise be in the trajectory, and the next message's tone would rate it).
        if stop.is_cancelled() {
            return TurnEndReason::Interrupted;
        }
        // What this turn runs with, all read now: settings changed while it runs (configure(), or the model list
        // arriving and remaking the provider) apply from the next turn. That includes the factory, so a subagent
        // naming its own model gets it from the same provider as the turn, and its usage is counted (local or not) as
        // that one's, priced with what the model list said about the turn's model.
        // The servers have settled: the first turn with MCP tools decides whether this conversation defers them (#14).
        let context = self.context_length();
        let mcp_choice = self.decide_mcp(context);
        let turn_specs = init.mcp.as_ref().map(|m| m.specs()).unwrap_or_default();
        self.state().turn_mcp_specs = turn_specs.clone();
        let (specs, tools) = self.offered();
        let (sandbox, yolo, factory, provider, info, diagnostics, checks_on, max_steps, history, system) = {
            let s = self.state();
            (
                s.sandbox,
                s.yolo,
                s.factory.clone(),
                s.provider.clone(),
                s.info.clone(),
                s.diagnostics,
                s.checks,
                s.max_steps,
                s.conversation.clone(),
                s.system.clone(),
            )
        };
        let log = self.trajectory();
        let mut tainted = self.inner.taint.sources().len();
        if let Some(mesh) = &init.mesh {
            crate::mesh::log_to(mesh, log.as_ref());
        }
        let record: Record = {
            let log = log.clone();
            Arc::new(move |r: TrajectoryRecord| {
                if let Some(log) = &log {
                    log.write(&r);
                }
            })
        };
        self.start_turn_log(log.as_ref(), turn, text, for_model, &specs, mcp_choice).await;
        *lock(main) = Some(recorder(&record, turn, "main"));

        // Nearly out of context: summarize first, so this message (and what follows) fits.
        if self.try_compact() {
            emit(SessionEvent::Status(TurnStatus::Compacting));
            let result = self.compact_if_full(&provider, factory.as_ref(), info.as_ref(), stop, false).await;
            emit(SessionEvent::Compaction(result.clone()));
            if matches!(result, CompactResult::NotCompacted { reason: CompactFailure::Stopped, .. }) {
                // Stopping meant "stop": the message too (it would run on the nearly full context).
                if let Some(main) = lock(main).as_mut() {
                    main.finish("aborted");
                }
                return TurnEndReason::Interrupted;
            }
        }
        // After a compaction above (which forgets what was announced), so the note then lists everything again. It
        // goes at the start of the user's message, never as a message of its own (two user messages in a row), and
        // never into the transcript.
        let mcp_note = self.announce(&turn_specs);
        if let Some(note) = &mcp_note {
            record(TrajectoryRecord::McpNote { turn: turn.into(), text: note.clone() });
        }
        let message = match &mcp_note {
            Some(note) => format!("{note}\n\n{for_model}"),
            None => for_model.to_string(),
        };
        // Mesh messages that came in since the last step: before the user's message, fenced as data.
        let inbox = init.mesh.as_ref().and_then(|m| crate::mesh::inbox_note(m, &crate::mesh::parent_id(), &self.inner.taint));
        let message = match inbox {
            Some(inbox) => format!("{inbox}\n\n{message}"),
            None => message,
        };
        // An undo since the last turn: the model hears of it here, in the message Marv sends anyway (the cache holds).
        let undo = self.state().pending_note.take();
        let message = match undo {
            Some(note) => format!("{note}\n\n{message}"),
            None => message,
        };
        lock(&history).push(ChatTurn::User { text: message });
        self.note(Role::User, text);
        emit(SessionEvent::Status(TurnStatus::Running));

        // The turn's checkpoint pair (#16): the before is taken by the first call that can change files.
        let (number, checkpoints_on, prune) = {
            let mut s = self.state();
            s.turn_number += 1;
            let number = s.turn_number;
            s.reported.insert(number, Vec::new());
            let prune = s.checkpoints && !s.pruned;
            s.pruned |= prune;
            (number, s.checkpoints, prune)
        };
        if prune {
            let store = self.inner.checkpoints.clone();
            tokio::spawn(async move { store.prune(20, 14).await });
        }
        let checkpoint = checkpoints_on.then(|| Arc::new(self.checkpoint_turn(number, text, turn, emit, &record)));
        *lock(pair) = checkpoint.clone();

        let approve: Option<Arc<dyn Approver>> = init.approve.clone().map(|inner| {
            Arc::new(SessionApprover { inner, always: self.inner.always.clone(), cancel: stop.clone() }) as Arc<dyn Approver>
        });
        let checks = self.turn_checks(turn, sandbox, diagnostics, checks_on, stop, emit, main, &record);
        let sink = Arc::new(TurnSink {
            session: self.clone(),
            factory: factory.clone(),
            info: info.clone(),
            emit: emit.clone(),
            subs: subs.clone(),
            turn_number: number,
        });
        // What the agent tool needs to start subagents. Only the main agent gets one; what a subagent does reaches
        // this conversation only as its tool result, and the client through Subagent events.
        let host = Arc::new(SessionAgentHost {
            agents: Arc::new(init.agents.clone()),
            cwd: self.cwd(),
            instructions: init.instructions.clone(),
            nested_instructions: init.nested_instructions,
            worktrees_dir: init.worktrees_dir.clone(),
            factory: factory.clone(),
            provider: provider.clone(),
            approver: init.approve.clone(),
            always: self.inner.always.clone(),
            sink,
            mesh: init.mesh.clone().map(|mesh| MeshAccess { mesh, approver: self.wire_approver() }),
        });
        let mut ctx = ToolContext::new(&init.root);
        let mut sandbox = marv_sandbox::Sandbox::from_env(sandbox);
        // The config's extra writable folders (#98) ride in every command's sandbox (an approved command's own
        // writable mounts, e.g. a worktree's .git, still go on top).
        sandbox.writable = init.writable.clone();
        ctx.sandbox = Arc::new(sandbox);
        ctx.home = ctx.sandbox.home.clone();
        ctx.cancel = stop.clone();
        ctx.approver = approve.clone();
        ctx.yolo = yolo;
        ctx.skills = self.inner.skills.clone();
        ctx.memory = init.memory.as_ref().map(|(paths, _)| paths.for_tools());
        ctx.agent_host = Some(host);
        // Where the plan tool writes; only the main agent gets it (as it does agent_host), so subagents have no plan.
        if init.plan {
            ctx.plan = Some(Arc::new(SessionPlan(self.clone())));
        }
        ctx.before_change = checks.as_ref().map(|c| c.before_change.clone());
        ctx.checkpoint = checkpoint.map(|c| c as Arc<dyn marv_tools::Checkpoint>);
        ctx.taint = self.inner.taint.clone();
        // The main agent's doors out of the sandbox, fixed for the session (the prompt names them: the cache).
        ctx.doors.host = init.host_commands;
        ctx.doors.github = init.github;
        // The process door (Marv #51): the main agent's alone. Subagents get the forwards (their bash reaches the
        // servers), never the processes.
        ctx.doors.process = self.inner.process_on;
        if self.inner.process_on {
            ctx.processes = self.inner.processes.clone().map(|m| Arc::new(m) as Arc<dyn marv_tools::ProcessHost>);
        }
        ctx.forwards = self.inner.processes.clone().map(|m| Arc::new(m) as Arc<dyn marv_tools::ProcessForwards>);
        let mut runner = SessionToolRunner::new(ctx, Arc::new(tools));
        if init.nested_instructions {
            runner.nested = Some(self.inner.nested.clone());
        }
        let runner = Arc::new(runner);

        let mut options = AgentOptions::new(provider.clone(), history, system, specs, runner.clone(), stop.clone());
        options.max_steps = max_steps.unwrap_or(marv_agent::agent::DEFAULT_MAX_STEPS);
        // Always one, even with no checks: the loop looks at the stop after `after_step`, so a stop during a mid-turn
        // compaction (on_full) ends the turn there instead of sending one more request (Marv stopped before it).
        options.after_step = Some(
            checks.map_or_else(|| -> marv_agent::agent::AfterStep { Arc::new(|_| Box::pin(async { None })) }, |c| c.after_step),
        );
        if let Some(mesh) = &init.mesh {
            let taint = self.inner.taint.clone();
            options.after_step =
                Some(crate::mesh::with_inbox(options.after_step.take(), mesh.clone(), crate::mesh::parent_id(), taint));
        }
        // A direct call to a deferred MCP tool runs as the mcp_call it meant (#14), and its result says so.
        let deferred = self.state().listing.as_ref().is_some_and(|l| l.mode == McpMode::Deferred);
        let mcp_names: HashSet<String> =
            if deferred { turn_specs.iter().map(|s| s.name.clone()).collect() } else { HashSet::new() };
        if !mcp_names.is_empty() {
            let runner = runner.clone();
            options.rewrite_call = Some(Arc::new(move |call: &ToolCall| {
                let (fixed, note) = as_mcp_call(call, &mcp_names)?;
                runner.note_rewrite(&call.id, note);
                Some(fixed)
            }));
        }
        // A long turn can fill the window by itself: check between steps, and compact there too. The events say when
        // ("Compacting the conversation…") and how it went, and the loop goes on from the summary.
        options.on_full = Some({
            let (session, provider, factory, info, stop, emit) =
                (self.clone(), provider.clone(), factory.clone(), info.clone(), stop.clone(), emit.clone());
            Arc::new(move || {
                let (session, provider, factory, info, stop, emit) =
                    (session.clone(), provider.clone(), factory.clone(), info.clone(), stop.clone(), emit.clone());
                Box::pin(async move {
                    if !session.try_compact() {
                        return false;
                    }
                    emit(SessionEvent::Status(TurnStatus::Compacting));
                    let result = session.compact_if_full(&provider, factory.as_ref(), info.as_ref(), &stop, true).await;
                    emit(SessionEvent::Compaction(result.clone()));
                    match result {
                        CompactResult::Compacted { .. } => true,
                        // Esc during the summary meant "stop the turn", not "keep going on the old context".
                        CompactResult::NotCompacted { reason: CompactFailure::Stopped, .. } => {
                            stop.cancel();
                            false
                        }
                        CompactResult::NotCompacted { .. } => false,
                    }
                })
            })
        });
        // At the step limit, ask instead of stopping dead; with no one to ask, it stops there. max_steps 0 (the guard
        // off) leaves on_limit unset, so there is nothing to ask.
        if let Some(approve) = approve.clone()
            && max_steps != Some(0)
        {
            let every = max_steps.unwrap_or(marv_agent::agent::DEFAULT_MAX_STEPS);
            options.on_limit = Some(Arc::new(move |steps| {
                let approve = approve.clone();
                Box::pin(async move {
                    // Only a yes goes on: "unavailable" (marv -p --approve answers that for anything outside its list)
                    // means no one could say so, and stops like a hard limit.
                    match approve.approve(step_limit_request(steps, every)).await {
                        Decision::Yes | Decision::Always => LimitAnswer::Continue,
                        Decision::Unavailable => LimitAnswer::Unavailable,
                        Decision::No => LimitAnswer::Stop,
                    }
                })
            }));
        }

        // With no one to say "keep going" (no approver: `marv -p`; or one that answers "unavailable": `marv -p
        // --approve`), the limit first gets one tools-off wrap-up reply (#59), the headless reply. After the user's "no"
        // the loop stops without it.
        options.wrap_up_at_limit = true;

        let mut reason = TurnEndReason::Done(StopReason::Error);
        let mut subagents = 0;
        let mut events = std::pin::pin!(marv_agent::run_agent(options));
        while let Some(event) = events.next().await {
            if let Some(main) = lock(main).as_mut() {
                main.event(&event);
            }
            self.state().note_changed(number, &event);
            // After a call's result, or the mesh inbox read before the turn or between steps: anything but a delta.
            let check_taint =
                !matches!(event, LoopEvent::TextDelta(_) | LoopEvent::ThinkingDelta(_) | LoopEvent::ToolCallDelta { .. });
            match &event {
                LoopEvent::Usage(usage) => {
                    self.state().last = Some(*usage);
                    self.count(usage, factory.as_ref(), info.as_ref());
                }
                LoopEvent::Assistant { text } => self.note(Role::Assistant, text),
                LoopEvent::ToolStart { call, .. } if call.name == "agent" => {
                    let args = marv_agent::agent::agent_args(&call.arguments);
                    subagents += 1;
                    let subagent = format!("{turn}.{subagents}");
                    record(TrajectoryRecord::SubagentStart {
                        who: Who { turn: turn.into(), agent: "main".into() },
                        subagent: subagent.clone(),
                        call: call.id.clone(),
                        agent_type: args.kind,
                        description: args.description,
                        prompt: args.prompt,
                        isolation: args.isolation,
                    });
                    lock(subs).insert(call.id.clone(), recorder(&record, turn, &subagent));
                }
                LoopEvent::ToolEnd { call, run } => {
                    // A subagent that failed before its loop started never sent Done.
                    if let Some(mut sub) = lock(subs).remove(&call.id) {
                        sub.finish(if run.result.is_error { "error" } else { "end" });
                    }
                }
                LoopEvent::Done(r) => reason = TurnEndReason::Done(*r),
                _ => {}
            }
            emit(SessionEvent::Loop(event));
            // marv-rs: a result that tainted the session (web, MCP, mesh, image) is shown and logged once.
            if check_taint {
                let sources = self.inner.taint.sources();
                for source in sources.iter().skip(tainted) {
                    record(TrajectoryRecord::Taint { turn: Some(turn.into()), source: *source });
                    emit(SessionEvent::Tainted { source: *source });
                }
                tainted = sources.len();
            }
        }
        reason
    }

    /// The typecheck and the project's checks after file changes: a baseline before the first, a check after each
    /// step that made one. Both can run at once: `Checking` lasts until the last of them is done.
    #[allow(clippy::too_many_arguments)]
    fn turn_checks(
        &self,
        turn: &str,
        sandbox: bool,
        diagnostics: bool,
        checks_on: bool,
        stop: &CancellationToken,
        emit: &Emit,
        main: &Recorder,
        record: &Record,
    ) -> Option<TurnChecks> {
        if !diagnostics && !checks_on {
            return None;
        }
        let checking = Arc::new(AtomicI32::new(0));
        let on_status: Arc<dyn Fn(bool) + Send + Sync> = {
            let (emit, main) = (emit.clone(), main.clone());
            Arc::new(move |on| {
                let now =
                    if on { checking.fetch_add(1, Ordering::SeqCst) + 1 } else { checking.fetch_sub(1, Ordering::SeqCst) - 1 };
                if on && now == 1 {
                    emit(SessionEvent::Status(TurnStatus::Checking));
                }
                if !on && now == 0 {
                    emit(SessionEvent::Status(TurnStatus::Running));
                    if let Some(main) = lock(&main).as_mut() {
                        main.restart_clock();
                    }
                }
            })
        };
        let root = self.inner.init.root.clone();
        let mut parts = Vec::new();
        if diagnostics {
            let (emit, record, turn, kind) = (emit.clone(), record.clone(), turn.to_string(), self.inner.typecheck.kind.clone());
            parts.extend(self.inner.typecheck.turn(TurnOptions {
                root: root.clone(),
                sandbox,
                cancel: stop.clone(),
                on_status: on_status.clone(),
                on_result: Arc::new(move |result: CheckResult| {
                    record(check_record(&turn, None, &result, |added| kind.note(added)));
                    emit(SessionEvent::Check(result));
                }),
            }));
        }
        if checks_on {
            let (emit, record, turn, kind) =
                (emit.clone(), record.clone(), turn.to_string(), self.inner.project_checks.kind.clone());
            parts.extend(self.inner.project_checks.turn(TurnOptions {
                root,
                sandbox,
                cancel: stop.clone(),
                on_status,
                on_result: Arc::new(move |result: ProjectCheckResult| {
                    record(check_record(&turn, Some("checks"), &result, |added| kind.note(added)));
                    emit(SessionEvent::Checks(result));
                }),
            }));
        }
        combine_checks(parts)
    }

    /// The turn's checkpoint pair: its snapshots go to the trajectory, a failure is said once (and kept for
    /// `undo_plan()` and /checkpoints), and a slow before shows as `Checkpointing`.
    fn checkpoint_turn(&self, number: u32, label: &str, turn: &str, emit: &Emit, record: &Record) -> CheckpointTurn {
        let (session, record, turn) = (self.clone(), record.clone(), turn.to_string());
        let on_snapshot = move |kind: CheckpointKind, r: &SnapshotResult| {
            record(checkpoint_record(&turn, kind, r));
            if r.ok() {
                session.state().checkpoint_failure = None;
            }
        };
        let (session, failed_emit, said) = (self.clone(), emit.clone(), AtomicBool::new(false));
        let on_failed = move |message: String| {
            session.state().checkpoint_failure = Some(message.clone());
            if !said.swap(true, Ordering::SeqCst) {
                failed_emit(SessionEvent::CheckpointFailed { message });
            }
        };
        let slow_emit = emit.clone();
        let on_slow =
            move |on: bool| slow_emit(SessionEvent::Status(if on { TurnStatus::Checkpointing } else { TurnStatus::Running }));
        CheckpointTurn::new(self.inner.checkpoints.clone(), number, label)
            .on_snapshot(Arc::new(on_snapshot))
            .on_failed(Arc::new(on_failed))
            .on_slow(Arc::new(on_slow))
    }

    /// Stops the running turn or compaction (Esc). Approvals still waiting are answered "no".
    pub fn interrupt(&self) {
        if let Some(stop) = &self.state().current {
            stop.cancel();
        }
    }

    /// `/compact [focus]`: summarizes the conversation now and continues from the summary. Fails during a turn.
    pub async fn compact(&self, focus: Option<&str>) -> Result<CompactResult, SessionError> {
        let stop = CancellationToken::new();
        let done = CancellationToken::new();
        let (provider, factory, info) = {
            let mut s = self.state();
            self.idle(&s)?;
            s.running = true;
            s.current = Some(stop.clone());
            s.working = Some(done.clone());
            (s.provider.clone(), s.factory.clone(), s.info.clone())
        };
        // Freed however it ends, a caller dropping the future included.
        let _free = Free { session: self, done };
        Ok(self.summarize_into(focus, &stop, &provider, factory.as_ref(), info.as_ref()).await)
    }

    /// Whether compaction should be tried at all now: nearly full, and no mid-turn summary has failed this turn.
    fn try_compact(&self) -> bool {
        self.nearly_full() && !self.state().mid_turn_compact_failed
    }

    /// When the context is nearly full, summarizes the conversation and continues from the summary, right where the
    /// turn is (also between steps: a long turn can fill the window by itself). Emits nothing: the caller emits the
    /// status and the compaction event around it, so the client shows one notice per compaction. `mid_turn`: between
    /// two steps (not before a user message), so the summary needs a "continue" note after it.
    async fn compact_if_full(
        &self,
        provider: &Arc<dyn Provider>,
        factory: &dyn ProviderFactory,
        info: Option<&ModelInfo>,
        stop: &CancellationToken,
        mid_turn: bool,
    ) -> CompactResult {
        if !self.try_compact() {
            return CompactResult::NotCompacted { reason: CompactFailure::Empty, error: "The context isn't nearly full.".into() };
        }
        let result = self.summarize_into(None, stop, provider, factory, info).await;
        match &result {
            CompactResult::Compacted { .. } if mid_turn => {
                // The compacted history ends with the model's own "Got it", so mid-turn the next request needs an
                // instruction to go on: it follows the summary here, before the loop's next request. Before a user
                // message, that message is the instruction.
                let specs = self.state().turn_mcp_specs.clone();
                let text = match self.announce(&specs) {
                    Some(note) => format!("{MID_TURN_NOTE}\n\n{note}"),
                    None => MID_TURN_NOTE.to_string(),
                };
                let history = self.state().conversation.clone();
                lock(&history).push(ChatTurn::User { text });
            }
            CompactResult::Compacted { .. } => {}
            // A summary that failed isn't tried again within the turn: the next step would ask for the same expensive
            // summary that just failed. The next turn (a new user message) tries again.
            CompactResult::NotCompacted { .. } => self.state().mid_turn_compact_failed = true,
        }
        result
    }

    /// Summarizes the conversation and continues from the summary (`marv_agent::compact`). Only what the model sees
    /// changes. The provider, factory and info are the caller's (a turn's `info` can be `None`, and a default would
    /// swap in the current one).
    async fn summarize_into(
        &self,
        focus: Option<&str>,
        stop: &CancellationToken,
        provider: &Arc<dyn Provider>,
        factory: &dyn ProviderFactory,
        info: Option<&ModelInfo>,
    ) -> CompactResult {
        let (conversation, system, before) = {
            let s = self.state();
            (s.conversation.clone(), s.system.clone(), s.last)
        };
        let history = lock(&conversation).clone();
        if history.is_empty() {
            return CompactResult::NotCompacted { reason: CompactFailure::Empty, error: "Nothing to compact yet.".into() };
        }
        let tools = self.offered().0;
        let on_usage = |usage: Usage| self.count(&usage, factory, info);
        let result = marv_agent::compact::summarize(SummarizeOptions {
            provider: provider.as_ref(),
            history: &history,
            system: &system,
            tools: &tools,
            cancel: stop.clone(),
            focus,
            on_usage: Some(&on_usage),
        })
        .await;
        let summary = match result {
            Ok(summary) => summary,
            Err(error) => {
                let reason = if stop.is_cancelled() { CompactFailure::Stopped } else { CompactFailure::Failed };
                return CompactResult::NotCompacted { reason, error };
            }
        };
        // In place: the loop holds this very history for the whole turn, so a compaction between its steps must
        // rewrite it where it stands; replacing the Arc would leave the loop on the full history.
        let plan = self.state().plan.clone().filter(|p| marv_tools::plan::plan_open(Some(p)));
        *lock(&conversation) = compacted_history(&summary, plan.as_deref().map(marv_tools::plan::plan_text).as_deref());
        {
            // A summary may drop the list of MCP tools: the next message announces them all again (the cache starts
            // over here anyway), and schemas seen before it may be gone too.
            let mut s = self.state();
            if let Some(listing) = s.listing.as_mut() {
                listing.announced.clear();
            }
            s.last = None;
        }
        lock(&self.inner.loaded).clear();
        // The summary replaces the tool results that carried nested instruction files (#62): give them again.
        self.inner.nested.reset();
        // From here the model sees the summary, not the turns before it: the trajectory needs it to say what the model
        // saw. The turn it follows only if that turn is in this file (after resume(), the last turn was in another one).
        if let Some(log) = self.trajectory() {
            let after = self.state().last_turn.clone().filter(|(_, session)| *session == log.session).map(|(turn, _)| turn);
            log.write(&TrajectoryRecord::Compact { turn: after, summary: summary.clone() });
        }
        CompactResult::Compacted { summary, tokens_before: before.map(|b| b.prompt_tokens + b.completion_tokens) }
    }

    /// `/clear`: a new conversation (and session file), with memory reloaded for the new system prompt. Totals stay.
    /// Saves the previous conversation first.
    pub async fn clear(&self) -> Result<(), SessionError> {
        {
            let transcript = self.transcript();
            let mut s = self.state();
            self.idle(&s)?;
            // The last turn's save may still be waiting: it must go to the old file, with the old conversation.
            self.save_now(&mut s, transcript);
            s.saved = new_session(&self.inner.init.root, s.factory.id(), s.factory.model(), crate::time::now_ms());
            s.new_chain(&self.inner.checkpoints);
            s.conversation = Arc::default();
            s.messages.clear();
            s.last = None;
            s.listing = None; // a new conversation decides its MCP mode again
            s.plan = None; // a new conversation, a new plan
        }
        lock(&self.inner.loaded).clear();
        self.inner.nested.reset();
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
        let (resumed, id) = {
            let transcript = self.transcript();
            let mut s = self.state();
            // A turn may have started while it loaded: swapping the conversation under it would mix the two.
            self.idle(&s)?;
            // The last turn's save may still be waiting: it belongs to the conversation being left.
            self.save_now(&mut s, transcript);
            lock(&self.inner.loaded).clear();
            // The saved conversation may lack a note this one had: give them again (a repeat is harmless).
            self.inner.nested.reset();
            s.conversation = Arc::new(Mutex::new(saved.conversation.clone()));
            s.messages = saved.transcript.clone();
            s.totals = saved.totals.clone();
            s.last = None;
            s.listing = saved.mcp_listing();
            // The model needs nothing extra: its last plan call (or the summary holding the plan) is still in the
            // conversation. A session without the plan tool restores none: compaction would tell the model to keep
            // updating it.
            s.plan = if self.inner.init.plan { saved.plan_items() } else { None };
            // marv-rs: the conversation still holds the web, MCP or mesh text that tainted it, so its taint comes back
            // with it (docs/SECURITY.md): egress keeps asking after `marv -c`.
            self.inner.taint.restore(&saved.taint_sources());
            let resumed = Resumed {
                id: saved.id.clone(),
                updated_at: saved.updated_at,
                model: saved.model.clone(),
                transcript: saved.transcript.clone(),
                totals: saved.totals.clone(),
                plan: s.plan.clone(),
            };
            let (saved_checkpoints, pending_note, id) = (saved.saved_checkpoints(), saved.pending_note(), saved.id.clone());
            s.saved = saved;
            s.new_chain(&self.inner.checkpoints);
            // Its chain (the saved one: a recovery below may have replaced the session's own), if it still exists:
            // numbering goes on after its last checkpointed turn, and an undo preview still knows what each turn reported.
            if let Some(saved) = saved_checkpoints {
                s.chain_id = saved.chain;
                self.inner.checkpoints.set_session(&s.chain_id);
                s.reported = saved.reported.into_iter().collect();
            }
            // An undo's note the model hadn't been sent yet: it still leads the next message (the conversation is
            // untouched).
            s.pending_note = pending_note;
            (resumed, id)
        };
        // One that can't be read is left as it is: this conversation's turns go on a chain of their own, so their
        // numbers can't meet its turns. Its id is saved with the session, so the next resume finds those turns.
        let chain = self.inner.checkpoints.chain().await;
        let mut s = self.state();
        match chain {
            Ok(chain) => s.turn_number = chain.iter().map(|c| c.turn).max().unwrap_or(0),
            Err(error) => {
                s.chain_id = format!("{id}.{}", crate::time::now_ms());
                self.inner.checkpoints.set_session(&s.chain_id);
                s.reported.clear();
                s.checkpoint_failure = Some(format!("{error}: the turns before this resume can't be undone"));
            }
        }
        Ok(Some(resumed))
    }

    /// /undo's preview: what `undo()` would put back, or why there's nothing to. Fails during a turn.
    pub async fn undo_plan(&self) -> Result<Result<UndoPlan, UndoRefusal>, SessionError> {
        let (on, failure) = {
            let s = self.state();
            self.idle(&s)?;
            (s.checkpoints, s.checkpoint_failure.clone())
        };
        let refusal = |reason: String| Ok(Err(UndoRefusal { reason, failed: false }));
        if !on {
            return refusal("checkpoints are off (/checkpoints on turns them on)".into());
        }
        let store = &self.inner.checkpoints;
        if let Some(off) = store.off() {
            return refusal(format!("checkpoints are off for this project: {off}"));
        }
        let reported = |turn: u32| self.state().reported.get(&turn).cloned().unwrap_or_default();
        let plan = plan_undo(store, reported, None).await;
        if let Err(refused) = &plan {
            if let Err(error) = store.locate().await {
                return refusal(format!("no checkpoints here: {error}"));
            }
            if let Some(failure) = failure {
                return Ok(Err(UndoRefusal {
                    reason: format!("{}; the last checkpoint failed: {failure}", refused.reason),
                    failed: refused.failed,
                }));
            }
        }
        Ok(plan)
    }

    /// /undo's yes: puts the plan's files back (after a snapshot of how they are now), and tells the model at the
    /// start of the next message. Fails during a turn, and if the plan is stale.
    pub async fn undo(&self, plan: &UndoPlan) -> Result<UndoResult, SessionError> {
        {
            let mut s = self.state();
            self.idle(&s)?;
            s.running = true; // no turn may start while files are being written back
        }
        struct Free<'a>(&'a MarvSession);
        impl Drop for Free<'_> {
            fn drop(&mut self) {
                self.0.state().running = false;
                self.0.schedule_save(); // the note waits in the session file, in case Marv exits before the next message
            }
        }
        let _free = Free(self);
        let result = apply_undo(&self.inner.checkpoints, plan, None).await.map_err(SessionError::Undo)?;
        if let Some(log) = self.trajectory() {
            let turn = self.state().last_turn.as_ref().map(|(turn, _)| turn.clone()).unwrap_or_default();
            log.write(&TrajectoryRecord::Checkpoint {
                turn,
                kind: CheckpointKind::PreUndo.as_str(),
                ok: true,
                sha: Some(result.pre_undo.clone()),
                files: Some(result.pre_undo_files),
                ms: result.pre_undo_ms,
                message: None,
            });
            let paths = result.restored.iter().chain(&result.recreated).chain(&result.deleted).cloned().collect();
            let skipped = result.refused.iter().map(|r| r.path.clone()).collect();
            log.write(&TrajectoryRecord::Undo { turn: plan.turn, paths, skipped });
        }
        if let Some(note) = undo_note(plan, &result) {
            let mut s = self.state();
            s.pending_note = Some(match s.pending_note.take() {
                Some(earlier) => format!("{earlier}\n\n{note}"),
                None => note,
            });
        }
        Ok(result)
    }

    /// For /checkpoints: why the last checkpoint failed, or why checkpoints are off for this project.
    pub fn checkpoint_problem(&self) -> CheckpointProblem {
        CheckpointProblem { failure: self.state().checkpoint_failure.clone(), off: self.inner.checkpoints.off() }
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
            let provider = factory.make(None, info.as_ref());
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
        if let Some(v) = changes.checkpoints {
            s.checkpoints = v;
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
        let transcript = self.transcript();
        let saved = self.snapshot(&mut self.state(), transcript);
        store.save(&saved).await?;
        Ok(())
    }

    /// The client's transcript, read without the session's lock held (its callback may read the session).
    fn transcript(&self) -> Option<Vec<Message>> {
        self.inner.init.transcript.as_ref().map(|f| f())
    }

    /// What `save()` writes: every tool call answered (a run winding down may still have unanswered calls).
    fn snapshot(&self, s: &mut State, transcript: Option<Vec<Message>>) -> SavedSession {
        let conversation = marv_agent::agent::answer_all_calls(&s.conversation.lock().unwrap_or_else(|e| e.into_inner()));
        let mut saved = s.saved.clone();
        saved.provider = s.factory.id().into();
        saved.model = s.factory.model().into();
        saved.conversation = conversation;
        saved.transcript = transcript.unwrap_or_else(|| s.messages.clone());
        saved.totals = s.totals.clone();
        saved.mcp = s.listing.as_ref().and_then(|l| serde_json::to_value(l).ok());
        saved.plan = s.plan.as_ref().and_then(|p| serde_json::to_value(p).ok());
        let checkpoints = SavedCheckpoints {
            chain: s.chain_id.clone(),
            reported: s.reported.iter().map(|(turn, paths)| (*turn, paths.clone())).collect(),
        };
        saved.checkpoints = serde_json::to_value(checkpoints).ok();
        saved.pending_note = s.pending_note.clone().map(serde_json::Value::String);
        saved.taint = Some(self.inner.taint.sources());
        s.saved = saved.clone();
        saved
    }

    /// The waiting save, now: before the conversation is swapped (clear, resume), or the timer would save the new one.
    /// The snapshot is taken here, so starting the write is enough; flush() waits for it to land.
    fn save_now(&self, s: &mut State, transcript: Option<Vec<Message>>) {
        if let Some(timer) = s.save_timer.take() {
            timer.abort();
            self.start_save(s, transcript);
        }
    }

    /// Writes the current snapshot in the background (`flushing`).
    fn start_save(&self, s: &mut State, transcript: Option<Vec<Message>>) {
        let (Some(store), Ok(handle)) = (self.inner.init.sessions.clone(), tokio::runtime::Handle::try_current()) else {
            return;
        };
        let saved = self.snapshot(s, transcript);
        s.flushing.retain(|h| !h.is_finished());
        s.flushing.push(handle.spawn(async move {
            let _ = store.save(&saved).await;
        }));
    }

    /// Saves shortly (a burst of turns is one write). When the timer fires it's `save_now`, so flush() waits for it.
    fn schedule_save(&self) {
        if self.inner.init.sessions.is_none() {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else { return };
        let session = self.clone();
        let mut s = self.state();
        if let Some(timer) = s.save_timer.take() {
            timer.abort();
        }
        s.save_timer = Some(handle.spawn(async move {
            tokio::time::sleep(SAVE_DELAY).await;
            let transcript = session.transcript();
            let mut s = session.state();
            // This task is the timer: taken, not aborted.
            if s.save_timer.take().is_some() {
                session.start_save(&mut s, transcript);
            }
        }));
    }

    /// Before exiting: the pending save, and the trajectory's queued records, on disk.
    pub async fn flush(&self) {
        let (timer, log, flushing) = {
            let mut s = self.state();
            (s.save_timer.take(), s.log.clone(), std::mem::take(&mut s.flushing))
        };
        if let Some(t) = timer {
            t.abort();
        }
        let _ = self.save().await;
        if let Some(log) = log {
            log.flush().await;
        }
        for write in flushing {
            let _ = write.await;
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
        // A turn's end saves its conversation (and arms a save timer): waiting for it means that save is part of
        // this flush, rather than one that lands after close() returned.
        if let Some(done) = working {
            done.cancelled().await;
        }
        self.flush().await;
        if self.inner.init.owns_mcp
            && let Some(mcp) = &self.inner.init.mcp
        {
            mcp.close().await;
        }
        if let Some(processes) = &self.inner.processes {
            processes.close_all().await;
        }
    }

    /// The background processes (Marv #51): name, command, ports, state.
    pub fn processes(&self) -> Vec<ProcessInfo> {
        self.inner.processes.as_ref().map(ProcessManager::list).unwrap_or_default()
    }

    /// Stops a background process and everything it started; false when there's none of that name.
    pub async fn stop_process(&self, name: &str) -> bool {
        match &self.inner.processes {
            Some(processes) => processes.stop(name).await,
            None => false,
        }
    }

    /// Stops every background process (Marv's exit); later starts refuse. `close()` does this too.
    pub async fn stop_processes(&self) {
        if let Some(processes) = &self.inner.processes {
            processes.close_all().await;
        }
    }

    /// Whether this session offers the process tool (bubblewrap works, and it wasn't turned off).
    pub fn processes_offered(&self) -> bool {
        self.inner.process_on
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
            doors: crate::prompt::PromptDoors { github: init.github, process: self.inner.process_on, host: init.host_commands },
            // #98: what's pre-approved, and which folders outside the project are writable — fixed per session.
            pre_approved: init.pre_approved.clone(),
            writable: init.writable.iter().map(|d| crate::paths::shorten_home(d, &crate::paths::home_dir())).collect(),
            date: crate::time::iso_date(crate::time::now_ms()),
        });
        match &init.system_prompt {
            Some(SystemPrompt::Append(more)) => format!("{base}\n\n{more}"),
            _ => base,
        }
    }

    /// What the model is offered: the built-in tools and the caller's (and the mesh's), then the MCP servers' (fixed
    /// once they've started), or in a deferred conversation `tool_search` and `mcp_call` in their place (#14).
    fn offered(&self) -> (Vec<ToolSpec>, Vec<ToolRef>) {
        let init = &self.inner.init;
        let mut specs = self.inner.specs.clone();
        // The plan, process and github tools only where the session offers them: elsewhere the model can't call
        // them, even by name.
        let mut tools: Vec<ToolRef> = marv_tools::builtin_tools()
            .iter()
            .filter(|t| {
                (init.plan || t.name() != "plan")
                    && (self.inner.process_on || t.name() != "process")
                    && (init.github || t.name() != "github")
            })
            .cloned()
            .collect();
        tools.extend(init.tools.iter().cloned());
        if let Some(mesh) = &init.mesh
            && let Ok(plug) = mesh.plug(crate::mesh::parent_id())
        {
            tools.extend(MeshTool::all(&plug, mesh, self.wire_approver()).into_iter().map(|t| Arc::new(t) as ToolRef));
        }
        let Some(mcp) = &init.mcp else { return (specs, tools) };
        let taint = self.inner.taint.clone();
        // Deferred: two constant specs instead of the servers', whatever the servers do (prompt cache).
        if self.state().listing.as_ref().is_some_and(|l| l.mode == McpMode::Deferred) {
            specs.extend(deferred_specs());
            let loaded = self.inner.loaded.clone();
            tools.push(Arc::new(ToolSearchTool { mcp: mcp.clone(), loaded: loaded.clone() }));
            tools.push(Arc::new(McpCallTool { mcp: mcp.clone(), taint, loaded }));
        } else {
            specs.extend(mcp.specs().iter().cloned());
            tools.extend(
                mcp.tools().iter().map(|t| Arc::new(McpToolAdapter { tool: t.clone(), taint: taint.clone() }) as ToolRef),
            );
        }
        (specs, tools)
    }

    /// The first turn with MCP tools decides the conversation's mode; it's kept until clear() (switching would change
    /// the tool list).
    fn decide_mcp(&self, context: Option<u64>) -> Option<ModeChoice> {
        let mcp = self.inner.init.mcp.as_ref()?;
        let mut s = self.state();
        let specs = mcp.specs();
        if s.listing.is_some() || specs.is_empty() {
            return None;
        }
        let choice = choose_mode(s.mcp_tools, &specs, context);
        s.listing = Some(McpListing { mode: choice.mode, announced: Vec::new() });
        Some(choice)
    }

    /// What the model hasn't been told about the MCP tools (new ones, gone ones), now marked as told; `None` when not
    /// deferred or nothing changed.
    fn announce(&self, specs: &[ToolSpec]) -> Option<String> {
        let mut s = self.state();
        let listing = s.listing.as_mut().filter(|l| l.mode == McpMode::Deferred)?;
        let names: HashSet<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        let added: Vec<ToolSpec> = specs.iter().filter(|s| !listing.announced.contains(&s.name)).cloned().collect();
        let gone: Vec<String> = listing.announced.iter().filter(|n| !names.contains(n.as_str())).cloned().collect();
        let note = announcement(&added, &gone);
        listing.announced = specs.iter().map(|s| s.name.clone()).collect();
        note
    }

    /// The session's setup (once per file), what this message says about the last turn, and the new turn.
    async fn start_turn_log(
        &self,
        log: Option<&Arc<Trajectory>>,
        turn: &str,
        text: &str,
        for_model: &str,
        specs: &[ToolSpec],
        mcp_choice: Option<ModeChoice>,
    ) {
        if let Some(log) = log {
            let init = &self.inner.init;
            if self.state().session_logged.as_deref() != Some(log.session.as_str()) {
                let git = crate::git::git_head(&init.root).await;
                let (provider, model, system) = {
                    let s = self.state();
                    (s.factory.id().to_string(), s.factory.model().to_string(), s.system.clone())
                };
                log.write(&TrajectoryRecord::Session {
                    root: init.root.to_string_lossy().into_owned(),
                    marv: init.version.clone(),
                    provider,
                    model,
                    system,
                    tools: specs.iter().map(|t| t.name.clone()).collect(),
                    git,
                });
                self.state().session_logged = Some(log.session.clone());
            }
            let (previous, record) = {
                let s = self.state();
                let record = TrajectoryRecord::TurnStart {
                    turn: turn.into(),
                    text: text.into(),
                    for_model: (for_model != text).then(|| for_model.to_string()),
                    provider: s.factory.id().into(),
                    model: s.factory.model().into(),
                    yolo: s.yolo,
                    sandbox: s.sandbox,
                    diagnostics: s.diagnostics,
                    checks: s.checks,
                    mcp_tools: mcp_choice,
                };
                (s.last_turn.clone(), record)
            };
            // A /skill's arguments aren't a reply.
            let signal = if for_model == text { classify_reply(text) } else { None };
            if let (Some((previous, session)), Some(signal)) = (previous, signal)
                && session == log.session
            {
                log.write(&TrajectoryRecord::Feedback {
                    turn: previous,
                    score: signal.score,
                    source: "implicit",
                    labels: None,
                    note: None,
                    phrase: Some(signal.phrase),
                });
            }
            log.write(&record);
        }
        // Only a logged turn can be rated: feedback for a turn the file never recorded would be an orphan.
        self.state().last_turn = log.map(|l| (turn.to_string(), l.session.clone()));
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
                // What the list says changes the request: the reasoning switch, and the output limit (#96).
                if info.reasoning.is_some() || info.max_output.is_some() {
                    s.provider = factory.make(None, Some(&info));
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
            // The previous conversation's log may still have records queued: flush() must wait for those too.
            if let (Some(old), Ok(handle)) = (s.log.take(), tokio::runtime::Handle::try_current()) {
                s.flushing.push(handle.spawn(async move { old.flush().await }));
            }
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
        assert!(check_tool_names(&tools(&["deploy"]), true, false).is_ok());
        assert!(check_tool_names(&tools(&["bash"]), true, false).unwrap_err().contains("already built in"));
        assert!(check_tool_names(&tools(&["mcp_call"]), true, false).unwrap_err().contains("already built in"));
        assert!(check_tool_names(&tools(&["mcp__x__y"]), true, false).unwrap_err().contains("mcp__"));
        assert!(check_tool_names(&tools(&["a", "a"]), true, false).unwrap_err().contains("Two tools"));
        // plan is a free name where the session doesn't offer it (#15).
        assert!(check_tool_names(&tools(&["plan"]), true, false).unwrap_err().contains("\"plan\" is already built in"));
        assert!(check_tool_names(&tools(&["plan"]), false, false).is_ok());
        // So is github (#47).
        assert!(check_tool_names(&tools(&["github"]), true, false).is_ok());
        assert!(check_tool_names(&tools(&["github"]), true, true).unwrap_err().contains("already built in"));
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

/// Ports `tests/session.test.ts` (the parts whose collaborators are ported: no subagents, worktrees, MCP manager
/// lifecycle or typecheck runs yet).
#[cfg(test)]
mod turn_tests {
    use super::*;
    use futures::future::BoxFuture;
    use marv_core::ProviderEvent;
    use marv_provider::{EventStream, ReasoningSupport, StreamOptions};
    use marv_tools::ApprovalRequest;
    use std::path::Path;

    type Requests = Arc<Mutex<Vec<(Vec<ChatTurn>, StreamOptions)>>>;

    /// A model that follows a script: each request gets the next list of events. Records every request.
    #[derive(Clone)]
    struct Scripted {
        steps: Arc<Mutex<std::collections::VecDeque<Vec<ProviderEvent>>>>,
        requests: Requests,
        context: Option<u64>,
    }

    impl Scripted {
        fn new(steps: Vec<Vec<ProviderEvent>>) -> Self {
            Self { steps: Arc::new(Mutex::new(steps.into())), requests: Arc::default(), context: None }
        }
        fn with_context(mut self, context: u64) -> Self {
            self.context = Some(context);
            self
        }
        fn requests(&self) -> Vec<(Vec<ChatTurn>, StreamOptions)> {
            lock(&self.requests).clone()
        }
        fn last_texts(&self) -> Vec<String> {
            self.requests().iter().map(|(h, _)| text_of(h.last().unwrap()).to_string()).collect()
        }
    }

    impl Provider for Scripted {
        fn name(&self) -> &str {
            "scripted"
        }
        fn context_length(&self) -> Option<u64> {
            self.context
        }
        fn stream(&self, history: &[ChatTurn], options: StreamOptions) -> EventStream {
            lock(&self.requests).push((history.to_vec(), options));
            let step = lock(&self.steps).pop_front().unwrap_or_else(|| say("(script ended)"));
            Box::pin(futures::stream::iter(step).then(|e| async move {
                tokio::task::yield_now().await;
                e
            }))
        }
    }

    /// A model that says something, then works until it's stopped.
    #[derive(Default)]
    struct Hanging {
        requests: std::sync::atomic::AtomicUsize,
    }

    impl Provider for Hanging {
        fn name(&self) -> &str {
            "hanging"
        }
        fn context_length(&self) -> Option<u64> {
            None
        }
        fn stream(&self, _history: &[ChatTurn], options: StreamOptions) -> EventStream {
            self.requests.fetch_add(1, Ordering::SeqCst);
            let cancel = options.cancel.clone();
            Box::pin(futures::stream::once(async { ProviderEvent::TextDelta("Working".into()) }).chain(futures::stream::once(
                async move {
                    cancel.cancelled().await;
                    ProviderEvent::Done { reason: None }
                },
            )))
        }
    }

    /// Calls `f` for each request, then streams what it returns.
    struct Hook<F>(F);
    impl<F: Fn(&[ChatTurn], &StreamOptions) -> EventStream + Send + Sync> Provider for Hook<F> {
        fn name(&self) -> &str {
            "hook"
        }
        fn context_length(&self) -> Option<u64> {
            Some(1000)
        }
        fn stream(&self, history: &[ChatTurn], options: StreamOptions) -> EventStream {
            (self.0)(history, &options)
        }
    }

    type Make = Arc<dyn Fn(Option<&str>, Option<&ModelInfo>) -> Arc<dyn Provider> + Send + Sync>;
    type Lookup = Arc<dyn Fn() -> BoxFuture<'static, Option<ModelInfo>> + Send + Sync>;

    struct TestFactory {
        id: String,
        model: String,
        local: bool,
        make: Make,
        lookup: Option<Lookup>,
    }

    impl ProviderFactory for TestFactory {
        fn id(&self) -> &str {
            &self.id
        }
        fn model(&self) -> &str {
            &self.model
        }
        fn local(&self) -> bool {
            self.local
        }
        fn make(&self, model: Option<&str>, info: Option<&ModelInfo>) -> Arc<dyn Provider> {
            (self.make)(model, info)
        }
        fn lookup(&self) -> Option<BoxFuture<'static, Option<ModelInfo>>> {
            self.lookup.as_ref().map(|l| l())
        }
    }

    fn fixed(provider: Arc<dyn Provider>) -> TestFactory {
        TestFactory {
            id: "test".into(),
            model: "test-model".into(),
            local: false,
            make: Arc::new(move |_, _| provider.clone()),
            lookup: None,
        }
    }

    fn say(text: &str) -> Vec<ProviderEvent> {
        vec![ProviderEvent::TextDelta(text.into()), ProviderEvent::Done { reason: Some("stop".into()) }]
    }
    fn use_tools(calls: Vec<ToolCall>) -> Vec<ProviderEvent> {
        let mut events: Vec<ProviderEvent> = calls.into_iter().map(ProviderEvent::ToolCall).collect();
        events.push(ProviderEvent::Done { reason: Some("tool_calls".into()) });
        events
    }
    fn call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall { id: id.into(), name: name.into(), arguments: args.to_string() }
    }
    /// A read of a file named after the call (each call differs, so the tool-loop guard stays out of it).
    fn read(id: &str) -> ToolCall {
        call(id, "read_file", serde_json::json!({ "path": id }))
    }
    fn used(prompt_tokens: u64) -> ProviderEvent {
        ProviderEvent::Usage(Usage { prompt_tokens, completion_tokens: 0, cached_tokens: None, cost: None })
    }
    fn text_of(turn: &ChatTurn) -> &str {
        match turn {
            ChatTurn::User { text } | ChatTurn::Assistant { text, .. } | ChatTurn::Tool { text, .. } => text,
        }
    }
    fn role_of(turn: &ChatTurn) -> &'static str {
        match turn {
            ChatTurn::User { .. } => "user",
            ChatTurn::Assistant { .. } => "assistant",
            ChatTurn::Tool { .. } => "tool",
        }
    }
    /// Tool calls in a history without a result: a request with one is rejected by the API.
    fn unanswered(history: &[ChatTurn]) -> Vec<String> {
        let answered: HashSet<&str> = history
            .iter()
            .filter_map(|t| if let ChatTurn::Tool { call_id, .. } = t { Some(call_id.as_str()) } else { None })
            .collect();
        history
            .iter()
            .flat_map(|t| if let ChatTurn::Assistant { tool_calls, .. } = t { tool_calls.clone() } else { vec![] })
            .filter(|c| !answered.contains(c.id.as_str()))
            .map(|c| c.id)
            .collect()
    }

    /// A temp project with notes.txt, and a temp folder for stores; removed on drop.
    struct Dirs {
        project: PathBuf,
        dir: PathBuf,
    }
    impl Dirs {
        fn new() -> Self {
            let base = std::env::temp_dir().join(format!("marv-turn-{}-{}", std::process::id(), marv_fs::random_hex()));
            let (project, dir) = (base.join("project"), base.join("store"));
            std::fs::create_dir_all(&project).unwrap();
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(project.join("notes.txt"), "remember the milk\n").unwrap();
            Self { project, dir }
        }
    }
    impl Drop for Dirs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.project.parent().unwrap());
        }
    }

    fn make(dirs: &Dirs, factory: TestFactory, edit: impl FnOnce(&mut SessionInit)) -> MarvSession {
        let mut init = SessionInit::new(&dirs.project, ProviderChoice::Factory(Arc::new(factory)));
        init.diagnostics = false; // off unless a test turns them on (checks have their own tests, #13)
        init.checks = false;
        edit(&mut init);
        MarvSession::new(init).unwrap()
    }
    fn scripted(dirs: &Dirs, provider: &Scripted, edit: impl FnOnce(&mut SessionInit)) -> MarvSession {
        make(dirs, fixed(Arc::new(provider.clone())), edit)
    }

    fn go(session: &MarvSession, text: &str) -> Turn {
        session.send(text, SendOptions::default()).unwrap()
    }
    async fn collect(mut turn: Turn) -> Vec<SessionEvent> {
        let mut out = Vec::new();
        while let Some(e) = turn.next().await {
            out.push(e);
        }
        out
    }
    fn kind(e: &SessionEvent) -> &'static str {
        match e {
            SessionEvent::TurnStart { .. } => "turn_start",
            SessionEvent::Status(_) => "status",
            SessionEvent::Loop(LoopEvent::TextDelta(_)) => "text_delta",
            SessionEvent::Loop(LoopEvent::Assistant { .. }) => "assistant",
            SessionEvent::Loop(LoopEvent::Done(_)) => "done",
            SessionEvent::Loop(LoopEvent::Usage(_)) => "usage",
            SessionEvent::Loop(LoopEvent::ToolStart { .. }) => "tool_start",
            SessionEvent::Loop(LoopEvent::ToolEnd { .. }) => "tool_end",
            SessionEvent::Loop(LoopEvent::Error(_)) => "error",
            SessionEvent::Loop(LoopEvent::Compacted) => "compacted",
            SessionEvent::Loop(LoopEvent::StepLimit { .. }) => "step_limit",
            SessionEvent::Loop(_) => "loop",
            SessionEvent::Compaction(_) => "compaction",
            SessionEvent::TurnEnd { .. } => "turn_end",
            _ => "other",
        }
    }
    fn kinds(events: &[SessionEvent]) -> Vec<&'static str> {
        events.iter().map(kind).collect()
    }
    fn end_reason(events: &[SessionEvent]) -> TurnEndReason {
        match events.last() {
            Some(SessionEvent::TurnEnd { reason, .. }) => *reason,
            other => panic!("not a turn_end: {other:?}"),
        }
    }
    fn has_done(events: &[SessionEvent], reason: StopReason) -> bool {
        events.iter().any(|e| matches!(e, SessionEvent::Loop(LoopEvent::Done(r)) if *r == reason))
    }
    fn compactions(events: &[SessionEvent]) -> Vec<CompactResult> {
        events.iter().filter_map(|e| if let SessionEvent::Compaction(r) = e { Some(r.clone()) } else { None }).collect()
    }
    fn statuses(events: &[SessionEvent], status: TurnStatus) -> usize {
        events.iter().filter(|e| matches!(e, SessionEvent::Status(s) if *s == status)).count()
    }

    /// Answers every request the same way, and records them.
    struct Answer(Mutex<Vec<ApprovalRequest>>, Decision);
    #[async_trait::async_trait]
    impl Approver for Answer {
        async fn approve(&self, request: ApprovalRequest) -> Decision {
            lock(&self.0).push(request);
            self.1
        }
    }
    fn answer(decision: Decision) -> Arc<Answer> {
        Arc::new(Answer(Mutex::default(), decision))
    }

    async fn until(condition: impl Fn() -> bool) -> bool {
        for _ in 0..100 {
            if condition() {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        condition()
    }

    async fn records(store: &TrajectoryStore, root: &Path, session: &MarvSession) -> Vec<serde_json::Value> {
        session.flush().await;
        std::fs::read_to_string(store.open(root, &session.id()).path)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    // ---- a turn ----

    #[tokio::test]
    async fn starts_with_turn_start_ends_with_turn_end_and_streams_the_reply_in_between() {
        let dirs = Dirs::new();
        let events = collect(go(&scripted(&dirs, &Scripted::new(vec![say("Hello.")]), |_| {}), "hi")).await;
        assert_eq!(kinds(&events), ["turn_start", "status", "text_delta", "assistant", "done", "turn_end"]);
        let (SessionEvent::TurnStart { turn: start }, SessionEvent::TurnEnd { turn: end, reason }) =
            (&events[0], events.last().unwrap())
        else {
            panic!()
        };
        assert_eq!((start, *reason), (end, TurnEndReason::Done(StopReason::End)));
    }

    #[tokio::test]
    async fn one_at_a_time() {
        let dirs = Dirs::new();
        let provider = Scripted::new(vec![say("One."), say("Two.")]);
        let session = scripted(&dirs, &provider, |_| {});
        let first = go(&session, "one");
        assert!(session.busy());
        assert!(session.send("two", SendOptions::default()).err().unwrap().to_string().contains("working"));
        collect(first).await;
        assert!(!session.busy());
        collect(go(&session, "two")).await;
        assert_eq!(provider.last_texts(), ["one", "two"]);
    }

    #[tokio::test]
    async fn interrupt_stops_the_running_turn() {
        let dirs = Dirs::new();
        let session = make(&dirs, fixed(Arc::new(Hanging::default())), |_| {});
        let mut turn = go(&session, "go");
        let mut events = Vec::new();
        while let Some(e) = turn.next().await {
            if kind(&e) == "text_delta" {
                session.interrupt();
            }
            events.push(e);
        }
        assert!(has_done(&events, StopReason::Aborted));
        assert_eq!(end_reason(&events), TurnEndReason::Done(StopReason::Aborted));
    }

    #[tokio::test]
    async fn so_does_the_token_passed_to_send() {
        let dirs = Dirs::new();
        let session = make(&dirs, fixed(Arc::new(Hanging::default())), |_| {});
        let stop = CancellationToken::new();
        let mut turn = session.send("go", SendOptions { cancel: Some(stop.clone()), ..Default::default() }).unwrap();
        let mut events = Vec::new();
        while let Some(e) = turn.next().await {
            if kind(&e) == "text_delta" {
                stop.cancel();
            }
            events.push(e);
        }
        assert_eq!(end_reason(&events), TurnEndReason::Done(StopReason::Aborted));
    }

    #[tokio::test]
    async fn leaving_early_interrupts_the_turn_and_every_tool_call_still_gets_a_result() {
        let dirs = Dirs::new();
        let provider = Scripted::new(vec![use_tools(vec![read("c1"), read("c2")]), say("Next.")]);
        let session = scripted(&dirs, &provider, |_| {});
        let mut turn = go(&session, "read it twice");
        while let Some(e) = turn.next().await {
            if kind(&e) == "tool_start" {
                break;
            }
        }
        turn.finish().await;
        assert!(!session.busy());
        collect(go(&session, "next")).await;
        assert_eq!(unanswered(&provider.requests()[1].0), Vec::<String>::new());
    }

    #[tokio::test]
    async fn usage_adds_up_and_the_last_requests_size_is_kept() {
        let dirs = Dirs::new();
        let mut events =
            vec![ProviderEvent::Usage(Usage { prompt_tokens: 100, completion_tokens: 20, cached_tokens: None, cost: None })];
        events.extend(say("Hi."));
        let provider = Scripted::new(vec![events]);
        let session = make(&dirs, TestFactory { local: true, ..fixed(Arc::new(provider)) }, |_| {});
        collect(go(&session, "hi")).await;
        let usage = session.usage();
        assert_eq!(usage.last.map(|u| (u.prompt_tokens, u.completion_tokens)), Some((100, 20)));
        assert_eq!((usage.totals.requests, usage.totals.prompt_tokens, usage.totals.local), (1, 100, Some(true)));
    }

    #[tokio::test]
    async fn a_provider_error_ends_the_turn_with_reason_error() {
        let dirs = Dirs::new();
        let session = scripted(&dirs, &Scripted::new(vec![vec![ProviderEvent::Error("bad key".into())]]), |_| {});
        let events = collect(go(&session, "hi")).await;
        assert!(events.iter().any(|e| matches!(e, SessionEvent::Loop(LoopEvent::Error(m)) if m == "bad key")));
        assert_eq!(end_reason(&events), TurnEndReason::Done(StopReason::Error));
    }

    #[tokio::test]
    async fn a_provider_that_panics_ends_the_turn_not_the_session() {
        let dirs = Dirs::new();
        let broken = Hook(|_: &[ChatTurn], _: &StreamOptions| -> EventStream { panic!("kaput") });
        let session = make(&dirs, fixed(Arc::new(broken)), |_| {});
        let events = collect(go(&session, "hi")).await;
        assert!(events.iter().any(|e| matches!(e, SessionEvent::Loop(LoopEvent::Error(m)) if m == "Error: kaput")));
        assert_eq!(end_reason(&events), TurnEndReason::Done(StopReason::Error));
        assert!(!session.busy());
    }

    #[tokio::test]
    async fn the_system_prompt_replaces_marvs_or_adds_to_it() {
        let dirs = Dirs::new();
        let replaced = Scripted::new(vec![say("ok")]);
        let s = scripted(&dirs, &replaced, |i| i.system_prompt = Some(SystemPrompt::Replace("You are a pirate.".into())));
        collect(go(&s, "hi")).await;
        assert_eq!(replaced.requests()[0].1.system.as_deref(), Some("You are a pirate."));
        let appended = Scripted::new(vec![say("ok")]);
        let s = scripted(&dirs, &appended, |i| i.system_prompt = Some(SystemPrompt::Append("Answer briefly.".into())));
        collect(go(&s, "hi")).await;
        let system = appended.requests()[0].1.system.clone().unwrap();
        assert!(system.starts_with("You are Marv") && system.ends_with("Answer briefly."));
    }

    // ---- approvals ----

    #[tokio::test]
    async fn without_an_approver_a_yolo_safe_edit_runs_anything_else_is_refused_and_the_turn_goes_on() {
        let dirs = Dirs::new();
        let provider = Scripted::new(vec![
            use_tools(vec![
                call("c1", "write_file", serde_json::json!({ "path": "a.txt", "content": "hi\n" })),
                call("c2", "bash", serde_json::json!({ "command": "curl example.com", "network": true })),
            ]),
            say("Done."),
        ]);
        let events = collect(go(&scripted(&dirs, &provider, |_| {}), "go")).await;
        assert_eq!(std::fs::read_to_string(dirs.project.join("a.txt")).unwrap(), "hi\n");
        let refused = events.iter().find_map(|e| match e {
            SessionEvent::Loop(LoopEvent::ToolEnd { call, run }) if call.id == "c2" => Some(run.result.clone()),
            _ => None,
        });
        let refused = refused.unwrap();
        assert!(refused.is_error && refused.output.contains("no one to ask"), "{}", refused.output);
        assert!(has_done(&events, StopReason::End));
    }

    #[tokio::test]
    async fn always_covers_that_scope_for_the_rest_of_the_session() {
        let dirs = Dirs::new();
        let provider = Scripted::new(vec![
            use_tools(vec![call("c1", "write_file", serde_json::json!({ "path": "a.txt", "content": "1" }))]),
            say("One."),
            use_tools(vec![call("c2", "write_file", serde_json::json!({ "path": "b.txt", "content": "2" }))]),
            say("Two."),
        ]);
        let approver = answer(Decision::Always);
        let session = scripted(&dirs, &provider, |i| {
            i.yolo = false;
            i.approve = Some(approver.clone());
        });
        collect(go(&session, "one")).await;
        collect(go(&session, "two")).await;
        assert_eq!(lock(&approver.0).iter().map(|r| r.label.clone()).collect::<Vec<_>>(), ["a.txt"]);
        assert!(dirs.project.join("b.txt").exists());
    }

    struct Stopping(Mutex<Option<MarvSession>>);
    #[async_trait::async_trait]
    impl Approver for Stopping {
        async fn approve(&self, _request: ApprovalRequest) -> Decision {
            if let Some(s) = lock(&self.0).as_ref() {
                s.interrupt();
            }
            futures::future::pending().await // never answers: only the stop can
        }
    }

    #[tokio::test]
    async fn stopping_the_turn_answers_a_pending_approval_with_no() {
        let dirs = Dirs::new();
        let provider = Scripted::new(vec![use_tools(vec![call(
            "c1",
            "write_file",
            serde_json::json!({ "path": "a.txt", "content": "1" }),
        )])]);
        let approver = Arc::new(Stopping(Mutex::default()));
        let session = scripted(&dirs, &provider, |i| {
            i.yolo = false;
            i.approve = Some(approver.clone());
        });
        *lock(&approver.0) = Some(session.clone());
        let events = collect(go(&session, "go")).await;
        *lock(&approver.0) = None;
        assert!(events.iter().any(
            |e| matches!(e, SessionEvent::Loop(LoopEvent::ToolEnd { run, .. }) if run.declined && run.result.summary.as_deref() == Some("interrupted"))
        ));
        assert!(!dirs.project.join("a.txt").exists());
        assert_eq!(end_reason(&events), TurnEndReason::Done(StopReason::Aborted));
    }

    fn reads(n: usize) -> Vec<Vec<ProviderEvent>> {
        (0..n).map(|i| use_tools(vec![read(&format!("c{i}"))])).collect()
    }

    #[tokio::test]
    async fn without_an_approver_it_stops_at_the_step_limit_and_max_steps_moves_it() {
        let dirs = Dirs::new();
        let provider = Scripted::new(reads(30));
        let events = collect(go(&scripted(&dirs, &provider, |_| {}), "loop")).await;
        // 25 steps, then the wrap-up request (#59).
        assert_eq!(provider.requests().len(), 26);
        assert!(has_done(&events, StopReason::MaxSteps));
        let provider = Scripted::new(reads(40));
        let events = collect(go(&scripted(&dirs, &provider, |i| i.max_steps = Some(30)), "loop")).await;
        assert_eq!(provider.requests().len(), 31);
        assert!(has_done(&events, StopReason::MaxSteps));
    }

    /// The text of the turn's last `Assistant` event.
    fn last_reply(events: &[SessionEvent]) -> Option<String> {
        events.iter().rev().find_map(|e| match e {
            SessionEvent::Loop(LoopEvent::Assistant { text }) => Some(text.clone()),
            _ => None,
        })
    }

    #[tokio::test]
    async fn with_no_one_to_ask_the_step_limit_gets_a_tools_off_wrap_up_reply() {
        // #59: `marv -p` without --approve has no on_limit. One more request, with Marv's note and tool_choice none;
        // the tools are still sent (same list: the cached prefix holds). The reply is the turn's last word.
        let dirs = Dirs::new();
        let mut steps = reads(4);
        steps.push(say("Read 4 files. Left: the fifth. Next: read it."));
        let provider = Scripted::new(steps);
        let events = collect(go(&scripted(&dirs, &provider, |i| i.max_steps = Some(4)), "loop")).await;
        let requests = provider.requests();
        assert_eq!(requests.len(), 5);
        let (history, options) = &requests[4];
        assert_eq!(options.tool_choice, marv_core::ToolChoice::None);
        assert_eq!(options.tools, requests[3].1.tools);
        assert_eq!(options.system, requests[3].1.system);
        assert!(requests[..4].iter().all(|(_, o)| o.tool_choice == marv_core::ToolChoice::Auto));
        assert_eq!(text_of(history.last().unwrap()), marv_agent::agent::STEP_LIMIT_NOTE);
        assert!(history.starts_with(&requests[3].0), "append-only");
        assert_eq!(last_reply(&events).as_deref(), Some("Read 4 files. Left: the fifth. Next: read it."));
        assert!(has_done(&events, StopReason::MaxSteps));
        assert_eq!(end_reason(&events), TurnEndReason::Done(StopReason::MaxSteps));
    }

    #[tokio::test]
    async fn an_unavailable_answer_at_the_step_limit_gets_the_wrap_up_too_and_a_stray_call_is_not_run() {
        // `marv -p --approve …` answers the step-limit question "unavailable": no one could say keep going.
        let dirs = Dirs::new();
        let mut steps = reads(3);
        steps.push(use_tools(vec![call("w1", "write_file", serde_json::json!({ "path": "late.txt", "content": "x" }))]));
        let provider = Scripted::new(steps);
        let approver = answer(Decision::Unavailable);
        let session = scripted(&dirs, &provider, |i| {
            i.max_steps = Some(3);
            i.approve = Some(approver.clone());
        });
        let events = collect(go(&session, "loop")).await;
        assert_eq!(provider.requests().len(), 4);
        assert_eq!(provider.requests()[3].1.tool_choice, marv_core::ToolChoice::None);
        // The call the wrap-up made anyway (Ollama has no tool_choice) never ran, and has its result.
        assert!(!dirs.project.join("late.txt").exists());
        let history = session.conversation();
        assert!(unanswered(&history).is_empty());
        assert!(
            history.iter().any(|t| matches!(t, ChatTurn::Tool { text, .. } if text == marv_agent::agent::not_run::STEP_LIMIT))
        );
        assert_eq!(end_reason(&events), TurnEndReason::Done(StopReason::MaxSteps));
    }

    #[tokio::test]
    async fn a_no_at_the_step_limit_stops_without_a_wrap_up() {
        // The TUI's "Keep going?" flow is unchanged: the user chose to stop.
        let dirs = Dirs::new();
        let provider = Scripted::new(reads(10));
        let approver = answer(Decision::No);
        let session = scripted(&dirs, &provider, |i| {
            i.max_steps = Some(4);
            i.approve = Some(approver.clone());
        });
        let events = collect(go(&session, "loop")).await;
        assert_eq!(provider.requests().len(), 4);
        assert!(provider.requests().iter().all(|(_, o)| o.tool_choice == marv_core::ToolChoice::Auto));
        assert_eq!(end_reason(&events), TurnEndReason::Done(StopReason::MaxSteps));
    }

    #[tokio::test]
    async fn the_step_limit_question_says_when_it_asks_again_in_the_continue_scope() {
        let dirs = Dirs::new();
        let approver = answer(Decision::No);
        let session = scripted(&dirs, &Scripted::new(reads(10)), |i| {
            i.max_steps = Some(4);
            i.approve = Some(approver.clone());
        });
        let events = collect(go(&session, "loop")).await;
        let asked = lock(&approver.0).clone();
        assert_eq!(asked.iter().map(|r| r.scope.key.as_str()).collect::<Vec<_>>(), ["continue"]);
        assert!(asked[0].preview.note.as_deref().unwrap().contains("after another 4"));
        assert!(events.iter().any(|e| matches!(e, SessionEvent::Loop(LoopEvent::StepLimit { steps: 4, continued: false }))));
    }

    #[tokio::test]
    async fn max_steps_0_runs_past_the_default_without_asking_even_with_an_approver() {
        let dirs = Dirs::new();
        let mut steps = reads(30);
        steps.push(say("Finally done."));
        let provider = Scripted::new(steps);
        let approver = answer(Decision::No);
        let session = scripted(&dirs, &provider, |i| {
            i.max_steps = Some(0);
            i.approve = Some(approver.clone());
        });
        let events = collect(go(&session, "loop")).await;
        assert_eq!(provider.requests().len(), 31);
        assert!(lock(&approver.0).is_empty());
        assert!(!kinds(&events).contains(&"step_limit"));
        assert!(has_done(&events, StopReason::End));
    }

    #[tokio::test]
    async fn configure_max_steps_moves_the_limit_from_the_next_turn_on() {
        let dirs = Dirs::new();
        let session = scripted(&dirs, &Scripted::new(reads(30)), |_| {});
        collect(go(&session, "first")).await; // stops at the default 25
        session.configure(Configure { max_steps: Some(0), ..Default::default() }).unwrap();
        let events = collect(go(&session, "loop")).await;
        assert!(has_done(&events, StopReason::End));
        session.close().await;
    }

    // ---- MCP servers ----

    use serde_json::json;

    /// A manager over `servers` (yours), not started.
    fn mcp_manager(dirs: &Dirs, servers: Vec<serde_json::Value>) -> McpHandle {
        use marv_mcp::config::{ServerSource, parse_server};
        use marv_mcp::manager::{CONNECT_TIMEOUT, LOGIN_TIMEOUT, ManagerOptions, McpManager};
        let configs = servers
            .iter()
            .enumerate()
            .map(|(i, raw)| parse_server(&format!("s{i}"), raw, ServerSource::Personal, &|_| None, "test").unwrap())
            .collect();
        McpManager::new(
            configs,
            ManagerOptions {
                root: dirs.project.clone(),
                project_key: "p".into(),
                home: dirs.dir.clone(),
                version: "0".into(),
                trust: None,
                logins: None,
                env: Default::default(),
                connect_timeout: CONNECT_TIMEOUT,
                login_timeout: LOGIN_TIMEOUT,
            },
        )
    }

    /// A Streamable HTTP MCP server with one tool, `ping`, that answers `initialize` only once `gate` opens.
    async fn gated_mcp_server(gate: tokio::sync::watch::Receiver<bool>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://127.0.0.1:{}/mcp", listener.local_addr().unwrap().port());
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut gate = gate.clone();
                tokio::spawn(async move {
                    let mut data = Vec::new();
                    let mut buf = [0u8; 4096];
                    let body = loop {
                        let n = stream.read(&mut buf).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        data.extend_from_slice(&buf[..n]);
                        let text = String::from_utf8_lossy(&data).to_string();
                        let Some((head, body)) = text.split_once("\r\n\r\n") else { continue };
                        let length = head
                            .lines()
                            .find_map(|l| {
                                l.to_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if body.len() >= length {
                            break body.to_string();
                        }
                    };
                    let message: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
                    let method = message["method"].as_str().unwrap_or_default();
                    let reply = match method {
                        m if m == marv_mcp::jsonrpc::method::INITIALIZE => {
                            let _ = gate.wait_for(|open| *open).await;
                            Some(
                                json!({"protocolVersion": "2025-06-18", "capabilities": {}, "serverInfo": {"name": "x", "version": "1"}}),
                            )
                        }
                        "tools/list" => {
                            Some(json!({"tools": [{"name": "ping", "description": "Ping.", "inputSchema": {"type": "object"}}]}))
                        }
                        _ => None,
                    };
                    let response = match reply {
                        Some(result) => {
                            let body = json!({"jsonrpc": "2.0", "id": message["id"], "result": result}).to_string();
                            format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                                body.len()
                            )
                        }
                        None => "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".into(),
                    };
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        url
    }

    #[tokio::test]
    async fn the_first_request_waits_for_them_and_offers_their_tools() {
        let dirs = Dirs::new();
        let (open, gate) = tokio::sync::watch::channel(false);
        let url = gated_mcp_server(gate).await;
        let mcp = mcp_manager(&dirs, vec![json!({"type": "http", "url": url})]);
        let provider = Scripted::new(vec![say("Pong.")]);
        let session = scripted(&dirs, &provider, |init| init.mcp = Some(mcp.clone()));
        tokio::spawn({
            let mcp = mcp.clone();
            async move { mcp.start().await }
        });
        let mut turn = go(&session, "ping");
        let mut waited = false;
        while let Some(event) = turn.next().await {
            if matches!(event, SessionEvent::Status(TurnStatus::WaitingForMcp)) {
                assert!(provider.requests().is_empty());
                waited = true;
                open.send_replace(true);
            }
        }
        assert!(waited);
        assert!(provider.requests()[0].1.tools.iter().any(|t| t.name == "mcp__s0__ping"));
        session.close().await;
    }

    #[tokio::test]
    async fn interrupting_the_wait_ends_the_turn_before_anything_is_sent() {
        let dirs = Dirs::new();
        let (_never, gate) = tokio::sync::watch::channel(false);
        let url = gated_mcp_server(gate).await;
        let mcp = mcp_manager(&dirs, vec![json!({"type": "http", "url": url})]);
        let provider = Scripted::new(vec![say("never")]);
        let session = scripted(&dirs, &provider, |init| init.mcp = Some(mcp.clone()));
        tokio::spawn({
            let mcp = mcp.clone();
            async move { mcp.start().await }
        });
        let mut turn = go(&session, "ping");
        let mut events = Vec::new();
        while let Some(event) = turn.next().await {
            if matches!(event, SessionEvent::Status(_)) {
                session.interrupt();
            }
            events.push(event);
        }
        assert_eq!(kinds(&events), ["turn_start", "status", "turn_end"]);
        assert!(matches!(events.last(), Some(SessionEvent::TurnEnd { reason: TurnEndReason::Interrupted, .. })));
        assert!(provider.requests().is_empty());
    }

    /// The race the oracle hit about once in fifteen runs: servers that settle before the first turn checks (here,
    /// one that fails at once, awaited before the message). The first turn still says it waited; later ones don't.
    #[tokio::test]
    async fn the_first_turn_with_mcp_servers_always_goes_through_the_wait() {
        let dirs = Dirs::new();
        let mcp = mcp_manager(&dirs, vec![json!({"command": "/nonexistent/marv-mcp-test-server"})]);
        mcp.start().await;
        assert!(mcp.settled());
        let provider = Scripted::new(vec![say("One."), say("Two.")]);
        let session = scripted(&dirs, &provider, |init| init.mcp = Some(mcp.clone()));
        let first = collect(go(&session, "first")).await;
        assert_eq!(statuses(&first, TurnStatus::WaitingForMcp), 1);
        let second = collect(go(&session, "second")).await;
        assert_eq!(statuses(&second, TurnStatus::WaitingForMcp), 0);
        // A manager without servers (the TUI's, before /mcp add) never waits.
        let dirs = Dirs::new();
        let empty = mcp_manager(&dirs, vec![]);
        let provider = Scripted::new(vec![say("One.")]);
        let session = scripted(&dirs, &provider, |init| init.mcp = Some(empty));
        assert_eq!(statuses(&collect(go(&session, "first")).await, TurnStatus::WaitingForMcp), 0);
    }

    // ---- compaction ----

    fn compacted(summary: &str, tokens_before: u64) -> CompactResult {
        CompactResult::Compacted { summary: summary.into(), tokens_before: Some(tokens_before) }
    }
    fn with(first: ProviderEvent, rest: Vec<ProviderEvent>) -> Vec<ProviderEvent> {
        std::iter::once(first).chain(rest).collect()
    }

    #[tokio::test]
    async fn a_nearly_full_context_is_summarized_before_the_next_message() {
        let dirs = Dirs::new();
        let provider =
            Scripted::new(vec![with(used(900), say("First answer.")), say("SUMMARY of it all"), say("Second answer.")])
                .with_context(1000);
        let session = scripted(&dirs, &provider, |_| {});
        collect(go(&session, "first")).await;
        let events = collect(go(&session, "second")).await;
        assert_eq!(statuses(&events, TurnStatus::Compacting), 1);
        assert_eq!(compactions(&events), [compacted("SUMMARY of it all", 900)]);
        let after = &provider.requests()[2].0;
        assert!(text_of(&after[0]).contains("SUMMARY of it all"));
        assert_eq!(text_of(after.last().unwrap()), "second");
        // Before a message, the message itself says what to do next: no mid-turn "continue" note in between.
        assert_eq!(after.iter().map(role_of).collect::<Vec<_>>(), ["user", "assistant", "user"]);
    }

    /// The first request answers with `first`; every later one hangs until stopped. `requests` counts them all.
    fn first_then_hanging(first: Vec<ProviderEvent>, requests: Arc<std::sync::atomic::AtomicUsize>) -> impl Provider {
        let first = Scripted::new(vec![first]);
        let hanging = Hanging::default();
        Hook(move |history: &[ChatTurn], options: &StreamOptions| {
            if requests.fetch_add(1, Ordering::SeqCst) == 0 {
                first.stream(history, options.clone())
            } else {
                hanging.stream(history, options.clone())
            }
        })
    }

    async fn interrupt_at_compacting(session: &MarvSession, text: &str) -> Vec<SessionEvent> {
        let mut turn = go(session, text);
        let mut events = Vec::new();
        while let Some(e) = turn.next().await {
            if matches!(e, SessionEvent::Status(TurnStatus::Compacting)) {
                session.interrupt();
            }
            events.push(e);
        }
        events
    }

    fn stopped() -> CompactResult {
        CompactResult::NotCompacted { reason: CompactFailure::Stopped, error: "Stopped.".into() }
    }

    #[tokio::test]
    async fn stopping_the_compaction_stops_the_turn_and_the_message_isnt_sent() {
        let dirs = Dirs::new();
        let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider = first_then_hanging(with(used(900), say("First answer.")), requests.clone());
        let session = make(&dirs, fixed(Arc::new(provider)), |_| {});
        collect(go(&session, "first")).await;
        let events = interrupt_at_compacting(&session, "second").await;
        assert_eq!(compactions(&events), [stopped()]);
        assert_eq!(end_reason(&events), TurnEndReason::Interrupted);
        assert_eq!(requests.load(Ordering::SeqCst), 2); // the first answer and the summary: "second" was never sent
    }

    #[tokio::test]
    async fn compact_nothing_to_do_on_an_empty_conversation_afterwards_the_next_request_starts_from_the_summary() {
        let dirs = Dirs::new();
        let provider = Scripted::new(vec![say("Answer."), say("THE SUMMARY"), say("Next.")]);
        let session = scripted(&dirs, &provider, |_| {});
        assert_eq!(
            session.compact(None).await.unwrap(),
            CompactResult::NotCompacted { reason: CompactFailure::Empty, error: "Nothing to compact yet.".into() }
        );
        collect(go(&session, "first")).await;
        let result = session.compact(Some("the plan")).await.unwrap();
        assert!(matches!(result, CompactResult::Compacted { ref summary, .. } if summary == "THE SUMMARY"));
        assert!(!session.busy());
        assert!(text_of(provider.requests()[1].0.last().unwrap()).contains("Focus especially on: the plan"));
        collect(go(&session, "next")).await;
        assert!(text_of(&provider.requests()[2].0[0]).contains("THE SUMMARY"));
    }

    #[tokio::test]
    async fn a_compaction_forgets_which_nested_instruction_files_were_given() {
        // The summary replaces the tool results that carried them (#62): the next touch gives them again.
        let dirs = Dirs::new();
        std::fs::create_dir_all(dirs.project.join("pkg")).unwrap();
        std::fs::write(dirs.project.join("pkg/AGENTS.md"), "pkg rules").unwrap();
        let provider = Scripted::new(vec![say("Answer."), say("THE SUMMARY")]);
        let session = scripted(&dirs, &provider, |_| {});
        let root = session.inner.init.root.clone();
        let touch = || session.inner.nested.note_for(&root, &root.join("pkg/a.rs"), "read_file");
        assert!(touch().is_some());
        assert!(touch().is_none());
        collect(go(&session, "first")).await;
        assert!(matches!(session.compact(None).await.unwrap(), CompactResult::Compacted { .. }));
        assert!(touch().is_some_and(|n| n.contains("pkg rules")));
    }

    #[tokio::test]
    async fn a_long_turn_compacts_between_steps_and_continues_from_the_summary() {
        let dirs = Dirs::new();
        let provider =
            Scripted::new(vec![with(used(900), use_tools(vec![read("c1")])), say("SUMMARY of it all"), say("Finished.")])
                .with_context(1000);
        let events = collect(go(&scripted(&dirs, &provider, |_| {}), "first")).await;
        assert_eq!(statuses(&events, TurnStatus::Compacting), 1);
        assert!(kinds(&events).contains(&"compacted"));
        assert_eq!(compactions(&events), [compacted("SUMMARY of it all", 900)]);
        assert_eq!(end_reason(&events), TurnEndReason::Done(StopReason::End));
        // The summary request carried the whole conversation, tool result included.
        let summary_request = &provider.requests()[1].0;
        assert!(text_of(summary_request.last().unwrap()).contains("Summarize our conversation"));
        assert_eq!(role_of(&summary_request[summary_request.len() - 2]), "tool");
        // The next request starts from the summary: the conversation was swapped under the loop.
        let after = &provider.requests()[2].0;
        assert_eq!(after.iter().map(role_of).collect::<Vec<_>>(), ["user", "assistant", "user"]);
        assert!(text_of(&after[0]).contains("SUMMARY of it all"));
        // The compacted history ends with the model's own "Got it.", so Marv's note to go on comes last.
        assert!(text_of(after.last().unwrap()).contains("Continue where you left off"));
    }

    #[tokio::test]
    async fn a_failed_mid_turn_compaction_leaves_the_history_and_the_turn_goes_on() {
        let dirs = Dirs::new();
        let provider = Scripted::new(vec![
            with(used(900), use_tools(vec![read("c1")])),
            vec![ProviderEvent::Error("summary failed".into())],
            with(used(900), use_tools(vec![read("c2")])),
            say("Finished."),
        ])
        .with_context(1000);
        let events = collect(go(&scripted(&dirs, &provider, |_| {}), "first")).await;
        assert_eq!(
            compactions(&events),
            [CompactResult::NotCompacted { reason: CompactFailure::Failed, error: "summary failed".into() }]
        );
        assert!(events.iter().any(|e| matches!(e, SessionEvent::Loop(LoopEvent::Assistant { text }) if text == "Finished.")));
        assert_eq!(end_reason(&events), TurnEndReason::Done(StopReason::End));
        let requests = provider.requests();
        assert_eq!(role_of(requests[2].0.last().unwrap()), "tool"); // not replaced by the summary
        assert!(unanswered(&requests[2].0).is_empty());
        // Still nearly full after the next step, but the failed summary isn't tried again within the turn.
        assert_eq!(statuses(&events, TurnStatus::Compacting), 1);
        let summaries =
            requests.iter().filter(|(h, _)| text_of(h.last().unwrap()).starts_with("Summarize our conversation")).count();
        assert_eq!(summaries, 1);
    }

    #[tokio::test]
    async fn stopping_a_mid_turn_compaction_stops_the_turn() {
        let dirs = Dirs::new();
        let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider = first_then_hanging(with(used(900), use_tools(vec![read("c1")])), requests.clone());
        let session = make(&dirs, fixed(Arc::new(provider)), |_| {});
        let events = interrupt_at_compacting(&session, "first").await;
        assert_eq!(compactions(&events), [stopped()]);
        // The turn was already running, so this is an abort, not the pre-message "interrupted".
        assert_eq!(end_reason(&events), TurnEndReason::Done(StopReason::Aborted));
        assert_eq!(requests.load(Ordering::SeqCst), 2); // the first answer and the summary: the loop never went on
    }

    #[tokio::test]
    async fn a_failed_automatic_compaction_lets_the_turn_go_on() {
        let dirs = Dirs::new();
        let provider = Scripted::new(vec![
            with(used(900), say("First.")),
            vec![ProviderEvent::Error("summary failed".into())],
            say("Second."),
        ])
        .with_context(1000);
        let session = scripted(&dirs, &provider, |_| {});
        collect(go(&session, "first")).await;
        let events = collect(go(&session, "second")).await;
        assert_eq!(
            compactions(&events),
            [CompactResult::NotCompacted { reason: CompactFailure::Failed, error: "summary failed".into() }]
        );
        assert_eq!(end_reason(&events), TurnEndReason::Done(StopReason::End));
        assert_eq!(provider.requests()[2].0.iter().map(text_of).collect::<Vec<_>>(), ["first", "First.", "second"]);
    }

    // ---- trajectories ----

    #[tokio::test]
    async fn a_turn_is_recorded_and_the_next_messages_tone_rates_it() {
        let dirs = Dirs::new();
        let store = Arc::new(TrajectoryStore::new(&dirs.dir));
        let session = scripted(&dirs, &Scripted::new(vec![say("Done."), say("Glad to help.")]), |i| {
            i.trajectories = Some(store.clone());
            i.version = "9.9.9".into();
        });
        collect(go(&session, "fix it")).await;
        collect(go(&session, "thanks, perfect")).await;
        let log = records(&store, &dirs.project, &session).await;
        let types: Vec<&str> = log.iter().map(|r| r["type"].as_str().unwrap()).collect();
        assert_eq!(
            types,
            [
                "session",
                "turn_start",
                "request",
                "assistant",
                "agent_end",
                "feedback",
                "turn_start",
                "request",
                "assistant",
                "agent_end"
            ]
            .into_iter()
            .filter(|t| log.iter().any(|r| r["type"] == *t))
            .collect::<Vec<_>>()
        );
        assert_eq!(
            (log[0]["marv"].as_str(), log[0]["provider"].as_str(), log[0]["model"].as_str()),
            (Some("9.9.9"), Some("test"), Some("test-model"))
        );
        let feedback = log.iter().find(|r| r["type"] == "feedback").unwrap();
        assert_eq!((feedback["source"].as_str(), feedback["score"].as_i64()), (Some("implicit"), Some(1)));
        assert_eq!(feedback["turn"], log[1]["turn"]);
    }

    #[tokio::test]
    async fn rate_rates_the_last_turn_and_says_why_when_it_cant() {
        let dirs = Dirs::new();
        let store = Arc::new(TrajectoryStore::new(&dirs.dir));
        let session = scripted(&dirs, &Scripted::new(vec![say("Done.")]), |i| i.trajectories = Some(store.clone()));
        let rate = |score| Feedback { score, note: None, labels: None };
        assert_eq!(session.rate(rate(1)), RateOutcome::Nothing);
        collect(go(&session, "fix it")).await;
        assert_eq!(session.rate(Feedback { note: Some("wrong file".into()), ..rate(-1) }), RateOutcome::Rated);
        let last = records(&store, &dirs.project, &session).await.pop().unwrap();
        assert_eq!(
            (last["type"].as_str(), last["source"].as_str(), last["score"].as_i64()),
            (Some("feedback"), Some("explicit"), Some(-1))
        );
        assert_eq!(last["note"], "wrong file");
        session.configure(Configure { trajectories: Some(false), ..Default::default() }).unwrap();
        assert_eq!(session.rate(rate(1)), RateOutcome::Off);
        assert_eq!(scripted(&dirs, &Scripted::new(vec![]), |_| {}).rate(rate(1)), RateOutcome::Off);
    }

    #[tokio::test]
    async fn a_turn_stopped_before_its_message_is_sent_leaves_nothing_in_the_trajectory() {
        let dirs = Dirs::new();
        let store = Arc::new(TrajectoryStore::new(&dirs.dir));
        let provider = Scripted::new(vec![say("Glad to help.")]);
        let session = scripted(&dirs, &provider, |i| i.trajectories = Some(store.clone()));
        let stop = CancellationToken::new();
        stop.cancel();
        let events = collect(session.send("hi", SendOptions { cancel: Some(stop), ..Default::default() }).unwrap()).await;
        assert_eq!(end_reason(&events), TurnEndReason::Interrupted);
        assert!(provider.requests().is_empty());
        collect(go(&session, "thanks, perfect")).await;
        let log = records(&store, &dirs.project, &session).await;
        let starts: Vec<&str> = log.iter().filter(|r| r["type"] == "turn_start").map(|r| r["text"].as_str().unwrap()).collect();
        assert_eq!(starts, ["thanks, perfect"]);
        assert!(!log.iter().any(|r| r["type"] == "feedback"));
        assert_eq!(provider.requests()[0].0.iter().map(text_of).collect::<Vec<_>>(), ["thanks, perfect"]);
    }

    // ---- saving and resuming ----

    #[tokio::test]
    async fn a_turn_is_saved_and_another_session_can_resume_it() {
        let dirs = Dirs::new();
        let sessions = Arc::new(SessionStore::new(&dirs.dir));
        let a = scripted(&dirs, &Scripted::new(vec![say("Noted.")]), |i| i.sessions = Some(sessions.clone()));
        collect(go(&a, "remember 42")).await;
        a.flush().await;
        let provider = Scripted::new(vec![say("42.")]);
        let b = scripted(&dirs, &provider, |i| i.sessions = Some(sessions.clone()));
        let resumed = b.resume(ResumeTarget::Latest).await.unwrap().unwrap();
        assert_eq!((resumed.id.as_str(), resumed.model.as_str()), (a.id().as_str(), "test-model"));
        let shown: Vec<(Role, &str)> = resumed.transcript.iter().map(|m| (m.role, m.text.as_str())).collect();
        assert_eq!(shown, [(Role::User, "remember 42"), (Role::Assistant, "Noted.")]);
        assert_eq!(b.id(), a.id());
        collect(go(&b, "what was it?")).await;
        assert_eq!(provider.requests()[0].0.iter().map(text_of).collect::<Vec<_>>(), ["remember 42", "Noted.", "what was it?"]);
    }

    #[tokio::test]
    async fn the_clients_transcript_is_whats_saved() {
        let dirs = Dirs::new();
        let sessions = Arc::new(SessionStore::new(&dirs.dir));
        let transcript = vec![Message::new(7, Role::User, "hi"), Message::new(8, Role::System, "a notice")];
        let shown = transcript.clone();
        let session = scripted(&dirs, &Scripted::new(vec![say("Hello.")]), |i| {
            i.sessions = Some(sessions.clone());
            i.transcript = Some(Arc::new(move || shown.clone()));
        });
        collect(go(&session, "hi")).await;
        session.flush().await;
        assert_eq!(sessions.load(&dirs.project, &session.id()).await.unwrap().transcript, transcript);
    }

    #[tokio::test]
    async fn without_a_store_or_for_an_unknown_id_theres_nothing_to_resume() {
        let dirs = Dirs::new();
        assert!(scripted(&dirs, &Scripted::new(vec![]), |_| {}).resume(ResumeTarget::Latest).await.unwrap().is_none());
        let sessions = Arc::new(SessionStore::new(&dirs.dir));
        let s = scripted(&dirs, &Scripted::new(vec![]), |i| i.sessions = Some(sessions));
        assert!(s.resume(ResumeTarget::Id("nope".into())).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn clear_right_after_a_turn_still_saves_that_turns_conversation() {
        let dirs = Dirs::new();
        let sessions = Arc::new(SessionStore::new(&dirs.dir));
        let session = scripted(&dirs, &Scripted::new(vec![say("Noted.")]), |i| i.sessions = Some(sessions.clone()));
        collect(go(&session, "remember 42")).await;
        let first = session.id();
        session.clear().await.unwrap();
        assert_ne!(session.id(), first);
        session.flush().await;
        let saved = sessions.load(&dirs.project, &first).await.unwrap();
        assert_eq!(saved.conversation[0], ChatTurn::User { text: "remember 42".into() });
    }

    #[tokio::test]
    async fn so_does_resume_into_another_session() {
        let dirs = Dirs::new();
        let sessions = Arc::new(SessionStore::new(&dirs.dir));
        let other = scripted(&dirs, &Scripted::new(vec![say("Old.")]), |i| i.sessions = Some(sessions.clone()));
        collect(go(&other, "an old conversation")).await;
        other.flush().await;
        let session = scripted(&dirs, &Scripted::new(vec![say("Noted.")]), |i| i.sessions = Some(sessions.clone()));
        collect(go(&session, "remember 42")).await;
        let first = session.id();
        session.resume(ResumeTarget::Id(other.id())).await.unwrap().unwrap();
        session.flush().await;
        let user = |text: &str| ChatTurn::User { text: text.into() };
        assert_eq!(sessions.load(&dirs.project, &first).await.unwrap().conversation[0], user("remember 42"));
        assert_eq!(sessions.load(&dirs.project, &other.id()).await.unwrap().conversation[0], user("an old conversation"));
    }

    // ---- between turns ----

    #[tokio::test]
    async fn clear_resume_and_compact_refuse_during_a_turn_configure_applies_from_the_next_one() {
        let dirs = Dirs::new();
        let hanging = Arc::new(Hanging::default());
        let other = Scripted::new(vec![say("From the other model.")]);
        let session = make(&dirs, fixed(hanging.clone()), |i| i.sessions = Some(Arc::new(SessionStore::new(&dirs.dir))));
        let mut turn = go(&session, "go");
        while let Some(e) = turn.next().await {
            if kind(&e) != "text_delta" {
                continue;
            }
            assert!(session.clear().await.unwrap_err().to_string().contains("working"));
            assert!(session.resume(ResumeTarget::Latest).await.unwrap_err().to_string().contains("working"));
            assert!(session.compact(None).await.unwrap_err().to_string().contains("working"));
            let provider = ProviderChoice::Factory(Arc::new(fixed(Arc::new(other.clone()))));
            session.configure(Configure { provider: Some(provider), ..Default::default() }).unwrap();
            session.interrupt();
        }
        assert_eq!(hanging.requests.load(Ordering::SeqCst), 1);
        collect(go(&session, "again")).await;
        assert_eq!(other.requests().len(), 1);
    }

    #[tokio::test]
    async fn clear_starts_a_new_conversation_with_a_new_id() {
        let dirs = Dirs::new();
        let provider = Scripted::new(vec![say("One."), say("Two.")]);
        let session = scripted(&dirs, &provider, |_| {});
        collect(go(&session, "one")).await;
        let before = session.id();
        session.clear().await.unwrap();
        assert_ne!(session.id(), before);
        collect(go(&session, "two")).await;
        assert_eq!(provider.requests()[1].0, [ChatTurn::User { text: "two".into() }]);
    }

    fn info(context: Option<u64>, reasoning: Option<ReasoningSupport>) -> ModelInfo {
        ModelInfo { id: "x/y".into(), context, reasoning, ..ModelInfo::default() }
    }

    #[tokio::test]
    async fn the_models_info_arrives_in_the_background() {
        let dirs = Dirs::new();
        let made: Arc<Mutex<Vec<Option<ReasoningSupport>>>> = Arc::default();
        let changed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let factory = TestFactory {
            id: "openrouter".into(),
            model: "x/y".into(),
            local: false,
            make: {
                let made = made.clone();
                Arc::new(move |_, info| {
                    lock(&made).push(info.and_then(|i| i.reasoning));
                    Arc::new(Scripted::new(vec![])) as Arc<dyn Provider>
                })
            },
            lookup: Some(Arc::new(|| Box::pin(async { Some(info(Some(200_000), Some(ReasoningSupport::Optional))) }))),
        };
        let counter = changed.clone();
        let session = make(&dirs, factory, |i| {
            i.on_change = Some(Arc::new(move || {
                counter.fetch_add(1, Ordering::SeqCst);
            }))
        });
        assert!(until(|| session.context_length() == Some(200_000)).await);
        assert_eq!(session.model_info().unwrap().reasoning, Some(ReasoningSupport::Optional));
        assert_eq!(*lock(&made), [None, Some(ReasoningSupport::Optional)]);
        assert_eq!(changed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn every_request_extends_the_previous_one_exactly() {
        let dirs = Dirs::new();
        let provider = Scripted::new(vec![
            use_tools(vec![read("c1")]),
            say("One."),
            use_tools(vec![read("c2")]),
            say("Two."),
            say("Three."),
        ]);
        let session = scripted(&dirs, &provider, |_| {});
        collect(go(&session, "one")).await;
        session.configure(Configure { yolo: Some(false), sandbox: Some(false), ..Default::default() }).unwrap();
        collect(go(&session, "two")).await;
        collect(go(&session, "three")).await;
        let requests = provider.requests();
        assert_eq!(requests.len(), 5);
        for pair in requests.windows(2) {
            let ((before, b), (after, a)) = (&pair[0], &pair[1]);
            assert_eq!(&after[..before.len()], &before[..]); // only appended to
            assert!(after.len() > before.len());
            assert_eq!(a.system, b.system); // the same system prompt
            assert_eq!(a.tools, b.tools); // the same tool definitions
        }
    }

    // ---- review fixes ----

    #[tokio::test]
    async fn a_turn_nobody_reads_still_runs_to_the_end_and_frees_the_session() {
        let dirs = Dirs::new();
        let provider = Scripted::new(vec![say("One."), say("Two.")]);
        let session = scripted(&dirs, &provider, |_| {});
        let unread = go(&session, "one"); // never read (dropping it would interrupt it)
        assert!(until(|| !session.busy()).await);
        let events = collect(go(&session, "two")).await;
        assert_eq!(end_reason(&events), TurnEndReason::Done(StopReason::End));
        assert_eq!(provider.last_texts(), ["one", "two"]);
        drop(unread);
    }

    #[tokio::test]
    async fn the_session_is_free_at_turn_end() {
        let dirs = Dirs::new();
        let provider = Scripted::new(vec![say("One."), say("Two.")]);
        let session = scripted(&dirs, &provider, |_| {});
        let mut turn = go(&session, "one");
        let mut next = None;
        while let Some(e) = turn.next().await {
            if kind(&e) == "turn_end" {
                assert!(!session.busy());
                next = Some(go(&session, "two"));
            }
        }
        assert_eq!(end_reason(&collect(next.unwrap()).await), TurnEndReason::Done(StopReason::End));
        assert_eq!(provider.last_texts(), ["one", "two"]);
    }

    #[tokio::test]
    async fn close_waits_for_the_running_turn_and_nothing_starts_after_it() {
        let dirs = Dirs::new();
        let sessions = Arc::new(SessionStore::new(&dirs.dir));
        let session = make(&dirs, fixed(Arc::new(Hanging::default())), |i| i.sessions = Some(sessions.clone()));
        let mut turn = go(&session, "go");
        while let Some(e) = turn.next().await {
            if kind(&e) == "text_delta" {
                break; // the model is working
            }
        }
        session.close().await;
        assert!(!session.busy());
        let saved = sessions.load(&dirs.project, &session.id()).await.unwrap();
        assert_eq!(saved.conversation[0], ChatTurn::User { text: "go".into() });
        // A timer armed after close() would fire within the save delay.
        assert!(session.state().save_timer.is_none());
        session.close().await; // twice is fine
        assert_eq!(session.send("hi", SendOptions::default()).err().unwrap().to_string(), CLOSED);
        assert_eq!(session.compact(None).await.unwrap_err().to_string(), CLOSED);
        assert_eq!(session.clear().await.unwrap_err().to_string(), CLOSED);
        assert_eq!(session.resume(ResumeTarget::Latest).await.unwrap_err().to_string(), CLOSED);
        // Settings and saving still work.
        session.configure(Configure { yolo: Some(false), ..Default::default() }).unwrap();
        session.flush().await;
        drop(turn);
    }

    #[tokio::test]
    async fn an_on_change_that_panics_doesnt_break_the_session() {
        let dirs = Dirs::new();
        let factory = TestFactory {
            lookup: Some(Arc::new(|| Box::pin(async { Some(info(Some(1000), None)) }))),
            ..fixed(Arc::new(Scripted::new(vec![say("Hi.")])))
        };
        let session = make(&dirs, factory, |i| i.on_change = Some(Arc::new(|| panic!("the UI broke"))));
        assert!(until(|| session.context_length() == Some(1000)).await);
        let _ = session.clear().await;
        assert_eq!(end_reason(&collect(go(&session, "hi")).await), TurnEndReason::Done(StopReason::End));
    }

    #[tokio::test]
    async fn configure_with_the_same_model_keeps_what_the_model_list_said() {
        let dirs = Dirs::new();
        let made: Arc<Mutex<Vec<Option<ReasoningSupport>>>> = Arc::default();
        let factory = || TestFactory {
            id: "openrouter".into(),
            model: "x/y".into(),
            local: false,
            make: {
                let made = made.clone();
                Arc::new(move |_, info| {
                    lock(&made).push(info.and_then(|i| i.reasoning));
                    Arc::new(Scripted::new(vec![])) as Arc<dyn Provider>
                })
            },
            lookup: Some(Arc::new(|| Box::pin(async { Some(info(Some(200_000), Some(ReasoningSupport::Optional))) }))),
        };
        let session = make(&dirs, factory(), |_| {});
        assert!(until(|| session.model_info().is_some()).await);
        let again = TestFactory { lookup: None, ..factory() };
        session.configure(Configure { provider: Some(ProviderChoice::Factory(Arc::new(again))), ..Default::default() }).unwrap();
        assert_eq!(session.model_info().unwrap().reasoning, Some(ReasoningSupport::Optional));
        assert_eq!(*lock(&made).last().unwrap(), Some(ReasoningSupport::Optional));
    }

    #[tokio::test]
    async fn a_lookup_for_a_provider_that_was_replaced_meanwhile_is_ignored() {
        let dirs = Dirs::new();
        let (tx, rx) = tokio::sync::oneshot::channel::<ModelInfo>();
        let rx = Arc::new(Mutex::new(Some(rx)));
        let stale = TestFactory {
            lookup: Some(Arc::new(move || {
                let rx = lock(&rx).take();
                Box::pin(async move { rx?.await.ok() })
            })),
            ..fixed(Arc::new(Scripted::new(vec![])))
        };
        let changed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = changed.clone();
        let session = make(&dirs, stale, |i| {
            i.on_change = Some(Arc::new(move || {
                counter.fetch_add(1, Ordering::SeqCst);
            }))
        });
        let other = TestFactory { id: "other".into(), model: "other-model".into(), ..fixed(Arc::new(Scripted::new(vec![]))) };
        session.configure(Configure { provider: Some(ProviderChoice::Factory(Arc::new(other))), ..Default::default() }).unwrap();
        tx.send(info(Some(123), None)).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(session.model_info().is_none());
        assert_eq!(session.context_length(), None);
        assert_eq!(changed.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_turn_prices_its_requests_with_the_model_it_started_with() {
        let dirs = Dirs::new();
        let slot: Arc<Mutex<Option<MarvSession>>> = Arc::default();
        let inside = slot.clone();
        let provider = Hook(move |_: &[ChatTurn], _: &StreamOptions| -> EventStream {
            // The user switches models while the turn runs, before its request is counted.
            if let Some(session) = lock(&inside).as_ref() {
                let switched =
                    TestFactory { id: "other".into(), model: "other-model".into(), ..fixed(Arc::new(Scripted::new(vec![]))) };
                session
                    .configure(Configure { provider: Some(ProviderChoice::Factory(Arc::new(switched))), ..Default::default() })
                    .unwrap();
            }
            let mut events = vec![ProviderEvent::Usage(Usage {
                prompt_tokens: 1_000_000,
                completion_tokens: 500_000,
                cached_tokens: None,
                cost: None,
            })];
            events.extend(say("Hi."));
            Box::pin(futures::stream::iter(events))
        });
        let priced = TestFactory {
            id: "openrouter".into(),
            model: "x/y".into(),
            lookup: Some(Arc::new(|| {
                Box::pin(async { Some(ModelInfo { price_in: Some(2.0), price_out: Some(4.0), ..info(None, None) }) })
            })),
            ..fixed(Arc::new(provider))
        };
        let session = make(&dirs, priced, |_| {});
        assert!(until(|| session.model_info().is_some()).await); // the prices arrive
        *lock(&slot) = Some(session.clone());
        collect(go(&session, "hi")).await;
        *lock(&slot) = None;
        let totals = session.usage().totals;
        assert_eq!((totals.requests, totals.cost, totals.estimated), (1, Some(4.0), Some(true))); // $2 in + $2 out
    }

    #[tokio::test]
    async fn after_resume_a_compaction_doesnt_point_at_a_turn_in_the_other_sessions_log() {
        let dirs = Dirs::new();
        let sessions = Arc::new(SessionStore::new(&dirs.dir));
        let other = scripted(&dirs, &Scripted::new(vec![say("Old.")]), |i| i.sessions = Some(sessions.clone()));
        collect(go(&other, "an old conversation")).await;
        other.flush().await;
        let store = Arc::new(TrajectoryStore::new(dirs.dir.join("t")));
        let session = scripted(&dirs, &Scripted::new(vec![say("New."), say("THE SUMMARY")]), |i| {
            i.sessions = Some(sessions.clone());
            i.trajectories = Some(store.clone());
        });
        collect(go(&session, "a new one")).await;
        session.resume(ResumeTarget::Id(other.id())).await.unwrap().unwrap();
        assert!(matches!(session.compact(None).await.unwrap(), CompactResult::Compacted { .. }));
        let log = records(&store, &dirs.project, &session).await;
        let compact = log.iter().find(|r| r["type"] == "compact").unwrap();
        assert_eq!(compact["summary"], "THE SUMMARY");
        assert!(compact.get("turn").is_none());
    }

    #[tokio::test]
    async fn yolo_off_where_it_isnt_allowed_stays_off_and_the_edit_is_refused() {
        let dirs = Dirs::new();
        let provider = Scripted::new(vec![
            use_tools(vec![call("w1", "write_file", serde_json::json!({ "path": "out.txt", "content": "hi\n" }))]),
            say("Refused."),
        ]);
        let session = scripted(&dirs, &provider, |i| {
            i.no_yolo = Some("Not here.".into());
            i.yolo = false;
        });
        let other = ProviderChoice::Factory(Arc::new(fixed(Arc::new(Scripted::new(vec![say("Other.")])))));
        let err = session.configure(Configure { yolo: Some(true), provider: Some(other), ..Default::default() });
        assert_eq!(err.unwrap_err().to_string(), "Not here.");
        session.configure(Configure { yolo: Some(false), ..Default::default() }).unwrap();
        let events = collect(go(&session, "write it")).await;
        assert!(events.iter().any(|e| matches!(e, SessionEvent::Loop(LoopEvent::Assistant { text }) if text == "Refused.")));
        assert!(!dirs.project.join("out.txt").exists());
    }

    // ---- the doors out of the sandbox (Marv #47, #52; tests/doors.test.ts, tests/github-tool.test.ts) ----

    /// Records the doors each call's context has.
    struct Probe(Arc<Mutex<Vec<marv_tools::types::Doors>>>);
    #[async_trait::async_trait]
    impl marv_tools::Tool for Probe {
        type Input = serde_json::Value;
        fn name(&self) -> &str {
            "probe"
        }
        fn description(&self) -> &str {
            "probe"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn label(&self, _input: &Self::Input) -> String {
            "probe".into()
        }
        async fn run(
            &self,
            _input: &Self::Input,
            ctx: &marv_tools::ToolContext,
        ) -> Result<marv_tools::ToolOutput, marv_tools::ToolError> {
            lock(&self.0).push(ctx.doors);
            Ok(marv_tools::ToolOutput::ok("", ""))
        }
    }

    #[tokio::test]
    async fn the_main_agent_has_the_host_door_only_with_host_commands_and_the_github_door_only_with_github() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        for (host, github) in [(false, false), (true, false), (false, true)] {
            let dirs = Dirs::new();
            let provider = Scripted::new(vec![use_tools(vec![call("p", "probe", serde_json::json!({}))]), say("Done.")]);
            let session = scripted(&dirs, &provider, |i| {
                i.tools = vec![Arc::new(Probe(seen.clone()))];
                i.host_commands = host;
                i.github = github;
            });
            collect(go(&session, "go")).await;
        }
        let doors: Vec<(bool, bool)> = lock(&seen).iter().map(|d| (d.host, d.github)).collect();
        assert_eq!(doors, [(false, false), (true, false), (false, true)]);
    }

    #[tokio::test]
    async fn a_session_with_github_offers_it_names_its_door_in_the_prompt_and_sends_the_same_specs_each_request() {
        let dirs = Dirs::new();
        let provider = Scripted::new(vec![say("a"), say("b")]);
        let session = scripted(&dirs, &provider, |i| i.github = true);
        collect(go(&session, "one")).await;
        collect(go(&session, "two")).await;
        let requests = provider.requests();
        let (first, second) = (&requests[0].1, &requests[1].1);
        assert!(first.tools.iter().any(|t| t.name == "github"));
        assert_eq!(first.tools, second.tools);
        let system = first.system.as_deref().unwrap();
        assert!(system.contains("# Outside the sandbox"));
        assert!(system.contains("github runs the GitHub CLI (gh) with the user's own login"));

        let dirs = Dirs::new();
        let without = Scripted::new(vec![say("a")]);
        let plain = scripted(&dirs, &without, |_| {});
        collect(go(&plain, "one")).await;
        let options = &without.requests()[0].1;
        assert!(!options.tools.iter().any(|t| t.name == "github"));
        assert!(!options.system.as_deref().unwrap().contains("github runs the GitHub CLI"));
    }

    /// A door of any kind, as the github and host doors are: it asks, once.
    struct DoorTool;
    #[async_trait::async_trait]
    impl marv_tools::Tool for DoorTool {
        type Input = serde_json::Value;
        fn name(&self) -> &str {
            "d"
        }
        fn description(&self) -> &str {
            "A door for tests."
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn kind(&self) -> marv_core::ToolKind {
            marv_core::ToolKind::Execute
        }
        fn label(&self, _input: &Self::Input) -> String {
            "d".into()
        }
        fn door(&self, _input: &Self::Input, _ctx: &marv_tools::ToolContext) -> Option<marv_core::Door> {
            Some(marv_core::Door::Host)
        }
        fn once(&self, _input: &Self::Input, _ctx: &marv_tools::ToolContext) -> bool {
            true
        }
        async fn run(
            &self,
            _input: &Self::Input,
            _ctx: &marv_tools::ToolContext,
        ) -> Result<marv_tools::ToolOutput, marv_tools::ToolError> {
            Ok(marv_tools::ToolOutput::ok("ran", "ran"))
        }
    }

    // Marv's tests/doors.test.ts: the trajectory's tool record names the door.
    #[tokio::test]
    async fn the_trajectorys_tool_record_names_the_door() {
        let dirs = Dirs::new();
        let store = Arc::new(TrajectoryStore::new(&dirs.dir));
        let calls = vec![call("c1", "d", serde_json::json!({})), call("c2", "read_file", serde_json::json!({"path": "nope"}))];
        let provider = Scripted::new(vec![use_tools(calls), say("Done.")]);
        let session = scripted(&dirs, &provider, |i| {
            i.tools = vec![Arc::new(DoorTool)];
            i.approve = Some(answer(Decision::Yes));
            i.yolo = false;
            i.trajectories = Some(store.clone());
        });
        collect(go(&session, "go")).await;
        let log = records(&store, &dirs.project, &session).await;
        let tools: Vec<&serde_json::Value> = log.iter().filter(|r| r["type"] == "tool").collect();
        assert_eq!((tools[0]["door"].as_str(), tools[0]["approval"].as_str()), (Some("host"), Some("yes")));
        assert!(tools[1].get("door").is_none());
    }

    #[tokio::test]
    async fn a_session_with_host_commands_names_the_host_door_in_the_prompt() {
        let dirs = Dirs::new();
        let provider = Scripted::new(vec![say("a")]);
        let session = scripted(&dirs, &provider, |i| i.host_commands = true);
        collect(go(&session, "one")).await;
        assert!(provider.requests()[0].1.system.as_deref().unwrap().contains("bash with sandbox: false runs one command"));
    }

    #[tokio::test]
    async fn a_session_without_github_doesnt_run_it_even_when_the_model_names_it() {
        let dirs = Dirs::new();
        let provider =
            Scripted::new(vec![use_tools(vec![call("g1", "github", serde_json::json!({"args": ["pr", "list"]}))]), say("ok")]);
        let session = scripted(&dirs, &provider, |_| {});
        let events = collect(go(&session, "go")).await;
        let output = events
            .iter()
            .find_map(|e| match e {
                SessionEvent::Loop(LoopEvent::ToolEnd { run, .. }) => Some(run.result.output.clone()),
                _ => None,
            })
            .unwrap();
        assert!(output.contains("Unknown tool \"github\""), "{output}");
    }

    // ---- the plan (#15) ----

    fn plan_items() -> serde_json::Value {
        serde_json::json!([
            { "text": "Read the report command", "status": "done" },
            { "text": "Add the --json flag", "status": "in_progress" },
            { "text": "Update README and CHANGELOG", "status": "pending" },
        ])
    }
    fn plan_call(id: &str, items: serde_json::Value) -> ToolCall {
        call(id, "plan", serde_json::json!({ "items": items }))
    }
    fn expected_plan() -> Vec<PlanItem> {
        serde_json::from_value(plan_items()).unwrap()
    }
    fn tool_names(options: &StreamOptions) -> Vec<String> {
        options.tools.iter().map(|t| t.name.clone()).collect()
    }
    fn plan_shown(events: &[SessionEvent]) -> Option<Vec<PlanItem>> {
        events.iter().find_map(|e| match e {
            SessionEvent::Loop(LoopEvent::ToolEnd { run, .. }) => {
                run.display.as_ref().and_then(|d| d.downcast_ref::<crate::runner::ToolDisplay>()).and_then(|d| d.plan.clone())
            }
            _ => None,
        })
    }
    const PLAN_TEXT: &str = "Your plan (from the plan tool), as it stood:\n- [done] Read the report command\n- [in progress] Add the --json flag\n- [pending] Update README and CHANGELOG\nKeep updating it with the plan tool.";

    #[tokio::test]
    async fn a_plan_call_sets_the_plan_and_the_system_prompt_and_specs_stay_byte_identical() {
        let dirs = Dirs::new();
        let provider = Scripted::new(vec![use_tools(vec![plan_call("p1", plan_items())]), say("Working."), say("More.")]);
        let session = scripted(&dirs, &provider, |_| {});
        assert_eq!(session.plan(), None);
        let events = collect(go(&session, "do three things")).await;
        assert_eq!(session.plan(), Some(expected_plan()));
        assert_eq!(plan_shown(&events), Some(expected_plan()));
        // The model got only the line; the list is in its own call.
        let line = "Plan saved: 1 of 3 done; now: Add the --json flag.";
        assert_eq!(
            provider.requests()[1].0.last(),
            Some(&ChatTurn::Tool { call_id: "p1".into(), name: "plan".into(), text: line.into() })
        );
        collect(go(&session, "and then?")).await;
        let requests = provider.requests();
        assert!(tool_names(&requests[0].1).contains(&"plan".to_string()));
        for (_, options) in &requests[1..] {
            assert_eq!(options.system, requests[0].1.system);
            assert_eq!(options.tools, requests[0].1.tools);
        }
        assert!(requests[0].1.system.as_deref().unwrap().contains("keep a plan with the plan tool"));
    }

    #[tokio::test]
    async fn an_empty_plan_clears_it_and_clear_drops_it() {
        let dirs = Dirs::new();
        let provider = Scripted::new(vec![
            use_tools(vec![plan_call("p1", plan_items())]),
            say("Ok."),
            use_tools(vec![plan_call("p2", serde_json::json!([]))]),
            say("Cleared."),
            use_tools(vec![plan_call("p3", plan_items())]),
            say("Again."),
        ]);
        let session = scripted(&dirs, &provider, |_| {});
        collect(go(&session, "plan")).await;
        assert!(session.plan().is_some());
        let events = collect(go(&session, "drop it")).await;
        assert_eq!(session.plan(), None);
        assert_eq!(plan_shown(&events), Some(vec![]));
        collect(go(&session, "plan again")).await;
        assert!(session.plan().is_some());
        session.clear().await.unwrap();
        assert_eq!(session.plan(), None);
    }

    // Guard (docs/SECURITY.md, taint): the outside text is still in a resumed conversation, so its taint comes back,
    // whether the file says it or (an older file) its tool calls do; a clean saved session resumes clean.
    #[tokio::test]
    async fn resume_restores_the_conversations_taint() {
        let dirs = Dirs::new();
        let sessions = Arc::new(SessionStore::new(&dirs.dir));
        let saved = |id: &str, tool: &str, taint: Option<Vec<Provenance>>| {
            let mut s = crate::sessions::new_session(&dirs.project, "openrouter", "test-model", 1_000);
            s.id = id.into();
            s.conversation = vec![
                ChatTurn::User { text: "look it up".into() },
                ChatTurn::Assistant {
                    text: String::new(),
                    tool_calls: vec![ToolCall { id: "c1".into(), name: tool.into(), arguments: "{}".into() }],
                    reasoning: None,
                },
                ChatTurn::Tool { call_id: "c1".into(), name: tool.into(), text: "outside text".into() },
            ];
            s.transcript = vec![Message::new(1, Role::User, "look it up")];
            s.taint = taint;
            s
        };
        sessions.save(&saved("marked", "read_file", Some(vec![Provenance::Mesh]))).await.unwrap();
        sessions.save(&saved("older", "web_fetch", None)).await.unwrap();
        sessions.save(&saved("clean", "read_file", Some(vec![]))).await.unwrap();
        let b = scripted(&dirs, &Scripted::new(vec![]), |i| i.sessions = Some(sessions.clone()));
        b.resume(ResumeTarget::Id("marked".into())).await.unwrap().unwrap();
        assert_eq!(b.tainted(), [Provenance::Mesh]);
        b.resume(ResumeTarget::Id("older".into())).await.unwrap().unwrap();
        assert_eq!(b.tainted(), [Provenance::Web]);
        b.resume(ResumeTarget::Id("clean".into())).await.unwrap().unwrap();
        assert!(b.tainted().is_empty());
        // And it's saved: a session's file records what tainted it.
        let a = scripted(&dirs, &Scripted::new(vec![say("ok")]), |i| i.sessions = Some(sessions.clone()));
        a.inner.taint.mark(Provenance::Web);
        collect(go(&a, "hi")).await;
        a.flush().await;
        assert_eq!(sessions.load(&dirs.project, &a.id()).await.unwrap().taint, Some(vec![Provenance::Web]));
    }

    #[tokio::test]
    async fn the_plan_is_saved_with_the_session_and_restored_by_resume() {
        let dirs = Dirs::new();
        let sessions = Arc::new(SessionStore::new(&dirs.dir));
        let provider = Scripted::new(vec![use_tools(vec![plan_call("p1", plan_items())]), say("Planned.")]);
        let a = scripted(&dirs, &provider, |i| i.sessions = Some(sessions.clone()));
        collect(go(&a, "plan it")).await;
        a.flush().await;
        assert_eq!(sessions.load(&dirs.project, &a.id()).await.unwrap().plan_items(), Some(expected_plan()));
        let b = scripted(&dirs, &Scripted::new(vec![]), |i| i.sessions = Some(sessions.clone()));
        let resumed = b.resume(ResumeTarget::Id(a.id())).await.unwrap().unwrap();
        assert_eq!(resumed.plan, Some(expected_plan()));
        assert_eq!(b.plan(), Some(expected_plan()));
    }

    #[tokio::test]
    async fn resumed_with_plan_off_an_unfinished_saved_plan_isnt_restored_nor_handed_to_compaction() {
        let dirs = Dirs::new();
        let sessions = Arc::new(SessionStore::new(&dirs.dir));
        let a = scripted(&dirs, &Scripted::new(vec![use_tools(vec![plan_call("p1", plan_items())]), say("Planned.")]), |i| {
            i.sessions = Some(sessions.clone())
        });
        collect(go(&a, "plan it")).await;
        a.flush().await;
        let provider = Scripted::new(vec![say("The summary."), say("Ok.")]);
        let b = scripted(&dirs, &provider, |i| {
            i.sessions = Some(sessions.clone());
            i.plan = false;
        });
        let resumed = b.resume(ResumeTarget::Id(a.id())).await.unwrap().unwrap();
        assert_eq!(resumed.plan, None);
        assert_eq!(b.plan(), None);
        assert!(matches!(b.compact(None).await.unwrap(), CompactResult::Compacted { .. }));
        collect(go(&b, "go on")).await;
        let history = format!("{:?}", provider.requests().last().unwrap().0);
        assert!(history.contains("The summary."));
        assert!(!history.contains("plan tool"));
    }

    #[tokio::test]
    async fn a_malformed_saved_plan_is_dropped_and_the_session_still_loads() {
        let dirs = Dirs::new();
        let sessions = Arc::new(SessionStore::new(&dirs.dir));
        let a = scripted(&dirs, &Scripted::new(vec![say("Hi.")]), |i| i.sessions = Some(sessions.clone()));
        collect(go(&a, "hi")).await;
        a.flush().await;
        let file = sessions.file(&dirs.project, &a.id());
        let mut saved: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        saved["plan"] = serde_json::json!([{ "text": "x", "status": "started" }]);
        std::fs::write(&file, saved.to_string()).unwrap();
        let b = scripted(&dirs, &Scripted::new(vec![]), |i| i.sessions = Some(sessions.clone()));
        let resumed = b.resume(ResumeTarget::Id(a.id())).await.unwrap().unwrap();
        assert_eq!(resumed.id, a.id());
        assert_eq!(resumed.plan, None);
        assert_eq!(b.plan(), None);
    }

    #[tokio::test]
    async fn plan_off_leaves_it_out_of_every_request_and_the_system_prompt_and_a_call_to_it_is_unknown() {
        let dirs = Dirs::new();
        let provider = Scripted::new(vec![use_tools(vec![plan_call("p1", plan_items())]), say("Ok.")]);
        let session = scripted(&dirs, &provider, |i| i.plan = false);
        let events = collect(go(&session, "plan")).await;
        for (_, options) in provider.requests() {
            assert!(!tool_names(&options).contains(&"plan".to_string()));
            assert!(!options.system.as_deref().unwrap().contains("plan tool"));
        }
        let end = events.iter().find_map(|e| match e {
            SessionEvent::Loop(LoopEvent::ToolEnd { run, .. }) => Some(run.result.clone()),
            _ => None,
        });
        let end = end.unwrap();
        assert!(end.is_error);
        assert!(end.output.starts_with("Unknown tool \"plan\""), "{}", end.output);
        assert_eq!(session.plan(), None);
    }

    #[tokio::test]
    async fn a_subagent_is_offered_no_plan_and_its_call_to_one_fails() {
        let dirs = Dirs::new();
        let parent = Scripted::new(vec![
            use_tools(vec![call(
                "a1",
                "agent",
                serde_json::json!({ "description": "look around", "prompt": "sub-task: find the notes" }),
            )]),
            say("Parent done."),
        ]);
        let sub = Scripted::new(vec![use_tools(vec![plan_call("s1", plan_items())]), say("Found them.")]);
        let (p, s) = (parent.clone(), sub.clone());
        let routed = Hook(move |history: &[ChatTurn], options: &StreamOptions| {
            let is_sub = history.iter().any(|t| matches!(t, ChatTurn::User { text } if text.contains("sub-task")));
            if is_sub { s.stream(history, options.clone()) } else { p.stream(history, options.clone()) }
        });
        let session = make(&dirs, fixed(Arc::new(routed)), |_| {});
        collect(go(&session, "go")).await;
        let sub_requests = sub.requests();
        assert!(!tool_names(&sub_requests[0].1).contains(&"plan".to_string()));
        assert!(text_of(sub_requests[1].0.last().unwrap()).starts_with("Unknown tool \"plan\""));
        assert_eq!(session.plan(), None);
    }

    #[tokio::test]
    async fn mid_turn_compaction_keeps_the_plan_then_the_mid_turn_note() {
        let dirs = Dirs::new();
        let provider = Scripted::new(vec![
            use_tools(vec![plan_call("p1", plan_items())]),
            with(used(900), use_tools(vec![read("c1")])),
            say("SUMMARY"),
            say("Finished."),
        ])
        .with_context(1000);
        let session = scripted(&dirs, &provider, |_| {});
        collect(go(&session, "long task")).await;
        let after = &provider.requests()[3].0;
        assert!(text_of(&after[0]).ends_with(&format!("SUMMARY\n\n{PLAN_TEXT}")), "{}", text_of(&after[0]));
        assert_eq!(text_of(after.last().unwrap()), MID_TURN_NOTE);
    }

    #[tokio::test]
    async fn compaction_before_a_message_keeps_the_plan_and_the_message_follows() {
        let dirs = Dirs::new();
        let provider = Scripted::new(vec![
            use_tools(vec![plan_call("p1", plan_items())]),
            with(used(900), say("Paused.")),
            say("SUMMARY"),
            say("Going on."),
        ])
        .with_context(1000);
        let session = scripted(&dirs, &provider, |_| {});
        collect(go(&session, "long task")).await;
        collect(go(&session, "go on")).await;
        let after = &provider.requests()[3].0;
        assert!(text_of(&after[0]).contains("- [in progress] Add the --json flag"));
        assert_eq!(after.last(), Some(&ChatTurn::User { text: "go on".into() }));
    }

    #[tokio::test]
    async fn a_finished_plan_isnt_handed_to_compaction() {
        let dirs = Dirs::new();
        let done = serde_json::json!([{ "text": "All of it", "status": "done" }]);
        let provider = Scripted::new(vec![
            use_tools(vec![plan_call("p1", done)]),
            with(used(900), say("Done.")),
            say("SUMMARY"),
            say("Ok."),
        ])
        .with_context(1000);
        let session = scripted(&dirs, &provider, |_| {});
        collect(go(&session, "task")).await;
        collect(go(&session, "next")).await;
        assert!(!text_of(&provider.requests()[3].0[0]).contains("plan tool"));
    }
}
