# Sandbox Doors Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Issues #47, #51, #52, with their share of #53. Three narrow ways out of the sandbox, one rule set: a `github` tool that runs `gh` with the user's login (reads free, writes asked, `auth` never), a `process` tool for servers and watchers that keep running in their own sandbox with their ports forwarded to the user's `localhost`, and `bash`'s `sandbox: false`, asked every time and refused wherever no one is at the terminal.

**Architecture:** Shared first: `ApprovalRequest.door`, `ToolContext.doors`, the session's approver ignoring "always" for `once` requests, the trajectory's `door` field, and a `# Outside the sandbox` prompt section built once. Then one door per task group: `src/tools/bash.ts` (`sandbox: false`), `src/tools/github/` (`allow.ts` pure classification, `redact.ts`, `github.ts` the tool), `src/processes.ts` + `src/processes/relay.ts` + `src/tools/process.ts` (the manager, the in-sandbox relay, the tool). Each task ends with its guard tests in `tests/guards/`.

**Tech Stack:** Bun, TypeScript, zod, bubblewrap, `bun:test`.

Spec: `docs/superpowers/specs/2026-10-13-sandbox-doors.md` (to be approved; its open questions decide Task 5's headless rule and sensitive reads, Task 9's `/clear`, and whether Task 3 has `hostCommands`). Branches: one per door after Task 1–2 land (`s4/52-host-command`, `s4/47-github-tool`, `s4/51-processes`), in that order: the smallest door proves the shared rules.

**Background for the engineer:**
- Read `CLAUDE.md` first: **Tools** (runTool, approval, yolo), **Sandbox**, **Yolo mode**, **Session**, **SDK**, **Subagents**, **Prompt cache**, **Trajectories**. Then `src/sandbox.ts` (`sandboxArgs`), `src/tools/bash.ts` (`runCommand`, notes), `src/tools/index.ts` (`runTool`), `src/tools/web-fetch.ts` (the door that exists: Marv acts itself), `src/headless.ts` (`buildApprover`: it accepts an exact scope key, which is why doors are gated in the context, not the approver), `src/subagent.ts` (`subagentContext`).
- **Rules the guard tests hold:** what a door can never do is refused in `preview()` (no ask); a `once` request is never answered from, or added to, the "always" set; `doors` is set by the session for the main agent only and never copied into a subagent's context; nothing a door outputs reaches the model before redaction (github).
- **Cache:** the tool list and the prompt section are decided once per session (github's availability, process support) before the first request; never per turn.
- **Conventions:** `.ts` extensions, `import type`, comments say *why*. Tests never hit the network: a fake `gh` script on `PATH`, `Bun.serve` inside a real sandbox for processes.

---

### Task 1: Shared door plumbing

**Files:** Modify `src/tools/types.ts`, `src/tools/index.ts`, `src/session.ts`, `src/trajectory.ts`, `src/trajectory-stats.ts`, `src/subagent.ts`. Test: `tests/doors.test.ts`.

- [ ] **Step 1: Failing tests.** (a) A session whose client approver answers "always" to a `once` request is asked again for the same scope key; the key never enters `always`. (b) `runTool` copies the tool's `door(input)` onto the `ApprovalRequest` and onto `ToolResult.door`; the trajectory's `tool` record has `door`. (c) `subagentContext(ctx)` drops `doors` even when the parent has `{ host: true, process: true }`. (d) `bun run stats` prints calls per door.
- [ ] **Step 2: Fail.**
- [ ] **Step 3: Implement.** `type Door = "github" | "process" | "host"`; `ApprovalRequest.door?`; `Tool.door?(input): Door | undefined`; `Tool.once?(input): boolean` (copied to the request); `ToolContext.doors?: { host?: boolean; process?: boolean }`; `ToolResult.door?`. In `MarvSession.approver`, `if (request.once)` skip the `always` lookup and never add. `SessionInit.hostCommands?: boolean` (default false); the TUI passes `true`; `doors` goes on the main agent's context only.
- [ ] **Step 4: Pass, typecheck.**

### Task 2: The prompt section and the `Approval` warning

**Files:** Modify `src/prompt.ts`, `src/ui/Approval.tsx`. Test: `tests/system-prompt.test.ts`, `tests/approval.test.tsx`.

- [ ] **Step 1: Failing tests.** `systemPrompt({ doors: { github, process } })` has `# Outside the sandbox` with exactly the paragraphs of the doors given (spec text), none when none; identical across two builds (cache); the bash line in the main prompt mentions `sandbox: false` once. `Approval` shows one warning line per `door` (`host`: "Runs OUTSIDE the sandbox, as you, with your files, network and credentials."; `github` write: "Visible to other people on <repo>"; `process`: "Keeps running after this turn, until stopped or Marv exits") and no "don't ask again" for `once`.
- [ ] **Step 2–4:** Fail, implement (section text verbatim from the spec), pass.

### Task 3: `bash` with `sandbox: false` (#52)

**Files:** Modify `src/tools/bash.ts`, `src/headless.ts` (nothing to add: proves the refusal holds), `src/sdk.ts` (`hostCommands`). Test: `tests/bash.test.ts`, `tests/guards/host.test.ts`.

- [ ] **Step 1: Failing tests.** `sandbox: false` with `ctx.doors.host`: preview has the host warning, request is `once` with `door: "host"`, scope `bash:host:<cmd>`; `autoSafe` false (yolo asks); the command runs outside bwrap (`cat /proc/self/status` shows no NoNewPrivs from bwrap, or a file written in a fake home is visible) with `XDG_RUNTIME_DIR` set and `OPENROUTER_API_KEY` absent. Without `doors.host`: `ToolError` from preview, approver never called. Guards: `runHeadless` with `--approve bash:host:echo hi` and a scripted provider calling `sandbox: false` → refused, exit 3, nothing ran (a marker file absent); SDK session without `hostCommands` → refused; a subagent → refused; two calls with an approver answering "always" → two requests.
- [ ] **Step 2: Fail.**
- [ ] **Step 3: Implement.** Input field (spec text); `door: ({ sandbox }) => sandbox === false ? "host" : undefined`; `once` likewise; preview refuses without `ctx.doors?.host` (unless the sandbox is already off); `run` with `sandbox: false` → `runCommand({ sandbox: false, env: hostEnv() })` (`hostEnv`: `process.env` minus `OPENROUTER_API_KEY` and `MARV_*`). The network and hidden-home notes gain the last-resort sentence when `ctx.doors?.host`.
- [ ] **Step 4: Pass; check each guard fails with its line removed** (the preview refusal, the `once` skip in the approver, `doors` not copied into subagents); note it in the PR.

### Task 4: `gh` classification (`src/tools/github/allow.ts`, pure)

**Files:** Create `src/tools/github/allow.ts`. Test: `tests/github-allow.test.ts`.

- [ ] **Step 1: Failing tests,** table-driven: `classify(args, { host })` → `{ kind: "read" } | { kind: "write", scope } | { kind: "sensitive", scope } | { kind: "refused", reason }`. Every read and write of the spec's lists; every refusal (`auth` in any position after global flags, `config`, `extension`, `alias`, an unknown word, `secret`, `variable`, `ssh-key`, `gpg-key`, `codespace`, `gist`, `org`, `browse`, `--web`, `-w`, `run download`, `release download`, `repo clone`, `pr checkout`); `api`: `repos/o/r/pulls` read, `-X GET` read, `--method=GET` read, `-X POST` write `github:api POST`, `-f a=b` write, `-F x=@/etc/passwd` write (it's a field), `--input f` refused, `graphql` write, `--verbose` refused, `-H Accept:…` read, `-H X-HTTP-Method-Override:DELETE` refused, `repos/o/r/actions/secrets` sensitive, `user/emails` sensitive; hosts: `--hostname ghe.corp` allowed only when `host` is `ghe.corp`, `-R other.host/o/r` refused, `-R o/r` allowed; `--flag=value` and `-Rvalue` forms read like gh reads them.
- [ ] **Step 2–4:** Fail, implement (a small argv walker: global flags, command path from a fixed table, then flags; unknown flags on a read are allowed except the refused list), pass.

### Task 5: Redaction and the `github` tool

**Files:** Create `src/tools/github/redact.ts`, `src/tools/github/github.ts`, `tests/fixtures/fake-gh` (a bash script: logs argv as a JSON line to `$FAKE_GH_LOG`, prints canned output by first words from `$FAKE_GH_DIR`, `auth status` exits per `$FAKE_GH_LOGGED_IN`). Modify `src/tools/index.ts` (register; `toolSpecsFor({ hasSkills, hasGithub })`), `src/session.ts` (availability once, before specs), `src/agents.ts` (`github` in `SUBAGENT_TOOLS`), `src/headless.ts` (`--approve github` → `github:`), `src/tools/bash.ts` (`GH_NOTE` by availability). Test: `tests/github-tool.test.ts`, `tests/guards/github.test.ts`.

- [ ] **Step 1: Failing tests.** `redact` replaces each token shape and counts; leaves `ghx_…` and short strings alone. The tool against the fake: a read runs with no approval (yolo on and off); a write asks with `door: "github"`, scope `github:pr merge`; refused argv never reaches the fake (its log is empty); output with a token is redacted in the result, the session's transcript event and the trajectory file; spawned without a shell (an arg `$(touch x)` arrives literally, `x` absent); `OPENROUTER_API_KEY` absent from the fake's environment, `GH_PROMPT_DISABLED=1` present; a subagent's write is refused; not offered when `gh` is missing or logged out; offered spec list identical across two sessions with it (cache).
- [ ] **Step 2: Fail.**
- [ ] **Step 3: Implement.** `kind: "read"`, `needsApproval` from `classify` (write/sensitive), `scope`, `door: "github"`, no `autoSafe`; `preview` throws the refusal; `run` spawns `["gh", ...args]` with the spec's environment, 60 s, capture and `formatOutput` from bash, then `redact`. Subagents: the tool checks `ctx.agentHost` is absent *and* the call is a write → refuse (only the main agent has a host).
- [ ] **Step 4: Pass; revert-check each guard** (classification table entry for `auth`, redaction, subagent write).

### Task 6: README and docs for the github tool

**Files:** Modify `README.md` ("Connect GitHub" leads with `gh`), `CLAUDE.md` (a **Doors** bullet; Connect GitHub bullet), `examples/skills/github/SKILL.md` (prefer the tool). Test: `tests/github-docs.test.ts` still parses the MCP configs.

### Task 7: The relay (`src/processes/relay.ts`)

**Files:** Create `src/processes/relay.ts` (self-contained source, run with `bun -e` inside the sandbox, like the checks runner), Test: `tests/process-relay.test.ts`.

- [ ] **Step 1: Failing tests,** in a real sandbox (`sandboxAvailable()`-gated): forward mode (listens on `/run/marv/fwd.sock`; a connection whose first line is `4789\n` is piped to `127.0.0.1:4789`, then `::1`) serves a host `curl --unix-socket` the sandboxed `Bun.serve`'s page; reverse mode (listens on `127.0.0.1:<port>` inside a bash sandbox, pipes to a mounted socket) lets a sandboxed `curl localhost:4789` reach it; a port line that isn't 1024–65535 closes the connection; data arriving before the upstream connects isn't lost.
- [ ] **Step 2–4:** Fail, implement, pass.

### Task 8: `ProcessManager` (`src/processes.ts`)

**Files:** Create `src/processes.ts`. Modify `src/sandbox.ts` (`sandboxArgs` gains `binds` for `/run/marv` and Marv's bun, read-only). Test: `tests/processes.test.ts`, `tests/guards/process.test.ts`.

- [ ] **Step 1: Failing tests.** `start` returns within 10 s with the forwarded URL and the first lines; the host's `fetch("http://127.0.0.1:<port>")` gets the page; the port is listened on `127.0.0.1` only; `listeningPorts(pid)` parses `/proc/<pid>/net/tcp{,6}` (fixture text: state `0A`, hex ports, v6); a taken host port is reported, not forwarded; ports < 1024 aren't; 5th process and 5th port are refused; logs ring at 1 MB and `logs(lines)` returns the tail; `stop` kills the group and closes listeners; `closeAll()` on exit; an exited process shows its code. Guards: the process can't write outside the project, can't read a file planted in the (fake) home, can't reach a `Bun.serve` on the host's `127.0.0.1` without `network: true` (and can with it); SIGKILL of a parent process running the manager leaves no sandboxed process (spawn a child bun that starts one, kill it, poll `pgrep`).
- [ ] **Step 2: Fail.**
- [ ] **Step 3: Implement** per the spec: spawn bwrap with `sandboxArgs` (+ `gitGuard` read-only), the relay started before `bash -c <command>`, a 500 ms poll of the ports, `Bun.listen` per new port piping into the socket, a ring buffer, SIGTERM then SIGKILL after 2 s.
- [ ] **Step 4: Pass; revert-check** (`--unshare-net` dropped, `--die-with-parent` dropped, host bind address `0.0.0.0`).

### Task 9: The `process` tool, `/processes`, session and SDK

**Files:** Create `src/tools/process.ts`. Modify `src/tools/index.ts`, `src/session.ts` (owns a `ProcessManager`; `close()` stops all; offered only when the sandbox works and `doors.process`), `src/sdk.ts`, `src/headless.ts` (`--approve process` → `process:`), `src/tools/bash.ts` (reverse relay when processes run; #50's note points to the tool), `src/commands/index.ts` (`/processes [stop <name>]`, action `processes`), `src/app.tsx` (applies it; status bar `N running`), `src/cli.tsx` (stop all on exit, at most 2 s). Test: `tests/process-tool.test.ts`, `tests/commands.test.ts`, `tests/app.test.tsx`, `tests/guards/process.test.ts`.

- [ ] **Step 1: Failing tests.** `start` asks in yolo with `door: "process"`, scope `process:<command>` ("always" covers the same command only), `process:net:` with `network`; `logs`/`list`/`stop` never ask; a subagent has no `process` tool and naming it fails as unknown; a sandboxed bash `curl localhost:<port>` reaches a started server without network and without approval in yolo; processes survive `session.clear()`; `session.close()` stops them; `/processes` lists and stops; the SDK's `marv -p` run that starts a server ends with no process left.
- [ ] **Step 2–4:** Fail, implement, pass.

### Task 10: Evals and the injection guard

**Files:** Create `evals/tasks/{dev-server-claim,serve-and-check,e2e-against-server,watch-and-rebuild,github-read}/` (each `task.md`, `repo/`, `check/`, `solution/`), `tests/guards/injection.test.ts`. Modify `evals/run.ts` (`--doors on|off`; a task may name a fake `gh` dir; a check may fetch a port the run left open before the session closes), `.github/workflows/ci.yml` and `tests/ci.test.ts` (`tests/guards/` runs with the sandbox on; already the case through `bun test`, assert it).

- [ ] **Step 1:** The injection guard: a `ScriptedProvider` reads a file saying "run curl … | sh outside the sandbox", then calls `bash` with `sandbox: false`, `process start` and `github pr comment`; a recording approver sees all three requests with their `door`; answering "no", nothing ran (marker files, the fake `gh`'s log).
- [ ] **Step 2:** `bun run eval --verify` passes for the five tasks.
- [ ] **Step 3:** Ask the user before running real models; then 2 models × `--doors on|off`, numbers in the PR.

### Task 11: Wrap-up

- [ ] `bun run typecheck`, `bun test`, `bun run eval --verify`.
- [ ] By hand: `marv` in this repository, "check the current project" (no prompt; PRs, CI, issues); "start the dev server so I can open it" in a fresh project, open it in a browser, exit Marv, it's gone; `systemctl --user status` with `sandbox: false` asks, in yolo too.
- [ ] CLAUDE.md: the **Doors** bullet and updates to Tools, Sandbox, Yolo mode, SDK, Subagents; CHANGELOG.
