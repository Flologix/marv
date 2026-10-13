# The oracle: proof that marv-rs behaves like Marv

ts-rust ported the TypeScript compiler by diffing its output against the original on thousands of tests. The oracle does the same for Marv, at the level that matters: what the agent *does*.

The code is the dev crate `crates/marv-oracle` (see `docs/subsystems/oracle.md`).

## Running it

```sh
# Check a binary against the recordings (prints a diff per scenario that differs):
MARV_ORACLE_BIN=target/debug/marv cargo test -p marv-oracle -- --ignored --nocapture

# TypeScript Marv must pass its own recordings:
MARV_ORACLE_BIN=~/Projects/marv/bin/marv cargo test -p marv-oracle -- --ignored --nocapture

# Re-record from TypeScript Marv (after changing a scenario, or when Marv's behaviour changes on purpose):
MARV_ORACLE_RECORD=1 MARV_ORACLE_BIN=~/Projects/marv/bin/marv cargo test -p marv-oracle -- --ignored

# The same from the command line, for a few scenarios:
cargo run -p marv-oracle -- check  --bin ~/Projects/marv/bin/marv cut-off text-loop
cargo run -p marv-oracle -- record --bin ~/Projects/marv/bin/marv cut-off
```

`MARV_ORACLE_BIN` defaults to this workspace's `target/debug/marv`. The MCP scenarios also run `marv-oracle tracker`, the binary Cargo builds next to the test. `MARV_ORACLE_KEEP=1` keeps each run's temp folder (`$TMPDIR/marv-oracle-<scenario>-…`) for a look afterwards.

No run reaches a real model or spends money: every request goes to the fake server on `127.0.0.1`.

## How a run works
1. The scenario's `project/` is copied into a temp folder (`gitignore.oracle` becomes `.gitignore`) and committed (`git init`, one commit with a fixed date), as Marv's evals do: glob and grep list files with `git ls-files`.
2. A fake OpenAI-compatible server starts on a free port. It answers each `POST /v1/chat/completions` with the next line of `script.jsonl`, streamed as SSE, and records the body. `GET /v1/models` lists `run.json`'s `models` (none by default).
3. With `mcp` in `run.json`, the throwaway config's `mcp.json` starts the tracker MCP server (`marv-oracle tracker`, a port of Marv's `evals/mcp/tracker.ts`) on a copy of `tests/oracle/mcp/<data>`. After the run, the comments it recorded are copied into the project as `__tracker_comments__.json`, as the evals do.
4. The binary runs as `marv -p <prompt> --output stream-json [--max-steps N] [--approve …]` in the copy, with a cleared environment:
   - `MARV_CONFIG_DIR` is a throwaway folder whose `config.json` selects provider `openrouter`, the server's `baseUrl` and a dummy key. Trajectories, diagnostics and checks are off unless `run.json` turns them on;
   - `HOME` is a throwaway folder too, so personal skills in `~/.marv` don't reach the system prompt;
   - `OPENROUTER_API_KEY`, `MARV_MODEL` and `MARV_MAX_STEPS` are not passed on: they beat the config file.
5. What it did is normalized (temp paths become `<project>`, `<home>`, `<config>`, `<tmp>`; the port `<port>`; the tracker's binary `<oracle>`; today's date `<date>`) and compared with `expect/`.
6. The run's own requests are checked against the prompt-cache prefix rule (below).

A recording is refused when the run timed out, left scripted replies unused or broke the prefix rule: those are broken scenarios, not behaviour.

## How requests are compared
Two checks, because the prompt cache needs each run's requests to be byte-stable, not marv-rs's bytes to equal Marv's:
- **Against the recording, as canonical JSON.** `requests.jsonl` holds each body with its keys sorted and `cache_control` breakpoints (#56) taken out. A marked message (`[{"type":"text","text":…,"cache_control":…}]`) counts as its plain text. Key order and breakpoints are each implementation's business; every value must match.
- **Against the run's own previous request, as bytes (the prefix rule, AGENTS.md "Prompt cache").** Every message of the previous request must be in the next one, at the same index, with the same bytes, and every other field (`model`, `tools`, …) must be byte-identical. Two things are exempt:
  - `cache_control` markers, which move to the last message on purpose;
  - `tool_choice`: the wrap-up reply at the step limit sends `"none"` (#59).

  A compaction starts the cache over on purpose: `run.json` `prefix_resets` names the request after it.

## Scenario format
Each scenario is a directory under `scenarios/`:
- `project/`: the starting files.
- `script.jsonl`: the fake provider's replies, one per request. Lines starting with `//` are comments. A reply has `text`, `reasoning`, `tool_calls` (`{"id"?, "name", "arguments"}`, arguments as a JSON string or an object), `finish_reason` (default `tool_calls` or `stop`; `length` is a cut-off), `usage` and `chunk` (characters per streamed piece, default 50). Faults: `status` (with `retry_after`, `body`) for an HTTP error, `drop` for a stream that closes before it finishes, `stream_error` for an error chunk after the 200. See `crates/marv-oracle/src/script.rs`.
- `run.json`: `prompt`, `description`, `max_steps`, `approve` (a list), `config` (laid over the default `config.json`), `git` (default true), `timeout_secs` (default 60), `models` (what `GET /models` lists, in OpenRouter's format: a `context_length` lets Marv compact), `mcp` (`{"server": "tracker", "data": "tracker.json"}`, as Marv's eval tasks' `mcp.json`), `prefix_resets` (1-based requests allowed to break the prefix rule).
- A file named `gitignore.oracle` (in `project/` and `expect/files/`) is the project's `.gitignore`: under its real name it would apply to this repository too.
- `expect/`, recorded from TypeScript Marv:
  - `events.jsonl`: the `stream-json` event log, normalized, one canonical JSON line per event (compared as JSON: key order means nothing here);
  - `requests.jsonl`: the request bodies sent to the provider, as canonical JSON (see above);
  - `files/`: the project after the run (`.git` left out);
  - `exit_code`.

`.gitattributes` here turns off line-ending conversion: CRLF files and trailing spaces are what some scenarios test.

## Scenarios

All 17 of Marv's `evals/tasks/*`, with a scripted model instead of a real one. The scripts don't run `bun test`, as a real model would: its output carries timings and Bun's version. Reads, grep and `git diff` stand in for it.

| Scenario | What it checks |
|---|---|
| `crlf-line-endings` | an edit written with `\n` in a CRLF file keeps the file CRLF |
| `duplicate-blocks` | an `old_string` found 3 times is refused as ambiguous; the retry with more context lands once |
| `tabs-off-by-one` | two edits in one step, indented with spaces in a tab file, are re-indented with tabs; then `git diff --stat` in the sandbox |
| `trailing-whitespace` | `old_string` without the file's trailing spaces still matches, and the file keeps them |
| `json-edit` | glob, two reads in one step, an edit to JSON |
| `rename-across-files` | grep, five `replace_all` edits in one step, grep again |
| `add-feature` | a function from its doc comment: parallel reads, the body edited in (non-ASCII in `new_string`), the export added |
| `fix-failing-test` | read the test and the code, fix the code |
| `failing-suite` | one bug behind several failures: grep, a ranged read (`offset`/`limit`), one edit |
| `long-file` | a file past `read_file`'s 400 lines: the read stops with a note, grep, a ranged read, a one-character fix |
| `refactor-signature` | a signature change and its callers in `src/` and `scripts/`, then `git diff --stat` |
| `ts-make-async` | a change that ripples through callers: parallel reads, a `write_file` rewrite, edits in several files per step |
| `ts-rename-export` | a rename across five TypeScript files, then `git status` (the project has a `.gitignore`) |
| `ts-required-field` | an `old_string` that isn't in the file gets the closest lines back; the retry lands |
| `mcp-fix-from-issue` | deferred MCP tools: `tool_search` loads a spec, `mcp_call` runs it, the fix follows |
| `mcp-comment` | the model calls `mcp__tracker__add_comment` by name: Marv runs it as `mcp_call` and says so; the comment is recorded |
| `mcp-unused` | a configured server whose tools are only announced; a plain edit |

The loop's edge cases:

| Scenario | What it checks |
|---|---|
| `duplicate-call-0` | Ollama's repeated `call_0` (across steps and within one) gets unique ids before the history sees them |
| `cut-off` | a reply cut off at the output limit: its `write_file` isn't run, the model is told, the redo runs |
| `cut-off-twice` | two cut-offs in a row end the run (`length`, exit 2) |
| `text-loop` | a reply repeating one sentence is stopped while it streams and dropped; the model is told once |
| `tool-loop` | the same call with the same result: warned once, then the turn ends (`loop`, exit 2) |
| `step-limit` | `--max-steps 3` with no one to ask: stops after 3 steps (exit 2) |
| `empty-reply` | an empty reply is retried, a second one nudged, the third answers |
| `refused-approval` | a network command with no `--approve` is refused; the turn ends but exits 3 and lists it |
| `dropped-stream` | a stream that closes early is retried from scratch, after the 1 s backoff |
| `rate-limited` | an HTTP 429 with `Retry-After: 1` is retried after that wait |
| `mid-turn-compaction` | a step past 85% of the context window triggers a summary request (same prefix), then the turn goes on from the summary |

## Still to write
- Every guard in `docs/SECURITY.md` (#8).
- Not planned: a declined approval. `marv -p` never asks: with no `--approve` a call outside yolo is refused ("there's no one to ask", see `refused-approval`), and `--approve` answers "yes" or "unavailable", never "no". A decline needs the TUI.
