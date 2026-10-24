# marv-agent

The agent loop (`run_agent`), its repetition guards, and compaction. It knows nothing about the UI or vendors: it pulls `ProviderEvent`s from a `Provider`, runs tools through a `ToolRunner`, and yields `LoopEvent`s.

Spec: Marv `CLAUDE.md` bullets **Agent loop** and **Compaction**, `src/agent.ts`, `src/repetition.ts`, `src/compact.ts`, and `tests/{agent,repetition,compact}.test.ts`.

## Modules

| Marv (TS) | Rust module | Status |
|---|---|---|
| `src/agent.ts` | `agent.rs` | Constants and notes (verbatim) ported. ported, all of `tests/agent.test.ts` ported (against `ScriptedProvider` and a fake `ToolRunner`), plus tests for #58 default grouping, #59 wrap-up and `on_full` |
| — (marv-rs #90) | `announce.rs` | marv-rs only: the announced-next-step detector (`announces_next_step`) and `ANNOUNCED_STEP_NOTE`. A plain answer that announces its next step ("Now checking issue #7 before commenting:") but makes no tool call ends the run halfway; the loop says "continue" once per turn (a second announcement ends the run, still logged). Conservative on purpose: a dangling `:` at the reply's end, a last sentence starting "let me …" (never "let me know …"), or "now <verb>ing" from a closed verb list; questions, summaries and code blocks never fire. See the `agent.rs` header bullet |
| (new) | `send_now.rs` | **Done, with tests.** marv-rs only: "send now". `SendNow` (the running turn's queue of the person's messages: `push`, `close` hands back what was never delivered) and `at_step_boundary` (every call of the last reply that made calls has its result). The loop puts them in as one user message, joined by blank lines (`AgentOptions::send_now`, `LoopEvent::SentNow` for each), at the top of a step that goes on to an ordinary request (after `after_step`'s note; at the step limit only after a "continue"), or in place of ending on a plain answer (one more step reads them, unless it would be at the limit). Never two user turns in a row: see the port note. See the `agent.rs` header bullet. Guard: `tests/guards/tests/messages.rs` |
| `src/repetition.ts` | `repetition.rs` | ported, all tests ported |
| `src/repetition.ts` | `repetition.rs` | ported, all tests ported. marv-rs #81: `FailureLoopState`, the failing-command guard (below) |
| (new) | `suggest.rs` | **Done, with tests.** `suggest` (the next-message request: compaction's shape on the same prefix, never added to the history), `suggestion_request`, `clean` (one short line, or none), `INSTRUCTIONS`. |
| `src/compact.ts` | `compact.rs` | `summarize`, `summary_request`, `compacted_history` (with the open plan's text appended, #21: it takes the text, not the items, as marv-agent doesn't depend on marv-tools), `MID_TURN_NOTE` ported, tests ported. #63: `Version`, `SummaryKind`, the v2 template (`TEMPLATE`, `FULL_INSTRUCTIONS`, `ROLLING_INSTRUCTIONS`, `summary_instructions`), `is_structured`, the one nudge (`TEMPLATE_NUDGE`), `tail_start`, `compacted_history_with_tail`, `default_tail_tokens`, and for overflow recovery `fit_for_summary`, `drop_to_fit`, `DROPPED_NOTE`, `MIN_HISTORY_BUDGET`, `V2_OVERFLOW_TARGETS`; tests per `tail_start` case and per request bound |
| `tests/fake-provider.ts` | `testing.rs` (`ScriptedProvider`, test-only) | ported (scripted replies + recorded requests) |

`grep -rn 'todo!("port' crates/marv-agent` lists what's left.

## Interfaces other crates use

```rust
pub type History = Arc<std::sync::Mutex<Vec<ChatTurn>>>;      // the loop appends; on_full swaps it in place

#[async_trait]
pub trait ToolRunner: Send + Sync {                              // implemented by marv-session
    fn kind(&self, call: &ToolCall) -> ToolKind;
    fn concurrent(&self, call: &ToolCall) -> bool { self.kind(call) == ToolKind::Read } // session: also `agent`
    async fn run(&self, call: ToolCall, cancel: CancellationToken) -> Result<ToolRun, MarvError>;
}

pub struct ToolRun {
    pub result: ToolResult, pub label: String, pub declined: bool,
    pub provenance: Provenance, pub changed: Option<String>,
    pub approval: ApprovalRecord,                                 // None | Auto | Interrupted | Yes | Always | No | Unavailable
    pub display: Option<Arc<dyn Any + Send + Sync>>,             // e.g. marv-tools' DiffShown; never sent to the model
}

pub fn run_agent(options: AgentOptions) -> impl Stream<Item = LoopEvent> + Send;
// AgentOptions::new(provider, history, system, tools, runner, cancel), then set:
//   max_steps (25), max_parallel (4), on_limit, rewrite_call, on_full, after_step, wrap_up_at_limit (#59)
pub type OnLimit    = Arc<dyn Fn(u32) -> BoxFuture<'static, LimitAnswer> + Send + Sync>;
pub type OnFull     = Arc<dyn Fn() -> BoxFuture<'static, bool> + Send + Sync>;
pub type AfterStep  = Arc<dyn Fn(Vec<ToolRun>) -> BoxFuture<'static, Option<String>> + Send + Sync>;
pub type RewriteCall = Arc<dyn Fn(&ToolCall) -> Option<ToolCall> + Send + Sync>;
```

- **The session adapts marv-tools.** It wraps `run_tool` and converts `ToolOutput` into `ToolRun`: `into_core()` for `result`, plus `declined`, `provenance`, `changed` and `approval` (how it was approved: trajectories and `marv -p` read it), with the `diff` in `display`. marv-agent doesn't depend on marv-tools. (A comment in marv-tools says it does; that's stale.)
- **`not_run::ABORTED`** must equal marv-tools' `INTERRUPTED`; both are `"Interrupted by the user before this tool ran."`.
- **`run_tool` should return `Err` rarely.** An `Err` becomes `"<name> failed: <err>"` with `is_error`, and the run goes on.

## Port notes

- **Pull-based loop (`LoopState`).** Marv's loop is an async generator; here it's a state machine behind `futures::stream::unfold`. That way "a call starts only after its `tool_start` was delivered" holds:
  - `advance()` queues `ToolStart`s for the calls it reserves;
  - it launches a call only on the pull *after* that call's `ToolStart` was taken from the queue;
  - running calls sit in a `FuturesUnordered`, and results are appended in call order once the group is done.
- **Every call gets a result.** Marv's `finally` is `impl Drop for LoopState`: dropping the stream mid-tools answers the rest with `not_run::ABORTED` (tested). Keep `in_flight.answered` in step with every append.
- **The history lock is never held across an `.await`.** Snapshot the history (`clone`) for `provider.stream(&snapshot, …)`. The adapter serializes it immediately anyway.
- **Text-loop check:** every 400 *bytes* of text while streaming (Marv counts UTF-16 units), and once at the end. The repetition thresholds count `char`s. `StepReply::apply` resets the check position on `Retry` too; Marv didn't (a harmless lag), and this is noted in the module doc.
- **Result hashes** for tool-loop detection use `std::hash::DefaultHasher` (Marv: `Bun.hash`). They're only compared within one run.
- **Failing commands (marv-rs, #81).** bash resets the tool-loop history, and a model retrying a command that can't work varies its tail, so a `git clone` without network was re-sent 11 times. `FailureLoopState` remembers each bash call by `marv_core::command::command_key` plus its `network`/`sandbox: false` flags. The key keeps setup before the work (`cd a && make` ≠ `cd b && make`), the work and its non-display pipe stages (`| grep missing` kept), and any `|| <other>`. It drops only what can't change whether the work failed: a list after the work, joined by `&&`, `;`, `&` or a newline, that only reads (`is_read_only_command`: `&& du -sh`, `; ls`, `& wait`; any other list is kept, since `cargo build && ./app --a` and `… --b` differ once the build passes), `|| true`/`|| :`, display-only stages (`| tail`, `| head`, `| wc`, `| cat`, `| less`), and `2>&1`-style redirections. Each call has a failure signature (`failure_signature`). An exit-0 result has none, unless it carries bash's network note (`NETWORK_NOTE_START`, held to `marv_sandbox::notes::NETWORK_NOTE` by a marv-session test), which is the `git clone … || true` case. Otherwise the signature is the first line that says something failed (`fatal:`, `could not resolve host`, …), else a line with an error word, else the ending and last line, ignoring digits and case. After `FAILURE_REPEATS` (2) failures in a row with the same signature, another try isn't run. The check happens when the call is about to run (in the `Tools` phase, after earlier calls in the reply were answered and recorded), so an edit earlier in the same reply resets first. The first time the call is answered with `failing_command_note` (a `ToolLoop { continued: true }`); the next time the run ends `Loop`, with the rest of the step "not run" (`failure_stop`, the same `warned` flag). Resets: a call to any tool but bash and the `READ_ONLY_TOOLS` (`read_file`, `glob`, `grep`, `web_fetch`, `plan`, `skill`) clears all counts (an edit, a subagent, `process`, an MCP tool); so does a successful bash command that isn't read-only (`marv_core::command::is_read_only_command`, a short list of readers like `ls`, `cat`, `grep`, `echo`, `whoami`, `git status`, with no `>`; there was no existing classifier, since yolo's `auto_safe` judges the sandbox, not the command). A read-only success forgets only its own key, and another error starts the count again. The guard's own notes aren't recorded.
- **"Send now" (marv-rs).** The person's message typed during a turn goes in at the next step boundary, never mid-step: a running tool call finishes (only a cancel stops one), and the message is appended, so the cache holds and nothing a request already carried is re-serialized. `can_deliver` checks the history itself right before (whatever the phase says): `at_step_boundary` (a history with a call still waiting for its result keeps the message waiting), and **never two user turns in a row**. Everything waiting goes in as one user message (blank lines between); if the history ends with a user turn no request has carried yet (`LoopState::sent_len`, the history's length at the last request, reset by a mid-turn compaction and by an overflow recovery's `Recovered`, which rewrite the history: the turn's own message, a check's note, a nudge), the message joins it after a blank line; if it ends with one a request already carried (an empty reply's retry resends the same request), the message waits for the next boundary rather than follow it or rewrite it. Delivered only before an ordinary request: at the step limit it waits for `on_limit`'s Continue (and then goes in, with anything typed during the question); after a Stop or a hard stop (and its wrap-up reply) it goes back with the turn. A plain answer with a message waiting goes on for one more step, unless that step would be at the limit (then the turn ends normally and the message goes back). Not delivered during a cancel, and not after a decline, a loop stop or a cut-off stop: those end the run first. The step it causes counts toward `max_steps`. A plain answer with a message waiting doesn't run #90's "continue": the person's words come first.
- **Tool-loop keys** are canonical JSON with keys sorted at every depth, printed by hand so serde_json's `preserve_order` feature can't change them.
- **One transition per `advance()`.** Phases: `Top` (step limit) → `Request` → `Streaming` → decide → `Tools` → `Full` (`on_full`) → `AfterStep`. `next_event` loops `advance` until something is queued, so nothing after an event happens before the consumer pulls again (e.g. no request after a `StepLimit` it stopped at).
- **A group mirrors Marv's `runGroup`:** reserved calls queue a `GroupEvent::Start`; delivering it queues the `ToolStart` and marks the call `announced`; the next `advance` launches it and polls it once (`now_or_never`), so a cancel or approval inside the tool happens before the next `ToolStart` is considered. A `Start` delivered after a cancel or "no" becomes `ToolSkipped`.
- **Kept from Marv as is** (worth a look, not changed here): a reply with calls that ends in a provider `Error` puts its calls in the history unanswered (the session's `answer_all_calls` covers saving), and the second tool loop answers its calls with `not_run::CUT_OFF`'s text.
- **Tests:** `ScriptedProvider` replies with an empty `Done` after its script (Marv: "(script ended)"), so a test that runs past its script sees empty-reply retries first; the tests that count requests script their last reply.
- **Two compaction failure rules live in the session** (Marv's `midTurnCompactFailed`; here `compact_failures`/`compact_failed_at`): a summary that failed isn't asked for again right away, and Esc during the summary stops the turn (the loop checks for abort right after `on_full`). **marv-rs #144 differs from Marv:** Marv never asks again that turn, but a `marv -p` run is one turn, so one failed summary let a SWE-bench run grow from 112k to 201k tokens and time out. Here a failed summary is asked for again within the turn once the context (last prompt + completion) is at least 25% past its size when the summary failed (`COMPACT_RETRY_GROWTH`), at most 3 failures a turn (`MAX_COMPACT_FAILURES`, `may_compact_again`); a new turn starts over. A stopped summary still stops the turn. The failure's reason and error reach the `compaction` event in stream-json (`reason`, `error`) and the trajectory (a `compact_failed` record). And `summarize` asks again once with `tool_choice: "none"` when the model called a tool instead of summarizing (tools still sent, same prefix). The retry point is the smaller of 1.25× the failed size and halfway from it to the window (a failure at the 85% trigger would otherwise need 106% of the window). A tool-call retry sends `tool_choice: "none"` on OpenRouter; Ollama has no `tool_choice`, so there it is the same request again. At most 3 failures a turn × 2 requests each.
- **When to compact also lives in the session** (`nearly_full`, marv-rs #140): at `COMPACT_AT` (85%) of the window as in Marv, or sooner once the context passes the user's `compactTokens` (default 100k; `0`/`false` off). Marv compacts only at 85%; on OpenRouter models with large windows that was never reached, and long sessions resent 200k–400k tokens every step (replayed on real trajectories: about 60% of the cost of long turns saved at 100k). Subagent loops have no `on_full` hook and still never compact (as before #140). The threshold counts prompt plus completion tokens, so a thinking model's reasoning counts too.

## Planned improvements (Marv issues)

- **#58 parallel read-only tools:**
  - `ToolRunner::concurrent` defaults to `ToolKind::Read`.
  - `call_groups` turns a run of consecutive concurrent calls into one group; anything else is a barrier (tested).
  - Results are still appended in call order.
  - Out of scope: starting tools while the reply is still streaming.
- **#57 reasoning round-trip:** `StepReply` keeps `ProviderEvent::Reasoning` and puts it on the assistant `ChatTurn` (tested). Sessions must save and restore it; old sessions without it still load (`#[serde(default)]` in marv-core).
- **#59 wrap-up at the step limit (done):**
  - With `wrap_up_at_limit`, a hard stop (no `on_limit`, or `Unavailable`) first sends `STEP_LIMIT_NOTE` with `ToolChoice::None`. Tools are still sent, so the cached prefix holds.
  - Any call the reply makes anyway is answered `not_run::STEP_LIMIT`. An `on_limit` "no" (`Stop`) gets no wrap-up.
  - The run still ends `Done(MaxSteps)`, so headless still exits 2.
  - Turned on by the session for the main agent and for subagents (`SUBAGENT_MAX_STEPS`): `marv -p` prints the wrap-up as its reply, and a subagent's report is the wrap-up followed by Marv's limit line. The TUI asks "Keep going?" (`on_limit`), so it never gets here unless the answer is `Unavailable`.
- **#64 continue after a dropped stream:**
  - `LoopEvent::Retry { continued }` is in place. When the provider signals a continuable drop, keep the partial text as the assistant turn and append `DROPPED_STREAM_NOTE` instead of discarding it.
  - **This needs `marv_core::ProviderEvent::Retry` to gain `continued: bool`** (a marv-core change, still to request).
  - `Retry` also carries `cause: marv_core::RetryCause` (#88 stall, #96 degenerate reply); the loop passes it on in `LoopEvent::Retry` for the trajectory and otherwise treats every cause the same.
- **#90 announced next step, no tool call (marv-rs, done):**
  - `announce::announces_next_step` runs when a plain answer (no calls, not aborted) is about to end the run in `run_calls`. It fires at most once per turn (`LoopState::announced`): the first announce­ment pushes `ANNOUNCED_STEP_NOTE` and takes one more step; a second emits `AnnouncedStep { continued: false }` and ends the run as before — both logged (`LoopEvent::AnnouncedStep`, the TUI's `✻` line, the trajectory's `announced_step` record).
  - It only looks at the reply's last line: a dangling `:`, a last sentence starting "let me " (not "let me know "), or "now " + a closed progressive-verb list. Questions, summaries (long last sentences), odd fence counts (a reply cut inside a code block) never fire; the table tests pin both sides.
- **#63 overflow hook (done):** `AgentOptions::on_overflow` (`OnOverflow`, `OverflowOutcome::{Recovered, Failed, Stopped}`). A step whose error `is_context_overflow` matches goes to the hook before anything of the reply enters the history; `LoopEvent::Overflow { attempt }` replaces the `Error`. `Recovered` requests the same step again (the step counter doesn't move), at most `MAX_OVERFLOW_RECOVERIES` (4) a step, so at most 5 requests; `Failed(text)` ends `Error` with the hook's text; `Stopped` or a cancel during the hook ends `Aborted`. The attempt resets after a step that got a reply. Unset, nothing changes.
- **#63 compaction v2 (spec `docs/specs/2026-10-14-compaction-v2.md`; a deliberate change from Marv, behind the temporary `compaction` key, default `"v1"` until the eval):**
  - The template (`V2_SECTIONS`, 7 headings). A reply is structured with at least 5 at line start, any case. A good text reply that isn't gets one nudge (the same request, the reply, `TEMPLATE_NUDGE`: still a cache hit up to the reply). Never after #144's `tool_choice: none` re-ask, so a summary is at most 2 requests and #144's 6-a-turn bound holds. The nudge can't fail a compaction: anything but a structured reply keeps the first (`structured: false`); only a stop stops.
  - The verbatim tail (`tail_start`, the spec's five cases in order): about `compactTailTokens` (default `min(15k, 15% of min(85% window, compactTokens))`) of the newest turns, moved earlier so a result is never split from its call, none when even the newest turn is over budget, when snapping reaches index 0, or when the snapped tail is over 2× the budget. `compacted_history_with_tail` leaves the ACK out when the tail starts with an assistant turn (never two user or two assistant turns in a row). The summary covers the tail too (one cache-hitting request).
  - Rolling: `summary_request(history, focus, kind)`; `SummaryKind::Rolling` when the session holds a summary (`SavedSession::summary`), never by sniffing `COMPACTED_PREFIX` in the text. The old `rolling_update_request` stub is gone (it would have built a new request and lost the cache).
  - Overflow recovery helpers (both versions): `fit_for_summary` keeps the last summary and the task, drops the oldest whole steps, cuts any result over a quarter of the budget to head and tail; `drop_to_fit` (level 3) keeps the summary and the newest turns that fit, never starting at a result, then `DROPPED_NOTE`. The session drives them (`session.md`).
