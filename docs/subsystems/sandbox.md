# marv-sandbox

What commands can touch and which paths an agent may reach. It covers:
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
| `src/sandbox.ts` (`sandboxArgs`, `runMounts`, `resolveExtra`, `sandboxAvailable`) | `bwrap.rs` | **Ported and tested.** Adds protected-path read-only binds and the mesh plug at `/run/marv/mesh.sock`. The file system is injectable (`FsProbe`). #24: `binds` (Marv's Sprint 4 `binds`: a mount at a path of its own after the empty `/run`, read-only unless `writable`), for the process door's relay |
| `src/sandbox.ts` (`TOOLCHAINS`, `CREDENTIALS`, `credentialMasks`, `hiddenPathFolders`) | `home.rs` | **Ported and tested** |
| `src/tools/files.ts` (`resolveInProject`, `insideGitDir`, `touchesGit`, `refuseGit`, `projectPath`) | `paths.rs` | **Ported and tested.** New: `resolve_within`, the realpath containment check for skill files, with a symlink test |
| `src/tools/bash.ts` (`gitGuard`, `worktreeGit`, `worktreeGitDirs`) | `git_guard.rs` | **Ported and tested** |
| `src/tools/bash.ts` (`GIT_HINT`, `HOME_NOTE`, `NETWORK_NOTE`, `GH_NOTE`, `GH_TOOL_NOTE`, `BACKGROUND_NOTE`, `BACKGROUND_NOTE_PROCESS`, `touchesHiddenHome`, `backgroundsProcess`, `looksLikeNetworkFailure`) | `notes.rs` | **Ported and tested** (regex has no lookahead, so the word ends are checked by hand, and `backgrounds_process`'s `&` lookahead is a consuming group). The background notes and `backgrounds_process` (Marv #50, #51) came with #24. marv-rs #81: `ESCALATED_NOTE`, on a retry bash ran with network because the same command already got `NETWORK_NOTE` |
| new (`docs/SECURITY.md`) | `protected.rs` | **Done and tested:** `PROTECTED_PATHS`, `rule_for_relative`, `protected_rule`/`is_protected` (realpath, a dangling final symlink followed, fails closed), `hooks_path`, `lifecycle_scripts_changed`, `protected_mounts` (top level only), and the post-command scan: `protected_snapshot`, `scan_created`, `revert_created`, `created_note` (#8); `is_secret_name` (docs/SECURITY.md's secret names: `.env*` but `.envrc`, `*.pem`, `id_*`; used by instruction files, #62; the read_file gate is still to come) |
| `src/tools/bash.ts` (`secretVariable`, `hostEnv`) | `host_env.rs` | **Ported and tested** (marv-rs #23, Marv #52; `host-env.test.ts`): the environment of what runs on the host, Marv's keys and secret-looking variables stripped |
| new (`docs/SECURITY.md`) | `taint.rs` | **Done and tested:** `TaintState` (`mark`, `is_tainted`, `must_ask`, approved prefixes, `for_subagent`) |
| (new, #98) | `writable.rs` | **Done and tested:** the config's `"writable"` resolved once at startup (`resolve` → canonical paths + startup problems; `~` expanded, symlinks followed, the rules judged on the canonical path), and the refuse list (`/`, the home and its parents, the project's parents, any hidden top-level home entry (`~/.<anything>` — the message names the known ones, the rule doesn't need the list), a git repository (the entry is one, under a `.git` component, or directly holding a `.git` entry, worktree file included; a repo planted deeper inside an accepted folder later is *not* scanned — stated in the doc), the mesh's runtime sockets (`marv/` in the runtime dir, inside it or holding it), Marv's config folder (`config_dir`, wherever `MARV_CONFIG_DIR` puts it), missing/non-folders) |
| — | `lib.rs` | `Sandbox` (the session's handle: `from_env`, `active`, `command`, `command_argv` for an inner command other than `bash -c`; #98: `writable`, the session's extra writable folders, merged under each command's own), `CommandMounts` (with `binds`, #24), `SandboxError` |

No `todo!` is left in this crate. The gaps are listed below.

## Interfaces other crates use

```rust
pub struct Sandbox { pub enabled: bool, pub home: PathBuf, pub path: String, pub lang: String, pub mesh_socket: Option<PathBuf>, pub hidden: Vec<PathBuf>, pub writable: Vec<PathBuf> }  // writable: the #98 extras
pub fn writable::resolve(entries: &[String], home: &Path, root: &Path, runtime: Option<&Path>, config_dir: Option<&Path>) -> (Vec<PathBuf>, Vec<String>);  // accepted + problems
impl Sandbox {
    pub fn from_env(enabled: bool) -> Self;
    pub fn active(&self) -> bool;                                    // enabled && bwrap works here
    pub fn command(&self, root: &Path, mounts: &CommandMounts, script: &str) -> Result<Vec<OsString>, SandboxError>;
}
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
  10. credential masks, last.
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
