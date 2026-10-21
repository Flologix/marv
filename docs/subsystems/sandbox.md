# marv-sandbox

What commands can touch and which paths an agent may reach. It covers:
- the sandbox's plan and backends (bubblewrap on Linux, Seatbelt on macOS, `docs/specs/2026-10-17-macos-sandbox.md`);
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
| (new, #163) | `plan.rs` | **Done and tested.** `Plan` (was `SandboxOptions`, which stays as bwrap's alias): what one command's sandbox allows, built by `Sandbox::plan`. The policy every backend shares is methods over a `FsProbe`: `read_only_extras`/`writable_extras` (`resolve_extra`, moved here: refused when the home, a parent of it, `/` or (read-only) the project; `Extra { given, real, around_project }`), `home_project`, `project_read_only`, `toolchains`, `hidden_dirs`, `config_dir_hidden`, `credentials`, `mesh_bind`, `home_masks` (moved from both renderers, #166), and `env` (G19's list, plus `MARV_MESH_SOCKET` when plugged, plus the backend's `extra_env`) |
| (new, #163) | `backend.rs` | **Done.** `SandboxBackend` (`name`, `available`, `command` → `SandboxCommand { argv, env, leftovers }`, `guest_path`, `hides`, `kill`, `terminate`, `after`) and `backend()`, which picks the platform's: Seatbelt on macOS, bubblewrap elsewhere. #166: `extra_env`, `processes` (false for Seatbelt: no process door yet, #152), `kill_group` (refuses pid <= 1), `wait_exited` (a child's exit seen without reaping it: `waitid(WNOWAIT)` on Linux, kqueue `NOTE_EXIT` on macOS), and `Prepared::spawned` (a `Prepared` dropped while its command runs kills it before cleaning up) |
| (new, #163) | `fs.rs` | `FsProbe`, `RealFs`, `EntryKind`, `FileKind`, moved out of `bwrap.rs` (re-exported there) so every backend checks paths the same way; tests share one `Fake` (`test_fs.rs`) |
| (new, #164) | `seatbelt.rs` | **Done and tested; probed on macOS** (#166). `profile(plan, invocation, fs) -> Profile { text, params }`: the plan as a deny-default SBPL profile for `/usr/bin/sandbox-exec` (spec §6.2's nine sections, in order: later rules win), `Profile::args` (`-p …`, `-D KEY=VALUE`…). `Invocation` carries what the macOS backend makes per command: the private `TMPDIR`, the user's `/var/folders/<xx>/<id>`, Marv's binary, the forwarded ports. `canonical` resolves each prefix through `FsProbe` (`/tmp` → `/private/tmp`, the missing rest as written). Pure, so its tests run on Linux |
| (new, #166) | `seatbelt_backend.rs` | **Done; guards on the macOS job.** `Seatbelt`: `/usr/bin/sandbox-exec -p <profile> -D… -- <marv> __guard <marv-pid> -- inner…` (`GUARD_SUBCOMMAND`, `argv`), the profile in a file in the TMPDIR over 256 KB (`-f`); a private `TMPDIR` per command, `mkdtemp` (0700) in the user's `T/` from `getconf DARWIN_USER_TEMP_DIR` (never `TMPDIR`), removed by `after` (only a `marv-cmd-*` folder right in `T/`); `kill` = `killpg(SIGKILL)`, `terminate` = SIGTERM to the guard alone; `hides` = `/private/tmp`, the user's temporary folder, the home's hidden entries but toolchains, `~/Library`; caches (`XDG_CACHE_HOME`, `GOCACHE`, `PIP_CACHE_DIR`, `npm_config_cache`, pnpm's `npm_config_store_dir`) in the TMPDIR; `available` runs the base rules and the guard once per guard binary (`set_guard_exe` lets the guards' fixture answer `__guard`; a test binary that doesn't answer leaves Seatbelt unavailable there) |
| `src/sandbox.ts` (`sandboxArgs`, `runMounts`, `resolveExtra`, `sandboxAvailable`) | `bwrap.rs` | **Ported and tested.** #163: `Bwrap`, the `SandboxBackend` over `sandbox_args`, which reads the policy from the `Plan` and keeps only how bwrap spells it (mount order, tmpfs or `/dev/null`, `run_mounts`, the home's socket scan). Adds protected-path read-only binds and the mesh plug at `/run/marv/mesh.sock`. The file system is injectable (`FsProbe`). #24: `binds` (Marv's Sprint 4 `binds`: a mount at a path of its own after the empty `/run`, read-only unless `writable`), for the process door's relay |
| `src/sandbox.ts` (`TOOLCHAINS`, `CREDENTIALS`, `credentialMasks`, `hiddenPathFolders`) | `home.rs` | **Ported and tested.** marv-rs: `HOME_MASKS` and `home_project` for a project that is the home folder (see "The home folder as the project") |
| `src/tools/files.ts` (`resolveInProject`, `insideGitDir`, `touchesGit`, `refuseGit`, `projectPath`) | `paths.rs` | **Ported and tested.** New: `resolve_within`, the realpath containment check for skill files, with a symlink test |
| `src/tools/bash.ts` (`gitGuard`, `worktreeGit`, `worktreeGitDirs`) | `git_guard.rs` | **Ported and tested** |
| `src/tools/bash.ts` (`GIT_HINT`, `HOME_NOTE`, `NETWORK_NOTE`, `GH_NOTE`, `GH_TOOL_NOTE`, `BACKGROUND_NOTE`, `BACKGROUND_NOTE_PROCESS`, `touchesHiddenHome`, `backgroundsProcess`, `looksLikeNetworkFailure`) | `notes.rs` | **Ported and tested** (regex has no lookahead, so the word ends are checked by hand, and `backgrounds_process`'s `&` lookahead is a consuming group). The background notes and `backgrounds_process` (Marv #50, #51) came with #24. marv-rs #81: `ESCALATED_NOTE`, on a retry bash ran with network because the same command already got `NETWORK_NOTE` |
| new (`docs/SECURITY.md`) | `protected.rs` | **Done and tested:** `PROTECTED_PATHS`, `rule_for_relative`, `protected_rule`/`is_protected` (realpath, a dangling final symlink followed, fails closed), `hooks_path`, `home_rule` (when the project is the home folder or holds it: the home's hidden top-level entries and the `core.hooksPath` folders of the global git config and of each repository in the home are `HomeDotfile`, since `edit_file`/`write_file` write there outside the sandbox; guard `in_the_home_folder_dotfiles_and_hooks_ask_even_in_yolo`), `lifecycle_scripts_changed`, `protected_mounts` (top level only), and the post-command scan: `protected_snapshot`, `scan_created`, `revert_created`, `created_note` (#8); `is_secret_name` (docs/SECURITY.md's secret names: `.env*` but `.envrc`, `*.pem`, `id_*`; used by instruction files, #62; the read_file gate is still to come) |
| `src/tools/bash.ts` (`secretVariable`, `hostEnv`) | `host_env.rs` | **Ported and tested** (marv-rs #23, Marv #52; `host-env.test.ts`): the environment of what runs on the host, Marv's keys and secret-looking variables stripped |
| new (`docs/SECURITY.md`) | `taint.rs` | **Done and tested:** `TaintState` (`mark`, `is_tainted`, `must_ask`, approved prefixes, `for_subagent`) |
| (new, #150/#162) | `runtime.rs` | **Done and tested:** Marv's runtime folder, one rule: `runtime_dir(env)` (checked: Linux `$XDG_RUNTIME_DIR/marv` when `hidden_in_sandbox` agrees; macOS `/tmp/marv-<uid>`, made 0700 and owner/symlink checked; `MARV_RUNTIME_DIR` on either, absolute, at most `MAX_RUNTIME_DIR` bytes, parent existing and judged by its real path, hidden, no symlink or other user's folder on the way (but `/tmp` and a root-owned link in a root-only folder, followed one hop at a time with its target walked too: `check_ancestors`, tested through a fake `Probe`), last part made 0700 non-recursively; `runtime_dir_for(env, Some(root))`/`override_refusal_for` also refuse an override inside the session's project or holding it (given, parent-real and own real paths), #168; only the session's own project: an override inside another project is still accepted, see SECURITY.md), `runtime_paths(env)` (the default and any override, unchecked: what the sandbox hides and `writable` refuses), `check_socket_path` (refuses a path over `sun_path`: 104 on macOS, 108 on Linux), `RUNTIME_DIR_RULE` for messages. `hidden_in_sandbox` compares against the real `/tmp` and `/run` (macOS: `/private/tmp`) |
| (new, #98) | `writable.rs` | **Done and tested:** the config's `"writable"` resolved once at startup (`resolve` → canonical paths + startup problems; `~` expanded, symlinks followed, the rules judged on the canonical path), and the refuse list (`/`, the home and its parents, the project's parents, any hidden top-level home entry (`~/.<anything>` — the message names the known ones, the rule doesn't need the list), a git repository (the entry is one, under a `.git` component, or directly holding a `.git` entry, worktree file included; a repo planted deeper inside an accepted folder later is *not* scanned — stated in the doc), the mesh's runtime sockets (the runtime folder, `runtime::runtime_path`, inside it or holding it), Marv's config folder (`config_dir`, wherever `MARV_CONFIG_DIR` puts it), missing/non-folders); an entry in the project or inside another accepted entry is left out quietly (#119 review: a swapped parent would let bwrap bind a symlink's target) |
| — | `lib.rs` | `Sandbox` (the session's handle: `from_env`, `active`, `backend`, `plan`, `prepare` (#163: the `Prepared` command callers spawn; it replaced `command` and `command_argv`); #98: `writable`, the session's extra writable folders, merged under each command's own), `CommandMounts` (with `binds`, #24), `SandboxError` |

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
    pub fn prepare(&self, root: &Path, mounts: &CommandMounts, inner: &[OsString]) -> Result<Prepared, SandboxError>;
}
impl SandboxCommand { pub fn to_command(&self, root: &Path) -> std::process::Command; }  // env_clear + env + current_dir
pub struct Prepared;  // Deref<Target = SandboxCommand>; finish() runs the backend's after() once, and Drop does if nothing did
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
  - bash, the process manager and the guards spawn `Sandbox::prepare`'s command only through `SandboxCommand::to_command`, which clears the environment and sets the backend's (`PATH` for bwrap, which sets the command's own with `--setenv`; `sandbox-exec` passes its environment on, so there a spawn that kept Marv's would leak API keys and `DYLD_*`). There is no argv-only API left to spawn by hand;
  - `Prepared::finish` runs the backend's `after` once the command has ended (bwrap: the placeholder mount points, removed while empty; bash no longer does this itself, and finishes before its scan for created protected files). Dropping a `Prepared` finishes it too, so a spawn that fails or an early return leaves nothing behind;
  - a timeout, a stop or Marv's exit kills through the backend (`kill`, `terminate`; bwrap: SIGKILL/SIGTERM to bwrap, whose PID namespace takes the rest down);
  - the relay's binary and socket paths are `guest_path` of their binds (`relay::sandbox_argv_at`), not `/run/marv-bin` and `/run/marv` assumed;
  - a plugged command gets `MARV_MESH_SOCKET` with its plug's path inside (bwrap: `/run/marv/mesh.sock`); `marv_mesh::plug::sandbox_socket` reads it, falling back to `SANDBOX_SOCKET`. Seatbelt can't mount, so there it will be the host path;
  - the runtime folder's check asks the backend (`backend().hides`, #166): bwrap's `hidden_in_sandbox`, Seatbelt's own rule on macOS.
  The spec's sketch stores the resolved values as `Plan` fields; they are methods here, so a hand-built `Plan` (the bwrap tests, unchanged) can't disagree with its resolved view, and `Sandbox::plan` can't fail (every refusal happens in the backend, before anything starts).
- **The Seatbelt profile (#164).** Every path is a `-D` parameter, canonical (`seatbelt::canonical` walks the path through `FsProbe::realpath` a prefix at a time, so the `Fake`'s `/tmp → /private/tmp` link is enough to test it on Linux); only the protected-name regexes and the home-project regex put a path in the text, its regex metacharacters escaped, and refused with `"`, `\`, a control character or when not UTF-8 (the #166 probe: `\.` reaches the regex as an escape, but `\"` doesn't escape a quote, which ends the literal; so the refusal stays). Choices the spec left to the code:
  - a home-folder project also denies `~/Library` (keychains, cookies, mail, app data);
  - hard links (`HARD_LINK_OP`, `file-link`): the #166 probe showed `link()` is checked against both the source and the new name, so `ALLOW_LINKS_IN_PROJECT` is on: they're allowed in the project and the private TMPDIR, after a deny out of the home and the shared temporary folders; everything denied above (Library, masks, Marv's folders, credentials, protected paths, read-only extras) and protected names at any depth (`name_rule`'s twin) are denied again either way. Tests cover both settings;
  - the user's `/private/var/folders/<xx>/<id>` (exactly that shape) comes from `Invocation::user_temp` or the private TMPDIR's path; otherwise the profile is refused. The private TMPDIR must be in its `T/` (never `T` itself, `C/` or `0/`), and neither inside nor around the home, the project, `/private/tmp`, a runtime folder, the config folder or an extra; the backend must create it fresh (#166);
  - a runtime folder is denied whether it exists yet or not (bwrap can only hide one that exists);
  - the `.git` name rule applies when the plan holds the project's `.git` read-only or as a placeholder (bash's git guard, i.e. a command that runs without asking), so the plan needs no new field;
  - protected-name rules skip `node_modules`, `target` and `.git`, as the after-scan does, so installs still write `node_modules/pkg/AGENTS.md`; names are case-insensitive, `.git`, `.marv`, `.vscode`, `.idea` included;
  - a read-only extra inside a write-allowed folder gets a write deny after it; one around the project gets none (it would cover the project);
  - Unix sockets may be bound and connected only inside the private `TMPDIR`, which is the command's own; `system-socket` is allowed for `AF_UNIX`, `AF_INET` and `AF_INET6`, since creating a socket reaches nothing; setuid programs need no rule: `sandbox-exec` refuses their exec (#166 probe);
  - `binds` (the process door's relay) are refused: the door is off on macOS until #152;
  - the environment (`path`, `lang`) isn't in the profile: the macOS backend sets it on the spawn (#166), and so the profile-file fallback over 256 KB (`-f`);
  - #166 additions from the probes: `machdep.cpu.brand_string`, `hw.model`, `kern.ngroups` and the `sysctl.` metadata names (node's `os.cpus()`, `sysctl -n`), `net.routetable.` with network only (the interface list); each runtime folder's Unix sockets denied by name (connect and bind), before the plug's allow; the folders above each folder shown again (the TMPDIR, the project, toolchains, extras, Marv's binary), where section 3 closed them, readable by their own metadata only (`PARENT_n`, `literal`): npm walks up its cache's path with `lstat`.
  The "to verify" items of the spec were probed on the macOS runner (`tests/seatbelt_probe.rs`, results on #166).
- **`marv __guard` (#165, spec §6.10) stands in for bwrap's PID namespace, `--die-with-parent` and `--new-session` where there are none (Seatbelt).** It lives in marv-tools (`crates/marv-tools/src/guard.rs`, moved from marv-cli by #166 so the guards' fixture can answer `__guard` too), a mode of the binary like `__relay`, in Seatbelt's argv: `/usr/bin/sandbox-exec -p … -- <marv> __guard <marv-pid> -- bash -c …`. It runs inside the sandbox, so nothing new runs unsandboxed. What it does:
  - `setsid()`: a new session and process group whose id is the guard's pid, which (exec keeps it) is the pid Marv spawned, so the backend's `kill` is `killpg(pid, SIGKILL)` and takes the guard, the command and everything still in the group. Without a session of its own it runs nothing (exit 127): spawned as a group leader (`process_group(0)`) it can't `setsid`, and the command would keep Marv's session and controlling terminal, so #166 must not spawn it that way;
  - refuses to start the command (exit 127) when its parent isn't the pid it was given, then polls `getppid()` every 200 ms and SIGKILLs its whole group, itself included, as soon as it changes: Marv exited, panicked or was SIGKILLed. Not `PR_SET_PDEATHSIG` on Linux: it fires when the spawning *thread* exits, and Tokio's threads come and go;
  - passes SIGTERM, SIGINT and SIGHUP on to the group, and every other catchable signal whose default would end the guard and leave the command unwatched (SIGQUIT, SIGUSR1/2, SIGALRM, SIGVTALRM, SIGPROF, SIGXCPU, SIGXFSZ: caught, not ignored, since an ignored signal survives exec), skipping its own copy of each one it sent (it's in the group). So `terminate` should signal the guard's pid alone (`kill`, as bwrap's does): a `killpg(pid, SIGTERM)` would reach the command twice;
  - exits with the command's code, 128 + n for signal n (`marv_tools::relay::exit_code`, how bash results report it), 127 when it can't start it.
  After a normal exit the guard doesn't kill what the command left in the background (it can't SIGKILL its group without ending itself first); bwrap's namespace does. So bash (#166) watches the guard's exit without reaping it (`wait_exited`), then kills the group through the backend before draining the pipes (which leftovers would hold open) and before reaping, so the group's id can't have been reused; and a dropped command kills its group (a guard declared after the child, dropped first). **Known limit** (spec §11, decision 5, docs/SECURITY.md): a process that leaves the group (`setsid`, `setpgid`) and double-forks is out of reach and can outlive the command, still sandboxed. No process-tree sweep. And the command can SIGKILL or SIGSTOP its own guard (same sandbox, same group): the group then outlives a Marv that crashes or is SIGKILLed; a Marv that exits normally or times the command out still `killpg`s it.
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
