# Security model

**Principle:** prompt injection can't be filtered out, so it's contained. An injected model must never hold all three of these together:
- private data;
- untrusted content in its context;
- a way out (network, the mesh, or code that runs outside the sandbox).

These rules are requirements for marv-rs from day one. They come from the 2026-10-12 audit of Marv 0.3.0; the details are in the private `marv-security-plan.md`.

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
- **Every path is checked after `realpath`.** No tool follows a symlink out of its root. That includes skill files, which the old code read outside the sandbox.

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
- **Approvals show the full payload.** A network approval lists the scripts written this session that the command will run.
- **Subagents don't inherit** network, web or mesh approvals.

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
