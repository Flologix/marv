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
//! - The project's formatter (#67): the setting (config `formatter`) goes onto each turn's tool context, where the
//!   edit tools run it on the files they write (`marv_tools::formatter`); subagents inherit the hook.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher, Hasher};
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures::{FutureExt, Stream, StreamExt};
use marv_agent::compact::{MID_TURN_NOTE, SummarizeOptions};
use marv_agent::{AgentOptions, LimitAnswer, LoopEvent};
use marv_core::{ChatTurn, Provenance, StopReason, ToolCall, ToolSpec, Usage};
use marv_mcp::deferred::{
    DEFERRED_NAMES, McpMode, McpToolsSetting, ModeChoice, announcement, as_mcp_call, choose_mode, deferred_specs,
};
use marv_provider::{ModelInfo, Provider, ProviderFactory, ProviderOption};
use marv_sandbox::TaintState;
use marv_tools::formatter::FormatterSetting;
use marv_tools::{AgentProgress, Approver, Decision, PlanHost, PlanItem, ProcessInfo, ToolContext, ToolRef};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_util::sync::CancellationToken;

use crate::agents::{AgentType, builtin_agents};
use crate::checkpoints::store::{CheckpointKind, CheckpointStore, SnapshotResult, StoreOptions};
use crate::checkpoints::turn::CheckpointTurn;
use crate::checkpoints::undo::{UndoPlan, UndoRefusal, UndoResult, apply_undo, plan_undo, undo_note};
use crate::checks::{ProjectCheckResult, Violation};
use crate::config::{Diagnostics, Language};
use crate::cut_off::{OpenTurn, SavingApprover, cut_off_note, cut_off_notice};
use crate::diagnostics::turn::{
    CheckOutcome, Checker, FailReason, OffReason, TurnChecks, TurnOptions, combine_checks, combine_checks_within,
};
use crate::diagnostics::{CheckResult, Diagnostic};
use crate::event_queue::EventQueue;
use crate::feedback::classify_reply;
use crate::hooks::{HookCommand, HookRun};
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
use crate::usage::{Prices, Totals, add_model_usage};

/// Saved this long after a turn ends (and again on flush), so a burst of turns is one write.
pub const SAVE_DELAY: std::time::Duration = std::time::Duration::from_millis(200);
pub const BUSY: &str = "Marv is working on a turn: wait for it to end, or interrupt() it, first.";
pub const CLOSED: &str = "This session is closed.";
/// `send_now()` with no turn to deliver to.
pub const NOT_RUNNING: &str = "No turn is running: send it as a message instead.";

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
            Self::Done(StopReason::TimeLimit) => "time_limit",
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

/// How long a turn waits for the model list when it hasn't answered yet (#143).
pub const LOOKUP_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// Sends `true` when dropped: the lookup's end, however it ends.
struct DropSend(tokio::sync::watch::Sender<bool>);
impl Drop for DropSend {
    fn drop(&mut self) {
        let _ = self.0.send(true);
    }
}

impl CompactFailure {
    /// The name clients and the trajectory use: "empty", "stopped" or "failed".
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

/// marv-rs #144: after a failed automatic summary, the next one waits until the context (last prompt + completion) has
/// grown this much past the size at which it failed.
pub const COMPACT_RETRY_GROWTH: f64 = 1.25;
/// marv-rs #144: at most this many automatic summaries may fail in one turn; then the turn goes on without compacting.
pub const MAX_COMPACT_FAILURES: u32 = 3;

/// marv-rs #144: whether an automatic summary may be asked for again after `failures` failed ones this turn, the last
/// at `failed_at` tokens, with the context now at `now`. Marv gave up for the rest of the turn after one failure, but a
/// `marv -p` run is a single turn: one failed summary (SWE-bench django-12663 at 112k) let it grow to 201k and time out.
/// Growth first, so the next step doesn't pay for the same summary that just failed; a cap, so a model that can't
/// summarize doesn't pay for one every few steps. With a known `window`, the retry point is never past halfway from
/// the failure to the window: when the trigger was 85% of it, 25% more would be past the window and never come.
pub(crate) fn may_compact_again(failures: u32, failed_at: Option<u64>, now: Option<u64>, window: Option<u64>) -> bool {
    if failures == 0 {
        return true;
    }
    if failures >= MAX_COMPACT_FAILURES {
        return false;
    }
    match (failed_at, now) {
        (Some(at), Some(now)) => {
            let grown = at as f64 * COMPACT_RETRY_GROWTH;
            let halfway = window.filter(|w| *w > at).map(|w| at as f64 + (w - at) as f64 / 2.0);
            now as f64 >= halfway.map_or(grown, |h| h.min(grown))
        }
        // The size at the failure isn't known: don't guess, wait for the next turn.
        _ => false,
    }
}

/// What started a compaction (#63): the context nearly full, `/compact`, or a request too long for the window
/// (`level`: how far the recovery went, 0–3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactTrigger {
    Auto,
    Manual,
    Overflow { level: u32 },
}

impl CompactTrigger {
    /// The name clients and the trajectory use: "auto", "manual" or "overflow".
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Manual => "manual",
            Self::Overflow { .. } => "overflow",
        }
    }

    /// The recovery level of an overflow, else `None`.
    pub fn level(self) -> Option<u32> {
        match self {
            Self::Overflow { level } => Some(level),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CompactResult {
    Compacted { summary: String, tokens_before: Option<u64>, trigger: CompactTrigger },
    NotCompacted { reason: CompactFailure, error: String, trigger: CompactTrigger },
}

impl CompactResult {
    pub fn trigger(&self) -> CompactTrigger {
        match self {
            Self::Compacted { trigger, .. } | Self::NotCompacted { trigger, .. } => *trigger,
        }
    }
}

/// The `compaction` event as stream-json and the web client send it: `compacted`, #144's `reason` and `error` for a
/// failed one, and #63's `trigger` (with `level` for an overflow).
pub fn compaction_json(result: &CompactResult) -> serde_json::Value {
    let mut event = match result {
        CompactResult::Compacted { .. } => serde_json::json!({"type": "compaction", "compacted": true}),
        CompactResult::NotCompacted { reason, error, .. } => {
            serde_json::json!({"type": "compaction", "compacted": false, "reason": reason.as_str(), "error": error})
        }
    };
    let trigger = result.trigger();
    event["trigger"] = trigger.as_str().into();
    if let Some(level) = trigger.level() {
        event["level"] = level.into();
    }
    event
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
    /// A diagnostics checker after a step's file changes (tsc; `label` names it): new errors, a failure, or that
    /// it's off.
    Check {
        label: &'static str,
        result: CheckResult,
    },
    /// The project's own checks after a step's file changes.
    Checks(ProjectCheckResult),
    /// The turn's checkpoint (#16) couldn't be saved: the turn goes on, but /undo can't take it back. Once per turn,
    /// and not again while the same failure lasts (`checkpoint_turn`).
    CheckpointFailed {
        message: String,
    },
    /// marv-rs: the session just became tainted by this source (show it; egress asks from now on, even in yolo).
    Tainted {
        source: Provenance,
    },
    /// marv-rs #124 (phase 3): a line in Marv's voice about the radio, for the transcript (a report sent at the
    /// turn's end). Checked names and fixed words only.
    RadioNote {
        text: String,
    },
    /// "Send now" messages the turn never delivered (it was stopped, declined, hit a limit, or ended before its next
    /// step), in the order they were sent: the person's again, never sent later on their own. Just before `TurnEnd`.
    NotSent {
        texts: Vec<String>,
    },
    /// A line in Marv's own words for the transcript (#63: saved pages that couldn't be written during an overflow's
    /// recovery). Fixed text and Marv's own errors only, never the model's.
    Notice {
        text: String,
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
    /// #202: its last turn was cut off (Marv quit mid-turn): the notice, also the transcript's last entry.
    pub cut_off: Option<String>,
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
    /// For a `ProviderOption`: `reasoningEffort` (#143), how hard a reasoning model reasons (a factory has it built in).
    pub reasoning_effort: Option<marv_provider::ReasoningEffort>,
    /// Marv's version, for trajectories.
    pub version: String,
    /// The instruction files, as they go into the prompt (`project_trust::load_project`, which gives each its
    /// heading); plain text of your own goes under "# Project instructions (from AGENTS.md)".
    pub instructions: Option<String>,
    /// Append a folder's own `AGENTS.md`/`CLAUDE.md` (below the root) to the first read_file, edit_file or write_file
    /// result there, once per conversation (#62; `crate::instructions::NestedInstructions`). Default false: an SDK
    /// session reads the project's files only when it asks for the `Project` source; the CLI sets it.
    pub nested_instructions: bool,
    /// Which of the project's subfolder instruction files count as the person's (`crate::project_trust`): any other
    /// is quoted as the project's data. Default: none. The caller loads `instructions`, `skills` and `agents` with the
    /// root's trust applied (`crate::project_trust::load_project`).
    pub nested_trust: Arc<crate::project_trust::NestedTrust>,
    /// The project's root files as loaded and where they stood (`crate::project_trust::load_project`), for `/trust`;
    /// `None`: the project's files weren't loaded.
    pub project_files: Option<(crate::project_trust::ProjectFiles, crate::project_trust::TrustStatus)>,
    pub skills: Vec<LoadedSkill>,
    /// Agent types the agent tool can start (default: the built-in ones, explore and general-purpose).
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
    /// When the run must be done (`marv -p --time-limit`), for the tools ([`ToolContext::deadline`]). None: no limit.
    pub deadline: Option<std::time::Instant>,
    /// `marv -p --dangerously-run-unsandboxed`: the file tools may reach outside the project ([`ToolContext::files_anywhere`]).
    pub files_anywhere: bool,
    /// Marv's own configured secrets (the provider keys: [`crate::config::configured_secrets`]), redacted from every
    /// tool result with the MCP servers' tokens (docs/SECURITY.md). Never sent, shown or saved.
    pub secrets: Vec<String>,
    /// Asked for every call that needs a yes; without it only what yolo vouches for runs. Wrapped by the session
    /// (`SessionApprover`): "always" scopes, `must_ask`, a stop answers "no".
    pub approve: Option<Arc<dyn Approver>>,
    /// The main agent's step limit (default 25; 0 never asks and never stops it). Subagents keep their own 50.
    pub max_steps: Option<u32>,
    /// Offer the plan tool (#15): a checklist the main agent keeps for a long task. Default true. Fixed for the
    /// session's life, since it changes the tool list (prompt cache).
    pub plan: bool,
    /// Which languages' checkers run after the model's edits (#187).
    pub diagnostics: Diagnostics,
    /// The check cache's root (#187), for checkers that keep a build cache; `None`: those don't run.
    pub check_cache: Option<PathBuf>,
    pub checks: bool,
    /// `compactTokens` (#140): compact once the context passes this many tokens, or at 85% of the window if that comes
    /// first. Default [`crate::config::DEFAULT_COMPACT_TOKENS`]; `None`: only the window rule.
    pub compact_tokens: Option<u64>,
    /// `compaction` (#63, temporary for the A/B): v1 (default) or v2 (the template, the verbatim tail, rolling updates).
    pub compaction: marv_agent::compact::Version,
    /// `compactTailTokens` (#63): the v2 verbatim tail's budget; `None`: `min(15k, 15% of the threshold)`; 0: no tail.
    pub compact_tail_tokens: Option<u64>,
    /// The formatter the edit tools run after a change (#67). Default on (detect from the project's config).
    pub formatter: FormatterSetting,
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
    /// Offer `web_fetch`. Default true. `marv -p` turns it off when nothing approves the `web` scope (no `--approve
    /// web`, no config `"approve": ["web"]`): there every fetch would be refused, and each try costs the model a step
    /// (10 refused fetches of the upstream fix in 6 SWE-bench instances in a benchmark run). Decided once, before
    /// the session exists: the tool list is fixed for the session (the prompt cache). Subagents follow it.
    pub web: bool,
    pub yolo: bool,
    /// `approveSandboxed` (config; default off here, on in the TUI): a sandboxed command without network runs without
    /// asking in a person's turn or a wake, yolo or not, the home folder included (its sandbox is read-only there).
    /// Never in `marv -p` (`one_shot`): its `--approve` flags stay the contract.
    pub approve_sandboxed: bool,
    /// Yolo isn't allowed here, and why (the project is the home folder or above it).
    pub no_yolo: Option<String>,
    pub sessions: Option<Arc<SessionStore>>,
    pub trajectories: Option<Arc<TrajectoryStore>>,
    pub log_trajectories: bool,
    /// `~/.marv/worktrees/<project>`; without it, no worktrees.
    pub worktrees_dir: Option<PathBuf>,
    /// `~/.marv/tool-output` (#61): a cut tool result is saved whole in `<dir>/<session id>/`, which read_file and grep
    /// may read; old sessions' folders are cleaned up at its first turn. Without it, cuts aren't saved.
    pub tool_output_dir: Option<PathBuf>,
    /// Where the edit tools' cross-process file locks go when there's no runtime folder (`~/.marv/file-locks`;
    /// `marv_tools::file_lock`). None: then such a session orders calls within its own process only.
    pub file_locks_dir: Option<PathBuf>,
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
    /// Session-start hooks (#108): the user's own config's only (`crate::hooks::session_start_hooks`), never a project's. Run
    /// for each conversation (the first, after `clear()`, on `resume()`), their output sent once at the start of its
    /// first message. Never in subagents. Default: none.
    pub hooks: Vec<HookCommand>,
    /// A one-shot run (`marv -p`): the session ends with the reply, and its background processes with it (#86).
    /// Default false.
    pub one_shot: bool,
    /// Offer the herdr tool (#74): Marv runs in a Herdr pane (`HERDR_ENV` at startup, checked once by the caller,
    /// since the tool list and the prompt are fixed for the session). Its reads run anywhere; its commands and spawns
    /// only with `host_commands` (someone approves each one at a terminal), and only for the main agent. Default
    /// none.
    pub herdr: Option<crate::herdr_door::HerdrSetup>,
    /// Start tainted (`MARV_TAINTED=1`, #74): this Marv was spawned by one whose conversation held untrusted text,
    /// and its task came from there. Marked (and shown, and logged) at the first turn, as `Provenance::Herdr`. It can
    /// only add taint. Default false.
    pub tainted_at_start: bool,
    /// The mesh radio (marv-rs #124): `None` (the default) for a session that is never on it (`marv -p`, the SDK);
    /// `Some(on)` for the TUI's, `on` being whether the user's own config has it on as the session starts. A
    /// conversation that starts with the radio on offers `radio_tx` and the prompt's radio sentence for its whole life
    /// (the prompt cache), even after `/radio off`; one that starts with it off offers them after `clear()` once it's
    /// on. Needs `mesh`, whose radio it reads after the first conversation.
    pub radio: Option<bool>,
}

impl SessionInit {
    pub fn new(root: impl Into<PathBuf>, provider: ProviderChoice) -> Self {
        Self {
            root: root.into(),
            cwd: None,
            provider,
            thinking: false,
            reasoning_effort: None,
            version: env!("CARGO_PKG_VERSION").into(),
            instructions: None,
            nested_instructions: false,
            nested_trust: Arc::default(),
            project_files: None,
            skills: Vec::new(),
            agents: builtin_agents(),
            memory: None,
            mcp: None,
            owns_mcp: false,
            mesh: None,
            wire_approver: None,
            tools: Vec::new(),
            system_prompt: None,
            deadline: None,
            files_anywhere: false,
            secrets: Vec::new(),
            approve: None,
            max_steps: None,
            plan: true,
            diagnostics: Diagnostics::ON,
            check_cache: Some(crate::diagnostics::cache::default_root_from_env()),
            checks: true,
            compact_tokens: Some(crate::config::DEFAULT_COMPACT_TOKENS),
            compaction: marv_agent::compact::Version::V1,
            compact_tail_tokens: None,
            formatter: FormatterSetting::Auto,
            checkpoints: true,
            checkpoints_dir: None,
            checkpoint_store: None,
            mcp_tools: McpToolsSetting::Auto,
            sandbox: true,
            host_commands: false,
            github: false,
            yolo: true,
            approve_sandboxed: false,
            no_yolo: None,
            sessions: None,
            trajectories: None,
            log_trajectories: true,
            worktrees_dir: None,
            tool_output_dir: None,
            file_locks_dir: None,
            transcript: None,
            on_change: None,
            on_warning: None,
            problems: Vec::new(),
            processes: true,
            process_manager: None,
            pre_approved: Vec::new(),
            writable: Vec::new(),
            hooks: Vec::new(),
            one_shot: false,
            web: true,
            herdr: None,
            tainted_at_start: false,
            radio: None,
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
    /// `approveSandboxed`: from the next turn (it is still never on for `marv -p`).
    pub approve_sandboxed: Option<bool>,
    pub trajectories: Option<bool>,
    pub diagnostics: Option<Diagnostics>,
    pub checks: Option<bool>,
    /// Checkpoints before each turn's changes (#16), for `undo()`.
    pub checkpoints: Option<bool>,
    /// The main agent's step limit (0 never asks).
    pub max_steps: Option<u32>,
    /// Applies from the next conversation that decides (after `clear()`).
    pub mcp_tools: Option<McpToolsSetting>,
}

/// What started a turn that the person didn't: nothing (they did), a message on a wired inbox (#78), or a radio
/// message (#124).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Woken {
    No,
    Inbox,
    Radio(crate::radio::RadioWakeBy),
}

impl Woken {
    fn is_woken(&self) -> bool {
        *self != Woken::No
    }
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
    /// The running model-list lookup: `true` once it has answered (or given up). A turn waits for it briefly (#143).
    lookup_done: Option<tokio::sync::watch::Receiver<bool>>,
    /// What the model sees. Shared with the running loop (`marv_agent::History`).
    conversation: marv_agent::History,
    saved: SavedSession,
    totals: Totals,
    last: Option<Usage>,
    /// Automatic summaries that failed this turn, and the context's size (prompt + completion) before the last one
    /// (#144, [`may_compact_again`]): each is another paid request.
    compact_failures: u32,
    compact_failed_at: Option<u64>,
    /// #63: the overflow recovery level that last succeeded in this step (the next starts below it), and whether that
    /// was the whole-window last attempt. Reset by an overflow's first attempt.
    overflow_level: Option<u32>,
    overflow_last_attempt: bool,
    /// Where each of this conversation's tool results came from (by call id), and whether the call worked, for its
    /// saved pages (their sources, and the page table's lists of files); cleared with the conversation.
    provenance: HashMap<String, (Provenance, bool)>,
    /// The person's latest message as it went into the history (#63 review): overflow recovery never drops the turn
    /// that holds it, whatever Marv's own notes follow it.
    person_message: Option<String>,
    current: Option<CancellationToken>,
    running: bool,
    closed: bool,
    memories: Option<Memories>,
    system: String,
    sandbox: bool,
    yolo: bool,
    approve_sandboxed: bool,
    diagnostics: Diagnostics,
    checks: bool,
    compact_tokens: Option<u64>,
    compaction: marv_agent::compact::Version,
    compact_tail_tokens: Option<u64>,
    formatter: FormatterSetting,
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
    /// The running turn's "send now" messages (`send_now()`), until the turn closes it at its end.
    send_now: Option<marv_agent::agent::SendNow>,
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
    /// Session-start hooks (#108): this conversation's round has started.
    hooks_started: bool,
    /// Bumped by each round: a round replaced meanwhile (clear, resume) drops its results.
    hook_round: u64,
    /// Cancelled when the current round has finished: the first turn waits for it.
    hooks_done: Option<CancellationToken>,
    /// The current round's runs, once it has finished.
    hook_runs: Option<Vec<HookRun>>,
    /// The runs haven't been sent to the model yet (they go once, in the next user message).
    hooks_unsent: bool,
    /// Kills the current round's commands (a newer round, or `close()`).
    hook_stop: Option<CancellationToken>,
    /// The current round's task: `close()` waits for it.
    hook_task: Option<tokio::task::JoinHandle<()>>,
    /// The first turn's checkpoint prune (a `git gc` in Marv's own git dir, at most once a day): close() waits for it,
    /// so nothing still writes in the config folder once the session is closed.
    prune_task: Option<tokio::task::JoinHandle<()>>,
    /// This conversation offers `radio_tx` (#124): it started with the radio on. Fixed until `clear()`.
    radio_offered: bool,
    /// Radio requests a turn read and hasn't reported on yet (#124, phase 3, §7.3): in the session's state (and its
    /// saved file), not the history, so compaction or a resume doesn't lose them. Kept across `clear()`.
    radio_requests: Vec<marv_mesh::radio::OpenRequest>,
    /// What the running turn's end needs for its automatic radio reports: its number, its last reply, and the tool
    /// the user declined.
    radio_turn: RadioTurnFacts,
    /// #202: the running turn, from its user message to its end: saved with it (`SavedSession::cut_off`).
    turn_open: Option<OpenTurn>,
    /// Snapshots, numbered; `written` is the last one on disk, per session file: a write that lost the race to a newer
    /// one of the same file is skipped.
    save_seq: u64,
    written: Arc<tokio::sync::Mutex<HashMap<String, u64>>>,
    /// #202: the resumed conversation's last turn was cut off: the notice, for a client that resumed through the
    /// builder (`take_cut_off_notice`).
    cut_off_notice: Option<String>,
}

/// What a turn's automatic radio report is built from (#124, §7.3): no model call, only what the session has.
#[derive(Debug, Clone, Default)]
struct RadioTurnFacts {
    number: u32,
    /// The turn read the radio (the person's own, or one the radio started) with requests open: it reports on them.
    handles: bool,
    reply: Option<String>,
    declined: Option<String>,
    /// The person joined the turn with "send now": its reply answered them too, and the report never quotes it.
    sent_now: bool,
}

/// Whether two checkpoint failures are the same one, apart from the numbers in them (a file count that moves turn
/// to turn).
fn same_failure(a: &str, b: &str) -> bool {
    let shape = |s: &str| s.chars().filter(|c| !c.is_ascii_digit()).collect::<String>();
    shape(a) == shape(b)
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

    /// The paths a turn's tool reported changing (`ToolOutput::changed`), main agent or subagent.
    fn note_changed(&mut self, turn: u32, event: &LoopEvent) {
        if let LoopEvent::ToolEnd { run, .. } = event
            && !run.changed.is_empty()
        {
            let paths = self.reported.entry(turn).or_default();
            for changed in &run.changed {
                if !paths.contains(changed) {
                    paths.push(changed.clone());
                }
            }
        }
    }
}

struct Inner {
    init: SessionInit,
    /// The hold on this session's saved-outputs folder (#61): the store's cleanup, here or in another Marv, never
    /// deletes it while the session lives. Moved to the resumed session's folder on a resume.
    tool_output_hold: Mutex<Option<marv_tools::output::LiveFolder>>,
    /// The hold on this conversation's swap folder (#63 review): taken at its first page, and on a resume of a
    /// conversation that has pages, so another Marv's cleanup never deletes a live session's pages.
    swap_hold: Mutex<Option<(PathBuf, marv_tools::output::LiveFolder)>>,
    /// The store's cleanup has run (#61): once, at the first turn, when a resume has already set the id.
    tool_output_cleaned: std::sync::atomic::AtomicBool,
    /// Built once: every request sends byte-identical tool definitions.
    specs: Vec<marv_core::ToolSpec>,
    always: AlwaysScopes,
    taint: Arc<TaintState>,
    /// The version of each file this conversation read or wrote (`marv_tools::file_versions`), kept across its turns:
    /// the edit tools refuse to write over a change it hasn't seen. A new conversation (/clear, a resume) starts empty.
    file_versions: Arc<marv_tools::FileVersions>,
    /// The skills as the `skill` tool takes them.
    skills: Arc<Vec<marv_tools::Skill>>,
    /// MCP tools whose schema this conversation has seen (tool_search, or a rejected call): their errors don't repeat it.
    loaded: Arc<Mutex<HashSet<String>>>,
    /// The diagnostics checkers, one per language that has one (#187: tsc so far); each keeps its own timeouts and
    /// turns off on its own.
    diagnostics: Vec<(Language, Arc<Checker<Diagnostic>>)>,
    /// The check cache (#187) the checkers that keep a build cache share.
    check_cache: Option<Arc<crate::diagnostics::cache::CheckCache>>,
    project_checks: Arc<Checker<Violation>>,
    /// Whether this session offers the process tool (Marv #51; decided at construction: it changes the tool list and
    /// the prompt).
    process_on: bool,
    /// The background processes. They belong to the session, not the conversation: they outlive turns, `clear()` and
    /// `resume()`, and stop on `close()`.
    processes: Option<ProcessManager>,
    /// The project's checkpoints (#16); its ref follows the conversation (`State::chain_id`).
    checkpoints: Arc<CheckpointStore>,
    /// The panes the Herdr door opened (#74). Like the processes, they outlive turns, `clear()` and `resume()`.
    peers: Arc<crate::herdr_door::Peers>,
    /// `tainted_at_start` was marked (at the first turn).
    start_taint_marked: AtomicBool,
    /// Nested instruction files the main agent's conversation has had (#62); emptied with it (`clear`, `resume`) and
    /// by a compaction (the summary replaces the results that carried them).
    nested: Arc<crate::instructions::NestedInstructions>,
    /// The running turn's radio hop (#124): what `radio_tx` sends carries it.
    radio_hop: Arc<crate::radio::TurnHop>,
    state: Mutex<State>,
}

/// What a `/clear` keeps of the taint: a background process started with the network, while it runs (it keeps
/// reaching out, and what it prints is outside text), and the taint of a Marv spawned from a tainted conversation
/// (`MARV_TAINTED`: its pane can be typed into by the one that spawned it). Everything else ends with the conversation
/// that held the outside text.
pub fn taint_kept_by_clear(processes: &[ProcessInfo], tainted_at_start: bool) -> Vec<Provenance> {
    let mut kept = Vec::new();
    if processes.iter().any(|p| p.network && p.state == marv_tools::ProcessState::Running) {
        kept.push(Provenance::Network);
    }
    if tainted_at_start {
        kept.push(Provenance::Herdr);
    }
    kept
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

fn factory_of(
    choice: &ProviderChoice,
    thinking: bool,
    reasoning_effort: Option<marv_provider::ReasoningEffort>,
) -> Arc<dyn ProviderFactory> {
    match choice {
        ProviderChoice::Factory(f) => f.clone(),
        ProviderChoice::Option(o) => crate::config::provider_factory(o.clone(), thinking, reasoning_effort),
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
/// Whether the project has no `.git` of its own, so confined commands see the sandbox's empty stand-in: nothing at
/// `.git` (not even a link), or an empty folder. bwrap leaves an empty mount point at `.git` while a confined command
/// or a background process runs (removed afterwards), so a conversation that starts meanwhile must not take it for a
/// repository; git doesn't either, and `git_guard` would mount such a folder read-only, empty, just the same.
fn no_git_of_its_own(root: &std::path::Path) -> bool {
    let git = root.join(".git");
    match std::fs::symlink_metadata(&git) {
        Err(_) => true,
        Ok(meta) if meta.is_dir() => std::fs::read_dir(&git).is_ok_and(|mut entries| entries.next().is_none()),
        Ok(_) => false,
    }
}

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

/// The diagnostics checkers, one per language that has one (#187: tsc, cargo check and pyright so far), sharing the
/// check cache. Their order is the order of their notes in a step's message, and of their share of its budget.
fn diagnostic_checkers(cache: &Option<Arc<crate::diagnostics::cache::CheckCache>>) -> Vec<(Language, Arc<Checker<Diagnostic>>)> {
    let kinds: Vec<(Language, Arc<dyn crate::diagnostics::turn::CheckKind<Diagnostic>>)> = vec![
        (Language::TypeScript, Arc::new(crate::diagnostics::turn::Typecheck)),
        (Language::Rust, Arc::new(crate::diagnostics::cargo::Cargo)),
        (Language::Python, Arc::new(crate::diagnostics::pyright::Pyright)),
    ];
    kinds
        .into_iter()
        .map(|(language, kind)| {
            let mut checker = Checker::new(kind, None);
            checker.cache = cache.clone();
            (language, Arc::new(checker))
        })
        .collect()
}

/// A diagnostics checker as `/diagnostics` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckerInfo {
    pub language: Language,
    pub label: &'static str,
    pub state: String,
}

/// What `/diagnostics` shows besides the setting (#187): each checker's state, and this project's cache folder with
/// its last measured size.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiagnosticsInfo {
    pub checkers: Vec<CheckerInfo>,
    pub cache: Option<(PathBuf, Option<u64>)>,
}

/// A check's trajectory record: `note` is exactly what the model was told; `cache_bytes` the project's check cache
/// size as last measured, for a kind with one.
fn check_record<E>(
    turn: &str,
    source: Option<&'static str>,
    result: &CheckOutcome<E>,
    note: impl Fn(&[E]) -> String,
    cache_bytes: Option<u64>,
) -> TrajectoryRecord {
    let who = Who { turn: turn.into(), agent: "main".into() };
    let (status, ms, before, errors, added, reason, told) = match result {
        CheckOutcome::Done { ms, before, errors, added, .. } => (
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
                FailReason::Pending => "pending",
                FailReason::LowDisk => "low-disk",
                FailReason::CacheRefused => "cache-refused",
            };
            (CheckStatus::Failed, Some(*ms), None, None, None, Some(reason.to_string()), None)
        }
        CheckOutcome::Off { reason } => {
            let reason = match reason {
                OffReason::NoSandbox => "no-sandbox",
                OffReason::Timeouts => "timeouts",
                OffReason::Unreadable => "unreadable",
                OffReason::CacheTooBig => "cache-too-big",
                OffReason::NotInstalled => "not-installed",
            };
            (CheckStatus::Off, None, None, None, None, Some(reason.to_string()), None)
        }
    };
    TrajectoryRecord::Check { who, source, status, ms, before, errors, added, reason, note: told, cache_bytes }
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

/// [`marv_tools::ToolProgress`] through the turn's sink: a call's status line is shown on its card like a subagent's
/// progress, with no steps.
struct CallProgress(Arc<dyn SubagentSink>);
impl marv_tools::ToolProgress for CallProgress {
    fn progress(&self, call_id: &str, line: String) {
        self.0.progress(call_id, AgentProgress { line, steps: Vec::new() });
    }
}

impl SubagentSink for TurnSink {
    fn usage(&self, usage: &Usage, model: Option<&str>) {
        self.session.count(usage, self.factory.as_ref(), self.info.as_ref(), model);
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
        // #98/#82: only names that mean something stay pre-approved; an unknown one is a problem (shown) and dropped,
        // so the seeds, the prompt's list and /permissions all name exactly what was seeded (fixed for the session).
        let (known, problems) = crate::approvals::known_names(&init.pre_approved, "Pre-approved scopes");
        init.pre_approved = known;
        init.problems.extend(problems);
        // An absolute path (#61): the marker names it, and read_file and grep only take an absolute one outside the
        // project. A relative MARV_CONFIG_DIR would otherwise make it a project path.
        init.tool_output_dir =
            init.tool_output_dir.map(|dir| marv_sandbox::paths::normalize(&std::path::absolute(&dir).unwrap_or(dir)));
        // Only where the backend runs a sandbox and its process door: not on macOS yet (#152).
        let backend = marv_sandbox::backend();
        let process_on = init.processes && backend.processes() && backend.available();
        // The process tool is built in only where the session offers it: elsewhere the name is free (Marv).
        let custom: Vec<ToolRef> = init.tools.iter().filter(|t| process_on || t.name() != "process").cloned().collect();
        check_tool_names(&custom, init.plan, init.github).map_err(SessionError::Invalid)?;
        // The door's run and spawn work only with host_commands: its description says which, fixed for the session.
        if let Some(setup) = &mut init.herdr {
            setup.runs = init.host_commands;
        }
        if init.herdr.is_some() && custom.iter().any(|t| t.name() == crate::herdr_door::HERDR) {
            return Err(SessionError::Invalid(format!(
                "A tool named \"{}\" is already built in: give yours another name.",
                crate::herdr_door::HERDR
            )));
        }
        // The main agent's plug (`agent.parent.inbox`), bound now so it's there before the first turn.
        if let Some(mesh) = &init.mesh {
            mesh.plug(crate::mesh::parent_id()).map_err(|e| SessionError::Invalid(format!("The mesh: {e}")))?;
        }
        if let Some(why) = &init.no_yolo
            && init.yolo
        {
            return Err(SessionError::Invalid(why.clone()));
        }
        let factory = factory_of(&init.provider, init.thinking, init.reasoning_effort);
        let provider = factory.make(None, None);
        let has_skills = !init.skills.is_empty();
        // Built once, so every request sends byte-identical tool definitions (the skill tool only when there are
        // skills, the plan tool unless it's off, github only where gh is logged in).
        let mut specs = marv_tools::tool_specs_for(has_skills, init.plan, init.github);
        // An unsandboxed run (`marv -p --dangerously-run-unsandboxed`, fixed for the session) gets bash's spec saying the
        // network is on and the sandbox off, not "off by default" (Terminal-Bench, 2026-11-06).
        if init.files_anywhere && !init.sandbox {
            specs = specs.into_iter().map(marv_tools::bash::unsandboxed_spec).collect();
        }
        specs.retain(|s| process_on || s.name != "process");
        // web_fetch only where a fetch can be approved (`marv -p` without the web scope leaves it out); fixed here.
        specs.retain(|s| init.web || s.name != "web_fetch");
        specs.extend(init.tools.iter().map(|t| t.spec()));
        // The Herdr door (#74), after the caller's tools: fixed for the session (the cache).
        if let Some(setup) = &init.herdr {
            specs.push(marv_tools::DynTool::spec(&crate::herdr_door::HerdrDoor::new(setup.clone(), Arc::default())));
        }
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
            lookup_done: None,
            conversation: Arc::default(),
            saved,
            totals: Totals::default(),
            last: None,
            compact_failures: 0,
            compact_failed_at: None,
            overflow_level: None,
            overflow_last_attempt: false,
            provenance: HashMap::new(),
            person_message: None,
            current: None,
            running: false,
            closed: false,
            memories,
            system: String::new(),
            sandbox: init.sandbox,
            yolo: init.yolo,
            approve_sandboxed: init.approve_sandboxed,
            diagnostics: init.diagnostics,
            checks: init.checks,
            compact_tokens: init.compact_tokens,
            compaction: init.compaction,
            compact_tail_tokens: init.compact_tail_tokens,
            formatter: init.formatter.clone(),
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
            send_now: None,
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
            hooks_started: false,
            hook_round: 0,
            hooks_done: None,
            hook_runs: None,
            hooks_unsent: false,
            hook_stop: None,
            hook_task: None,
            prune_task: None,
            radio_offered: init.radio == Some(true) && init.mesh.is_some(),
            radio_requests: Vec::new(),
            radio_turn: RadioTurnFacts::default(),
            turn_open: None,
            save_seq: 0,
            written: Arc::default(),
            cut_off_notice: None,
        };
        // The config's pre-approved scopes (#98) seed the "always" layer: they mean "always, for this session",
        // through the same path a person's "always" takes, so must_ask and taint still win. An unknown name is a
        // problem (shown), not a silent approval.
        let always = AlwaysScopes::default();
        // The names were checked above, so this can't fail; were it to, nothing is seeded (never more).
        match crate::approvals::seed_prefixes(&init.pre_approved) {
            Ok(prefixes) => always.seed(&prefixes),
            Err(problem) => {
                init.problems.push(problem);
                init.pre_approved.clear();
            }
        }
        // A subfolder instruction file quoted for the first time is a notice (the session's warnings).
        init.nested_trust.set_warn(init.on_warning.clone());
        let check_cache = init.check_cache.clone().map(|root| Arc::new(crate::diagnostics::cache::CheckCache::new(root)));
        let inner = Inner {
            specs,
            always,
            taint: Arc::new(TaintState::new()),
            file_versions: Arc::default(),
            skills: Arc::new(init.skills.iter().map(|s| s.skill.clone()).collect()),
            loaded: Arc::default(),
            nested: Arc::new(crate::instructions::NestedInstructions::new(init.nested_trust.clone())),
            diagnostics: diagnostic_checkers(&check_cache),
            check_cache,
            project_checks: Arc::new(Checker::new(Arc::new(crate::checks::kind::ProjectChecksKind), None)),
            process_on,
            processes: init.process_manager.clone().or_else(|| {
                process_on.then(|| {
                    let mut options = ProcessManagerOptions::new(&init.root);
                    options.on_change = init.on_change.clone();
                    options.one_shot = init.one_shot;
                    ProcessManager::new(options)
                })
            }),
            checkpoints,
            peers: Arc::default(),
            start_taint_marked: AtomicBool::new(false),
            radio_hop: Arc::default(),
            state: Mutex::new(state),
            tool_output_hold: Mutex::default(),
            swap_hold: Mutex::default(),
            tool_output_cleaned: Default::default(),
            init,
        };
        let session = Self { inner: Arc::new(inner) };
        // Held from the start, so no other Marv's cleanup takes it (#61).
        session.hold_tool_output();
        let system = session.build_system();
        session.state().system = system;
        session.lookup();
        Ok(session)
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The first turn is when old saved outputs go (#61), not the session's making: `marv -c` and the SDK resume
    /// after making it, and a cleanup started before then could take the resumed conversation's folder. On a thread
    /// of its own, so a big store never delays the turn.
    fn clean_tool_output_once(&self) {
        if self.inner.tool_output_cleaned.swap(true, std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let id = self.id();
        // #63: the project's swap folders follow the same age rule, and go with a session file that's gone.
        if let Some(store) = &self.inner.init.sessions
            && let Some(folder) = store.file(&self.inner.init.root, &id).parent().map(std::path::Path::to_path_buf)
        {
            let id = id.clone();
            std::thread::spawn(move || {
                let keep = std::time::Duration::from_secs(marv_tools::output::KEEP_DAYS * 86_400);
                marv_tools::swap::clean_swaps(&folder, keep, Some(&id), std::time::SystemTime::now());
            });
        }
        let Some(dir) = self.inner.init.tool_output_dir.clone() else { return };
        std::thread::spawn(move || {
            let keep = std::time::Duration::from_secs(marv_tools::output::KEEP_DAYS * 86_400);
            let max = marv_tools::output::MAX_TOTAL_BYTES;
            marv_tools::output::clean_tool_output(&dir, keep, max, Some(&id), std::time::SystemTime::now());
        });
    }

    /// Holds the current id's saved-outputs folder (#61), letting go of the one held before (a resume's). A hold that
    /// fails only leaves the folder to the usual cleanup.
    fn hold_tool_output(&self) {
        // The swap folder too: let go of the one left behind, hold the current one if it has pages.
        *lock(&self.inner.swap_hold) = None;
        if self.swap_dir().is_some_and(|d| std::fs::symlink_metadata(&d).is_ok_and(|m| m.is_dir())) {
            self.hold_swap();
        }
        let Some(dir) = &self.inner.init.tool_output_dir else { return };
        let held = marv_tools::output::hold(&dir.join(self.id())).ok();
        *lock(&self.inner.tool_output_hold) = held;
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
    /// What the session may do without asking, now (`/permissions`, #82): a copy, for showing only.
    pub fn permissions(&self) -> crate::permissions::Permissions {
        let (yolo, sandbox, approve_sandboxed) = {
            let s = self.state();
            (s.yolo, s.sandbox, s.approve_sandboxed)
        };
        let init = &self.inner.init;
        let doors = [("github", init.github), ("process", self.inner.process_on), ("herdr", init.herdr.is_some())]
            .into_iter()
            .filter_map(|(name, on)| on.then_some(name))
            .collect();
        crate::permissions::Permissions {
            yolo,
            // What's in effect: also off where the sandbox isn't working, as for the call itself (`auto_safe`).
            approve_sandboxed: approve_sandboxed && !init.one_shot && marv_sandbox::Sandbox::from_env(sandbox).active(),
            no_yolo: init.no_yolo.clone(),
            sandbox,
            host_commands: init.host_commands,
            doors,
            remembered: self.inner.always.remembered(),
            pre_approved: init.pre_approved.clone(),
            writable: init.writable.clone(),
            tainted: self.inner.taint.sources(),
            approved_urls: self.inner.taint.approved_prefixes(),
        }
    }
    /// The MCP manager (for `/mcp`), if the session has one.
    pub fn mcp(&self) -> Option<McpHandle> {
        self.inner.init.mcp.clone()
    }
    /// The mesh switchboard (for `marv mesh wire` and the TUI's wire approvals), if any.
    /// The conversation's first user message, as the user typed it (empty for a new conversation): the radio's
    /// presence title (#124) is made from it. Never a file's text, a tool's output, the model's words or another
    /// Marv's.
    pub fn first_message(&self) -> String {
        self.state().saved.transcript.iter().find(|m| m.role == Role::User).map(|m| m.text.clone()).unwrap_or_default()
    }

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

    /// Stops the running turn or compaction (Esc). Approvals still waiting are answered "no".
    pub fn interrupt(&self) {
        if let Some(stop) = &self.state().current {
            stop.cancel();
        }
    }

    /// For /checkpoints: why the last checkpoint failed, or why checkpoints are off for this project.
    /// What `/diagnostics` shows (#187): each checker's state, and this project's cache folder with its last measured
    /// size (read from its record: a big cache is never walked here). Only reads files.
    pub fn diagnostics_info(&self) -> DiagnosticsInfo {
        let checkers = self
            .inner
            .diagnostics
            .iter()
            .map(|(language, checker)| CheckerInfo {
                language: *language,
                label: checker.kind.label(),
                state: checker.state_at(&self.inner.init.root),
            })
            .collect();
        let cache = self.inner.check_cache.as_ref().and_then(|cache| {
            let key = crate::diagnostics::cache::repo_key(&self.inner.init.root);
            let dir = cache.dir(&key);
            std::fs::symlink_metadata(&dir).is_ok().then(|| (dir, cache.recorded_size(&key)))
        });
        DiagnosticsInfo { checkers, cache }
    }

    /// `/diagnostics clear` (#187): deletes this project's check cache folder. `Ok(Some(bytes))` freed, `Ok(None)`
    /// when there was none; an error when a check is using it.
    pub fn clear_check_cache(&self) -> Result<Option<u64>, String> {
        let Some(cache) = &self.inner.check_cache else { return Ok(None) };
        cache.clear(&crate::diagnostics::cache::repo_key(&self.inner.init.root))
    }

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
            let factory = factory_of(&option, thinking, self.inner.init.reasoning_effort);
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
        if let Some(v) = changes.approve_sandboxed {
            s.approve_sandboxed = v;
        }
        if let Some(v) = changes.trajectories {
            s.logging = v;
        }
        if let Some(v) = changes.diagnostics {
            for (language, checker) in &self.inner.diagnostics {
                match (s.diagnostics.get(*language), v.get(*language)) {
                    // Turned back on: forget what made checking give up, so /diagnostics on really does.
                    (false, true) => checker.reset(),
                    // Turned off: its warm-up stops too.
                    (true, false) => checker.stop(),
                    _ => {}
                }
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

    /// The project's root files as this session loaded them, and where they stood, for `/trust`.
    pub fn project_files(&self) -> Option<(crate::project_trust::ProjectFiles, crate::project_trust::TrustStatus)> {
        self.inner.init.project_files.clone()
    }

    /// The skills this session loaded (the person's, and a trusted project's), as the model is offered them.
    pub fn skills(&self) -> Vec<LoadedSkill> {
        self.inner.init.skills.clone()
    }

    /// The project's subfolder instruction files this session quoted (see above).
    pub fn nested_quoted(&self) -> std::collections::BTreeMap<String, String> {
        self.inner.init.nested_trust.quoted()
    }

    /// The background processes (Marv #51): name, command, ports, state.
    pub fn processes(&self) -> Vec<ProcessInfo> {
        self.inner.processes.as_ref().map(ProcessManager::list).unwrap_or_default()
    }

    /// A background process's last `lines` lines of output (the TUI's processes view).
    pub fn process_logs(&self, name: &str, lines: usize) -> Result<String, marv_tools::ToolError> {
        match &self.inner.processes {
            Some(processes) => processes.logs(name, lines),
            None => Err(marv_tools::ToolError::new("This session has no background processes.")),
        }
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
    /// The panes the Herdr door opened this session (#74), for `/peers`.
    pub fn peers(&self) -> Vec<crate::herdr_door::Peer> {
        self.inner.peers.list()
    }

    /// Whether this session has the Herdr door (#74).
    pub fn herdr_offered(&self) -> bool {
        self.inner.init.herdr.is_some()
    }

    /// The door's adapter, for `/peers` (the user's own commands, outside any turn). None without the door.
    fn herdr_adapter(&self) -> Option<marv_mesh::herdr::HerdrAdapter> {
        let setup = self.inner.init.herdr.as_ref()?;
        Some(marv_mesh::herdr::HerdrAdapter::new(&setup.program, setup.own_pane.clone()))
    }

    /// Each peer with its state as Herdr reports it now (None: Herdr didn't say plainly). A peer whose pane Herdr no
    /// longer has is forgotten here: it no longer counts against `herdr.peers`.
    pub async fn peer_states(&self) -> Vec<(crate::herdr_door::Peer, Option<String>)> {
        let Some(adapter) = self.herdr_adapter() else { return Vec::new() };
        let mut listed = Vec::new();
        for peer in self.peers() {
            let state = match adapter.pane(&peer.pane).await {
                Ok(p) => Some(p.status.unwrap_or_else(|| "running".into())),
                Err(e) if marv_mesh::herdr::pane_not_found(&e) => {
                    self.inner.peers.remove(&peer.name);
                    continue;
                }
                Err(_) => None,
            };
            listed.push((peer, state.filter(|s| marv_mesh::herdr::plain_id(s))));
        }
        listed
    }

    /// `/peers read <name>`: a peer's last lines, for the user's screen only (the caller shows it through
    /// `printable()`; it never reaches the model).
    pub async fn read_peer(&self, name: &str, lines: u64) -> Result<String, String> {
        let adapter = self.herdr_adapter().ok_or("This session has no Herdr door.")?;
        let peer = self.peers().into_iter().find(|p| p.name == name).ok_or_else(|| format!("No peer named {name}."))?;
        adapter.read(&peer.pane, lines).await.map_err(|e| e.to_string())
    }

    /// `/peers stop <name>` (the user's, never the model's): the worker is asked to leave if it's at an empty prompt,
    /// then its pane closes (`HerdrAdapter::stop`). A pane that's already gone just forgets the peer. A worktree it
    /// worked in stays (the user removes it once its branch is merged).
    pub async fn stop_peer(&self, name: &str) -> Result<(crate::herdr_door::Peer, marv_mesh::herdr::Stopped), String> {
        let adapter = self.herdr_adapter().ok_or("This session has no Herdr door.")?;
        let peer = self.peers().into_iter().find(|p| p.name == name).ok_or_else(|| format!("No peer named {name}."))?;
        let stopped = adapter.stop(&peer.pane, peer.kind == "marv").await.map_err(|e| e.to_string())?;
        self.inner.peers.remove(name);
        Ok((peer, stopped))
    }

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
        let (memory, radio) = {
            let s = self.state();
            (s.memories.clone(), s.radio_offered)
        };
        let mut tools: Vec<String> = self.inner.specs.iter().map(|s| s.name.clone()).collect();
        if radio {
            tools.push(crate::radio::RADIO_TX.into());
        }
        let base = system_prompt(&PromptInput {
            cwd: self.cwd(),
            tools,
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
            doors: crate::prompt::PromptDoors {
                github: init.github,
                process: self.inner.process_on,
                host: init.host_commands,
                herdr: init.herdr.is_some(),
                herdr_runs: init.host_commands,
            },
            // #98: what's pre-approved, and which folders outside the project are writable — fixed per session.
            pre_approved: init.pre_approved.clone(),
            writable: init.writable.iter().map(|d| crate::paths::shorten_home(d, &crate::paths::home_dir())).collect(),
            date: crate::time::iso_date(crate::time::now_ms()),
            // Checked per conversation, so the cache holds.
            git_placeholder: no_git_of_its_own(&init.root),
            // #268: the session's root against the home folder, both fixed for the session, so the cache holds.
            home_project: marv_sandbox::home_project(&init.root, &crate::paths::home_dir()),
            // Only `marv -p --dangerously-run-unsandboxed` sets `files_anywhere`, with the sandbox off, and nothing
            // toggles either there: fixed for the session (init, not the live `s.sandbox`), so the cache holds.
            unsandboxed: init.files_anywhere && !init.sandbox,
            // Measured once per process (`machine_limits`), so the cache holds.
            limits: (init.files_anywhere && !init.sandbox).then(crate::prompt::machine_limits).flatten(),
        });
        // #124: one fixed section while this conversation offers radio_tx (never a roster, a name or a state).
        let base = if radio { format!("{base}{}", crate::radio::RADIO_PROMPT) } else { base };
        match &init.system_prompt {
            Some(SystemPrompt::Append(more)) => format!("{base}\n\n{more}"),
            _ => base,
        }
    }

    /// Whether a conversation starting now starts with the radio on (#124): only a session that can be on it (the
    /// TUI's), whose switchboard's radio is on.
    fn radio_on_now(&self) -> bool {
        let init = &self.inner.init;
        init.radio.is_some() && init.mesh.as_ref().and_then(|m| m.radio()).is_some_and(|r| r.is_on())
    }

    /// Whether this conversation offers `radio_tx` (#124).
    pub fn radio_offered(&self) -> bool {
        self.state().radio_offered
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
                    && (init.web || t.name() != "web_fetch")
            })
            .cloned()
            .collect();
        tools.extend(init.tools.iter().cloned());
        if let Some(setup) = &init.herdr {
            let door = crate::herdr_door::HerdrDoor::new(setup.clone(), self.inner.peers.clone());
            // #124: its prompt refuses a Marv on the radio, found in this Marv's mesh folder.
            if let Some(dir) = init.mesh.as_ref().and_then(|m| m.peer_dir()) {
                door.adapter.set_mesh_dir(dir);
            }
            tools.push(Arc::new(door));
        }
        if let Some(mesh) = &init.mesh
            && let Ok(plug) = mesh.plug(crate::mesh::parent_id())
        {
            tools.extend(MeshTool::all(&plug, mesh, self.wire_approver()).into_iter().map(|t| Arc::new(t) as ToolRef));
        }
        // #124: radio_tx right after the mesh's tools, in a conversation that started with the radio on; constant
        // bytes, and still there after /radio off (it then says the radio is off).
        if let Some(mesh) = init.mesh.as_ref().filter(|_| self.state().radio_offered) {
            specs.push(crate::radio::radio_tx_spec());
            tools.push(Arc::new(crate::radio::RadioTx { mesh: mesh.clone(), hop: self.inner.radio_hop.clone() }));
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
    #[allow(clippy::too_many_arguments)]
    async fn start_turn_log(
        &self,
        log: Option<&Arc<Trajectory>>,
        turn: &str,
        text: &str,
        for_model: &str,
        specs: &[ToolSpec],
        mcp_choice: Option<ModeChoice>,
        woken: &Woken,
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
                    // A turn the person didn't start (the radio, #124; a mesh wake, #74) runs without yolo.
                    yolo: s.yolo && !woken.is_woken(),
                    sandbox: s.sandbox,
                    diagnostics: s.diagnostics.any(),
                    checks: s.checks,
                    mcp_tools: mcp_choice,
                    radio_wake: match woken {
                        Woken::Radio(by) => {
                            Some(crate::trajectory::RadioWake { id: by.id.clone(), from: by.from.clone(), hop: by.turn_hop() })
                        }
                        _ => None,
                    },
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
        let (done, waiting) = tokio::sync::watch::channel(false);
        self.state().lookup_done = Some(waiting);
        let session = self.clone();
        handle.spawn(async move {
            // Told on every way out (an empty answer, a provider changed meanwhile), so no turn waits past it.
            let _done = DropSend(done);
            let Some(info) = lookup.await else { return };
            {
                let mut s = session.state();
                if s.lookups != ticket {
                    return; // the provider changed meanwhile
                }
                // What the list says changes the request: the reasoning switch, the output limit (#96), and the
                // window that clamps it (#116).
                if info.reasoning.is_some() || info.max_output.is_some() || info.context.is_some() {
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
    ///
    /// `model` is the one that served the request when it isn't the factory's own (a subagent's type named another
    /// model); the per-model totals count it under that name, and without its prices the cost is only the reported one.
    pub(crate) fn count(&self, usage: &Usage, factory: &dyn ProviderFactory, info: Option<&ModelInfo>, model: Option<&str>) {
        let mut s = self.state();
        let served = model.unwrap_or_else(|| factory.model());
        let same = factory.id() == s.factory.id() && factory.model() == s.factory.model();
        let own_prices = model.is_none_or(|m| m == factory.model());
        let prices = match info.or(if same { s.info.as_ref() } else { None }) {
            Some(info) if own_prices => Prices::from(info),
            _ => Prices::default(),
        };
        let local = (s.totals.requests == 0 || s.totals.local == Some(true)) && factory.local();
        let mut totals = add_model_usage(&s.totals, served, usage, &prices);
        totals.local = Some(local);
        s.totals = totals;
    }

    /// Whether the last request used ≥85% of the context window, or passed `compactTokens` (#140): on a model with a
    /// large window (GLM's, on OpenRouter) 85% was never reached, and long sessions resent 200k–400k tokens a step.
    pub(crate) fn nearly_full(&self) -> bool {
        let context = self.context_length();
        let s = self.state();
        let window = context.filter(|c| *c > 0).map(|c| c as f64 * marv_agent::compact::COMPACT_AT);
        let threshold = match (window, s.compact_tokens) {
            (Some(w), Some(b)) => Some(w.min(b as f64)),
            (w, b) => w.or(b.map(|b| b as f64)),
        };
        match (threshold, s.last) {
            (Some(t), Some(last)) => (last.prompt_tokens + last.completion_tokens) as f64 >= t,
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

mod compaction;
mod lifecycle;
mod saving;
mod turn;

#[cfg(test)]
mod tests;

/// Ports `tests/session.test.ts` (the parts whose collaborators are ported: no subagents, worktrees, MCP manager
/// lifecycle or typecheck runs yet).
#[cfg(test)]
mod turn_tests;
