# Tool batching: fewer, bigger tool calls

- Issue: FlologixAI/marv #215
- Status: **phase 1 approved 2026-10-24** (the maintainer: "let's do the eval"); phases 2 and 3 are decided on phase 1's numbers. The decisions are in §8.
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
- Consecutive `ToolKind::Read` calls that don't need approval: `read_file`, `grep`, `glob`, `plan`, `skill`, `mesh_list`, `github`'s reads (its writes ask), and MCP tools with `readOnlyHint` while the session is untainted.
- `agent` calls: they're `Execute`, but `parallel()` is true.
- `read_file`, `grep` and `glob` never ask (no `needs_approval`, no protected target), so they always qualify. Only `github` and MCP reads can fall out of a group by asking.
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

   > Look at the actual code before answering questions about it, and don't guess what a file contains. Start narrow: grep for a name or glob for a file pattern, then read the relevant part of a file rather than whole large files. When you already know the next few files, searches or pages you need (up to about four), send those calls together in one reply: read_file, grep and glob calls in one reply run at the same time. When paging through a file, continue from the offset the result gives rather than a few lines at a time; still read only the parts you need. Paths are relative to the project root.

   The cap ("up to about four") matches `MAX_PARALLEL` and keeps one reply from pulling in 1,600+ lines, which would bring compaction early. "Still read only the parts you need" keeps "start narrow" in force.

2. **`read_file`'s description**, adding: "To read several files or several parts of one, send the calls together in one reply: they run at the same time."
3. **`bash`'s description**, adding: "Combine read-only checks into one command with `&&` or `;` (e.g. `git log -3 --stat; git show abc123 | head -50`) rather than one command per step."

   An "always" answer is remembered for the exact command (bash.rs), so a combined command won't match one remembered alone. That only matters for commands that ask, and read-only checks in the sandbox don't.

**Fixed per build:** the prompt and specs stay byte-stable within a session, so it costs one cache break per release, as with any prompt change.

**Oracle:** `unword` rewrites both the system prompt and the tool descriptions before comparing with TypeScript Marv. Add three `MARV_RS_TEXT` entries, each mapping a new sentence to `""` as `process`'s is mapped: the `BASE_TOOLS_TEXT` sentences, and the `read_file` and `bash` additions.

**Subagents:** `BASE_TOOLS_TEXT` is only in the main agent's prompt, so subagents don't get phase 1 (§7).

**Measure.** Each arm is run twice (the 0.9.0 eval showed ±2 issues and ±40% total cost between single runs):
- SWE-bench Verified, the same 30 issues, GLM-5.3-flash, sandbox on: base vs phase 1. The 0.9.0 runs cost $0.27 and $0.38, so about $1.50 for the four runs.
- The bench (21 tasks × 2, GLM-5.3-flash). About $0.01 a run.

Report:
- steps per turn and the single read-only share (from phase 0's script);
- resolved and passed;
- total and median cost;
- request time.

**Ship rule.** Single runs vary by ±40% in total cost, so two runs can't detect a saving near 16%. The deciding metric is steps per turn, pooled across both runs of each arm.
- **Steps:** fewer steps per turn than base, median and mean.
- **Resolved:** the sum of resolved over the two runs is within 2 of base's sum.
- **Bench:** no task fails on both runs that passed on base.

Cost and request time are reported but don't decide.

## 5. Phase 2: notice it (a once-per-turn nudge)

Only if phase 1 leaves long runs.

**Trigger:** after `BATCH_NUDGE_AFTER` (proposed 4) consecutive steps that each made one read-only call, the loop adds one note.

It lives in the loop's own `Phase::AfterStep` (agent.rs), not in the session's `after_step` hook. Notes from that hook are fenced as untrusted project text (provenance.rs), and this one is Marv's own. The switch is an `AgentOptions.batch_nudge` bool, which the session sets for the main agent only, since subagents wire their own `after_step`. Proposed text:

> Marv: the last few steps each read one thing. If you already know the next files, searches or pages you need, send them together in one reply: read-only calls run at the same time.

- **Once per turn** (a `LoopState` flag, like `announced`), so it can't loop.
- **It costs no extra step:** unlike #90's note, which follows a reply with no calls and forces one more request, this one comes after tool results, and the next request goes out anyway. It's appended, so the prompt cache holds.
- **One user turn:** if another note is due at the same point, both go in the same user message, never two user turns in a row.
- **The count resets** on compaction, and on any step that isn't a single read-only call.
- **Steps that don't count:** steps answered with a tool-loop note, or with a #81 "not run" record.
- **Never together with a tool-loop note:** the repeated-read runs (the same file nine times) also trip the tool-cycle detector. In that step, the cycle note wins.
- **Approval-free turns:** `no_auto_approval` turns don't change it, since these reads never ask.
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
- **Subagents:** they don't get `BASE_TOOLS_TEXT`. Apply phase 1 to the subagent prompt, and phase 2 through `batch_nudge`, if phase 1 shows gains.

## 8. Decisions

1. **Phase 1 now:** prompt plus two description sentences, measured with two runs per arm (about $1.50 SWE-bench plus the bench). Steps per turn decides.
2. **Phase 2 trigger:** after 4 single read-only steps, once per turn, on by default if it passes the ship rule. Alternatives: 3, or 5.
3. **Phase 3:** decide after phase 2's numbers.
4. **Parallel bash:** stays out of scope until multi-call replies are common (§7).

## 9. PRs

1. Phase 0 + phase 1: `evals/batching_stats.py`, the prompt and descriptions, oracle mapping, tests that pin the new sentences, CHANGELOG with the measurement.
2. Phase 2 (if needed): the loop's nudge (`AgentOptions.batch_nudge`), config switch, trajectory record, TUI line. Tests:
   - fires after 4;
   - once per turn;
   - not for subagents;
   - resets on a non-read call and on compaction;
   - never with a tool-loop note;
   - shares the user turn with other notes.
3. Phase 3 (if needed): `read_file` `ranges`.
