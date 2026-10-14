# marv-cli

The `marv` binary. It covers argument parsing, headless `marv -p`, the `marv mesh` commands, the hidden `__html-convert` converter process and `__run-checks` checks runner, and launching the TUI.

Spec: the startup bullet of Marv's `CLAUDE.md` ("`bin/marv` is the `bin` entry…"), `src/cli.tsx`, `src/headless.ts`, `bin/marv`, and `tests/headless.test.ts`.

## Modules

| Marv (TS) | Rust module | Status |
|---|---|---|
| `src/cli.tsx` (argv) | `cli.rs` | **Done and tested:** the clap `Cli` (`-p/--prompt`, `run`, `--output text\|json\|stream-json`, repeated `--approve` including `mesh:<r>`, `--max-steps`, `-m/--model`, `-c/--continue`, `-r/--resume`, `-v/--version`, `-h/--help`), the `mesh ls\|wire\|plug` and hidden `__html-convert`, `__run-checks` and `__relay` subcommands, and `Cli::mode()` |
| `src/cli.tsx` (dispatch) | `main.rs` | **Done:** exit codes, stdin for `-p -`, and the converter process run before the Tokio runtime starts |
| `src/cli.tsx` (TUI startup), `src/app.tsx` (engine side), Herdr reporting | `tui.rs` | **Ported and tested (#14):** the startup sequence, the bridge (`Bridge`/`Port`: `SessionPort` over `MarvSession`), `map_event`/`map_loop`, the command actions, setup, resume, the startup notice, `TuiApprover`, Herdr states. Tests: `herdr.test.tsx`'s App-in-a-pane case, approvals (yes/no/Esc), resume, commands, `/compact` and `/clear`, first-run setup, the mappings. `/undo` (#22): the plan, its files asked through the approval queue (`once`), `undo()` and its summary, "Reading the checkpoints…"/"Undoing…" as `Busy`; refused during a turn (`app.test.tsx`'s `/undo` cases); `/checkpoints on|off`; the `checkpoint` event's line; `checkpoints_dir` under the config dir. See "The TUI bridge" below. Sprint 4 (marv-rs #23): the TUI opens the host door and offers github where gh is logged in (asked alongside startup); a host command and a github write ask with their door |
| `src/headless.ts` | `headless.rs` | **Ported and tested:** `build_approver` (`ScopeApprover`, `mesh:` grants), `exit_code_for`, `HeadlessResult`, `Collector` (step counting, refused calls including subagents', stream-json lines), `text_output` (printable), `collect_and_output`, `run_headless`. The config's `checkpoints` reaches the SDK (#22); the `checkpointing` status line. `headless.test.ts`'s "checkpoints: false in the config" spawns the CLI against a fake server, which marv-rs has no harness for yet: not ported. #23: `--approve github`, a host command listed as refused (Marv #52), github offered when `MARV_GITHUB` doesn't hide it (asked before the mesh starts) |
| `bin/marv` | (none) | Not needed. It only renamed the `bun` process to `marv`, and a native binary is already called `marv` |
| `scripts/release.ts` | `.github/workflows/release.yml`, `packaging/package.sh`, `install.sh` | **Done and tested (#19):** static musl binaries on a `v*` tag, SHA-256 sums, the installer and the `marv` → `marv-ts` switch. See "Release and install" below |
| (new) | `mesh_cmd.rs` | **Done, with tests (#16).** `start` (the process's switchboard, named by `discovery::mesh_name`, with Herdr's agent panes when in a Herdr pane) and `Mesh::open_door`; `ls` (one line per live Marv, and the Herdr agent panes it can reach), `wire` (`parse_cli_wire`: `alpha -> beta:inbox`, `alpha/worker-1 -> alpha:inbox`, `alpha -> herdr.pane.w1-p2.send:call`; asks the named Marv's door), `plug` (the stdio↔socket shim, `ShimNames`: MCP-safe tool names, sends to inboxes). The two-process test is `tests/mesh.rs` |

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
- **`--approve mesh:<receptacle>`** is split off by `marv_session::mesh::headless_grants`. It pre-approves wires to that receptacle (`HeadlessWires`, from this run's agents or another Marv's door) and the mesh tool calls to it (scope `mesh:<receptacle>:<mode>`), and nothing else. The other names keep Marv's `APPROVE_PREFIXES`.
- **The mesh in `marv -p`** (#16): only when asked for, with an `--approve mesh:<r>` or `MARV_MESH_NAME`. Otherwise a one-off run isn't on the mesh and its model isn't offered the mesh tools (the oracle runs without `XDG_RUNTIME_DIR` anyway). The TUI is always on the mesh when `XDG_RUNTIME_DIR` is set; wires asked for from other processes wait in its approval queue, and a notice says its name.
- **`__run-checks <spec>`** (#13) calls `marv_session::checks::runner::main` before the runtime starts: the project checks' runner, which the session runs inside the sandbox with this binary mounted read-only. Tested in the real sandbox by `tests/run_checks.rs`.
- **`__relay <spec>`** (#24) calls `marv_tools::relay::main` before the runtime starts: the process door's relay, run inside a sandbox with this binary mounted at `/run/marv-bin`. Tested by `tests/relay.rs`. End to end (the real binary as its own relay, a scripted model starting a dev server, sandboxed bash curling it, the host reaching it, nothing left after exit): `tests/process_door.rs`.
- **Background processes never outlive Marv (#24, Marv #51).** The TUI stops them on the way out (`stop_processes`: SIGTERM, then SIGKILL after 2 s; at most 2.5 s), `marv -p`'s `session.close()` does too, and `main` SIGKILLs whatever is left when it ends, a panic included (`StopProcesses`, a drop guard around the runtime, calling `marv_session::processes::kill_running`). If Marv is killed harder, bwrap's `--die-with-parent` takes them.
- **`--approve process`** (#24) approves starting a background process (scope `process:`); one with `network: true` (`process:net:`) needs `network` too, as Marv's `buildApprover`.
- **`__html-convert`** calls `marv_tools::web::html_convert_cli::main()` (the web reader's converter, run as its own process).

## Release and install (#19)

Marv's `scripts/release.ts` built an npm package: it bundled the CLI with its patched dependencies and wrote a bash launcher. None of that applies to one native binary, so nothing of it is ported line by line. What carries over is its check that the result stands alone. Here that's the workflow's "Check it's static" step (`file` must say statically linked).

- **`.github/workflows/release.yml`** runs on a `v*` tag; `workflow_dispatch` is a dry run without the release. It builds `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` with `cargo zigbuild` from one x86_64 runner, because zig's cc cross-compiles ring's C with no sysroot or Docker. It smoke-tests the x86_64 binary (`--version` and the installer tests against it), then attaches `marv-<target>.tar.gz`, the `.sha256` files, `SHA256SUMS` and `install.sh` to the release. macOS is a commented job.
- **Asset names have no version**, so `releases/latest/download/<asset>` always names the newest build.
- **`packaging/package.sh <target> <binary> <out>`** packs `marv`, `LICENSE` and `README.md`. It is reproducible: same binary, same bytes.
- **`install.sh`** (POSIX sh) downloads and checks the asset against `SHA256SUMS` before touching anything. It installs to `$PREFIX/bin/marv` (default `~/.local`) by a staged copy and an atomic rename. Options: `--prefix`, `--version`, `--from <local build>`, `--uninstall`. `MARV_RELEASE_URL` and `MARV_TARGET` exist for tests and mirrors.
- **The command switch.** A `marv` that isn't marv-rs is moved to `marv-ts`, and the installer says so. A symlink (npm's bin) is moved as a link. If `marv-ts` also exists, the installer refuses and changes nothing. `--uninstall` removes marv-rs only and moves `marv-ts` back. Both directions are idempotent.
- **"Is marv-rs" is decided without running the file:** native executable magic (ELF or Mach-O) plus the bytes `marv-session`. Panic locations keep the crate source paths even in a stripped release build.
- **Release profile** (root `Cargo.toml`): `lto = true`, `codegen-units = 1`, `strip = true`. The x86_64 musl binary is about 12 MB.
- **Tests:** `packaging/test-install.sh` runs in the release workflow. It works in temp dirs with a fake release over `file://`, a fake TS launcher, and a fake marv-rs binary (or a real one via `MARV_TEST_BINARY`). It covers fresh installs, the switch and re-running it, symlinks, `--version`, a bad or missing checksum, the conflict, `--from` refusals, uninstalling (twice), and arguments. It isn't in `ci.yml` yet; that file isn't #19's.

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
