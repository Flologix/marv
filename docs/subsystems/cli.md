# marv-cli

The `marv` binary. It covers argument parsing, headless `marv -p`, the `marv mesh` commands, the hidden `__html-convert` converter process, and launching the TUI.

Spec: the startup bullet of Marv's `CLAUDE.md` ("`bin/marv` is the `bin` entry…"), `src/cli.tsx`, `src/headless.ts`, `bin/marv`, and `tests/headless.test.ts`.

## Modules

| Marv (TS) | Rust module | Status |
|---|---|---|
| `src/cli.tsx` (argv) | `cli.rs` | **Done and tested:** the clap `Cli` (`-p/--prompt`, `run`, `--output text\|json\|stream-json`, repeated `--approve` including `mesh:<r>`, `--max-steps`, `-m/--model`, `-c/--continue`, `-r/--resume`, `-v/--version`, `-h/--help`), the `mesh ls\|wire` and hidden `__html-convert` subcommands, and `Cli::mode()` |
| `src/cli.tsx` (dispatch) | `main.rs` | **Done:** exit codes, stdin for `-p -`, and the converter process run before the Tokio runtime starts |
| `src/cli.tsx` (TUI startup), `src/app.tsx` (engine side), Herdr reporting | `tui.rs` | **Ported and tested (#14):** the startup sequence, the bridge (`Bridge`/`Port`: `SessionPort` over `MarvSession`), `map_event`/`map_loop`, the command actions, setup, resume, the startup notice, `TuiApprover`, Herdr states. Tests: `herdr.test.tsx`'s App-in-a-pane case, approvals (yes/no/Esc), resume, commands, `/compact` and `/clear`, first-run setup, the mappings. See "The TUI bridge" below |
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

## The TUI bridge (`tui.rs`)

marv-tui deliberately doesn't depend on marv-session. It drives a `view::SessionPort` and renders `view::UiEvent`s; `tui.rs` is the other half:

- **`Bridge` / `Port`** implement `SessionPort` over `MarvSession`. Every method returns at once; the work runs on a task and answers with events. `send` starts a turn and forwards its events; `abort` is `session.interrupt()`; `flush` is `session.flush()` (the App awaits it after restoring the terminal).
- **`map_event`** turns each `SessionEvent` into `UiEvent`s: the loop's events one to one (`ToolEnd` carries the `ToolDisplay` diff, and a plain failure's first output line as its summary, as Marv's App did); `Usage` gets `session.usage().totals`; `Subagent` events get their title from the `agent` call's `ToolStart` label; a subagent's usage also updates the session's totals; `Check`/`Checks` become messages (`check_notice`, `project_check_notice`); `Tainted` becomes a message (marv-rs).
- **Commands** go through `commands::run_command`, then `Bridge::apply` does the engine side of each `CommandAction` (config save + `configure`, memory, `/compact`, `/clear`, `/resume`, ratings, skills) and sends the UI side as events (`Message`, `Clear`, `OpenSetup`, `Sessions`, `Exit`).
- **Approvals:** `TuiApprover` is the session's approver. Each request becomes `UiEvent::Approval { id, request }` and waits for `SessionPort::answer`. The queue (head, "N more waiting", Always covering the scope) is the App's. An ask the session stops waiting for (the turn ended) is forgotten.
- **Herdr** (Marv `app.tsx` + `src/herdr.ts`, through marv-mesh's `HerdrReporter`): `blocked` while an approval waits (message: the asking subagent and the label), `working` while a turn or `/compact` runs, `idle` otherwise; the session (id and `SessionStore::file`) at startup, after `/clear` and after a resume. The App has no hook for its state, so the bridge tracks what it handed out.
- **Shutdown, as in Marv:** `session.flush()` (the App bounds it), then MCP close (at most 2 s), then Herdr release (at most 1 s).

Port notes:
- **`marv -c` resumes before the UI starts**, so nothing typed can land in the conversation being swapped out (Marv held `send` with "Loading your last session…"). `-r` sends the picker's list as the first event.
- **The saved transcript is the session's default** (the messages and the replies): the App owns the transcript the user sees, and the boundary can't read it back. Tool entries and notices aren't restored on resume. Fixing this needs a boundary change (see below).
- **`/mcp add|search|remove|trust|login|logout`** say they aren't available yet: `McpManager`'s methods are still `todo!` (#6). `/mcp` (the list) works.
- **The MCP manager is only made when a config lists servers.** Marv always made one so `/mcp add` could start the first; that needs #6.
- **A protected path's reason joins the approval's warning line** (`changes .envrc: …`), since the view's `ApprovalRequest` has no `protected` field.
- **The terminal title** is pushed, set to `marv` (OSC 0) and popped on the way out, as Marv's `cli.tsx` did, when stdin and stdout are terminals.
- marv-cli now depends on marv-sandbox (`hidden_path_folders`, `sandbox_available`).

Boundary gaps found while wiring (marv-tui's side):
- `UiEvent::Resumed` makes the App compare `resumed.model` (the saved model id, `fake/model`) with `StatusInfo::model` (the provider name, `openrouter · fake/model`), so every resume says "(it used fake/model; continuing with openrouter · fake/model)". Marv compared with the config's model; the App needs the model id, or `Resumed` needs a flag.
- No way for the engine to read the App's transcript for saving (Marv's `transcript: () => items`): a `UiEvent` request/answer or a shared transcript handle.
- `ToolStart` has no arguments, so a subagent's view can't show its task (already listed in tui.md).

**End-to-end check (no real model):** `marv` in tmux (110×34) against the oracle's `FakeServer` (a scratch binary serving a script), with a throwaway `HOME` and `MARV_CONFIG_DIR` (`provider: openrouter`, the server's `baseUrl`, `yolo: false`). Seen: the welcome banner and status bar; a prompt streamed its reply; `write_file` raised the approval with the diff; Yes wrote the file and the entry showed `created · 2 lines` with the diff under it; the final reply and the cost in the status bar; `/cost`; ctrl+c twice exited 0 with the terminal restored. `marv -c` brought the conversation back, and the next message continued it (same history in the request). ctrl+c at an approval ended the turn as `interrupted`, wrote nothing and sent no further request.
