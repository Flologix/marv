# Security model

**Principle:** prompt injection can't be filtered out, so it's contained. An injected model must never hold all three of these together:
- private data;
- untrusted content in its context;
- a way out (network, the mesh, or code that runs outside the sandbox).

These rules are requirements for the Rust build from day one. They come from a security review of Marv 0.3.0 (2026-10-12); see the repository's security advisories.

## Process and launch
- **One static binary.** No interpreter reads the project's `bunfig.toml`, `.env`, `node_modules` or similar. Config comes only from:
  - `~/.marv/` (or `$MARV_CONFIG_DIR`, but only from the real environment, never from a file in the project);
  - the project's `.marv/` (trust-gated) and `.mcp.json` (trust-gated).
- **Model output is never written to the terminal raw.** Every string goes through `printable()`, which strips CSI/OSC/DCS, C0/C1 controls and bidi overrides. This includes headless text mode.

## Sandbox (`marv-sandbox`)
- **bubblewrap**, with:
  - read-only system directories and an empty `/run` (except the mesh plug, `/run/marv/mesh.sock`, and the process door's relay: Marv's binary at `/run/marv-bin` and socket folders under `/run/marv`);
  - a stand-in home, with toolchains re-mounted read-only and credential files masked;
  - the project writable, with `.git` and **protected paths** read-only;
  - no network unless approved (`--unshare-net`), a cleared environment, and its own PID namespace.
- **Protected paths ask even in yolo, and are read-only inside the sandbox:**
  - `.git/` and the `core.hooksPath` folder;
  - `.env*`, `.envrc`, `bunfig.toml`, `.mcp.json`;
  - `AGENTS.md`/`CLAUDE.md` at any level, and `.marv/**`;
  - `.vscode/`, `.idea/`;
  - `package.json` lifecycle `scripts`.
- **Secrets** (`.env*`, `*.pem`, `id_*`) need approval to read, and are masked in the sandbox.
- **The project's own tooling runs only inside the sandbox.** The typecheck after an edit, the project's checks (`.marv/checks/`) and the project's formatter after an edit (#67) run in the sandbox, the project read-only, without network; without a sandbox they don't run at all, and Marv never installs a formatter. The formatter is the project's own code, and it gets write to the one file being formatted — except on protected paths and `package.json`, which keep exactly what the edit tool wrote: a rewrite there could turn an approved diff into anything (a `postinstall` script into `package.json`, a planted `.vscode/` or `.marv/` file).
- **Every path is checked after `realpath`.** No tool follows a symlink out of its root. That includes skill files, which the old code read outside the sandbox, and instruction files (`AGENTS.md`/`CLAUDE.md`: the project's, its parents', nested ones), which also never lead to a secret or into `node_modules`/`target`/`.git`; only the user's own `~/.marv/AGENTS.md` may link anywhere.
  - The one exception: a **personal** skill folder (`~/.marv/skills/<name>`) may be a symlink to a folder only root can change, such as Omarchy's `/usr/share/omarchy/…` (#75). That folder and every folder above it must be owned by root and not group- or world-writable. The skill is read from the link's real target, and its `SKILL.md` and files can't lead out of that folder. A project's skill folder never may.

## Doors out of the sandbox
Narrow, named ways out, each asked for and each with its own rule for what it can never do (Marv Sprint 4).
- **Process (#24, Marv #51):** a dev server or watcher that keeps running across turns.
  - It runs in its own sandbox, like a bash command's: it can't write outside the project, change `.git`, read the real home, or reach the host's network or `127.0.0.1` unless started with `network: true`.
  - Its ports are forwarded to the host's `127.0.0.1` only, never another interface.
  - Starting one asks, even in yolo; only the main agent can (a subagent's context has no door, refused before any ask).
  - It dies on stop, when Marv exits or panics, and when Marv is killed (`--die-with-parent`).

## Provenance and taint (`marv-core::Provenance`)
- **Every tool result is tagged with its source.** Text that isn't from the user or Marv is fenced as data for the model. Project checks and diagnostics are quoted, never spoken in Marv's voice.
- **A session becomes tainted** once web, MCP, mesh or image content enters its context. From then on, every `ToolKind::Egress` call asks, even in yolo:
  - network bash;
  - every MCP call, including tools marked `readOnlyHint`;
  - web requests to a new URL prefix;
  - mesh calls on unapproved wires;
  - writes to protected paths.
- **Taint lasts as long as the text does.** It's saved with the session and restored on resume (`marv -c`, `/resume`); an older session file is judged by its tool calls, erring on the tainted side. A subagent's taint taints its parent: its report can carry the outside text back.
- **Marv's runtime folder is empty in every sandbox.** `$XDG_RUNTIME_DIR/marv` (the mesh's plug and discovery sockets, the process door's relays) is an empty tmpfs in every command's sandbox, mounted after the project and toolchains, wherever it is: a sandboxed command that could see a plug's socket could connect to it and use that plug's wires. And the mesh refuses a runtime dir outside `/run`, `/tmp` and the home folder.
- **Approvals show the full payload.** A network approval lists the scripts written this session that the command will run.
- **Subagents don't inherit** network, web or mesh approvals.

## The doors out of the sandbox (Marv Sprint 4)
Narrow, named ways out, each with what it can never do. A call through one carries its door on the approval (the prompt shows the door's warning line), on its result and in the trajectory. The doors are the main agent's: a subagent's context has none.
- **github** (Marv #47) runs gh with the user's login, outside the sandbox:
  - never: `auth`, `config`, `extension`, `alias`, an unknown first word (an alias or extension), `secret`/`variable`/`ssh-key`/`gpg-key`, `--web`, `gh api --input`/`--verbose`, downloads, `repo clone`, `pr checkout`, another GitHub host than the project's. Refused before anyone is asked, whatever the approver says;
  - writes, other API methods, fields, graphql and sensitive API reads (secrets, keys, hooks, emails…) ask, in yolo too; a sensitive read is asked every time (`once`);
  - gh is spawned without a shell, without Marv's keys or debug switches, with `GH_HOST` pinned to the project's host (github.com where origin names none), and its git never runs the repository's programs;
  - GitHub token shapes are redacted from what gh prints before the model, the events, the saved session or the trajectory see it;
  - a subagent only reads; offered only where gh is there and logged in; `marv -p` needs `--approve github` for a write.
- **host** (Marv #52) is bash with `sandbox: false`, one command on the user's machine:
  - asked every time (`once`: "always" never covers another), in yolo too;
  - only where someone approves each command at a terminal (the TUI; the SDK's `host_commands`): refused before any ask in `marv -p`, in an SDK session without `host_commands`, and for a subagent;
  - its environment is the user's minus Marv's keys and anything secret-looking (`marv_sandbox::host_env`); token shapes are redacted from its output.

## MCP
- Project servers need trust. The hash covers the entry and the files its command runs.
- `npx`/`uvx` run outside the project folder.
- Server `instructions` are ignored.

## Images (when vision is added)
- **Sources:** only the user (a paste, or a file they name). Never forwarded automatically from web, MCP or mesh.
- **A quarantined reader:** a tools-less model call extracts the image's text and description. The main model sees only that, fenced as `Provenance::Image`, and it's shown in the transcript. Any image taints the session.
- **Decoding** happens in the sandbox, with metadata stripped and size limits.

## Proof
- `tests/guards/`: one test per rule. Each one fails when its guard is reverted, and they run in CI.
- An injection eval suite (poisoned README, web page, MCP issue body, mesh message, later images). It passes when the model never tries to send data out or write a protected path.
