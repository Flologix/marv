//! The agent loop: the heart of a coding agent. Ports `src/agent.ts`.
//!
//! ```text
//! send the conversation (+ tool definitions) → the model replies
//!   ├─ it asked for tools?  run them, append the results, go again
//!   └─ plain answer?        done
//! ```
//!
//! The model decides which tools to use; the loop only runs what it asks for and reports back. It
//! knows nothing about the UI or the vendor: [`run_agent`] yields [`LoopEvent`]s, and the history
//! (the conversation the model sees) is **appended to in place, never rewritten**, so every
//! request starts with the previous one and the provider's prompt cache keeps hitting.
//!
//! Behaviour (Marv `CLAUDE.md`, "Agent loop", and `tests/agent.test.ts`):
//! - **Every tool call gets a result**, even after ctrl+c, a "no", or a consumer that stops
//!   mid-tools (Marv: a `finally`; here: [`LoopState`]'s `Drop` answers the calls left without a
//!   result with [`not_run::ABORTED`], in order). Otherwise the API rejects every later request.
//! - **Tool-call ids are unique across the conversation** ([`make_ids_unique`]): an empty id or one
//!   already used (a duplicate in the reply, Ollama's `call_0` every step, a resumed session's)
//!   gets a `_<n>` suffix, before the history (or anyone else) sees the call. Then `rewrite_call`
//!   (the session turns a direct call to a deferred MCP tool into `mcp_call`, #14).
//! - **Step limit**: every `max_steps` steps (default [`DEFAULT_MAX_STEPS`]; 0 = never), after that
//!   step's tool results are in, `on_limit` is asked: Continue goes on for another `max_steps`;
//!   Stop ends with "as you asked"; Unavailable (no one to answer, `marv -p --approve`) and no
//!   `on_limit` at all (subagents at their own 50) are a hard stop: `Error` + `Done(MaxSteps)`.
//!   With `on_limit`, a `StepLimit` event says what happened. Cancel while asking → `Aborted`.
//! - **Text loops** ([`crate::repetition`]): checked every [`TEXT_CHECK_EVERY`] characters while
//!   streaming and once at the end. The reply is dropped entirely (none of it, nor its calls,
//!   enters the history) and the model is told ([`TEXT_LOOP_NOTE`]); a second in a row ends the
//!   run `Loop`. A step that answered resets the count.
//! - **Tool loops**: a period-1/2 cycle of identical calls *with identical results*: the first
//!   time every call is answered with [`tool_loop_note`] instead of running; the second time the
//!   calls are answered and the run ends `Loop`. marv-rs (#81): a bash command that already failed the same way
//!   twice since the last change, whatever its tail, isn't run again ([`crate::repetition::FailureLoopState`]),
//!   checked as each call is about to run, so an edit earlier in the same reply counts: the note the first time,
//!   the run ends `Loop` (the step's remaining calls not run) the next.
//! - **Empty replies** (no text, whitespace doesn't count, no calls; not failed, cut off or
//!   cancelled): the first is asked for again unchanged (nothing enters the history, so the cache
//!   holds; OpenRouter's upstream hosts sometimes bill for a reply and send nothing, a third of
//!   the failed eval runs), the second gets [`EMPTY_REPLY_NOTE`], the third ends the run `End`.
//! - **Cut-offs** (`done` reason `length`): nothing runs (a `write_file` whose content stops
//!   mid-line would break the file); each call gets [`not_run::CUT_OFF`]; with no calls the model
//!   is told ([`CUT_OFF_NOTE`], or [`CUT_OFF_REASONING_NOTE`] when the reply only reasoned). The
//!   results go into the history **before** the `CutOff` event, so a consumer that stops there
//!   leaves it valid. Two in a row end the run `Length`; after a step that finished, one is
//!   forgiven again (writing several big files in parts).
//! - **Running calls**: one at a time, except a run of consecutive concurrent calls ([`call_groups`]),
//!   at most `max_parallel` ([`MAX_PARALLEL`]) at once; their `ToolStart`/`ToolEnd` events arrive
//!   as they happen, but **results are appended in call order** (prompt cache, determinism).
//!   **A call starts only after its `ToolStart` was delivered** (a consumer that stopped never has
//!   a tool running it can't show or stop): that's why the loop is pull-based. After a cancel or a
//!   "no", nothing new starts: queued calls get "not run" (cancel wins: "Interrupted", not "declined").
//! - After a step's calls are all answered: `on_full` (mid-turn compaction: the history was swapped
//!   for the summary + `MID_TURN_NOTE`; yields `Compacted`), then `after_step` (its note, if any,
//!   goes in as a user message from Marv; skipped after a cut-off, empty reply, decline, cancel or
//!   final answer; a cancel during it ends `Aborted` and drops the note), then the next step.
//! - A `Retry` from the provider means it starts over: this step's text, calls and reasoning are
//!   discarded so the retry doesn't duplicate them.
//! - Marv #63 **overflow recovery**: a reply that failed because the request was longer than the window
//!   (`marv_provider::errors::is_context_overflow`) goes to `on_overflow` when it's set: nothing of the failed reply
//!   enters the history, an [`LoopEvent::Overflow`] replaces the `Error`, and `Recovered` sends the same step again
//!   (the session made the history fit). At most [`MAX_OVERFLOW_RECOVERIES`] a step, so at most 5 requests; the count
//!   resets after a step that got a reply. `Failed(text)` ends `Error` with the hook's text, `Stopped` (or a cancel
//!   during the hook) ends `Aborted`. Unset, an overflow ends the run like any provider error.
//!
//! marv-rs changes and planned work:
//! - **#90 announced next step, no tool call**: a plain answer that ends by announcing what it
//!   will do next ("Now checking issue #7 before commenting:") is how a turn dies halfway (2 of
//!   6 GLM runs of `mcp-comment`; opencode: 0 of 6). When such an answer is about to end the
//!   run, the loop says "continue" once per turn (an [`LoopEvent::AnnouncedStep`], a note, one
//!   more step) — what a user would do. A second announcement ends the run as before, still
//!   logged. The detector is [`crate::announce::announces_next_step`]; the tests pin the
//!   conservative side (questions, summaries and code blocks never fire).
//!   The continue is an ordinary step: it counts toward `max_steps`, so at the limit the #59
//!   wrap-up (or `on_limit`) comes next, and the wrap-up reply is never continued. The #81
//!   failure-loop guard only sees tool calls, so the two never feed each other.
//! - **#58 parallel read-only tools**: Marv runs only consecutive `agent` calls together
//!   (`isParallel`). Here [`ToolRunner::concurrent`] decides, defaulting to `ToolKind::Read`; the
//!   session also returns true for `agent`. Anything else is a barrier.
//! - **#57 reasoning round-trip**: a `Reasoning` event is kept on the assistant turn.
//! - **#59 wrap-up at the step limit**: with `wrap_up_at_limit`, a hard stop first runs one more
//!   request with [`STEP_LIMIT_NOTE`] and `ToolChoice::None` (tools still sent: the cache holds);
//!   any call it makes anyway is answered [`not_run::STEP_LIMIT`]. The stop reason stays `MaxSteps`
//!   (`Aborted` if cancelled during it). Not after an `on_limit` "no": the user chose to stop.
//! - **#64 continue after a dropped stream**: a `Retry` with `continued: true` keeps the partial
//!   text as the assistant turn and sends [`DROPPED_STREAM_NOTE`] instead of starting over (needs a
//!   `continued` flag on `marv_core::ProviderEvent::Retry`, see `docs/subsystems/agent.md`).
//! - A tool's first poll happens right after launch (Marv: `runTool` is called synchronously), so
//!   what it does first (an approval, a cancel) is seen before the next call of the group is shown.
//! - The text-loop check position is reset on `Retry` too (Marv forgot it, so after a retry the
//!   check could lag by up to the discarded length).

use std::collections::{HashSet, VecDeque};
use std::ops::Range;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use futures::{FutureExt, Stream, StreamExt};
use marv_core::{
    ChatTurn, MarvError, Provenance, ProviderEvent, Reasoning, StopReason, ToolCall, ToolChoice, ToolKind, ToolResult, ToolSpec,
    Usage,
};
use marv_provider::{EventStream, Provider, StreamOptions};
use tokio_util::sync::CancellationToken;

pub use crate::announce::{ANNOUNCED_STEP_NOTE, announces_next_step};
use crate::repetition::{
    FailureLoopState, ToolCallKey, ToolLoopState, detect_text_repetition, failing_command_note, make_tool_key, tool_loop_detected,
};

pub const DEFAULT_MAX_STEPS: u32 = 25;
/// Subagents' own hard limit (Marv `SUBAGENT_MAX_STEPS`, in `src/subagent.ts`).
pub const SUBAGENT_MAX_STEPS: u32 = 50;
pub const MAX_PARALLEL: usize = 4;
/// How often (in characters of reply text) a streaming reply is checked for a text loop.
pub const TEXT_CHECK_EVERY: usize = 400;

/// What the model is told about a call that never ran (Marv `NOT_RUN`).
pub mod not_run {
    pub const ABORTED: &str = "Interrupted by the user before this tool ran.";
    pub const DECLINED: &str = "Not run: the user declined an earlier action.";
    pub const CUT_OFF: &str = "Not run: your reply hit the output limit before it finished, so this call may be incomplete. Redo it in smaller steps (for a big file: write_file the first part, then add the rest with append: true).";
    pub const TEXT_LOOP: &str = "Not run: your reply was stuck repeating the same text, so it was dropped.";
    /// #59: a call the wrap-up reply made anyway (it was asked not to).
    pub const STEP_LIMIT: &str = "Not run: the step limit was reached.";
}

/// Told to the model (a user message from Marv) when a reply with nothing to run was cut off at
/// the output limit: without it, the model would see only its own unfinished reply, or nothing
/// (Ollama drops a tool call it was still writing), and would likely try the same thing again.
pub const CUT_OFF_NOTE: &str = "(Automatic note from Marv, the program running you) Your last reply hit the output limit and was cut off before it finished; nothing in it was run. Continue in smaller steps (for a big file: write_file the first part, then add the rest with append: true).";

/// The same for a reply that reasoned and left nothing: it reasoned until the limit or started a
/// tool call that was cut off and dropped; Marv can't tell which.
pub const CUT_OFF_REASONING_NOTE: &str = "(Automatic note from Marv, the program running you) Your last reply used up the output limit, while reasoning or while writing a tool call (an unfinished call is dropped), so it did nothing. Reason briefly, and write a big file in parts (write_file the first part, then add the rest with append: true).";

/// Told to the model after a second empty reply in a row. The first is asked for again unchanged:
/// upstream hosts sometimes bill for a reply and send nothing, and a resample usually comes back
/// whole. A second suggests the model itself stopped.
pub const EMPTY_REPLY_NOTE: &str = "(Automatic note from Marv, the program running you) Your last reply was empty: no text and no tool calls. If the task isn't finished, continue it with your tools; if it is, say what you did.";

/// Told to the model when its reply got stuck repeating one sentence. The reply is dropped entirely.
pub const TEXT_LOOP_NOTE: &str = "(Automatic note from Marv, the program running you) Your last reply got stuck repeating one sentence and was stopped; nothing in it was used. Take the next concrete step.";

/// The result of each repeated call when tool calls repeat without progress.
pub fn tool_loop_note(repeat_count: usize, _call_name: &str) -> String {
    format!(
        "Marv: you have made this exact call {repeat_count} times in a row and it returned the same result; use what it returned, or try something else."
    )
}

/// Marv #59: the wrap-up request's note at a hard step limit.
pub const STEP_LIMIT_NOTE: &str = "(Automatic note from Marv, the program running you) You've reached the step limit. Don't call tools. Reply with what you finished, what's left, and the next step.";

/// Marv #64: sent after a partial reply kept from a dropped stream. The provider owns the continue
/// request (it owns retries), so the text lives there; re-exported for the loop's tests and docs.
pub use marv_provider::openai_compat::DROPPED_STREAM_NOTE;

/// A copy of the history in which every tool call has a result: calls still waiting at the end
/// (a run cut short, e.g. by quitting while it wound down) get "not run". Without one the API
/// rejects every request made from this history, so a session saved this way couldn't continue.
pub fn answer_all_calls(history: &[ChatTurn]) -> Vec<ChatTurn> {
    let answered: HashSet<&str> = history
        .iter()
        .filter_map(|t| match t {
            ChatTurn::Tool { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    let waiting: Vec<ChatTurn> = history
        .iter()
        .rev()
        .find_map(|t| match t {
            ChatTurn::Assistant { tool_calls, .. } if !tool_calls.is_empty() => Some(tool_calls),
            _ => None,
        })
        .into_iter()
        .flatten()
        .filter(|c| !answered.contains(c.id.as_str()))
        .map(|c| ChatTurn::Tool { call_id: c.id.clone(), name: c.name.clone(), text: not_run::ABORTED.into() })
        .collect();
    let mut out = history.to_vec();
    out.extend(waiting);
    out
}

/// A subagent's label ("explore · Find the config"): used before the call runs and by the agent
/// tool's own label after, so the transcript entry doesn't change when the call starts.
pub fn agent_label(kind: Option<&str>, description: &str) -> String {
    format!("{} · {description}", kind.unwrap_or("general-purpose"))
}

/// What an agent call asked for: its task and the rest, for its trajectory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentArgs {
    pub prompt: String,
    pub kind: String,
    pub description: String,
    pub isolation: Option<String>,
}

/// Reads an `agent` call's arguments. The model can send any JSON ("null", "[]", "3"): only an
/// object has fields; the tool itself reports the bad input.
pub fn agent_args(args: &str) -> AgentArgs {
    let parsed = serde_json::from_str::<serde_json::Value>(args).ok().filter(|v| v.is_object());
    let text = |key: &str| parsed.as_ref().and_then(|v| v.get(key)).and_then(|v| v.as_str()).map(str::to_string);
    AgentArgs {
        prompt: text("prompt").unwrap_or_default(),
        kind: text("type").unwrap_or_else(|| "general-purpose".into()),
        description: text("description").unwrap_or_default(),
        isolation: text("isolation"),
    }
}

/// The transcript label for a call before it runs ("src/app.ts"): `path`, `pattern`, `command`,
/// an agent label, or the raw arguments.
pub fn label_of(call: &ToolCall) -> String {
    let Ok(serde_json::Value::Object(args)) = serde_json::from_str::<serde_json::Value>(&call.arguments) else {
        return call.arguments.clone();
    };
    let shown = |v: &serde_json::Value| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
    for key in ["path", "pattern", "command"] {
        if let Some(v) = args.get(key).filter(|v| !v.is_null()) {
            return shown(v);
        }
    }
    if let Some(description) = args.get("description").and_then(|d| d.as_str()) {
        return agent_label(args.get("type").and_then(|t| t.as_str()), description);
    }
    call.arguments.clone()
}

/// Ids already in the conversation (a resumed session's too): a new call may not reuse one.
pub fn used_ids(history: &[ChatTurn]) -> HashSet<String> {
    history
        .iter()
        .flat_map(|t| match t {
            ChatTurn::Assistant { tool_calls, .. } => tool_calls.iter().map(|c| c.id.clone()).collect(),
            _ => Vec::new(),
        })
        .collect()
}

/// Gives every call an id of its own: results are matched to calls by id, by the API and by the
/// UI. An empty id or one already used gets `<id or "call">_<n>`, the smallest free n.
pub fn make_ids_unique(calls: &mut [ToolCall], used: &mut HashSet<String>) {
    for call in calls {
        if !call.id.is_empty() && !used.contains(&call.id) {
            used.insert(call.id.clone());
            continue;
        }
        let base = if call.id.is_empty() { "call".to_string() } else { call.id.clone() };
        let mut n = 1;
        while used.contains(&format!("{base}_{n}")) {
            n += 1;
        }
        call.id = format!("{base}_{n}");
        used.insert(call.id.clone());
    }
}

/// Splits a reply's calls into groups that run together: a run of consecutive concurrent calls
/// is one group; anything else is a group of its own (a barrier: calls before it finish first,
/// calls after it wait). Marv #58.
pub fn call_groups(calls: &[ToolCall], concurrent: impl Fn(&ToolCall) -> bool) -> Vec<Range<usize>> {
    let mut groups = Vec::new();
    let mut i = 0;
    while i < calls.len() {
        let mut end = i + 1;
        if concurrent(&calls[i]) {
            while end < calls.len() && concurrent(&calls[end]) {
                end += 1;
            }
        }
        groups.push(i..end);
        i = end;
    }
    groups
}

/// How a call that needed approval got it, as trajectories and `marv -p` record it. The loop never reads it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalRecord {
    /// It needed none.
    #[default]
    None,
    /// Yolo ran it.
    Auto,
    Interrupted,
    Yes,
    Always,
    No,
    /// Declined with a reason (#185): not run, and the turn went on. The reason is in the result, never here.
    Declined,
    Unavailable,
}

/// A finished tool call, as the loop needs it.
#[derive(Debug, Clone)]
pub struct ToolRun {
    /// `result.output` is what the model reads; `summary`/`is_error` are for the transcript.
    pub result: ToolResult,
    /// The transcript label ("src/app.ts").
    pub label: String,
    /// The user said no: the run stops after this step so they can say what to do instead.
    pub declined: bool,
    /// Where `output` came from: anything but `User`/`Marv` is fenced as data, and a tainting
    /// source taints the session (`docs/SECURITY.md`).
    pub provenance: Provenance,
    /// The file a change wrote (project-relative): which project checks/diagnostics `after_step` runs.
    pub changed: Option<String>,
    /// How it was approved (trajectories, `marv -p`'s `tool_end` and refusals). Never sent to the model.
    pub approval: ApprovalRecord,
    /// The door out of the sandbox the call went through (Marv #47, #51, #52), for trajectories; None for an
    /// ordinary call.
    pub door: Option<marv_core::Door>,
    /// Display-only payload the loop never reads (marv-tools' `DiffShown`); the UI downcasts it.
    /// Never sent to the model.
    pub display: Option<Arc<dyn std::any::Any + Send + Sync>>,
}

impl ToolRun {
    /// A plain result with no extras.
    pub fn new(result: ToolResult, label: impl Into<String>) -> Self {
        Self {
            result,
            label: label.into(),
            declined: false,
            provenance: Provenance::Project,
            changed: None,
            approval: ApprovalRecord::None,
            door: None,
            display: None,
        }
    }
}

/// How the loop runs tools. Implemented by `marv-session` (wrapping marv-tools' `run_tool` with
/// approvals, yolo, taint and repair); tests use fakes.
#[async_trait]
pub trait ToolRunner: Send + Sync {
    /// How much the call can do (decided before it runs, from its name and arguments).
    fn kind(&self, call: &ToolCall) -> ToolKind;

    /// Whether the call may run at the same time as its concurrent neighbours (Marv #58).
    /// Default: reads only. The session also says yes for `agent` (subagents), as Marv did.
    fn concurrent(&self, call: &ToolCall) -> bool {
        self.kind(call) == ToolKind::Read
    }

    /// Runs one call. Approval happens inside (so a call's approval is asked before the next call
    /// of a sequential group is considered). `Err` becomes an error result
    /// `"<name> failed: <message>"` and the run goes on (Marv: a tool that throws or rejects).
    async fn run(&self, call: ToolCall, cancel: CancellationToken) -> Result<ToolRun, MarvError>;
}

/// What happens after an empty reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmptyNext {
    /// Asked for again unchanged (the first time).
    Retry,
    /// Asked again with [`EMPTY_REPLY_NOTE`] (the second).
    Nudge,
    /// The run ends as if it had answered (the third).
    Stop,
}

impl EmptyNext {
    /// For the `n`th empty reply in a row (0-based).
    pub fn for_count(n: u32) -> Self {
        match n {
            0 => Self::Retry,
            1 => Self::Nudge,
            _ => Self::Stop,
        }
    }
}

#[derive(Debug, Clone)]
pub enum LoopEvent {
    TextDelta(String),
    ThinkingDelta(String),
    /// One request's usage, with how it ended (#90): the vendor's finish reason always, the
    /// raw response when debug recording was on (`StreamOptions::debug_raw`).
    Usage(Usage, marv_core::ReplyEnd),
    /// One step's reply text is complete.
    Assistant {
        text: String,
    },
    /// A piece of a tool call still streaming, for display only: nothing runs yet.
    ToolCallDelta {
        index: usize,
        name: String,
        text: String,
    },
    ToolStart {
        call: ToolCall,
        label: String,
    },
    ToolEnd {
        call: ToolCall,
        run: ToolRun,
    },
    /// A call answered without running (after a no, an interrupt, a cut-off or a loop): no ToolStart came first.
    ToolSkipped {
        call: ToolCall,
        output: String,
    },
    Error(String),
    /// It reached a multiple of `max_steps` and asked `on_limit` whether to keep going.
    StepLimit {
        steps: u32,
        continued: bool,
    },
    EmptyReply {
        next: EmptyNext,
    },
    /// A reply hit the output limit: nothing in it ran. `continued`: the model was told and goes on.
    CutOff {
        continued: bool,
    },
    /// The reply was stuck repeating one sentence and was dropped. `continued`: the model was told.
    TextLoop {
        continued: bool,
    },
    /// #90 (marv-rs): the reply announced its next step but made no tool call, so the run had
    /// stopped halfway. `continued`: Marv said "continue" (once per turn); a second one ends
    /// the run as before, still logged so the bench can count the turns that announced.
    AnnouncedStep {
        continued: bool,
    },
    /// The same tool calls repeated without progress: the model was told (once), or the run stopped.
    ToolLoop {
        continued: bool,
    },
    /// A mid-run compaction swapped the history for the summary (`on_full`).
    Compacted,
    /// Marv #63: the request didn't fit the context window, and `on_overflow` is making it fit. `attempt` counts from 0
    /// within the step.
    Overflow {
        attempt: u32,
    },
    Done(StopReason),
    /// The provider threw the reply away without retrying (#96: it degenerated again); an `Error` follows.
    Discarded {
        cause: marv_core::RetryCause,
    },
    /// The connection was lost and the provider is retrying. `continued` (#64): the partial reply
    /// was kept and the model asked to continue, instead of starting over. `cause`: why (#88, #96).
    Retry {
        attempt: u32,
        max: u32,
        message: String,
        continued: bool,
        cause: marv_core::RetryCause,
    },
}

/// `on_limit`'s answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitAnswer {
    /// Yes / always: another `max_steps`.
    Continue,
    /// No: stops "as you asked", with a hint to say "continue".
    Stop,
    /// No one could answer (`marv -p --approve`'s approver): stops like a hard limit.
    Unavailable,
}

/// The message when the run stops at the step limit. `asked`: `on_limit` was there and answered no.
pub fn step_limit_message(steps: u32, asked: bool) -> String {
    if asked {
        format!("Stopped at {steps} steps, as you asked. Say \"continue\" to pick up where it left off.")
    } else {
        format!("Stopped after {steps} steps without a final answer. Say \"continue\" to pick up where it left off.")
    }
}

/// Whether step `step` (0-based, before its request) is at the limit. `max_steps` 0 never is.
pub fn at_step_limit(step: u32, max_steps: u32) -> bool {
    max_steps > 0 && step > 0 && step.is_multiple_of(max_steps)
}

/// The conversation, shared between the loop and the session: the loop appends; the session's
/// `on_full` swaps it for the compacted history in place. The lock is never held across an await.
pub type History = Arc<Mutex<Vec<ChatTurn>>>;
pub type OnLimit = Arc<dyn Fn(u32) -> BoxFuture<'static, LimitAnswer> + Send + Sync>;
/// True when the context was nearly full and the history was just compacted. Never fails.
pub type OnFull = Arc<dyn Fn() -> BoxFuture<'static, bool> + Send + Sync>;
/// Marv #63: what `on_overflow` did about a request that didn't fit the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverflowOutcome {
    /// The history was made to fit: retry the step.
    Recovered,
    /// Recovery gave up. The message is what the user should read: the session's own explanation, or the provider
    /// error that stopped it.
    Failed(String),
    /// Esc during recovery.
    Stopped,
}
/// Marv #63: called with the provider's error and the attempt (from 0, per step) when a request overflowed the window.
pub type OnOverflow = Arc<dyn Fn(String, u32) -> BoxFuture<'static, OverflowOutcome> + Send + Sync>;
/// Marv #63: recoveries a step may have (overflow levels 0–3), so at most 5 requests a step.
pub const MAX_OVERFLOW_RECOVERIES: u32 = 4;
/// A note for the model after a step (the typecheck), or `None`. Never fails.
pub type AfterStep = Arc<dyn Fn(Vec<ToolRun>) -> BoxFuture<'static, Option<String>> + Send + Sync>;
/// A call to record and run instead, or `None` to keep it.
pub type RewriteCall = Arc<dyn Fn(&ToolCall) -> Option<ToolCall> + Send + Sync>;

/// Marv's `AgentOptions`.
#[derive(Clone)]
pub struct AgentOptions {
    pub provider: Arc<dyn Provider>,
    /// Appended to as the run goes.
    pub history: History,
    pub system: String,
    pub tools: Vec<ToolSpec>,
    pub runner: Arc<dyn ToolRunner>,
    /// ctrl+c / Esc.
    pub cancel: CancellationToken,
    /// Steps before the limit is checked; 0 turns the limit off (no asking, no hard stop).
    pub max_steps: u32,
    /// At most this many concurrent calls at once.
    pub max_parallel: usize,
    /// Asked at each multiple of `max_steps`, after that step's results are in. `None`: a hard stop.
    pub on_limit: Option<OnLimit>,
    pub rewrite_call: Option<RewriteCall>,
    /// Checked after a step's calls are all answered, before `after_step`.
    pub on_full: Option<OnFull>,
    /// Marv #63: a request that overflowed the window; `None` ends the run with the provider's error, as before.
    pub on_overflow: Option<OnOverflow>,
    pub after_step: Option<AfterStep>,
    /// #59: one tools-off wrap-up reply before a hard stop at the step limit (no `on_limit`, or it answered
    /// `Unavailable`). The session sets it for the main agent and subagents.
    pub wrap_up_at_limit: bool,
    /// #90: providers collect the raw response payloads (the trajectory's `request` record
    /// carries them). The session sets this from `MARV_DEBUG_RAW`.
    pub debug_raw: bool,
}

impl AgentOptions {
    /// Options with Marv's defaults: 25 steps, 4 in parallel, no hooks.
    pub fn new(
        provider: Arc<dyn Provider>,
        history: History,
        system: String,
        tools: Vec<ToolSpec>,
        runner: Arc<dyn ToolRunner>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            provider,
            history,
            system,
            tools,
            runner,
            cancel,
            max_steps: DEFAULT_MAX_STEPS,
            max_parallel: MAX_PARALLEL,
            on_limit: None,
            rewrite_call: None,
            on_full: None,
            on_overflow: None,
            after_step: None,
            wrap_up_at_limit: false,
            debug_raw: false,
        }
    }
}

/// One step's reply, folded from the provider's events.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StepReply {
    pub text: String,
    /// The reply reasoned (thinking models). Only to pick the cut-off note.
    pub reasoned: bool,
    pub calls: Vec<ToolCall>,
    pub error: Option<String>,
    pub stop_reason: Option<String>,
    /// #57: kept on the assistant turn.
    pub reasoning: Option<Reasoning>,
    /// Stopped while streaming: stuck repeating itself (the rest isn't read).
    pub looped: bool,
    checked_at: usize,
}

impl StepReply {
    /// Folds one provider event in; returns what to pass on to the consumer, if anything. After
    /// it sets [`StepReply::looped`] the caller stops reading the stream (the delta that tripped
    /// it isn't passed on).
    pub fn apply(&mut self, event: ProviderEvent) -> Option<LoopEvent> {
        match event {
            ProviderEvent::TextDelta(text) => {
                self.text.push_str(&text);
                if self.text.len() - self.checked_at >= TEXT_CHECK_EVERY {
                    self.checked_at = self.text.len();
                    if detect_text_repetition(&self.text) {
                        self.looped = true;
                        return None;
                    }
                }
                Some(LoopEvent::TextDelta(text))
            }
            ProviderEvent::ThinkingDelta(text) => {
                self.reasoned = true;
                Some(LoopEvent::ThinkingDelta(text))
            }
            ProviderEvent::Usage(usage, end) => Some(LoopEvent::Usage(usage, end)),
            ProviderEvent::ToolCallDelta { index, name, text } => Some(LoopEvent::ToolCallDelta { index, name, text }),
            ProviderEvent::ToolCall(call) => {
                self.calls.push(call);
                None
            }
            ProviderEvent::Reasoning(reasoning) => {
                self.reasoning = Some(reasoning);
                None
            }
            ProviderEvent::Done { reason } => {
                self.stop_reason = reason;
                None
            }
            ProviderEvent::Error(message) => {
                self.error = Some(message);
                None
            }
            ProviderEvent::Retry { attempt, max, message, continued, cause } => {
                // Retrying from scratch: discard this attempt's partial text, calls and reasoning so
                // the retry doesn't duplicate them. A continued retry (#64) keeps the partial text:
                // the next attempt appends to it.
                if !continued {
                    self.discard();
                }
                Some(LoopEvent::Retry { attempt, max, message, continued, cause })
            }
            // #96: the same, with no attempt after it (an error follows): nothing of it reaches the history.
            ProviderEvent::Discard { cause } => {
                self.discard();
                Some(LoopEvent::Discarded { cause })
            }
        }
    }

    /// Forgets this attempt's text, calls and reasoning.
    fn discard(&mut self) {
        self.text.clear();
        self.checked_at = 0;
        self.reasoned = false;
        self.reasoning = None;
        self.calls.clear();
    }

    pub fn cut_off(&self) -> bool {
        self.stop_reason.as_deref() == Some("length")
    }

    /// What kind of step this was, in the order the loop checks (the tool-loop check, which needs
    /// state, comes between `CutOff` and `Tools`/`Answer`).
    pub fn outcome(&self, aborted: bool) -> StepOutcome {
        if !aborted && (self.looped || (!self.text.is_empty() && detect_text_repetition(&self.text))) {
            return StepOutcome::TextLoop;
        }
        if self.text.trim().is_empty() && self.calls.is_empty() && self.error.is_none() && !self.cut_off() && !aborted {
            return StepOutcome::Empty;
        }
        if let Some(error) = &self.error {
            return StepOutcome::Error(error.clone());
        }
        if self.cut_off() && !aborted {
            return StepOutcome::CutOff;
        }
        if self.calls.is_empty() { StepOutcome::Answer } else { StepOutcome::Tools }
    }

    /// The note for a cut-off reply with nothing to run: its reasoning isn't in the history, so a
    /// reply that was all reasoning is told that, not to split a file it never started.
    pub fn cut_off_note(&self) -> &'static str {
        if self.reasoned && self.text.is_empty() { CUT_OFF_REASONING_NOTE } else { CUT_OFF_NOTE }
    }

    /// The assistant turn for the history, when there is anything to record.
    pub fn assistant_turn(&self) -> Option<ChatTurn> {
        (!self.text.is_empty() || !self.calls.is_empty()).then(|| ChatTurn::Assistant {
            text: self.text.clone(),
            tool_calls: self.calls.clone(),
            reasoning: self.reasoning.clone(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepOutcome {
    TextLoop,
    Empty,
    /// The assistant turn (if any) goes in first, then `Error` and `Done(Error)`.
    Error(String),
    CutOff,
    Tools,
    Answer,
}

/// A hash of a call's result, so a repeated call with the same result is a loop and one whose
/// result changes (the next item from a queue) is new work. Marv: `Bun.hash`.
pub fn result_hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::hash::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// The calls of the current step and how many have their result in the history (results go in
/// call order, so the rest are a suffix).
#[derive(Debug, Default)]
struct InFlight {
    calls: Vec<ToolCall>,
    answered: usize,
}

/// What a running group has ready for the consumer (Marv's `GroupEvent`), by index in the group.
#[derive(Debug, Clone, Copy)]
enum GroupEvent {
    /// A reserved slot: its `ToolStart` is due (or, after a cancel or a "no", its "not run").
    Start(usize),
    End(usize),
    Skip(usize),
}

type Running = FuturesUnordered<BoxFuture<'static, (usize, Result<ToolRun, MarvError>)>>;

/// One group from [`call_groups`] while it runs (Marv's `runGroup`). Calls finish whenever they
/// like; their events wait in `ready` until the consumer gets to them.
struct Group {
    range: Range<usize>,
    results: Vec<Option<ToolRun>>,
    /// The next call to reserve a slot for.
    next: usize,
    active: usize,
    settled: usize,
    ready: VecDeque<GroupEvent>,
    /// Its `ToolStart` was queued: it launches on the next pull, once the consumer has taken it.
    announced: Option<usize>,
    running: Running,
}

impl Group {
    fn new(range: Range<usize>) -> Self {
        Self {
            results: vec![None; range.len()],
            range,
            next: 0,
            active: 0,
            settled: 0,
            ready: VecDeque::new(),
            announced: None,
            running: FuturesUnordered::new(),
        }
    }

    /// Reserves free slots for queued calls (or answers them, after a cancel or a "no"). Runs nothing.
    fn start_more(&mut self, limit: usize, calls: &[ToolCall], stopped: Option<&'static str>) {
        while self.active < limit && self.next < self.range.len() {
            let k = self.next;
            self.next += 1;
            if let Some(output) = stopped {
                self.settled += 1;
                self.results[k] = Some(not_run_record(&calls[self.range.start + k], output));
                self.ready.push_back(GroupEvent::Skip(k));
                continue;
            }
            self.active += 1;
            self.ready.push_back(GroupEvent::Start(k));
        }
    }

    fn done(&self) -> bool {
        self.settled == self.range.len() && self.ready.is_empty() && self.announced.is_none()
    }
}

/// A step's calls while they run, group by group.
struct Tools {
    groups: Vec<Range<usize>>,
    next_group: usize,
    group: Option<Group>,
    /// A call was declined: nothing new starts, and the run stops after this step.
    declined: bool,
    /// Every result of this step, in call order, for `after_step`.
    runs: Vec<ToolRun>,
}

/// The record of a call answered without running, for `after_step` and the events.
fn not_run_record(call: &ToolCall, output: &str) -> ToolRun {
    ToolRun::new(ToolResult { output: output.to_string(), summary: Some("not run".into()), is_error: false }, call.name.clone())
}

/// Where the loop is between pulls.
enum Phase {
    /// The top of a step: the step limit, if this step is at one.
    Top,
    /// Send the request. `wrap_up` (#59): the message to stop with after this tools-off reply.
    Request {
        tool_choice: ToolChoice,
        wrap_up: Option<String>,
    },
    Streaming {
        stream: EventStream,
        reply: StepReply,
        wrap_up: Option<String>,
    },
    Tools(Tools),
    /// The step's calls are all answered: `on_full`.
    Full(Vec<ToolRun>),
    /// Marv #63: the request overflowed the window: `on_overflow`, with the provider's error (and the #59 wrap-up's
    /// message when it was the wrap-up request).
    Overflow(String, Option<String>),
    AfterStep(Vec<ToolRun>),
}

/// The loop's state between pulls. A resumable state machine instead of Marv's generator:
/// - `Top`/`Request`/`Streaming`: the step limit, then stream one reply into a [`StepReply`]
///   (forwarding events through `queue`), then decide its [`StepOutcome`];
/// - `Tools`: for each group from [`call_groups`], reserve up to `max_parallel` calls, push their
///   `ToolStart`s, and only launch a call once its `ToolStart` was pulled; collect `ToolEnd`s as
///   they finish (a `FuturesUnordered`), append results in call order when the group is done;
/// - `Full`/`AfterStep`: `on_full`, then `after_step`, then `Top` again.
///
/// Each `advance` does one transition; [`LoopState::next_event`] calls it until an
/// event is queued, so nothing after an event happens before the consumer asks for the next one.
pub struct LoopState {
    options: AgentOptions,
    queue: VecDeque<LoopEvent>,
    finished: bool,
    phase: Phase,
    step: u32,
    used_ids: HashSet<String>,
    /// Replies cut off at the output limit in a row.
    cut_offs: u32,
    /// Empty replies in a row.
    empties: u32,
    /// Text loops in a row.
    text_loops: u32,
    /// #90: whether this run already continued an announced next step (once per turn: the
    /// first is rescued, a second announcement ends the run, still logged as an event).
    announced: bool,
    tool_loop: ToolLoopState,
    /// (tool, normalized args) → hash of what it returned last time. A call repeated with the same
    /// result is a loop; one whose result changes each time (the next item from a queue) is new work.
    last_result: std::collections::HashMap<(String, String), u64>,
    /// Bash commands failing the same way since the last edit (#81): another try is a loop, whatever its tail.
    /// Checked as each call is about to run, after the calls before it in the reply were recorded.
    failures: FailureLoopState,
    /// A failing command was tried again after the warning: the step's remaining calls don't run, the run ends `Loop`.
    failure_stop: bool,
    in_flight: InFlight,
    /// Marv #63: overflow recoveries in this step (`on_overflow`'s `attempt`); reset by a step that got a reply.
    overflows: u32,
}

impl LoopState {
    pub fn new(options: AgentOptions) -> Self {
        let used_ids = used_ids(&options.history.lock().unwrap_or_else(|e| e.into_inner()));
        Self {
            options,
            queue: VecDeque::new(),
            finished: false,
            phase: Phase::Top,
            step: 0,
            used_ids,
            cut_offs: 0,
            empties: 0,
            text_loops: 0,
            announced: false,
            tool_loop: ToolLoopState::default(),
            last_result: Default::default(),
            failures: FailureLoopState::default(),
            failure_stop: false,
            in_flight: InFlight::default(),
            overflows: 0,
        }
    }

    fn push_turn(&self, turn: ChatTurn) {
        self.options.history.lock().unwrap_or_else(|e| e.into_inner()).push(turn);
    }

    /// Marv's notes go in as user turns, merged into a trailing one (a mid-turn compaction's note, the turn's own
    /// message): never two user turns in a row (#63 review).
    fn push_note(&self, text: &str) {
        crate::compact::push_user(&mut self.options.history.lock().unwrap_or_else(|e| e.into_inner()), text);
    }

    fn push_result(&self, call: &ToolCall, text: &str) {
        self.push_turn(ChatTurn::Tool { call_id: call.id.clone(), name: call.name.clone(), text: text.to_string() });
    }

    fn aborted(&self) -> bool {
        self.options.cancel.is_cancelled()
    }

    fn emit(&mut self, event: LoopEvent) {
        self.queue.push_back(event);
    }

    fn finish(&mut self, reason: StopReason) {
        self.emit(LoopEvent::Done(reason));
        self.finished = true;
    }

    /// On to the next step (every `continue` of Marv's `for (step…)`).
    fn next_step(&mut self) {
        self.step += 1;
        self.phase = Phase::Top;
    }

    fn call_key(call: &ToolCall) -> (String, String) {
        let key = make_tool_key(&call.name, &call.arguments);
        (key.name, key.args)
    }

    /// Appends a call's result (in call order) and remembers it for tool-loop detection.
    fn answer(&mut self, call: &ToolCall, text: &str) {
        self.push_result(call, text);
        self.last_result.insert(Self::call_key(call), result_hash(text));
        self.failures.record(&call.name, &call.arguments, text);
        self.in_flight.answered += 1;
    }

    /// The next event, or `None` when the run is over.
    pub async fn next_event(&mut self) -> Option<LoopEvent> {
        loop {
            if let Some(event) = self.queue.pop_front() {
                return Some(event);
            }
            if self.finished {
                return None;
            }
            self.advance().await;
        }
    }

    /// One transition of the state machine.
    async fn advance(&mut self) {
        match std::mem::replace(&mut self.phase, Phase::Top) {
            Phase::Top => self.top().await,
            Phase::Request { tool_choice, wrap_up } => {
                // The lock is never held across an await: the adapter serializes the snapshot at once.
                let snapshot = self.options.history.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let options = StreamOptions {
                    system: Some(self.options.system.clone()),
                    tools: self.options.tools.clone(),
                    tool_choice,
                    cancel: self.options.cancel.clone(),
                    debug_raw: self.options.debug_raw,
                };
                let stream = self.options.provider.stream(&snapshot, options);
                self.phase = Phase::Streaming { stream, reply: StepReply::default(), wrap_up };
            }
            Phase::Streaming { mut stream, mut reply, wrap_up } => match stream.next().await {
                Some(event) => {
                    if let Some(out) = reply.apply(event) {
                        self.emit(out);
                    }
                    // A reply stuck repeating itself is stopped while it streams, not after it ran its course.
                    if reply.looped {
                        self.decide(reply, wrap_up);
                    } else {
                        self.phase = Phase::Streaming { stream, reply, wrap_up };
                    }
                }
                None => self.decide(reply, wrap_up),
            },
            Phase::Tools(mut tools) => {
                if !self.tools(&mut tools).await {
                    self.phase = Phase::Tools(tools);
                }
            }
            Phase::Full(runs) => {
                // A long turn can fill the context by itself: compaction can't wait for the next user
                // message. If it compacted, the history was swapped under this loop for the summary plus
                // a "continue" note (a fresh, valid history), and the next request simply starts from it.
                if let Some(on_full) = self.options.on_full.clone()
                    && on_full().await
                {
                    self.emit(LoopEvent::Compacted);
                    if self.aborted() {
                        return self.finish(StopReason::Aborted);
                    }
                }
                self.phase = Phase::AfterStep(runs);
            }
            Phase::Overflow(message, wrap_up) => self.overflow(message, wrap_up).await,
            Phase::AfterStep(runs) => {
                if let Some(after_step) = self.options.after_step.clone() {
                    let note = after_step(runs).await;
                    if self.aborted() {
                        return self.finish(StopReason::Aborted);
                    }
                    if let Some(note) = note {
                        self.push_note(&note);
                    }
                }
                self.next_step();
            }
        }
    }

    /// The step limit guards against a model that never finishes. With `on_limit`, the user decides
    /// at each multiple whether it goes on. `max_steps` 0 (the guard off) never matches.
    async fn top(&mut self) {
        if !at_step_limit(self.step, self.options.max_steps) {
            self.phase = Phase::Request { tool_choice: ToolChoice::Auto, wrap_up: None };
            return;
        }
        let answer = match self.options.on_limit.clone() {
            Some(on_limit) => Some(on_limit(self.step).await),
            None => None,
        };
        let continued = answer == Some(LimitAnswer::Continue);
        if answer.is_some() {
            self.emit(LoopEvent::StepLimit { steps: self.step, continued });
        }
        if self.aborted() {
            return self.finish(StopReason::Aborted);
        }
        if continued {
            self.phase = Phase::Request { tool_choice: ToolChoice::Auto, wrap_up: None };
            return;
        }
        let asked = answer == Some(LimitAnswer::Stop);
        let message = step_limit_message(self.step, asked);
        // #59: a hard stop first asks for a wrap-up, tools off (but still sent: the cache holds).
        if self.options.wrap_up_at_limit && !asked {
            self.push_note(STEP_LIMIT_NOTE);
            self.phase = Phase::Request { tool_choice: ToolChoice::None, wrap_up: Some(message) };
            return;
        }
        self.emit(LoopEvent::Error(message));
        self.finish(StopReason::MaxSteps);
    }

    /// The reply is in: what kind of step was it.
    fn decide(&mut self, mut reply: StepReply, wrap_up: Option<String>) {
        let aborted = self.aborted();
        // Every call needs an id of its own: results are matched to calls by id, by the API and by
        // the UI. Providers can repeat them (Ollama numbers each step's calls from call_0) or leave
        // them out, so fix them here, before the history (and anyone else) sees them.
        make_ids_unique(&mut reply.calls, &mut self.used_ids);
        if let Some(rewrite) = &self.options.rewrite_call {
            for call in &mut reply.calls {
                if let Some(rewritten) = rewrite(call) {
                    *call = rewritten;
                }
            }
        }
        let outcome = reply.outcome(aborted);
        // Marv #63: a request too long for the window. Checked before anything of the reply enters the history: it has
        // no text or calls, and any partial text is dropped as for a retry. The #59 wrap-up request too: it's sent
        // again, still the wrap-up, once the history fits.
        if let StepOutcome::Error(message) = &outcome
            && self.options.on_overflow.is_some()
            && marv_provider::errors::is_context_overflow(message)
        {
            self.phase = Phase::Overflow(message.clone(), wrap_up);
            return;
        }
        if let Some(message) = wrap_up {
            return self.wrap_up(reply, aborted, message);
        }
        self.overflows = 0;
        match outcome {
            // A reply stuck repeating itself (caught while it streamed, or at its end): it's dropped, so
            // none of it reaches the history (its tool calls never ran and aren't in it either), and the
            // model is told. Twice in a row ends the run.
            StepOutcome::TextLoop => {
                self.text_loops += 1;
                let continued = self.text_loops == 1;
                self.emit(LoopEvent::TextLoop { continued });
                if !continued {
                    return self.finish(StopReason::Loop);
                }
                self.push_note(TEXT_LOOP_NOTE);
                self.next_step();
            }
            // Nothing came back: no text (whitespace doesn't count) and nothing to run. Not after an
            // error, a cut-off or an interrupt. Nothing goes into the history for it, so a retry is the
            // same request (cached).
            StepOutcome::Empty => {
                let next = EmptyNext::for_count(self.empties);
                self.empties += 1;
                self.emit(LoopEvent::EmptyReply { next });
                match next {
                    EmptyNext::Retry => self.next_step(),
                    EmptyNext::Nudge => {
                        self.push_note(EMPTY_REPLY_NOTE);
                        self.next_step();
                    }
                    EmptyNext::Stop => self.finish(StopReason::End),
                }
            }
            outcome => {
                self.empties = 0;
                if let Some(turn) = reply.assistant_turn() {
                    self.push_turn(turn);
                }
                if !reply.text.is_empty() {
                    self.emit(LoopEvent::Assistant { text: reply.text.clone() });
                }
                self.text_loops = 0;
                match outcome {
                    StepOutcome::Error(message) => {
                        self.emit(LoopEvent::Error(message));
                        self.finish(StopReason::Error);
                    }
                    StepOutcome::CutOff => self.cut_off(reply),
                    _ => {
                        self.cut_offs = 0;
                        self.run_calls(reply, aborted);
                    }
                }
            }
        }
    }

    /// Marv #63: the request overflowed the window. The hook makes the history fit (or gives up); the same step is sent
    /// again, at most [`MAX_OVERFLOW_RECOVERIES`] times.
    async fn overflow(&mut self, message: String, wrap_up: Option<String>) {
        let attempt = self.overflows;
        self.emit(LoopEvent::Overflow { attempt });
        let Some(on_overflow) = self.options.on_overflow.clone() else {
            self.emit(LoopEvent::Error(message));
            return self.finish(StopReason::Error);
        };
        let outcome = on_overflow(message.clone(), attempt).await;
        if self.aborted() {
            return self.finish(StopReason::Aborted);
        }
        match outcome {
            OverflowOutcome::Recovered if attempt < MAX_OVERFLOW_RECOVERIES => {
                self.overflows += 1;
                let tool_choice = if wrap_up.is_some() { ToolChoice::None } else { ToolChoice::Auto };
                // The rewritten history may have lost the step limit's note: the wrap-up asks for it again.
                if wrap_up.is_some() {
                    let last = self.options.history.lock().unwrap_or_else(|e| e.into_inner()).last().cloned();
                    if !matches!(last, Some(ChatTurn::User { text }) if text.ends_with(STEP_LIMIT_NOTE)) {
                        self.push_note(STEP_LIMIT_NOTE);
                    }
                }
                self.phase = Phase::Request { tool_choice, wrap_up };
            }
            // A hook that would go on past the cap: the provider's error stands.
            OverflowOutcome::Recovered => {
                self.emit(LoopEvent::Error(message));
                self.finish(StopReason::Error);
            }
            OverflowOutcome::Failed(text) => {
                self.emit(LoopEvent::Error(text));
                self.finish(StopReason::Error);
            }
            OverflowOutcome::Stopped => self.finish(StopReason::Aborted),
        }
    }

    /// Cut off at the output limit: whatever the reply was doing is unfinished, so its calls aren't
    /// run (a write_file whose content stops mid-line would break the file), and the model is told,
    /// so it can redo the work in smaller pieces. Two in a row end the run (a model stuck on a reply
    /// too big for the limit); after a step that finished, it may happen again (writing several big
    /// files in parts).
    fn cut_off(&mut self, reply: StepReply) {
        self.cut_offs += 1;
        let continued = self.cut_offs == 1;
        // The history first, so it's valid even if the consumer stops at one of the events below.
        for call in &reply.calls {
            self.push_result(call, not_run::CUT_OFF);
        }
        // With calls, their results say it; with none, the model is told directly.
        if continued && reply.calls.is_empty() {
            self.push_note(reply.cut_off_note());
        }
        self.emit(LoopEvent::CutOff { continued });
        for call in reply.calls {
            self.emit(LoopEvent::ToolSkipped { call, output: not_run::CUT_OFF.into() });
        }
        if continued { self.next_step() } else { self.finish(StopReason::Length) }
    }

    /// Tool-loop detection, then the calls (or the end of the run, for a plain answer).
    fn run_calls(&mut self, reply: StepReply, aborted: bool) {
        let calls = reply.calls;
        let warned = self.tool_loop.warned;
        let hit = !aborted && !calls.is_empty() && {
            let keys = calls
                .iter()
                .map(|call| {
                    let key = make_tool_key(&call.name, &call.arguments);
                    let last = self.last_result.get(&Self::call_key(call)).map(|h| format!("{h:x}")).unwrap_or_default();
                    ToolCallKey { args: format!("{}\u{0}{last}", key.args), name: key.name }
                })
                .collect();
            tool_loop_detected(&mut self.tool_loop, keys)
        };
        // The first time: every call is answered with the note instead of running.
        if hit && !warned {
            self.tool_loop.warned = true;
            let note = tool_loop_note(self.tool_loop.history.len(), &calls[0].name);
            for call in &calls {
                self.push_result(call, &note);
            }
            self.emit(LoopEvent::ToolLoop { continued: true });
            for call in calls {
                self.emit(LoopEvent::ToolSkipped { call, output: note.clone() });
            }
            return self.next_step();
        }
        // The second time: stop the turn, every call answered (the history must be valid). Marv
        // answers them with the cut-off text; kept as is.
        if hit {
            for call in &calls {
                self.push_result(call, not_run::CUT_OFF);
            }
            self.emit(LoopEvent::ToolLoop { continued: false });
            for call in calls {
                self.emit(LoopEvent::ToolSkipped { call, output: not_run::CUT_OFF.into() });
            }
            return self.finish(StopReason::Loop);
        }
        if calls.is_empty() {
            // #90: a plain answer that announces its next step ("Now checking issue #7 before
            // commenting:") is how a turn dies halfway. Once per turn, say "continue" like a
            // user would; a second announcement just ends the run, still logged.
            if !aborted && announces_next_step(&reply.text) {
                let continued = !self.announced;
                self.announced = true;
                self.emit(LoopEvent::AnnouncedStep { continued });
                if continued {
                    self.push_note(ANNOUNCED_STEP_NOTE);
                    return self.next_step();
                }
            }
            return self.finish(if aborted { StopReason::Aborted } else { StopReason::End });
        }
        let runner = &self.options.runner;
        let groups = call_groups(&calls, |call| runner.concurrent(call));
        // Every call must get a result, even after an interrupt or a "no": a request with an
        // unanswered tool call is rejected by the API. `Drop` covers a consumer that stops.
        self.in_flight = InFlight { calls, answered: 0 };
        self.phase = Phase::Tools(Tools { groups, next_group: 0, group: None, declined: false, runs: Vec::new() });
    }

    /// #59: the wrap-up reply at a hard step limit. Whatever it says goes in; any call it makes
    /// anyway is answered without running. The run still ends `MaxSteps`.
    fn wrap_up(&mut self, reply: StepReply, aborted: bool, message: String) {
        if reply.outcome(aborted) != StepOutcome::TextLoop {
            if let Some(turn) = reply.assistant_turn() {
                self.push_turn(turn);
            }
            for call in &reply.calls {
                self.push_result(call, not_run::STEP_LIMIT);
            }
            if !reply.text.is_empty() {
                self.emit(LoopEvent::Assistant { text: reply.text.clone() });
            }
            for call in reply.calls {
                self.emit(LoopEvent::ToolSkipped { call, output: not_run::STEP_LIMIT.into() });
            }
        }
        if aborted {
            return self.finish(StopReason::Aborted);
        }
        if let Some(error) = reply.error {
            self.emit(LoopEvent::Error(error));
        }
        self.emit(LoopEvent::Error(message));
        self.finish(StopReason::MaxSteps);
    }

    /// One transition of running the step's calls. True when they're all answered and the phase moved on.
    async fn tools(&mut self, tools: &mut Tools) -> bool {
        // The abort first: Esc declines what's waiting *and* aborts, and that's an interrupt, not a "no".
        let stopped = if self.aborted() {
            Some(not_run::ABORTED)
        } else if tools.declined {
            Some(not_run::DECLINED)
        } else if self.failure_stop {
            Some(not_run::CUT_OFF)
        } else {
            None
        };
        let limit = self.options.max_parallel.max(1);
        let Some(group) = tools.group.as_mut() else {
            let Some(range) = tools.groups.get(tools.next_group).cloned() else {
                return self.tools_done(tools);
            };
            tools.next_group += 1;
            if let Some(output) = stopped {
                let calls = self.in_flight.calls[range].to_vec();
                for call in &calls {
                    self.answer(call, output);
                }
                for call in calls {
                    self.emit(LoopEvent::ToolSkipped { call, output: output.into() });
                }
                return false;
            }
            let mut group = Group::new(range);
            group.start_more(limit, &self.in_flight.calls, None);
            tools.group = Some(group);
            return false;
        };
        // Its ToolStart was delivered: now it runs. A consumer that stopped at it (it failed, or the
        // user quit) never sees a tool run that it can't show or stop.
        if let Some(k) = group.announced.take() {
            let call = self.in_flight.calls[group.range.start + k].clone();
            // #81: a command that keeps failing the same way isn't run again: the note the first time, the end of
            // the run the next (the tool-loop guard's `warned`).
            if let Some((count, shown)) = self.failures.repeated(&call.name, &call.arguments) {
                let continued = !self.tool_loop.warned;
                self.tool_loop.warned = true;
                self.failure_stop |= !continued;
                self.emit(LoopEvent::ToolLoop { continued });
                let run = not_run_record(&call, &failing_command_note(count, &shown));
                group.running.push(Box::pin(async move { (k, Ok(run)) }));
            } else {
                let runner = self.options.runner.clone();
                let cancel = self.options.cancel.clone();
                group.running.push(Box::pin(async move { (k, runner.run(call, cancel).await) }));
            }
            // Polled once right away, so whatever the tool does first (asking for approval, say)
            // happens before the next call is considered.
            if let Some(Some((k, result))) = group.running.next().now_or_never() {
                Self::settle(group, k, result, &self.in_flight.calls, &mut tools.declined, limit, stopped);
            }
            return false;
        }
        if group.done() {
            let group = tools.group.take().expect("a group is running");
            // In call order, whatever order they finished in (prompt cache).
            for (k, run) in group.results.into_iter().enumerate() {
                let run = run.expect("every call of a finished group is settled");
                let call = self.in_flight.calls[group.range.start + k].clone();
                self.answer(&call, &run.result.output);
                tools.runs.push(run);
            }
            return false;
        }
        if let Some(event) = group.ready.pop_front() {
            let calls = &self.in_flight.calls;
            let event = match event {
                GroupEvent::Start(k) => match stopped {
                    // Reserved before the interrupt or the "no", but not shown yet: answer it instead
                    // (no ToolStart after either), and let the rest follow.
                    Some(output) => {
                        group.active -= 1;
                        group.settled += 1;
                        let call = calls[group.range.start + k].clone();
                        group.results[k] = Some(not_run_record(&call, output));
                        group.start_more(limit, calls, stopped);
                        LoopEvent::ToolSkipped { call, output: output.into() }
                    }
                    None => {
                        group.announced = Some(k);
                        let call = calls[group.range.start + k].clone();
                        LoopEvent::ToolStart { label: label_of(&call), call }
                    }
                },
                GroupEvent::End(k) => LoopEvent::ToolEnd {
                    call: calls[group.range.start + k].clone(),
                    run: group.results[k].clone().expect("an ended call has its result"),
                },
                GroupEvent::Skip(k) => LoopEvent::ToolSkipped {
                    call: calls[group.range.start + k].clone(),
                    output: group.results[k].as_ref().map(|r| r.result.output.clone()).unwrap_or_default(),
                },
            };
            self.emit(event);
            return false;
        }
        // Nothing to report yet: wait for a running call.
        if let Some((k, result)) = group.running.next().await {
            Self::settle(group, k, result, &self.in_flight.calls, &mut tools.declined, limit, stopped);
        }
        false
    }

    /// A call finished: record it, report it, and give its slot to the next one.
    fn settle(
        group: &mut Group,
        k: usize,
        result: Result<ToolRun, MarvError>,
        calls: &[ToolCall],
        declined: &mut bool,
        limit: usize,
        stopped: Option<&'static str>,
    ) {
        let call = &calls[group.range.start + k];
        // A tool that fails becomes an error result, and the run goes on.
        let run = result.unwrap_or_else(|err| {
            ToolRun::new(
                ToolResult { output: format!("{} failed: {err}", call.name), summary: Some("error".into()), is_error: true },
                call.name.clone(),
            )
        });
        group.active -= 1;
        group.settled += 1;
        *declined |= run.declined;
        group.results[k] = Some(run);
        group.ready.push_back(GroupEvent::End(k));
        // A "no" from this call stops what's queued behind it.
        let stopped = stopped.or_else(|| declined.then_some(not_run::DECLINED));
        group.start_more(limit, calls, stopped);
    }

    /// Every call of the step is answered.
    fn tools_done(&mut self, tools: &mut Tools) -> bool {
        self.in_flight = InFlight::default();
        if self.aborted() {
            self.finish(StopReason::Aborted);
        } else if tools.declined {
            // The user said no: stop here and let them say what to do instead.
            self.finish(StopReason::Declined);
        } else if self.failure_stop {
            self.finish(StopReason::Loop);
        } else {
            self.phase = Phase::Full(std::mem::take(&mut tools.runs));
        }
        true
    }
}

impl Drop for LoopState {
    /// The consumer stopped mid-tools (it dropped the stream): the calls without a result would
    /// leave the history invalid and the API would reject every request from now on.
    fn drop(&mut self) {
        let InFlight { calls, answered } = std::mem::take(&mut self.in_flight);
        for call in calls.into_iter().skip(answered) {
            self.push_turn(ChatTurn::Tool { call_id: call.id, name: call.name, text: not_run::ABORTED.into() });
        }
    }
}

/// Runs the agent loop: one stream of [`LoopEvent`]s per turn, ending after a `Done`. Pull-based:
/// nothing happens (no request, no tool) until the consumer asks for the next event, and dropping
/// the stream answers every call still without a result.
pub fn run_agent(options: AgentOptions) -> impl Stream<Item = LoopEvent> + Send {
    futures::stream::unfold(LoopState::new(options), |mut state| async move {
        let event = state.next_event().await?;
        Some((event, state))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str, path: &str) -> ToolCall {
        ToolCall { id: id.into(), name: "read_file".into(), arguments: format!(r#"{{"path":"{path}"}}"#) }
    }

    // Ported from tests/agent.test.ts.

    #[test]
    fn answer_all_calls_gives_waiting_calls_a_not_run_result() {
        let history = vec![
            ChatTurn::User { text: "go".into() },
            ChatTurn::Assistant { text: "".into(), tool_calls: vec![call("c1", "a.ts"), call("c2", "b.ts")], reasoning: None },
            ChatTurn::Tool { call_id: "c1".into(), name: "read_file".into(), text: "a".into() },
        ];
        let answered = answer_all_calls(&history);
        assert_eq!(answered[..3], history[..]);
        assert!(matches!(&answered[3], ChatTurn::Tool { call_id, text, .. } if call_id == "c2" && text.contains("Interrupted")));
        assert_eq!(answered.len(), 4);
        let done = vec![
            ChatTurn::User { text: "hi".into() },
            ChatTurn::Assistant { text: "hello".into(), tool_calls: vec![], reasoning: None },
        ];
        assert_eq!(answer_all_calls(&done), done);
    }

    #[test]
    fn tool_call_ids_are_made_unique_across_the_conversation() {
        // A resumed session already used call_0_1; Ollama numbers each step's calls from call_0.
        let history = vec![ChatTurn::Assistant { text: "".into(), tool_calls: vec![call("call_0_1", "x")], reasoning: None }];
        let mut used = used_ids(&history);
        let mut step1 = vec![call("call_0", "a.ts"), call("call_0", "b.ts"), call("", "c.ts")];
        make_ids_unique(&mut step1, &mut used);
        let mut step2 = vec![call("call_0", "d.ts")];
        make_ids_unique(&mut step2, &mut used);
        let ids: Vec<_> = step1.iter().chain(&step2).map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["call_0", "call_0_2", "call_1", "call_0_3"]);
    }

    #[test]
    fn labels_before_a_call_runs() {
        assert_eq!(label_of(&call("c", "src/app.ts")), "src/app.ts");
        let agent = ToolCall {
            id: "c".into(),
            name: "agent".into(),
            arguments: r#"{"type":"explore","description":"Find the config"}"#.into(),
        };
        assert_eq!(label_of(&agent), "explore · Find the config");
        let untyped = ToolCall { arguments: r#"{"description":"x"}"#.into(), ..agent.clone() };
        assert_eq!(label_of(&untyped), "general-purpose · x");
        let bad = ToolCall { arguments: "not json".into(), ..agent };
        assert_eq!(label_of(&bad), "not json");
    }

    #[test]
    fn agent_args_reads_only_objects() {
        assert_eq!(
            agent_args(r#"{"prompt":"p","type":"explore","description":"d","isolation":"worktree"}"#),
            AgentArgs { prompt: "p".into(), kind: "explore".into(), description: "d".into(), isolation: Some("worktree".into()) }
        );
        for junk in ["null", "[]", "3", "nope"] {
            assert_eq!(
                agent_args(junk),
                AgentArgs { prompt: "".into(), kind: "general-purpose".into(), description: "".into(), isolation: None }
            );
        }
    }

    #[test]
    fn concurrent_calls_group_and_anything_else_is_a_barrier() {
        let calls: Vec<ToolCall> = ["read_file", "grep", "write_file", "glob", "read_file", "bash"]
            .iter()
            .map(|n| ToolCall { id: "".into(), name: (*n).into(), arguments: "{}".into() })
            .collect();
        let reads = |c: &ToolCall| matches!(c.name.as_str(), "read_file" | "grep" | "glob");
        assert_eq!(call_groups(&calls, reads), vec![0..2, 2..3, 3..5, 5..6]);
    }

    #[test]
    fn step_limit_arithmetic_and_messages() {
        assert!(!at_step_limit(0, 25));
        assert!(at_step_limit(25, 25) && at_step_limit(50, 25));
        assert!(!at_step_limit(25, 0), "0 turns the limit off");
        assert!(step_limit_message(25, true).contains("as you asked"));
        assert!(step_limit_message(25, false).starts_with("Stopped after 25 steps without a final answer."));
    }

    #[test]
    fn empty_replies_retry_then_nudge_then_stop() {
        assert_eq!(
            [0, 1, 2, 3].map(EmptyNext::for_count),
            [EmptyNext::Retry, EmptyNext::Nudge, EmptyNext::Stop, EmptyNext::Stop]
        );
    }

    fn fold(events: Vec<ProviderEvent>) -> (StepReply, Vec<LoopEvent>) {
        let mut reply = StepReply::default();
        let mut out = Vec::new();
        for event in events {
            out.extend(reply.apply(event));
            if reply.looped {
                break;
            }
        }
        (reply, out)
    }

    #[test]
    fn a_discard_resets_the_steps_text_and_calls_before_the_error() {
        // #96: the second degenerate reply.
        let (reply, events) = fold(vec![
            ProviderEvent::ThinkingDelta("\n\n".into()),
            ProviderEvent::TextDelta("Half".into()),
            ProviderEvent::ToolCall(call("c1", "a")),
            ProviderEvent::Discard { cause: marv_core::RetryCause::Degenerate { discarded: 6 } },
            ProviderEvent::Error("degenerated again".into()),
        ]);
        assert_eq!(reply.text, "");
        assert!(reply.calls.is_empty());
        assert!(events.iter().any(|e| matches!(e, LoopEvent::Discarded { .. })));
        assert_eq!(reply.outcome(false), StepOutcome::Error("degenerated again".into()));
    }

    #[test]
    fn a_retry_resets_the_steps_text_and_calls() {
        let (reply, events) = fold(vec![
            ProviderEvent::TextDelta("Half".into()),
            ProviderEvent::ToolCall(call("c1", "a")),
            ProviderEvent::Retry { attempt: 1, max: 2, message: "retrying".into(), continued: false, cause: Default::default() },
            ProviderEvent::TextDelta("Whole".into()),
            ProviderEvent::Done { reason: Some("stop".into()) },
        ]);
        assert_eq!(reply.text, "Whole");
        assert!(reply.calls.is_empty());
        assert!(matches!(events[1], LoopEvent::Retry { attempt: 1, continued: false, .. }));
        assert_eq!(reply.outcome(false), StepOutcome::Answer);
    }

    #[test]
    fn a_text_loop_is_stopped_while_streaming() {
        let deltas = (0..100).map(|_| ProviderEvent::TextDelta("Fix the test scripts now. ".into()));
        let (reply, events) = fold(deltas.chain([ProviderEvent::ToolCall(call("c", "a"))]).collect());
        assert!(reply.looped);
        assert!(reply.calls.is_empty(), "the rest of the stream isn't read");
        assert!(events.len() < 100);
        assert_eq!(reply.outcome(false), StepOutcome::TextLoop);
    }

    #[test]
    fn outcomes_empty_error_cut_off_tools() {
        let empty = fold(vec![
            ProviderEvent::ThinkingDelta("hm".into()),
            ProviderEvent::TextDelta("  ".into()),
            ProviderEvent::Done { reason: None },
        ])
        .0;
        assert_eq!(empty.outcome(false), StepOutcome::Empty);
        assert_ne!(empty.outcome(true), StepOutcome::Empty, "not after an interrupt");
        assert_eq!(empty.assistant_turn(), Some(ChatTurn::Assistant { text: "  ".into(), tool_calls: vec![], reasoning: None }));
        let failed = fold(vec![ProviderEvent::Error("boom".into())]).0;
        assert_eq!(failed.outcome(false), StepOutcome::Error("boom".into()));
        let cut = fold(vec![ProviderEvent::ThinkingDelta("hm".into()), ProviderEvent::Done { reason: Some("length".into()) }]).0;
        assert_eq!(cut.outcome(false), StepOutcome::CutOff);
        assert_eq!(cut.cut_off_note(), CUT_OFF_REASONING_NOTE);
        let tools =
            fold(vec![ProviderEvent::ToolCall(call("c", "a")), ProviderEvent::Done { reason: Some("tool_calls".into()) }]).0;
        assert_eq!(tools.outcome(false), StepOutcome::Tools);
    }

    #[test]
    fn reasoning_is_kept_on_the_assistant_turn() {
        let reasoning = Reasoning(serde_json::json!([{"type": "reasoning.text"}]));
        let (reply, _) = fold(vec![ProviderEvent::TextDelta("ok".into()), ProviderEvent::Reasoning(reasoning.clone())]);
        assert_eq!(
            reply.assistant_turn(),
            Some(ChatTurn::Assistant { text: "ok".into(), tool_calls: vec![], reasoning: Some(reasoning) })
        );
    }

    #[test]
    fn dropping_the_loop_mid_tools_answers_the_rest() {
        struct NoTools;
        #[async_trait]
        impl ToolRunner for NoTools {
            fn kind(&self, _call: &ToolCall) -> ToolKind {
                ToolKind::Read
            }
            async fn run(&self, _call: ToolCall, _cancel: CancellationToken) -> Result<ToolRun, MarvError> {
                unreachable!()
            }
        }
        let history: History = Arc::new(Mutex::new(vec![]));
        let provider = Arc::new(crate::testing::ScriptedProvider::new(vec![]));
        let mut state = LoopState::new(AgentOptions::new(
            provider,
            history.clone(),
            "S".into(),
            vec![],
            Arc::new(NoTools),
            CancellationToken::new(),
        ));
        state.in_flight = InFlight { calls: vec![call("c1", "a"), call("c2", "b")], answered: 1 };
        drop(state);
        let history = history.lock().unwrap();
        assert!(matches!(&history[..], [ChatTurn::Tool { call_id, text, .. }] if call_id == "c2" && text == not_run::ABORTED));
    }

    #[test]
    fn tool_loop_note_counts_the_repeats() {
        assert!(tool_loop_note(4, "read_file").starts_with("Marv: you have made this exact call 4 times in a row"));
    }

    // The loop itself, against the scripted provider and a fake runner (tests/agent.test.ts).

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use crate::testing::ScriptedProvider;
    use futures::StreamExt;

    type RunFn = Arc<dyn Fn(ToolCall, CancellationToken) -> BoxFuture<'static, Result<ToolRun, MarvError>> + Send + Sync>;

    /// A runner that runs `run`; `parallel` says which calls go together (Marv's `isParallel`, default none).
    struct FakeRunner {
        run: RunFn,
        parallel: fn(&ToolCall) -> bool,
    }

    #[async_trait]
    impl ToolRunner for FakeRunner {
        fn kind(&self, call: &ToolCall) -> ToolKind {
            if call.name == "write_file" { ToolKind::Write } else { ToolKind::Read }
        }
        fn concurrent(&self, call: &ToolCall) -> bool {
            (self.parallel)(call)
        }
        async fn run(&self, call: ToolCall, cancel: CancellationToken) -> Result<ToolRun, MarvError> {
            (self.run)(call, cancel).await
        }
    }

    fn specs() -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "read_file".into(),
            description: "Read a file".into(),
            parameters: serde_json::json!({"type": "object"}),
        }]
    }
    fn say(text: &str) -> Vec<ProviderEvent> {
        vec![ProviderEvent::TextDelta(text.into()), ProviderEvent::Done { reason: Some("stop".into()) }]
    }
    fn use_tools(calls: &[ToolCall]) -> Vec<ProviderEvent> {
        let mut events: Vec<_> = calls.iter().cloned().map(ProviderEvent::ToolCall).collect();
        events.push(ProviderEvent::Done { reason: Some("tool_calls".into()) });
        events
    }
    fn cut_off(mut events: Vec<ProviderEvent>) -> Vec<ProviderEvent> {
        events.push(ProviderEvent::Done { reason: Some("length".into()) });
        events
    }
    fn arg(call: &ToolCall, key: &str) -> serde_json::Value {
        serde_json::from_str::<serde_json::Value>(&call.arguments).unwrap()[key].clone()
    }
    fn ok(output: impl Into<String>, label: impl Into<String>) -> ToolRun {
        ToolRun::new(ToolResult { output: output.into(), summary: Some("ok".into()), is_error: false }, label)
    }
    fn fake_tool() -> RunFn {
        Arc::new(|c, _| {
            let path = arg(&c, "path").as_str().unwrap_or_default().to_string();
            Box::pin(async move { Ok(ok(format!("contents of {path}"), path)) })
        })
    }
    fn user(text: &str) -> ChatTurn {
        ChatTurn::User { text: text.into() }
    }
    fn tool(id: &str, text: &str) -> ChatTurn {
        ChatTurn::Tool { call_id: id.into(), name: "read_file".into(), text: text.into() }
    }
    fn said(text: &str) -> ChatTurn {
        ChatTurn::Assistant { text: text.into(), tool_calls: vec![], reasoning: None }
    }
    fn history(turns: Vec<ChatTurn>) -> History {
        Arc::new(Mutex::new(turns))
    }
    fn snapshot(history: &History) -> Vec<ChatTurn> {
        history.lock().unwrap().clone()
    }
    fn tool_texts(history: &History) -> Vec<String> {
        snapshot(history)
            .into_iter()
            .filter_map(|t| match t {
                ChatTurn::Tool { text, .. } => Some(text),
                _ => None,
            })
            .collect()
    }
    fn options(provider: Arc<ScriptedProvider>, history: &History, runner: FakeRunner) -> AgentOptions {
        AgentOptions::new(provider, history.clone(), "SYSTEM".into(), specs(), Arc::new(runner), CancellationToken::new())
    }
    fn plain() -> FakeRunner {
        FakeRunner { run: fake_tool(), parallel: |_| false }
    }
    async fn collect(options: AgentOptions) -> Vec<LoopEvent> {
        run_agent(options).collect().await
    }
    fn end(events: &[LoopEvent]) -> Option<StopReason> {
        match events.last() {
            Some(LoopEvent::Done(reason)) => Some(*reason),
            _ => None,
        }
    }
    fn starts(events: &[LoopEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                LoopEvent::ToolStart { call, .. } => Some(call.id.clone()),
                _ => None,
            })
            .collect()
    }
    fn count(events: &[LoopEvent], pred: impl Fn(&LoopEvent) -> bool) -> usize {
        events.iter().filter(|e| pred(e)).count()
    }

    /// What's wrong with a history's tool results (tests/answered.ts): every assistant tool call must
    /// be followed by exactly one result each, right after it and in call order, and no result may
    /// answer a call that isn't there. Empty: all good.
    fn answer_problems(history: &[ChatTurn]) -> Vec<String> {
        let mut problems = Vec::new();
        let mut i = 0;
        while i < history.len() {
            match &history[i] {
                ChatTurn::Tool { call_id, .. } => {
                    problems.push(format!("result {call_id} at {i} answers no call just before it"))
                }
                ChatTurn::Assistant { tool_calls, .. } if !tool_calls.is_empty() => {
                    let ids: Vec<&str> = tool_calls.iter().map(|c| c.id.as_str()).collect();
                    let mut results = Vec::new();
                    while let Some(ChatTurn::Tool { call_id, .. }) = history.get(i + 1) {
                        results.push(call_id.as_str());
                        i += 1;
                    }
                    if results != ids {
                        problems.push(format!("calls {ids:?} got results {results:?}"));
                    }
                }
                _ => {}
            }
            i += 1;
        }
        problems
    }

    /// A provider that cancels the run as its reply starts, then sends `events`.
    struct CancelsWhileStreaming {
        cancel: CancellationToken,
        events: Vec<ProviderEvent>,
    }

    impl Provider for CancelsWhileStreaming {
        fn name(&self) -> &str {
            "p"
        }
        fn context_length(&self) -> Option<u64> {
            None
        }
        fn stream(&self, _history: &[ChatTurn], _options: StreamOptions) -> marv_provider::EventStream {
            self.cancel.cancel();
            Box::pin(futures::stream::iter(self.events.clone()))
        }
    }

    #[tokio::test]
    async fn a_plain_answer_is_one_step() {
        let provider = Arc::new(ScriptedProvider::new(vec![say("Hi!")]));
        let h = history(vec![user("hello")]);
        let events = collect(options(provider, &h, plain())).await;
        assert_eq!(snapshot(&h), [user("hello"), said("Hi!")]);
        assert!(matches!(&events[..], [LoopEvent::TextDelta(_), LoopEvent::Assistant { .. }, LoopEvent::Done(StopReason::End)]));
    }

    #[tokio::test]
    async fn runs_the_tools_the_model_asks_for_then_sends_the_results_back() {
        let provider =
            Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts"), call("c2", "b.ts")]), say("Both read.")]));
        let h = history(vec![user("read a and b")]);
        let events = collect(options(provider.clone(), &h, plain())).await;
        assert_eq!(
            snapshot(&h),
            [
                user("read a and b"),
                ChatTurn::Assistant {
                    text: "".into(),
                    tool_calls: vec![call("c1", "a.ts"), call("c2", "b.ts")],
                    reasoning: None
                },
                tool("c1", "contents of a.ts"),
                tool("c2", "contents of b.ts"),
                said("Both read."),
            ]
        );
        // The second request carried the tool results.
        assert_eq!(provider.requests()[1].0.last(), Some(&tool("c2", "contents of b.ts")));
        let labels: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                LoopEvent::ToolStart { label, .. } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(labels, ["a.ts", "b.ts"]);
        assert_eq!(count(&events, |e| matches!(e, LoopEvent::ToolEnd { .. })), 2);
    }

    #[tokio::test]
    async fn every_request_extends_the_previous_one_exactly() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            use_tools(&[call("c1", "a.ts")]),
            use_tools(&[call("c2", "b.ts")]),
            say("Done."),
            use_tools(&[call("c3", "c.ts")]),
            say("Again."),
        ]));
        let h = history(vec![user("first")]);
        collect(options(provider.clone(), &h, plain())).await;
        h.lock().unwrap().push(user("second"));
        collect(options(provider.clone(), &h, plain())).await;

        let requests = provider.requests();
        assert_eq!(requests.len(), 5);
        for pair in requests.windows(2) {
            let (before, after) = (&pair[0], &pair[1]);
            assert_eq!(after.0[..before.0.len()], before.0[..], "only appended to");
            assert!(after.0.len() > before.0.len());
            assert_eq!(after.1.system, before.1.system, "same system prompt");
            assert_eq!(after.1.tools, before.1.tools, "the same tool definitions");
        }
    }

    fn endless(n: usize) -> Vec<Vec<ProviderEvent>> {
        (0..n).map(|i| use_tools(&[call(&format!("c{i}"), &format!("file-{i}.ts"))])).collect()
    }

    fn on_limit(f: impl Fn(u32) -> LimitAnswer + Send + Sync + 'static) -> Option<OnLimit> {
        Some(Arc::new(move |steps| {
            let answer = f(steps);
            Box::pin(async move { answer })
        }))
    }

    #[tokio::test]
    async fn stops_after_max_steps_if_the_model_never_finishes() {
        let provider = Arc::new(ScriptedProvider::new(endless(10)));
        let h = history(vec![user("go")]);
        let events = collect(AgentOptions { max_steps: 3, ..options(provider.clone(), &h, plain()) }).await;
        assert_eq!(provider.requests().len(), 3);
        assert!(events.iter().any(|e| matches!(e, LoopEvent::Error(m) if m.contains("3 steps"))));
        assert_eq!(end(&events), Some(StopReason::MaxSteps));
    }

    #[tokio::test]
    async fn at_the_limit_it_asks_yes_buys_another_max_steps_no_stops() {
        let provider = Arc::new(ScriptedProvider::new(endless(10)));
        let h = history(vec![user("go")]);
        let asked = Arc::new(Mutex::new(Vec::new()));
        let seen = asked.clone();
        let events = collect(AgentOptions {
            max_steps: 2,
            on_limit: on_limit(move |steps| {
                seen.lock().unwrap().push(steps);
                if steps < 4 { LimitAnswer::Continue } else { LimitAnswer::Stop }
            }),
            ..options(provider.clone(), &h, plain())
        })
        .await;
        assert_eq!(*asked.lock().unwrap(), [2, 4]);
        assert_eq!(provider.requests().len(), 4);
        let limits: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                LoopEvent::StepLimit { steps, continued } => Some((*steps, *continued)),
                _ => None,
            })
            .collect();
        assert_eq!(limits, [(2, true), (4, false)]);
        assert!(events.iter().any(|e| matches!(e, LoopEvent::Error(m)
            if m == "Stopped at 4 steps, as you asked. Say \"continue\" to pick up where it left off.")));
        assert_eq!(end(&events), Some(StopReason::MaxSteps));
    }

    #[tokio::test]
    async fn it_asks_only_after_a_steps_tool_results_are_in() {
        let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts")]), say("done")]));
        let h = history(vec![user("go")]);
        let seen = h.clone();
        collect(AgentOptions {
            max_steps: 1,
            on_limit: on_limit(move |_| {
                assert!(matches!(seen.lock().unwrap().last(), Some(ChatTurn::Tool { call_id, .. }) if call_id == "c1"));
                LimitAnswer::Continue
            }),
            ..options(provider, &h, plain())
        })
        .await;
        assert_eq!(snapshot(&h).last(), Some(&said("done")));
    }

    #[tokio::test]
    async fn esc_while_it_asks_is_an_interrupt() {
        let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts")]), say("never")]));
        let h = history(vec![user("go")]);
        let opts = options(provider.clone(), &h, plain());
        let cancel = opts.cancel.clone();
        let events = collect(AgentOptions {
            max_steps: 1,
            on_limit: on_limit(move |_| {
                cancel.cancel();
                LimitAnswer::Stop
            }),
            ..opts
        })
        .await;
        assert_eq!(provider.requests().len(), 1);
        assert_eq!(end(&events), Some(StopReason::Aborted));
        assert!(!events.iter().any(|e| matches!(e, LoopEvent::Error(_))));
    }

    #[tokio::test]
    async fn tool_call_ids_are_unique_in_the_history() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            use_tools(&[call("call_0", "a.ts"), call("call_0", "b.ts"), call("", "c.ts")]),
            use_tools(&[call("call_0", "d.ts")]), // Ollama numbers each step's calls from call_0
            say("done"),
        ]));
        let h = history(vec![
            user("earlier"),
            ChatTurn::Assistant { text: "".into(), tool_calls: vec![call("call_0_1", "x")], reasoning: None }, // a resumed session
            tool("call_0_1", "x"),
            user("go"),
        ]);
        let events = collect(options(provider, &h, plain())).await;
        let turns = snapshot(&h);
        let ids: Vec<String> = turns
            .iter()
            .flat_map(|t| match t {
                ChatTurn::Assistant { tool_calls, .. } => tool_calls.iter().map(|c| c.id.clone()).collect(),
                _ => vec![],
            })
            .collect();
        assert_eq!(ids.iter().collect::<HashSet<_>>().len(), ids.len());
        assert!(ids.iter().all(|id| !id.is_empty()));
        // Each result answers its own call, and the events carry the same ids.
        let results: Vec<String> = turns
            .iter()
            .filter_map(|t| match t {
                ChatTurn::Tool { call_id, .. } => Some(call_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(results, ids);
        assert_eq!(starts(&events), ids[1..]);
    }

    #[tokio::test]
    async fn a_consumer_that_stops_mid_tools_still_leaves_every_call_answered() {
        let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts"), call("c2", "b.ts")])]));
        let h = history(vec![user("go")]);
        let mut stream = Box::pin(run_agent(options(provider, &h, plain())));
        while let Some(event) = stream.next().await {
            if matches!(event, LoopEvent::ToolStart { .. }) {
                break; // like the App failing while handling it
            }
        }
        drop(stream);
        let turns = snapshot(&h);
        assert_eq!(turns[turns.len() - 2..], [tool("c1", not_run::ABORTED), tool("c2", not_run::ABORTED)]);
    }

    #[tokio::test]
    async fn calls_answered_without_running_are_reported_too() {
        let provider =
            Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts"), call("c2", "b.ts"), call("c3", "c.ts")])]));
        let h = history(vec![user("go")]);
        let decline_first: RunFn =
            Arc::new(|c, _| Box::pin(async move { Ok(ToolRun { declined: c.id == "c1", ..ok("x", c.id.clone()) }) }));
        let events = collect(options(provider, &h, FakeRunner { run: decline_first, parallel: |_| false })).await;
        let skipped: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                LoopEvent::ToolSkipped { call, output } => Some((call.clone(), output.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(skipped, [(call("c2", "b.ts"), not_run::DECLINED.into()), (call("c3", "c.ts"), not_run::DECLINED.into())]);
    }

    #[tokio::test]
    async fn an_interrupt_mid_tools_still_answers_every_call() {
        let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts"), call("c2", "b.ts")])]));
        let h = history(vec![user("go")]);
        // The user presses ctrl+c while the first tool runs.
        let aborting: RunFn = Arc::new(|c, cancel| {
            cancel.cancel();
            fake_tool()(c, cancel)
        });
        let events = collect(options(provider, &h, FakeRunner { run: aborting, parallel: |_| false })).await;
        assert_eq!(tool_texts(&h), ["contents of a.ts", not_run::ABORTED]);
        assert_eq!(end(&events), Some(StopReason::Aborted));
    }

    #[tokio::test]
    async fn a_declined_action_stops_the_run_and_later_calls_are_answered_too() {
        let provider =
            Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts"), call("c2", "b.ts")]), say("never asked")]));
        let h = history(vec![user("change things")]);
        let declining: RunFn = Arc::new(|c, _| {
            Box::pin(async move { Ok(ToolRun { declined: true, ..ok("The user declined this.", c.id.clone()) }) })
        });
        let events = collect(options(provider.clone(), &h, FakeRunner { run: declining, parallel: |_| false })).await;
        assert_eq!(provider.requests().len(), 1, "no further request: the user decides what's next");
        assert_eq!(tool_texts(&h), ["The user declined this.", not_run::DECLINED]);
        assert_eq!(end(&events), Some(StopReason::Declined));
    }

    #[tokio::test]
    async fn a_provider_error_ends_the_run_and_is_reported() {
        let provider = Arc::new(ScriptedProvider::new(vec![vec![ProviderEvent::Error("Rate limited".into())]]));
        let h = history(vec![user("go")]);
        let events = collect(options(provider, &h, plain())).await;
        assert!(matches!(
            &events[..],
            [LoopEvent::Error(m), LoopEvent::Done(StopReason::Error)] if m == "Rate limited"
        ));
    }

    #[tokio::test]
    async fn tool_call_fragments_are_passed_on_as_they_stream() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                ProviderEvent::ToolCallDelta { index: 0, name: "read_file".into(), text: r#"{"path":"#.into() },
                ProviderEvent::ToolCall(call("c1", "a.ts")),
                ProviderEvent::Done { reason: None },
            ],
            say("Done."),
        ]));
        let h = history(vec![user("go")]);
        let events = collect(options(provider, &h, plain())).await;
        assert!(events.iter().any(|e| matches!(e, LoopEvent::ToolCallDelta { index: 0, name, text }
            if name == "read_file" && text == r#"{"path":"#)));
    }

    #[tokio::test]
    async fn a_retry_resets_this_steps_text_and_calls() {
        let c = call("c1", "a.ts");
        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            ProviderEvent::TextDelta("Hel".into()),
            ProviderEvent::ToolCall(c.clone()),
            ProviderEvent::Retry { attempt: 1, max: 2, message: "lost".into(), continued: false, cause: Default::default() },
            ProviderEvent::TextDelta("Hello".into()),
            ProviderEvent::ToolCall(c),
            ProviderEvent::Done { reason: Some("tool_calls".into()) },
        ]]));
        let h = history(vec![user("go")]);
        let events = collect(options(provider, &h, plain())).await;
        assert!(events.iter().any(|e| matches!(e, LoopEvent::Assistant { text } if text == "Hello")));
        assert_eq!(starts(&events).len(), 1);
        assert!(
            events.iter().any(
                |e| matches!(e, LoopEvent::Retry { attempt: 1, max: 2, message, continued: false, .. } if message == "lost")
            )
        );
    }

    // An empty reply (no text, no tool calls). Upstream hosts sometimes bill for a reply and send back
    // nothing (OpenRouter's Novita and SiliconFlow for qwen3-coder, gpt-oss-120b): the run ended as if
    // the model had finished. A third of the failed eval runs.

    fn empty() -> Vec<ProviderEvent> {
        vec![ProviderEvent::Done { reason: Some("stop".into()) }]
    }
    fn empties(events: &[LoopEvent]) -> Vec<EmptyNext> {
        events
            .iter()
            .filter_map(|e| match e {
                LoopEvent::EmptyReply { next } => Some(*next),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn an_empty_reply_is_asked_for_again_unchanged() {
        let provider = Arc::new(ScriptedProvider::new(vec![empty(), use_tools(&[call("c1", "a.ts")]), say("Done.")]));
        let h = history(vec![user("go")]);
        let events = collect(options(provider.clone(), &h, plain())).await;
        let requests = provider.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[1].0, requests[0].0, "the prompt cache still hits");
        assert_eq!(empties(&events), [EmptyNext::Retry]);
        assert_eq!(end(&events), Some(StopReason::End));
    }

    #[tokio::test]
    async fn empty_again_the_model_is_told_once_a_third_time_the_run_ends() {
        let provider = Arc::new(ScriptedProvider::new(vec![empty(), empty(), empty(), say("never asked")]));
        let h = history(vec![user("go")]);
        let events = collect(options(provider.clone(), &h, plain())).await;
        let requests = provider.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[2].0.last(), Some(&user(&format!("go\n\n{EMPTY_REPLY_NOTE}"))));
        assert_eq!(empties(&events), [EmptyNext::Retry, EmptyNext::Nudge, EmptyNext::Stop]);
        assert_eq!(end(&events), Some(StopReason::End));
    }

    #[tokio::test]
    async fn whitespace_or_reasoning_only_is_empty_too_and_isnt_put_in_the_history() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![ProviderEvent::TextDelta("\n\n".into()), ProviderEvent::Done { reason: Some("stop".into()) }],
            vec![ProviderEvent::ThinkingDelta("Hmm.".into()), ProviderEvent::Done { reason: Some("stop".into()) }],
            say("Done."),
        ]));
        let h = history(vec![user("go")]);
        collect(options(provider, &h, plain())).await;
        assert_eq!(snapshot(&h), [user(&format!("go\n\n{EMPTY_REPLY_NOTE}")), said("Done.")]);
    }

    #[tokio::test]
    async fn each_step_gets_its_own_empty_retries() {
        let provider = Arc::new(ScriptedProvider::new(vec![empty(), use_tools(&[call("c1", "a.ts")]), empty(), say("Done.")]));
        let h = history(vec![user("go")]);
        let events = collect(options(provider, &h, plain())).await;
        assert_eq!(empties(&events), [EmptyNext::Retry, EmptyNext::Retry]);
        assert_eq!(end(&events), Some(StopReason::End));
    }

    #[tokio::test]
    async fn not_empty_after_an_interrupt_or_an_error() {
        let h = history(vec![user("go")]);
        let cancel = CancellationToken::new();
        let provider = Arc::new(CancelsWhileStreaming { cancel: cancel.clone(), events: empty() });
        let events = collect(AgentOptions::new(provider, h.clone(), "S".into(), specs(), Arc::new(plain()), cancel)).await;
        assert!(matches!(&events[..], [LoopEvent::Done(StopReason::Aborted)]));

        let failing =
            Arc::new(ScriptedProvider::new(vec![vec![ProviderEvent::Error("Rate limited".into())], say("never asked")]));
        let events = collect(options(failing.clone(), &history(vec![user("go")]), plain())).await;
        assert!(empties(&events).is_empty());
        assert_eq!(failing.requests().len(), 1);
    }

    // A reply that announces its next step ("Now checking issue #7 before commenting:") but
    // makes no tool call ends the run halfway (#90). The loop says "continue" once per turn,
    // like a user would.

    fn announced(text: &str) -> Vec<ProviderEvent> {
        vec![ProviderEvent::TextDelta(text.into()), ProviderEvent::Done { reason: Some("stop".into()) }]
    }
    fn announced_steps(events: &[LoopEvent]) -> Vec<bool> {
        events
            .iter()
            .filter_map(|e| match e {
                LoopEvent::AnnouncedStep { continued } => Some(*continued),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn an_announced_step_with_no_tool_call_gets_one_continue() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            announced("Both modes work. Now let me look at issue #7 and comment:"),
            use_tools(&[call("c1", "a.ts")]),
            say("Done."),
        ]));
        let h = history(vec![user("go")]);
        let events = collect(options(provider.clone(), &h, plain())).await;
        let requests = provider.requests();
        assert_eq!(requests.len(), 3, "the run did not stop at the announcement");
        assert_eq!(requests[1].0.last(), Some(&user(ANNOUNCED_STEP_NOTE)));
        // The announced reply stays in the history before the note (a valid, ordinary turn).
        assert_eq!(
            snapshot(&h)[1..3],
            [said("Both modes work. Now let me look at issue #7 and comment:"), user(ANNOUNCED_STEP_NOTE)]
        );
        assert_eq!(announced_steps(&events), [true]);
        assert_eq!(end(&events), Some(StopReason::End));
    }

    #[tokio::test]
    async fn a_second_announced_step_after_the_rescue_only_ends_the_run() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            announced("Now checking issue #7 before commenting:"),
            announced("Now also summarizing the diff:"),
            say("never asked"),
        ]));
        let h = history(vec![user("go")]);
        let events = collect(options(provider.clone(), &h, plain())).await;
        assert_eq!(provider.requests().len(), 2, "one continue per turn: the second announcement stops the run");
        assert_eq!(announced_steps(&events), [true, false]);
        assert_eq!(end(&events), Some(StopReason::End));
        let notes = snapshot(&h).iter().filter(|t| matches!(t, ChatTurn::User { text } if text == ANNOUNCED_STEP_NOTE)).count();
        assert_eq!(notes, 1);
    }

    #[tokio::test]
    async fn a_question_or_a_summary_ending_is_never_rescued() {
        // A plain answer (a question to the user, a summary) ends the run as before: no note,
        // no continue, no second request.
        let provider = Arc::new(ScriptedProvider::new(vec![announced("Should I also update the docs?"), say("never asked")]));
        let h = history(vec![user("go")]);
        let events = collect(options(provider.clone(), &h, plain())).await;
        assert_eq!(provider.requests().len(), 1, "the question ended the run as before");
        assert_eq!(announced_steps(&events), Vec::<bool>::new());
        assert!(snapshot(&h).iter().all(|t| !matches!(t, ChatTurn::User { text } if text == ANNOUNCED_STEP_NOTE)));
        assert_eq!(end(&events), Some(StopReason::End));

        let provider = Arc::new(ScriptedProvider::new(vec![announced("Done. Both modes work as expected."), say("never asked")]));
        let events = collect(options(provider, &history(vec![user("go")]), plain())).await;
        assert_eq!(announced_steps(&events), Vec::<bool>::new());
        assert_eq!(end(&events), Some(StopReason::End));
    }

    #[tokio::test]
    async fn an_announced_step_counts_toward_the_step_limit_and_the_wrap_up_is_never_continued() {
        // #90 + #59: the continue is an ordinary step, so it can reach the limit; the wrap-up reply
        // that follows is tools-off and final even when it announces a next step itself.
        let mut script = steps(1);
        script.push(announced("Read a. Now let me fix b:"));
        script.push(announced("Read a; b is left. Next, let me fix b:"));
        script.push(say("never asked"));
        let provider = Arc::new(ScriptedProvider::new(script));
        let h = history(vec![user("go")]);
        let (_state, runner) = tracked(None);
        let events =
            collect(AgentOptions { max_steps: 2, wrap_up_at_limit: true, ..options(provider.clone(), &h, runner) }).await;
        let requests = provider.requests();
        assert_eq!(requests.len(), 3, "the continue used the last step; then only the wrap-up");
        assert!(matches!(requests[2].0.last(), Some(ChatTurn::User { text }) if text.ends_with(STEP_LIMIT_NOTE)));
        assert!(crate::compact::alternates(&requests[2].0));
        assert_eq!(requests[2].1.tool_choice, ToolChoice::None);
        assert_eq!(announced_steps(&events), [true]);
        assert_eq!(end(&events), Some(StopReason::MaxSteps));
    }

    #[tokio::test]
    async fn an_announced_step_with_tool_calls_is_an_ordinary_step() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            // The same reply announced its step and called the tool: the calls run, nothing to rescue.
            vec![
                ProviderEvent::TextDelta("Now checking issue #7:".into()),
                ProviderEvent::ToolCall(call("c1", "a.ts")),
                ProviderEvent::Done { reason: Some("tool_calls".into()) },
            ],
            say("Done."),
        ]));
        let h = history(vec![user("go")]);
        let events = collect(options(provider.clone(), &h, plain())).await;
        assert_eq!(provider.requests().len(), 2);
        assert_eq!(announced_steps(&events), Vec::<bool>::new());
        assert_eq!(end(&events), Some(StopReason::End));
    }

    // #90: debug recording is a plain option on its way to the provider (which collects the
    // raw payloads for the trajectory's request records when it's on).
    #[tokio::test]
    async fn debug_recording_reaches_the_provider() {
        let provider = Arc::new(ScriptedProvider::new(vec![say("Done.")]));
        let h = history(vec![user("go")]);
        let mut flagged = options(provider.clone(), &h, plain());
        flagged.debug_raw = true;
        collect(flagged).await;
        assert!(provider.requests()[0].1.debug_raw);

        let provider = Arc::new(ScriptedProvider::new(vec![say("Done.")]));
        collect(options(provider.clone(), &history(vec![user("go")]), plain())).await;
        assert!(!provider.requests()[0].1.debug_raw, "off by default");
    }

    // A reply cut off at the output limit.

    fn cut_offs(events: &[LoopEvent]) -> Vec<bool> {
        events
            .iter()
            .filter_map(|e| match e {
                LoopEvent::CutOff { continued } => Some(*continued),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn cut_off_calls_arent_run_each_gets_a_result_and_the_run_goes_on() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            cut_off(vec![ProviderEvent::ToolCall(call("c1", "a.ts"))]),
            say("Smaller steps, then."),
        ]));
        let h = history(vec![user("go")]);
        let ran = Arc::new(AtomicUsize::new(0));
        let counted = ran.clone();
        let counting: RunFn = Arc::new(move |c, cancel| {
            counted.fetch_add(1, Ordering::SeqCst);
            fake_tool()(c, cancel)
        });
        let events = collect(options(provider.clone(), &h, FakeRunner { run: counting, parallel: |_| false })).await;
        assert_eq!(ran.load(Ordering::SeqCst), 0);
        assert_eq!(cut_offs(&events), [true]);
        assert!(events.iter().any(|e| matches!(e, LoopEvent::ToolSkipped { call: c, output }
            if *c == call("c1", "a.ts") && output == not_run::CUT_OFF)));
        assert_eq!(provider.requests()[1].0.last(), Some(&tool("c1", not_run::CUT_OFF)));
        assert_eq!(end(&events), Some(StopReason::End));
    }

    #[tokio::test]
    async fn cut_off_with_nothing_to_run_the_model_is_told_once_a_second_ends_the_run() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            cut_off(vec![ProviderEvent::TextDelta("Here is the fi".into())]),
            cut_off(vec![]),
        ]));
        let h = history(vec![user("go")]);
        let events = collect(options(provider.clone(), &h, plain())).await;
        let requests = provider.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].0[requests[1].0.len() - 2..], [said("Here is the fi"), user(CUT_OFF_NOTE)]);
        assert_eq!(cut_offs(&events), [true, false]);
        assert_eq!(end(&events), Some(StopReason::Length));
    }

    #[tokio::test]
    async fn only_two_cut_offs_in_a_row_end_the_run() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            cut_off(vec![]),
            use_tools(&[call("c1", "a.ts")]),
            cut_off(vec![]),
            say("Done."),
        ]));
        let h = history(vec![user("go")]);
        let events = collect(options(provider, &h, plain())).await;
        assert_eq!(cut_offs(&events), [true, true]);
        assert_eq!(end(&events), Some(StopReason::End));
    }

    #[tokio::test]
    async fn a_cut_off_reply_that_was_all_reasoning_is_told_to_reason_briefly() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            cut_off(vec![ProviderEvent::ThinkingDelta("Let me think about boids at length…".into())]),
            say("Done."),
        ]));
        let h = history(vec![user("go")]);
        collect(options(provider.clone(), &h, plain())).await;
        assert_eq!(
            provider.requests()[1].0.last(),
            Some(&user(&format!("go\n\n{CUT_OFF_REASONING_NOTE}"))),
            "joined, not a second user turn"
        );
    }

    #[tokio::test]
    async fn a_consumer_that_stops_at_the_cut_off_event_still_leaves_every_call_answered() {
        let provider = Arc::new(ScriptedProvider::new(vec![cut_off(vec![ProviderEvent::ToolCall(call("c1", "a.ts"))])]));
        let h = history(vec![user("go")]);
        let mut stream = Box::pin(run_agent(options(provider, &h, plain())));
        while let Some(event) = stream.next().await {
            if matches!(event, LoopEvent::CutOff { .. }) {
                break;
            }
        }
        drop(stream);
        let turns = snapshot(&h);
        assert_eq!(answer_all_calls(&turns), turns);
        assert_eq!(turns.last(), Some(&tool("c1", not_run::CUT_OFF)));
    }

    #[tokio::test]
    async fn cut_off_and_interrupted_reads_as_interrupted() {
        let cancel = CancellationToken::new();
        let provider = Arc::new(CancelsWhileStreaming {
            cancel: cancel.clone(),
            events: cut_off(vec![ProviderEvent::ToolCall(call("c1", "a.ts"))]),
        });
        let h = history(vec![user("go")]);
        let events = collect(AgentOptions::new(provider, h.clone(), "S".into(), specs(), Arc::new(plain()), cancel)).await;
        assert!(cut_offs(&events).is_empty());
        assert_eq!(snapshot(&h).last(), Some(&tool("c1", not_run::ABORTED)));
        assert_eq!(end(&events), Some(StopReason::Aborted));
    }

    // Parallel calls. Marv runs consecutive `agent` calls together; here the runner's `concurrent` decides (#58).

    fn sub(id: &str, ms: u64) -> ToolCall {
        ToolCall { id: id.into(), name: "agent".into(), arguments: serde_json::json!({"description": id, "ms": ms}).to_string() }
    }
    fn is_agent(call: &ToolCall) -> bool {
        call.name == "agent"
    }

    #[derive(Default)]
    struct Tracked {
        active: usize,
        max: usize,
        started: Vec<String>,
    }

    /// A runner that sleeps `ms` (from the arguments) and records how many run at once.
    fn tracked(decline: Option<&'static str>) -> (Arc<Mutex<Tracked>>, FakeRunner) {
        let state = Arc::new(Mutex::new(Tracked::default()));
        let shared = state.clone();
        let run: RunFn = Arc::new(move |c, _| {
            let state = shared.clone();
            {
                let mut s = state.lock().unwrap();
                s.started.push(c.id.clone());
                s.active += 1;
                s.max = s.max.max(s.active);
            }
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(arg(&c, "ms").as_u64().unwrap_or(0))).await;
                state.lock().unwrap().active -= 1;
                Ok(ToolRun { declined: Some(c.id.as_str()) == decline, ..ok(format!("out {}", c.id), c.id.clone()) })
            })
        });
        (state, FakeRunner { run, parallel: is_agent })
    }

    fn ends(events: &[LoopEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                LoopEvent::ToolEnd { call, .. } => Some(call.id.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn consecutive_parallel_calls_run_at_once_results_go_in_call_order() {
        let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[sub("a", 60), sub("b", 10)]), say("Both done.")]));
        let h = history(vec![user("go")]);
        let (state, runner) = tracked(None);
        let events = collect(options(provider, &h, runner)).await;
        assert_eq!(state.lock().unwrap().max, 2);
        // b finished first…
        assert_eq!(ends(&events), ["b", "a"]);
        // …but the history is in call order, so every request is deterministic (prompt cache).
        assert_eq!(tool_texts(&h), ["out a", "out b"]);
        assert_eq!(end(&events), Some(StopReason::End));
    }

    #[tokio::test]
    async fn at_most_max_parallel_run_at_once() {
        let calls: Vec<_> = (0..6).map(|i| sub(&format!("s{i}"), 20)).collect();
        let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&calls), say("ok")]));
        let (state, runner) = tracked(None);
        collect(options(provider, &history(vec![user("go")]), runner)).await;
        let state = state.lock().unwrap();
        assert_eq!(state.max, MAX_PARALLEL);
        assert_eq!(state.started.len(), 6);
    }

    #[tokio::test]
    async fn a_non_concurrent_call_between_them_splits_the_group() {
        let provider =
            Arc::new(ScriptedProvider::new(vec![use_tools(&[sub("a", 20), call("r", "x.ts"), sub("b", 20)]), say("ok")]));
        let (state, runner) = tracked(None);
        collect(options(provider, &history(vec![user("go")]), runner)).await;
        let state = state.lock().unwrap();
        assert_eq!(state.max, 1);
        assert_eq!(state.started, ["a", "r", "b"]);
    }

    #[tokio::test]
    async fn by_default_reads_run_together_and_a_write_is_a_barrier() {
        // #58: no `concurrent` override, so the runner's kinds decide.
        struct Kinds(FakeRunner);
        #[async_trait]
        impl ToolRunner for Kinds {
            fn kind(&self, call: &ToolCall) -> ToolKind {
                self.0.kind(call)
            }
            async fn run(&self, call: ToolCall, cancel: CancellationToken) -> Result<ToolRun, MarvError> {
                self.0.run(call, cancel).await
            }
        }
        let read = |id: &str| ToolCall { name: "read_file".into(), ..sub(id, 20) };
        let write = ToolCall { name: "write_file".into(), ..sub("w", 0) };
        let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[read("a"), read("b"), write, read("c")]), say("ok")]));
        let (state, runner) = tracked(None);
        let h = history(vec![user("go")]);
        let opts = AgentOptions::new(provider, h.clone(), "S".into(), specs(), Arc::new(Kinds(runner)), CancellationToken::new());
        collect(opts).await;
        assert_eq!(state.lock().unwrap().max, 2);
        assert_eq!(state.lock().unwrap().started, ["a", "b", "w", "c"]);
        assert_eq!(tool_texts(&h), ["out a", "out b", "out w", "out c"]);
    }

    #[tokio::test]
    async fn an_interrupt_answers_queued_calls_without_running_them() {
        let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[sub("a", 10), sub("b", 10), sub("c", 10)])]));
        let h = history(vec![user("go")]);
        let aborting: RunFn = Arc::new(|c, cancel| {
            cancel.cancel();
            Box::pin(async move { Ok(ok(format!("out {}", c.id), c.id.clone())) })
        });
        let events =
            collect(AgentOptions { max_parallel: 1, ..options(provider, &h, FakeRunner { run: aborting, parallel: is_agent }) })
                .await;
        assert_eq!(tool_texts(&h), ["out a", not_run::ABORTED, not_run::ABORTED]);
        assert_eq!(end(&events), Some(StopReason::Aborted));
    }

    #[tokio::test]
    async fn a_no_in_a_parallel_group_lets_the_running_ones_finish_then_stops() {
        let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[sub("a", 5), sub("b", 30)]), say("never")]));
        let h = history(vec![user("go")]);
        let (_, runner) = tracked(Some("a"));
        let events = collect(options(provider.clone(), &h, runner)).await;
        assert_eq!(tool_texts(&h), ["out a", "out b"]);
        assert_eq!(provider.requests().len(), 1);
        assert_eq!(end(&events), Some(StopReason::Declined));
    }

    /// Pulls until the first ToolStart, then stops (as a consumer whose handler fails would).
    async fn stop_at_first_tool_start(provider: Arc<ScriptedProvider>, runner: FakeRunner) {
        let mut stream = Box::pin(run_agent(options(provider, &history(vec![user("go")]), runner)));
        while let Some(event) = stream.next().await {
            if matches!(event, LoopEvent::ToolStart { .. }) {
                break;
            }
        }
        drop(stream);
        tokio::time::sleep(Duration::from_millis(20)).await; // anything already launched would have started by now
    }

    #[tokio::test]
    async fn a_tool_doesnt_start_until_its_tool_start_was_delivered_single_call() {
        let (state, runner) = tracked(None);
        stop_at_first_tool_start(Arc::new(ScriptedProvider::new(vec![use_tools(&[call("r", "x.ts")]), say("ok")])), runner).await;
        assert!(state.lock().unwrap().started.is_empty());
    }

    #[tokio::test]
    async fn a_tool_doesnt_start_until_its_tool_start_was_delivered_parallel_group() {
        let (state, runner) = tracked(None);
        stop_at_first_tool_start(
            Arc::new(ScriptedProvider::new(vec![use_tools(&[sub("a", 5), sub("b", 5)]), say("ok")])),
            runner,
        )
        .await;
        assert!(state.lock().unwrap().started.is_empty());
    }

    #[tokio::test]
    async fn a_no_in_a_parallel_group_answers_its_queued_calls_without_running_them() {
        let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[sub("a", 5), sub("b", 5), sub("c", 5)]), say("never")]));
        let h = history(vec![user("go")]);
        let (state, runner) = tracked(Some("a"));
        let events = collect(AgentOptions { max_parallel: 1, ..options(provider, &h, runner) }).await;
        assert_eq!(state.lock().unwrap().started, ["a"]);
        assert_eq!(tool_texts(&h), ["out a", not_run::DECLINED, not_run::DECLINED]);
        assert_eq!(end(&events), Some(StopReason::Declined));
    }

    #[tokio::test]
    async fn a_tool_that_fails_in_a_group_becomes_an_error_result_and_the_run_goes_on() {
        let provider =
            Arc::new(ScriptedProvider::new(vec![use_tools(&[sub("a", 0), sub("b", 0), sub("c", 0)]), say("Recovered.")]));
        let h = history(vec![user("go")]);
        let failing: RunFn = Arc::new(|c, _| {
            Box::pin(async move {
                match c.id.as_str() {
                    "a" => Err(MarvError::Tool("boom".into())),
                    "b" => Err(MarvError::Provider("not a tool error".into())),
                    _ => Ok(ok("out c", "c")),
                }
            })
        });
        let events = collect(options(provider.clone(), &h, FakeRunner { run: failing, parallel: is_agent })).await;
        assert_eq!(tool_texts(&h), ["agent failed: boom", "agent failed: provider: not a tool error", "out c"]);
        assert!(events.iter().any(|e| matches!(e, LoopEvent::ToolEnd { run, .. } if run.result.is_error)));
        assert_eq!(provider.requests().len(), 2);
        assert_eq!(end(&events), Some(StopReason::End));
    }

    #[tokio::test]
    async fn after_an_interrupt_reserved_and_queued_calls_are_answered_without_a_tool_start() {
        let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[sub("a", 10), sub("b", 10), sub("c", 10)])]));
        let h = history(vec![user("go")]);
        let started = Arc::new(Mutex::new(Vec::new()));
        let seen = started.clone();
        let aborting: RunFn = Arc::new(move |c, cancel| {
            seen.lock().unwrap().push(c.id.clone());
            cancel.cancel();
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                Ok(ok(format!("out {}", c.id), c.id.clone()))
            })
        });
        let events =
            collect(AgentOptions { max_parallel: 2, ..options(provider, &h, FakeRunner { run: aborting, parallel: is_agent }) })
                .await;
        // b had a slot too, but the interrupt came before its ToolStart was delivered.
        assert_eq!(*started.lock().unwrap(), ["a"]);
        assert_eq!(starts(&events), ["a"]);
        assert_eq!(tool_texts(&h), ["out a", not_run::ABORTED, not_run::ABORTED]);
        assert_eq!(end(&events), Some(StopReason::Aborted));
    }

    #[tokio::test]
    async fn a_call_after_a_declined_parallel_group_isnt_run() {
        let provider =
            Arc::new(ScriptedProvider::new(vec![use_tools(&[sub("a", 5), sub("b", 5), call("r", "x.ts")]), say("never")]));
        let h = history(vec![user("go")]);
        let (state, runner) = tracked(Some("a"));
        let events = collect(options(provider, &h, runner)).await;
        assert_eq!(state.lock().unwrap().started, ["a", "b"]);
        assert_eq!(tool_texts(&h), ["out a", "out b", not_run::DECLINED]);
        assert_eq!(end(&events), Some(StopReason::Declined));
    }

    /// Esc at an approval declines what's waiting and aborts the run: the model must read that as an
    /// interrupt, not as the user saying no to those calls.
    fn esc_on(id: &'static str) -> FakeRunner {
        let run: RunFn = Arc::new(move |c, cancel| {
            if c.id == id {
                cancel.cancel();
            }
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(5)).await;
                Ok(ToolRun { declined: c.id == id, ..ok(format!("out {}", c.id), c.id.clone()) })
            })
        });
        FakeRunner { run, parallel: is_agent }
    }

    #[tokio::test]
    async fn after_esc_a_groups_queued_calls_read_as_interrupted() {
        let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[sub("a", 5), sub("b", 5), sub("c", 5)]), say("never")]));
        let h = history(vec![user("go")]);
        let events = collect(AgentOptions { max_parallel: 1, ..options(provider, &h, esc_on("a")) }).await;
        assert_eq!(tool_texts(&h), ["out a", not_run::ABORTED, not_run::ABORTED]);
        assert_eq!(end(&events), Some(StopReason::Aborted));
    }

    #[tokio::test]
    async fn after_esc_the_calls_after_the_group_read_as_interrupted_too() {
        let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[sub("a", 5), call("r", "x.ts")]), say("never")]));
        let h = history(vec![user("go")]);
        collect(options(provider, &h, esc_on("a"))).await;
        assert_eq!(tool_texts(&h), ["out a", not_run::ABORTED]);
    }

    #[tokio::test]
    async fn an_agent_call_is_labeled_with_its_type_and_description_while_it_runs() {
        let typed = ToolCall {
            id: "t".into(),
            name: "agent".into(),
            arguments: serde_json::json!({"type": "explore", "description": "Find the config"}).to_string(),
        };
        let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[sub("Read notes", 0), typed]), say("ok")]));
        let (_, runner) = tracked(None);
        let events = collect(options(provider, &history(vec![user("go")]), runner)).await;
        let labels: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                LoopEvent::ToolStart { label, .. } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(labels, ["general-purpose · Read notes", "explore · Find the config"]);
    }

    #[tokio::test]
    async fn a_tools_display_payload_is_for_the_transcript_the_model_gets_only_the_output() {
        let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts")]), say("Done.")]));
        let with_diff: RunFn = Arc::new(|c, cancel| {
            Box::pin(async move {
                let run = fake_tool()(c, cancel).await?;
                Ok(ToolRun { display: Some(Arc::new("SECRET".to_string())), ..run })
            })
        });
        collect(options(provider.clone(), &history(vec![user("go")]), FakeRunner { run: with_diff, parallel: |_| false })).await;
        assert!(!format!("{:?}", provider.requests()[1].0).contains("SECRET"));
    }

    // after_step.

    fn after_step(f: impl Fn(Vec<ToolRun>) -> Option<String> + Send + Sync + 'static) -> Option<AfterStep> {
        Some(Arc::new(move |runs| {
            let note = f(runs);
            Box::pin(async move { note })
        }))
    }

    #[tokio::test]
    async fn after_steps_note_goes_in_after_all_of_the_steps_results_before_the_next_request() {
        let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts"), call("c2", "b.ts")]), say("Fixed.")]));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        collect(AgentOptions {
            after_step: after_step(move |runs| {
                record.lock().unwrap().push(runs.iter().map(|r| r.result.output.clone()).collect::<Vec<_>>());
                Some("Marv: 1 new error".into())
            }),
            ..options(provider.clone(), &history(vec![user("go")]), plain())
        })
        .await;
        assert_eq!(*seen.lock().unwrap(), [["contents of a.ts", "contents of b.ts"]]);
        let requests = provider.requests();
        let second = &requests[1].0;
        assert!(matches!(&second[second.len() - 3..], [ChatTurn::Tool { .. }, ChatTurn::Tool { .. }, ChatTurn::User { .. }]));
        assert_eq!(second.last(), Some(&user("Marv: 1 new error")));
        // Appended, not inserted: the second request still starts with the first one (prompt cache).
        assert_eq!(second[..requests[0].0.len()], requests[0].0[..]);
    }

    #[tokio::test]
    async fn after_step_none_adds_nothing() {
        let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts")]), say("Done.")]));
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        collect(AgentOptions {
            after_step: after_step(move |_| {
                counted.fetch_add(1, Ordering::SeqCst);
                None
            }),
            ..options(provider.clone(), &history(vec![user("go")]), plain())
        })
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(matches!(provider.requests()[1].0.last(), Some(ChatTurn::Tool { .. })));
    }

    /// How many times `after_step` ran in a run of `script` with `runner`.
    async fn after_step_calls(script: Vec<Vec<ProviderEvent>>, runner: FakeRunner) -> (usize, Vec<LoopEvent>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let events = collect(AgentOptions {
            after_step: after_step(move |_| {
                counted.fetch_add(1, Ordering::SeqCst);
                None
            }),
            ..options(Arc::new(ScriptedProvider::new(script)), &history(vec![user("go")]), runner)
        })
        .await;
        (calls.load(Ordering::SeqCst), events)
    }

    #[tokio::test]
    async fn after_step_is_not_called_after_a_plain_answer() {
        assert_eq!(after_step_calls(vec![say("Hi.")], plain()).await.0, 0);
    }

    #[tokio::test]
    async fn after_step_stopped_during_it_ends_the_run_with_every_call_answered_and_no_note() {
        let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts")]), say("never asked")]));
        let h = history(vec![user("go")]);
        let opts = options(provider.clone(), &h, plain());
        let cancel = opts.cancel.clone();
        let events = collect(AgentOptions {
            after_step: after_step(move |_| {
                cancel.cancel();
                Some("Marv: too late".into())
            }),
            ..opts
        })
        .await;
        assert_eq!(end(&events), Some(StopReason::Aborted));
        assert_eq!(provider.requests().len(), 1);
        assert!(matches!(snapshot(&h).last(), Some(ChatTurn::Tool { .. })));
    }

    #[tokio::test]
    async fn after_step_is_not_called_after_a_declined_call() {
        let declining: RunFn =
            Arc::new(|c, cancel| Box::pin(async move { Ok(ToolRun { declined: true, ..fake_tool()(c, cancel).await? }) }));
        let (calls, events) = after_step_calls(
            vec![use_tools(&[call("c1", "a.ts")]), say("never asked")],
            FakeRunner { run: declining, parallel: |_| false },
        )
        .await;
        assert_eq!(end(&events), Some(StopReason::Declined));
        assert_eq!(calls, 0);
    }

    #[tokio::test]
    async fn after_step_is_not_called_after_a_cut_off_reply() {
        let script = vec![cut_off(vec![ProviderEvent::ToolCall(call("c1", "a.ts"))]), say("Smaller steps, then.")];
        assert_eq!(after_step_calls(script, plain()).await.0, 0);
    }

    #[tokio::test]
    async fn after_step_gets_a_parallel_groups_results_in_call_order() {
        let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[call("a", "a.ts"), call("b", "b.ts")]), say("Done.")]));
        let slow_a: RunFn = Arc::new(|c, cancel| {
            Box::pin(async move {
                if c.id == "a" {
                    tokio::time::sleep(Duration::from_millis(40)).await;
                }
                fake_tool()(c, cancel).await
            })
        });
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        collect(AgentOptions {
            after_step: after_step(move |runs| {
                record.lock().unwrap().push(runs.iter().map(|r| r.result.output.clone()).collect::<Vec<_>>());
                None
            }),
            ..options(provider, &history(vec![user("go")]), FakeRunner { run: slow_a, parallel: |_| true })
        })
        .await;
        assert_eq!(*seen.lock().unwrap(), [["contents of a.ts", "contents of b.ts"]]);
    }

    #[tokio::test]
    async fn at_the_step_limit_the_note_is_already_in_the_history_when_on_limit_is_asked() {
        let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts")]), say("never")]));
        let h = history(vec![user("go")]);
        let seen = h.clone();
        let last = Arc::new(Mutex::new(None));
        let record = last.clone();
        collect(AgentOptions {
            max_steps: 1,
            after_step: after_step(|_| Some("Marv: note".into())),
            on_limit: on_limit(move |_| {
                *record.lock().unwrap() = seen.lock().unwrap().last().cloned();
                LimitAnswer::Stop
            }),
            ..options(provider, &h, plain())
        })
        .await;
        assert_eq!(*last.lock().unwrap(), Some(user("Marv: note")));
    }

    #[tokio::test]
    async fn on_full_compacts_mid_turn_and_the_next_request_starts_from_the_summary() {
        let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts")]), say("Done.")]));
        let h = history(vec![user("go")]);
        let swapped = h.clone();
        let on_full: OnFull = Arc::new(move || {
            *swapped.lock().unwrap() = vec![user("summary"), user(crate::compact::MID_TURN_NOTE)];
            Box::pin(async { true })
        });
        let events = collect(AgentOptions { on_full: Some(on_full), ..options(provider.clone(), &h, plain()) }).await;
        assert!(events.iter().any(|e| matches!(e, LoopEvent::Compacted)));
        assert_eq!(provider.requests()[1].0, [user("summary"), user(crate::compact::MID_TURN_NOTE)]);
        assert_eq!(end(&events), Some(StopReason::End));
    }

    // ---- Marv #63: overflow recovery ----

    const OVERFLOW: &str = "OpenRouter returned 400.\nThis endpoint's maximum context length is 1000 tokens. However, you requested about 1200 tokens (1200 of text input, 0 in the output).";

    fn overflow() -> Vec<ProviderEvent> {
        vec![ProviderEvent::TextDelta("partial".into()), ProviderEvent::Error(OVERFLOW.into())]
    }

    /// A hook answering from a list, recording each attempt; it swaps the history for `[user("fitted")]`.
    fn on_overflow(h: &History, answers: Vec<OverflowOutcome>) -> (OnOverflow, Arc<Mutex<Vec<u32>>>) {
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let (seen, h, answers) = (attempts.clone(), h.clone(), Arc::new(Mutex::new(VecDeque::from(answers))));
        let hook: OnOverflow = Arc::new(move |error, attempt| {
            assert!(error.contains("maximum context length"));
            seen.lock().unwrap().push(attempt);
            *h.lock().unwrap() = vec![user("fitted")];
            let answer = answers.lock().unwrap().pop_front().unwrap_or(OverflowOutcome::Failed("gave up".into()));
            Box::pin(async move { answer })
        });
        (hook, attempts)
    }

    #[tokio::test]
    async fn an_overflow_with_the_hook_retries_the_same_step_with_nothing_of_the_failed_reply() {
        let provider = Arc::new(ScriptedProvider::new(vec![overflow(), say("Done.")]));
        let h = history(vec![user("go")]);
        let (hook, attempts) = on_overflow(&h, vec![OverflowOutcome::Recovered]);
        let events = collect(AgentOptions { on_overflow: Some(hook), ..options(provider.clone(), &h, plain()) }).await;
        assert!(events.iter().any(|e| matches!(e, LoopEvent::Overflow { attempt: 0 })));
        assert!(!events.iter().any(|e| matches!(e, LoopEvent::Error(_))));
        assert_eq!(*attempts.lock().unwrap(), [0]);
        assert_eq!(provider.requests()[1].0, [user("fitted")], "the failed reply's partial text never entered it");
        assert_eq!(end(&events), Some(StopReason::End));
    }

    #[tokio::test]
    async fn an_overflow_hook_that_fails_ends_with_its_text_and_none_ends_as_before() {
        let provider = Arc::new(ScriptedProvider::new(vec![overflow()]));
        let h = history(vec![user("go")]);
        let (hook, _) = on_overflow(&h, vec![OverflowOutcome::Failed("too big".into())]);
        let events = collect(AgentOptions { on_overflow: Some(hook), ..options(provider, &h, plain()) }).await;
        assert!(
            matches!(&events[events.len() - 2..], [LoopEvent::Error(m), LoopEvent::Done(StopReason::Error)] if m == "too big")
        );

        let provider = Arc::new(ScriptedProvider::new(vec![overflow()]));
        let h = history(vec![user("go")]);
        let events = collect(options(provider, &h, plain())).await;
        assert!(!events.iter().any(|e| matches!(e, LoopEvent::Overflow { .. })));
        assert!(
            matches!(&events[events.len() - 2..], [LoopEvent::Error(m), LoopEvent::Done(StopReason::Error)] if m == OVERFLOW)
        );
    }

    #[tokio::test]
    async fn at_most_four_recoveries_a_step_and_the_count_resets_after_a_good_step() {
        let mut script = vec![overflow(), use_tools(&[call("c1", "a.ts")])];
        script.extend((0..5).map(|_| overflow()));
        let provider = Arc::new(ScriptedProvider::new(script));
        let h = history(vec![user("go")]);
        let (hook, attempts) = on_overflow(&h, vec![OverflowOutcome::Recovered; 10]);
        let events = collect(AgentOptions { on_overflow: Some(hook), ..options(provider.clone(), &h, plain()) }).await;
        // Step 1: one recovery; step 2: four, then the fifth overflow gives up.
        assert_eq!(*attempts.lock().unwrap(), [0, 0, 1, 2, 3, 4]);
        assert_eq!(provider.requests().len(), 2 + 5);
        assert!(
            matches!(&events[events.len() - 2..], [LoopEvent::Error(m), LoopEvent::Done(StopReason::Error)] if m == OVERFLOW)
        );
    }

    #[tokio::test]
    async fn the_wrap_up_request_is_recovered_too_and_sent_again_as_the_wrap_up() {
        // max_steps 1, no on_limit: the step limit's tools-off wrap-up (#59) overflows, recovers, and is sent again.
        let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts")]), overflow(), say("Wrapped up.")]));
        let h = history(vec![user("go")]);
        let (hook, attempts) = on_overflow(&h, vec![OverflowOutcome::Recovered]);
        let opts = AgentOptions {
            max_steps: 1,
            wrap_up_at_limit: true,
            on_overflow: Some(hook),
            ..options(provider.clone(), &h, plain())
        };
        let events = collect(opts).await;
        assert_eq!(*attempts.lock().unwrap(), [0]);
        let requests = provider.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[2].1.tool_choice, ToolChoice::None, "still the wrap-up");
        assert_eq!(requests[2].0.last(), Some(&user(&format!("fitted\n\n{STEP_LIMIT_NOTE}"))), "its note again");
        assert_eq!(end(&events), Some(StopReason::MaxSteps));
    }

    #[tokio::test]
    async fn esc_during_the_overflow_hook_ends_aborted() {
        let provider = Arc::new(ScriptedProvider::new(vec![overflow(), say("never")]));
        let h = history(vec![user("go")]);
        let opts = options(provider.clone(), &h, plain());
        let cancel = opts.cancel.clone();
        let hook: OnOverflow = Arc::new(move |_, _| {
            cancel.cancel();
            Box::pin(async { OverflowOutcome::Recovered })
        });
        let events = collect(AgentOptions { on_overflow: Some(hook), ..opts }).await;
        assert_eq!(end(&events), Some(StopReason::Aborted));
        assert!(!events.iter().any(|e| matches!(e, LoopEvent::Error(_))));
        assert_eq!(provider.requests().len(), 1);

        let provider = Arc::new(ScriptedProvider::new(vec![overflow()]));
        let h = history(vec![user("go")]);
        let hook: OnOverflow = Arc::new(|_, _| Box::pin(async { OverflowOutcome::Stopped }));
        let events = collect(AgentOptions { on_overflow: Some(hook), ..options(provider, &h, plain()) }).await;
        assert_eq!(end(&events), Some(StopReason::Aborted));
        assert!(!events.iter().any(|e| matches!(e, LoopEvent::Error(_))));
    }

    #[tokio::test]
    async fn a_note_after_a_mid_turn_compaction_joins_its_note_and_every_request_alternates() {
        let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts")]), say("Done.")]));
        let h = history(vec![user("go")]);
        let swapped = h.clone();
        let on_full: OnFull = Arc::new(move || {
            let ack = ChatTurn::Assistant { text: crate::compact::COMPACTED_ACK.into(), tool_calls: vec![], reasoning: None };
            *swapped.lock().unwrap() = vec![user("summary"), ack, user(crate::compact::MID_TURN_NOTE)];
            Box::pin(async { true })
        });
        let opts = AgentOptions {
            on_full: Some(on_full),
            after_step: after_step(|_| Some("Marv: the typecheck found 1 new error".into())),
            ..options(provider.clone(), &h, plain())
        };
        collect(opts).await;
        for (sent, _) in provider.requests() {
            assert!(crate::compact::alternates(&sent), "{sent:?}");
        }
        let last = provider.requests()[1].0.last().cloned().unwrap();
        assert_eq!(last, user(&format!("{}\n\nMarv: the typecheck found 1 new error", crate::compact::MID_TURN_NOTE)));
    }

    #[tokio::test]
    async fn esc_during_on_full_ends_the_run() {
        let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts")]), say("never")]));
        let h = history(vec![user("go")]);
        let opts = options(provider.clone(), &h, plain());
        let cancel = opts.cancel.clone();
        let on_full: OnFull = Arc::new(move || {
            cancel.cancel();
            Box::pin(async { true })
        });
        let events = collect(AgentOptions { on_full: Some(on_full), ..opts }).await;
        assert_eq!(end(&events), Some(StopReason::Aborted));
        assert_eq!(provider.requests().len(), 1);
    }

    // Issue #5: whatever ends a run, the history it leaves must be one the API accepts. Each step mixes
    // plain calls with a parallel group of agent calls (a slow one in the middle), so the groups are
    // covered too.

    fn mixed(n: &str) -> Vec<ToolCall> {
        vec![
            call(&format!("{n}r1"), "a.ts"),
            sub(&format!("{n}a"), 5),
            sub(&format!("{n}b"), 30),
            sub(&format!("{n}c"), 5),
            call(&format!("{n}r2"), "b.ts"),
        ]
    }
    fn steps(n: usize) -> Vec<Vec<ProviderEvent>> {
        (1..=n).map(|i| use_tools(&mixed(&format!("s{i}")))).collect()
    }
    fn cut_off_calls(calls: Vec<ToolCall>) -> Vec<ProviderEvent> {
        cut_off(calls.into_iter().map(ProviderEvent::ToolCall).collect())
    }

    #[derive(Default)]
    struct Drive {
        /// The call to cancel on (ctrl+c while it runs), and to answer "no" (both: Esc at an approval).
        abort_on: Option<&'static str>,
        decline_on: Option<&'static str>,
        /// Where the consumer stops (it drops the stream: a break or a failure, alike here).
        stop_at: Option<StopAt>,
    }

    type StopAt = Box<dyn Fn(&LoopEvent) -> bool + Send>;
    /// The call of a tool event of one kind, for where a consumer stops.
    type PickCall = fn(&LoopEvent) -> Option<&ToolCall>;

    struct Driven {
        history: Vec<ChatTurn>,
        events: Vec<LoopEvent>,
        stopped: bool,
        calls: usize,
    }

    impl Driven {
        fn end(&self) -> Option<StopReason> {
            end(&self.events)
        }
    }

    async fn drive(script: Vec<Vec<ProviderEvent>>, how: Drive, configure: impl FnOnce(AgentOptions) -> AgentOptions) -> Driven {
        let Drive { abort_on, decline_on, stop_at } = how;
        let h = history(vec![user("go")]);
        let run: RunFn = Arc::new(move |c, cancel| {
            if Some(c.id.as_str()) == abort_on {
                cancel.cancel();
            }
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(arg(&c, "ms").as_u64().unwrap_or(0))).await;
                Ok(ToolRun { declined: Some(c.id.as_str()) == decline_on, ..ok(format!("out {}", c.id), c.id.clone()) })
            })
        });
        let opts = configure(AgentOptions {
            max_parallel: 2,
            ..options(Arc::new(ScriptedProvider::new(script)), &h, FakeRunner { run, parallel: is_agent })
        });
        let mut stream = Box::pin(run_agent(opts));
        let mut events = Vec::new();
        let mut stopped = false;
        while let Some(event) = stream.next().await {
            let stop = stop_at.as_ref().is_some_and(|f| f(&event));
            events.push(event);
            if stop {
                stopped = true;
                break;
            }
        }
        drop(stream);
        tokio::time::sleep(Duration::from_millis(50)).await; // tools still running after the consumer left must not add anything
        let history = snapshot(&h);
        let calls = history
            .iter()
            .map(|t| match t {
                ChatTurn::Assistant { tool_calls, .. } => tool_calls.len(),
                _ => 0,
            })
            .sum();
        Driven { history, events, stopped, calls }
    }

    fn same(o: AgentOptions) -> AgentOptions {
        o
    }

    #[tokio::test]
    async fn every_call_answered_a_run_that_answers() {
        let mut script = steps(2);
        script.push(say("Done."));
        let d = drive(script, Drive::default(), same).await;
        assert_eq!(d.calls, 10);
        assert_eq!(answer_problems(&d.history), Vec::<String>::new());
        assert_eq!(d.end(), Some(StopReason::End));
    }

    fn two_steps_then_never() -> Vec<Vec<ProviderEvent>> {
        let mut script = steps(2);
        script.push(say("never"));
        script
    }

    #[tokio::test]
    async fn every_call_answered_ctrl_c_mid_tools() {
        for id in ["s2r1", "s2a", "s2b", "s2r2"] {
            let d = drive(two_steps_then_never(), Drive { abort_on: Some(id), ..Default::default() }, same).await;
            assert_eq!(answer_problems(&d.history), Vec::<String>::new(), "while {id} runs");
            assert_eq!(d.end(), Some(StopReason::Aborted), "while {id} runs");
        }
    }

    #[tokio::test]
    async fn every_call_answered_a_no_from_the_approver() {
        for id in ["s2r1", "s2a", "s2b", "s2r2"] {
            let d = drive(two_steps_then_never(), Drive { decline_on: Some(id), ..Default::default() }, same).await;
            assert_eq!(answer_problems(&d.history), Vec::<String>::new(), "to {id}");
            assert_eq!(d.end(), Some(StopReason::Declined), "to {id}");
        }
    }

    #[tokio::test]
    async fn every_call_answered_esc_at_an_approval_in_the_group() {
        let d =
            drive(two_steps_then_never(), Drive { decline_on: Some("s2b"), abort_on: Some("s2b"), ..Default::default() }, same)
                .await;
        assert_eq!(answer_problems(&d.history), Vec::<String>::new());
        assert_eq!(d.end(), Some(StopReason::Aborted));
    }

    #[tokio::test]
    async fn every_call_answered_a_consumer_that_stops() {
        let cases: [(&str, PickCall, &str); 4] = [
            (
                "at a plain call's tool_start",
                |e| if let LoopEvent::ToolStart { call, .. } = e { Some(call) } else { None },
                "s2r1",
            ),
            (
                "at a group's first tool_start (the other has a slot)",
                |e| if let LoopEvent::ToolStart { call, .. } = e { Some(call) } else { None },
                "s2a",
            ),
            (
                "at a group's tool_end, with a call still running",
                |e| if let LoopEvent::ToolEnd { call, .. } = e { Some(call) } else { None },
                "s2a",
            ),
            ("at the last call's tool_end", |e| if let LoopEvent::ToolEnd { call, .. } = e { Some(call) } else { None }, "s2r2"),
        ];
        for (name, pick, id) in cases {
            let stop_at = Box::new(move |e: &LoopEvent| pick(e).is_some_and(|c| c.id == id));
            let d = drive(two_steps_then_never(), Drive { stop_at: Some(stop_at), ..Default::default() }, same).await;
            assert!(d.stopped, "{name}: never stopped");
            assert_eq!(pick(d.events.last().unwrap()).map(|c| c.id.as_str()), Some(id), "{name}: nothing came after it");
            assert_eq!(answer_problems(&d.history), Vec::<String>::new(), "{name}");
        }
    }

    #[tokio::test]
    async fn every_call_answered_a_consumer_that_stops_at_a_tool_skipped_after_a_no() {
        let d = drive(
            two_steps_then_never(),
            Drive {
                decline_on: Some("s2r1"),
                stop_at: Some(Box::new(|e| matches!(e, LoopEvent::ToolSkipped { .. }))),
                ..Default::default()
            },
            same,
        )
        .await;
        assert!(matches!(d.events.last(), Some(LoopEvent::ToolSkipped { call, .. }) if call.id == "s2a"));
        assert_eq!(answer_problems(&d.history), Vec::<String>::new());
    }

    #[tokio::test]
    async fn every_call_answered_a_cut_off_once() {
        let mut script = steps(1);
        script.push(cut_off_calls(mixed("s2")));
        script.push(say("Smaller steps."));
        let d = drive(script, Drive::default(), same).await;
        assert_eq!(answer_problems(&d.history), Vec::<String>::new());
        assert_eq!(d.end(), Some(StopReason::End));
    }

    #[tokio::test]
    async fn every_call_answered_cut_off_twice_in_a_row() {
        let mut script = steps(1);
        script.push(cut_off_calls(mixed("s2")));
        script.push(cut_off_calls(mixed("s3")));
        let d = drive(script, Drive::default(), same).await;
        assert_eq!(d.calls, 15);
        assert_eq!(answer_problems(&d.history), Vec::<String>::new());
        assert_eq!(d.end(), Some(StopReason::Length));
    }

    #[tokio::test]
    async fn every_call_answered_a_consumer_that_stops_at_a_cut_off_calls_tool_skipped() {
        let d = drive(
            vec![cut_off_calls(mixed("s1"))],
            Drive { stop_at: Some(Box::new(|e| matches!(e, LoopEvent::ToolSkipped { .. }))), ..Default::default() },
            same,
        )
        .await;
        assert!(matches!(d.events.last(), Some(LoopEvent::ToolSkipped { call, .. }) if call.id == "s1r1"));
        assert_eq!(answer_problems(&d.history), Vec::<String>::new());
    }

    #[tokio::test]
    async fn every_call_answered_max_steps_0_turns_the_limit_off() {
        let mut script = steps(3);
        script.push(say("Done."));
        let d = drive(script, Drive::default(), |o| AgentOptions { max_steps: 0, ..o }).await;
        assert_eq!(d.calls, 15, "3 steps × 5 calls, none cut off by a limit");
        assert_eq!(answer_problems(&d.history), Vec::<String>::new());
        assert_eq!(d.end(), Some(StopReason::End));
    }

    #[tokio::test]
    async fn every_call_answered_without_on_limit_a_hard_stop() {
        let d = drive(steps(5), Drive::default(), |o| AgentOptions { max_steps: 2, ..o }).await;
        assert_eq!(d.calls, 10);
        assert_eq!(answer_problems(&d.history), Vec::<String>::new());
        assert_eq!(d.end(), Some(StopReason::MaxSteps));
    }

    #[tokio::test]
    async fn every_call_answered_with_on_limit_a_no_stops_it() {
        let d =
            drive(steps(5), Drive::default(), |o| AgentOptions { max_steps: 2, on_limit: on_limit(|_| LimitAnswer::Stop), ..o })
                .await;
        assert_eq!(answer_problems(&d.history), Vec::<String>::new());
        assert_eq!(d.end(), Some(StopReason::MaxSteps));
    }

    #[tokio::test]
    async fn every_call_answered_with_on_limit_unavailable_stops_like_a_hard_limit() {
        let d = drive(steps(5), Drive::default(), |o| AgentOptions {
            max_steps: 2,
            on_limit: on_limit(|_| LimitAnswer::Unavailable),
            ..o
        })
        .await;
        assert_eq!(answer_problems(&d.history), Vec::<String>::new());
        assert!(d.events.iter().any(|e| matches!(e, LoopEvent::StepLimit { steps: 2, continued: false })));
        assert!(d.events.iter().any(|e| matches!(e, LoopEvent::Error(m) if m.contains("without a final answer"))));
        assert_eq!(d.end(), Some(StopReason::MaxSteps));
    }

    #[tokio::test]
    async fn every_call_answered_with_on_limit_a_yes_goes_on_to_the_next_limit() {
        let d = drive(steps(5), Drive::default(), |o| AgentOptions {
            max_steps: 2,
            on_limit: on_limit(|n| if n < 4 { LimitAnswer::Continue } else { LimitAnswer::Stop }),
            ..o
        })
        .await;
        assert_eq!(d.calls, 20);
        assert_eq!(answer_problems(&d.history), Vec::<String>::new());
        assert_eq!(d.end(), Some(StopReason::MaxSteps));
    }

    #[tokio::test]
    async fn every_call_answered_with_on_limit_esc_while_it_asks() {
        let d = drive(steps(5), Drive::default(), |o| {
            let cancel = o.cancel.clone();
            AgentOptions {
                max_steps: 2,
                on_limit: on_limit(move |_| {
                    cancel.cancel();
                    LimitAnswer::Stop
                }),
                ..o
            }
        })
        .await;
        assert_eq!(answer_problems(&d.history), Vec::<String>::new());
        assert_eq!(d.end(), Some(StopReason::Aborted));
    }

    #[tokio::test]
    async fn every_call_answered_a_consumer_that_stops_at_the_step_limit_event() {
        let d = drive(
            steps(5),
            Drive { stop_at: Some(Box::new(|e| matches!(e, LoopEvent::StepLimit { .. }))), ..Default::default() },
            |o| AgentOptions { max_steps: 2, on_limit: on_limit(|_| LimitAnswer::Continue), ..o },
        )
        .await;
        assert!(matches!(d.events.last(), Some(LoopEvent::StepLimit { steps: 2, continued: true })));
        assert_eq!(answer_problems(&d.history), Vec::<String>::new());
    }

    #[tokio::test]
    async fn wrap_up_at_the_step_limit_asks_for_one_tools_off_reply() {
        // #59: tools still sent (the cache holds), any call answered without running, still MaxSteps.
        let mut script = steps(2);
        script.push(vec![
            ProviderEvent::TextDelta("Read both; next: fix b.".into()),
            ProviderEvent::ToolCall(call("late", "c.ts")),
            ProviderEvent::Done { reason: Some("tool_calls".into()) },
        ]);
        let provider = Arc::new(ScriptedProvider::new(script));
        let h = history(vec![user("go")]);
        let (state, runner) = tracked(None);
        let events =
            collect(AgentOptions { max_steps: 2, wrap_up_at_limit: true, ..options(provider.clone(), &h, runner) }).await;
        let requests = provider.requests();
        assert_eq!(requests.len(), 3);
        let (wrap_history, wrap_options) = &requests[2];
        assert_eq!(wrap_history.last(), Some(&user(STEP_LIMIT_NOTE)));
        assert_eq!(wrap_options.tool_choice, ToolChoice::None);
        assert_eq!(wrap_options.tools, requests[0].1.tools);
        assert_eq!(requests[0].1.tool_choice, ToolChoice::Auto);
        assert!(!state.lock().unwrap().started.contains(&"late".to_string()));
        let turns = snapshot(&h);
        assert_eq!(turns.last(), Some(&tool("late", not_run::STEP_LIMIT)));
        assert_eq!(answer_problems(&turns), Vec::<String>::new());
        assert!(events.iter().any(|e| matches!(e, LoopEvent::Assistant { text } if text == "Read both; next: fix b.")));
        assert!(events.iter().any(|e| matches!(e, LoopEvent::Error(m) if m.contains("without a final answer"))));
        assert_eq!(end(&events), Some(StopReason::MaxSteps));
    }

    #[tokio::test]
    async fn no_wrap_up_when_the_user_said_stop() {
        let provider = Arc::new(ScriptedProvider::new(steps(5)));
        let events = collect(AgentOptions {
            max_steps: 2,
            wrap_up_at_limit: true,
            on_limit: on_limit(|_| LimitAnswer::Stop),
            ..options(provider.clone(), &history(vec![user("go")]), tracked(None).1)
        })
        .await;
        assert_eq!(provider.requests().len(), 2);
        assert_eq!(end(&events), Some(StopReason::MaxSteps));
    }

    // Text loop detection.

    fn repeat(sentence: &str, n: usize) -> Vec<ProviderEvent> {
        vec![ProviderEvent::TextDelta(vec![sentence; n].join("\n")), ProviderEvent::Done { reason: Some("stop".into()) }]
    }
    fn text_loops(events: &[LoopEvent]) -> Vec<bool> {
        events
            .iter()
            .filter_map(|e| match e {
                LoopEvent::TextLoop { continued } => Some(*continued),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_sentence_repeated_8_plus_times_is_caught_and_the_reply_dropped() {
        let provider =
            Arc::new(ScriptedProvider::new(vec![repeat("Fix the test scripts now.", 10), say("I'll do something else.")]));
        let events = collect(options(provider.clone(), &history(vec![user("go")]), plain())).await;
        assert_eq!(text_loops(&events), [true]);
        assert!(matches!(provider.requests()[1].0.last(), Some(ChatTurn::User { text }) if text.contains("stuck repeating")));
        assert_eq!(end(&events), Some(StopReason::End));
    }

    #[tokio::test]
    async fn a_text_loop_is_stopped_while_streaming_and_none_of_it_reaches_the_next_request() {
        let sentence = "Fix the test scripts now. ";
        let mut looping: Vec<_> = (0..60).map(|_| ProviderEvent::TextDelta(sentence.into())).collect();
        looping.push(ProviderEvent::ToolCall(call("c9", "a.ts")));
        looping.push(ProviderEvent::Done { reason: Some("stop".into()) });
        let provider = Arc::new(ScriptedProvider::new(vec![looping, say("I'll do something else.")]));
        let events = collect(options(provider.clone(), &history(vec![user("go")]), plain())).await;
        assert_eq!(text_loops(&events), [true]);
        assert!(starts(&events).is_empty());
        // Stopped early: far fewer than 60 deltas were passed on.
        assert!(count(&events, |e| matches!(e, LoopEvent::TextDelta(_))) < 40);
        let next = &provider.requests()[1].0;
        assert!(!format!("{next:?}").contains(sentence.trim()));
        assert!(matches!(next.last(), Some(ChatTurn::User { text }) if text.contains("stuck repeating")));
    }

    #[tokio::test]
    async fn a_second_text_loop_in_a_row_ends_the_run() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            repeat("Fix the test scripts now.", 10),
            repeat("Fix the test scripts now.", 10),
        ]));
        let events = collect(options(provider, &history(vec![user("go")]), plain())).await;
        assert_eq!(text_loops(&events), [true, false]);
        assert_eq!(end(&events), Some(StopReason::Loop));
    }

    #[tokio::test]
    async fn a_step_that_answered_resets_the_text_loop_count() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            repeat("Fix the test scripts now.", 10),
            use_tools(&[call("c1", "a.ts")]),
            repeat("Fix the test scripts now.", 10),
            say("Done."),
        ]));
        let events = collect(options(provider, &history(vec![user("go")]), plain())).await;
        assert_eq!(text_loops(&events), [true, true]);
        assert_eq!(end(&events), Some(StopReason::End));
    }

    #[tokio::test]
    async fn varied_text_doesnt_trigger_a_text_loop() {
        let text = [
            "- Install dependencies with `bun install`.",
            "- Run tests with `bun test`.",
            "- Check types with `bun run typecheck`.",
            "- Done at http://localhost:3000.",
        ]
        .join("\n");
        let provider = Arc::new(ScriptedProvider::new(vec![say(&text), say("Done.")]));
        let events = collect(options(provider, &history(vec![user("go")]), plain())).await;
        assert!(text_loops(&events).is_empty());
        assert_eq!(end(&events), Some(StopReason::End));
    }

    // Tool loop detection.

    fn read_call(id: &str, path: &str, extra: serde_json::Value) -> ToolCall {
        let mut args = serde_json::json!({ "path": path });
        if let (Some(args), serde_json::Value::Object(extra)) = (args.as_object_mut(), extra) {
            args.extend(extra);
        }
        ToolCall { id: id.into(), name: "read_file".into(), arguments: args.to_string() }
    }
    fn tool_loops(events: &[LoopEvent]) -> Vec<bool> {
        events
            .iter()
            .filter_map(|e| match e {
                LoopEvent::ToolLoop { continued } => Some(*continued),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn period_1_the_same_call_with_the_same_result_warns_on_the_4th_and_stops_on_the_5th() {
        // A call counts as a repeat when it also got the same result as last time, which the first call
        // can't have: calls 2, 3 and 4 are the three identical entries.
        let script = (1..=5).map(|i| use_tools(&[read_call(&format!("c{i}"), "a.ts", serde_json::json!({}))])).collect();
        let events = collect(options(Arc::new(ScriptedProvider::new(script)), &history(vec![user("go")]), plain())).await;
        assert_eq!(tool_loops(&events), [true, false]);
        assert_eq!(end(&events), Some(StopReason::Loop));
    }

    #[tokio::test]
    async fn period_2_alternating_offsets_are_caught() {
        // The exact pattern from issue #37: read_file with offset 95, then 98, alternating, each getting
        // the same result as the last time it ran.
        let script = [95, 98, 95, 98, 95, 98, 95]
            .iter()
            .enumerate()
            .map(|(i, offset)| {
                use_tools(&[read_call(
                    &format!("c{i}"),
                    "src/tools/index.ts",
                    serde_json::json!({"offset": offset, "limit": 40}),
                )])
            })
            .collect();
        let events = collect(options(Arc::new(ScriptedProvider::new(script)), &history(vec![user("go")]), plain())).await;
        assert_eq!(tool_loops(&events), [true]);
        assert_eq!(end(&events), Some(StopReason::End)); // no more scripted steps after the warning
    }

    #[tokio::test]
    async fn the_same_call_getting_a_new_result_each_time_is_new_work_not_a_loop() {
        // A tool that hands out the next queued item on each identical call.
        let n = Arc::new(AtomicUsize::new(0));
        let queue: RunFn = Arc::new(move |c, _| {
            let item = n.fetch_add(1, Ordering::SeqCst) + 1;
            Box::pin(async move { Ok(ok(format!("item {item}"), c.id.clone())) })
        });
        let script = (0..6).map(|i| use_tools(&[read_call(&format!("c{i}"), "queue", serde_json::json!({}))])).collect();
        let events = collect(options(
            Arc::new(ScriptedProvider::new(script)),
            &history(vec![user("go")]),
            FakeRunner { run: queue, parallel: |_| false },
        ))
        .await;
        assert!(tool_loops(&events).is_empty());
        assert_eq!(count(&events, |e| matches!(e, LoopEvent::ToolEnd { .. })), 6);
    }

    #[tokio::test]
    async fn a_write_tool_between_calls_resets_the_loop_counter() {
        let read = |id: &str| use_tools(&[read_call(id, "a.ts", serde_json::json!({}))]);
        let write = ToolCall {
            id: "c3".into(),
            name: "write_file".into(),
            arguments: serde_json::json!({"path": "a.ts", "content": "edited"}).to_string(),
        };
        let script = vec![read("c1"), read("c2"), use_tools(&[write]), read("c4"), read("c5"), say("Done.")];
        let events = collect(options(Arc::new(ScriptedProvider::new(script)), &history(vec![user("go")]), plain())).await;
        // The write_file reset, so 4 and 5 are fresh: history stays too short to trigger.
        assert!(tool_loops(&events).is_empty());
        assert_eq!(end(&events), Some(StopReason::End));
    }

    // Issue #81: a clone that can't work without network, re-sent with another tail each time.
    fn bash_call(id: &str, command: &str) -> ToolCall {
        ToolCall { id: id.into(), name: "bash".into(), arguments: serde_json::json!({ "command": command }).to_string() }
    }
    /// Every bash command fails like a clone without network, counting how many ran.
    fn offline(ran: Arc<AtomicUsize>) -> FakeRunner {
        let run: RunFn = Arc::new(move |c, _| {
            ran.fetch_add(1, Ordering::SeqCst);
            let out = "fatal: unable to access 'https://github.com/x/y/': Could not resolve host: github.com\n\n[exit code 128]";
            Box::pin(async move { Ok(ok(out, c.id.clone())) })
        });
        FakeRunner { run, parallel: |_| false }
    }
    const CLONE: &str = "git clone https://github.com/x/y";

    #[tokio::test]
    async fn a_command_failing_the_same_way_with_another_tail_each_time_warns_then_stops() {
        let tails = ["2>&1 | tail -2", "&& du -sh y", "|| true", "&& command -v tokei", "; ls"];
        let mut script: Vec<_> =
            tails.iter().enumerate().map(|(i, t)| use_tools(&[bash_call(&format!("c{i}"), &format!("{CLONE} {t}"))])).collect();
        // A probe in between doesn't hide it.
        script.insert(1, use_tools(&[bash_call("p", "echo test")]));
        let ran = Arc::new(AtomicUsize::new(0));
        let history = history(vec![user("go")]);
        let events = collect(options(Arc::new(ScriptedProvider::new(script)), &history, offline(ran.clone()))).await;
        assert_eq!(tool_loops(&events), [true, false]);
        assert_eq!(end(&events), Some(StopReason::Loop));
        // The two clones and the probe ran; the third clone got the note, the fourth stopped the run.
        assert_eq!(ran.load(Ordering::SeqCst), 3);
        let turns = history.lock().unwrap();
        let noted = turns.iter().any(|t| {
            matches!(t, ChatTurn::Tool { text, .. } if text.starts_with("Marv: not run. This command already failed 2 times"))
        });
        assert!(noted, "{turns:?}");
    }

    // Greptile on #111: each call is checked when it's about to run, after the calls before it in the reply.
    #[tokio::test]
    async fn an_edit_earlier_in_the_same_reply_resets_the_failures_before_the_retry_runs() {
        let edit = ToolCall {
            id: "e".into(),
            name: "edit_file".into(),
            arguments: serde_json::json!({"path": "a.rs", "old": "x", "new": "y"}).to_string(),
        };
        let script = vec![
            use_tools(&[bash_call("c1", "cargo test")]),
            use_tools(&[bash_call("c2", "cargo test")]),
            use_tools(&[edit, bash_call("c3", "cargo test")]),
            say("Done."),
        ];
        let (bash_runs, edits) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (b, e) = (bash_runs.clone(), edits.clone());
        let run: RunFn = Arc::new(move |c, _| {
            let out = if c.name == "bash" {
                b.fetch_add(1, Ordering::SeqCst);
                "error[E0308]: mismatched types\n\n[exit code 101]"
            } else {
                e.fetch_add(1, Ordering::SeqCst);
                "Edited a.rs"
            };
            Box::pin(async move { Ok(ok(out, c.id.clone())) })
        });
        let runner = FakeRunner { run, parallel: |_| false };
        let events = collect(options(Arc::new(ScriptedProvider::new(script)), &history(vec![user("go")]), runner)).await;
        assert!(tool_loops(&events).is_empty());
        assert_eq!((edits.load(Ordering::SeqCst), bash_runs.load(Ordering::SeqCst)), (1, 3));
        assert_eq!(end(&events), Some(StopReason::End));
    }

    #[tokio::test]
    async fn re_running_a_failing_command_after_an_edit_isnt_a_loop() {
        let write = ToolCall {
            id: "w".into(),
            name: "write_file".into(),
            arguments: serde_json::json!({"path": "a.rs", "content": "fixed"}).to_string(),
        };
        let script = vec![
            use_tools(&[bash_call("c1", CLONE)]),
            use_tools(&[bash_call("c2", CLONE)]),
            use_tools(&[write]),
            use_tools(&[bash_call("c3", CLONE)]),
            say("Done."),
        ];
        let ran = Arc::new(AtomicUsize::new(0));
        let events =
            collect(options(Arc::new(ScriptedProvider::new(script)), &history(vec![user("go")]), offline(ran.clone()))).await;
        assert!(tool_loops(&events).is_empty());
        assert_eq!(end(&events), Some(StopReason::End));
        assert_eq!(ran.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn rewrite_call_changes_a_call_before_it_enters_the_history_and_it_runs_as_rewritten() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            use_tools(&[ToolCall { id: "c1".into(), name: "mcp__x__ping".into(), arguments: r#"{"a":1}"#.into() }]),
            say("Done."),
        ]));
        let ran = Arc::new(Mutex::new(Vec::new()));
        let record = ran.clone();
        let run: RunFn = Arc::new(move |c, _| {
            record.lock().unwrap().push(c.name.clone());
            Box::pin(async move { Ok(ok("ok", c.name.clone())) })
        });
        let rewrite: RewriteCall = Arc::new(|c| {
            (c.name == "mcp__x__ping").then(|| ToolCall {
                name: "mcp_call".into(),
                arguments: format!(r#"{{"tool":"mcp__x__ping","arguments":{}}}"#, c.arguments),
                ..c.clone()
            })
        });
        collect(AgentOptions {
            rewrite_call: Some(rewrite),
            tools: vec![],
            ..options(provider.clone(), &history(vec![user("ping")]), FakeRunner { run, parallel: |_| false })
        })
        .await;
        assert_eq!(*ran.lock().unwrap(), ["mcp_call"]);
        assert_eq!(
            provider.requests()[1].0[1],
            ChatTurn::Assistant {
                text: "".into(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "mcp_call".into(),
                    arguments: r#"{"tool":"mcp__x__ping","arguments":{"a":1}}"#.into()
                }],
                reasoning: None
            }
        );
    }
}
