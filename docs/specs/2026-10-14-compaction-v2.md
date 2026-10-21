# Compaction v2: structured rolling summary, verbatim tail, overflow recovery

- Issue: FlologixAI/marv #63 (tracked privately as marv-rs #17)
- Status: **approved 2026-10-20 with amendments.** The amendments are folded into the text below; the decisions are in §9, the PRs in §8.
- Date: 2026-10-14 (draft), revised against `main` 2026-10-20 (after #116, #140, #144, #62)
- Scope: `crates/marv-agent` (compaction, loop hook), `crates/marv-provider` (overflow detection), `crates/marv-core` (`estimate_tokens`), `crates/marv-session` (wiring, state, config, trajectory).

The issue was written against the TypeScript build (`src/compact.ts`, `src/provider/errors.ts`). This spec is grounded in the Rust code on `main`. No local opencode checkout exists (only the `opencode` binary under mise), so opencode's behaviour is taken from the issue text and is marked as such.

## 1. Problem and evidence

Today, compaction replaces the whole model history with one free-form summary once the context passes the threshold (§2). That has three weaknesses (from the issue):

1. **The work in progress is summarized away.** The file the model just read and the failing test output it is acting on become one line of prose. After compaction the model reads them again and repeats steps.
2. **Detail decays.** The second compaction summarizes "summary + new history" with the same open-ended instructions, so each round loses a little more. Long tasks degrade.
3. **No recovery from overflow.** If a request is rejected as too long (the estimate was off, or one giant tool result landed), the turn ends with a provider error. `marv_provider::errors::is_context_overflow` is a `todo!`.

Evidence still to collect (see §7): the eval measures repeated reads after compaction. There is no trajectory-derived number in the issue. This spec doesn't claim a baseline. Cheaper evidence already exists for compaction in general: #140's replay of real long turns (about 60% of the cost saved at a 100k trigger) and its SWE-bench runs; v2's first numbers should come from the same replay before any paid A/B.

## 2. Current Rust design

| What | Where |
|---|---|
| Trigger: `nearly_full()` = last prompt + completion ≥ `min(COMPACT_AT (0.85) × window, compactTokens)` (#140). `compactTokens` defaults to `DEFAULT_COMPACT_TOKENS` = 100k, at least `MIN_COMPACT_TOKENS` = 40k, `0`/`false` = 85% only | `crates/marv-session/src/session.rs` (`nearly_full`), `config.rs` |
| Summary instructions `INSTRUCTIONS`, `summary_request(history, focus)`: the same history plus one user message | `crates/marv-agent/src/compact.rs` |
| `summarize(SummarizeOptions)`: streams the reply. A reply that calls a tool is asked again **once** with `tool_choice: None` (same prefix, tools still sent; #144). Errors (with the history kept) on a second tool call (`TOOL_CALL_ERROR`), a `length` cut-off, an empty reply, a provider error or a cancel. `Retry`/`Discard` clear the text | `compact.rs` |
| `compacted_history(summary, plan_text)`: `[User(COMPACTED_PREFIX + summary [+ plan]), Assistant(COMPACTED_ACK)]` | `compact.rs` |
| `MID_TURN_NOTE`: pushed after a mid-turn compaction | `compact.rs`, pushed by `MarvSession::compact_if_full` |
| v2 stubs: `V2_SECTIONS`, `V2_TAIL_TOKENS = 15_000`, `V2_OVERFLOW_TARGETS = [0.70, 0.50, 0.35]`, `tail_start` (todo), `rolling_update_request` (todo) | `compact.rs` |
| Failure rule (#144): `State::compact_failures` / `compact_failed_at`; `try_compact()` = `nearly_full() && may_compact_again(…)`: after a failure, again only once the context grew 25% (`COMPACT_RETRY_GROWTH`, or halfway to the window if sooner), at most `MAX_COMPACT_FAILURES` = 3 failures a turn; reset when the turn ends. So a turn makes at most 6 summary requests (3 × 2), as CHANGELOG promises | `session.rs` |
| `summarize_into(focus, …)`: swaps the shared `History` **in place**, clears the MCP `announced` list and `loaded` schemas, sets `last = None`, **resets the nested instruction files (#62, `inner.nested.reset()`)**, and writes `TrajectoryRecord::Compact { turn, summary }`, or on failure `TrajectoryRecord::CompactFailed { turn, reason, error, tokens }` | `session.rs`, `trajectory.rs` |
| Events: `SessionEvent::Compaction(CompactResult)`; stream-json `{"type":"compaction","compacted":true}` or `{…,"compacted":false,"reason","error"}` (#144) | `crates/marv-cli/src/headless.rs`, `crates/marv-web/src/events.rs` |
| Before a turn: `if self.try_compact() { … compact_if_full(…, false) }`. Mid-turn: `options.on_full`. `/compact [focus]`: `MarvSession::compact` | `session.rs` |
| The plan (#15): `plan_text(items)` for an open plan, appended to the summary message | `crates/marv-tools/src/plan.rs`, `session.rs::summarize_into` |
| Provider errors reach the loop as `ProviderEvent::Error(String)`, built by `http_error(status, detail, …)`. Only 429/502/503/504 and network drops are retried | `crates/marv-provider/src/errors.rs`, `retry.rs` |
| #116 window refit: OpenRouter's refusal "maximum context length is N tokens. However, you requested about M tokens (X of text input, Y in the output)" is parsed by `window_refusal` (`WINDOW_REFUSAL`) into (window, input). The adapter remembers both in its `Room` (window, `tokens_per_byte`) and resends once with the room left, then without a cap. Only a refusal that still fails after that reaches the loop. `Room` is held by `OpenAiCompatProvider` and **not exposed** through the `Provider` trait | `crates/marv-provider/src/openai_compat.rs` |
| Loop: `Phase::Full` calls `AgentOptions::on_full` after a step's calls are answered. `StepOutcome::Error(msg)` emits `LoopEvent::Error` and finishes `StopReason::Error`; `decide` pushes `reply.assistant_turn()` before matching the outcome | `crates/marv-agent/src/agent.rs` |
| Window: `MarvSession::context_length()` = `provider.context_length()` or the model list's `ModelInfo::context` | `session.rs`, `crates/marv-provider/src/models.rs` |
| Token estimate, 4 chars a token, in four copies: `marv_session::usage::estimate_tokens`, `marv_mcp::deferred::estimate_tokens`, `marv_tui::ui::thinking_view::estimate_tokens` (UTF-16 units), and an inline one in `marv-tui/src/app.rs` (the compaction notice); plus `ASSUMED_TOKENS_PER_BYTE = 0.25` in `openai_compat.rs` | — |
| Ollama: `num_ctx` is fixed per session. Past it, Ollama **shifts the window silently** (no error) | `crates/marv-provider/src/ollama.rs` header |

What works and must stay: mid-turn compaction (`on_full`), `/compact [focus]`, a summary request that reuses the cached prefix, the transcript untouched, the #144 failure rules (growth before a retry, 3 failures a turn, Esc during the summary stops the turn), the tool-call re-ask, the plan carried verbatim, MCP re-announcement, nested instruction files given again after a compaction.

## 3. Proposed design

### 3.1 History after a v2 compaction

```
User      COMPACTED_PREFIX + <structured summary> [+ "\n\n" + plan_text]
Assistant COMPACTED_ACK                     ← only when the tail is empty or starts with a User turn
<tail>    the newest turns, verbatim (ChatTurn values moved, never rebuilt)
[User     MID_TURN_NOTE (+ MCP announcement)] ← mid-turn only, as today
```

- **The ACK rule.** Two user messages or two assistant messages in a row are what the current shape avoids. If the tail starts with an `Assistant` turn (a step's calls), the ACK is left out: User(summary) → Assistant(calls) → Tool … is valid. If the tail starts with a `User` turn, or is empty, the ACK stays. The tail never starts with a `Tool` turn (§3.2).
- **Tail turns are the same `ChatTurn` values.** `ToolCall::arguments`, `reasoning` (#57) and tool result text keep their bytes. Provenance fences in tool results stay as they are, so untrusted text stays fenced as data.
- The **summary covers the whole history, tail included** (decided, §9). Asking the model to summarize "everything before message N" is unreliable, and it would need a different request (no cache hit). The overlap costs some summary tokens and is harmless: the summary says what happened, the tail shows it.
- **Nested instruction files (#62) and the tail.** Today `summarize_into` calls `inner.nested.reset()` because the results that carried the notes are gone. With a tail, some of those results survive verbatim, and a plain reset would give their files a second time. So the session records, per tool call id, the folders whose note it attached to that result (a map cleared with the history). After a v2 compaction it resets, then marks as seen again the folders whose notes sit in a tail result. Folders whose notes were summarized away come again on the next touch, as today. The notes keep #62's fencing and checks unchanged; nothing in the summary counts as having delivered a file.

### 3.2 `tail_start(history, tail_tokens) -> usize`

- Walk back from the end, summing an estimate per turn (`marv_core::estimate_tokens`; tool calls count their name + arguments). Stop before the turn that would push the sum over `tail_tokens`.
- **Snap to a safe boundary:** move the start *earlier* to the nearest index whose turn is `User` or `Assistant`. A `Tool` turn is never first, so a result is never split from the assistant turn that called it. Moving earlier can exceed the budget by one step's results; the cap below bounds it.
- **Return values.** The cases are checked in this order:
  1. **Already fits:** the whole history's estimate is ≤ `tail_tokens` (an empty history included) → `0`. This is the only way to get `0`. It means there is nothing worth compacting: `CompactFailure::Empty`, "Nothing to compact yet.", as for an empty history today.
  2. **Pulled in by snapping:** the history doesn't fit, but snapping moved the start back to index 0 → `history.len()` (no tail). Everything gets summarized.
  3. **Over the cap:** the snapped tail is larger than `2 × tail_tokens` → `history.len()` (no tail). A giant tool result then goes into the summary, not the tail.
  4. **Otherwise:** the snapped index, which is `> 0` and `< history.len()`.
- **Overflow recovery (§3.5)** never calls `tail_start` with the full budget on a history that just overflowed. It passes the level's smaller budget, or `0` at level 2, and a budget of `0` returns `history.len()`. Case 1 only applies when the budget is above zero: an empty history with budget `0` is still `0`.
- **The default budget** is `min(15k, 15% of the effective threshold)`, the threshold being `nearly_full`'s `min(0.85 × window, compactTokens)`. At the 100k default that is 15k; on a 32k Ollama window it is about 4k, so the tail never crowds out the summary.
- Pure, in `marv-agent/src/compact.rs`. The 4-chars estimate moves into `marv-core` as one `estimate_tokens` (types-only crate, no I/O), and the four copies in §2 use it (`ASSUMED_TOKENS_PER_BYTE` is defined from it).

### 3.3 Structured template and rolling update

`INSTRUCTIONS` becomes the v2 template (`V2_SECTIONS`, opencode's sections per the issue). Proposed text:

```text
Summarize our conversation so far, so we can continue it from your summary alone. The earlier messages will be removed; the most recent ones stay as they are after your summary.

Use exactly these sections, as Markdown headings, in this order:

## Objective
What the user wants overall, and what they asked for most recently, in their words where it matters.
## Requirements
Constraints and preferences the user stated: what to do, what to avoid.
## Decisions
What was decided, and why. Approaches tried and rejected, and why.
## Work State
What's done, what's in progress, what's left. Commands run and their important results (tests, errors), exactly.
## Next Move
The very next step you were about to take.
## Relevant Files
At most 15 paths, one per line, each with what it is and what changed in it.
## Important Context
Anything else needed to continue: names, numbers, error messages, URLs, exactly.

Write it as notes for yourself. Keep names, paths, numbers and error messages exact. Don't call any tools, and don't do anything else.
```

**Rolling update.** When the history already starts with a summary (a previous compaction), the appended instruction is `ROLLING_INSTRUCTIONS` instead:

```text
Update the summary at the start of this conversation with everything that happened since it, so we can continue from your updated summary alone. Keep what is still true; change what changed; drop what no longer matters (finished details, superseded decisions). Use exactly the same sections. Return the whole updated summary, not just the changes.
```

…followed by the same section list and closing lines.

- **This is still a cache hit.** After a v2 compaction the history *is* "previous summary + what happened since", so the rolling request is `history + one user message`, as today. It replaces the stub `rolling_update_request(previous_summary, since, focus)`, which would build a new request and lose the cache. **Change:** drop that stub and make `summary_request(history, focus, rolling: bool)`.
- `rolling` is true when the session holds a previous summary (§3.6), not by sniffing `COMPACTED_PREFIX` in the text. A user message that quotes the prefix must not switch modes.
- `/compact <focus>` appends `"\n\nFocus especially on: <focus>"` as today.

**The template nudge.** A reply counts as structured when at least 5 of the 7 `## <section>` headings are present, matched at line start and ignoring case.
- The nudge is sent **only after a good text reply** (not cut off, not empty, no tool call) that isn't structured: the same request + that reply as an assistant turn + `TEMPLATE_NUDGE` ("Rewrite that summary using exactly the sections asked for: …"). It is still a cache hit up to the first reply.
- **Never compounding.** A summary sends at most **2** requests: the second is either #144's `tool_choice: None` re-ask (after a tool call) or the nudge (after an unstructured text reply), never both. A re-ask that comes back unstructured is accepted as it is.
- The nudge can't fail a compaction: if the nudge's reply is unstructured, cut off, empty, a tool call or an error, the first reply is kept, logged `structured: false` (decided, §9). Only Esc during the nudge stops, as today.
- So the #144 bound holds: a failed summary costs at most 2 requests, 3 failures a turn, 6 failed summary requests a turn.

### 3.4 Plan, instructions, MCP

- **The plan (#15)** is appended verbatim after the summary, as today (`plan_text`, only for an open plan). The template doesn't ask the model to restate it.
- **Instructions:** `AGENTS.md`, memory and skills are in the system prompt, which is never summarized, so there is nothing to re-inject. Nested instruction files come on tool results; §3.1 says how the tail keeps them.
- **MCP:** unchanged. `announced` is cleared and the next message (or the mid-turn note) announces the tools again.

### 3.5 Overflow recovery

**Detection:** `marv_provider::errors::is_context_overflow(message: &str) -> bool`, a case-insensitive match over the whole error string (it includes the HTTP detail). It **builds on #116**: `WINDOW_REFUSAL` / `window_refusal` already recognize OpenRouter's "maximum context length is N tokens … X of text input" wording, and the adapter refits `max_tokens` on it first. `is_context_overflow` must only match what **escapes** that refit: a prompt that alone doesn't fit, after the adapter's own resend. A refusal the adapter recovers from never reaches the loop. Other seed patterns, **to be checked against real captured messages before merge** (fixtures, M1):

- OpenAI and OpenRouter: `window_refusal(…)` parses, or `context_length_exceeded`, `exceeds the context window`
- Anthropic upstream: `prompt is too long`
- Gemini upstream: `input token count .* exceeds the maximum`
- generic: `context (length|window)`, together with `exceed|too long|too large`
- HTTP 413 from OpenRouter: `returned 413`

There is no separate `overflow_window` helper: `window_refusal` already returns the window and the prompt's size.

**Ollama (assumption, to verify):** Ollama's native API truncates past `num_ctx` instead of failing. So overflow recovery is effectively OpenRouter-only. Ollama stays covered by `nearly_full`, which uses its exact `prompt_eval_count`. If a captured Ollama error does exist, its wording is added to the list.

**Loop hook (marv-agent):** a new optional `AgentOptions::on_overflow: Option<OnOverflow>`.

```rust
pub type OnOverflow = Arc<dyn Fn(String /* error */, u32 /* attempt, from 0 */) -> BoxFuture<'static, OverflowOutcome> + Send + Sync>;

pub enum OverflowOutcome {
    /// The history was made to fit: retry the step.
    Recovered,
    /// Recovery gave up. The message is what the user should read: the session's own explanation
    /// (§3.5, "When the fixed content doesn't fit"), or the provider error that stopped it.
    Failed(String),
    /// Esc during recovery.
    Stopped,
}
```

In `LoopState::decide`, `StepOutcome::Error(message)` where `is_context_overflow(&message)` and `on_overflow` is set:
- Nothing from the failed reply enters the history. It has no text or calls. If it somehow has partial text, that is dropped too, as for a `Retry`. Today `decide` pushes `reply.assistant_turn()` before matching `StepOutcome::Error`, so the overflow check has to come before that push.
- Emit `LoopEvent::Overflow { attempt }` (new) instead of `LoopEvent::Error`.
- `on_overflow(message, attempt)`:
  - `Recovered` → `Phase::Request` again (same step; the step counter is not advanced).
  - `Failed(text)` → emit `LoopEvent::Error(text)` (the hook's text, not the raw provider message) and `finish(StopReason::Error)`. The session's turn ends `TurnEndReason::Done(StopReason::Error)`, so headless exits as for any provider error.
  - `Stopped` → `finish(StopReason::Aborted)`, no `Error` event, as Esc during `on_full`.
- No hook set → today's path with the raw message (`LoopEvent::Error(message)`, `StopReason::Error`). Unset, the hook changes nothing.
- `attempt` counts per step and resets after a step that got a reply. **At most 5 recoveries per step** (levels 0–3 and the whole-window last attempt); the hook call after that returns `Failed`.
- A cancel during the hook ends `Aborted` whatever the hook returns, as `on_full` does (the loop checks `aborted()` after the hook).

marv-agent depends on marv-provider already, so it can call `is_context_overflow` directly.

**Session hook (marv-session):** `attempt` picks the **starting** level. Within one hook call, the session walks down the levels itself (§3.5.1), so a summary request that overflows doesn't end the recovery:

| attempt | What the session does |
|---|---|
| 0 | Normal compaction, but the **summary request is built from a cut-down history** that fits `0.70 × window` (the full history just overflowed, so reusing the prefix would overflow again). |
| 1 | Same, target `0.50 × window`, tail budget halved. |
| 2 | Same, target `0.35 × window`, no tail. |
| 3 | **No summary:** keep the previous summary (if any) + the newest exchanges that fit `0.35 × window`, drop the rest, and add a note: "(Automatic note from Marv) Older messages were dropped to fit the context window; ask the user if you need something from them." No asking (decided, §9). |

Under `compaction: "v1"` (and in PR (c), before v2 exists) "normal compaction" is today's v1 summary on the cut-down history, with no tail: overflow recovery is a bug fix and doesn't wait for v2.

- **Cutting down for the summary request** (`fit_for_summary(history, budget) -> Vec<ChatTurn>`, pure, in compact.rs): drop the oldest whole exchanges after the previous summary (never splitting call/result). Any single tool result still larger than `budget / 4` is shortened to head + tail with `[… cut for the summary …]`, reusing `marv_tools::output`'s head/tail shape. This request is never a cache hit; that is accepted, because it only happens after an error.
- **The budget is for the history only; the fixed cost comes off first.** A summary request also sends the system prompt, the tool schemas and the summary instruction, and the reply needs room. So:
  - `fixed = estimate(system) + estimate(tool specs as sent, JSON) + estimate(instruction text, rolling or full, with any focus)`;
  - `reply_reserve = min(8192, window / 8)`;
  - `budget = target × window − fixed − reply_reserve`.

  Level 3 sends no summary request, but the retried step still sends the system prompt and tools, so its budget is `0.35 × window − estimate(system) − estimate(tool specs) − reply_reserve`. The tools stay in recovery requests, even though there is no cache to keep. **Assumption:** some upstreams reject a history containing tool calls when no tools are defined.
- **Estimates are calibrated when the error allows it.** `estimate` is `marv_core::estimate_tokens`. When `window_refusal(message)` parses, the refusal's measured input tokens over the estimate of the request that overflowed gives a ratio, and the budgets scale by it (the same idea as `Room::tokens_per_byte`). The adapter's `Room` stays private; exposing it through the `Provider` trait is not needed for this.
- **A level that is too small is skipped.** If a level's `budget` is below `MIN_HISTORY_BUDGET` (1,024 tokens), the level is skipped rather than run with an empty history.
- **When the fixed content doesn't fit.** Let `fixed_step = estimate(system) + estimate(tool specs)`: what every retried step sends whatever the history. If level 3's budget (at 35%) is also below the minimum, there are three cases, checked in order:
  1. **The fixed content exceeds the window** (`fixed_step ≥ window`). No history can help. The hook returns `Failed("The system prompt and tool definitions alone (about N tokens) are larger than this model's context window (M tokens). Pick a model with a larger window (/model), or turn off MCP servers or tools you don't need (/mcp).")`.
  2. **The fixed content fits but leaves no usable room** (`window − fixed_step − reply_reserve < MIN_HISTORY_BUDGET`). It returns `Failed("After the system prompt and tool definitions (about N tokens) and room for the reply, only about K tokens of this model's M-token window are left for the conversation, which is too little to go on. Pick a model with a larger window (/model), or turn off MCP servers or tools you don't need (/mcp).")`.
  3. **The fixed content fits, but the 35% target leaves no room.** One **last attempt** at level 3 uses the whole window: `budget = window − fixed_step − reply_reserve`. It is the same drop with no request. If even that step overflows again, the next hook call has nothing left to try and returns `Failed` with case 2's message, using the measured numbers.

  There is no drop loop: every level, including the last attempt, is tried at most once per step, so the number of requests is bounded.
- **Window when unknown** (custom provider, model list not loaded): `window_refusal(message)`'s window. If that fails too, the size of the request that overflowed (`estimate_tokens` of the history) stands in for the window.
- **An overflow during an ordinary summary** (`summarize` returns `Err(msg)` with `is_context_overflow(msg)`) goes straight to level 0 instead of `NotCompacted`.

#### 3.5.1 When a recovery summary itself overflows

`summarize` returns its error to the session, not to the loop, so the session handles it inside the same hook call:

```text
level = attempt                        (0..=3)
loop:
  if level == 3: drop to fit (no request); return Recovered   ← or the whole-window last attempt / Failed (§3.5)
  if compact_failures >= MAX_COMPACT_FAILURES: level = 3; continue
  budget too small? → level += 1; continue
  summarize(fit_for_summary(history, budget), …)
    Ok(summary)                 → install the new history; return Recovered (the loop retries the step)
    Err(e) if overflow(e)       → count a failure; level += 1; continue    ← our estimate was off: try smaller
    Err(_) if cancelled         → return Stopped
    Err(e)                      → count a failure; return Failed(e)        (the turn ends with e, as today)
```

- **How recovery counts toward `compact_failures` (#144).** Recovery is not gated by `may_compact_again`: the alternative is a dead turn. But every recovery summary that fails (it overflowed too, or any other error) adds one to `compact_failures` and sets `compact_failed_at`, like a failed automatic summary. Once the turn has `MAX_COMPACT_FAILURES`, recovery goes straight to level 3 (no request). So failed summary requests stay within #144's bound of 3 failures × 2 requests = 6 a turn, whether they came from the trigger or from recovery. A successful recovery counts nothing; the counters still reset when the turn ends.
- If the retried **agent** request overflows again after a successful recovery, the loop calls the hook with `attempt + 1`. Recovery then starts one level lower than the level that last succeeded (the session remembers it for the step), so a step never repeats a level.
- **No template nudge during overflow recovery.** A recovery summary that ignores the template is accepted as it is and logged `structured: false`. A nudge would be one more request per level, each of which could overflow too. #144's tool-call re-ask still applies (it is the same size as the request that just fit).
- Altogether, a step makes at most 3 summary attempts (≤ 6 requests, and fewer once the turn's failures run out) and 6 agent requests (the original, one after each of levels 0–2, one after level 3, and one after the last attempt) before it gives up.
- Every recovery emits `SessionEvent::Compaction(…)`, so the user sees a notice. `CompactResult::Compacted` gains `trigger: Auto | Manual | Overflow { level }`, which the TUI, stream-json and trajectory carry (§3.6).
- Before the turn's first request the same hook applies: an overflow there is caught by the loop like any other step.

### 3.6 State, persistence, trajectory and events

- `State::summary: Option<String>`: the last summary, set by `summarize_into`, cleared by `clear()`. It drives `rolling`.
- `SavedSession::summary: Option<String>` (`#[serde(default, skip_serializing_if = "Option::is_none")]`), restored on resume. **Older session files:** rolling mode stays off until the next compaction (no sniffing); the next summary is a full one.
- **Extend the existing records and events, no parallel ones.**
  - `TrajectoryRecord::Compact` gains `trigger` (`"auto" | "manual" | "overflow"`), `level` (overflow only), `tail_turns`, `structured: bool`, `rolling: bool`.
  - `TrajectoryRecord::CompactFailed` (#144) gains `trigger` and `level` (overflow only); its `reason` and `error` carry why, as today (an overflowed recovery summary is `reason: "failed"` with the provider's error).
  - The stream-json and web `compaction` event: `compacted: true` gains `trigger` (and `level`); `compacted: false` keeps `reason`/`error` and gains `trigger`.
  - All fields are additive: `trajectory_stats` ignores unknown fields.

### 3.7 Config keys

In `~/.marv/config.json` (`FileConfig`, camelCase like the existing keys) and the SDK's options:

| Key | Default | Meaning |
|---|---|---|
| `compactTailTokens` | `min(15000, 15% of the effective threshold)` | Verbatim tail budget (§3.2). `0` = no tail. |
| `compaction` | `"v1"` | `"v2"` turns on the template, the tail and rolling updates. **Temporary**, for the A/B: the default flips to `"v2"` in M7 and the key is removed one release after the flip. |

There is **no `compactWindow`**: `compactTokens` (#140) already moves the trigger, and evals use it. If the bench needs a trigger under the 40k minimum on small tasks, it gets an eval-only bypass (M6), not a user-facing key.

Overflow recovery isn't switchable: it only acts where today's behaviour is a dead turn.

**No `compactModel`** (decided, §9). The summary request hits the cache on the session's model; a different model would pay full input price on the same tokens and lose the cache, which is usually more expensive, not less.

## 4. What stays byte-stable for the prompt cache

- **The system prompt and tool specs are untouched.** Compaction never changes them; `/clear` is still the only rebuild.
- **Summary requests (normal and rolling) are `history + one user message`** with the same system prompt and tools: the cache hits as today. The tool-call re-ask changes only `tool_choice`; the nudge request extends the prefix.
- **After a compaction, the history is rewritten once** (the existing, documented exception). The tail turns are the same values, but their position changed, so the cache starts over once, as today. From there the history is append-only again.
- **Overflow levels are never cache hits.** That is accepted (error path only).
- `ToolCall::arguments` are moved, never re-serialized. `cache_control` breakpoints (`openai_compat.rs`, `needs_cache_breakpoints`) apply to the new last messages as usual.

## 5. Security, sandbox and taint

- **Taint lasts as long as the text does, and no compaction path changes that.** A summary can paraphrase untrusted text, and the tail can carry it verbatim. `summarize_into`, **overflow recovery and the level-3 drop** must never touch `TaintState`: dropping the turns that carried untrusted text doesn't untaint the session (the model may have acted on it, and the summary may repeat it). A session tainted before compaction stays tainted after it, in memory and in the saved session. **New guard:** `tests/guards/tests/provenance.rs::taint_survives_compaction`, covering the v1 summary, the v2 summary, an overflow recovery and the level-3 drop (fails if any clears or skips saving the taint).
- **Fences survive in the tail.** Tail tool results keep their `Provenance` fences byte for byte, and nested instruction notes keep #62's fencing. The guard checks that a fenced web result in the tail is unchanged after compaction.
- **The summary stays a user-role message** (decided, §9). That is today's behaviour, and v2 doesn't add a new channel: an injection that steers the summary could already have steered the next reply. It does mean a summary written from injected content can read like user instructions. Mitigation, kept as today: the `COMPACTED_PREFIX` says it's a summary. Not done now: fencing the summary as data, because it would lose its instruction value and the model wrote it. To revisit with an injection eval.
- **The overflow note and level 3 drop history without asking.** That loses context, not safety. The user sees a `Compaction` notice with `trigger: overflow`.
- No sandbox or approval changes. Compaction runs no tools: a tool call in a summary reply is still "not a summary".

## 6. Test plan

Unit tests (`crates/marv-agent/src/compact.rs`, `ScriptedProvider`):
- `tail_start`, one test per return case (§3.2):
  - already fits → `0`;
  - empty with budget > 0 → `0`; empty with budget 0 → `0`;
  - doesn't fit, snapped back to index 0 → `len`;
  - over the 2× cap → `len`;
  - budget `0` on a non-empty history → `len`;
  - otherwise the snapped index, never at a `Tool` turn; the budget is respected and snapping moves earlier.
- `compacted_history` v2: the ACK is present iff the tail is empty or starts with `User`; tail turns are `==` the originals; the plan text sits between the summary and the ACK, as today.
- `summary_request(…, rolling)`: full vs rolling instruction; the prefix equals the history (cache); focus is appended.
- Template check and request bounds:
  - 5/7 headings pass; 4/7 after a good text reply trigger exactly one nudge, whose prefix = the first request + the reply;
  - a second unstructured reply is accepted with `structured: false`; a nudge reply that errors, is cut off or calls a tool keeps the first reply;
  - a tool call → the `tool_choice: None` re-ask, and an unstructured re-ask reply gets **no** nudge (at most 2 requests per summary, in every combination).
- All existing `compact.rs` tests keep passing under `compaction: "v1"`, including #144's re-ask test, and v2 equivalents are added.
- `fit_for_summary`: drops oldest whole exchanges, keeps the previous summary, shortens a giant result, never splits call/result.
- The budget: the system prompt, tool specs, instruction and reply reserve are subtracted (a large system prompt gives a smaller history); the `window_refusal` ratio scales it. A level whose budget is below `MIN_HISTORY_BUDGET` is skipped.

Core (`crates/marv-core`): `estimate_tokens` keeps today's rounding (`usage.rs`'s test moves with it); the four call sites give the same numbers as before (the TUI's UTF-16 count included, or its difference noted).

Provider (`crates/marv-provider/src/errors.rs`):
- `is_context_overflow` against **real captured messages** (fixtures in `crates/marv-provider/tests/fixtures/overflow/*.txt`, one per upstream), plus negatives: 429 text, "tool_use ids must be unique", the 404 model message, the HTML page.
- In `tests/openai_compat.rs`: a window refusal the adapter refits successfully never reaches the loop as an error; one that still fails after the refit is an `Error` that `is_context_overflow` matches.

Loop (`crates/marv-agent/src/agent.rs`):
- An overflow `Error` with `on_overflow` → `Overflow` event, the same step is retried, and the history has nothing from the failed reply.
- `Failed` → `Error` + `Done(Error)`. No hook → as today.
- The attempt counter climbs per step and resets after a good step; at most 5.
- Esc during the hook → `Aborted`.

Session (`crates/marv-session/src/session.rs` tests, the "Done when" fake-provider test):
- **The issue's acceptance test:** a fake provider answers the first request with an OpenRouter overflow error, then a summary, then a reply. The turn ends `Done(End)` with one `Compaction { trigger: Overflow { level: 0 } }` and no user action. Under v1 (PR (c)) and v2.
- Climbing levels: overflow, overflow, overflow → level 3 drop, and the turn still finishes.
- **The summary request overflows, all the way down:**
  - the agent request overflows;
  - the level 0, 1 and 2 summary requests each overflow too;
  - level 3 drops old messages without a request;
  - the retried step succeeds.

  Expected: one hook call, 3 summary requests, each smaller than the last (the recorded requests' estimated sizes go down); `compact_failures` is 3; `Compaction { trigger: Overflow { level: 3 } }`; the turn ends `Done(End)`.
- **Failures shared with #144:** after 2 failed automatic summaries in the turn, an overflow's recovery sends one summary request, then goes to level 3 when it fails. A turn's failed summary requests never exceed 6.
- **The fixed content exceeds the window:** a system prompt larger than the window. The hook returns `Failed` without sending a summary request. The loop emits `LoopEvent::Error` with the "larger than this model's context window" text (not the provider's), and the turn ends `Done(Error)` after 1 request.
- **The fixed content fits but leaves no room:** the result is `Failed` with the "only about K tokens … left" text; no summary request, no drop; `Done(Error)`.
- **The 35% target is too small, but the whole window has room:** the last attempt drops to the whole-window budget, the retried step succeeds, and the turn ends `Done(End)`.
- **No nudge in recovery:** a recovery summary without the template headings is accepted with `structured: false`. In the "all the way down" test, the total is still 3 summary requests.
- `OverflowOutcome` in the loop: `Recovered` → the same step is requested again; `Failed(text)` → `LoopEvent::Error(text)` + `Done(Error)`; `Stopped` → `Done(Aborted)` with no `Error` event.
- A non-overflow error during a recovery summary (rate limited) ends the turn with that error; Esc during it → `Interrupted`.
- Rolling: a second compaction's request contains the rolling instruction; `State::summary` is replaced; save, then resume, keeps it.
- Mid-turn with a tail: the history ends with the tail then `MID_TURN_NOTE`; the next request is valid (every call answered).
- Nested instruction files: a note in a tail result isn't given again on the next touch of its folder; a note that was summarized away is.
- Plan carried: the existing `mid_turn_compaction_keeps_the_plan_then_the_mid_turn_note` and friends pass in v2.
- Trajectory and events: `Compact` has `trigger`, `tail_turns`, `structured`, `rolling`; `CompactFailed` and the stream-json event have `trigger`.
- Config: `compactTailTokens: 0` gives the v1 shape with the v2 template; the default tail follows `min(15k, 15% of the threshold)` for a 32k window and for the 100k default.

Guard (`tests/guards/tests/provenance.rs`): `taint_survives_compaction` (§5), proven by reverting.

Oracle: compaction scenarios in `tests/oracle/` run under `compaction: "v1"` until the oracle gets v2 expectations. This is a deliberate behaviour change, noted in `compact.rs`'s module doc with the issue number, per AGENTS.md.

## 7. Eval plan (no runs without approval: they cost money)

**Free first:** run v2 through #140's replay of real long turns (the same trajectories), and re-read its SWE-bench trajectories for repeated reads after a `compact` record. That gives a first v1-vs-v2 number without paid runs.

**What to measure, v1 vs v2 (`compaction` key), same model and seeds:**
- **Repeated reads after a compaction** (the issue's metric): `read_file` calls, within the 10 steps after a `compact` record, on a path whose read (same path, overlapping range) appears before it. New counter in `trajectory_stats.rs`.
- Pass rate, compactions per run, total tokens and cost (summary requests included: they're `request` records), turns ending `error`, nudges needed (`structured` false on the first reply).
- Overflow recoveries (should be 0 in normal runs; any > 0 is worth reading).

**Tasks:**
- None of the 21 tasks in `evals/tasks/` is long enough to compact twice. The biggest are `callback-to-promise`, `multi-ask`, `ts-make-async` and `refactor-signature`.
- Proposal: run those four with `compactTokens` set low (40k, the minimum; lower only through M6's eval-only bypass) so each compacts 2+ times. Also add **one new task**, `long-haul`: ~15 files to read and change, a test suite to run between steps, so it compacts naturally. `bench.py --verify` must pass for it.
- `bench.py` has no way yet to put `compactTokens`/`compaction` into the throwaway `MARV_CONFIG_DIR` config: `--marv-config key=value` is added in M6 (shared with #66).
- Suggested size: 5 tasks × 2 variants × `--repeat 5` = 50 runs on haiku, with `--budget` set. Ask for the budget before running.

## 8. Implementation plan

One PR each, in this order. Only the last needs a paid eval.

- **(a) M1 Overflow detection.** `is_context_overflow` on top of `window_refusal`, matching only what escapes the #116 refit; fixtures from real captured errors; `marv-core::estimate_tokens`, with the four copies using it. No behaviour change.
- **(b) M2 Loop hook.** `AgentOptions::on_overflow`, `LoopEvent::Overflow`, the per-step attempt counter (at most 5), loop tests. Inert when unset.
- **(c) M5-lite Overflow recovery, on today's v1 summary.** A bug fix, no A/B: the session hook with levels 0–3 and the last attempt, `fit_for_summary` and the budgets, the #144 failure counting, `CompactResult` `trigger`, the `CompactFailed`/event `trigger`, the TUI and headless notice text, the acceptance test, and the guard `taint_survives_compaction`.
- **(d) M3 Pure v2 helpers.** `tail_start`, the v2 `compacted_history` (ACK rule), `summary_request(…, rolling)`, the template text and check. Unit tests. Not wired.
- **(e) M4 v2 wiring behind `compaction: "v2"`.** `State::summary`, `SavedSession::summary`, the nudge with the 2-request bound, the nested-instructions tail rule, the `Compact` trajectory fields, `compactTailTokens`. The default stays `"v1"`.
- **(f) M6 Eval support** (shared with #66): `bench.py --marv-config`, the eval-only `compactTokens` bypass if needed, the `trajectory_stats` repeated-reads counter, the `long-haul` task with `--verify`, the replay run. Then **ask** to run §7.
- **M7 Flip the default to v2** (paid eval) if it shows fewer repeated reads with no pass-rate loss. Update `docs/subsystems/agent.md` and `session.md`. Remove `compaction: "v1"` one release later.

## 9. Decisions (2026-10-20)

1. **The summary covers the verbatim tail too.** One cache-hitting request; the overlap is cheap.
2. **No `compactModel`.** The cached same-model summary is usually cheaper than an uncached cheap model.
3. **The default tail is `min(15k, 15% of the effective threshold)`**, the threshold being `min(85% × window, compactTokens)`. `compactWindow` is dropped; `compactTokens` covers it.
4. **Level 3 drops the oldest history with a visible notice and a note to the model**, without asking: headless runs have no one to ask.
5. **A second unstructured summary is accepted** and logged `structured: false`; failing would leave a nearly full context.
6. **The summary stays a user-role message**, not fenced as data. Revisit with an injection eval.
7. **A temporary `compaction: "v1" | "v2"` key, default `"v1"`**, removed one release after the default flips.
8. **Recoveries per step: 5** (levels 0–3 and the whole-window last attempt). The draft's §6 said 4; §3.5 was right.
