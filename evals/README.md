# Evals: Marv on real models

The oracle (`tests/oracle`) proves Marv behaves the same as the TypeScript build with scripted replies. These evals
measure what a real model gets done through Marv: Marv's 20 eval tasks, run with `marv -p`, each graded afterwards by
hidden tests. `bench.py` can run Claude Code on the same tasks (`claude -p`) for comparison (#73).

## Running them

```sh
cargo build --release -p marv-cli && cargo build -p marv-oracle   # Marv, and the tracker MCP server the MCP tasks use
python3 -I evals/bench.py --verify                                # every check fails on the task's repo and passes with its solution
python3 -I evals/bench.py --harness marv,claude --repeat 5 --label haiku --budget 1.00
python3 -I evals/bench.py --report evals/results/<run> [<run> …]  # the tables again, or several runs side by side
```

Needs Python 3.10+, Bun (it runs the checks, and `bun install` in `evals/` fetches the TypeScript compiler on first use),
git, and bubblewrap: the checks run model-written code, so they run only in bubblewrap (no network, cleared environment, empty home, only the workspace writable). Marv uses the OpenRouter key from `OPENROUTER_API_KEY` or `~/.marv`;
Claude Code its own login.

Options: `--tasks a,b`, `--marv-model` (default `anthropic/claude-haiku-5.5`), `--claude-model` (default
`claude-haiku-5-5`), `--marv-bin` (default `target/release/marv`, else `marv` on PATH), `--concurrency` (4),
`--timeout` (300 s per run), `--budget` (stops starting runs once that many dollars are spent).

## What a run is

1. The task's `repo/` is copied to a temp folder and committed. A TypeScript task gets the compiler in `node_modules`.
2. The agent gets `task.md` as its prompt:
   - Marv: `marv -p --output stream-json --approve files --approve process --approve mcp`, with a throwaway
     `MARV_CONFIG_DIR`, so your own MCP servers, skills and settings stay out.
   - Claude Code: `claude -p --output-format stream-json` with project settings only, no MCP servers but the
     task's, and the tools Read, Edit, Write, Glob, Grep, Bash, TodoWrite.
   - The MCP tasks get the tracker server (`marv-oracle tracker`) on `tests/oracle/mcp/tracker.json`, and what it
     recorded is copied in as `__tracker_comments__.json`. A check that reads the final reply finds it in
     `__eval_reply__.md`.
3. The task's `check/` is copied in and `bun test ./__eval__.test.js` grades it.

`dev-server-claim` doesn't run for Claude Code: it checks that the reply doesn't say a server is up for the user, which
is true of Marv (its sandbox keeps the server in) and not of Claude Code.

## What you get

`evals/results/<time>-<label>/results.jsonl` has a line per run: pass, seconds, requests, plan-only requests (`planOnly`,
`planOnlyMs`: replies whose only calls were plan updates, #85; `--report` counts them from the trajectory for older
runs), tool calls by name, tool errors, tokens (prompt, cached, completion), cost, and the check's output when it failed. `logs/<harness>/<task>-<rep>/`
keeps the stream, Marv's trajectory, stderr and the diff. Marv's cost is what OpenRouter charged; Claude Code's is its
own estimate at API prices.

## Adding a task

A folder in `evals/tasks/` with `task.md`, `repo/`, `check/__eval__.test.js` and `solution/` (plus `mcp.json`,
`{ "server": "tracker", "data": "tracker.json" }`, for the tracker). Run `--verify` before committing it.
