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
//!   the run ends `Loop` (the step's remaining calls not run) the next. **marv-rs changes this** (below).
//! - **Repeated invalid calls** (marv-rs, #267): a call that failed validation (`ToolRun::invalid`: its arguments
//!   aren't JSON or the tool's parse refused them, so it never ran) is remembered by tool and normalized arguments.
//!   An identical call refused for the same reason is answered with [`refused_call_note`] from its 2nd try, not the
//!   tool-loop guard's 4th: a validation error is deterministic. Nothing is skipped or stopped; the #37 guard still
//!   backs it, and calls that ran are left to it.
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
//! - **"Send now"** (`send_now`, [`crate::send_now`]; new in marv-rs): the person's messages sent while the run
//!   goes go in as one user message (joined by blank lines) at the next step boundary: at the top of a step that
//!   goes on to an ordinary request (after `after_step`'s note, so the person has the last word; at the step limit
//!   only after a "continue"), or in place of ending the run on a plain answer (one more step reads them, unless
//!   that step would be at the limit). Only where every call has its result ([`crate::send_now::at_step_boundary`]).
//!   Never two user turns in a row: a user turn no request has carried yet (`sent_len`) takes them after a blank
//!   line; one a request already carried (an empty reply's retry resends it) is never rewritten, so they wait. Each
//!   yields a `SentNow` once it's in the history. Nothing is stopped for one. Not during a cancel, and not after the
//!   run ended: the session hands those back.
//! - Marv #63 **overflow recovery**: a reply that failed because the request was longer than the window
//!   (`marv_provider::errors::is_context_overflow`) goes to `on_overflow` when it's set: nothing of the failed reply
//!   enters the history, an [`LoopEvent::Overflow`] replaces the `Error`, and `Recovered` sends the same step again
//!   (the session made the history fit). At most [`MAX_OVERFLOW_RECOVERIES`] a step, so at most 5 requests; the count
//!   resets after a step that got a reply. `Failed(text)` ends `Error` with the hook's text, `Stopped` (or a cancel
//!   during the hook) ends `Aborted`. Unset, an overflow ends the run like any provider error.
//!
//! marv-rs changes and planned work:
//! - **Tool loops get a final turn** (marv-rs; Terminal-Bench 0.15.0: two runs that had the fix, or a working
//!   pipeline and 92 minutes left, were ended for re-sending `read_file {"path": …}` without the `offset` their own
//!   reasoning named). The note now echoes the exact call (tool and arguments, cut at [`LOOP_ARGS_SHOWN`] chars) and
//!   what's missing when that can be told ([`tool_loop_note`]: `read_file` without `offset`). A loop after the
//!   warning isn't the end any more: its calls get [`tool_loop_final_note`] (not run; one final turn to save the
//!   deliverable and answer, tools still allowed), and only a loop after that ends the run `Loop`. The failing-command
//!   guard escalates the same way. Calls not run when the run ends get [`not_run::TOOL_LOOP`], not the false
//!   [`not_run::CUT_OFF`] Marv uses. The warning is forgotten after progress
//!   ([`crate::repetition::TOOL_LOOP_PROGRESS`] distinct calls that ran and succeeded), so an early warning no longer
//!   makes a later, unrelated loop end the run unwarned; at most [`crate::repetition::MAX_TOOL_LOOP_RESCUES`] final
//!   turns a run keep the hard stop ([`crate::repetition::ToolLoopState::escalate`]).
//! - **A reply that degenerates twice** (marv-rs, #96 follow-up; Terminal-Bench 0.15.0 `gcode-to-text`: rendered
//!   `#` art copied into the reasoning ended the run with 8 minutes left and nothing written). The provider asks once
//!   more itself, then gives up with a `Discard` (cause `Degenerate`) and an `Error`. The loop treats that pair as
//!   [`StepOutcome::Degenerate`]: nothing of the reply enters the history, a `Retry` event (cause `Degenerate`) says
//!   so, and the conversation goes on with [`DEGENERATE_NOTE`], up to [`MAX_DEGENERATE_RECOVERIES`] times a turn (as
//!   with reasoning-only cut-offs); past that the error ends the run as before.
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
//! - **Replies cut off while only reasoning** (marv-rs): at effort max a reply can spend its whole output budget
//!   reasoning and do nothing; that's continued up to [`MAX_REASONING_CUT_OFFS`] times in a row (with
//!   [`CUT_OFF_REASONING_NOTE`]), any other cut-off once, as before. With the provider's bigger cap at effort
//!   `xhigh`/`max` (`openai_compat::max_tokens_for_reasoning`), this took 4 of 21 failed Terminal-Bench 2.1 runs.
//! - **Unasked self-summary, no tool call** (marv-rs): a reply in a compaction summary's shape ("<analysis>\nLet me
//!   work through the conversation chronologically…") when nothing was compacted ends the run halfway: DeepSeek V4.1
//!   Flash did it in 9 of 46 failed Terminal-Bench runs and none of 36 passes. The loop tells the model so
//!   ([`crate::announce::SELF_SUMMARY_NOTE`]) and goes on, up to three times a turn, logged as an `AnnouncedStep`.
//!   Past that the run ends (never also continued as an announced step).
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
//! - **Run deadline** (marv-rs, Terminal-Bench 0.15.0): `deadline` (`marv -p --time-limit`) used to reach only the
//!   tools (bash trims each command to it), so five benchmark runs kept reasoning or calling tools past "time left: 0m
//!   00s" and were killed from outside before they saved their work or `marv -p` printed its result. Now the loop
//!   owns it: (1) every result of a call that ran ends with [`time_left_note`] (it was bash's alone); (2) at the top of
//!   the first step within [`deadline_margin`] of the deadline, [`deadline_note`] goes into the history once (a user
//!   message from Marv, like the step limit's note; tools stay on, because saving the deliverable takes a tool call);
//!   (3) at the deadline the run's own stop (a child of `cancel`) fires: a reply still streaming is dropped (its text so
//!   far is kept, its calls answered [`not_run::TIME_LIMIT`]), running tools see a stop as for ctrl+c, and the run
//!   ends with an `Error` ([`TIME_LIMIT_MESSAGE`]) and `Done(TimeLimit)`, so the session and `marv -p` finish cleanly.
//!   A person's own stop still ends `Aborted`. The note is appended, never edited in; the system prompt is untouched.
//!   **The note reaches a reply still reasoning** (marv-rs, Terminal-Bench 2026-11-06 `dna-insert`: a request started
//!   317 s before the deadline reasoned until it, so the note, checked only at the top of a step, never went in). While
//!   a reply streams, the start of the margin is watched too ([`LoopState::wrap_up_at`]): if by then the reply has
//!   only reasoned (no text, no tool call started: [`StepReply::only_reasoning_so_far`]), that request alone is
//!   stopped (each request has its own child of the run's stop), the note goes in, and the step is asked again.
//!   Chosen over capping the request (`max_tokens` from the time left) because tokens don't map to seconds (provider
//!   speed varies several-fold), and because this is sound: nothing of a reply enters the history before it ends, so
//!   the dropped one leaves no trace, and the next request is the last one plus the note (the system prompt and tools
//!   unchanged). The note joins a trailing user turn as every note does (never two user turns in a row), so the only
//!   bytes that change are that last turn's, which the dropped request carried but never got a reply to; the cached
//!   prefix before it holds. A reply already writing text or a call is left to finish (dropping it would lose the
//!   deliverable it may be saving); the deadline itself still stops it. Not for the step limit's tools-off wrap-up.
//! - **Retry after a reasoning-only cut-off reasons less** (marv-rs, same run: three replies in a row spent the whole
//!   64k-token budget reasoning, about 400 s each, and each retry re-derived the same reasoning, which is discarded).
//!   The requests after one go with `StreamOptions::effort_cap` = [`CUT_OFF_RETRY_EFFORT`] (`high`), and after a
//!   second in a row [`CUT_OFF_LAST_RETRY_EFFORT`] (`low`), until a step finishes: the provider lowers an effort it
//!   sends above that (OpenRouter's `reasoning.effort`, the Responses API's); it never adds one. The output cap stays:
//!   a first version also dropped the 64k deep-reasoning cap to 32k, and `regex-chess` (5/5 before) then reasoned past
//!   32k at `high` three times in a row and the run ended at step 5 with 53 minutes left (TB 2026-11-06, the second
//!   k=1 run). Effort isn't part of the prompt, so the cached
//!   prefix holds. [`CUT_OFF_REASONING_NOTE`] still says to reason briefly. Carrying the discarded reasoning as a note
//!   was not chosen: it would put up to 64k tokens of unvetted text into the history for good.
//! - **Notes are logged** (marv-rs; audits of the same run had to infer them): every note Marv gives the model is also
//!   a [`LoopEvent::Note`] with its [`NoteKind`] and exact text: the deadline note and each tool result's time-left
//!   footer, the tool-loop and failing-command guards' notes, a read given the next page, and the cut-off, empty-reply,
//!   text-loop, degenerate, step-limit, announced-step, self-summary and `after_step` notes. Only an event (stream-json
//!   `note`, the trajectory's `note` record): what the model sees is unchanged.
//! - **Read-only loops and repeated truncated reads** (marv-rs, same run, `schemelike-metacircular-eval` ended 22 s
//!   into 40 minutes with nothing written; `sam-cell-seg` the same way): a loop whose calls all only read is warned
//!   about up to [`crate::repetition::MAX_READ_ONLY_LOOP_WARNINGS`] times before the final turn and the stop; the same
//!   `read_file` again after a truncated page is run with the next page's offset (for the runner only: the history keeps
//!   the call's own bytes) and its result starts with [`read_on_note`] ([`crate::repetition::ReadPages`]). read_file's
//!   own text stays Marv's (`Use offset=N to read more.` ends every truncated page already).
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

pub use crate::announce::{
    ANNOUNCED_STEP_NOTE, MAX_SELF_SUMMARY_CONTINUES, SELF_SUMMARY_NOTE, announces_next_step, summarizes_itself,
};
use crate::repetition::{
    FailureLoopState, LoopVerdict, ReadPages, ToolCallKey, ToolLoopState, detect_text_repetition, failing_command_note,
    make_tool_key, only_reads, read_on_note, tool_loop_detected, with_offset,
};
pub use crate::send_now::{SendNow, at_step_boundary};

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
    /// marv-rs: a call not run because the tool-loop guard ended the run (Marv answers it with [`CUT_OFF`], which
    /// isn't what happened).
    pub const TOOL_LOOP: &str = "Not run: Marv ended the run because the same calls kept repeating without progress.";
    /// #59: a call the wrap-up reply made anyway (it was asked not to).
    pub const STEP_LIMIT: &str = "Not run: the step limit was reached.";
    /// marv-rs: a call left when the run's time limit (`AgentOptions::deadline`) ended it.
    pub const TIME_LIMIT: &str = "Not run: the run's time limit was reached.";
    /// #202: a call that waited for approval when Marv quit (a crash, a force-quit): the saved session answers it so.
    pub const QUIT_APPROVAL: &str = "Not run: Marv quit while this call waited for approval.";
    /// #202: a call with no result when Marv quit: it may not have started, or stopped part-way.
    pub const QUIT: &str = "No result: Marv quit before this call finished, so it may not have run, or only in part.";
}

/// Told to the model (a user message from Marv) when a reply with nothing to run was cut off at
/// the output limit: without it, the model would see only its own unfinished reply, or nothing
/// (Ollama drops a tool call it was still writing), and would likely try the same thing again.
pub const CUT_OFF_NOTE: &str = "(Automatic note from Marv, the program running you) Your last reply hit the output limit and was cut off before it finished; nothing in it was run. Continue in smaller steps (for a big file: write_file the first part, then add the rest with append: true).";

/// The same for a reply that reasoned and left nothing: it reasoned until the limit or started a
/// tool call that was cut off and dropped; Marv can't tell which.
/// How many replies in a row may be cut off while still only reasoning (no text, no call) before the run ends: deep
/// reasoning (effort max) can fill a reply's whole budget and do nothing, and a second try usually acts. Any other
/// cut-off is continued once, as before.
pub const MAX_REASONING_CUT_OFFS: u32 = 3;

pub const CUT_OFF_REASONING_NOTE: &str = "(Automatic note from Marv, the program running you) Your last reply used up the output limit, while reasoning or while writing a tool call (an unfinished call is dropped), so it did nothing. Reason briefly, and write a big file in parts (write_file the first part, then add the rest with append: true).";

/// marv-rs: the most reasoning effort the request after a reply cut off while only reasoning may use
/// (`StreamOptions::effort_cap`), until a step finishes. `high`: nearly every reasoning model takes it (DeepSeek V4.1
/// Flash takes max, high and low), and below `xhigh` the provider's output cap drops back from 64k to 32k tokens.
pub const CUT_OFF_RETRY_EFFORT: marv_provider::ReasoningEffort = marv_provider::ReasoningEffort::High;

/// marv-rs: the effort cap after a second reasoning-only cut-off in a row: `high` can still reason past the whole
/// budget (`regex-chess`, TB 2026-11-06), and the next one would end the run.
pub const CUT_OFF_LAST_RETRY_EFFORT: marv_provider::ReasoningEffort = marv_provider::ReasoningEffort::Low;

/// Told to the model after a second empty reply in a row. The first is asked for again unchanged:
/// upstream hosts sometimes bill for a reply and send nothing, and a resample usually comes back
/// whole. A second suggests the model itself stopped.
pub const EMPTY_REPLY_NOTE: &str = "(Automatic note from Marv, the program running you) Your last reply was empty: no text and no tool calls. If the task isn't finished, continue it with your tools; if it is, say what you did.";

/// Told to the model when its reply got stuck repeating one sentence. The reply is dropped entirely.
pub const TEXT_LOOP_NOTE: &str = "(Automatic note from Marv, the program running you) Your last reply got stuck repeating one sentence and was stopped; nothing in it was used. Take the next concrete step.";

/// How many characters of a repeated call's arguments its loop note shows.
pub const LOOP_ARGS_SHOWN: usize = 300;

/// The call as the model sent it, its arguments cut at [`LOOP_ARGS_SHOWN`] chars (never re-serialized).
fn shown_call(name: &str, arguments: &str) -> String {
    let arguments = arguments.trim();
    match arguments.char_indices().nth(LOOP_ARGS_SHOWN) {
        Some((cut, _)) => format!("{name} {}…", &arguments[..cut]),
        None => format!("{name} {arguments}"),
    }
}

/// What the repeated call lacks to get something new, when that can be told from it.
fn loop_hint(name: &str, arguments: &str) -> &'static str {
    let args = serde_json::from_str::<serde_json::Value>(arguments).ok();
    let has = |field: &str| args.as_ref().and_then(|a| a.get(field)).is_some_and(|v| !v.is_null());
    if name == "read_file" && !has("offset") {
        return " You sent no offset, so it returns the same first lines every time: to read further, pass offset (the result says which line comes next).";
    }
    ""
}

/// The result of each repeated call when tool calls repeat without progress. marv-rs: shows the exact call, and what
/// it lacks when that can be told (TB 0.15.0: `read_file` resent without the `offset` the model meant to pass).
pub fn tool_loop_note(repeat_count: usize, name: &str, arguments: &str) -> String {
    format!(
        "Marv: you have made this exact call {repeat_count} times in a row and it returned the same result: `{}`. Not run again.{} Use what it returned, or change the call.",
        shown_call(name, arguments),
        loop_hint(name, arguments),
    )
}

/// The final-turn note appended to a looping call's result after the warning (marv-rs).
pub const FINAL_TURN_NOTE: &str = "You were warned and sent it again, so this is your final turn: save your work now (write the files or the report the task asks for) and give your final answer. Tools still work, but repeating this call again ends the run.";

/// marv-rs: the result of each call of a loop after the warning: not run, and one final turn.
pub fn tool_loop_final_note(repeat_count: usize, name: &str, arguments: &str) -> String {
    format!(
        "Marv: not run. You have made this exact call {repeat_count} times in a row and it returned the same result: `{}`.{} {FINAL_TURN_NOTE}",
        shown_call(name, arguments),
        loop_hint(name, arguments),
    )
}

/// marv-rs: how many times a turn the loop goes on after the provider gave up on a degenerate reply (#96).
pub const MAX_DEGENERATE_RECOVERIES: u32 = 2;

/// marv-rs: told to the model when the provider gave up on its reply for degenerating twice (#96).
pub const DEGENERATE_NOTE: &str = "(Automatic note from Marv, the program running you) Your last reply degenerated into repeated text (whitespace, or the same short piece over and over) and was cut; nothing in it was used. Don't reproduce rendered or bulk output (ASCII art, grids, long data) in your reply or your reasoning: analyse it with code and print short summaries. Keep going with the task.";

/// marv-rs #267: the result of a call identical to one that failed validation the same way earlier in the run. A
/// validation error is deterministic, so the #37 guard's four tries were three too many (the ChatGPT provider resent
/// one `herdr` `list` call refused for `branch, base and task: only for action spawn` until that guard stopped it).
pub fn refused_call_note(reason: &str) -> String {
    let reason = reason.trim().trim_end_matches('.');
    format!(
        "Marv: not run. This exact call was refused for: {reason}. Change the arguments (only send the fields this action uses), or tell the user it can't be done."
    )
}

/// Marv #59: the wrap-up request's note at a hard step limit.
pub const STEP_LIMIT_NOTE: &str = "(Automatic note from Marv, the program running you) You've reached the step limit. Don't call tools. Reply with what you finished, what's left, and the next step.";

/// The most time before the run's deadline at which the model is told to finish ([`deadline_margin`]).
pub const DEADLINE_WRAP_UP_MAX: std::time::Duration = std::time::Duration::from_secs(90);

/// How long before the deadline [`deadline_note`] goes in, for a run that had `budget` when it started: a quarter of
/// it, at most [`DEADLINE_WRAP_UP_MAX`]. 90 s is about two steps of a slow reasoning model (one reply that saves the
/// deliverable, one final reply), and bash already ends each command 30 s before the deadline
/// (`marv_tools::bash::DEADLINE_MARGIN_S`), so a command started just before the note still leaves time to read its
/// output. A quarter keeps a short limit (2 minutes: 30 s) mostly for the work itself.
pub fn deadline_margin(budget: std::time::Duration) -> std::time::Duration {
    (budget / 4).min(DEADLINE_WRAP_UP_MAX)
}

/// marv-rs: the note at [`deadline_margin`] before the deadline, once a run. Tools stay offered: writing the
/// deliverable is a tool call, and a reply that can't make one can't save anything.
pub fn deadline_note(left: std::time::Duration) -> String {
    format!(
        "(Automatic note from Marv, the program running you) About {} s are left before the time limit stops this run. Finish now: if your result isn't saved yet, make the one or two tool calls that save it, then give your final reply. Don't start anything new.",
        left.as_secs()
    )
}

/// marv-rs: the `Error` the run ends with at its deadline (`Done(TimeLimit)` follows).
pub const TIME_LIMIT_MESSAGE: &str = "Stopped at the time limit: the run's time ran out before it finished.";

/// What each result of a call that ran ends with while the run has a deadline: the time left, so the model can plan
/// the rest (it was bash's alone; now every tool's). Empty without a deadline.
pub fn time_left_note(deadline: Option<std::time::Instant>) -> String {
    match deadline {
        Some(at) => {
            let left = at.saturating_duration_since(std::time::Instant::now()).as_secs();
            format!("\n[time left in this run: {}m {:02}s]", left / 60, left % 60)
        }
        None => String::new(),
    }
}

/// Marv #64: sent after a partial reply kept from a dropped stream. The provider owns the continue
/// request (it owns retries), so the text lives there; re-exported for the loop's tests and docs.
pub use marv_provider::openai_compat::DROPPED_STREAM_NOTE;

/// A copy of the history in which every tool call has a result: calls still waiting at the end
/// (a run cut short, e.g. by quitting while it wound down) get "not run". Without one the API
/// rejects every request made from this history, so a session saved this way couldn't continue.
pub fn answer_all_calls(history: &[ChatTurn]) -> Vec<ChatTurn> {
    answer_all_calls_with(history, |_| not_run::ABORTED.into())
}

/// [`answer_all_calls`] with each waiting call's own result: a session saved mid-turn (#202) gives a call that has
/// finished its output (a parallel group's results go into the history only once all of them are done), and one that
/// hasn't a "Marv quit" text.
pub fn answer_all_calls_with(history: &[ChatTurn], result: impl Fn(&ToolCall) -> String) -> Vec<ChatTurn> {
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
        .map(|c| ChatTurn::Tool { call_id: c.id.clone(), name: c.name.clone(), text: result(c) })
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
    /// The files a change wrote (project-relative; empty: none): which project checks/diagnostics `after_step` runs.
    pub changed: Vec<String>,
    /// How it was approved (trajectories, `marv -p`'s `tool_end` and refusals). Never sent to the model.
    pub approval: ApprovalRecord,
    /// The door out of the sandbox the call went through (Marv #47, #51, #52), for trajectories; None for an
    /// ordinary call.
    pub door: Option<marv_core::Door>,
    /// The call failed validation and the tool never ran (#267: marv-tools' `ToolOutput::invalid`): the reason. The
    /// same call will fail the same way, so the loop refuses an identical repeat with [`refused_call_note`].
    pub invalid: Option<String>,
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
            changed: Vec::new(),
            approval: ApprovalRecord::None,
            door: None,
            invalid: None,
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

/// marv-rs: which of Marv's own notes to the model a [`LoopEvent::Note`] logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteKind {
    /// [`deadline_note`]: the run's time is nearly up.
    Deadline,
    /// [`time_left_note`]: the footer after a tool's result.
    TimeLeft,
    /// The tool-loop or failing-command guard's note, the result of the calls it didn't run.
    ToolLoop,
    /// [`read_on_note`]: a repeated truncated read was given the next page.
    ReadOn,
    CutOff,
    EmptyReply,
    TextLoop,
    Degenerate,
    StepLimit,
    AnnouncedStep,
    SelfSummary,
    /// `after_step`'s note (the typecheck, the project's checks).
    AfterStep,
}

impl NoteKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deadline => "deadline",
            Self::TimeLeft => "time_left",
            Self::ToolLoop => "tool_loop",
            Self::ReadOn => "read_on",
            Self::CutOff => "cut_off",
            Self::EmptyReply => "empty_reply",
            Self::TextLoop => "text_loop",
            Self::Degenerate => "degenerate",
            Self::StepLimit => "step_limit",
            Self::AnnouncedStep => "announced_step",
            Self::SelfSummary => "self_summary",
            Self::AfterStep => "after_step",
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
    /// "Send now": the person's message went into the history as a user message, at a step boundary.
    SentNow {
        text: String,
    },
    /// Marv #63: the request didn't fit the context window, and `on_overflow` is making it fit. `attempt` counts from 0
    /// within the step.
    Overflow {
        attempt: u32,
    },
    Done(StopReason),
    /// marv-rs: a note Marv gave the model, exactly as the model got it (a user message from Marv, a tool result's
    /// footer, or a guard's result), so logs show it without inferring it. Only a record: nothing is added for it.
    Note {
        kind: NoteKind,
        text: String,
    },
    /// The provider threw the reply away without retrying (#96: it degenerated again); an `Error` follows from the
    /// provider. marv-rs: the loop may then go on instead (a `Retry` with cause `Degenerate`, no `Error` event).
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
    /// The person's "send now" messages, appended at the next step boundary ([`crate::send_now`]). The session sets
    /// it for the main agent only.
    pub send_now: Option<SendNow>,
    /// marv-rs: when the run must be done (`marv -p --time-limit`): the time-left footer, the wrap-up note, and the
    /// stop at the deadline (the module doc's "Run deadline"). None: no limit.
    pub deadline: Option<std::time::Instant>,
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
            send_now: None,
            deadline: None,
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
    /// The provider gave up on the reply for degenerating (#96): a `Discard` with cause `Degenerate`.
    pub degenerated: bool,
    checked_at: usize,
    /// marv-rs: text or a tool call has started streaming (the reply is acting, not only reasoning).
    wrote: bool,
}

impl StepReply {
    /// Folds one provider event in; returns what to pass on to the consumer, if anything. After
    /// it sets [`StepReply::looped`] the caller stops reading the stream (the delta that tripped
    /// it isn't passed on).
    pub fn apply(&mut self, event: ProviderEvent) -> Option<LoopEvent> {
        match event {
            ProviderEvent::TextDelta(text) => {
                self.wrote = true;
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
            ProviderEvent::ToolCallDelta { index, name, text } => {
                self.wrote = true;
                Some(LoopEvent::ToolCallDelta { index, name, text })
            }
            ProviderEvent::ToolCall(call) => {
                self.wrote = true;
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
                self.degenerated |= matches!(cause, marv_core::RetryCause::Degenerate { .. });
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
        self.wrote = false;
    }

    /// Nothing but reasoning has streamed so far (or nothing at all): dropping it loses no text and no call.
    pub fn only_reasoning_so_far(&self) -> bool {
        !self.wrote
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
            if self.degenerated && !aborted {
                return StepOutcome::Degenerate(error.clone());
            }
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
    /// marv-rs: the provider gave up on the reply for degenerating (a `Discard`, then this error): nothing of it
    /// enters the history, and the loop goes on with [`DEGENERATE_NOTE`] (up to [`MAX_DEGENERATE_RECOVERIES`]).
    Degenerate(String),
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
        /// This request's own stop (a child of the run's), so the deadline's wrap-up can drop just it.
        request: CancellationToken,
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
    /// Degenerate replies the provider gave up on that this turn went on past (#96).
    degenerates: u32,
    /// #90: whether this run already continued an announced next step (once per turn: the
    /// first is rescued, a second announcement ends the run, still logged as an event).
    announced: bool,
    /// How many unasked self-summaries this turn continued past ([`summarizes_itself`]).
    summarized: u32,
    tool_loop: ToolLoopState,
    /// (tool, normalized args) → hash of what it returned last time. A call repeated with the same
    /// result is a loop; one whose result changes each time (the next item from a queue) is new work.
    last_result: std::collections::HashMap<(String, String), u64>,
    /// Bash commands failing the same way since the last edit (#81): another try is a loop, whatever its tail.
    /// Checked as each call is about to run, after the calls before it in the reply were recorded.
    failures: FailureLoopState,
    /// A failing command was tried again after the warning: the step's remaining calls don't run, the run ends `Loop`.
    failure_stop: bool,
    /// #267: (tool, normalized args) → why that call failed validation (the tool never ran). An identical call that
    /// fails for the same reason is answered with [`refused_call_note`]: the #37 guard waits for four identical
    /// results, but a validation error repeats for certain. Calls that ran stay with the #37 guard alone.
    invalid: std::collections::HashMap<(String, String), String>,
    in_flight: InFlight,
    /// The history's length when the last request was sent: a turn below it was carried by a request and is never
    /// rewritten (a "send now" message joins only a user turn above it).
    sent_len: usize,
    /// Marv #63: overflow recoveries in this step (`on_overflow`'s `attempt`); reset by a step that got a reply.
    overflows: u32,
    /// The caller's `cancel`. With a deadline, `options.cancel` is a child of it that the deadline fires too: a stop
    /// from here is a person's (`Aborted`), one only there is the time limit (`TimeLimit`).
    user_cancel: CancellationToken,
    /// How long before the deadline the wrap-up note goes in ([`deadline_margin`]).
    margin: std::time::Duration,
    /// The wrap-up note went in (once a run).
    deadline_warned: bool,
    /// The last reply was cut off while only reasoning: the next requests use at most [`CUT_OFF_RETRY_EFFORT`] until a
    /// step finishes.
    reasoning_cut_off: bool,
    /// A repeated truncated `read_file` reads on ([`crate::repetition::ReadPages`]).
    read_pages: ReadPages,
}

impl LoopState {
    pub fn new(mut options: AgentOptions) -> Self {
        let used_ids = used_ids(&options.history.lock().unwrap_or_else(|e| e.into_inner()));
        let user_cancel = options.cancel.clone();
        let mut margin = std::time::Duration::ZERO;
        if let Some(deadline) = options.deadline {
            // The run's own stop: the provider and the tools get it, so the deadline stops them as ctrl+c would.
            options.cancel = user_cancel.child_token();
            margin = deadline_margin(deadline.saturating_duration_since(std::time::Instant::now()));
        }
        Self {
            user_cancel,
            margin,
            deadline_warned: false,
            reasoning_cut_off: false,
            read_pages: ReadPages::default(),
            options,
            queue: VecDeque::new(),
            finished: false,
            phase: Phase::Top,
            step: 0,
            used_ids,
            cut_offs: 0,
            empties: 0,
            text_loops: 0,
            degenerates: 0,
            announced: false,
            summarized: 0,
            tool_loop: ToolLoopState::default(),
            last_result: Default::default(),
            failures: FailureLoopState::default(),
            failure_stop: false,
            invalid: Default::default(),
            in_flight: InFlight::default(),
            sent_len: 0,
            overflows: 0,
        }
    }

    fn push_turn(&self, turn: ChatTurn) {
        self.options.history.lock().unwrap_or_else(|e| e.into_inner()).push(turn);
    }

    /// Marv's notes go in as user turns, merged into a trailing one (a mid-turn compaction's note, the turn's own
    /// message): never two user turns in a row (#63 review).
    /// marv-rs: each is logged as a [`LoopEvent::Note`] too.
    fn push_note(&mut self, kind: NoteKind, text: &str) {
        crate::compact::push_user(&mut self.options.history.lock().unwrap_or_else(|e| e.into_inner()), text);
        self.emit(LoopEvent::Note { kind, text: text.to_string() });
    }

    fn push_result(&self, call: &ToolCall, text: &str) {
        self.push_turn(ChatTurn::Tool { call_id: call.id.clone(), name: call.name.clone(), text: text.to_string() });
    }

    fn aborted(&self) -> bool {
        self.options.cancel.is_cancelled()
    }

    /// The deadline passed and the person didn't stop the run themselves.
    fn timed_out(&self) -> bool {
        self.options.deadline.is_some_and(|at| std::time::Instant::now() >= at) && !self.user_cancel.is_cancelled()
    }

    /// What a call that won't run after a stop is answered: the time limit's text when that was the stop.
    fn stop_text(&self) -> &'static str {
        if self.timed_out() { not_run::TIME_LIMIT } else { not_run::ABORTED }
    }

    /// The wrap-up note, once, at the top of the first step within the margin of the deadline.
    fn warn_deadline(&mut self) {
        let Some(at) = self.options.deadline else { return };
        let left = at.saturating_duration_since(std::time::Instant::now());
        if !self.deadline_warned && left <= self.margin {
            self.deadline_warned = true;
            self.push_note(NoteKind::Deadline, &deadline_note(left));
        }
    }

    /// When a reply still streaming should be dropped for the wrap-up note: the start of the margin, while the note
    /// hasn't gone in, the reply has only reasoned so far, and it isn't the step limit's wrap-up (tools off already).
    fn wrap_up_at(&self, reply: &StepReply, wrap_up: bool) -> Option<std::time::Instant> {
        // No margin (a run with no time to speak of): the deadline itself is all there is.
        if wrap_up || self.deadline_warned || self.margin.is_zero() || !reply.only_reasoning_so_far() {
            return None;
        }
        self.options.deadline.and_then(|at| at.checked_sub(self.margin))
    }

    /// The margin began while the reply only reasoned: drop this request (and only it), put the wrap-up note in, and
    /// ask again. Nothing of the dropped reply reached the history, so the next request starts with the one before
    /// (plus the note).
    fn interrupt_for_deadline(&mut self, stream: EventStream, request: CancellationToken) {
        request.cancel();
        drop(stream);
        self.warn_deadline();
        self.phase = Phase::Request { tool_choice: ToolChoice::Auto, wrap_up: None };
    }

    fn emit(&mut self, event: LoopEvent) {
        self.queue.push_back(event);
    }

    fn finish(&mut self, reason: StopReason) {
        // A stop that only the deadline fired is the time limit, not the person.
        let reason = if reason == StopReason::Aborted && self.timed_out() {
            self.emit(LoopEvent::Error(TIME_LIMIT_MESSAGE.into()));
            StopReason::TimeLimit
        } else {
            reason
        };
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
        self.answer_with(call, text, "");
    }

    /// [`Self::answer`], with `footer` (the time left) after the text in the history only: the loop guards compare
    /// the tool's own text, which a ticking clock would make look new every time.
    fn answer_with(&mut self, call: &ToolCall, text: &str, footer: &str) {
        self.push_result(call, &format!("{text}{footer}"));
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
            // With a deadline, the run's stop fires at it, whatever this transition is waiting for (a reply, a tool,
            // a hook): it then winds down as after ctrl+c, and `finish` names the time limit.
            match self.options.deadline {
                Some(at) if !self.options.cancel.is_cancelled() => {
                    let stop = self.options.cancel.clone();
                    let advance = self.advance();
                    tokio::pin!(advance);
                    tokio::select! {
                        biased;
                        () = &mut advance => {}
                        () = tokio::time::sleep_until(at.into()) => {
                            stop.cancel();
                            advance.await;
                        }
                    }
                }
                _ => self.advance().await,
            }
        }
    }

    /// One transition of the state machine.
    async fn advance(&mut self) {
        match std::mem::replace(&mut self.phase, Phase::Top) {
            Phase::Top => self.top().await,
            Phase::Request { tool_choice, wrap_up } => {
                // The lock is never held across an await: the adapter serializes the snapshot at once.
                let snapshot = self.options.history.lock().unwrap_or_else(|e| e.into_inner()).clone();
                self.sent_len = snapshot.len();
                let request = self.options.cancel.child_token();
                let options = StreamOptions {
                    system: Some(self.options.system.clone()),
                    tools: self.options.tools.clone(),
                    tool_choice,
                    cancel: request.clone(),
                    debug_raw: self.options.debug_raw,
                    // After a reply that reasoned until the output limit, the retry reasons less (the module doc).
                    effort_cap: self.reasoning_cut_off.then_some(if self.cut_offs >= 2 {
                        CUT_OFF_LAST_RETRY_EFFORT
                    } else {
                        CUT_OFF_RETRY_EFFORT
                    }),
                };
                let stream = self.options.provider.stream(&snapshot, options);
                self.phase = Phase::Streaming { stream, reply: StepReply::default(), wrap_up, request };
            }
            Phase::Streaming { mut stream, mut reply, wrap_up, request } => {
                // The wrap-up time came while the reply has only reasoned: drop it and ask again with the note.
                let warn_at = self.wrap_up_at(&reply, wrap_up.is_some());
                if warn_at.is_some_and(|at| std::time::Instant::now() >= at) {
                    return self.interrupt_for_deadline(stream, request);
                }
                match next_provider_event(&mut stream, self.options.deadline, warn_at).await {
                    // The deadline came mid-reply: drop the request (a provider that doesn't watch the stop would
                    // otherwise keep the run going) and end with what streamed so far.
                    Next::Deadline => {
                        self.options.cancel.cancel();
                        drop(stream);
                        self.decide(reply, wrap_up);
                    }
                    Next::WrapUp => self.interrupt_for_deadline(stream, request),
                    Next::Event(Some(event)) => {
                        if let Some(out) = reply.apply(event) {
                            self.emit(out);
                        }
                        // A reply stuck repeating itself is stopped while it streams, not after it ran its course.
                        if reply.looped {
                            self.decide(reply, wrap_up);
                        } else {
                            self.phase = Phase::Streaming { stream, reply, wrap_up, request };
                        }
                    }
                    Next::Event(None) => self.decide(reply, wrap_up),
                }
            }
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
                    // A fresh history (the summary and a note): none of it was sent yet, and the pages read are gone.
                    self.sent_len = 0;
                    self.read_pages.clear();
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
                        self.push_note(NoteKind::AfterStep, &note);
                    }
                }
                self.next_step();
            }
        }
    }

    /// The step limit guards against a model that never finishes. With `on_limit`, the user decides
    /// at each multiple whether it goes on. `max_steps` 0 (the guard off) never matches.
    async fn top(&mut self) {
        if self.timed_out() {
            return self.finish(StopReason::Aborted);
        }
        self.warn_deadline();
        if !at_step_limit(self.step, self.options.max_steps) {
            // The step boundary: the person's "send now" messages go in before this step's request. Only before an
            // ordinary request: at the limit they wait for a "continue", or go back to the person with the turn.
            self.deliver_sent_now();
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
            // Including what was typed while the limit was asked.
            self.deliver_sent_now();
            self.phase = Phase::Request { tool_choice: ToolChoice::Auto, wrap_up: None };
            return;
        }
        let asked = answer == Some(LimitAnswer::Stop);
        let message = step_limit_message(self.step, asked);
        // #59: a hard stop first asks for a wrap-up, tools off (but still sent: the cache holds).
        if self.options.wrap_up_at_limit && !asked {
            self.push_note(NoteKind::StepLimit, STEP_LIMIT_NOTE);
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
                self.push_note(NoteKind::TextLoop, TEXT_LOOP_NOTE);
                self.next_step();
            }
            // The provider gave up on a reply that degenerated twice (#96): it was discarded, so nothing of it reaches
            // the history. Like a reasoning-only cut-off, the turn goes on with a note, a bounded number of times.
            StepOutcome::Degenerate(message) => {
                self.degenerates += 1;
                if self.degenerates > MAX_DEGENERATE_RECOVERIES {
                    self.emit(LoopEvent::Error(message));
                    return self.finish(StopReason::Error);
                }
                self.emit(LoopEvent::Retry {
                    attempt: self.degenerates,
                    max: MAX_DEGENERATE_RECOVERIES,
                    message: "the reply degenerated again and was thrown away; Marv told the model and goes on".into(),
                    continued: false,
                    cause: marv_core::RetryCause::Degenerate { discarded: 0 },
                });
                self.push_note(NoteKind::Degenerate, DEGENERATE_NOTE);
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
                        self.push_note(NoteKind::EmptyReply, EMPTY_REPLY_NOTE);
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
                        self.reasoning_cut_off = false;
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
                // The hook rewrote the history (as a mid-turn compaction does): none of it was carried by a request
                // that got through, so "send now" may join its last user turn until the resend below carries it.
                self.sent_len = 0;
                self.read_pages.clear();
                self.overflows += 1;
                let tool_choice = if wrap_up.is_some() { ToolChoice::None } else { ToolChoice::Auto };
                // The rewritten history may have lost the step limit's note: the wrap-up asks for it again.
                if wrap_up.is_some() {
                    let last = self.options.history.lock().unwrap_or_else(|e| e.into_inner()).last().cloned();
                    if !matches!(last, Some(ChatTurn::User { text }) if text.ends_with(STEP_LIMIT_NOTE)) {
                        self.push_note(NoteKind::StepLimit, STEP_LIMIT_NOTE);
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
        // A reply that was all reasoning did nothing: up to MAX_REASONING_CUT_OFFS in a row get another try.
        let only_reasoning = reply.reasoned && reply.text.is_empty() && reply.calls.is_empty();
        let continued = self.cut_offs == 1 || (only_reasoning && self.cut_offs <= MAX_REASONING_CUT_OFFS);
        self.reasoning_cut_off = only_reasoning && continued;
        // The history first, so it's valid even if the consumer stops at one of the events below.
        for call in &reply.calls {
            self.push_result(call, not_run::CUT_OFF);
        }
        // With calls, their results say it; with none, the model is told directly.
        if continued && reply.calls.is_empty() {
            self.push_note(NoteKind::CutOff, reply.cut_off_note());
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
        // The first time every call is answered with the note (showing the call) instead of running; after the
        // warning, with the final-turn note; after that the turn stops, every call answered (the history must be
        // valid) with what happened (Marv: the cut-off text, which misled the model).
        if hit {
            // A loop that only reads costs a step and changes nothing: warned about longer before it can end the run.
            let names: Vec<&str> = calls.iter().map(|c| c.name.as_str()).collect();
            let verdict = if only_reads(&names) { self.tool_loop.escalate_read_only() } else { self.tool_loop.escalate() };
            let repeats = self.tool_loop.history.len();
            let outputs: Vec<String> = calls
                .iter()
                .map(|call| match verdict {
                    LoopVerdict::Warn => tool_loop_note(repeats, &call.name, &call.arguments),
                    LoopVerdict::FinalChance => tool_loop_final_note(repeats, &call.name, &call.arguments),
                    LoopVerdict::Stop => not_run::TOOL_LOOP.to_string(),
                })
                .collect();
            for (call, output) in calls.iter().zip(&outputs) {
                self.push_result(call, output);
            }
            let continued = verdict != LoopVerdict::Stop;
            self.emit(LoopEvent::ToolLoop { continued });
            if let Some(first) = outputs.first() {
                self.emit(LoopEvent::Note { kind: NoteKind::ToolLoop, text: first.clone() });
            }
            for (call, output) in calls.into_iter().zip(outputs) {
                self.emit(LoopEvent::ToolSkipped { call, output });
            }
            return if continued { self.next_step() } else { self.finish(StopReason::Loop) };
        }
        if calls.is_empty() {
            // "Send now": a message that came while this reply streamed is read by one more step, rather than left
            // for after the turn. It's what the person said last, so it goes ahead of #90's "continue".
            // Not when the next step is at the step limit: then the turn ends and they go back to the person.
            if !aborted && self.can_deliver() && !at_step_limit(self.step + 1, self.options.max_steps) {
                return self.next_step();
            }
            // A reply that summarizes the conversation unasked (a compaction summary's shape, nothing compacted)
            // ends the run halfway too: up to MAX_SELF_SUMMARY_CONTINUES times a turn, say so and go on. Logged as an
            // announced step (the same kind of rescue; no new event for the clients to learn).
            if !aborted && summarizes_itself(&reply.text) {
                let continued = self.summarized < MAX_SELF_SUMMARY_CONTINUES;
                self.summarized += 1;
                self.emit(LoopEvent::AnnouncedStep { continued });
                if continued {
                    self.push_note(NoteKind::SelfSummary, SELF_SUMMARY_NOTE);
                    return self.next_step();
                }
                // Past the limit it ends the run: not also an announced step ("Let me work through…" reads as one).
                return self.finish(StopReason::End);
            }
            // #90: a plain answer that announces its next step ("Now checking issue #7 before
            // commenting:") is how a turn dies halfway. Once per turn, say "continue" like a
            // user would; a second announcement just ends the run, still logged.
            if !aborted && announces_next_step(&reply.text) {
                let continued = !self.announced;
                self.announced = true;
                self.emit(LoopEvent::AnnouncedStep { continued });
                if continued {
                    self.push_note(NoteKind::AnnouncedStep, ANNOUNCED_STEP_NOTE);
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

    /// "Send now": appends the person's waiting messages as user messages, if the history is at a step boundary
    /// (every call answered: never between a call and its result), and says so. True when any went in. Not while
    /// the run is being stopped: they stay waiting, and the session hands them back.
    fn deliver_sent_now(&mut self) -> bool {
        let Some(send_now) = self.options.send_now.clone() else { return false };
        if !self.can_deliver() {
            return false;
        }
        let texts = {
            let mut history = self.options.history.lock().unwrap_or_else(|e| e.into_inner());
            let texts = send_now.take();
            let joined = texts.join("\n\n");
            // Never two user turns in a row: a user turn that no request has carried yet (the turn's own message, a
            // check's note, a nudge) takes the person's words after a blank line. One already sent is never rewritten.
            match history.last_mut() {
                Some(ChatTurn::User { text }) => {
                    text.push_str("\n\n");
                    text.push_str(&joined);
                }
                _ => history.push(ChatTurn::User { text: joined }),
            }
            texts
        };
        for text in texts {
            self.emit(LoopEvent::SentNow { text });
        }
        true
    }

    /// Whether waiting "send now" messages can go in now: some wait, the run isn't being stopped, every call has its
    /// result ([`at_step_boundary`]), and the history doesn't end with a user turn a request already carried (the
    /// same request again after an empty reply: appending would make two user turns in a row, rewriting it would
    /// break the cache). Then they wait for the next boundary.
    fn can_deliver(&self) -> bool {
        let Some(send_now) = &self.options.send_now else { return false };
        if send_now.is_empty() || self.aborted() {
            return false;
        }
        let history = self.options.history.lock().unwrap_or_else(|e| e.into_inner());
        let sent_user = matches!(history.last(), Some(ChatTurn::User { .. })) && history.len() <= self.sent_len;
        at_step_boundary(&history) && !sent_user
    }

    /// One transition of running the step's calls. True when they're all answered and the phase moved on.
    async fn tools(&mut self, tools: &mut Tools) -> bool {
        // The abort first: Esc declines what's waiting *and* aborts, and that's an interrupt, not a "no".
        let stopped = if self.aborted() {
            Some(self.stop_text())
        } else if tools.declined {
            Some(not_run::DECLINED)
        } else if self.failure_stop {
            Some(not_run::TOOL_LOOP)
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
            // #81: a command that keeps failing the same way isn't run again: the note the first time, the final
            // turn the next, then the end of the run (the tool-loop guard's escalation, shared).
            if let Some((count, shown)) = self.failures.repeated(&call.name, &call.arguments) {
                let verdict = self.tool_loop.escalate();
                let continued = verdict != LoopVerdict::Stop;
                self.failure_stop |= !continued;
                self.emit(LoopEvent::ToolLoop { continued });
                let mut note = failing_command_note(count, &shown);
                if verdict == LoopVerdict::FinalChance {
                    note = format!("{note} {FINAL_TURN_NOTE}");
                }
                self.emit(LoopEvent::Note { kind: NoteKind::ToolLoop, text: note.clone() });
                let run = not_run_record(&call, &note);
                group.running.push(Box::pin(async move { (k, Ok(run)) }));
            } else {
                let runner = self.options.runner.clone();
                let cancel = self.options.cancel.clone();
                // #267: an identical call that failed validation before still goes to the runner (validating is all
                // it does, and if the call passes now it should run), but the same refusal again is answered with
                // the direct note: the arguments are what's wrong, and another identical try won't change that.
                let refused_before = self.invalid.get(&Self::call_key(&call)).cloned();
                // marv-rs: the same truncated read again reads on: the runner gets the call with the next page's
                // offset (the history keeps the model's own bytes), and the result says so.
                let mut call = call;
                let read_on = self.read_pages.next(&call.name, &call.arguments).and_then(|offset| {
                    let arguments = with_offset(&call.arguments, offset)?;
                    call.arguments = arguments;
                    Some(read_on_note(offset))
                });
                if let Some(note) = &read_on {
                    self.emit(LoopEvent::Note { kind: NoteKind::ReadOn, text: note.trim_end().to_string() });
                }
                group.running.push(Box::pin(async move {
                    let mut result = runner.run(call, cancel).await;
                    if let Ok(run) = &mut result {
                        if run.invalid.is_some() && run.invalid == refused_before {
                            run.result.output = refused_call_note(run.invalid.as_deref().unwrap_or_default());
                        } else if let Some(note) = read_on
                            && run.invalid.is_none()
                            && !run.result.is_error
                        {
                            run.result.output = format!("{note}{}", run.result.output);
                        }
                    }
                    (k, result)
                }));
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
                if let Some(reason) = &run.invalid {
                    self.invalid.insert(Self::call_key(&call), reason.clone());
                }
                // Progress for the tool-loop guard: a call that ran and succeeded (not a guard's "not run" note).
                if run.invalid.is_none()
                    && !run.result.is_error
                    && !run.declined
                    && run.result.summary.as_deref() != Some("not run")
                {
                    self.tool_loop.progressed(make_tool_key(&call.name, &call.arguments));
                }
                if run.result.summary.as_deref() != Some("not run") && !run.declined {
                    self.read_pages.record(&call.name, &call.arguments, &run.result.output);
                }
                let footer = time_left_note(self.options.deadline);
                self.answer_with(&call, &run.result.output, &footer);
                if !footer.is_empty() {
                    self.emit(LoopEvent::Note { kind: NoteKind::TimeLeft, text: footer.trim().to_string() });
                }
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

/// What came first while a reply streams.
enum Next {
    Event(Option<ProviderEvent>),
    /// The run's deadline.
    Deadline,
    /// The start of the wrap-up margin, for a reply that has only reasoned ([`LoopState::wrap_up_at`]).
    WrapUp,
}

/// The provider's next event, or the deadline or the wrap-up time, whichever comes first.
async fn next_provider_event(
    stream: &mut EventStream,
    deadline: Option<std::time::Instant>,
    wrap_up: Option<std::time::Instant>,
) -> Next {
    let until = |at: Option<std::time::Instant>| async move {
        match at {
            Some(at) => tokio::time::sleep_until(at.into()).await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        biased;
        event = stream.next() => Next::Event(event),
        () = until(deadline) => Next::Deadline,
        () = until(wrap_up) => Next::WrapUp,
    }
}

impl Drop for LoopState {
    /// The consumer stopped mid-tools (it dropped the stream): the calls without a result would
    /// leave the history invalid and the API would reject every request from now on.
    fn drop(&mut self) {
        let InFlight { calls, answered } = std::mem::take(&mut self.in_flight);
        let text = self.stop_text();
        for call in calls.into_iter().skip(answered) {
            self.push_turn(ChatTurn::Tool { call_id: call.id, name: call.name, text: text.into() });
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
mod tests;
