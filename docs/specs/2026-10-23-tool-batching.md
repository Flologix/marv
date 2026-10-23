# Tool batching: fewer, bigger tool calls

- Issue: FlologixAI/marv #215
- Status: **draft, for approval.** Decisions to make are in §8.
- Date: 2026-10-23, against `main` after 0.9.0 and #201–#205.
- Scope:
  - `crates/marv-session`: prompt;
  - `crates/marv-tools`: tool descriptions; `read_file` (phase 3 only);
  - `crates/marv-agent`: a once-per-turn nudge (phase 2);
  - `evals/`: a stats script.

## 1. Problem and evidence

A turn the maintainer watched ran a `bash` command about every half second, gathering one fact per model request. Each step is a full request: a round trip, plus the whole context sent again. Most of the context is cached, but a cached prompt still costs money and time.

Measured on 9,002 steps in 533 turns: ~/.marv trajectories (TUI and headless), the bench archive, and the SWE-bench runs `marv-base205` and `marv-pr205`. Almost all of them are GLM-5.3-flash.

- **Steps with exactly one call:** 89%. Of those single calls, 41% only read: `read_file`, `grep`, `glob`, or a read-only `bash` (`ls`, `cat`, `grep`, `git log/show/status`, `sed -n` …).
- **What batching could save:** a run of k consecutive single-call read-only steps could be one step. Folding every such run would save:

| group | steps/turn | 1-call steps | of those, read-only | steps saved | request time saved | prompt tokens saved |
|---|---|---|---|---|---|---|
| all | 16.9 | 89% | 41% | 20% | 16% | 18% |
| ~/.marv (TUI, headless) | 15.7 | 93% | 36% | 18% | 12% | 16% |
| SWE-bench base205 | 17.2 | 99% | 46% | 29% | 39% | 33% |
| SWE-bench pr205 | 24.6 | 98% | 48% | 31% | 34% | 30% |

  That's an **upper bound**: in many runs each read decides the next.
- **Run lengths** (k: how many runs): 1: 681, 2: 279, 3: 129, 4: 53, 5–10: 101, 11 or more: 33, the longest 51.
- **Models can batch:** 629 of the 964 multi-call steps are all reads. They just rarely do.
- **Typical runs:**
  - Paging one file 15 lines at a time: `read_file` at offsets 2302, 2317, 2332, 2342, while a page is 400 lines.
  - `git show` of five commits, one per request.
  - The same file read nine times in a row.
  - `glob **/*`, then `ls -F`, then `read_file` at 1, 401 and 801.

**Why it happens:**
- Nothing in the prompt says that calls in one reply can run together.
- `BASE_TOOLS_TEXT` says "Start narrow: grep for a name or glob for a file pattern, then read the relevant part of a file rather than whole large files". That's good advice for context size, but it pushes towards many small steps.
- The tool descriptions never mention combining calls.

**What runs together today** (`crates/marv-agent/src/agent.rs` `call_groups`, `MAX_PARALLEL` = 4; `crates/marv-session/src/runner.rs`):
- Consecutive `ToolKind::Read` calls that don't need approval: `read_file`, `grep`, `glob`, `plan`, `skill`, `mesh_list`, and MCP tools with `readOnlyHint` while the session is untainted.
- `agent` calls.
- `bash` is `Execute` and never runs in parallel. Neither do `web_fetch`, the edit tools or `process`.
- Results go into the history in call order, so the prompt cache holds.

## 2. Goals and non-goals

**Goals:**
- Fewer steps per turn, less request time and less cost, at the same resolve rate on SWE-bench and the same pass rate on the bench.
- Measured as #147 and #85 were.

**Non-goals:**
- Making `bash` parallel in this plan. A command's text can't safely tell whether it writes (`find -exec`, `sort -o`, redirections, `cd`), and `is_read_only_command` is documented as "never for a security decision". Revisit only if phase 1 makes multi-call replies common (§7).
- Raising the step limit, or changing reasoning effort.
- A new "batch" meta-tool. Models already send several calls in one reply; the gap is that they aren't told to.

## 3. Phase 0: measure (no behaviour change)

`evals/batching_stats.py` reads trajectories (a folder, or a SWE-bench run's `logs/*/trajectories`) and prints the table above for a run:
- steps per turn;
- the calls-per-step distribution;
- the share of single read-only steps;
- the run-length histogram;
- the request time and prompt tokens in those runs.

It uses the same read-only rule as the analysis (the `is_read_only_command` list, plus `sed -n`, `awk` and `nl`). It runs on the existing runs, so every later phase compares against a fixed baseline. It goes in with the first PR.

## 4. Phase 1: say it (prompt and tool descriptions)

The cheapest lever, and the one with the most reach: most of the waste is models not knowing.

1. **`BASE_TOOLS_TEXT`** (prompt.rs). Keep "start narrow" for *what* to read, and add *how*. Proposed replacement:

   > Look at the actual code before answering questions about it, and don't guess what a file contains. Start narrow: grep for a name or glob for a file pattern, then read the relevant part of a file rather than whole large files. Read-only calls sent together in one reply (read_file, grep, glob) run at the same time: when you already know the next few files, searches or pages you need, send them in one reply instead of one per step, and read a range that's big enough (a page is 400 lines) rather than a few lines at a time. Paths are relative to the project root.

2. **`read_file`'s description**, adding: "To read several files or several parts of one, send the calls together in one reply: they run at the same time."
3. **`bash`'s description**, adding: "Combine read-only checks into one command (e.g. `git log -3 --stat; git show abc123 | head -50`) rather than one command per step."

**Fixed per build:** the prompt and specs stay byte-stable within a session, so it costs one cache break per release, as with any prompt change.

**Oracle:** `BASE_TOOLS_TEXT` is shared with TypeScript Marv's prompt. Map the new sentence back in `marv_oracle::runner::MARV_RS_TEXT`, as #85's plan sentence is mapped. The description additions may need the same treatment; check the oracle's tool comparison.

**Measure.** Each arm is run twice (the 0.9.0 eval showed ±2 issues and ±40% total cost between single runs):
- SWE-bench Verified, the same 30 issues, GLM-5.3-flash, sandbox on: base vs phase 1. About $0.30 per run, so about $1.20 for the four runs.
- The bench (21 tasks × 2, GLM-5.3-flash). About $0.01 a run.

Report:
- steps per turn and the single read-only share (from phase 0's script);
- resolved and passed;
- total and median cost;
- request time.

**Ship rule:** fewer steps per turn (median and mean) and lower median cost, with resolved within 1 of base on both runs and the bench pass rate unchanged.

## 5. Phase 2: notice it (a once-per-turn nudge)

Only if phase 1 leaves long runs.

**Trigger:** after `BATCH_NUDGE_AFTER` (proposed 4) consecutive steps that each made one read-only call, the loop adds one note through the existing `after_step` hook (agent.rs; the session wires it), the way #90's announced-step note works. Proposed text:

> Marv: the last few steps each read one thing. If you already know the next files, searches or pages you need, send them together in one reply: read-only calls run at the same time.

- **Once per turn** (a `LoopState` flag, like `announced`), so it can't loop.
- **It costs no extra step:** it rides along with the next request, after the tool results, and is appended, so the prompt cache holds.
- **Not counted as untrusted:** it's Marv's own text.
- **Logged** to the trajectory (`batch_nudge`), and shown in the TUI as a dim `✻` line, like #90's.
- **Off switch:** `"batchNudge": false` in the user's config, for comparing.
- **Read-only** here uses the same rule as phase 0.
- Not for subagents in the first version; measure the main agent first.

**Measure:** phase 1 vs phase 1 + 2, the same runs and ship rule as phase 1.

## 6. Phase 3: make bigger reads natural (`read_file` ranges)

Only if the data still shows one file paged through in small steps.

`read_file` would take an optional `ranges: [[offset, limit], …]` (at most 4, the same 2,000-line total cap), returning each range with its line numbers and a `…` between them.

- One call then covers "the function at 1713 and its caller at 2302", which models now do in two steps.
- It's a schema change, so it needs an oracle note and a cache break at release. It's additive: `offset`/`limit` keep working.

**Measure:** the same as phase 1.

## 7. Later, and only with evidence

- **Parallel read-only `bash`.** This only pays once models send several bash calls per reply.
  - It would need a strict allowlist parser: a single simple command from a short list; no redirections, `;`, `&&`, subshells or `$(…)`; no `find -exec`.
  - It must never cover `network: true`, `sandbox: false` or anything that asks.
  - The #81 failing-command guard would have to record calls in order.
- **Subagents:** apply phase 1 to the subagent prompt if phase 1 shows gains.

## 8. Decisions

1. **Phase 1 now:** prompt plus two description sentences, measured with two runs per arm (about $1.20 SWE-bench plus the bench).
2. **Phase 2 trigger:** after 4 single read-only steps, once per turn, on by default if it passes the ship rule. Alternatives: 3, or 5.
3. **Phase 3:** decide after phase 2's numbers.
4. **Parallel bash:** stays out of scope until multi-call replies are common (§7).

## 9. PRs

1. Phase 0 + phase 1: `evals/batching_stats.py`, the prompt and descriptions, oracle mapping, tests that pin the new sentences, CHANGELOG with the measurement.
2. Phase 2 (if needed): `after_step` nudge, config switch, trajectory record, TUI line, tests (fires after 4, once per turn, not for subagents, not after a non-read call).
3. Phase 3 (if needed): `read_file` `ranges`.
