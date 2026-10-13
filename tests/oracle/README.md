# The oracle: proof that marv-rs behaves like Marv

ts-rust ported the TypeScript compiler by diffing its output against the original on thousands of tests. The oracle does the same for Marv, at the level that matters: what the agent *does*.

## Scenario format (planned)
Each scenario is a directory:
- `project/`: the starting files.
- `script.jsonl`: the fake provider's replies, one per request. Text, tool calls, reasoning, usage, stop reasons, and injected faults (a dropped stream, a 429, a cut-off).
- `run.json`: the prompt, flags (`--yolo`, `--approve …`, max steps) and approver answers.
- `expect/`:
  - `events.jsonl`: the `stream-json` event log, normalized (ids, timings and paths scrubbed);
  - `requests.jsonl`: the exact request bodies sent to the provider. This checks the prompt-cache prefix rule;
  - `files/`: the project after the run;
  - `exit_code`.

## Running it against both Marvs
1. **Record:** run each scenario through TypeScript Marv:

   ```sh
   marv -p --output stream-json
   ```

   Use a fake OpenAI-compatible server that replays `script.jsonl`. Save the result as `expect/`.
2. **Check:** run the same scenario through `marv-rs -p --output stream-json` against the same fake server, and diff.

## First scenarios to write
- Seed from Marv's `evals/tasks/*` (17 tasks: editing stress tests, ts-*, failing-suite, MCP).
- Plus the loop edge cases:
  - Ollama's duplicate `call_0`;
  - an output-limit cut-off, and two in a row;
  - an empty reply;
  - a text loop and a tool loop;
  - the step limit;
  - a declined approval;
  - mid-turn compaction;
  - a retry after a dropped stream.
- Plus every guard in `docs/SECURITY.md`.
