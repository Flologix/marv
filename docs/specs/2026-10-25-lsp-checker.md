# Checkers for Rust, Python and Go: new errors after edits

- Issue: FlologixAI/marv #187 (split out of #69)
- Status: **draft, awaiting maintainer approval**. The decisions are in §9.
- Date: 2026-10-25, against `main` after #216.
- New in marv-rs: TypeScript Marv has only the tsc checker (`src/diagnostics/`), so the new kinds' module docs say they port nothing, while the shared machinery keeps `turn.ts`'s shape.
- Scope:
  - `crates/marv-session`: `diagnostics/` (the machinery, new check kinds), `session.rs` (wiring), `config.rs` (the setting), `commands.rs` (`/diagnostics`);
  - `crates/marv-sandbox`: nothing new in phase 1 beyond a per-check writable cache mount, which `RunOptions.writable` already carries;
  - `crates/marv-cli` (TUI) and `crates/marv-web`: showing the new results;
  - phase 2 only: a long-lived sandboxed stdio child and a small LSP client.

## 1. Summary

The issue asks for an LSP-backed checker (rust-analyzer, pyright/basedpyright, gopls). This spec recommends getting there in two phases:

- **Phase 1: one-shot command-line checkers** on the machinery the typecheck already uses: `cargo check --message-format=json`, `basedpyright`/`pyright --outputjson`, and `go vet`. Each is a new `CheckKind`. It needs no new process lifecycle, no protocol client and no new dependency. It checks the whole project, so it catches the error in another file that the edit broke. That's the case that matters most, and the one an LSP client watching open files tends to miss.
- **Phase 2: long-lived language servers**, only if phase 1's numbers show it's needed. That would be checks too slow even when warm (big Python projects, Go monorepos). Phase 2 is a spike with its own decision. rust-analyzer comes last, or never: its type errors come from running `cargo check` itself ("flycheck"), so a server would add memory but not much signal.

My estimate is that phase 1 gets about 80% of the value for about 10–20% of the work of an LSP client in the sandbox.

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
- **The baseline:** `BaselineHook::before_change` runs once per turn, before the first change. The edit tools start it from their preview and wait for it before writing, so yolo turns get it too. Later calls wait for the same run.
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
  - output at `MAX_CAPTURE` counts as unreadable, because a cut list would make problems look fixed;
  - a parse panic counts as unreadable.

  Only the main agent gets checks.
- **The sandbox:** `sandboxed_options` builds `RunOptions` for `marv_tools::bash::run_command`:
  - `sandbox: true`, `network: false`;
  - `project_read_only: true`;
  - the worktree's `.git` read-only;
  - `writable: []`, `pipe_check: false`.

  Without a working sandbox nothing runs, and the first change of the session says so once (`Off { NoSandbox }`). docs/SECURITY.md: "The project's own tooling runs only inside the sandbox."
- **Detection:** `tsc::detect_checker` uses only the project's own `node_modules/.bin/tsgo|tsc`, and only when `tsconfig.json` exists: "A global one isn't used". The formatter (#67, `marv_tools::formatter`) is the precedent for global toolchains. It runs `rustfmt` when `Cargo.toml` exists and `gofmt` when `go.mod` does, found through the sandbox's PATH, and it skips quietly when the tool isn't installed.
- **Plugging in a kind:**
  - `ProjectChecksKind` (`checks/kind.rs`) is the second kind and shows the pattern: implement `CheckKind`, add a `Checker` field in `session.rs` (`typecheck`, `project_checks`), add one block in `turn_checks`, and an event and notice.
  - The problem type is per kind. `TsError` is generic in all but name: file, line, column, code, message.
- **Config:** `diagnostics` (bool, default `true`) in `~/.marv/config.json`, with `/diagnostics on|off`. **There is no project-level config file.** `config.rs` reads only `~/.marv` or `MARV_CONFIG_DIR` from the real environment, never a project file. A project file can choose *whether* a check applies (`tsconfig.json`, `.marv/checks/`), never *which program on the host* runs. Everything runs sandboxed.
- **Oracle:** `crates/marv-oracle/src/runner.rs` writes `"diagnostics": false` into every scenario's config. TypeScript Marv has no checkers for these languages.

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

- After an edit in a Rust, Python or Go project, the model hears about compile/type errors that are **new since the turn started**, with the same guarantees as the typecheck:
  - sandboxed;
  - the project read-only;
  - no network;
  - fenced as project data;
  - told once;
  - silent when unsure.
- Use only toolchains already installed (Marv never installs one), found the way the sandbox's `bash` would find them.
- Bounded cost on the user's machine: CPU, memory, disk, and the latency added to an edit.
- No change to the system prompt or tool specs, so no prompt-cache break.

**Non-goals:**

- Warnings and lints: clippy, `ruff check`, unused variables. That's errors only, as with tsc; warnings could come later (§8).
- Hover, go-to-definition, symbol search or any other LSP feature exposed as a tool. That's a separate issue if wanted.
- Subagents: they don't get checks today either.
- Checks after `bash` changes: that's an existing limit of `has_diff`, unchanged here.
- A user-defined checker command: that's what `.marv/checks/` is for.

## 4. Option A: one-shot command-line checkers (phase 1)

| | Rust | Python | Go |
|---|---|---|---|
| Applies when | `Cargo.toml` at the root | `pyrightconfig.json`, or `pyproject.toml` / `setup.py` / `setup.cfg` / `requirements.txt` at the root | `go.mod` at the root |
| Binary | `cargo` (rustup shim on PATH) | the project's `.venv/bin/basedpyright` or `node_modules/.bin/pyright`, then `basedpyright`, then `pyright` on PATH | `go` on PATH |
| Command | `cargo check --workspace --all-targets --offline --message-format=json --target-dir <cache>/target` | `<bin> --outputjson` | `go vet ./...` |
| Env | `CARGO_TERM_COLOR=never` | none | `GOFLAGS=-mod=readonly GOPROXY=off GOTOOLCHAIN=local GOCACHE=<cache>/go-build`; `GOMODCACHE` mounted read-only |
| Writable cache | yes, the target dir (GBs) | none needed | the build cache (hundreds of MB) |
| Warm check | ~0.3 s small crate; seconds on a workspace | seconds; ~30 s+ on very large projects | seconds |
| Cold check | minutes on a big workspace (all dependencies) | the same as warm | tens of seconds (std + deps) |
| Output | JSON lines; `compiler-message` with `level: "error"` | JSON; `generalDiagnostics[]` with `file`, `severity`, `message`, `rule`, `range` | text: `path:line:col: message` (with an optional `vet: ` prefix) under `# pkg` headers |
| Key | file (primary span) + code + message + primary label | file + rule + message | file + message |

### Notes per language

**Rust:**

- `--all-targets`, so errors in tests and `#[cfg(test)]` modules count. The model edits tests too. It costs about twice a lib-only check when cold, much less when warm.
- **Precedence:** `--target-dir` beats the project's `.cargo/config.toml` `build.target-dir` and `CARGO_TARGET_DIR`.
- **What the project's code can do:** `build.rs`, proc macros and a project `rustc-wrapper` all run, sandboxed, as they would for the model's own `cargo check` in `bash`.
- `rust-toolchain.toml` naming a toolchain that isn't installed fails offline. That counts as unreadable, with one notice.
- **Parse rules:**
  - `None` (unreadable) when `build-finished` says `success: false` but no `error` was parsed: a failed build script, a missing toolchain, a lockfile that needs updating under `--offline`;
  - `None` when a line isn't JSON (cargo's own text goes to stderr, which we keep apart; see §6.1).
  - The message includes the primary span's label. The key does too, because rustc's bare messages ("mismatched types") are too generic to tell two errors in one file apart.

**Python:**

- **basedpyright first:** its PyPI wheel bundles node, so it works offline. The PyPI `pyright` wrapper downloads node and the npm package on first run, which fails without network (unreadable, one notice). npm's `pyright` needs `node` (the same `missing` path as tsc: `env: 'node': No such file or directory`, exit 127).
- **Errors only:** `severity: "error"`.
- **Paths:** `file` is absolute, so it's shown relative to the root, and a file outside the root is dropped.
- **Imports:** the project's `.venv` is read inside the project, so imports resolve when it exists. When it doesn't, the `reportMissingImports` errors are in the baseline and never told.
- A project with neither pyright config nor `[tool.pyright]`/`[tool.basedpyright]` runs in pyright's default "standard" mode. The baseline absorbs the noise; the cost is time.

**Go:**

- **Why `go vet` over `go build ./...`:** it type-checks test files too, and its analyzer findings (printf mismatches, copied locks) are real bugs.
- **Offline:** `GOTOOLCHAIN=local` and `GOPROXY=off` fail fast instead of trying the network. `-mod=readonly` keeps `go.mod` untouched, which the read-only project enforces anyway.
- **The module cache:** `~/go/pkg/mod` (`go env GOMODCACHE`) sits in the stand-in home, which is empty. The pass mounts it read-only through `CheckPass.read_only`, which exists for this.
- **Parse rules:** `# pkg` headers are skipped. Any other line that isn't `path:line:col: message` makes the run unreadable, as tsc's parser does.

### Why this fits

- It is the typecheck, three more times. `CheckKind`, baseline, `new_problems`, notes, notices, timeouts, Esc handling and the sandbox are all reused.
- **Whole project:** an edit to a function signature reports the callers it broke in other files.
- **Memory and CPU:** peak memory only while a check runs; nothing stays resident.
- **macOS:** `run_command` already runs under Seatbelt (#164). Nothing new to port.

### What it costs

- **Cold start:** Rust's cold check on a real workspace takes minutes, which the 60 s timeout would cut off twice and turn off. That needs warm-up and "pending" semantics (§6.2).
- **Disk:** a second Rust target dir. It must not be the project's `target/`: §6.3.
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

   Often that's on top of the same servers in the user's editor. With several Marv sessions (mesh, desktop background chats) it multiplies.

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

1. **One problem type:** rename `TsError` to `Diagnostic { file, line, column, code, message }`, a struct that already fits all four tools. tsc's parser keeps returning it. `CheckResult` stays `CheckOutcome<Diagnostic>`. Each kind's `Checker` gets a `label` ("tsc", "cargo", "pyright", "go vet") for notices, the note's intro and `check_record`'s `source`. The event becomes `SessionEvent::Check { label, result }`, and the TUI/web show "✻ cargo check: 2 new errors, told the model:".
2. **Per-kind policy on `CheckKind`**, with defaults that keep tsc's behaviour:
   - `timeout()` (60 s);
   - `warm_up()` (none; §6.2);
   - `cache()`, a writable cache folder to mount (§6.3);
   - `env()`, extra environment for the sandboxed command: `sandboxed_options` gets an env list for the sandbox, not the unsandboxed `env`. That's a small `RunOptions` addition, or the env set in the command as `VAR=… cmd`, which needs no API change and is what phase 1 does.
3. **Stdout and stderr:** `run_command` captures them together. cargo's progress text on stderr would break the JSON-lines parser. The command redirects `2>/dev/null` (`cargo … 2>/dev/null`), or the parser skips lines that don't start with `{`. **Decision:** redirect, keeping the parser strict.
4. **Output size:** `MAX_CAPTURE` applies. A cold `cargo check --message-format=json` prints one `compiler-artifact` line per crate, which adds up. No filter goes in the pipe, since `jq` isn't guaranteed in the sandbox. The parser ignores artifact lines. If a real workspace hits `MAX_CAPTURE`, the check is unreadable, which is safe. PR 2 measures this on Marv's own workspace. If it's too close, a filter in Marv's own binary is the fix, run in the sandbox as the project checks' runner is (`marv` mounted read-only).
5. **Lower priority:** commands run under `nice -n 10` (part of the command string), so a check never competes with the user's editor at equal priority.

### 6.2 Cold start: warm-up and "pending"

The baseline is taken before the turn's first write, and the write waits for it. A cold Rust check would hold up the first edit for 60 s, then time out and count toward turning Rust off. So:

- **Warm-up.** A kind with `warm_up()` (Rust; Go when there's no build cache yet) starts one background run of its baseline command:
  - **When:** the first turn of a session where `prepare` is true, so the cache is warm by the time the model edits.
  - **Lifetime:** session-scoped. It's cancelled by session end and `/diagnostics off`, not by the turn's Esc.
  - **Timeout:** `WARM_UP_TIMEOUT` (10 min), not counted in `MAX_TIMEOUTS`.
  - **Output:** discarded. Its only job is the cache.
- **Pending.** If the turn's baseline is due while the warm-up is still running, `before_change` waits up to `BASELINE_WAIT` (10 s). Still not done:
  - the baseline is `Taken(None)`, so this turn says nothing;
  - the outcome is `Failed { reason: Pending }`, with a notice once per session: "✻ cargo check is still building its cache for this project (first run); Marv will tell the model about new errors once it's ready."

  The next turn's baseline runs on the warm cache.
- **Timeouts after warm-up still count.** A warm check that can't finish in 60 s is too slow for every step, and two of them turn the kind off, as today.
- **Only one at a time.** The warm-up and a turn's check never run together. A check waits for the warm-up up to `BASELINE_WAIT`, and they would contend for cargo's build-dir lock anyway.

### 6.3 Caches, disk and trust

- **Where:** `$XDG_CACHE_HOME/marv/checks/<project key>/` (default `~/.cache/marv/checks/…`; `~/Library/Caches/marv/checks/…` on macOS). The key is `paths::project_key`. Inside it: `target/` (Rust) and `go-build/` (Go).
- **Mounted writable for that check only** through `RunOptions.writable`, the way an approved `git_write` command gets its worktree records. It is **not** added to the user's `writable` list (#98).
  - **The #98 rule against hidden home folders:** it would refuse `~/.cache`. That rule exists because the host might *execute* something from there (`.local/bin`, autostart, hooks). Nothing on the host reads or runs this folder: Marv only creates it (0700) and prunes it.
  - **Pruning** never follows symlinks (`remove_dir_all` on the folder itself, after checking it is a real directory under the cache root, not a link). A check can only poison its own project's later checks, which run the same project code anyway.
- **Why not the project's `target/`:**
  - **Security:** checks run with the project read-only on purpose, because they run without approval. A writable `target/` would let a sandboxed `build.rs` plant artifacts that the user's own *unsandboxed* `cargo build`/`cargo test` later links and runs.
  - **Contention:** different flags and env would make the user's builds and Marv's checks invalidate each other's fingerprints. They would also block on cargo's build-dir lock while the user's editor's rust-analyzer holds it.
- **Disk:**
  - a Rust project's check cache is about the size of its `target/debug` without the link artifacts, often 1–5 GB;
  - caches untouched for **14 days** are pruned at startup;
  - `/diagnostics` shows each checker's state and the project's cache size;
  - `/diagnostics clear` deletes this project's cache.

  This matters: a full disk stalls everything else on the machine.
- **Which binary runs:**
  - The command names fixed programs (`cargo`, `go`, `basedpyright`, `pyright`). Resolution happens inside the sandbox with the sandbox's PATH, exactly as the model's own `bash` would resolve them.
  - A project-local `node_modules/.bin/pyright` or `.venv/bin/basedpyright` is the project's own code, as `node_modules/.bin/tsc` is: allowed, because it runs sandboxed with the project read-only and no network.
  - No setting from any project file chooses a binary, a command or a flag. The user's config switches languages on and off and takes no commands (§6.5).
- **Toolchains in the sandbox:**
  - `.cargo`, `.rustup`, `.local/bin`, `.local/share/mise`, `.asdf`, `.pyenv` are mounted read-only (`home::TOOLCHAINS`), so rustup and mise shims work.
  - pipx's venvs (`~/.local/share/pipx`) aren't. A pipx `basedpyright` is a symlink into a hidden folder, so it shows up as missing.
  - `~/go/pkg/mod` is mounted per Go check (§4).

### 6.4 Detection, once per session

- `prepare(root)` checks only the project markers (cheap, every turn, as today).
- **Whether the tool exists** is learned from its first run:
  - exit 127 with `command not found`, or the `env:` message, means the tool isn't there;
  - `missing` names the hidden interpreter when there is one (pyright's `node`), with one notice, as tsc does;
  - otherwise the kind goes **quietly** off for the session, as the formatter does. Most Python projects don't have pyright installed, and that isn't worth a notice.
- `/diagnostics` lists, per language: on, off, not installed, pending, or turned off after timeouts.

### 6.5 The setting

- `diagnostics` in `~/.marv/config.json` accepts its current bool (all kinds) or an object:

  ```json
  "diagnostics": { "typescript": true, "rust": true, "python": true, "go": true }
  ```

  A missing key takes its default. `true` means every kind at its default, and `false` means all off: the oracle's `false` keeps working.
- **Defaults:**
  - typescript, python and go: **on**. They're cheap: no or small cache, and they skip quietly when not installed.
  - rust: **on**, with warm-up. The disk cost is the trade-off; see decision 4.
- `/diagnostics on|off` sets all. `/diagnostics rust off` sets one.
- `Checker::reset` runs on any switch from off to on, as today.

### 6.6 Diffing

- **The same rule as tsc:** `new_problems` over a key without line numbers, counted, each check the next baseline, `covers: None`. Each pass is whole-project, so the whole baseline is re-checked.
- **Keys:** see the table in §4. Lines move with edits. File, code, message and (for rustc) the primary label survive moves, and two identical errors in one file are counted.
- **Known limit, as with tsc:** fixing one error and creating an identical one in the same file in the same step goes unreported.
- **Renamed files:** an error in a moved file shows as new. That's correct, since the model did it.

### 6.7 Output budget

- **Per kind:** `MAX_REPORTED` = 20 errors, the rest counted. A message is cut at 400 characters; rustc's notes and pyright's multi-line messages can be long.
- **Per step:** a polyglot repo could add three notes to one step, so the joined note is capped at 40 errors in all. Kinds are filled in order, with each one's count kept: "and 12 more".
- rustc's `children` (help/note) and `rendered` aren't sent; the code and label say enough. The model can run `cargo check` itself for the full text.
- The note stays fenced (`project_note`). The intro is Marv's sentence: "cargo check found 2 new errors after your changes."

### 6.8 Oracle impact

None in behaviour:

- the oracle's config sets `"diagnostics": false`, which turns every kind off;
- no prompt or tool-spec text changes;
- the trajectory's check records already carry `source`, and the new values (`"cargo"`, `"pyright"`, `"go"`) appear only when checks run.

The `TsError` to `Diagnostic` rename is internal. The oracle tests stay as they are.

## 7. Phase 2 gate (LSP)

Decided on phase 1's trajectories after two weeks of use:

- the median and p90 `ms` of after-step checks per kind;
- timeouts and offs per kind;
- how often warm-up was pending.

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

## 9. Decisions

1. **Phase 1 is one-shot CLI checkers on the existing `CheckKind` machinery:** `cargo check`, basedpyright/pyright `--outputjson`, `go vet`. Phase 2 (LSP) only past the §7 gate, as its own spec.
   - *Alternative:* LSP client first, as the issue's title says. Rejected for now: a new long-lived sandbox primitive, per-server "settled" rules, resident memory, and weaker whole-project coverage.
2. **One problem type and an event with a label:** `TsError` becomes `Diagnostic`, and `SessionEvent::Check { label, result }`. Kinds stay separate `Checker`s, so timeouts and turning off are per language.
3. **Caches:**
   - per project under `$XDG_CACHE_HOME/marv/checks/<key>/`;
   - mounted writable per check only, never the project's `target/`;
   - pruned after 14 days;
   - shown and cleared by `/diagnostics`.
4. **Defaults:** typescript, python and go on. Rust **on with warm-up**.
   - *Alternative:* Rust opt-in because of the 1–5 GB cache.
   - *Recommendation:* on. The maintainer's own projects are Rust, and pruning bounds the disk.
5. **Cold start:** a session-scoped warm-up (10 min, not counted as a timeout), with `BASELINE_WAIT` 10 s, then "pending" (silent this turn, one notice per session). The 60 s check timeout and two-timeouts-off are unchanged.
6. **Rust command:** `--workspace --all-targets --offline`, stderr dropped, under `nice`.
   - *Alternative:* without `--all-targets`, faster but blind to test code.
7. **Python:** project-local binary first, then basedpyright, then pyright. Errors only. Whole project.
8. **Go:** `go vet ./...` with `GOTOOLCHAIN=local GOPROXY=off -mod=readonly`, and `GOMODCACHE` mounted read-only.
9. **Trust:** no project file chooses a binary or a command. Fixed program names, resolved inside the sandbox. The user's setting takes only on/off per language.
10. **Output budget:** 20 per kind, 40 per step, 400 characters per message, fenced.
11. **Setting:** `diagnostics` takes a bool or a per-language object; `/diagnostics <lang> on|off`; the oracle's `false` keeps everything off.

## 10. PRs

1. **Machinery:**
   - `Diagnostic` and `label`;
   - per-kind `timeout`/`warm_up`/`cache`;
   - `Pending`;
   - the cache folder (create, mount, prune, clear);
   - the setting's object form and `/diagnostics` per language;
   - the TUI/web notices.

   No new language yet; tsc's behaviour is unchanged. Tests:
   - tsc's existing tests pass unchanged after the rename;
   - a fake warm-up kind: a baseline due during warm-up waits `BASELINE_WAIT`, then is pending (no note, one notice per session, not counted as a timeout), and the next turn's baseline runs;
   - a warm-up is cancelled by `/diagnostics off` and session end, not by Esc;
   - the cache mount is in `RunOptions.writable` for that run only, and never in the bash tool's runs;
   - pruning removes old folders, keeps fresh ones, and never follows a symlinked entry;
   - the config takes a bool or an object, rejects unknown keys and non-bools, and `false` turns every kind off;
   - a guard test: every check kind runs with `project_read_only`, `network: false` and `sandbox: true` (extending the existing guard).
2. **Rust:** the `Cargo` kind. Tests:
   - parse fixtures from real `cargo check` JSON: errors, warnings ignored, `build-finished` false with no error → `None`, a non-JSON line → `None`;
   - the key with the primary label;
   - the note format and the 400-character cut;
   - an integration test (bwrap CI job, skipped without cargo) on a scratch crate: baseline, an edit adding E0308 is told once, fixing it tells nothing, the project's `target/` is never created;
   - measured on Marv's own workspace: cold time, warm after-step time, output size against `MAX_CAPTURE`, and cache size. These go in the PR body.
3. **Python:** the `Pyright` kind. Tests:
   - parse fixtures from basedpyright and pyright `--outputjson`: errors only, absolute paths made relative, files outside dropped;
   - choosing between project-local and PATH;
   - exit 127 with the `env: node` message is named once, and a plain "not found" is quiet;
   - an integration test when basedpyright is installed.
4. **Go:** the `GoVet` kind. Tests:
   - parse fixtures (`# pkg` headers, the `vet: ` prefix, an unreadable line);
   - the `GOMODCACHE` mount and env in the command;
   - an integration test when `go` is installed.
5. **After two weeks of use:** a short report from trajectories (§7), and the phase 2 decision.

Each PR gets a CHANGELOG "Unreleased" entry, the docs/SECURITY.md line on "the project's own tooling" names the new checkers and the cache mount, and the README is updated in PRs 2 to 4.
