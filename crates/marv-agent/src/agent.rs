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
//!   calls are answered and the run ends `Loop`.
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
//!
//! marv-rs changes and planned work:
//! - **#58 parallel read-only tools**: Marv runs only consecutive `agent` calls together
//!   (`isParallel`). Here [`ToolRunner::concurrent`] decides, defaulting to `ToolKind::Read`; the
//!   session also returns true for `agent`. Anything else is a barrier.
//! - **#57 reasoning round-trip**: a `Reasoning` event is kept on the assistant turn.
//! - **#59 wrap-up at the step limit**: with `wrap_up_at_limit`, a hard stop first runs one more
//!   request with [`STEP_LIMIT_NOTE`] and `ToolChoice::None` (tools still sent: the cache holds);
//!   any call it makes anyway is answered "Not run". The stop reason stays `MaxSteps`.
//! - **#64 continue after a dropped stream**: a `Retry` with `continued: true` keeps the partial
//!   text as the assistant turn and sends [`DROPPED_STREAM_NOTE`] instead of starting over (needs a
//!   `continued` flag on `marv_core::ProviderEvent::Retry`, see `docs/subsystems/agent.md`).
//! - The text-loop check position is reset on `Retry` too (Marv forgot it, so after a retry the
//!   check could lag by up to the discarded length).

use std::collections::{HashSet, VecDeque};
use std::ops::Range;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::Stream;
use futures::future::BoxFuture;
use marv_core::{
    ChatTurn, MarvError, Provenance, ProviderEvent, Reasoning, StopReason, ToolCall, ToolKind, ToolResult, ToolSpec, Usage,
};
use marv_provider::Provider;
use tokio_util::sync::CancellationToken;

use crate::repetition::{ToolLoopState, detect_text_repetition};

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

/// Marv #59 (planned): the wrap-up request's note at a hard step limit.
pub const STEP_LIMIT_NOTE: &str = "(Automatic note from Marv, the program running you) You've reached the step limit. Don't call tools. Reply with what you finished, what's left, and the next step.";

/// Marv #64 (planned): sent after a partial reply kept from a dropped stream.
pub const DROPPED_STREAM_NOTE: &str = "(Automatic note from Marv, the program running you) The previous response was interrupted. Continue from where you left off.";

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
    /// Display-only payload the loop never reads (marv-tools' `DiffShown`); the UI downcasts it.
    /// Never sent to the model.
    pub display: Option<Arc<dyn std::any::Any + Send + Sync>>,
}

impl ToolRun {
    /// A plain result with no extras.
    pub fn new(result: ToolResult, label: impl Into<String>) -> Self {
        Self { result, label: label.into(), declined: false, provenance: Provenance::Project, changed: None, display: None }
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
    Usage(Usage),
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
    /// The same tool calls repeated without progress: the model was told (once), or the run stopped.
    ToolLoop {
        continued: bool,
    },
    /// A mid-run compaction swapped the history for the summary (`on_full`).
    Compacted,
    Done(StopReason),
    /// The connection was lost and the provider is retrying. `continued` (#64): the partial reply
    /// was kept and the model asked to continue, instead of starting over.
    Retry {
        attempt: u32,
        max: u32,
        message: String,
        continued: bool,
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
    pub after_step: Option<AfterStep>,
    /// #59 (planned): one tools-off wrap-up reply before a hard stop at the step limit.
    pub wrap_up_at_limit: bool,
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
            after_step: None,
            wrap_up_at_limit: false,
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
            ProviderEvent::Usage(usage) => Some(LoopEvent::Usage(usage)),
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
            ProviderEvent::Retry { attempt, max, message, continued } => {
                // Retrying from scratch: discard this attempt's partial text, calls and reasoning so
                // the retry doesn't duplicate them. A continued retry (#64) keeps the partial text:
                // the next attempt appends to it.
                if !continued {
                    self.text.clear();
                    self.checked_at = 0;
                    self.reasoned = false;
                    self.reasoning = None;
                    self.calls.clear();
                }
                Some(LoopEvent::Retry { attempt, max, message, continued })
            }
        }
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

/// The loop's state between pulls. A resumable state machine instead of Marv's generator:
/// - `Request`: stream one reply into a [`StepReply`] (forwarding events through `queue`), then
///   decide its [`StepOutcome`];
/// - `Tools`: for each group from [`call_groups`], reserve up to `max_parallel` calls, push their
///   `ToolStart`s, and only launch a call once its `ToolStart` was pulled; collect `ToolEnd`s as
///   they finish (a `FuturesUnordered`), append results in call order when the group is done;
/// - `AfterTools`: `on_full`, then `after_step`, then the step limit, then `Request` again.
pub struct LoopState {
    options: AgentOptions,
    queue: VecDeque<LoopEvent>,
    finished: bool,
    step: u32,
    used_ids: HashSet<String>,
    /// Replies cut off at the output limit in a row.
    cut_offs: u32,
    /// Empty replies in a row.
    empties: u32,
    /// Text loops in a row.
    text_loops: u32,
    tool_loop: ToolLoopState,
    /// (tool, normalized args) → hash of what it returned last time.
    last_result: std::collections::HashMap<(String, String), u64>,
    in_flight: InFlight,
}

impl LoopState {
    pub fn new(options: AgentOptions) -> Self {
        let used_ids = used_ids(&options.history.lock().unwrap_or_else(|e| e.into_inner()));
        Self {
            options,
            queue: VecDeque::new(),
            finished: false,
            step: 0,
            used_ids,
            cut_offs: 0,
            empties: 0,
            text_loops: 0,
            tool_loop: ToolLoopState::default(),
            last_result: Default::default(),
            in_flight: InFlight::default(),
        }
    }

    fn push_turn(&self, turn: ChatTurn) {
        self.options.history.lock().unwrap_or_else(|e| e.into_inner()).push(turn);
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

    /// Runs the state machine until it has queued at least one event or finished.
    async fn advance(&mut self) {
        let _ = (self.step, &self.used_ids, self.cut_offs, self.empties, self.text_loops, &self.tool_loop, &self.last_result);
        todo!("port: runAgent's body as the Request / Tools / AfterTools state machine (see LoopState docs)")
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
    fn a_retry_resets_the_steps_text_and_calls() {
        let (reply, events) = fold(vec![
            ProviderEvent::TextDelta("Half".into()),
            ProviderEvent::ToolCall(call("c1", "a")),
            ProviderEvent::Retry { attempt: 1, max: 2, message: "retrying".into(), continued: false },
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
}
