# marv-agent

The agent loop (`run_agent`), its repetition guards, and compaction. It knows nothing about the UI or vendors: it pulls `ProviderEvent`s from a `Provider`, runs tools through a `ToolRunner`, and yields `LoopEvent`s.

Spec: Marv `CLAUDE.md` bullets **Agent loop** and **Compaction**, `src/agent.ts`, `src/repetition.ts`, `src/compact.ts`, and `tests/{agent,repetition,compact}.test.ts`.

## Modules

| Marv (TS) | Rust module | Status |
|---|---|---|
| `src/agent.ts` | `agent.rs` | Constants and notes (verbatim) ported. Ported and tested: `answer_all_calls`, `agent_label`, `agent_args`, `label_of`, `used_ids`/`make_ids_unique`, `call_groups`, `StepReply` (stream fold and outcome), `EmptyNext`, the step-limit helpers, and the `Drop` guard. **The loop body (`LoopState::advance`) is todo.** |
| `src/repetition.ts` | `repetition.rs` | ported, all tests ported |
| `src/compact.ts` | `compact.rs` | `summarize`, `summary_request`, `compacted_history`, `MID_TURN_NOTE` ported, tests ported; v2 (#63) helpers todo |
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
- **Tool-loop keys** are canonical JSON with keys sorted at every depth, printed by hand so serde_json's `preserve_order` feature can't change them.
- **Port these tests next** with `ScriptedProvider` and a fake `ToolRunner`, from `tests/agent.test.ts`:
  - every request extends the previous one;
  - the step limit and `on_limit`;
  - unique ids end to end;
  - interrupt and decline in groups;
  - empty replies and cut-offs;
  - `after_step` ordering;
  - text and tool loops;
  - `rewrite_call`;
  - parallel order and the `MAX_PARALLEL` cap.
- **Two compaction failure rules live in the session** (`midTurnCompactFailed`): a summary that failed mid-turn isn't asked for again that turn, and Esc during the summary stops the turn (the loop checks for abort right after `on_full`).

## Planned improvements (Marv issues)

- **#58 parallel read-only tools:**
  - `ToolRunner::concurrent` defaults to `ToolKind::Read`.
  - `call_groups` turns a run of consecutive concurrent calls into one group; anything else is a barrier (tested).
  - Results are still appended in call order.
  - Out of scope: starting tools while the reply is still streaming.
- **#57 reasoning round-trip:** `StepReply` keeps `ProviderEvent::Reasoning` and puts it on the assistant `ChatTurn` (tested). Sessions must save and restore it; old sessions without it still load (`#[serde(default)]` in marv-core).
- **#59 wrap-up at the step limit:**
  - With `wrap_up_at_limit`, a hard stop (no `on_limit`, or `Unavailable`) first sends `STEP_LIMIT_NOTE` with `ToolChoice::None`. Tools are still sent, so the cached prefix holds.
  - Any call the reply makes anyway is answered "Not run".
  - The run still ends `Done(MaxSteps)`, so headless still exits 2.
- **#64 continue after a dropped stream:**
  - `LoopEvent::Retry { continued }` is in place. When the provider signals a continuable drop, keep the partial text as the assistant turn and append `DROPPED_STREAM_NOTE` instead of discarding it.
  - **This needs `marv_core::ProviderEvent::Retry` to gain `continued: bool`** (a marv-core change, still to request).
- **#63 compaction v2 (spec first; nothing is settled):**
  - `V2_SECTIONS` (structured template), `V2_TAIL_TOKENS` = 15k (verbatim tail), `V2_OVERFLOW_TARGETS` = 70/50/35%;
  - `tail_start` (never splits a call from its result) and `rolling_update_request` are todo;
  - overflow recovery needs `marv_provider::errors::is_context_overflow`.
