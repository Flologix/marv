# Git Checkpoints Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Issue #16. Every turn that changes files gets a *before* and an *after* snapshot (git commits written through a temporary index, kept under `refs/marv/checkpoints/<session>`, or in Marv's own git dir outside a repository), and `/undo` shows the last turn's changes and, on a yes, puts those files back, without touching the user's branch, index or stash, and without rewriting the conversation.

**Architecture:** `src/git.ts` gains `runGitAsync` (same rules as `runGit`). A new `src/checkpoints/` holds `store.ts` (`CheckpointStore`: where snapshots go, `snapshot()`, the ref chain, pruning), `undo.ts` (`planUndo`, `applyUndo`: diff, changed-since, restore path by path) and `turn.ts` (the per-turn memoized `checkpoint()` and the end-of-turn *after*). `runTool` calls `ctx.checkpoint` before any non-read call; `subagentContext` passes it to shared-folder subagents. `MarvSession` owns one store, runs the pair per turn, exposes `undoPlan()`/`undo()`, and prepends the undo note to the next user message. `/undo` and `/checkpoints` are new commands; the App shows the plan with `DiffView` as a `once` approval.

**Tech Stack:** Bun, TypeScript, git plumbing (`add -A`, `write-tree`, `commit-tree`, `update-ref`, `diff-tree`, `cat-file`, `hash-object`), `bun:test`.

Spec: `docs/superpowers/specs/2026-10-13-git-checkpoints.md` (to be approved; open question 1 decides Task 2's storage, 2–4 Tasks 5–6's scope). Branch: `s4/16-checkpoints`.

**Background for the engineer:**
- Read `CLAUDE.md` first: **Agent loop**, **Session**, **Tools** (approval, `runTool`), **Sandbox**, **Yolo mode**, **Worktrees** (`commitGitDir`: why the index is copied), **Diagnostics** (`beforeChange`: the memoized per-turn hook this mirrors), **Prompt cache** and **Compaction**.
- **Rules that tests guard:** a snapshot never changes `HEAD`, the user's index (compare its bytes), branches, `refs/stash` or `git status`; no hook or fsmonitor runs; a failed snapshot never fails the turn; every request still extends the previous one exactly (the undo note goes into a user message Marv was sending anyway).
- **Conventions:** `.ts` extensions in imports, `import type` for types, comments say *why*. Temp repos via `mkdtempSync` + `git init` (as `tests/worktree.test.ts` does). `gitEnvironment` drops inherited `GIT_*`, so a test can't isolate git with `GIT_CONFIG_GLOBAL`; the "no `user.email`" test proves Marv's own identity works whatever the global config says.
- **Commands:** `bun test <path substring>`, `bun test -t "<name>"`, `bun run typecheck`, `bun test`.

---

### Task 1: `runGitAsync` (`src/git.ts`)

**Files:** Modify `src/git.ts`. Test: create `tests/git.test.ts`.

- [ ] **Step 1: Failing tests.** Same result shape as `runGit`; `NO_PROGRAMS` applied (a `pre-commit` hook and `core.fsmonitor=<script that touches a file>` in a temp repo never run); a `GIT_DIR` in `process.env` is ignored; the timeout kills git and returns `timedOut`; an `AbortSignal` kills it too.
- [ ] **Step 2: Fail. Step 3: Implement** with `Bun.spawn` (stdin `"ignore"`, `timeout`, `killSignal`), sharing `NO_PROGRAMS` and `gitEnvironment`. **Step 4: Pass.**

### Task 2: The store (`src/checkpoints/store.ts`)

**Files:** Create `src/checkpoints/store.ts`. Test `tests/checkpoints.test.ts`.

- [ ] **Step 1: Failing tests.**
  - `locate(root)`: a repository's top → `{ kind: "repo", gitDir, top, prefix: "" }`; a subfolder → `prefix: "pkg/"`; a git worktree → its own git dir for the index, the common dir's refs; no repository → `{ kind: "own", gitDir: <configDir>/checkpoints/<projectKey>/git }`, created bare on first use with `info/exclude` holding the walk's skip list.
  - `snapshot(label)` in a repo with tracked, modified, untracked, ignored and force-added files: the commit's tree has all but the ignored one; `HEAD`, `.git/index` (bytes), `git branch`, `git stash list` and `git status --porcelain` are identical before and after; the ref moved, the parent is the previous checkpoint; a subfolder project's tree has only its prefix.
  - no `user.email` configured: works (fixed Marv identity via `GIT_AUTHOR_*`/`GIT_COMMITTER_*` in the pinned env).
  - own git dir: a second snapshot reuses its index (count `hash-object` work by timing is flaky: assert the index file persists); over the file or size cap: `{ ok: false, reason: "too-large" }`.
  - `dropTo(sha)` moves the ref back (used for a no-op pair); `chain()` lists checkpoints newest first with turn, kind (`before`/`after`/`pre-undo`/`undo`) parsed from the message.
  - `prune({ keep: 20, days: 14 })`: deletes older session refs beyond those; in the own git dir also runs `gc` (at most once a day, a stamp file).
- [ ] **Step 2: Fail.**
- [ ] **Step 3: Implement.** All calls through `runGitAsync` with pinned `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`. Repo kind: copy `<gitDir>/index` to a temp file (missing → empty), `add -A -- <prefix or .>`, `write-tree`, `commit-tree -p <ref>` (none on the first), `update-ref <ref> <new> <old>` (compare-and-swap: two Marv processes on one session id can't happen, but a stale old value fails loudly instead of losing a checkpoint), delete the temp index in `finally`. Result `{ ok: true, sha, tree, ms }` or `{ ok: false, reason, message }`; never throws.
- [ ] **Step 4: Pass; typecheck.**

### Task 3: Plan and apply an undo (`src/checkpoints/undo.ts`)

**Files:** Create `src/checkpoints/undo.ts`. Test `tests/checkpoints.test.ts` (undo section).

- [ ] **Step 1: Failing tests** (each: snapshot, change files, snapshot, plan, apply, check the disk).
  - modified → restored; created → deleted and its now-empty folders removed; deleted → recreated with its folders; `chmod +x` → mode restored; a symlink → recreated as a symlink with the old target.
  - a file edited after the *after* snapshot → `changedSince` lists it; it is still restored.
  - `reported` paths (from tools) in neither tree → `notRestored`.
  - a path whose parent is now a symlink to a folder outside the project → refused, listed in `refused`, nothing written outside.
  - `applyUndo` takes a `pre-undo` snapshot first and appends an `undo` marker; `nextUndoable(chain)` skips undone turns, so two undos walk back two turns.
  - the plan's diffs: `DiffShown` from `src/tools/diff.ts` (current → before), binary named only.
- [ ] **Step 2: Fail.**
- [ ] **Step 3: Implement.** `planUndo(store, turn)`: `diff-tree -r -z --no-renames`, `hash-object` of each current file vs the after blob, blobs via `cat-file --batch`. `applyUndo(plan)`: per path, `realpath` of the parent must be inside the root (as `resolveInProject`), write via a temp file + rename, `symlink`/`chmod` per mode. Returns `{ restored, deleted, recreated, refused }`.
- [ ] **Step 4: Pass.**

### Task 4: The hook (`src/checkpoints/turn.ts`, `src/tools/index.ts`, `src/subagent.ts`)

**Files:** Create `src/checkpoints/turn.ts`; modify `src/tools/types.ts` (`ToolContext.checkpoint?: () => Promise<void>`), `src/tools/index.ts` (`runTool`), `src/subagent.ts` (`subagentContext`). Test `tests/tools.test.ts`, `tests/agent-tool.test.ts`.

- [ ] **Step 1: Failing tests.** `runTool` awaits `ctx.checkpoint` for `write_file`, `edit_file`, `bash` (approved or yolo) after the approval and before `run`; not for `read_file`/`glob`/`grep`/`web_fetch`/`skill`/MCP tools; not when declined, invalid or aborted; a rejecting `checkpoint` doesn't fail the call. A shared-folder subagent's context carries it; a worktree subagent's doesn't. `CheckpointTurn.checkpoint()` called concurrently runs one snapshot.
- [ ] **Step 2: Fail. Step 3: Implement.** `CheckpointTurn` (one per turn): `checkpoint()` memoized, errors caught into `failed`; `finish()` takes the *after*, drops the pair if the trees match. **Step 4: Pass.**

### Task 5: The session (`src/session.ts`)

**Files:** Modify `src/session.ts`, `src/sdk.ts`, `src/config/config.ts`, `src/trajectory.ts`. Test `tests/session.test.ts`, `tests/agent.test.ts`, `tests/sdk.test.ts`.

- [ ] **Step 1: Failing tests** (`ScriptedProvider`, temp repo as root):
  - a read-only turn: no ref; a `write_file` + a bash `echo > f` turn: one pair, `undoPlan()` lists both files; a no-op bash turn: the pair dropped.
  - the turn's *after* is taken on Esc and at the step limit too.
  - `undo(plan)` restores; the next request's last user message starts with the `Marv:` undo note; every request extends the previous one; the transcript has no note.
  - `undoPlan()`/`undo()` reject during a turn; `checkpoints: false` (init and `configure`) takes none; `clear()` uses a new ref, `resume(id)` finds the old one.
  - trajectories get `checkpoint` and `undo` records; the SDK's `checkpoints` option works.
- [ ] **Step 2: Fail.**
- [ ] **Step 3: Implement.** One `CheckpointStore` per session (`SessionInit.checkpoints`, `configDir` for the own git dir); `runTurn` makes a `CheckpointTurn`, passes `checkpoint` in the tool context, emits `status: "checkpointing"` if it runs over 300 ms, and calls `finish()` in its cleanup before `turn_end`. `pendingNote` (like the MCP announcement) is prefixed to the next `send()`'s user message. Prune once, in the background, at the first turn.
- [ ] **Step 4: Pass; the whole suite.**

### Task 6: `/undo`, `/checkpoints` and the App

**Files:** Modify `src/commands/index.ts`, `src/app.tsx`, `src/ui/Approval.tsx` (if the plan needs more than `preview`), `src/ui/DiffView.tsx` (several files). Test `tests/commands.test.ts`, `tests/app.test.tsx`.

- [ ] **Step 1: Failing tests.** `/undo` → `{ type: "undo" }`; `/checkpoints on|off` → `{ type: "checkpoints", on }`, bare `/checkpoints` prints the state and the last failure. App: `/undo` shows `Undo turn N ("…")? K files`, each file's diff, the changed-since and not-restored lists, Yes/No only; Yes restores and prints `↶ Undid turn N: …`; No changes nothing; nothing to undo prints why; during a turn it's refused.
- [ ] **Step 2: Fail. Step 3: Implement** (a `once: true` `ApprovalRequest` whose preview is the plan; the App awaits it outside a turn, as `/mcp add` does). **Step 4: Pass; typecheck.**

### Task 7: Docs

- [ ] `CLAUDE.md`: a **Checkpoints** bullet under *Safety and checks after edits* (storage, the trigger, the pair, `/undo`, the note, pruning, known limits: ignored files, edits made during a turn by the user, clean filters); mention `ctx.checkpoint` in **Tools** and the note in **Prompt cache**.
- [ ] `README.md`: `/undo` and what it can't undo. `CHANGELOG.md` entry. `/help` text.

### Task 8: Try it for real

- [ ] In a real project: a yolo turn that edits, creates and deletes files plus a bash `sed -i`; `/undo`; check `git status`, `git stash list` and the index are as before the turn; `git log --all --oneline | head` to see what the hidden ref looks like (open question 1).
