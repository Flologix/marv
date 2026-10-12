# ORCHESTRATION.md

How issue work on Marv is run by agents: a planner, an orchestrator and workers, with the user deciding what can't be undone. **This file binds the orchestrator.** Whoever orchestrates (Claude Code, or Marv itself later) reads it at the start of a sprint and follows it. A worker never needs to read it: it gets a brief (below).

The goal is merged, verified code with as little process as possible. Every rule here exists because skipping it cost real time or money in Sprint 1 (#29).

## Roles: one output each

| Role | Who | When | Output | Nothing else |
|---|---|---|---|---|
| **Planner** | the orchestrator, in planning mode (or the user) | once per sprint | one sprint issue (label `sprint`): goal, issues grouped into waves, files each one touches, stop points | no re-planning mid-sprint unless new evidence changes priorities |
| **Orchestrator** | Claude Code (or Marv on a strong model, once it passes the gates below) | during the sprint | one PR per sprint, from the integration branch `sprint-N` | no status reports to anyone but the user |
| **Worker** | Marv or Claude Code, one per issue, in its own worktree | per issue | one uncommitted diff, plus `DONE #N` and a summary | no git writes, no GitHub, no scope beyond its issue |
| **Verifier** | a script (`bun run typecheck && bun test`, CI, `bun run eval` where behavior changes) | per issue and per sprint | pass or fail | not an agent: it can't be talked into passing |
| **User** | you | at the stop points | decisions | |

No role exists only to produce text for another role. If a new role seems needed, first ask which artifact it would produce that isn't produced already.

## The sprint loop

1. **Pick** the lowest-numbered open `sprint` issue whose dependencies are met. Read it and the issues it lists. That's the whole plan.
2. **Set up:** a worktree for `sprint-N` from `origin/main`; `bun install` in every worktree you create.
3. **For each wave**, in parallel, one worker per issue:
   1. `herdr worktree create --cwd ~/Projects/marv --branch sN/<issue>-<slug> --base sprint-N --path ~/Projects/marv-worktrees/sN-<issue>-<slug> --no-focus`, then `bun install` there.
   2. Write the brief to `TASK.md` in the worktree (template below). Start the worker (below) and send it "Read TASK.md and do the task it describes."
   3. Wait for events, don't poll in a tight loop (below).
   4. When the worker reports, **verify, then review** (definition of done). Fix in place what takes under ~15 minutes; otherwise send the worker one precise list of fixes.
   5. Commit in the worker's worktree with explicit paths (never `TASK.md`), naming the worker and model in the message.
   6. **Open Code Review** that commit: `bun run review --base sprint-N` (`ocr` with the model in `~/.opencodereview/config.json`). Fix or send back every critical/high finding and judge the rest; a finding you reject gets one line in the sprint PR saying why. Then merge into `sprint-N` with `--no-ff`. Resolve CHANGELOG/CLAUDE.md conflicts by keeping both entries; check for conflict markers before committing.
   7. Close the worker.
4. Start the next wave only when the previous one is merged into `sprint-N` and the full suite passes there.
5. **Ship:** push `sprint-N`, open one PR whose body says `Fixes #N` for every issue in it, wait for CI, the `OCR review` check and the Greptile review, fix what they find. **The user merges.**
6. **Report** once, at the end (see Metrics). Then remove the sprint's worktrees.

## Definition of done (per issue)

All of these, checked by the orchestrator, not taken from the worker's report:
- `bun run typecheck` is clean and `bun test` passes on the merged `sprint-N` (the only allowed exceptions are those under "Known environment quirks", each with its issue).
- The orchestrator read the whole diff. Tests prove the change: for a bug, a test that fails without the fix (check by reverting only the fix, not the whole file).
- Behavior changes that models feel (prompts, tool handling, loop logic) have a before/after `bun run eval` with the same models, tasks and repeats.
- CHANGELOG (Unreleased) and the matching CLAUDE.md bullet are updated.

A worker saying "DONE" is a request for review, not a result. In Sprint 1 workers reported done with bugs their own tests couldn't catch, twice.

## Routing and escalation

**Route by the issue's Size:**
- XS/S, well specified: Marv on a cheap model (`z-ai/glm-5.3-flash`).
- M, or touching the agent loop, session, sandbox or prompt cache: Marv on a stronger model, or a Claude Code worker.
- L/XL or architectural: spec first (a PR under `docs/superpowers/`, approved by the user), then split.

**Escalate along a fixed ladder.** Each step is used once, then you move on:
1. The worker.
2. One redirect with exact, numbered steps.
3. A fresh context (`/clear`) with the same steps.
4. A stronger model.
5. The orchestrator finishes it.
6. The user.

**Budget per issue:** $0.50 or 45 minutes of worker time, whichever comes first. Going over is a signal to move one step up the ladder, not a reason to keep nudging.

Stop a worker at once and move up the ladder when it repeats text or tool calls (#37), or ends its turn without a report twice.

## Stop points: ask the user

Only these. Batch them into one message per wave where you can.
- Pushing a branch, opening or merging a PR. **Never merge to `main` yourself.**
- Spending money on eval runs: propose models, tasks, repeats and a `--budget`.
- Approving a spec before implementation.
- Repository or project settings (rulesets, fields, iterations), deleting anything, publishing a release.
- Anything a worker asks that you can't answer from the issue, the code or this file.

Everything else runs without asking.

## GitHub writes: the paperwork budget

The board follows the code. Its automations ("Pull request linked to issue", "Pull request merged", "Item closed") do the bookkeeping:
- a PR that says `Fixes #N` links the issue;
- merging the PR closes the issue and sets it Done.

At sprint start, the orchestrator may set the sprint's issues to In progress in one batch. That's the only manual status change.

**Per issue, the orchestrator's GitHub writes are:** the sprint PR's `Fixes #N` line, and at most one comment if something about the issue changed (scope, findings). Per sprint, also: ticking the sprint issue's checklist once at the end.

**Never:**
- Set Status to Done by hand. The project's "Auto-close issue" workflow closes the issue; in Sprint 1 that closed five issues whose code wasn't on `main`.
- Create projects, fields or labels without asking. List first (`gh project list --owner FlologixAI`); the roadmap is project #1.
- Re-list the board to find one item: GitHub's GraphQL limit (5,000/hour) is shared by every agent on the account, and Sprint 1 exhausted it doing that.

**File a new issue only with evidence:** something observed in a run, with numbers or output to show for it. Prefer extending an existing issue to opening a near-duplicate. Fix small things in place instead of filing them.

## Working with Herdr

- **Start a Marv worker:** `herdr pane run <pane> "bun ~/Projects/marv-worktrees/marv-runner/src/cli.tsx"`, then `herdr agent rename <pane> marv-<issue>`. `marv-runner` is a detached worktree at `origin/main`, so merging into `sprint-N` doesn't change the code the workers run. Set `MARV_MAX_STEPS=0` once #33 is on `main`.
- **Running a Marv worker headless (once #38 is on main):** `marv -p "add a test for X" --output json` runs one turn and prints a JSON result (reply, stop reason, steps, tokens, cost). Same `--output text` (default, just the reply on stdout) and `--output stream-json` (each event as a JSON line). With no one to ask, yolo-safe calls run; everything else the model is told to try another way. `--approve git_write --approve network` allows more. Exit code 0 for a finished turn, non-zero for errors, stops or missing approvals.
- **Start a Claude worker:** `herdr agent start <name> --kind claude --pane <pane> -- --permission-mode auto`.
- **Send a Marv worker text:** `herdr pane send-text <pane> "…"`, then `herdr pane send-keys <pane> enter`. Herdr's `agent prompt` and `agent send-keys` don't accept Marv yet. Marv ignores Enter while it's working, so interrupt with `esc` first if you must redirect it.
- **Wait for events:** `herdr agent wait <name> --until blocked` and the idle/done states. A watcher loop should check every 5–10 s, report only a state held for two checks in a row, and never match its own command when you clean it up. Once `marv -p` (#38) exists, run workers headless and read their JSON result instead.
- **Read a worker:** `herdr agent read <name> --source recent-unwrapped --lines 150`.
- **Answer approvals deliberately.** Read what's asked before answering. Choose "don't ask again" only for the step limit and for read-only fetches. Never answer a memory or git-write prompt without reading it.

## Worker brief (`TASK.md`)

```
# Task: issue #N — <title>

<the issue body, verbatim>

<optional: evidence the orchestrator has, e.g. eval findings, in priority order>

## How to work
- You are working in a git worktree of Marv's own repository. CLAUDE.md describes the architecture and conventions; follow them.
- Test first: add or extend a test in tests/ that fails, then make it pass.
- Run the relevant tests with `bun test tests/<name>`, and `bun run typecheck`.
- When behavior changes, update CHANGELOG.md (under "## [Unreleased]") and the matching CLAUDE.md bullet.
- Don't change the git repository: no commit, branch or stash. Don't edit TASK.md.
- Stay within this issue.
- When you're finished, reply with a line starting "DONE #N", then a short summary: the files you changed and the test results.
```

## Metrics: one report per sprint

At the end of each sprint, one comment on the sprint issue:
- **Per issue:** worker and model, cost, redirects, and whether the orchestrator had to fix anything.
- **Totals:** dollars per merged issue; bugs found in review; eval results.
- **Orchestrator overhead:** GitHub writes and user questions per merged issue.

If overhead per merged issue grows from one sprint to the next, cut process before adding any.

## When Marv orchestrates

Marv takes over a role only after passing a measured gate, not on a date:

| Stage | Gate |
|---|---|
| **1. Worker** | Finishes S/M issues with at most one redirect, no loops, and survives dropped connections (#34–#37). |
| **2. Scriptable worker** | `marv -p` with JSON output (#38). |
| **3. Reviewer** | Catches Sprint 1's real review findings on a review eval set: the two #8 compaction bugs and the #2 CRLF bug. |
| **4. Orchestrator** | Runs one wave under this file, on a strong model, at no worse cost and quality than a Claude orchestrator. It needs GitHub through MCP (#27), a plan tool (#15) and checkpoints (#16). |

## Known environment quirks (remove when fixed)

None open. Sprint 1 fixed the three it started with: node from mise or asdf invisible in the sandbox (#34), git failing in a git worktree (#35), and a dropped connection ending a worker's turn (#36). A test that fails is a failure until shown otherwise; don't excuse it as "the environment".
