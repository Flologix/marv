# marv-cli

The `marv` binary. It covers argument parsing, headless `marv -p`, the `marv mesh` commands, the hidden `__html-convert` converter process, and launching the TUI.

Spec: the startup bullet of Marv's `CLAUDE.md` ("`bin/marv` is the `bin` entry…"), `src/cli.tsx`, `src/headless.ts`, `bin/marv`, and `tests/headless.test.ts`.

## Modules

| Marv (TS) | Rust module | Status |
|---|---|---|
| `src/cli.tsx` (argv) | `cli.rs` | **Done and tested:** the clap `Cli` (`-p/--prompt`, `run`, `--output text\|json\|stream-json`, repeated `--approve` including `mesh:<r>`, `--max-steps`, `-m/--model`, `-c/--continue`, `-r/--resume`, `-v/--version`, `-h/--help`), the `mesh ls\|wire` and hidden `__html-convert` subcommands, and `Cli::mode()` |
| `src/cli.tsx` (dispatch) | `main.rs` | **Done:** exit codes, stdin for `-p -`, and the converter process run before the Tokio runtime starts |
| `src/cli.tsx` (TUI startup) | `tui.rs` | **Todo:** the startup sequence and the bridge to `marv_tui::view::SessionPort` (see below) |
| `src/headless.ts` | `headless.rs` | **Ported and tested:** `build_approver` (`ScopeApprover`, `mesh:` grants), `exit_code_for`, `HeadlessResult`, `Collector` (step counting, refused calls including subagents', stream-json lines), `text_output` (printable), `collect_and_output`, `run_headless` |
| `bin/marv` | (none) | Not needed. It only renamed the `bun` process to `marv`, and a native binary is already called `marv` |
| (new) | `mesh_cmd.rs` | `ls` checks `XDG_RUNTIME_DIR`, and `wire` parses the spec (`parse_wire_spec`). Both bodies are todo (mesh v2 and v1) |

`grep -rn 'todo!("port' crates/marv-cli` lists what's left.

## Exit codes (`marv -p`)

| Code | Meaning |
|---|---|
| 0 | The model answered normally (`end`) |
| 1 | An error: bad config, a provider failure, a bad argument, or an unknown `--approve` |
| 2 | Stopped: step limit, output limit, or a loop |
| 3 | An approval it couldn't get: a refused call (even if the turn finished), a decline, or an interrupt |

## Port notes

- **Argument errors exit 1.** Clap's default is 2, which would read as "stopped". `main` uses `Cli::try_parse()` and maps only help and version to 0.
- **Text output goes through `marv_core::printable()`.** This fixes a Marv 0.3.0 bug: Marv wrote the reply raw, so a model could send OSC 52 clipboard writes, retitle the terminal, or hide text behind bidi overrides. Errors and warnings on stderr go through `printable()` as well. JSON output is escaped by `serde_json`.
- **`--approve mesh:<receptacle>`** is split off by `marv_session::mesh::headless_grants`. It pre-approves wires to that receptacle and nothing else. The other names keep Marv's `APPROVE_PREFIXES`. Wiring the grants into the switchboard is todo (mesh v1).
- **`__html-convert`** calls `marv_tools::web::html_convert_cli::main()`, which is still todo in marv-tools.
- **TUI.** marv-tui deliberately doesn't depend on marv-session. It drives a `view::SessionPort` (`send`, `run_command`, `answer`, `abort`, `load_models`, `complete_setup`, …) and renders `view::UiEvent`s. The bridge belongs in marv-cli and has three jobs:
  - implement `SessionPort` over `MarvSession`;
  - map each `SessionEvent` to `UiEvent`s (`Loop(ToolEnd)` → `ToolEnd` with the `ToolDisplay` diff, `Usage` plus `session.usage().totals`, …);
  - apply each `commands::CommandAction`, and hold the approval queue behind a `marv_tools::Approver`.
- **Shutdown, as in Marv:** `session.flush()` (at most 2 s), then MCP close (at most 2 s), then Herdr release (at most 1 s).
