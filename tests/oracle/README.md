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

`MARV_ORACLE_BIN` defaults to this workspace's `target/debug/marv`. `MARV_ORACLE_KEEP=1` keeps each run's temp folder (`$TMPDIR/marv-oracle-<scenario>-…`) for a look afterwards.

No run reaches a real model or spends money: every request goes to the fake server on `127.0.0.1`.

## How a run works
1. The scenario's `project/` is copied into a temp folder and committed (`git init`, one commit with a fixed date), as Marv's evals do: glob and grep list files with `git ls-files`.
2. A fake OpenAI-compatible server starts on a free port. It answers each `POST /v1/chat/completions` with the next line of `script.jsonl`, streamed as SSE, and records the body. `GET /v1/models` is an empty list.
3. The binary runs as `marv -p <prompt> --output stream-json [--max-steps N] [--approve …]` in the copy, with a cleared environment:
   - `MARV_CONFIG_DIR` is a throwaway folder whose `config.json` selects provider `openrouter`, the server's `baseUrl` and a dummy key. Trajectories, diagnostics and checks are off unless `run.json` turns them on;
   - `HOME` is a throwaway folder too, so personal skills in `~/.marv` don't reach the system prompt;
   - `OPENROUTER_API_KEY`, `MARV_MODEL` and `MARV_MAX_STEPS` are not passed on: they beat the config file.
4. What it did is normalized (temp paths become `<project>`, `<home>`, `<config>`, `<tmp>`; the port `<port>`; today's date `<date>`) and compared with `expect/`.

A recording is refused when the run timed out or left scripted replies unused: those are broken scenarios, not behaviour.

## Scenario format
Each scenario is a directory under `scenarios/`:
- `project/`: the starting files.
- `script.jsonl`: the fake provider's replies, one per request. Lines starting with `//` are comments. A reply has `text`, `reasoning`, `tool_calls` (`{"id"?, "name", "arguments"}`, arguments as a JSON string or an object), `finish_reason` (default `tool_calls` or `stop`; `length` is a cut-off), `usage` and `chunk` (characters per streamed piece, default 50). Faults: `status` (with `retry_after`, `body`) for an HTTP error, `drop` for a stream that closes before it finishes, `stream_error` for an error chunk after the 200. See `crates/marv-oracle/src/script.rs`.
- `run.json`: `prompt`, `description`, `max_steps`, `approve` (a list), `config` (laid over the default `config.json`), `git` (default true), `timeout_secs` (default 60).
- `expect/`, recorded from TypeScript Marv:
  - `events.jsonl`: the `stream-json` event log, normalized, one canonical JSON line per event (compared as JSON: key order means nothing here);
  - `requests.jsonl`: the request bodies sent to the provider, **byte for byte**. This checks the prompt-cache prefix rule. A body that is the same JSON in other bytes (key order) is reported as such;
  - `files/`: the project after the run (`.git` left out);
  - `exit_code`.

`.gitattributes` here turns off line-ending conversion: CRLF files and trailing spaces are what some scenarios test.

## Scenarios

Seeded from Marv's `evals/tasks/*` (the editing tasks, with a scripted model instead of a real one):

| Scenario | What it checks |
|---|---|
| `crlf-line-endings` | an edit written with `\n` in a CRLF file keeps the file CRLF |
| `duplicate-blocks` | an `old_string` found 3 times is refused as ambiguous; the retry with more context lands once |
| `tabs-off-by-one` | two edits in one step, indented with spaces in a tab file, are re-indented with tabs; then `git diff --stat` in the sandbox |
| `trailing-whitespace` | `old_string` without the file's trailing spaces still matches, and the file keeps them |
| `json-edit` | glob, two reads in one step, an edit to JSON |
| `rename-across-files` | grep, five `replace_all` edits in one step, grep again |

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

## Still to write
- The rest of Marv's `evals/tasks/*`: `add-feature`, `fix-failing-test`, `failing-suite`, `long-file`, `refactor-signature`, the `ts-*` tasks (need `.gitignore` in `project/`, which would also apply to this repository: store it under another name and rename it in the runner) and the MCP tasks (need the tracker server).
- A declined approval (needs an approver that answers "no": headless only has `--approve` and refusal), mid-turn compaction, an HTTP 429 with `Retry-After`.
- Every guard in `docs/SECURITY.md`.
