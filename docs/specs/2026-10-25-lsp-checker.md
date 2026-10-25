# Checkers for Rust, Python and Go: new errors after edits

- Issue: FlologixAI/marv #187 (split out of #69)
- Status: **approved 2026-10-25** (the maintainer: "approve both specs"), with every decision in §9 as recommended. Revised after the first review on #236.
- Date: 2026-10-25, against `main` after #216.
- New in marv-rs: TypeScript Marv has only the tsc checker (`src/diagnostics/`). The new kinds' module docs say they port nothing; the shared machinery keeps `turn.ts`'s shape.
- Scope:
  - `crates/marv-session`: `diagnostics/` (the machinery, new check kinds, the check cache), `session.rs` (wiring), `config.rs` (the setting), `commands.rs` (`/diagnostics`);
  - `crates/marv-sandbox`: a Seatbelt test for the cache mount; possibly a `TOOLCHAINS` entry for uv's Pythons (§6.5, its own PR);
  - `crates/marv-cli` (TUI): showing the new results;
  - phase 2 only: a long-lived sandboxed stdio child and a small LSP client.

## 1. Summary

The issue asks for an LSP-backed checker (rust-analyzer, pyright/basedpyright, gopls). This spec recommends getting there in two phases:

- **Phase 1: one-shot command-line checkers** on the machinery the typecheck already uses: `cargo check --message-format=json-diagnostic-short`, `basedpyright`/`pyright --outputjson`, and `go vet`. Each is a new `CheckKind`. It needs no new process lifecycle, no protocol client and no new dependency. It checks the whole project, so it catches the error in another file that the edit broke. That's the case that matters most, and the one an LSP client watching open files tends to miss.
- **Phase 2: long-lived language servers**, only if phase 1's numbers show it's needed. That would be checks too slow even when warm (big Python projects, Go monorepos). Phase 2 is a spike with its own decision. rust-analyzer comes last, or never: its type errors come from running `cargo check` itself ("flycheck"), so a server would add memory but not much signal.

My estimate is that phase 1 gets about 80% of the value for about 10–20% of the work of an LSP client in the sandbox. The cost it adds is disk (a Rust check cache), and §6.3 bounds that before Rust can be on by default.

## 2. How the typecheck works today (evidence from the code)

`crates/marv-session/src/diagnostics/`:

- **`turn.rs`** is generic over the problem type `E`. A `CheckKind<E>` says:
  - `prepare(root)`: can anything run here?
  - `pass(root, changed)`: the command for the baseline (`None`) or for a step's changed files (`Some(files)`), plus `covers`, which says which baseline problems that pass re-checks;
  - `parse(output, exit_code)`: `None` means "can't tell", and nothing is said;
  - `missing`: an interpreter the sandbox hides;
  - `key`: what makes two problems the same;
  - `note`: the text for the model.

  The `Checker<E>` lasts the whole session. It counts timeouts and remembers the notices it has given. `Checker::turn` makes each turn's `TurnChecks`.
- **The baseline:** `BaselineHook::before_change` runs once per turn, before the first change. The edit tools start it from their preview and wait for it before writing, so yolo turns get it too. Later calls wait for the same run. **Today project code runs only from here on:** a session that never edits never runs the project's compiler.
- **After a step:** `Turn::after_step` runs only when a step's results carry a diff (`has_diff`). That means `edit_file`/`write_file` and the like. A `sed -i` through `bash` doesn't trigger a check, and neither does `cargo fmt`. The step:
  1. runs the pass;
  2. computes `new_problems(checked_baseline, now, key)`, counted, so a second copy of an old problem is new;
  3. makes the result the next baseline, so each problem is told once;
  4. returns `kind.note(added)`.
- **How results reach the model:** the returned note goes back to the agent loop's `after_step`, which appends it as a user message from Marv. `tsc::diagnostics_note` and `provenance::project_note` fence the errors as project data under Marv's own sentence. That was a security fix: compiler messages quote project text. `combine_checks` runs several kinds side by side and joins their notes into one message. The transcript gets `check_notice` (5 lines). The trajectory gets `check_record`. Clients get `SessionEvent::Check`/`Checks`.
- **Limits:**
  - `CHECK_TIMEOUT` is 60 s per check;
  - `MAX_TIMEOUTS` = 2 turns a kind off for the session;
  - `MAX_REPORTED` = 20 errors go to the model, the rest are counted;
  - output at `MAX_CAPTURE` (2 MB) counts as unreadable, because a cut list would make problems look fixed;
  - a parse panic counts as unreadable, and an unreadable run makes checking unavailable for the rest of that turn only.

  Only the main agent gets checks.
- **The sandbox:** `sandboxed_options` builds `RunOptions` for `marv_tools::bash::run_command`:
  - `sandbox: true`, `network: false`;
  - `project_read_only: true`;
  - the worktree's `.git` read-only;
  - `writable: []`, `pipe_check: false`.

  The sandbox's environment is built from scratch (PATH, HOME, LANG, TERM, TMPDIR), so a user's `RUSTC_WRAPPER`, `GOFLAGS` or `CARGO_TARGET_DIR` doesn't leak in. Without a working sandbox nothing runs, and the first change of the session says so once (`Off { NoSandbox }`). docs/SECURITY.md: "The project's own tooling runs only inside the sandbox."
- **Detection:** `tsc::detect_checker` uses only the project's own `node_modules/.bin/tsgo|tsc`, and only when `tsconfig.json` exists: "A global one isn't used". The formatter (#67, `marv_tools::formatter`) is the precedent for global toolchains. It runs `rustfmt` when `Cargo.toml` exists and `gofmt` when `go.mod` does, found through the sandbox's PATH, and it skips quietly when the tool isn't installed.
- **Plugging in a kind:**
  - `ProjectChecksKind` (`checks/kind.rs`) is the second kind and shows the pattern: implement `CheckKind`, add a `Checker` field in `session.rs` (`typecheck`, `project_checks`), add one block in `turn_checks`, and an event and notice.
  - The problem type is per kind. `TsError` is generic in all but name: file, line, column, code, message.
- **Config:** `diagnostics` (bool, default `true`) in `~/.marv/config.json`, with `/diagnostics on|off`. **There is no project-level Marv config file.** `config.rs` reads only `~/.marv` or `MARV_CONFIG_DIR` from the real environment.
- **Oracle:** `crates/marv-oracle/src/runner.rs` writes `"diagnostics": false` into every scenario's config. TypeScript Marv has no checkers for these languages.
- **macOS:** Seatbelt hides `~/Library` and the home's dot-folders, then shows the read-only and writable extras again ("a read-only or writable extra under it is shown again in section 4", `seatbelt.rs`). So a cache under `~/Library/Caches` can be mounted writable, which §6.3 relies on and PR 1 tests.

### Probe (this machine, bwrap, `--ro-bind / /`, project read-only, `--unshare-net`)

`cargo check --offline --message-format=json --target-dir <writable cache>` on a small crate with a registry dependency (`regex`):

- **Read-only `~/.cargo`, offline:** it works. The package-cache lock didn't stop it.
- **Cold:** 5.6 s, building 5 dependencies.
- **Warm, after an edit:** 0.28 s.
- **Output:** `compiler-message` lines whose `message` has:
  - `level: "error"`;
  - `code.code: "E0308"`;
  - `message: "mismatched types"`;
  - `spans[]`, each with `file_name`, `line_start`, `column_start`, `is_primary` and `label` (`"expected \`u32\`, found \`&str\`"`).

  A final `build-finished` line gives `success`. The project's `target/` was never touched.

## 3. Goals and non-goals

**Goals:**

- After an edit in a Rust, Python or Go project, the model hears about compile/type errors that weren't in the previous check, with the same guarantees as the typecheck:
  - sandboxed;
  - the project read-only;
  - no network;
  - fenced as project data;
  - told once;
  - silent when unsure.
- Use only toolchains already installed (Marv never installs one), found the way the sandbox's `bash` would find them.
- **Nothing project-steered runs on the host**, not even to detect a tool or find a path (§6.4).
- Bounded cost on the user's machine: CPU, I/O, disk, and the latency added to an edit.
- No change to the system prompt or tool specs, so no prompt-cache break.

**Non-goals:**

- Warnings and lints: clippy, `ruff check`, unused variables. That's errors only, as with tsc; warnings could come later (§8).
- Hover, go-to-definition, symbol search or any other LSP feature exposed as a tool. That's a separate issue if wanted.
- Subagents: they don't get checks today either.
- Checks after `bash` changes: that's an existing limit of `has_diff`, unchanged here.
- A user-defined checker command: that's what `.marv/checks/` is for.
- A memory cap. There's no cgroup in the sandbox. §6.2 documents this.

## 4. Option A: one-shot command-line checkers (phase 1)

| | Rust | Python | Go |
|---|---|---|---|
| Applies when | `Cargo.toml` at the root | `pyrightconfig.json`, or `pyproject.toml` / `setup.py` / `setup.cfg` / `requirements.txt` at the root | `go.mod` at the root |
| Binary | `cargo` (rustup shim on PATH) | the project's `.venv/bin/basedpyright` or `node_modules/.bin/pyright`, then `basedpyright`, then `pyright` on PATH | `go` on PATH |
| Command | `cargo check --workspace --all-targets --offline --message-format=json-diagnostic-short --target-dir <cache>/target 2>/dev/null` | `<bin> --outputjson` | `go vet ./...` |
| Env (set in the command) | `CARGO_TERM_COLOR=never RUSTUP_AUTO_INSTALL=0 CARGO_BUILD_JOBS=<half the cores>` | none | `GOFLAGS=-mod=readonly GOPROXY=off GOTOOLCHAIN=local GOCACHE=<cache>/go-build`, plus `GOWORK=off` unless `go.work` is at the root; `GOMODCACHE` mounted read-only (§6.4) |
| Writable cache | yes, the target dir (GBs) | none needed | the build cache (hundreds of MB) |
| Warm check | ~0.3 s small crate; seconds on a workspace | seconds; ~30 s+ on very large projects | seconds |
| Cold check | minutes on a big workspace (all dependencies) | the same as warm | tens of seconds (std + deps) |
| Output | JSON lines; `compiler-message` with `level: "error"` | JSON; `generalDiagnostics[]` with `file`, `severity`, `message`, `rule`, `range` | text: `path:line:col: message` (optional `vet: ` prefix), tab-indented continuation lines, `# pkg` headers |
| Key | file (primary span) + code + message + primary label | file + rule + message | file + message |

### Notes per language

**Rust:**

- `--all-targets`, so errors in tests and `#[cfg(test)]` modules count. The model edits tests too. It costs about twice a lib-only check when cold, much less when warm.
- **`json-diagnostic-short`** keeps `rendered` to one line per message, which keeps a warning-heavy run well under `MAX_CAPTURE`.
- **Precedence:** `--target-dir` beats the project's `.cargo/config.toml` `build.target-dir` and `CARGO_TARGET_DIR`.
- **What the project steers, inside the sandbox:**
  - `.cargo/config.toml` (the project's and its parents') sets `build.rustc`, `build.rustc-wrapper`, `target.*.linker` (used to link build scripts), `[env]` and `rustflags`;
  - `rust-toolchain.toml` picks any installed toolchain, and its `path =` key can point at a toolchain directory inside the project;
  - `build.rs` and proc macros run.

  All of that runs sandboxed, as it would for the model's own `cargo check` in `bash` (§6.4).
- **A toolchain that isn't installed:** `rust-toolchain.toml` naming one fails fast under `RUSTUP_AUTO_INSTALL=0` (and could not install anyway: `~/.rustup` is read-only and there's no network). That's unreadable, with one notice.
- **Parse rules:**
  1. Only `compiler-message` lines with `level: "error"` count.
  2. **Errors with no primary span are dropped.** rustc's summaries ("aborting due to 2 previous errors") carry a count that changes every step and would read as new.
  3. **Duplicates within one run are folded** by (file, line, column, code, message). `--all-targets` compiles `src/lib.rs` for both the lib and the lib-test target, so each lib error arrives twice or more.
  4. **No `build-finished` line → `None`.** Cargo failing before any build (a lockfile needing an update under `--offline`, a missing registry dependency, a bad manifest) prints nothing on stdout, since stderr is dropped.
  5. `build-finished` with `success: false` but no error parsed → `None`: a failed build script, a missing toolchain.
  6. A line that isn't JSON → `None`.
  - The message includes the primary span's label. The key does too, because rustc's bare messages ("mismatched types") are too generic to tell two errors in one file apart.

**Python:**

- **basedpyright first:** its PyPI wheel bundles node, so it works offline. The PyPI `pyright` wrapper downloads node and the npm package on first run, which fails without network (unreadable, one notice). npm's `pyright` needs `node`, which takes the same `missing` path as tsc (`env: 'node': No such file or directory`, exit 127).
- **What the project steers, inside the sandbox:** pyright *executes* a Python interpreter to find import paths. `pyrightconfig.json`/`[tool.pyright]` `venvPath`/`venv` choose which one, and the venv's `.pth` files run at its startup.
- **Errors only:** `severity: "error"`.
- **Paths:** `file` is absolute, so it's shown relative to the root, and a file outside the root is dropped.
- **Imports:** the project's `.venv` is read inside the project, so imports resolve when it exists. When it doesn't, the `reportMissingImports` errors are in the baseline and never told.
- **uv's venvs:** `.venv/bin/python` is a symlink into `~/.local/share/uv/python`, which the sandbox hides. Handled in §6.5.
- A project with neither pyright config nor `[tool.pyright]`/`[tool.basedpyright]` runs in pyright's default "standard" mode. The baseline absorbs the noise; the cost is time, and output size, which §6.6 bounds.

**Go:**

- **Why `go vet` over `go build ./...`:** it type-checks test files too, and its analyzer findings (printf mismatches, copied locks) are real bugs.
- **Offline:** `GOTOOLCHAIN=local` and `GOPROXY=off` fail fast instead of trying the network. `-mod=readonly` keeps `go.mod` untouched, which the read-only project enforces anyway.
- **A newer `go` line:** under `GOTOOLCHAIN=local`, a module whose `go` (or `toolchain`) line is newer than the installed go fails ("requires go >= 1.NN (running go 1.MM; GOTOOLCHAIN=local)"). Users who rely on auto-downloaded toolchains hit this every time. The kind recognises the message, turns itself off for the session, and says so once: "✻ This module needs go 1.NN; the installed go is 1.MM, and Marv doesn't download toolchains." `/diagnostics` shows it too.
- **Workspaces:** `GOWORK=off` unless `go.work` is at the root. A `go.work` in a parent directory outside the project would otherwise apply.
- **What the project steers, inside the sandbox:** cgo packages run the C toolchain (`CC`, `#cgo` flags).
- **The build cache:** the check's own `GOCACHE` in the check cache, **never the user's real `GOCACHE`**. Go trusts cache entries across projects, so a shared one would let one project's sandboxed check poison another project's unsandboxed builds.
- **Parse rules:**
  - `# pkg` headers are skipped;
  - a tab-indented line right after an error continues it, the way tsc's continuation lines do;
  - any other line that isn't `path:line:col: message` (optional `vet: ` prefix) makes the run unreadable.
  - `go vet -json` gives analyzer findings as JSON, but type errors still come as text, so the text parser is needed either way.

### Why this fits

- It is the typecheck, three more times. `CheckKind`, baseline, `new_problems`, notes, notices, timeouts, Esc handling and the sandbox are all reused.
- **Whole project:** an edit to a function signature reports the callers it broke in other files.
- **Memory:** peak memory only while a check runs; nothing stays resident.
- **macOS:** `run_command` already runs under Seatbelt (#164). The cache mount needs a Seatbelt test (§2, §6.3), nothing new to port.

### What it costs

- **Cold start:** Rust's cold check on a real workspace takes minutes, which the 60 s timeout would cut off twice and turn off. That needs warm-up and "pending" semantics (§6.2).
- **Disk:** a second Rust target dir, per repository. It must not be the project's `target/`, and it must be bounded (§6.3).
- **Latency on every edit step:** a warm whole-workspace `cargo check` takes seconds on a large workspace.

## 5. Option B: an LSP client in Marv, servers in the sandbox (phase 2, if ever)

**What it would take:**

1. **A long-lived, sandboxed child with stdio pipes:**
   - The process door (`processes/`, #51) doesn't fit: it runs with the project *writable*, a network namespace and a port relay, and it keeps logs, not a protocol pipe. It isn't offered on macOS yet (#152).
   - It needs a new primitive: `marv_sandbox::Prepared` for a read-only project, spawned with piped stdin/stdout, `--die-with-parent` (the async-task rule in `processes/mod.rs` applies), `marv __guard` on macOS, killed on session end, `/diagnostics off` and Marv's exit path.
2. **A JSON-RPC client:** `Content-Length` framing, request ids, `initialize`/`initialized`, `shutdown`/`exit`, and answering the server's requests (`workspace/configuration`, `client/registerCapability`, `window/workDoneProgress/create`). That's about 400–600 lines on tokio.
3. **Telling the server about edits:**
   - `didOpen`/`didChange` for files Marv's tools wrote, or `workspace/didChangeWatchedFiles`;
   - rust-analyzer and pyright prefer client-side watching, and the server's inotify through a bind mount works but is per-server configuration;
   - files changed by `bash` would go unseen unless Marv watches the tree.
4. **Knowing when diagnostics are settled.** `publishDiagnostics` is push-only, with no "done" signal. The options:
   - pull diagnostics (`textDocument/diagnostic`, `workspace/diagnostic`, LSP 3.17), which some servers support for some diagnostics;
   - rust-analyzer's `experimental/serverStatus` (quiescent), whose flycheck diagnostics are still pushed;
   - progress tokens;
   - a quiet-period heuristic.

   Each server needs its own rule, and a wrong one tells the model a half-finished picture. That breaks "silent when unsure".
5. **Whole-project diagnostics:**
   - pyright reports only open files by default (`diagnosticMode: "workspace"` changes that, at a cost);
   - gopls reports workspace packages;
   - rust-analyzer's native diagnostics are per file, and flycheck is `cargo check` anyway.

   Getting "your edit broke a caller elsewhere" reliably means workspace mode, which gives up much of the speed advantage.
6. **Resources on the user's machine, held for the whole session:**
   - rust-analyzer: 1–4 GB on a large workspace, and minutes of indexing;
   - gopls: hundreds of MB;
   - pyright: 200 MB–1 GB.

   Often that's on top of the same servers in the user's editor. With several Marv sessions (mesh, subagents) it multiplies.

**Crates** (if phase 2 happens):

- `lsp-types` (types only, MIT) plus our own framing is the minimal option. Its releases have broken APIs before (the `Url` to `Uri` change), so pinning matters. Plain `serde_json::Value` for the dozen messages used avoids even that.
- `async-lsp` has a client side, but it's built on tower, with more dependencies.
- `tower-lsp` and `tower-lsp-server` are server-only.
- rust-analyzer's `lsp-server` is synchronous (crossbeam threads).

  **Recommendation for phase 2:** `serde_json` with our own framing, and `lsp-types` only if the message set grows.

**The one-shot middle ground**, mentioned for completeness:

- `rust-analyzer diagnostics <path>` runs a full analysis per run (slow, and native diagnostics only).
- `gopls check <files>` loads the workspace each run, for named files only.

Neither beats `cargo check` or `go vet` for this use.

**Where LSP would win:** checks that are slow even when warm. A large Python project, where pyright's incremental server answers in under a second and the CLI takes 30 s or more. A Go monorepo. Phase 1's trajectories record each check's duration (`check_record`'s `ms`), so this can be decided on data (§7).

## 6. Design (phase 1)

### 6.1 Machinery changes (`diagnostics/turn.rs`)

1. **One problem type:** rename `TsError` to `Diagnostic { file, line, column, code, message }`, a struct that already fits all four tools. tsc's parser keeps returning it. `CheckResult` stays `CheckOutcome<Diagnostic>`. Each kind's `Checker` gets a `label` ("tsc", "cargo check", "pyright", "go vet") for notices, the note's intro and `check_record`'s `source`. The event becomes `SessionEvent::Check { label, result }`, and the TUI shows "✻ cargo check: 2 new errors, told the model:".
2. **Per-kind policy on `CheckKind`**, with defaults that keep tsc's behaviour:
   - `timeout()` (60 s);
   - `warm_up()` (none; §6.2);
   - `cache()`: whether it needs a writable cache folder (§6.3).
3. **Environment:** set in the command string (`VAR=… cmd`), which works with the sandbox's from-scratch environment. **No `RunOptions` change for env.** The only `RunOptions` field the new kinds use is the existing `writable`, for the cache key dir.
4. **Stdout and stderr:** `run_command` captures them together. cargo's progress text on stderr would break the JSON-lines parser, so the command drops stderr (`2>/dev/null`) and the parser stays strict. That's why "no `build-finished`" must mean unreadable (§4).
5. **Unreadable runs turn a kind off.** A kind that's unreadable `MAX_UNREADABLE` (2) times in a row is off for the session, like `MAX_TIMEOUTS`, with one line in `/diagnostics`. Otherwise an output over `MAX_CAPTURE` (thousands of baseline pyright errors in an unconfigured project) would rerun a 30 s check on every edit step and never say anything. A readable run resets the count.
6. **Priority:**
   - CPU: commands run under `nice -n 10`;
   - I/O, on Linux: `ionice -c3` when the sandbox has it (`command -v ionice >/dev/null && ionice -c3 …`, resolved inside the sandbox);
   - macOS: `nice` only;
   - parallelism: `CARGO_BUILD_JOBS` is half the logical cores, at least 1, because `combine_checks` can run cargo, pyright and tsc side by side in a polyglot repo.

### 6.2 Cold start, locks and "pending"

The baseline is taken before the turn's first write, and the write waits for it. A cold Rust check would hold up the first edit for 60 s, then time out and count toward turning Rust off. So:

- **Warm-up, started at the session's first edit.** A kind with `warm_up()` (Rust; Go when its cache is empty) starts one background run of its baseline command:
  - **When:** at the first `before_change` of the session, which the edit tools call from their preview. Not at the first turn. A read-only session (questions, review) never runs the project's `build.rs` or proc macros, never spends minutes of CPU and never writes a GB cache. That keeps today's rule: project code runs only once the model goes to edit.
  - **Lifetime:** session-scoped. It's cancelled by session end and `/diagnostics off`, not by the turn's Esc.
  - **Timeout:** `WARM_UP_TIMEOUT` (10 min), not counted in `MAX_TIMEOUTS`.
  - **Output:** discarded. Its only job is the cache.
- **Pending.** If the turn's baseline is due while the warm-up is still running, `before_change` waits up to `BASELINE_WAIT` (10 s). If it's still not done:
  - the baseline is `Taken(None)`, so this turn says nothing;
  - the outcome is `Failed { reason: Pending }`, with a notice once per session: "✻ cargo check is still building its cache for this project (first run); Marv will tell the model about new errors once it's ready."

  The trade-off is that the first editing turn in a cold Rust project usually gets no Rust checks. Later turns run on the warm cache.
- **Marv's own lock, so lock waits never count as timeouts.**
  - Worktrees of one repository share a cache (§6.3), and several sessions can share a project. Each check and warm-up takes a host-side `flock` on `<cache root>/<key>.lock`, which sits outside the mounted folder, before it starts.
  - The 60 s timeout starts only once the lock is held. Waiting for the lock is bounded separately: up to `BASELINE_WAIT` for a baseline, and a step's check skips (silent, `Pending`) when another session holds the lock.
  - So cargo's own build-dir lock is never contended between Marv sessions, and a busy neighbour can't turn Rust off.
- **Timeouts after warm-up still count.** A warm check that can't finish in 60 s is too slow for every step, and two of them turn the kind off, as today.
- **Memory is not capped.** There's no cgroup, and a cold `cargo check --all-targets` on a big workspace can take several GB at its peak. docs/SECURITY.md and the README say so. `check_record` records `ms` per run (already there) and the cache size after the run, for the §7 report.

### 6.3 Caches: location, bounds and hygiene

- **Where:** `<cache root>/<key>/`.
  - The cache root is `$XDG_CACHE_HOME/marv/checks` (default `~/.cache/marv/checks`) on Linux, and `~/Library/Caches/marv/checks` on macOS.
  - Inside a key dir: `target/` (Rust) and `go-build/` (Go).
- **The key is per repository.** For a worktree, the key is the repository's common git dir, read from the `.git` file and `commondir` as `git_guard::worktree_git_dirs` already does. Without spawning git, and only when that check passes. Otherwise it's `paths::project_key(root)`.
  - Worktrees of one repo (Marv's own workflow has many) share the dependencies' build, which is the bulk of a target dir. Workspace crates still rebuild per worktree path.
  - **Accepted:** a check in one worktree can leave poisoned artifacts for another worktree's checks. That's the same repository and trust domain, and both run sandboxed only.
- **Mounted writable for that check only,** through `RunOptions.writable`, the way an approved `git_write` command gets its worktree records. It is **not** added to the user's `writable` list (#98).
  - The mount is **exactly `<root>/<key>`**, never `<root>`. A check can only write its own key dir, so one project can't poison another project's cache. The lock and metadata files live in `<root>`, outside the mount.
  - The root and each key dir are created 0700 by Marv. The mount is refused (the check doesn't run, one notice) if the key dir is a symlink, isn't a directory or isn't owned by the user.
  - **The #98 rule against hidden home folders** would refuse `~/.cache`. That rule exists because the host might *execute* something from there (`.local/bin`, autostart, hooks). Nothing on the host executes or reads anything inside a key dir. Marv only creates key dirs, sizes them and deletes them, without following links.
- **Why not the project's `target/`:**
  - **Security:** checks run without approval with the project read-only on purpose. A writable `target/` would let a sandboxed `build.rs` plant artifacts that the user's own *unsandboxed* `cargo build`/`cargo test` later links and runs.
  - **Contention:** different flags and env would make the user's builds and Marv's checks invalidate each other's fingerprints. They would also block on cargo's build-dir lock while the user's editor's rust-analyzer holds it.
- **Disk bounds**, all needed before Rust can be on by default (decision 4):
  1. **Total cap with LRU:** `MAX_CHECK_CACHE` (20 GB) across all key dirs. When it's exceeded, the least recently used key dirs are deleted until it isn't, never one whose lock is held. Each run touches `<root>/<key>.json` with `last_used` and the project roots that used it.
  2. **Per-key cap:** a key dir over `MAX_KEY_CACHE` (10 GB) after a run turns that kind off for the session, says so once, and deletes the dir. That also covers a malicious `build.rs` filling its writable cache on purpose.
  3. **A free-space guard:** no warm-up or check starts when the cache filesystem has less than 5 GB or 5% free (`statvfs`). The kind is pending, with one notice: "✻ Less than 5 GB free; Marv isn't running cargo check." A full disk stalls everything else on the machine, btrfs especially.
  4. **Age and orphans:** at startup, key dirs unused for 14 days are deleted, and so are those whose recorded project roots all no longer exist (deleted worktrees).
  5. `/diagnostics` shows each checker's state and the size of this project's key dir. `/diagnostics clear` deletes this project's key dir.
- **Hygiene: no step follows a symlink.** A `build.rs` can plant `target/x -> /`.
  - Sizing walks with `symlink_metadata`, counts a link as its own size, and never descends into one.
  - Deleting (LRU, age, orphans, the per-key cap, `/diagnostics clear`) first checks that the key dir is a real directory directly under the cache root, then removes it with a walk that unlinks links instead of following them. (`std::fs::remove_dir_all` doesn't follow symlinks on Linux or macOS; a test pins that.)
  - Sizing a big target dir (hundreds of thousands of files) runs on a blocking task after the check, not on the step's path, and at most once per key per minute.

### 6.4 Nothing project-steered runs on the host

**The guarantee.** Project files *do* choose what runs inside the sandbox, for all three tools:

- `.cargo/config.toml`: `rustc`, `rustc-wrapper`, the linker, `[env]`, `rustflags`;
- `rust-toolchain.toml`, including `path =`;
- pyright's `venvPath`/`venv`, and the venv's `.pth` files;
- cgo, for `go vet`.

That is the same as the model's own `cargo check` in `bash`. The rule is: **everything the project steers runs only inside the sandbox, with the project read-only and no network. Nothing Marv runs on the host executes project tooling or is steered by project files.** Marv's own settings choose only which languages are on.

**No toolchain is spawned on the host.**

- Detection, cache sizing, pruning and clearing only `stat` and read files.
- `prepare` checks markers (`Cargo.toml`, `go.mod`, …).
- The repo key reads `.git`/`commondir` without running git.
- Whether a tool is installed is learned from its first sandboxed run.
- **`GOMODCACHE` is computed, never asked of `go`.** It comes from Marv's own environment (`GOMODCACHE`, else the first `GOPATH` entry + `/pkg/mod`, else `~/go/pkg/mod`), then the same keys in go's env file (`$XDG_CONFIG_HOME/go/env` or `~/Library/Application Support/go/env`), read as text.
  - Never `go env` on the host. Run with the project as cwd and the default `GOTOOLCHAIN=auto`, `go.mod`'s `toolchain` line would download and execute a toolchain outside the sandbox, with network.
  - If a later change ever needs `go env`, it must run with a cwd outside any project and `GOTOOLCHAIN=local`.
- **Resolution in the sandbox:** the command names fixed programs (`cargo`, `go`, `basedpyright`, `pyright`), resolved inside the sandbox with the sandbox's PATH. A project-local `node_modules/.bin/pyright` or `.venv/bin/basedpyright` is the project's own code, as `node_modules/.bin/tsc` is: allowed, because it runs sandboxed.
- **A test pins this** (PR 1): a PATH of fake `cargo`, `go`, `rustup`, `pyright`, `basedpyright` and `git` that write a marker when run. Detection, warm-up scheduling, sizing, pruning and clearing for a Rust, Go and Python project leave no marker on the host.

### 6.5 Detection and hidden interpreters

- `prepare(root)` checks only the project markers (cheap, every turn, as today).
- **Whether the tool exists** is learned from its first sandboxed run:
  - exit 127 with `command not found`: not installed. The kind goes **quietly** off for the session, as the formatter does. Most Python projects don't have pyright installed, and that isn't worth a notice;
  - `missing` names a hidden interpreter, with one notice, as tsc does. It now matches three forms:
    - `env: 'node': No such file or directory` (pyright via npm);
    - `bad interpreter: No such file or directory` and `required file not found` (exit 126/127), naming the shebang's path;
    - the venv's `python` symlink pointing into a hidden folder, which the kind checks on the host by reading the link (`read_link`, not running it).
- **uv's Pythons** are the common modern case. `.venv/bin/python` points into `~/.local/share/uv/python`, which isn't in `TOOLCHAINS`, so the venv's interpreter and a `.venv/bin/basedpyright` shebang dangle inside the sandbox.
  - Phase 1 always detects it and says once: "✻ pyright needs this project's Python (~/.local/share/uv/python/…), which the sandbox can't see. Marv can't tell the model about new type errors."
  - A separate small PR proposes adding `.local/share/uv/python` to `home::TOOLCHAINS`, read-only: interpreters only, no credentials. It has its own guard test and SECURITY.md line, since it widens what every sandboxed command can read.
- `/diagnostics` lists, per language: on, off, not installed, hidden interpreter, toolchain too old (Go), pending, low disk, or turned off after timeouts or unreadable runs.

### 6.6 Diffing and known limits

- **The same rule as tsc:** `new_problems` over a key without line numbers, counted, each check the next baseline, `covers: None`. Each pass is whole-project, so the whole baseline is re-checked.
- **Keys:** see the table in §4. Lines move with edits. File, code, message and (for rustc) the primary label survive moves, and identical errors in one file are counted after the in-run dedupe (§4).
- **Known limits:**
  - **Identical errors:** fixing one error and creating an identical one in the same file in the same step goes unreported (as with tsc).
  - **Cascades:**
    - when a lib error stops dependent targets from compiling, their pre-existing errors are absent from the baseline, and show up as "new" once the lib error is fixed, though the model didn't cause them;
    - `go vet` reports only the first type errors of a package, so the same happens per package;
    - tsc has a milder form of this.

    That's why the intro says the errors "weren't in the previous check", not "your changes caused".
  - **Renamed files:** an error in a moved file shows as new. That's correct, since the model did it.
  - **Pending turns:** a turn whose baseline is pending tells nothing, even about errors it introduced. The next check takes them in silently as baseline.

### 6.7 Output budget

- **Per kind:** `MAX_REPORTED` = 20 errors, the rest counted. A message is cut at 400 characters; rustc's and pyright's multi-line messages can be long.
- **Per step:** a polyglot repo could add three notes to one step, so the joined note is capped at 40 errors in all. Kinds are filled in order, with each one's count kept: "and 12 more".
- rustc's `children` (help/note) and `rendered` aren't sent; the code and label say enough. The model can run `cargo check` itself for the full text.
- The note stays fenced (`project_note`). The intro is Marv's sentence: "cargo check found 2 errors that weren't in the previous check."

### 6.8 The setting

- `diagnostics` in `~/.marv/config.json` accepts its current bool (all kinds) or an object:

  ```json
  "diagnostics": { "typescript": true, "rust": true, "python": true, "go": true }
  ```

  A missing key takes its default. `true` means every kind at its default, and `false` means all off: the oracle's `false` keeps working. Unknown keys and non-bools are config problems, reported like any other.
- **Defaults:**
  - typescript, python and go: **on**. They're cheap: no or small cache, and they skip quietly when not installed.
  - rust: **on only once §6.3's bounds have landed** (PR 1), with the warm-up at the first edit. Until then, or if the maintainer prefers, opt-in.
- `/diagnostics on|off` sets all. `/diagnostics rust off` sets one. `/diagnostics clear` deletes this project's cache.
- `Checker::reset` runs on any switch from off to on, as today, and also clears the unreadable count.

### 6.9 Oracle impact

None in behaviour:

- the oracle's config sets `"diagnostics": false`, which turns every kind off, and with it the warm-up and the cache;
- no prompt or tool-spec text changes;
- the trajectory's check records already carry `source`, and the new values (`"cargo"`, `"pyright"`, `"go"`) appear only when checks run.

The `TsError` to `Diagnostic` rename is internal. The oracle tests stay as they are.

## 7. Phase 2 gate (LSP)

Decided on phase 1's trajectories after two weeks of use:

- the median and p90 `ms` of after-step checks per kind;
- timeouts, unreadable-offs and pending turns per kind;
- cache sizes.

**Go ahead with a spike**, for pyright and/or gopls only, if a kind's warm p90 is over **10 s**, or its timeouts turn it off in more than 10% of sessions that use it. Rust stays on `cargo check`, since rust-analyzer's errors come from it.

The spike is its own spec. It needs:

- the stdio child primitive (§5.1);
- a framing client (`serde_json`);
- a "settled" rule per server, pinned by tests against real servers in CI;
- a memory budget, and a single server per project shared across the sessions of one Marv process.

## 8. Later, and only with evidence

- **Warnings** as a per-language option: rustc warnings, `go vet` is already in, pyright `warning`. They're noisy, but the baseline makes them "new warnings only".
- **mypy** for projects that configure it (`[tool.mypy]`, `mypy.ini`): `mypy --output json` (1.11+) with `--cache-dir <cache>/mypy`.
- **Subagents:** only once the main agent's numbers are in.
- **Checks after `bash` changes:** a change-detecting snapshot, which is a separate issue.
- **A memory cap** (a cgroup via systemd-run on Linux) if §7's data shows checks hurting the machine.

## 9. Decisions

1. **Phase 1 is one-shot CLI checkers on the existing `CheckKind` machinery:** `cargo check`, basedpyright/pyright `--outputjson`, `go vet`. Phase 2 (LSP) only past the §7 gate, as its own spec, with `serde_json` and our own framing.
   - *Alternative:* LSP client first, as the issue's title says. Rejected for now: a new long-lived sandbox primitive, per-server "settled" rules, resident memory, and weaker whole-project coverage.
2. **One problem type and an event with a label:** `TsError` becomes `Diagnostic`, and `SessionEvent::Check { label, result }`. Kinds stay separate `Checker`s, so timeouts and turning off are per language. The environment goes in the command string; the only `RunOptions` field used is the existing `writable`.
3. **Caches:**
   - **Location:** `<cache root>/<key>`, the root being `$XDG_CACHE_HOME/marv/checks` on Linux and `~/Library/Caches/marv/checks` on macOS. Keyed per repository (the common git dir, read without git), so worktrees share.
   - **Mount:** exactly `<root>/<key>`, writable for that check only. Never the project's `target/`, never the user's `GOCACHE`.
   - **Bounds:** 20 GB total with LRU, 10 GB per key, a 5 GB / 5% free-space guard, a 14-day age limit, and orphans of deleted roots pruned.
   - **Hygiene:** no symlink followed when sizing, pruning or clearing; a symlinked key dir is refused.
4. **Defaults:** typescript, python and go on. Rust on only together with decision 3's bounds (PR 1), otherwise opt-in.
   - *Alternative:* Rust opt-in regardless, because of the cache.
5. **Cold start:** the warm-up starts at the session's **first edit**, not its first turn, so read-only sessions never run project code. It runs up to 10 min, not counted as a timeout. A baseline waits at most 10 s, then the turn is "pending". The 60 s timeout and two-timeouts-off rule are unchanged.
6. **Locks and resources:**
   - **Lock:** a host-side `flock` per key. Lock waits are bounded separately and never count as timeouts.
   - **CPU and I/O:** `nice -n 10`, plus `ionice -c3` when present on Linux. `CARGO_BUILD_JOBS` is half the cores.
   - **Memory:** not capped, and documented.
   - **Unreadable:** 2 unreadable runs in a row turn a kind off for the session.
7. **Rust:**
   - **Command:** `--workspace --all-targets --offline --message-format=json-diagnostic-short`, stderr dropped, `RUSTUP_AUTO_INSTALL=0`.
   - **Parsing:** errors only; span-less summaries dropped; duplicates within a run folded; no `build-finished` means unreadable.
   - *Alternative:* without `--all-targets`, faster but blind to test code.
8. **Python:**
   - **Binary and scope:** the project-local binary first, then basedpyright, then pyright. Errors only, whole project.
   - **Hidden interpreters:** detected and reported once (`env: node`, `bad interpreter`, `required file not found`, a venv `python` linking into a hidden folder).
   - **uv:** adding uv's Python dir to `TOOLCHAINS` is a separate PR.
9. **Go:**
   - **Environment:** `go vet ./...` with `GOTOOLCHAIN=local GOPROXY=off -mod=readonly`, `GOWORK=off` unless `go.work` is at the root, and its own `GOCACHE`.
   - **Module cache:** `GOMODCACHE` computed from the environment and go's env file without running `go`, and mounted read-only.
   - **Parsing and notices:** continuation lines parsed. "Toolchain too old" is named once, and turns the kind off.
10. **Trust:**
    - Project files can steer what runs inside the sandbox (cargo config, `rust-toolchain.toml`, pyright's venv and `.pth`, cgo), as they do for the model's own `bash`.
    - Nothing Marv runs on the host executes or is steered by project tooling. **No toolchain is spawned on the host** for detection, paths, sizing or pruning, and a test pins it.
    - Marv's setting only switches languages on and off.
11. **Output budget:** 20 per kind, 40 per step, 400 characters per message, fenced. The intro says "weren't in the previous check", because of cascades.
12. **Setting:** `diagnostics` takes a bool or a per-language object; `/diagnostics <lang> on|off` and `/diagnostics clear`; the oracle's `false` keeps everything off.

## 10. PRs

1. **Machinery and the cache:**
   - `Diagnostic` and `label`;
   - per-kind `timeout`/`warm_up`/`cache`;
   - `Pending`, the warm-up at first edit, the per-key `flock`, `MAX_UNREADABLE`;
   - the cache: key per repo, create, mount, caps, LRU, free-space guard, age and orphan pruning, clear;
   - the setting's object form and `/diagnostics` per language;
   - the TUI notices.

   No new language yet; tsc's behaviour is unchanged. Tests:
   - tsc's existing tests pass unchanged after the rename;
   - **warm-up and pending**, with a fake warm-up kind:
     - no warm-up in a session without edits;
     - the warm-up starts at the first `before_change`;
     - a baseline due during warm-up waits `BASELINE_WAIT`, then is pending: no note, one notice per session, not counted as a timeout;
     - the next turn's baseline runs;
     - the warm-up is cancelled by `/diagnostics off` and session end, not by Esc;
   - **locks:** two sessions on one key; the second waits on the `flock`, and its wait never counts toward `MAX_TIMEOUTS`. A step's check skips as pending when the lock is held;
   - **unreadable:** two in a row turn the kind off; a readable run in between resets the count;
   - **the mount:** for that run, `RunOptions.writable` is exactly `[<root>/<key>]`, never `<root>`, and empty in the bash tool's runs. A symlinked or foreign-owned key dir is refused. **A guard test extends the existing one:** every check kind runs with `project_read_only`, `network: false`, `sandbox: true`, and `writable` holding only the key dir;
   - **the key:** worktrees of one repo get the same key, read without spawning git, and a forged `.git` file falls back to the path key;
   - **disk:**
     - the total cap evicts the least recently used (never a locked one);
     - the per-key cap turns the kind off and deletes the dir;
     - the free-space guard skips with one notice (a stand-in `statvfs`);
     - age and orphan pruning;
   - **hygiene:** sizing, pruning and `/diagnostics clear` with `target/x -> /` and `target/y -> <another key>` inside a key dir leave the link targets untouched and count only the links;
   - **no host spawn:** the marker PATH of §6.4. Detection, scheduling, sizing, pruning and clearing for Rust, Go and Python projects run nothing on the host;
   - **Seatbelt** (macOS CI job): the cache key dir under `~/Library/Caches/marv/checks` is writable in the check, while `~/Library` around it and other key dirs stay unreadable;
   - **config:** a bool or an object, unknown keys and non-bools rejected, `false` turns every kind off.
2. **Rust:** the `Cargo` kind. Tests:
   - parse fixtures from real `cargo check` JSON:
     - errors kept, warnings ignored;
     - the span-less "aborting due to N previous errors" dropped;
     - lib and lib-test duplicates folded into one;
     - no `build-finished` line → `None`;
     - `build-finished` false with no error → `None`;
     - a non-JSON line → `None`;
   - the key with the primary label;
   - the note format and the 400-character cut;
   - `RUSTUP_AUTO_INSTALL=0` and `CARGO_BUILD_JOBS` in the command;
   - **integration** (bwrap CI job, skipped without cargo), on a scratch crate:
     - baseline, then an edit adding E0308 is told once, and fixing it tells nothing;
     - the project's `target/` is never created;
   - **a malicious project:** its `build.rs` tries to write into the project, `$HOME`, another key's cache dir and the network. All of them fail, and the check still reports its errors;
   - **measured on Marv's own workspace:** cold time, warm after-step time, output size against `MAX_CAPTURE`, and cache size. These go in the PR body.
3. **Python:** the `Pyright` kind. Tests:
   - parse fixtures from basedpyright and pyright `--outputjson`: errors only, absolute paths made relative, files outside dropped;
   - choosing between project-local and PATH;
   - **hidden interpreters:**
     - `env: node` is named once, and a plain "not found" is quiet;
     - the uv case is named once: `bad interpreter` / `required file not found`, and a `.venv/bin/python` linking into `~/.local/share/uv/python`;
   - an integration test when basedpyright is installed.
4. **(Separate, small) uv's Pythons in `TOOLCHAINS`:** read-only, with a guard test and a SECURITY.md line. Can land before or after 3.
5. **Go:** the `GoVet` kind. Tests:
   - parse fixtures: `# pkg` headers, the `vet: ` prefix, tab continuation lines, an unreadable line;
   - "requires go >= …" turns the kind off with one notice;
   - `GOWORK=off` without a root `go.work`, and not with one;
   - `GOCACHE` in the key dir, never the user's;
   - `GOMODCACHE` from the environment, `GOPATH`, the default and go's env file, computed with no `go` run (the marker PATH);
   - an integration test when `go` is installed.
6. **After two weeks of use:** a short report from trajectories (§7), and the phase 2 decision.

Each PR gets a CHANGELOG "Unreleased" entry, the README is updated in PRs 2, 3 and 5, and docs/SECURITY.md's "project's own tooling" line is updated to say:

- the new checkers run in the sandbox;
- the warm-up starts at the first edit;
- the per-check cache mount and its bounds;
- project files can steer what runs inside the sandbox, never on the host;
- memory is not capped.
