//! The TUI's input boundary: what the engine tells the UI ([`UiEvent`]), what the UI asks of the
//! engine ([`SessionPort`]), and the view-model types both speak. New in marv-rs (no single Marv
//! file): in Marv the App imported `MarvSession` and the session's types directly
//! (`src/session.ts` `SessionEvent`, `src/types.ts` `Message`/`Notice`, `src/tools/types.ts`
//! `ApprovalRequest`/`DiffLine`, `src/provider/models.ts` `ModelInfo`, `src/sessions.ts`
//! `SessionSummary`, `src/usage.ts` `Totals`).
//!
//! Why a boundary instead of `use marv_session::*`:
//! - the engine and the TUI are built in parallel, and the TUI must not pin the engine's API;
//! - `docs/ARCHITECTURE.md` plans a daemon + socket protocol later; these types are what would go
//!   over it, so they are plain data built from `marv-core` types (no handles, no closures);
//! - the engine stays UI-free: it maps its `SessionEvent`/`LoopEvent` into [`UiEvent`] in one place.
//!
//! Shapes mirror Marv's so the mapping is 1:1. [`Message`] keeps Marv's JSON field names because
//! saved sessions store the transcript the user saw in exactly this shape (`Resumed.transcript`).
//!
//! **Text is raw here.** Every string that came from a model, a tool, a file, an MCP server or the
//! mesh is stored as-is and cleaned with `printable()` by the component that draws it.

use marv_core::{StopReason, Usage};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------------------------
// The transcript (Marv `src/types.ts`)
// ---------------------------------------------------------------------------------------------

/// Who a transcript entry is from. `System` is Marv's own notices (help text, errors), not the LLM
/// system prompt; `Tool` is a tool call the agent made, shown with its outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    System,
    Tool,
}

/// A tool entry's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolState {
    Running,
    Done,
    Error,
    Declined,
}

/// Marv's `ToolStatus`: what a tool entry shows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolStatus {
    /// What it was called on, e.g. the file path.
    pub label: String,
    pub status: ToolState,
    /// e.g. "42 lines", or the error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// A subagent's finished tool calls, shown with ctrl+o.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<Vec<String>>,
    /// What a file change did (edit_file, write_file), drawn under the entry: collapsed, all with ctrl+o.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<DiffShown>,
}

/// How important a notice is; its left bar, title and worst-of tone use its colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NoticeTone {
    Info,
    Warning,
    Error,
}

/// One bullet of a notice: a short headline, then (optionally) indented detail lines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoticeItem {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Vec<String>>,
}

/// A distinct, coloured block of startup and other issues (see `NoticeView`), transcript only,
/// never sent to the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notice {
    pub title: String,
    pub tone: NoticeTone,
    pub items: Vec<NoticeItem>,
}

/// A single entry in the transcript the user sees (Marv's `Message`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: u64,
    pub role: Role,
    /// For `Tool`: the tool name.
    pub text: String,
    /// Renders a system message in the error colour.
    #[serde(default, rename = "isError", skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
    /// Render a system message as Markdown (lists get hanging indents).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub markdown: bool,
    /// Renders a system message as a distinct notice block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<Notice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<ToolStatus>,
}

// ---------------------------------------------------------------------------------------------
// Diffs and approvals (Marv `src/tools/types.ts`; mirrors marv-tools' types field for field)
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiffKind {
    Add,
    Del,
    Ctx,
    /// Skipped unchanged lines between hunks ("…").
    Gap,
}

/// One line of a diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    pub kind: DiffKind,
    #[serde(default)]
    pub text: String,
    /// The line's number before the change (removed and unchanged lines).
    #[serde(rename = "oldLine", default, skip_serializing_if = "Option::is_none")]
    pub old_line: Option<usize>,
    /// The line's number after the change (added and unchanged lines).
    #[serde(rename = "newLine", default, skip_serializing_if = "Option::is_none")]
    pub new_line: Option<usize>,
}

/// A change as the transcript shows it: at most MAX_SHOWN_DIFF lines, and how many more there were.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffShown {
    pub lines: Vec<DiffLine>,
    #[serde(default)]
    pub more: usize,
}

/// What the approval prompt shows before a tool changes something.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Preview {
    /// "Edit src/app.ts", "Run a command".
    pub title: String,
    pub diff: Option<Vec<DiffLine>>,
    /// A shell command, shown as "$ command".
    pub command: Option<String>,
    /// Plain text, e.g. a memory to save.
    pub text: Option<String>,
    /// A small detail line, e.g. "sandboxed · no network".
    pub note: Option<String>,
    /// Shown in the warning colour, e.g. "runs WITHOUT a sandbox".
    pub warning: Option<String>,
    /// Several files, each with its own diff (/undo: what goes back where).
    pub files: Option<Vec<PreviewFile>>,
}

/// One file of [`Preview::files`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreviewFile {
    /// "Restore src/app.ts".
    pub title: String,
    pub diff: Option<DiffShown>,
    /// "binary", "too large to show", "changed since".
    pub note: Option<String>,
}

/// "Don't ask again this session" applies to everything with the same key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scope {
    pub key: String,
    /// For the prompt: "Yes, and don't ask again for <description> this session".
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub tool: String,
    pub label: String,
    pub preview: Preview,
    pub scope: Scope,
    /// Which subagent is asking ("implementer · Task 2"); None for the main agent.
    pub agent: Option<String>,
    /// The action can reach the network (bash with network: true).
    pub network: bool,
    /// Yes or no only, no "don't ask again": each one is its own decision (/mcp add, a door to the host).
    pub once: bool,
    /// The call leaves the sandbox through this door: the prompt shows its warning line (Marv Sprint 4).
    pub door: Option<marv_core::Door>,
}

/// Identifies one pending approval so the answer reaches the right waiter (parallel subagents can
/// ask at once; the TUI queues them and shows the head).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ApprovalId(pub u64);

/// The user's answer. (`unavailable` is the headless approver's; the TUI never sends it.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Run it.
    Yes,
    /// Run it, and everything in its scope this session.
    Always,
    /// Don't; this stops the run so the user can say what to do instead.
    No,
}

// ---------------------------------------------------------------------------------------------
// Status bar, pickers, setup
// ---------------------------------------------------------------------------------------------

/// The session's running totals (Marv `src/usage.ts` `Totals`), for the status bar's cost.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Totals {
    pub requests: u64,
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub completion_tokens: u64,
    /// USD; None when neither reported nor estimable.
    pub cost: Option<f64>,
    /// Some of the cost was estimated from prices rather than reported (`~$`).
    pub estimated: bool,
    /// A local model (Ollama): no cost.
    pub local: bool,
}

/// What the status bar shows besides the hint: the model, the folder and yolo.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatusInfo {
    /// The provider/model name as the session reports it (`session.providerName`).
    pub model: String,
    /// For display, e.g. "~/Projects/app".
    pub cwd: String,
    /// Yolo mode is on: always shown, so the user knows what runs without asking.
    pub yolo: bool,
    /// The model's context window, when known (status bar `12.3k/32.8k ctx`).
    pub context_length: Option<u64>,
    /// Background processes running (Marv #51): shown whenever there are any, since they keep running between turns.
    pub processes: usize,
}

/// What the welcome banner says was loaded at startup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WelcomeInfo {
    pub version: String,
    pub cwd: String,
    /// An AGENTS.md was loaded.
    pub instructions: bool,
    pub skills: usize,
    pub memories: usize,
    pub mcp_servers: usize,
}

/// Whether a model can reason before answering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningMode {
    /// Can be turned off.
    Optional,
    /// Can't (OpenRouter rejects trying).
    Mandatory,
}

/// A model the picker can offer (Marv `ModelInfo`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelInfo {
    pub id: String,
    /// Can call tools. None = unknown.
    pub tools: Option<bool>,
    /// USD per million tokens.
    pub price_in: Option<f64>,
    pub price_out: Option<f64>,
    pub price_cache_read: Option<f64>,
    pub context: Option<u64>,
    /// Runs on this machine (Ollama).
    pub local: bool,
    pub reasoning: Option<ReasoningMode>,
}

/// One saved session for `/resume` (Marv `SessionSummary`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    /// Unix ms.
    pub updated_at: u64,
    pub messages: usize,
    pub model: String,
}

/// A saved session, brought back (Marv `Resumed`).
#[derive(Debug, Clone, PartialEq)]
pub struct Resumed {
    pub id: String,
    pub updated_at: u64,
    pub model: String,
    /// What the user saw.
    pub transcript: Vec<Message>,
    pub totals: Totals,
    /// The plan tool's list as it was saved (#15), pinned again.
    pub plan: Option<Vec<PlanItem>>,
}

/// One item of the plan tool's checklist (Marv `src/tools/types.ts` `PlanItem`, #15).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanItem {
    pub text: String,
    pub status: PlanStatus,
}

/// At most one item is `InProgress` at a time (the current item).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanStatus {
    Pending,
    InProgress,
    Done,
}

/// The providers setup offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderId {
    OpenRouter,
    Ollama,
}

/// What setup needs to know about a provider (Marv `PRESETS[provider]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderPreset {
    pub id: ProviderId,
    pub label: String,
    /// The environment variable holding its key, if it needs one.
    pub key_env: Option<String>,
    pub key_url: Option<String>,
    pub default_model: String,
}

/// Where setup starts: the whole wizard (first run, `/setup`) or just the model (`/model`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupStart {
    FirstRun,
    Reconfigure,
    Model,
}

/// What the setup screen starts from. Keys arrive masked (`maskKey`): the full key never reaches
/// the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupContext {
    pub start: SetupStart,
    pub presets: Vec<ProviderPreset>,
    /// The saved provider and model; None on first run.
    pub current: Option<(ProviderId, String)>,
    /// The saved API key, masked, if there is one.
    pub saved_key: Option<String>,
    /// A key found in the environment for each provider, masked.
    pub env_keys: Vec<(ProviderId, String)>,
}

/// What the user chose in setup. `api_key: None` keeps the saved key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupAnswer {
    pub provider: ProviderId,
    pub model: String,
    pub api_key: Option<String>,
}

/// A slash command for the `/` menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandInfo {
    /// Without the slash; may have words ("mcp login github") for argument completions.
    pub name: String,
    pub description: String,
}

/// A running subagent's state for its transcript entry (Marv `AgentProgress`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentProgress {
    /// "worktree marv/x-1a2b · 3 tools · read_file src/a.ts"
    pub line: String,
    /// Its finished tool calls, newest last.
    pub steps: Vec<String>,
}

// ---------------------------------------------------------------------------------------------
// Events: engine → UI
// ---------------------------------------------------------------------------------------------

/// What a turn is doing between the model's replies (Marv `status` event).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnStatus {
    WaitingForMcp,
    Compacting,
    Checking,
    /// The turn's checkpoint is taking a while (#16): "Saving a checkpoint…".
    Checkpointing,
    Running,
}

/// How a turn ended: how the agent loop ended, or `Interrupted` before the loop began.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnEndReason {
    Loop(StopReason),
    Interrupted,
}

/// What an empty reply leads to (Marv `empty_reply.next`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmptyNext {
    Retry,
    Nudge,
    Stop,
}

/// How a compaction went (Marv `CompactResult`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactOutcome {
    Compacted { summary: String, tokens_before: Option<u64> },
    NotCompacted { reason: String, error: String },
}

/// Everything the engine tells the UI. One enum, sent on one channel, so the UI's event loop has a
/// single place to wake for engine work.
///
/// Turn events mirror Marv's `SessionEvent` (which includes the agent's `LoopEvent`s); a subagent's
/// events arrive wrapped in [`UiEvent::Subagent`]. The rest are answers to [`SessionPort`] calls.
#[derive(Debug, Clone, PartialEq)]
pub enum UiEvent {
    // --- a turn (Marv SessionEvent / LoopEvent) ---
    /// Always first.
    TurnStart {
        turn: String,
    },
    Status(TurnStatus),
    /// Streamed reply text. The UI batches these (see `app`: revealed at a pace, `REVEAL_MS`).
    TextDelta(String),
    /// The model's private reasoning, previewed by `ThinkingView`.
    ThinkingDelta(String),
    /// A tool call still streaming (display only), shown live by `DraftView`.
    ToolCallDelta {
        index: usize,
        name: String,
        text: String,
    },
    /// One step's reply text is complete.
    Assistant {
        text: String,
    },
    ToolStart {
        call_id: String,
        name: String,
        label: String,
    },
    ToolEnd {
        call_id: String,
        name: String,
        status: ToolStatus,
    },
    /// A call answered without running (after a no or an interrupt).
    ToolSkipped {
        call_id: String,
        name: String,
        output: String,
    },
    /// The latest request's usage plus the session totals.
    Usage {
        last: Usage,
        totals: Totals,
    },
    Error(String),
    StepLimit {
        steps: u32,
        continued: bool,
    },
    EmptyReply {
        next: EmptyNext,
    },
    CutOff {
        continued: bool,
    },
    TextLoop {
        continued: bool,
    },
    ToolLoop {
        continued: bool,
    },
    Compacted,
    Retry {
        attempt: u32,
        max: u32,
        message: String,
        continued: bool,
    },
    Done(StopReason),
    /// A subagent's own event; `call_id` is the agent tool call that started it (its view's key).
    Subagent {
        call_id: String,
        title: String,
        event: Box<UiEvent>,
    },
    SubagentProgress {
        call_id: String,
        progress: AgentProgress,
    },
    /// The main agent's plan call ended (#15): its new list, empty when cleared. (Marv's App read `result.plan` off
    /// the plan call's `tool_end`; subagents have no plan tool.)
    Plan(Vec<PlanItem>),
    /// An automatic (or `/compact`) compaction, and how it went.
    Compaction {
        automatic: bool,
        outcome: CompactOutcome,
    },
    /// Always last, exactly once, however the turn ended.
    TurnEnd {
        turn: String,
        reason: TurnEndReason,
    },

    // --- asks ---
    /// A tool wants approval. The UI queues these and answers each with [`SessionPort::answer`].
    Approval {
        id: ApprovalId,
        request: ApprovalRequest,
    },

    // --- transcript and status from outside a turn ---
    /// A message for the transcript: command output, a notice (startup, MCP, checks)...
    Message(Message),
    /// The model, folder, yolo or context window changed.
    StatusInfo(StatusInfo),
    /// What `ctrl+c`-to-exit / the welcome banner need, sent once at startup.
    Welcome(WelcomeInfo),

    // --- results of commands and port calls ---
    /// `/clear`: empty the transcript (and the selection's remembered rows).
    Clear,
    /// `/setup`, `/model`, or first run.
    OpenSetup(SetupContext),
    /// `/resume` without an id: the picker's list, newest first.
    Sessions(Vec<SessionSummary>),
    /// The answer to [`SessionPort::load_models`].
    Models(Result<Vec<ModelInfo>, String>),
    /// A session was brought back: replace the transcript.
    Resumed(Resumed),
    /// A short-lived status bar message ("Copied 3 lines").
    Flash(String),
    /// Work outside a turn started or stopped (`/mcp add`'s lookup...): shows the waiting line.
    Busy(Option<String>),
    /// `/exit`, or the engine is going away.
    Exit,
}

// ---------------------------------------------------------------------------------------------
// Requests: UI → engine
// ---------------------------------------------------------------------------------------------

/// What the TUI asks of the engine. Implemented by marv-session (later, perhaps, by a socket
/// client to a daemon).
///
/// Every method returns at once and must not block the UI thread: results come back as
/// [`UiEvent`]s on the channel handed to [`crate::App::new`]. That keeps the UI's event loop the
/// only place that waits, and keeps this trait object-safe without async-trait.
pub trait SessionPort: Send + Sync {
    /// The commands for the `/` menu (stable for the session; read once).
    fn commands(&self) -> Vec<CommandInfo>;

    /// Starts a turn with the user's message. A message while busy is the engine's to refuse
    /// (it answers with a system [`UiEvent::Message`]); the UI also doesn't send one.
    fn send(&self, text: String);

    /// Runs a slash command line (`"/model"`, `"/mcp login github"`). Marv's commands are pure
    /// functions returning an action; the engine applies the engine-side part (config, yolo,
    /// memory, MCP) and sends the UI-side part as events (`Message`, `Clear`, `OpenSetup`,
    /// `Sessions`, `Exit`, ...).
    fn run_command(&self, line: String);

    /// Answers one pending approval.
    fn answer(&self, id: ApprovalId, decision: Decision);

    /// Esc / ctrl+c: stop the running turn or command. The UI declines its queued approvals
    /// itself (with [`answer`](Self::answer)) before calling this.
    fn abort(&self);

    /// The setup screen's model step needs the provider's list ([`UiEvent::Models`]).
    fn load_models(&self, provider: ProviderId);

    /// Setup finished: save the config and reconfigure the provider.
    fn complete_setup(&self, answer: SetupAnswer);

    /// `/resume` picked a session, or `marv -c` (`None` = the latest here).
    fn resume(&self, id: Option<String>);

    /// Before the process exits: save the session and flush the trajectory. The returned future is
    /// awaited (bounded) after the terminal is restored.
    fn flush(&self) -> futures::future::BoxFuture<'static, ()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_keeps_marv_json_field_names() {
        let json = r#"{"id":3,"role":"tool","text":"edit_file","tool":{"label":"a.ts","status":"done","diff":{"lines":[{"kind":"add","text":"x","newLine":1}],"more":0}}}"#;
        let message: Message = serde_json::from_str(json).unwrap();
        assert_eq!(message.role, Role::Tool);
        assert_eq!(message.tool.as_ref().unwrap().diff.as_ref().unwrap().lines[0].new_line, Some(1));
        assert_eq!(serde_json::to_string(&message).unwrap(), json);
        let system: Message = serde_json::from_str(r#"{"id":1,"role":"system","text":"x","isError":true}"#).unwrap();
        assert!(system.is_error);
    }
}
