# Evals: Marv on real models

The oracle (`tests/oracle`) proves Marv behaves the same as the TypeScript build with scripted replies. These evals
measure what a real model gets done through Marv: Marv's eval tasks, run with `marv -p`, each graded afterwards by
hidden tests. `bench.py` can run Claude Code (`claude -p`) or opencode (`opencode run`) on the same tasks for comparison
(#73); opencode runs the same OpenRouter model as Marv, so that comparison is harness against harness.

## Running them

```sh
cargo build --release -p marv-cli && cargo build -p marv-oracle   # Marv, and the tracker MCP server the MCP tasks use
python3 -I evals/bench.py --verify                                # every check fails on the task's repo and passes with its solution
python3 -I evals/bench.py --harness marv,claude --repeat 5 --label haiku --budget 1.00
python3 -I evals/bench.py --harness marv,opencode --repeat 3 --marv-model z-ai/glm-5.3-flash --label glm
python3 -I evals/bench.py --report evals/results/<run> [<run> …]  # the tables again, or several runs side by side
```

Needs Python 3.10+, Bun (it runs the checks, and `bun install` in `evals/` fetches the TypeScript compiler on first use),
git, and bubblewrap: the checks run model-written code, so they run only in bubblewrap (no network, cleared environment, empty home, only the workspace writable). Marv uses the OpenRouter key from `OPENROUTER_API_KEY` or `~/.marv`;
Claude Code its own login; opencode the same OpenRouter key as Marv.

Options: `--tasks a,b`, `--marv-model` (default `anthropic/claude-haiku-5.5`), `--claude-model` (default
`claude-haiku-5-5`), `--marv-bin` (default `target/release/marv`, else `marv` on PATH), `--concurrency` (4),
`--timeout` (300 s per run), `--budget` (stops starting runs once that many dollars are spent), `--opencode-bin`
(default: what `mise which opencode` finds, else `opencode` on PATH).

`--marv-config key=value` (repeatable) puts a key into Marv's throwaway `config.json` (the value is JSON when it parses,
else a string), e.g. `--marv-config compaction=v2` for #63's comparison. A `compactTokens` below Marv's 40k minimum goes
to `MARV_EVAL_COMPACT_TOKENS` instead, an eval-only bypass (at least 4k), so short tasks compact more than once. The
report then counts compactions and **repeated reads**: `read_file` calls within 10 steps after a compaction, of a path
and an overlapping line range read before it. `long-haul` is the task made for it: 15 converters to change one at a
time with the tests run in between.

## What a run is

1. The task's `repo/` is copied to a temp folder and committed. A TypeScript task gets the compiler in `node_modules`.
2. The agent gets `task.md` as its prompt:
   - Marv: `marv -p --output stream-json --approve files --approve process --approve mcp`, with a throwaway
     `MARV_CONFIG_DIR`, so your own MCP servers, memory and settings stay out. Your personal skills
     (`~/.marv/skills`) still load: Marv's sandbox hides the folder `HOME` names, so a throwaway `HOME` would leave
     the real one readable. bench.py says so when you have some; move them aside for a run without them.
   - Claude Code: `claude -p --output-format stream-json` with project settings only, no MCP servers but the
     task's, and the tools Read, Edit, Write, Glob, Grep, Bash, TodoWrite.
   - opencode: `opencode run --pure --auto --format json -m openrouter/<--marv-model>`, with throwaway XDG config,
     data, state and cache folders and a throwaway `HOME` (so your own config, plugins, skills and sessions stay out;
     `--user-skills` keeps your `HOME`) and the task's MCP servers in
     `OPENCODE_CONFIG_CONTENT`. `--auto` approves everything: it has no sandbox. bench.py calls the binary itself, not
     mise's `opencode` wrapper, which would reinstall opencode in each run's empty config folder.
   - The MCP tasks get the tracker server (`marv-oracle tracker`) on `tests/oracle/mcp/tracker.json`, and what it
     recorded is copied in as `__tracker_comments__.json`. A check that reads the final reply finds it in
     `__eval_reply__.md`, and one that grades the run's length finds `{"requests": n}` in `__eval_run__.json`
     (`count-lines-local` passes only in 8 requests or fewer, #81).
3. The task's `check/` is copied in and `bun test ./__eval__.test.js` grades it.

`dev-server-claim` doesn't run for Claude Code or opencode: it checks that the reply doesn't say a server is up for the
user, which is true of Marv (its sandbox keeps the server in) and not of an unsandboxed agent.

## What you get

`evals/results/<time>-<label>/results.jsonl` has a line per run: pass, seconds, requests, plan-only requests (`planOnly`,
`planOnlyMs`: replies whose only calls were plan updates, #85; `--report` counts them from the trajectory for older
runs), tool calls by name, tool errors, tokens (prompt, cached, completion), cost, and the check's output when it failed. `logs/<harness>/<task>-<rep>/`
keeps the stream, Marv's trajectory, stderr and the diff. Marv's cost is what OpenRouter charged; Claude Code's is its
own estimate at API prices, opencode's its own estimate from its price list.

## How the model batches its calls

`python3 -I evals/batching_stats.py <folder> […]` prints, per run, the main agent's steps per turn, calls per step,
the share of steps that made one call and of those that only read, and how many steps were runs of single read-only
calls that could have been one step (#215). A folder is a bench run (`evals/results/<run>`), a SWE-bench run's `logs/`,
or `~/.marv/trajectories`.

## Adding a task

A folder in `evals/tasks/` with `task.md`, `repo/`, `check/__eval__.test.js` and `solution/` (plus `mcp.json`,
`{ "server": "tracker", "data": "tracker.json" }`, for the tracker). Run `--verify` before committing it.
