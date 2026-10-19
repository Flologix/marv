# marv-sandbox

What commands can touch and which paths an agent may reach. It covers:
- the sandbox's plan and backends (bubblewrap today; Seatbelt on macOS next, `docs/specs/2026-10-17-macos-sandbox.md`);
- the bubblewrap argument builder;
- the stand-in home and its credential masks;
- the `.git` guard;
- the `Marv:` notes bash adds;
- the path rules (every path is checked after `realpath`);
- protected paths and taint.

It is the enforcement half of `docs/SECURITY.md`. Nothing in it is async, and nothing in it depends on marv-tools.

Spec: the **Sandbox** and **Yolo mode** bullets in Marv's `CLAUDE.md`, `src/sandbox.ts`, the path half of `src/tools/files.ts`, `gitGuard` and the notes in `src/tools/bash.ts`, and `tests/{bash,tools,git-worktree-sandbox,worktree-sandbox}.test.ts`.

## Modules

| Marv (TS) | Rust module | Status |
|---|---|---|
| (new, #163) | `plan.rs` | **Done and tested.** `Plan` (was `SandboxOptions`, which stays as bwrap's alias): what one command's sandbox allows, built by `Sandbox::plan`. The policy every backend shares is methods over a `FsProbe`: `read_only_extras`/`writable_extras` (`resolve_extra`, moved here: refused when the home, a parent of it, `/` or (read-only) the project; `Extra { given, real, around_project }`), `home_project`, `project_read_only`, `toolchains`, `hidden_dirs`, `config_dir_hidden`, `credentials`, `mesh_bind`, and `env` (G19's list, plus `MARV_MESH_SOCKET` when plugged) |
| (new, #163) | `backend.rs` | **Done.** `SandboxBackend` (`name`, `available`, `command` → `SandboxCommand { argv, env, leftovers }`, `guest_path`, `hides`, `kill`, `terminate`, `after`) and `backend()`, which picks the platform's: bubblewrap everywhere for now (it isn't available on macOS, so commands there run unsandboxed with the warning, as before) |
| (new, #163) | `fs.rs` | `FsProbe`, `RealFs`, `EntryKind`, `FileKind`, moved out of `bwrap.rs` (re-exported there) so every backend checks paths the same way; tests share one `Fake` (`test_fs.rs`) |
| `src/sandbox.ts` (`sandboxArgs`, `runMounts`, `resolveExtra`, `sandboxAvailable`) | `bwrap.rs` | **Ported and tested.** #163: `Bwrap`, the `SandboxBackend` over `sandbox_args`, which reads the policy from the `Plan` and keeps only how bwrap spells it (mount order, tmpfs or `/dev/null`, `run_mounts`, the home's socket scan). Adds protected-path read-only binds and the mesh plug at `/run/marv/mesh.sock`. The file system is injectable (`FsProbe`). #24: `binds` (Marv's Sprint 4 `binds`: a mount at a path of its own after the empty `/run`, read-only unless `writable`), for the process door's relay |
| `src/sandbox.ts` (`TOOLCHAINS`, `CREDENTIALS`, `credentialMasks`, `hiddenPathFolders`) | `home.rs` | **Ported and tested.** marv-rs: `HOME_MASKS` and `home_project` for a project that is the home folder (see "The home folder as the project") |
| `src/tools/files.ts` (`resolveInProject`, `insideGitDir`, `touchesGit`, `refuseGit`, `projectPath`) | `paths.rs` | **Ported and tested.** New: `resolve_within`, the realpath containment check for skill files, with a symlink test |
| `src/tools/bash.ts` (`gitGuard`, `worktreeGit`, `worktreeGitDirs`) | `git_guard.rs` | **Ported and tested** |
| `src/tools/bash.ts` (`GIT_HINT`, `HOME_NOTE`, `NETWORK_NOTE`, `GH_NOTE`, `GH_TOOL_NOTE`, `BACKGROUND_NOTE`, `BACKGROUND_NOTE_PROCESS`, `touchesHiddenHome`, `backgroundsProcess`, `looksLikeNetworkFailure`) | `notes.rs` | **Ported and tested** (regex has no lookahead, so the word ends are checked by hand, and `backgrounds_process`'s `&` lookahead is a consuming group). The background notes and `backgrounds_process` (Marv #50, #51) came with #24. marv-rs #81: `ESCALATED_NOTE`, on a retry bash ran with network because the same command already got `NETWORK_NOTE` |
| new (`docs/SECURITY.md`) | `protected.rs` | **Done and tested:** `PROTECTED_PATHS`, `rule_for_relative`, `protected_rule`/`is_protected` (realpath, a dangling final symlink followed, fails closed), `hooks_path`, `home_rule` (when the project is the home folder or holds it: the home's hidden top-level entries and the `core.hooksPath` folders of the global git config and of each repository in the home are `HomeDotfile`, since `edit_file`/`write_file` write there outside the sandbox; guard `in_the_home_folder_dotfiles_and_hooks_ask_even_in_yolo`), `lifecycle_scripts_changed`, `protected_mounts` (top level only), and the post-command scan: `protected_snapshot`, `scan_created`, `revert_created`, `created_note` (#8); `is_secret_name` (docs/SECURITY.md's secret names: `.env*` but `.envrc`, `*.pem`, `id_*`; used by instruction files, #62; the read_file gate is still to come) |
| `src/tools/bash.ts` (`secretVariable`, `hostEnv`) | `host_env.rs` | **Ported and tested** (marv-rs #23, Marv #52; `host-env.test.ts`): the environment of what runs on the host, Marv's keys and secret-looking variables stripped |
| new (`docs/SECURITY.md`) | `taint.rs` | **Done and tested:** `TaintState` (`mark`, `is_tainted`, `must_ask`, approved prefixes, `for_subagent`) |
| (new, #98) | `writable.rs` | **Done and tested:** the config's `"writable"` resolved once at startup (`resolve` → canonical paths + startup problems; `~` expanded, symlinks followed, the rules judged on the canonical path), and the refuse list (`/`, the home and its parents, the project's parents, any hidden top-level home entry (`~/.<anything>` — the message names the known ones, the rule doesn't need the list), a git repository (the entry is one, under a `.git` component, or directly holding a `.git` entry, worktree file included; a repo planted deeper inside an accepted folder later is *not* scanned — stated in the doc), the mesh's runtime sockets (`marv/` in the runtime dir, inside it or holding it), Marv's config folder (`config_dir`, wherever `MARV_CONFIG_DIR` puts it), missing/non-folders); an entry in the project or inside another accepted entry is left out quietly (#119 review: a swapped parent would let bwrap bind a symlink's target) |
| — | `lib.rs` | `Sandbox` (the session's handle: `from_env`, `active`, `backend`, `plan`, `prepare` (#163: the `SandboxCommand` callers spawn), `command`, `command_argv` for an inner command other than `bash -c`; #98: `writable`, the session's extra writable folders, merged under each command's own), `CommandMounts` (with `binds`, #24), `SandboxError` |

No `todo!` is left in this crate. The gaps are listed below.

## Interfaces other crates use

```rust
pub struct Sandbox { pub enabled: bool, pub home: PathBuf, pub path: String, pub lang: String, pub mesh_socket: Option<PathBuf>, pub hidden: Vec<PathBuf>, pub writable: Vec<PathBuf>, pub config_dir: Option<PathBuf> }  // writable: the #98 extras; config_dir: hidden everywhere
pub fn home_project(root: &Path, home: &Path) -> bool;  // the project is the home folder or a parent of it
pub fn writable::resolve(entries: &[String], home: &Path, root: &Path, runtime: Option<&Path>, config_dir: Option<&Path>) -> (Vec<PathBuf>, Vec<String>);  // accepted + problems
impl Sandbox {
    pub fn from_env(enabled: bool) -> Self;
    pub fn active(&self) -> bool;                                    // enabled && backend().available()
    pub fn backend(&self) -> &'static dyn SandboxBackend;
    pub fn plan(&self, root: &Path, mounts: &CommandMounts) -> Plan; // the session's sandbox + the command's mounts
    pub fn prepare(&self, root: &Path, mounts: &CommandMounts, inner: &[OsString]) -> Result<SandboxCommand, SandboxError>;
    pub fn command(&self, root: &Path, mounts: &CommandMounts, script: &str) -> Result<Vec<OsString>, SandboxError>;
}
pub trait SandboxBackend: Send + Sync + Debug {                      // #163
    fn name(&self) -> &'static str;                                  // "bubblewrap"
    fn available(&self) -> bool;
    fn command(&self, plan: &Plan, inner: &[OsString], fs: &dyn FsProbe) -> Result<SandboxCommand, SandboxError>;
    fn guest_path(&self, bind: &Bind) -> PathBuf;                    // where a bind (or the plug) appears inside
    fn hides(&self, dir: &Path, home: &Path) -> bool;                // bwrap: hidden_in_sandbox
    fn kill(&self, pid: u32);                                        // the whole sandbox, at once
    fn terminate(&self, pid: u32);                                   // a process's stop, before kill
    fn after(&self, command: &SandboxCommand);                       // bwrap: rmdir the placeholder mount points
}
pub struct SandboxCommand { pub argv: Vec<OsString>, pub env: Vec<(OsString, OsString)>, pub leftovers: Vec<PathBuf> }
pub fn backend() -> &'static dyn SandboxBackend;
pub const MESH_SOCKET_ENV: &str = "MARV_MESH_SOCKET";
pub struct CommandMounts { pub network: bool, pub read_only: Vec<PathBuf>, pub writable: Vec<PathBuf>, pub placeholders: Vec<PathBuf>, pub project_read_only: bool }
pub fn sandbox_args(opts: &SandboxOptions, fs: &dyn FsProbe) -> Result<Vec<OsString>, SandboxError>;
pub fn sandbox_available() -> bool;
pub const MESH_SOCKET: &str = "/run/marv/mesh.sock";

pub fn resolve_in_project(root: &Path, path: &str, home: &Path) -> Result<PathBuf, SandboxError>;
pub fn resolve_within(dir: &Path, file: &str) -> Result<PathBuf, SandboxError>;  // skills: realpath containment
pub fn touches_git(root: &Path, path: &str, home: &Path) -> bool;
pub fn paths::refuse_git(root: &Path, absolute: &Path) -> Result<(), SandboxError>;
pub fn project_path(root: &Path, absolute: &Path) -> String;

pub fn is_protected(root: &Path, path: &Path) -> bool;                            // fails closed
pub fn protected_rule(root: &Path, path: &Path) -> Result<Option<ProtectedRule>, SandboxError>;
pub fn protected::lifecycle_scripts_changed(before: Option<&str>, after: &str) -> bool;
// After a command (#8): snapshot before, scan after (bwrap placeholders removed first), then revert and tell the model.
pub fn protected::protected_snapshot(root: &Path) -> ProtectedSnapshot;
pub fn protected::scan_created(root: &Path, before: &ProtectedSnapshot) -> Vec<CreatedProtected>;  // { path, rule, reverted }
pub fn protected::revert_created(root: &Path, created: &mut [CreatedProtected]);
pub fn protected::created_note(created: &[CreatedProtected]) -> Option<String>;               // the `Marv:` note

pub struct TaintState;  // mark(Provenance) -> bool, is_tainted(), sources(), must_ask(ToolKind, uses_network), approve_prefix, prefix_approved, for_subagent
pub fn git_guard::git_guard(root: &Path) -> GitGuard;  // { read_only, placeholders }
pub fn home::hidden_path_folders(path: &str, home: &Path, root: Option<&Path>, exists: &dyn Fn(&Path) -> bool) -> Vec<PathBuf>;
pub fn notes::{looks_like_network_failure, runs_gh, touches_hidden_home}; pub const notes::{GIT_HINT, HOME_NOTE, NETWORK_NOTE, ESCALATED_NOTE, GH_NOTE};
```

## Port notes

- **One plan, a backend per platform (#163).** The bwrap code mixed two jobs: deciding the policy and spelling it as arguments. The policy is the `Plan` and its methods; `Bwrap` renders it. Callers don't assume bwrap any more:
  - bash and the process manager spawn `Sandbox::prepare`'s `SandboxCommand` (`env_clear()`, then its `env`: `PATH` for bwrap, which sets the command's own with `--setenv`; `sandbox-exec` will need the whole list), and hand it to `after` once the command has ended (bwrap: the placeholder mount points, removed while empty; bash no longer does this itself);
  - a timeout, a stop or Marv's exit kills through the backend (`kill`, `terminate`; bwrap: SIGKILL/SIGTERM to bwrap, whose PID namespace takes the rest down);
  - the relay's binary and socket paths are `guest_path` of their binds (`relay::sandbox_argv_at`), not `/run/marv-bin` and `/run/marv` assumed;
  - a plugged command gets `MARV_MESH_SOCKET` with its plug's path inside (bwrap: `/run/marv/mesh.sock`); `marv_mesh::plug::sandbox_socket` reads it, falling back to `SANDBOX_SOCKET`. Seatbelt can't mount, so there it will be the host path;
  - the mesh's runtime folder check still calls `hidden_in_sandbox` directly; `SandboxBackend::hides` is the same rule for bwrap, for the runtime-folder work to switch to.
  The spec's sketch stores the resolved values as `Plan` fields; they are methods here, so a hand-built `Plan` (the bwrap tests, unchanged) can't disagree with its resolved view, and `Sandbox::plan` can't fail (every refusal happens in the backend, before anything starts).
- **`SandboxOptions.lang`** replaces Marv's read of `process.env.LANG` inside `sandboxArgs`, so the function stays pure.
- **Args are `OsString`s**, so a path that isn't UTF-8 still mounts.
- **Order of mounts:**
  1. `/`, `/dev`, `/proc`, `/tmp`, the home tmpfs;
  2. the empty `/run`, its kept folders, the mesh socket;
  3. toolchains;
  4. extras around the project;
  5. the project;
  6. other extras;
  7. writable folders (the command's own, then the session's #98 extras — a folder the caller made read-only stays that way);
  8. protected paths (skipped when the project is read-only);
  9. placeholders;
  10. Marv's runtime folder (`hidden`), then Marv's config folder (`config_dir`), an empty tmpfs unless the stand-in home hides it already;
  11. in a home-folder project, `home_masks` (below);
  12. credential masks, last.
- **The home folder as the project.** When the project is the home folder or a parent of it (`home::home_project`, on real paths: Marv started in `~`, or in `/home`), a writable project bind would put the real home back over the stand-in. Instead:
  - the project is mounted **read-only** (`--ro-bind`); at `/` it isn't mounted again (the first mount shows it read-only, and binding `/` again would cover `/tmp`, `/run` and the stand-in home, which stays). Protected paths and placeholders are skipped (nothing in the project is writable), and bash skips the protected-path scans and nested `.git` walk there;
  - `home_masks`, on top of it: (1) every hidden top-level entry (`~/.<anything>`) except toolchains named whole in `TOOLCHAINS` (`.bun`, `.cargo`, `.gitconfig`…) becomes an empty tmpfs (a folder) or `/dev/null` (anything else; a symlink masks its target inside the home); that covers `~/.marv`, credentials, cloud CLI folders, `~/.claude`, and `~/.config`, `~/.local` and `~/.cache`, where tools keep their sockets; (2) the toolchain folders inside those (`.local/bin`, `.config/git`, `.marv/skills`…) are shown again read-only; (3) `HOME_MASKS` (`.ssh`, `.gnupg`, `.config/herdr`, `.config/gh`, `.docker`, `.kube`, `.local/share/keyrings`…) are masked again wherever step 2 would show one; (4) every Unix socket and FIFO in the visible part of the home gets `/dev/null`, found by a breadth-first scan (`SOCKET_SCAN_DEPTH` 6 levels, `SOCKET_SCAN_LIMIT` 20,000 entries, no symlinks followed, `node_modules`, `target` and `.git/objects` skipped). A read-only mount doesn't stop `connect()` on a socket or a write into a FIFO, so they must be gone, not read-only. A home that can't be listed refuses the command;
  - commands can write only to `/tmp`. `edit_file` and `write_file` run in Marv's own process, outside bwrap, and keep their approvals (yolo is refused in the home folder; protected paths such as `~/.marv/**` ask every time; every other file change is asked `once`, with no "always": `marv_tools::registry::home_file_write`). Bash adds `HOME_PROJECT_NOTE` after a failed command or one that names the home or a hidden entry, so the model doesn't retry writes or take an empty `~/.ssh` for the real one;
  - the config's `writable` entries (#98) under the home are inside the project there, so `writable::resolve` leaves them out quietly as always: in a home-folder project they're read-only like the rest;
  - `hidden_in_sandbox` accepts a runtime folder under the home only in a hidden top-level entry (not a toolchain's), which this case hides too;
  - known limit: a socket deeper than the scan's bounds, or created in a visible folder while a command runs, isn't masked. Services keep theirs in hidden folders (masked whole) or under `/run` (empty).
- **Protected paths** are a marv-rs addition.
  - **Existing files (#8):** `protected_mounts` binds every protected entry that exists, at any level (`packages/a/AGENTS.md`, `apps/web/.vscode/`), the topmost one only, plus the `core.hooksPath` folder. It uses the scan's walker. A folder that holds a protected file can't be deleted by a command (its mount point is busy), as at the top level before. `.git` folders are left out: an approved `git_write` command must write them. For a command that runs without asking, bash also mounts nested `.git` folders read-only (`nested_git_dirs`); the project's own is `git_guard`'s.
  - **Created files (#8):** `scan_created` compares the project with a `protected_snapshot` taken before the command and finds every new protected entry at any level: a new `.envrc`, a nested `AGENTS.md` or `.git`, a file in a nested `.vscode/`, the missing hooks folder. Only the topmost new entry is reported. `revert_created` removes them (a symlink itself, never its target; nothing whose folder now leads outside the project), and `created_note` tells the model. Chosen over placeholders: placeholders cover only the names at the top level and leave mount points behind.
  - **Wired in bash (#8):** a sandboxed command takes `protected_snapshot` before `run_command`, removes bwrap's placeholders, then `scan_created` + `revert_created`, and appends `created_note` to the output. A new `.git` is kept when the user approved a `git_write` command (`git init`, a submodule); otherwise it goes too. Unsandboxed commands aren't scanned: nothing limits them, and their approval says so.
  - **What the walker skips (`SKIPPED_DIRS`):** `node_modules`, `target` and `.git`, at any level. The entry itself is still seen (a *new* `.git` is reported), but nothing inside it. Why: dependencies and build output are often hundreds of thousands of files, and the walker runs three times per sandboxed command (mounts, snapshot, scan). Marv loads nothing from those folders: `AGENTS.md` and skills come from the project's own folders, and direnv/dotenv read `.envrc`/`.env` in the folder the user works in. A file planted there matters only if something loads it from there, and the user working inside `node_modules` is out of reach for any scan. Known cost: a source folder that happens to be named `target` is skipped too. The guard `protected_files_a_command_creates_are_found_and_reverted` checks that a `.envrc` six folders deep is caught.
  - **Symlinks:** a protected path that is itself a symlink isn't mounted (bwrap can't mount on one). The path rules judge where it leads.
- **Dangling symlinks (#8):** `real_with_missing` walks every symlink, including a final one whose target doesn't exist yet. Before this it canonicalized the nearest existing ancestor, and `exists()` follows symlinks, so a dangling `out.sh -> /elsewhere/x` let `write_file` create a file outside the project, and `innocent.sh -> .envrc` wasn't seen as protected. `resolve_in_project` also refuses a dangling link into `.git` (`IntoGit`).
  - `.git` is left to `git_guard`, because an approved `git_write` command must be able to write it.
- **Taint** is a marv-rs addition.
  - `run_tool` in marv-tools sets `must_ask`.
  - The session's "always" wrapper must honour it.
  - Guard tests are in `tests/guards/` (the `marv-guards` crate, see guards.md).
- **Known limits** (as in Marv):
  - With network on, the sandbox shares the host's network namespace, so abstract sockets are reachable. Such calls always ask.
  - Tokens inside whole config files (`~/.gitconfig`) can't be masked.
  - A path can change between its check and its use (TOCTOU). Planned: `openat2(RESOLVE_BENEATH)`.
