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
| `src/sandbox.ts` (`sandboxArgs`, `runMounts`, `resolveExtra`, `sandboxAvailable`) | `bwrap.rs` | **Ported and tested.** Adds protected-path read-only binds and the mesh plug at `/run/marv/mesh.sock`. The file system is injectable (`FsProbe`) |
| `src/sandbox.ts` (`TOOLCHAINS`, `CREDENTIALS`, `credentialMasks`, `hiddenPathFolders`) | `home.rs` | **Ported and tested** |
| `src/tools/files.ts` (`resolveInProject`, `insideGitDir`, `touchesGit`, `refuseGit`, `projectPath`) | `paths.rs` | **Ported and tested.** New: `resolve_within`, the realpath containment check for skill files, with a symlink test |
| `src/tools/bash.ts` (`gitGuard`, `worktreeGit`, `worktreeGitDirs`) | `git_guard.rs` | **Ported and tested** |
| `src/tools/bash.ts` (`GIT_HINT`, `HOME_NOTE`, `NETWORK_NOTE`, `GH_NOTE`, `touchesHiddenHome`, `looksLikeNetworkFailure`) | `notes.rs` | **Ported and tested** (regex has no lookahead, so the word ends are checked by hand) |
| new (`docs/SECURITY.md`) | `protected.rs` | **Done and tested:** `PROTECTED_PATHS`, `rule_for_relative`, `protected_rule`/`is_protected` (realpath, fails closed), `hooks_path`, `lifecycle_scripts_changed`, `protected_mounts` (top level only) |
| new (`docs/SECURITY.md`) | `taint.rs` | **Done and tested:** `TaintState` (`mark`, `is_tainted`, `must_ask`, approved prefixes, `for_subagent`) |
| — | `lib.rs` | `Sandbox` (the session's handle: `from_env`, `active`, `command`), `CommandMounts`, `SandboxError` |

No `todo!` is left in this crate. The gaps are listed below.

## Interfaces other crates use

```rust
pub struct Sandbox { pub enabled: bool, pub home: PathBuf, pub path: String, pub lang: String, pub mesh_socket: Option<PathBuf> }
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

pub struct TaintState;  // mark(Provenance) -> bool, is_tainted(), sources(), must_ask(ToolKind, uses_network), approve_prefix, prefix_approved, for_subagent
pub fn git_guard::git_guard(root: &Path) -> GitGuard;  // { read_only, placeholders }
pub fn home::hidden_path_folders(path: &str, home: &Path, root: Option<&Path>, exists: &dyn Fn(&Path) -> bool) -> Vec<PathBuf>;
pub fn notes::{looks_like_network_failure, runs_gh, touches_hidden_home}; pub const notes::{GIT_HINT, HOME_NOTE, NETWORK_NOTE, GH_NOTE};
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
  7. writable folders;
  8. protected paths (skipped when the project is read-only);
  9. placeholders;
  10. credential masks, last.
- **Protected paths** are a marv-rs addition.
  - **Gap:** `protected_mounts` binds only the top-level protected entries that exist, plus the `core.hooksPath` folder. A command can still create a new `.envrc`, or an `AGENTS.md` deeper in the tree.
  - **Fix planned:**
    - placeholders for the top-level names;
    - a scan after each command for new protected files, which then need approval to keep.
  - `.git` is left to `git_guard`, because an approved `git_write` command must be able to write it.
- **Taint** is a marv-rs addition.
  - `run_tool` in marv-tools sets `must_ask`.
  - The session's "always" wrapper must honour it.
  - Guard tests go in `tests/guards/`, which belongs to the workspace, not this crate.
- **Known limits** (as in Marv):
  - With network on, the sandbox shares the host's network namespace, so abstract sockets are reachable. Such calls always ask.
  - Tokens inside whole config files (`~/.gitconfig`) can't be masked.
  - A path can change between its check and its use (TOCTOU). Planned: `openat2(RESOLVE_BENEATH)`.
