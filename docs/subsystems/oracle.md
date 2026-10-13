# marv-oracle

A dev crate (not shipped): the fake OpenAI-compatible server, the scenario runner, normalization, diffing and the prompt-cache prefix check behind `tests/oracle/`, plus the tracker MCP fixture. It runs any `marv` binary on a scripted model and diffs what it did against recordings from TypeScript Marv.

Spec: `tests/oracle/README.md`; Marv's `evals/run.ts` (the workspace setup), `evals/tasks/*` (the seed tasks) and `evals/mcp/tracker.ts` (the MCP fixture); the wire format Marv's adapter reads, `src/provider/openai-compat.ts` and `src/provider/sse.ts`; `src/headless.ts` for the `stream-json` events and exit codes.

## Modules

| Marv (TS) | Rust module | Status |
|---|---|---|
| (new; the format `src/provider/openai-compat.ts` reads) | `script.rs` | **Done and tested:** `script.jsonl` replies (text, reasoning, tool calls, finish reason, usage) and faults (HTTP status with `Retry-After`, dropped stream, error chunk) as SSE chunks |
| (new) | `server.rs` | **Done and tested:** `FakeServer` on `127.0.0.1:0`, one thread per connection, records every request body as bytes, 500 past the end of the script, `GET /models` from `run.json` |
| `evals/run.ts` (`workspace()`, the tracker's comments) | `runner.rs` | **Done and tested:** temp project (`gitignore.oracle` renamed, committed with fixed dates), throwaway `HOME` and `MARV_CONFIG_DIR`, cleared env, the MCP fixture in `mcp.json`, timeout, file snapshot, `canonical_request` |
| (new) | `normalize.rs` | **Done and tested:** temp paths, port, the tracker's binary, today's date (UTC ±1 day); events as canonical JSON |
| (new) | `compare.rs` | **Done and tested:** `expect/` read/write (stored names), a diff per part (events, canonical requests, files with CRLF and trailing spaces visible, exit code) |
| (new; the rule in Marv's `CLAUDE.md`, "Prompt cache") | `prefix.rs` | **Done and tested:** each request extends the previous one byte for byte, `cache_control` and `tool_choice` exempt, `prefix_resets` after a compaction |
| `evals/mcp/tracker.ts` | `tracker.rs` | **Done and tested:** all 25 tools, the specs byte for byte from the TypeScript's `tools/list`, answers as `JSON.stringify(…, null, 2)` in its key order, comments to `out.json` |
| (new) | `lib.rs`, `main.rs`, `tests/oracle.rs` | **Done:** `check_all` (parallel), the `marv-oracle check\|record\|tracker` CLI, the ignored `oracle` test (`MARV_ORACLE_BIN`, `MARV_ORACLE_RECORD`), and checks that every recording is complete, keeps the prefix rule and holds no `.gitignore` |

28 scenarios are recorded from TypeScript Marv (`7df450f`), which passes all of them: all 17 of Marv's eval tasks and 11 loop edge cases. The `SECURITY.md` guards are #8. marv-rs fails all of them today: `marv -p` isn't ported (#11).

## Port notes

- **The binary is a parameter** (`MARV_ORACLE_BIN`, or `--bin`). The same scenario records from TypeScript Marv and checks marv-rs.
- **Isolation.** The run never sees the user's setup: `HOME` and `MARV_CONFIG_DIR` are temp folders, and the environment is cleared except `PATH`, because Marv's `resolveConfig` lets `OPENROUTER_API_KEY`, `MARV_MODEL` and `MARV_MAX_STEPS` beat the config file. Personal skills in `~/.marv/skills` would otherwise change the system prompt.
- **Defaults off:** trajectories, diagnostics (the typecheck after edits) and project checks. They write outside the project or run tools the script didn't ask for. A scenario's `run.json` `config` can turn them on.
- **Requests: two checks, neither asks marv-rs for Marv's bytes** (changed from the first version, which compared request bytes with TypeScript Marv's and so would have forced marv-rs onto Marv's key order):
  - *Against the recording, as canonical JSON:* keys sorted, `cache_control` breakpoints (#56) out, a marked message counted as its plain text. Every value must match. Key order and breakpoints are left to each implementation, so marv-rs's sorted schema keys (`PORTING.md`, "Serialization") pass, and serde_json `preserve_order` isn't needed.
  - *Against the run's own previous request, as bytes (`prefix.rs`):* the prompt cache only needs each run's requests to extend one another. Every message of the previous request must be in the next at the same index with the same bytes, and every other field must be byte-identical. `cache_control` markers don't count (they move to the last message by design), and neither does `tool_choice` (#59's wrap-up sends `"none"`). A compaction rewrites the history on purpose, so `run.json` `prefix_resets` names the request after it. Recordings keep this rule too: TypeScript Marv passes it in every scenario, and a recording that breaks it is refused.
  - Still, don't turn `preserve_order` on in this crate: Cargo unifies features across the workspace, so it would change every crate's serialization in `cargo test --workspace`. The tracker keeps its key order with its own small ordered JSON type instead. `serde_json`'s `raw_value` feature is on (for the byte spans `prefix.rs` reads); it only adds a type.
- **Events compare as JSON values**, one canonical line each. `JSON.stringify` and serde order keys differently, and an event's field order means nothing to a consumer.
- **No real model, no money.** The server binds `127.0.0.1` only. A request past the end of the script gets a 500, which the binary reports as an error, so an extra request shows up in the diff instead of hanging. The tracker is local and writes only its own `out.json`.
- **Recording guards:** a run that timed out, left replies unused or broke the prefix rule isn't saved.
- **`.gitignore` in a project** is stored as `gitignore.oracle` (in `project/` and `expect/files/`) and renamed by the runner: under its real name it would apply to this repository and hide the scenario's own files. The non-ignored test fails if one slips in.
- **The tracker** is a subcommand of the `marv-oracle` binary (`marv-oracle tracker <data> <out>`), found next to the test (`target/<profile>/`). Its specs are the TypeScript tracker's `tools/list` answer, byte for byte (`tests/oracle/mcp/tracker-tools.json`). It doesn't validate arguments against the schemas, which the TypeScript's SDK does with zod; scripted calls send valid ones.
- **Not planned: a declined approval.** `marv -p` never asks: with no `--approve`, a call outside yolo is refused ("there's no one to ask", `refused-approval`), and `--approve` answers "yes" or "unavailable", never "no". A decline only happens in the TUI.
- **No test runs in the scripts.** The eval tasks' real models run `bun test`, but its output carries timings and Bun's version, which no normalization makes stable. The scripts read the tests instead, and use `git diff --stat` where a check is wanted.
- **`.gitattributes`** in `tests/oracle/` turns off line-ending conversion, so the CRLF and trailing-space fixtures stay byte-exact.
- **Output goes through `marv_core::printable()`**: diffs and stderr carry model and tool text.

## Marv behaviours the recordings pin
marv-rs must match these, or note the difference with an issue number:
- **The second tool loop** answers the calls it skips with the cut-off note (`NOT_RUN.cutOff`), not a loop note (`tool-loop`).
- **Requests that aren't steps.** `steps` counts replies that ended in text or tool calls, so these don't count:
  - an empty reply's retry and nudge: `steps` is 1 in `empty-reply`, while the usage covers 3 requests;
  - a text loop's dropped reply (`text-loop`);
  - the compaction's summary request: `steps` is 2 in `mid-turn-compaction` with 3 requests. The usage includes the summary's tokens.
- **A dropped stream's `text_delta`s** are already printed before the retry (`dropped-stream`). A 429's retry prints nothing (`rate-limited`).
- **Compaction events.** The summary request streams no `text_delta`. The turn shows `status: compacting` then `compaction {compacted: true}`, and no second `status: running` (`mid-turn-compaction`).
- **Deferred MCP labels.** For `tool_search` and `mcp_call`, `tool_start`'s label is the raw arguments JSON (`{"names":[…]}`, `{"tool":…,"arguments":…}`), while `tool_end`'s is the tool name(s) (`mcp-fix-from-issue`, `mcp-comment`).
- **An MCP tool called by its own name** (`mcp__tracker__add_comment`) runs as `mcp_call`. The stream shows `tool: "mcp_call"`, and the result ends with Marv's note that it isn't offered as a tool of its own (`mcp-comment`).
- **`status: waiting_for_mcp`** comes before `status: running` whenever an MCP server is configured (the MCP scenarios).
