# Changelog

All notable changes to Marv are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and Marv uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html). While it's below 1.0.0, a minor version (0.**x**.0) may contain breaking changes; a patch version (0.1.**x**) never does.

What counts as Marv's public API: the SDK (`@flologixai/marv/sdk`), the `marv` command's flags and slash commands, and the files it reads (`~/.marv/config.json`, `.marv/skills/`, `.marv/agents/`, `.mcp.json`, `AGENTS.md`, saved sessions).

## [Unreleased]

### Added

- Loop guard: detects when a model gets stuck repeating the same sentence or tool calls, and stops it. Text repetition (8+ consecutive identical lines) drops the reply and tells the model; tool-call loops (the same call or a 2-cycle repeated 4+ times without a write tool in between) first warn with a note in the tool result, then stop the turn. A new `loop` stop reason in trajectories lets `bun run stats` show how often each model does it. (Issue #37)

- `write_file` has `append: true`: add content at the end of an existing file instead of replacing it (the cut-off notes and the system prompt now point there for writing a long file in parts, instead of `edit_file`); a file that doesn't exist is just created, and a last line without a newline gets one first, which the result says.
- After the model changes files in a TypeScript project, Marv runs the project's typecheck (in the sandbox, read-only) and tells the model about errors its changes added, anywhere in the project, so it fixes the callers it broke before saying it's done. `/diagnostics on|off`, config `diagnostics`, SDK option `diagnostics` (on by default). Needs the sandbox, a `tsconfig.json` in the folder Marv starts in, and the project's own `typescript` (or tsgo) in that folder's `node_modules` (a global tsc isn't used). The SDK also exports the `CheckResult` and `TsError` types.
- Herdr support: in a [Herdr](https://herdr.dev) pane, Marv tells Herdr what it's doing, so Herdr's sidebar shows it working, waiting for your approval, or done, and `herdr agent list`, `get`, `read` and `wait` see it. Nothing to set up. Herdr 0.8.2 doesn't yet let `herdr agent prompt` or `send-keys` drive agents it doesn't have built in; `herdr pane send-text` and `send-keys` do.
- Malformed tool calls from weaker models are repaired where the meaning is beyond doubt, instead of bouncing back as an error (issue #10): invalid JSON gets raw control characters in strings escaped and parsed again; `bash`'s `timeout` given in milliseconds is divided and clamped to the maximum; a project path written with a leading slash that only exists relative to the project (`/src/x.ts`) is read as that file. Every repaired call's result says what was repaired (`Marv: …`), so the model learns the right form.
- SDK: `maxSteps` sets how many steps a turn runs before it stops (without `approve`) or asks whether to keep going (default 25).
- The step limit is configurable: `/steps` shows the interval, `/steps 40` sets it, `/steps off` (or `0`, or config `maxSteps: 0`, or `MARV_MAX_STEPS=0`) never asks — a long run just keeps going, like Claude Code's and Codex's, while subagents still stop at their own 50. Without an approver, `0` also lifts the hard stop for the main agent's turns.
- CI: a GitHub Actions workflow (`.github/workflows/ci.yml`) runs `bun install`, `bun run typecheck`, `bun test` and `bun run eval --verify` on every pull request and push to main (no API key: `--verify` only checks that every eval task's check fails before and passes with its solution). The runner installs bubblewrap and lifts Ubuntu 24.04's AppArmor restriction on unprivileged user namespaces, so the sandbox tests run rather than skip (the sandbox is Marv's main safety boundary), and the job summary says whether the sandbox really ran. Installs are cached (`~/.bun/install/cache`, keyed on `bun.lock`). `tests/ci.test.ts` checks the workflow file itself.
- A long turn compacts mid-turn: when the context is nearly full after a step's tool calls are in (`runAgent`'s new `onFull`), Marv summarizes the conversation there, not only before the next user message, so a turn with many tool calls can't outgrow the window (Ollama silently dropping old context, OpenRouter rejecting the request). A user note after the summary tells the model to continue where it left off (before a user message the message itself follows the summary). A `compacted` event says it happened, and the usual `status`/`compaction` events say how it went; Esc during it stops the turn, and a failed summary isn't tried again until the next turn.

### Changed

- When a sandboxed command without network fails in a way that looks like the missing network (DNS errors, refused connections, timeouts, or tools like `gh`, `curl`, `git fetch` and `npm install`), the model is told the sandbox blocked it and to retry with network on, instead of taking, say, `gh auth status`'s "invalid token" for real.

### Fixed

- File tools read a path starting with `~/` as the home folder, not a folder named `~` in the project (outside the project it's refused, as any other path there). `edit_file` no longer suggests `write_file` for a missing file whose folder doesn't exist either, which led a model to create a stray copy instead of editing the real file.
- A file rewritten with `write_file` showed only removed lines in the collapsed diff (and the approval prompt). It now shows the first few removed lines, then the start of the new file, also with ctrl+o for a rewrite of more than 400 lines.
- SDK: a rejected API key or an unknown model no longer says to run `/setup` or `/model`; the error names the option to change instead (`provider.apiKey`, `provider.model`).
- Ollama: the cap on each reply shrinks to the room the prompt leaves in the context window, so in a long conversation the prompt plus the reply no longer outgrow it (the cap had been a fixed share of the window, up to half with thinking on).
- Ollama never runs a model past the context length it was trained for: `/api/show` reports it (`model_info.*.context_length`), and the configured `contextLength` (still the upper bound — Ollama can't tell what fits in VRAM) is capped at it, for `num_ctx`, the reply cap, and the context-usage report.

## [0.1.2] - 2026-10-08

### Security

- The sandbox no longer exposes the host's `/run`. It had been mounted read-only, but a read-only mount doesn't stop a process from connecting to the sockets in it. A sandboxed command (which runs without asking in yolo mode, the default) could reach the user's D-Bus session and start programs outside the sandbox with `systemd-run --user`, and could reach the GPG and SSH agents, the keyring, the display and docker's socket. `/run` is now an empty folder in the sandbox. Only the folders on `PATH` and the DNS config that live there are shown again, read-only. Affects 0.1.0 and 0.1.1: update with `npm i -g @flologixai/marv@latest`.

### Changed

- The system prompt always explains how to set up an MCP server (where the config goes, its format, and that `~/.marv` isn't reachable from the sandbox), so the model hands you a correct config instead of inventing keys.

## [0.1.1] - 2026-10-06

### Changed

- README: on Windows, use WSL (the npm page shows the README of the version published).

## [0.1.0] - 2026-10-06

The first public release, on npm as `@flologixai/marv`.

### Added

- A full-screen terminal UI with streaming Markdown replies, mouse-wheel scrolling, drag-to-copy and a `/` command menu.
- An agent loop with tools: `read_file`, `glob`, `grep`, `edit_file`, `write_file`, `bash`, `web_fetch`, `memory`, `skill` and `agent`. It asks before continuing past 25 steps, retries an empty reply, and never runs a tool call cut off at the output limit.
- `edit_file` matches forgivingly (trailing whitespace, indentation), keeps CRLF line endings, and shows the closest lines when nothing matches. `edit_file` and `write_file` say when a change breaks the file's syntax.
- Providers: OpenRouter (any tool-capable model) and Ollama (local models), with a model picker and `/think`.
- A bubblewrap sandbox for commands on Linux, and yolo mode (on by default): sandboxed commands and edits in the project run without asking; everything else shows a diff or the command first.
- `web_fetch`: web pages as Markdown, and GitHub repositories, folders and files through GitHub's API.
- MCP servers (stdio and Streamable HTTP) in Claude Code's `.mcp.json` format; a project's servers start only once trusted (`/mcp trust`).
- Skills (`.marv/skills/`), subagents with Claude Code-compatible agent types (`.marv/agents/`), optionally in their own git worktree, and memory across sessions.
- Saved sessions (`marv -c`, `marv -r`, `/resume`), automatic and `/compact` compaction, and token and cost tracking (`/cost`).
- Trajectory logs with feedback (`/good`, `/bad`, `/label`) and `bun run stats`.
- The SDK: `createSession()` from `@flologixai/marv/sdk` runs Marv from code, without the UI.
- Evals: `bun run eval` runs real models on small tasks with hidden checks.

[Unreleased]: https://github.com/FlologixAI/marv/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/FlologixAI/marv/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/FlologixAI/marv/releases/tag/v0.1.0
