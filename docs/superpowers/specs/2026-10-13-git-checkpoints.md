# Git checkpoints and `/undo`

Issue: #16 (spec first, sprint 4, #32). Related: worktree subagents (`src/worktree.ts`: Marv already writes commits through a git dir of its own), Diagnostics' baseline (`beforeChange`: a per-turn hook started before the first change).

## Problem

A worktree subagent's work is a branch: throw it away and nothing happened. The main agent (and its shared-folder subagents) edit the user's files directly, and in yolo mode most of those edits run without a prompt. When a turn goes wrong, the only way back is by hand: `git checkout -- file` works only for files committed before the turn, loses the user's own uncommitted work in those files, and doesn't remove the files the turn created. A turn should be undoable as a unit: "put every file this turn touched back the way it was before it".

## Decisions

- **One checkpoint pair per turn that changes something**: a *before* snapshot taken just before the turn's first change, an *after* snapshot when the turn ends. `/undo` restores the paths that differ between the two to their *before* state, and only those.
- **Snapshots are git trees and commits written through a temporary index (`GIT_INDEX_FILE`)**, never the user's index, `HEAD`, branch or stash. In a git repository they go into the repository's object store under a hidden ref, `refs/marv/checkpoints/<session id>`. In a folder that isn't a repository, they go into a git dir of Marv's own, `~/.marv/checkpoints/<projectKey>/`.
- **The trigger is any tool that can change files, bash included**, not only `edit_file`/`write_file`: `runTool` calls a new `ctx.checkpoint()` after the approval and before running any call whose tool isn't `kind: "read"`. Shared-folder subagents get it too (they edit the same folder in the same turn).
- **What git ignores isn't covered.** A snapshot holds what `git add -A` would: tracked files plus untracked, non-ignored ones. `/undo` lists paths the turn changed that the snapshot can't restore (`.env`, `node_modules/…`) as "not restored".
- **`/undo` shows the change before asking**, like an approval: the files (restored, deleted, recreated), their diff in `DiffView`, and a Yes/No (`once: true`). Nothing is written before the yes.
- **The conversation isn't rewritten.** The model learns about the undo from a `Marv:` note at the start of the next user message (as deferred MCP tools are announced), so the prompt cache holds.
- **All git calls go through `runGit`'s rules** (`NO_PROGRAMS`: no hooks, no fsmonitor; inherited `GIT_*` dropped; stdin ignored; a timeout), made async so a snapshot of a big tree doesn't freeze the UI.
- **A checkpoint that fails never blocks the turn.** The turn goes on; the transcript says once that this turn can't be undone, and why.

## Storage

In a git repository (the project is the repository or a subfolder of one; a git worktree counts):

```
GIT_DIR=<the repository's git dir>  GIT_INDEX_FILE=<temp copy of its index>  GIT_WORK_TREE=<top>
git add -A -- <prefix>            # the project's files only (repo/pkg → pkg/)
git write-tree                    # → tree
git commit-tree <tree> -p <previous checkpoint> -m "marv checkpoint: turn 3 before"
git update-ref refs/marv/checkpoints/<session> <commit> <previous>
```

- **The temporary index starts as a copy of the user's index**, for the same two reasons as `commitGitDir`: its stat cache makes `add -A` rehash only what changed (fast on a big repository), and it is what says a force-added or skip-worktree file is tracked (an empty index would leave those out of the snapshot, so `/undo` would treat them as new and delete them). Without a readable index (a new repository), it starts empty. The copy is made with the user's index untouched; the temp file is deleted afterwards.
- **One ref per session**, pointing at the newest checkpoint; each checkpoint's parent is the previous one, so the ref keeps the whole chain reachable (`git gc` keeps it) and one `update-ref -d` lets it all go. The commit message carries the turn number and `before`/`after`; Marv's author/committer identity is fixed (`Marv <marv@localhost>`), so a repository without `user.email` works. `refs/marv/` isn't under `refs/heads`, `refs/tags` or `refs/remotes`, so no branch list, `git push`, `git fetch` or `git status` shows it; `git log --all` and `git push --mirror` do (open question 1).
- **Outside a repository**, the same commands run against `~/.marv/checkpoints/<projectKey>/git` (created with `git init --bare`, Marv's own, so `git gc` there is Marv's to run) with the project as the work tree and its own persistent index (the stat cache again). There is no `.gitignore` to apply, so its `info/exclude` lists the file listing's skip list (`node_modules`, `.git`, `dist`, …), and a snapshot above 20,000 files or 200 MB of new content stops: checkpoints are off for that project this session, said once (`/undo` says why).
- **Never committed through a ref the user can check out**, never `git stash create` (it reads and refreshes the real index), never `git checkout`/`reset` on the user's index.

## When a checkpoint is taken

- **Before**: `ctx.checkpoint()` is memoized per turn (one promise, like `beforeChange`), started by `runTool` for any non-read call after it was approved and before it runs, and awaited there. Parallel subagents share the promise. A read-only `bash` command (`ls`) also takes one; that's one `add -A` with a warm stat cache, and the pair is dropped at the end if nothing changed. Worktree subagents don't call it (their changes land on their branch); MCP tools don't either (they act outside the project).
- **After**: in `runTurn`'s cleanup, however the turn ended (answer, error, Esc, step limit), once all tool calls are answered. If the after tree equals the before tree, both commits are dropped (the ref is moved back), so `/undo` only ever sees turns that changed something.
- **Bash and hand edits**: both are covered because the snapshot is the whole tree, not a list of files the tools reported. What the user edited by hand between turns lands in the next turn's *before* snapshot, so undoing that turn keeps it. What the user edits while a turn runs ends up in that turn's change set (known limit, said in the preview only if it can tell; see below).

## `/undo`

`/undo` (refused during a turn, like `/resume`) takes the newest turn in this session that has a checkpoint pair and hasn't been undone:

1. **Plan** (`undoPlan()`): `git diff-tree -r --no-renames <before> <after> -- <prefix>` gives the paths and their kinds: modified, created (→ delete it), deleted (→ recreate it). For each, compare the file as it is now with the *after* snapshot (`git hash-object` of the current file): if it changed since the turn ended, it is marked **changed since** in the preview. Paths the turn's tools reported (`ToolResult.changed`) that are in neither tree are listed as **not restored (ignored by git)**.
2. **Preview**: an approval-style prompt, `Undo turn 3 ("add the parser")? 4 files`, each file with its `DiffView` diff (current → before; binary and over-cap files named only), the changed-since and not-restored lists, Yes/No.
3. **Apply**: first a *pre-undo* snapshot (same chain, message `before undo of turn 3`), so an undo is itself recoverable (`git checkout <sha> -- file` for now; open question 3). Then Marv writes each path itself, not through `git checkout` (which would run checkout filters and follow symlinks): the blob from `git cat-file blob`, its mode (`100755` → executable, `120000` → a symlink with that target), parent folders created, a created file deleted and its folders removed if they became empty. Every path goes through `resolveInProject`-style checks: one that now leads outside the project through a symlink is refused and reported, not written.
4. **Afterwards**: the turn is marked undone (an `undo` commit on the chain naming it), so the next `/undo` goes one turn further back. The transcript gets `↶ Undid turn 3: 4 files restored` (plus anything skipped). The next message the user sends starts with a note for the model: `Marv: the user undid your changes from an earlier turn ("add the parser"): src/a.ts and src/b.ts are back as they were before it, src/new.ts was removed. Re-read files before editing them.` (built like the MCP announcement: part of the user's message, never its own message, never in the transcript).

`/undo` on a turn whose files are all unchanged since it ended and with nothing skipped needs no special case; the preview just has no warnings. A changed-since file is restored like the others (the user saw it marked; the pre-undo snapshot keeps their edit).

## Sessions, resume and cleanup

- The chain is per session id, so `/clear` starts a new ref and `/resume` (or `marv -c`) finds the old one again: `/undo` after a resume works on that session's turns if its ref still exists. Nothing is added to the session file; the chain is the record.
- **Pruning** at session start (async, after the first frame): keep the refs of the 20 most recently updated sessions of this project and anything updated in the last 14 days; `update-ref -d` the rest. In the user's repository Marv never runs `git gc`; the objects go with the user's next gc (git's default prunes unreachable objects older than two weeks). In Marv's own git dir, Marv runs `git gc --prune=now --quiet` after pruning, at most once a day.
- `/undo` with no checkpoints says why: none yet this session, checkpoints off (config), not a git repository and the project too large, or the last snapshot failed (with git's message). Checkpoints don't need the sandbox: they're Marv's own git calls, not project code.

## Settings, the SDK and trajectories

- Config `checkpoints` (default on), `/checkpoints on|off` beside `/diagnostics`, `SessionInit.checkpoints`, `configure({ checkpoints })`, SDK `checkpoints` (default on). `MarvSession.undoPlan()` and `undo(plan)` are on the public `Session` interface, so an SDK client can offer its own undo.
- Trajectories: a `checkpoint` record per snapshot (`before`/`after`/`pre-undo`, sha, file count, ms, or the failure) and an `undo` record (turn, paths, skipped).
- Session events: `status: "checkpointing"` while a *before* snapshot that takes longer than 300 ms runs (the App shows "Saving a checkpoint…"; small trees never show it).

## Safety

- **Programs**: `add -A` can run a repository's clean filters (git-lfs, or a `filter.<x>.clean` in `.git/config`). Marv's own git calls already run in the user's repository outside the sandbox (the file listing, worktrees); a confined command can't change `.git` (read-only in the sandbox), and an approved one that plants a filter would also have it run on the user's next `git status`. So no new exposure, but the spec names it. Hooks and fsmonitor are off (`NO_PROGRAMS`); `commit-tree`/`update-ref`/`write-tree` run no hooks anyway.
- **Writes**: `/undo` only writes inside the project, path by path, after the user saw each path; the pre-undo snapshot makes it reversible.
- **Secrets**: an untracked, non-ignored `.env` is snapshotted into the user's object store (as `git stash -u` would). It never leaves the machine unless the user pushes `refs/marv/*` (`--mirror`).

## Testing (failing first)

- `tests/checkpoints.test.ts` on temp repositories (`git init` + a commit): a snapshot leaves `HEAD`, the index (byte-for-byte), branches, `refs/stash` and `git status` unchanged; it includes untracked files, excludes ignored ones, keeps force-added ones; a subfolder project snapshots only its prefix; outside a repo, Marv's own git dir with the skip list and the size cap; a hook and `core.fsmonitor` in the repo's config never run; a missing `user.email` doesn't matter.
- Undo: modified, created, deleted, mode change, symlink, a folder emptied; changed-since marking; a symlinked parent leading out of the project refused; the pre-undo snapshot exists; a second `/undo` goes one turn further back; pruning keeps the newest 20 and the recent ones.
- Session (`ScriptedProvider`): a turn with only `read_file` takes no checkpoint; a turn with `write_file` and a `bash` edit gets one pair covering both; a no-op bash turn's pair is dropped; parallel shared-folder subagents share one *before*; a failing snapshot doesn't stop the turn; the note reaches the next request's user message and every request still extends the previous one (`tests/agent.test.ts`'s cache check).
- App: `/undo` preview, Yes restores, No changes nothing, refused during a turn.

## Decided with the user (2026-10-13)

1. **The hidden ref in the user's repository** (`refs/marv/checkpoints/<session>`), as proposed.
2. **`/undo` doesn't rewind the conversation**: the conversation stays append-only and the model is told in a note, as proposed.
3. **No `/redo` in v1**; the pre-undo snapshot makes an undo recoverable by hand.
4. **`/undo` walks back one turn at a time**, as proposed.

## Open questions (as asked; answers above)

1. **Hidden ref in the user's repository, or Marv's own git dir everywhere?** The hidden ref is cheap (objects mostly exist; the index copy makes it fast) but shows up in `git log --all`, gitk, `git for-each-ref` and `git push --mirror`. A git dir under `~/.marv/checkpoints/` for every project leaves the repository untouched, at the cost of hashing the whole project once per session (a big repository: seconds and a copy of its contents). The spec picks the hidden ref; say if those listings are a problem for you.
2. **Should `/undo` also rewind the conversation?** The spec keeps it (append-only, cache intact, the model told by a note). Rewinding (dropping the undone turn from the history, as Claude Code's rewind does) would break the "conversation is only appended to" rule and restart the cache.
3. **`/redo`, or only the pre-undo snapshot?** v1 has no `/redo`; an undo is recoverable by hand from the chain. Add `/redo` (one level) if you want it in 0.4.0.
4. **Undo only the last turn, or pick one (`/undo 3` / a picker)?** v1 walks back one turn per `/undo`; undoing an older turn alone could conflict with later turns' edits of the same files.
