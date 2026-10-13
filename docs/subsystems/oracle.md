# marv-oracle

A dev crate (not shipped): the fake OpenAI-compatible server, the scenario runner, normalization and diffing behind `tests/oracle/`. It runs any `marv` binary on a scripted model and diffs what it did against recordings from TypeScript Marv.

Spec: `tests/oracle/README.md`; Marv's `evals/run.ts` (the workspace setup) and `evals/tasks/*` (the seed tasks); the wire format Marv's adapter reads, `src/provider/openai-compat.ts` and `src/provider/sse.ts`; `src/headless.ts` for the `stream-json` events and exit codes.

## Modules

| Marv (TS) | Rust module | Status |
|---|---|---|
| (new; the format `src/provider/openai-compat.ts` reads) | `script.rs` | **Done and tested:** `script.jsonl` replies (text, reasoning, tool calls, finish reason, usage) and faults (HTTP status, dropped stream, error chunk) as SSE chunks |
| (new) | `server.rs` | **Done and tested:** `FakeServer` on `127.0.0.1:0`, one thread per connection, records every request body as bytes, 500 past the end of the script |
| `evals/run.ts` (`workspace()`) | `runner.rs` | **Done and tested:** temp project (committed with fixed dates), throwaway `HOME` and `MARV_CONFIG_DIR`, cleared env, timeout, file snapshot |
| (new) | `normalize.rs` | **Done and tested:** temp paths, port, today's date (UTC ±1 day); events as canonical JSON |
| (new) | `compare.rs` | **Done and tested:** `expect/` read/write, a diff per part (events, requests by bytes with a key-order note, files with CRLF and trailing spaces visible, exit code) |
| (new) | `lib.rs`, `main.rs`, `tests/oracle.rs` | **Done:** `check_all` (parallel), the `marv-oracle check\|record` CLI, the ignored `oracle` test (`MARV_ORACLE_BIN`, `MARV_ORACLE_RECORD`) |

15 scenarios are recorded from TypeScript Marv (`7df450f`), which passes all of them. marv-rs fails all of them today: `marv -p` isn't ported (#11).

## Port notes

- **The binary is a parameter** (`MARV_ORACLE_BIN`, or `--bin`). The same scenario records from TypeScript Marv and checks marv-rs.
- **Isolation.** The run never sees the user's setup: `HOME` and `MARV_CONFIG_DIR` are temp folders, and the environment is cleared except `PATH`, because Marv's `resolveConfig` lets `OPENROUTER_API_KEY`, `MARV_MODEL` and `MARV_MAX_STEPS` beat the config file. Personal skills in `~/.marv/skills` would otherwise change the system prompt.
- **Defaults off:** trajectories, diagnostics (the typecheck after edits) and project checks. They write outside the project or run tools the script didn't ask for. A scenario's `run.json` `config` can turn them on.
- **Requests compare as bytes.** The prompt cache keys on bytes, so key order matters. marv-rs sorts tool-schema keys (`PORTING.md`, "Serialization"): the oracle reports that as "same JSON, different bytes", and marv-rs needs serde_json `preserve_order` (or Marv's key order) to pass. Don't turn `preserve_order` on in this crate: cargo unifies features across the workspace, so it would change every crate's serialization in `cargo test --workspace`.
- **Events compare as JSON values**, one canonical line each. `JSON.stringify` and serde order keys differently, and an event's field order means nothing to a consumer.
- **No real model, no money.** The server binds `127.0.0.1` only. A request past the end of the script gets a 500, which the binary reports as an error, so an extra request shows up in the diff instead of hanging.
- **Recording guards:** a run that timed out or left replies unused isn't saved.
- **Marv behaviours the recordings pin** (marv-rs must match them, or note the difference with an issue number):
  - the second tool loop answers the calls it skips with the cut-off note (`NOT_RUN.cutOff`), not a loop note (`tool-loop`);
  - an empty reply's retry and nudge count no step: `steps` is 1 in `empty-reply` while the usage covers 3 requests;
  - a text loop's dropped reply counts no step either (`text-loop`);
  - the `text_delta`s of a stream that drops are already printed before the retry (`dropped-stream`).
- **`.gitattributes`** in `tests/oracle/` turns off line-ending conversion, so the CRLF and trailing-space fixtures stay byte-exact.
- **Output goes through `marv_core::printable()`**: diffs and stderr carry model and tool text.
