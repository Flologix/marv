# Compaction v2: structured rolling summary, verbatim tail, overflow recovery

- Issue: FlologixAI/marv #63 (tracked privately as marv-rs #17)
- Status: **draft, for approval.** Nothing is implemented until this is approved.
- Date: 2026-10-14
- Scope: `crates/marv-agent` (compaction, loop hook), `crates/marv-provider` (overflow detection), `crates/marv-session` (wiring, state, config, trajectory).

The issue was written against the TypeScript build (`src/compact.ts`, `src/provider/errors.ts`). This spec is grounded in the Rust code on `main`. No local opencode checkout exists (only the `opencode` binary under mise), so opencode's behaviour is taken from the issue text and is marked as such.

## 1. Problem and evidence

Today, compaction replaces the whole model history with one free-form summary when the last request used 85% of the window. That has three weaknesses (from the issue):

1. **The work in progress is summarized away.** The file the model just read and the failing test output it is acting on become one line of prose. After compaction the model reads them again and repeats steps.
2. **Detail decays.** The second compaction summarizes "summary + new history" with the same open-ended instructions, so each round loses a little more. Long tasks degrade.
3. **No recovery from overflow.** If a request is rejected as too long (the 85% check was off, or one giant tool result landed), the turn ends with a provider error. `marv_provider::errors::is_context_overflow` is a `todo!`.

Evidence still to collect (see §7): the eval in §8 measures repeated reads after compaction. There is no trajectory-derived number in the issue. This spec doesn't claim a baseline.

## 2. Current Rust design

| What | Where |
|---|---|
| Threshold `COMPACT_AT = 0.85`, `should_compact` | `crates/marv-agent/src/compact.rs` |
| Summary instructions `INSTRUCTIONS`, `summary_request(history, focus)`: the same history plus one user message | `compact.rs` |
| `summarize(SummarizeOptions)`: streams the reply. Errors (with the history kept) on a tool call, a `length` cut-off, an empty reply, a provider error or a cancel. `Retry` clears the text. | `compact.rs` |
| `compacted_history(summary, plan_text)`: `[User(COMPACTED_PREFIX + summary [+ plan]), Assistant(COMPACTED_ACK)]` | `compact.rs` |
| `MID_TURN_NOTE`: pushed after a mid-turn compaction | `compact.rs`, pushed by `MarvSession::compact_if_full` |
| v2 stubs: `V2_SECTIONS`, `V2_TAIL_TOKENS = 15_000`, `V2_OVERFLOW_TARGETS = [0.70, 0.50, 0.35]`, `tail_start` (todo), `rolling_update_request` (todo) | `compact.rs` |
| Overflow detection stub: `is_context_overflow(message) -> bool` (todo) | `crates/marv-provider/src/errors.rs` |
| Provider errors reach the loop as `ProviderEvent::Error(String)`. HTTP errors are built by `http_error(status, detail, …)` = `"<label> returned 400.\n<detail>"` | `errors.rs`, `openai_compat.rs` (`emit.send(ProviderEvent::Error(http_error(…)))`) |
| Retries: only 429/502/503/504 and network drops; a 400 is never retried | `crates/marv-provider/src/retry.rs` |
| Loop: `Phase::Full` calls `AgentOptions::on_full` after a step's calls are answered, then emits `LoopEvent::Compacted`. `StepOutcome::Error(msg)` emits `LoopEvent::Error` and finishes `StopReason::Error` | `crates/marv-agent/src/agent.rs` (`LoopState::advance`, `decide`) |
| Session: `nearly_full()` (last prompt+completion ≥ 85% of `context_length()`), `try_compact()` (also checks `mid_turn_compact_failed`), `compact_if_full(…, mid_turn)`, `summarize_into(focus, …)`, which swaps the shared `History` **in place**, clears the MCP `announced` list and `loaded` schemas, sets `last = None`, and writes `TrajectoryRecord::Compact { turn, summary }` | `crates/marv-session/src/session.rs` |
| Before a turn: `if self.try_compact() { … compact_if_full(…, false) }`. Mid-turn: `options.on_full`. `/compact [focus]`: `MarvSession::compact` | `session.rs` (`run_turn` body around the "Nearly out of context" comment) |
| The plan (#15): the session passes `plan_text(items)` for an open plan, appended to the summary message | `crates/marv-tools/src/plan.rs` (`plan_text`, `plan_open`), `session.rs::summarize_into` |
| Window: `MarvSession::context_length()` = `provider.context_length()` or the model list's `ModelInfo::context` | `session.rs`, `crates/marv-provider/src/models.rs` |
| Token estimate: 4 chars per token, in three copies: `marv_session::usage::estimate_tokens`, `marv_mcp::deferred::estimate_tokens`, and the one `tail_start`'s doc promises | — |
| Ollama: `num_ctx` is fixed per session. Past it, Ollama **shifts the window silently** (no error); `reply_cap` keeps `num_predict` inside the window | `crates/marv-provider/src/ollama.rs` header |

What works and must stay: mid-turn compaction (`on_full`), `/compact [focus]`, a summary request that reuses the cached prefix, the transcript untouched, the failure rules (a failed mid-turn summary isn't retried that turn; Esc during the summary stops the turn), the plan carried verbatim, MCP re-announcement.

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
- The **summary covers the whole history, tail included.** Asking the model to summarize "everything before message N" is unreliable, and it would need a different request (no cache hit). The overlap costs some summary tokens and is harmless: the summary says what happened, the tail shows it.

### 3.2 `tail_start(history, tail_tokens) -> usize`

- Walk back from the end, summing an estimate per turn (chars / 4, as `usage::estimate_tokens`; tool calls count their name + arguments). Stop before the turn that would push the sum over `tail_tokens`.
- **Snap to a safe boundary:** move the start *earlier* to the nearest index whose turn is `User` or `Assistant`. A `Tool` turn is never first, so a result is never split from the assistant turn that called it. Moving earlier can exceed the budget by one step's results; the cap below bounds it.
- **Cap:** if snapping would make the tail larger than `2 × tail_tokens`, or the tail would be the whole history, return `history.len()` (no tail). A giant tool result then goes into the summary, not the tail.
- `tail_start == 0` (the whole history fits the tail) means there is nothing worth compacting: `CompactFailure::Empty`, "Nothing to compact yet.", as for an empty history today. Exception: overflow recovery (§3.5), which shrinks the tail instead.
- Pure, in `marv-agent/src/compact.rs`. The 4-chars estimate moves into `marv-core` as `estimate_tokens` (types-only crate, no I/O), and the two existing copies use it.

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

**The template nudge.** A reply counts as structured when at least 5 of the 7 `## <section>` headings are present, matched at line start and ignoring case. If it isn't, **one** follow-up request goes out: the same request + the unstructured reply as an assistant turn + `TEMPLATE_NUDGE` ("Rewrite that summary using exactly the sections asked for: …"). It is still a cache hit up to the first reply. If the second reply is still unstructured, **accept it anyway** (a summary beats none) and record `structured: false` in the trajectory. Cut-off, tool-call, empty and error rules apply to both replies, as today.

### 3.4 Plan, instructions, MCP

- **The plan (#15)** is appended verbatim after the summary, as today (`plan_text`, only for an open plan). The template doesn't ask the model to restate it.
- **Instructions:** `AGENTS.md`, memory and skills are in the system prompt, which is never summarized, so there is nothing to re-inject. The issue's "re-inject instructions separately" is already true in Marv by construction.
- **MCP:** unchanged. `announced` is cleared and the next message (or the mid-turn note) announces the tools again.

### 3.5 Overflow recovery

**Detection:** `marv_provider::errors::is_context_overflow(message: &str) -> bool`, a case-insensitive regex list over the whole error string (it includes the HTTP detail). Seed patterns, **to be checked against real captured messages before merge** (the stub already says so):

- OpenAI and OpenRouter: `maximum context length is \d+ tokens`, `context_length_exceeded`, `exceeds the context window`
- Anthropic upstream: `prompt is too long`
- Gemini upstream: `input token count .* exceeds the maximum`
- generic: `context (length|window)`, together with `exceed|too long|too large`
- HTTP 413 from OpenRouter: `returned 413`

A second pure helper, `overflow_window(message) -> Option<u64>`, parses the stated limit ("maximum context length is 131072 tokens") when the model list didn't give one.

**Ollama (assumption, to verify):** Ollama's native API truncates past `num_ctx` instead of failing (`ollama.rs` header: "Ollama shifts the window and loses the prompt's start"). So overflow recovery is effectively OpenRouter-only. Ollama stays covered by the 85% trigger, which uses its exact `prompt_eval_count`. If a captured Ollama error does exist, its wording is added to the list.

**Loop hook (marv-agent):** a new optional `AgentOptions::on_overflow: Option<OnOverflow>`.

```rust
pub type OnOverflow = Arc<dyn Fn(String /* error */, u32 /* attempt, from 0 */) -> BoxFuture<'static, bool> + Send + Sync>;
```

In `LoopState::decide`, `StepOutcome::Error(message)` where `is_context_overflow(&message)` and `on_overflow` is set:
- Nothing from the failed reply enters the history. It has no text or calls. If it somehow has partial text, that is dropped too, as for a `Retry`. Today `decide` pushes `reply.assistant_turn()` before matching `StepOutcome::Error`, so the overflow check has to come before that push.
- Emit `LoopEvent::Overflow { attempt }` (new) instead of `LoopEvent::Error`.
- `on_overflow(message, attempt)`: if it returns true, `Phase::Request` again (same step, the step counter is not advanced); if false, the error path as today (`LoopEvent::Error`, `StopReason::Error`).
- `attempt` counts per step and resets after a step that got a reply. At most `V2_OVERFLOW_TARGETS.len() + 1` = 4 recoveries per step.
- A cancel during the hook ends `Aborted`, as `on_full` does.

marv-agent depends on marv-provider already, so it can call `is_context_overflow` directly.

**Session hook (marv-session):** `attempt` picks the level:

| attempt | What the session does |
|---|---|
| 0 | Normal v2 compaction, but the **summary request is built from a cut-down history** that fits `0.70 × window` (the full history just overflowed, so reusing the prefix would overflow again). |
| 1 | Same, target `0.50 × window`, tail budget halved. |
| 2 | Same, target `0.35 × window`, no tail. |
| 3 | **No summary:** keep the previous summary (if any) + the newest exchanges that fit `0.35 × window`, drop the rest, and add a note: "(Automatic note from Marv) Older messages were dropped to fit the context window; ask the user if you need something from them." |

- **Cutting down for the summary request** (`fit_for_summary(history, budget) -> Vec<ChatTurn>`, pure, in compact.rs): drop the oldest whole exchanges after the previous summary (never splitting call/result). Any single tool result still larger than `budget / 4` is shortened to head + tail with `[… cut for the summary …]`, reusing `marv_tools::output`'s head/tail shape. This request is never a cache hit; that is accepted, because it only happens after an error.
- **Window when unknown** (custom provider, model list not loaded): `overflow_window(message)`. If that fails too, the size of the request that overflowed (`estimate_tokens` of the history) stands in for the window.
- **An overflow during an ordinary 85% summary** (`summarize` returns `Err(msg)` with `is_context_overflow(msg)`) goes straight to level 0 instead of `NotCompacted`.
- Every recovery emits `SessionEvent::Compaction(…)`, so the user sees a notice. A new `CompactResult::Compacted` field `trigger: Auto | Manual | Overflow { level }` tells the UI and trajectory which it was.
- Before the turn's first request the same hook applies: an overflow there is caught by the loop like any other step.
- The `mid_turn_compact_failed` rule doesn't block overflow recovery: the alternative is a dead turn.

### 3.6 State and persistence

- `State::summary: Option<String>`: the last summary, set by `summarize_into`, cleared by `clear()`. It drives `rolling`.
- `SavedSession::summary: Option<String>` (`#[serde(default, skip_serializing_if = "Option::is_none")]`), restored on resume. **Older session files:** rolling mode stays off until the next compaction (no sniffing); the next summary is a full one.
- `TrajectoryRecord::Compact` gains `trigger` (`"auto" | "manual" | "overflow"`), `level` (overflow only), `tail_turns`, `structured: bool`, `rolling: bool`. These fields are additive: `trajectory_stats` ignores unknown fields.

### 3.7 Config keys

In `~/.marv/config.json` (`FileConfig`, camelCase like the existing keys) and the SDK's options:

| Key | Default | Meaning |
|---|---|---|
| `compactTailTokens` | `15000`, capped at 15% of the window | Verbatim tail budget. `0` = no tail. |
| `compactWindow` | unset | Caps the window compaction uses: `min(model window, compactWindow)`. Lets evals force compactions on cheap models with a large window. Doesn't change `num_ctx` (Ollama keeps `contextLength`). |
| `compaction` | `"v2"` | `"v1"` restores today's prompt and shape (no template, no tail, no rolling). **Temporary**, for the A/B eval. Removed one release after v2 ships. |

Overflow recovery isn't switchable: it only acts where today's behaviour is a dead turn.

**No `compactModel` in v2.** The summary request hits the cache on the session's model: the prompt is ~85% of the window at cached-input prices. A different model would pay full input price on the same tokens and lose the cache, which is usually more expensive, not less. The one place a cheap model would help is overflow levels 0–2 (no cache hit there either). Revisit only if the overflow numbers say so (open question 2).

## 4. What stays byte-stable for the prompt cache

- **The system prompt and tool specs are untouched.** Compaction never changes them; `/clear` is still the only rebuild.
- **Summary requests (normal and rolling) are `history + one user message`** with the same system prompt and tools: the cache hits as today. The nudge request extends that prefix.
- **After a compaction, the history is rewritten once** (the existing, documented exception). The tail turns are the same values, but their position changed, so the cache starts over once, as today. From there the history is append-only again.
- **Overflow levels are never cache hits.** That is accepted (error path only).
- `ToolCall::arguments` are moved, never re-serialized. `cache_control` breakpoints (`openai_compat.rs`, `needs_cache_breakpoints`) apply to the new last messages as usual.

## 5. Security, sandbox and taint

- **Taint lasts as long as the text does, and compaction doesn't change that.** A summary can paraphrase untrusted text, and the tail can carry it verbatim. `summarize_into` must never touch `TaintState`. A session tainted before compaction stays tainted after it, in memory and in the saved session. **New guard:** `tests/guards/tests/provenance.rs::taint_survives_compaction` (fails if compaction clears or skips saving the taint).
- **Fences survive in the tail.** Tail tool results keep their `Provenance` fences byte for byte. The guard checks that a fenced web result in the tail is unchanged after compaction.
- **The summary is model text placed in a user-role message.** That is today's behaviour, and v2 doesn't add a new channel: an injection that steers the summary could already have steered the next reply. It does mean a summary written from injected content can read like user instructions. Mitigation, kept as today: the `COMPACTED_PREFIX` says it's a summary. Not proposed: fencing the summary as data, because it would lose its instruction value and the model wrote it. Flagged for review, not changed.
- **The overflow note and level 3 drop history without asking.** That loses context, not safety. The user sees a `Compaction` notice with `trigger: overflow`.
- No sandbox or approval changes. Compaction runs no tools: a tool call in a summary reply is still "not a summary".

## 6. Test plan

Unit tests (`crates/marv-agent/src/compact.rs`, `ScriptedProvider`):
- `tail_start`: never starts at a `Tool` turn; the budget is respected; snapping moves earlier; the 2× cap gives no tail; an all-fits history → 0; empty → 0.
- `compacted_history` v2: the ACK is present iff the tail is empty or starts with `User`; tail turns are `==` the originals; the plan text sits between the summary and the ACK, as today.
- `summary_request(…, rolling)`: full vs rolling instruction; the prefix equals the history (cache); focus is appended.
- Template check: 5/7 headings pass, 4/7 trigger exactly one nudge, the nudge request's prefix = the first request + the reply, a second unstructured reply is accepted with `structured: false`.
- All existing `compact.rs` tests keep passing under `compaction: "v1"`, and v2 equivalents are added.
- `fit_for_summary`: drops oldest whole exchanges, keeps the previous summary, shortens a giant result, never splits call/result.

Provider (`crates/marv-provider/src/errors.rs`):
- `is_context_overflow` against **real captured messages** (fixtures in `crates/marv-provider/tests/fixtures/overflow/*.txt`, one per upstream), plus negatives: 429 text, "tool_use ids must be unique", the 404 model message, the HTML page.
- `overflow_window` parses the stated limits.

Loop (`crates/marv-agent/src/agent.rs`):
- An overflow `Error` with `on_overflow` → `Overflow` event, the same step is retried, and the history has nothing from the failed reply.
- `on_overflow` false → `Error` + `Done(Error)`, as today. No hook → as today.
- The attempt counter climbs per step and resets after a good step; at most 4.
- Esc during the hook → `Aborted`.

Session (`crates/marv-session/src/session.rs` tests, the "Done when" fake-provider test):
- **The issue's acceptance test:** a fake provider answers the first request with an OpenRouter overflow error, then a summary, then a reply. The turn ends `Done(End)` with one `Compaction { trigger: Overflow { level: 0 } }` and no user action.
- Climbing levels: overflow, overflow, overflow → level 3 drop, and the turn still finishes.
- Rolling: a second compaction's request contains the rolling instruction; `State::summary` is replaced; save, then resume, keeps it.
- Mid-turn with a tail: the history ends with the tail then `MID_TURN_NOTE`; the next request is valid (every call answered).
- Plan carried: the existing `mid_turn_compaction_keeps_the_plan_then_the_mid_turn_note` and friends pass in v2.
- Trajectory: the `Compact` record has `trigger`, `tail_turns`, `structured`, `rolling`.
- Config: `compactTailTokens: 0` gives the v1 shape with the v2 template; `compactWindow` caps `nearly_full`.

Guard (`tests/guards/tests/provenance.rs`): `taint_survives_compaction` (§5), proven by reverting.

Oracle: compaction scenarios in `tests/oracle/` run under `compaction: "v1"` until the oracle gets v2 expectations. This is a deliberate behaviour change, noted in `compact.rs`'s module doc with the issue number, per AGENTS.md.

## 7. Eval plan (no runs without approval: they cost money)

**What to measure, v1 vs v2 (`compaction` key), same model and seeds:**
- **Repeated reads after a compaction** (the issue's metric): `read_file` calls, within the 10 steps after a `compact` record, on a path whose read (same path, overlapping range) appears before it. New counter in `trajectory_stats.rs`.
- Pass rate, compactions per run, total tokens and cost (summary requests included: they're `request` records), turns ending `error`, nudges needed (`structured` false on the first reply).
- Overflow recoveries (should be 0 in normal runs; any > 0 is worth reading).

**Tasks:**
- No existing task in `evals/tasks/` is long enough to compact twice. The issue's "#15's long task" isn't in this repo. The biggest are `callback-to-promise`, `multi-ask`, `ts-make-async` and `refactor-signature`.
- Proposal: run those four with `compactWindow` set low (e.g. 24k on `anthropic/claude-haiku-5.5`, the bench default) so each compacts 2+ times. Also add **one new task**, `long-haul`: ~15 files to read and change, a test suite to run between steps, so it compacts naturally at ~32k. `bench.py --verify` must pass for it.
- `bench.py` needs a way to put `compactWindow`/`compaction` into the throwaway `MARV_CONFIG_DIR` config (`--marv-config key=value`), a small bench change in milestone M6.
- Suggested size: 5 tasks × 2 variants × `--repeat 5` = 50 runs on haiku, with `--budget` set. Ask for the budget before running.

## 8. Milestones (PR-sized)

1. **M1 Overflow detection:** `is_context_overflow`, `overflow_window`, fixtures from real captured errors. `marv-core::estimate_tokens` (the three copies de-duplicated). No behaviour change.
2. **M2 Loop hook:** `AgentOptions::on_overflow`, `LoopEvent::Overflow`, loop tests. Unset, it changes nothing.
3. **M3 Pure v2 helpers:** `tail_start`, `fit_for_summary`, the v2 `compacted_history` (ACK rule), `summary_request(…, rolling)`, the template text and check. Unit tests. Not wired.
4. **M4 Session wiring, v2 behind `compaction: "v2"`:** `State::summary`, `SavedSession::summary`, the nudge, the trajectory fields, `compactTailTokens`, `compactWindow`. The default stays `"v1"` in this PR.
5. **M5 Overflow recovery in the session:** the hook levels 0–3, the acceptance test, `CompactResult` trigger, the TUI and headless notice text. Also the guard `taint_survives_compaction`.
6. **M6 Eval support:** the `trajectory_stats` repeated-reads counter, `bench.py --marv-config`, the `long-haul` task with `--verify`. Then **ask** to run §7.
7. **M7 Flip the default to v2** if the eval shows fewer repeated reads with no pass-rate loss. Update `docs/subsystems/agent.md` and `session.md`. Remove `compaction: "v1"` one release later.

## 9. Open questions for the user

1. **Should the summary also cover the verbatim tail, or only what's before it?** *Recommendation: cover everything.* One cache-hitting request; the overlap is cheap. "Summarize only before message N" needs a non-cached request and is unreliable.
2. **Should there be a cheaper `compactModel`?** *Recommendation: no, not in v2.* The cached same-model summary is usually cheaper than an uncached cheap model. Reconsider only for overflow levels if eval logs show many overflows.
3. **Should the default tail be 15k tokens, fixed or relative to the window?** *Recommendation: `min(15k, 15% of the window)`.* On a 32k Ollama window, 15k would leave too little room after the summary. The config key overrides it.
4. **Should level 3 drop history without a summary, or stop the turn and ask?** *Recommendation: drop with a visible notice and the note to the model.* Headless runs (`marv -p`) have no one to ask, and `/undo` doesn't apply to context anyway.
5. **A second unstructured summary: accept it or fail the compaction?** *Recommendation: accept, and log `structured: false`.* Failing would leave a nearly full context, which is worse.
6. **Should the summary stay a user-role message (as today), or become data-fenced?** *Recommendation: keep it as today* (§5). Revisit with the injection eval suite.
7. **Should the temporary `compaction: "v1"` key exist?** *Recommendation: yes, for the A/B only; remove it one release after v2 is the default.*
