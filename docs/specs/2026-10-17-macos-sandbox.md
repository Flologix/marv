# macOS sandbox: a Seatbelt backend next to bubblewrap

- Issues: FlologixAI/marv #150 (macOS port), #151 (Seatbelt backend), #152 (processes and the port relay)
- Status: **draft, for approval.** Nothing is implemented until this is approved.
- Date: 2026-10-17
- Scope: `crates/marv-sandbox` (a backend trait, the Seatbelt profile builder, the runtime folder), `crates/marv-tools` (bash, the relay), `crates/marv-session` (processes, mesh start), `crates/marv-mesh` (discovery, switchboard, plug), `crates/marv-cli` (messages, a `__guard` subcommand), `tests/guards`, `.github/workflows/ci.yml`, `docs/SECURITY.md`.

This spec is grounded in the Rust code on `feat/150-macos` (`06030e1`, the macOS CI job). OpenAI Codex's Seatbelt code (`codex-rs/sandboxing/src/seatbelt.rs` and its `.sbpl` files, read 2026-10-17) is used as the reference for what works in practice; where this spec follows it, it says so. Claims about macOS behaviour that the code can't show (how Seatbelt resolves paths, what `localhost` filters match) are marked **to verify**: each one gets a test on the macOS runner before the code relies on it (§9).

## 1. Problem and evidence

Marv's sandbox is bubblewrap (`crates/marv-sandbox/src/bwrap.rs`). Every bash command, every background process and every project check runs inside it, and `docs/SECURITY.md` is written around what it gives. macOS has no bubblewrap, no mount namespaces and no `/proc`.

The macOS CI job (`ci.yml:87-96`, `macos-14`, not required) shows where things stand:

1. **The workspace compiles.** Nothing in the code is Linux-only at the type level.
2. **63 tests fail**, none of them in the sandbox (those skip: `require_bwrap!`, `tests/guards/src/lib.rs:39`):

| Failures | Cause | Where |
|---|---|---|
| 43 | `own_uid()` reads the owner of `/proc/self`, which doesn't exist | `crates/marv-mesh/src/discovery.rs:133-135`, used by `private_folder` (:138), `check_socket` (:148) and `list` (:190) |
| 6 | Unix socket paths too long: tests build runtime folders under `std::env::temp_dir()`, which on macOS is `$TMPDIR` = `/var/folders/xx/<28 chars>/T/` (49 bytes), and `sun_path` is 104 bytes | the mesh and switchboard tests; the same would hit a real `XDG_RUNTIME_DIR`-less macOS run |
| 9 | `/var` is a symlink to `/private/var`: a canonical path is compared with one that isn't | `hidden_in_sandbox` (`lib.rs:45-62`) compares a canonical `dir` with literal `/tmp` and `/run`; test fixtures compare `temp_dir()` paths with canonical ones |
| 3 | Test fixtures assume GNU tools: BSD `wc -l` pads its number with spaces, and there is no `setsid` binary | `tests/guards/tests/mesh.rs:223`, `radio.rs:457`, the process fixtures |

So `$TMPDIR` is ruled out as the mesh's base folder, the uid needs another source, and every path comparison has to be canonical on both sides. None of that is the sandbox itself, which is the bulk of this spec.

## 2. Goals and non-goals

**Goals**

1. **The same rules on macOS.** Every guarantee in §3 is either met by the Seatbelt backend or listed in §6.12 with its mitigation, and that list goes into `docs/SECURITY.md`.
2. **One policy, two renderers.** What a command may see, write and reach is decided once, in shared code; bubblewrap and Seatbelt only render it. A rule added later can't be forgotten on one platform.
3. **Testable without a Mac.** The Seatbelt profile is built by a pure function over `FsProbe`, as `sandbox_args` is today, so its unit tests run on Linux CI too.
4. **The guards run on macOS.** `tests/guards` runs on the `macos-14` job with `MARV_REQUIRE_SANDBOX=1`, and the job becomes required.
5. **Fail closed.** No Seatbelt, no sandbox: the same path as a Linux machine without bwrap.

**Non-goals**

- Windows.
- The App Sandbox or Endpoint Security. Both need signing, entitlements or a system extension; Marv is a CLI.
- A full answer for background processes on macOS. §8 sets out the options; #152 decides.
- Making Linux behave differently. The refactor in §5 changes no bwrap argument.

## 3. What the bubblewrap backend guarantees today

### 3.1 The pieces

| Piece | What it does | Where |
|---|---|---|
| `FsProbe`, `RealFs` | The file system, injectable for tests: `entries`, `entry_kind`, `exists`, `kind`, `realpath` | `bwrap.rs:70-130` |
| `SandboxOptions` | One invocation's mounts: root, home, network, PATH, LANG, read-only and writable extras, placeholders, project read-only, protected paths, mesh socket, binds, hidden folders, config dir | `bwrap.rs:134-170` |
| `sandbox_args` | Renders the options as bwrap arguments | `bwrap.rs:239-381` |
| `sandbox_available` | Probes `bwrap --ro-bind / / --unshare-net --unshare-pid -- true` once per process | `bwrap.rs:509-519` |
| `Sandbox` | The session's sandbox: enabled, home, PATH, LANG, mesh socket, hidden, writable, config dir | `lib.rs:100-120` |
| `Sandbox::from_env` | Reads `HOME`, `PATH`, `LANG`, `XDG_RUNTIME_DIR` (hidden = `$XDG_RUNTIME_DIR/marv`) and `MARV_CONFIG_DIR` from the real environment | `lib.rs:136-152` |
| `Sandbox::active` | Enabled and `sandbox_available()` | `lib.rs:155-157` |
| `Sandbox::command`, `command_argv` | Merge the session's sandbox with one command's `CommandMounts` into `SandboxOptions`, add `protected_mounts`, return `bwrap <args> -- inner…` | `lib.rs:161-197` |
| `CommandMounts` | One command's extras: network, read-only, writable, placeholders, project read-only, binds | `lib.rs:124-132` |
| `hidden_in_sandbox` | Whether every sandbox hides a folder (under `/run`, `/tmp`, or a hidden top-level home entry that isn't a toolchain): the mesh lives only in such a folder | `lib.rs:45-62` |
| `TOOLCHAINS`, `CREDENTIALS`, `HOME_MASKS` | What the stand-in home shows again, what is masked inside it, and what stays hidden even in a home-folder project | `home.rs:21-64` |
| `home_project`, `credential_masks` | Whether the project is the home or holds it; where `/dev/null` goes for a (possibly symlinked) credential file | `home.rs:69-72`, `home.rs:104-122` |
| `protected_mounts`, `nested_git_dirs` | Existing protected paths at any level, and nested `.git` folders, mounted read-only | `protected.rs:277`, `protected.rs:306` |
| `protected_snapshot`, `scan_created`, `revert_created` | Protected files a command *created* (bwrap can only mount what exists), found after it and removed | `protected.rs:386`, `:410`, `:431` |
| `git_guard`, `worktree_git_dirs` | `.git` read-only (its symlink target, a worktree's records) or an empty read-only placeholder when missing | `git_guard.rs:27-51`, `:64` |
| `writable::resolve` | The config's `"writable"` entries, refused at startup unless plainly safe (not the home or a parent, not a dotfile tree, not a repository, not the runtime or config folder) | `writable.rs:65-128`, `:166` |
| `host_env`, `host_program` | The environment and program lookup for what runs *outside* the sandbox (host bash, gh, herdr) | `host_env.rs:26-47` |
| `resolve_in_project`, `real_with_missing` | Every path a tool gets is checked after `realpath`, dangling links included | `paths.rs:51`, `:116` |

### 3.2 The guarantees, one by one

Numbered so §6 can map each.

| # | Guarantee | How bwrap does it | Where |
|---|---|---|---|
| G1 | The system is visible but read-only | `--ro-bind / /`, `--dev /dev`, `--proc /proc` | `bwrap.rs:254-262` |
| G2 | A private `/tmp` | `--tmpfs /tmp`; `TMPDIR=/tmp` | `:263`, `:373` |
| G3 | `/run` is empty (D-Bus, agents, keyring, docker sockets gone), except PATH folders under it and, with network, the DNS config | `run_mounts` | `:210-236`, `:264` |
| G4 | The mesh plug is the one socket shown, at `/run/marv/mesh.sock` | `--bind <plug> /run/marv/mesh.sock` | `:267-269`; `plug.rs:34` |
| G5 | The process door's relay: Marv's binary at `/run/marv-bin`, socket folders under `/run/marv` | `binds` | `:270-273`; `relay.rs:35-38`, `:78` |
| G6 | A stand-in home: empty and writable (RAM), lost after the command | `--tmpfs $HOME` | `:263` |
| G7 | Toolchains shown again read-only | `TOOLCHAINS` re-bound | `:274-277`; `home.rs:21-37` |
| G8 | Extra read-only folders (a worktree's shared `.git`), one around the project mounted first | `resolve_extra`, refusing the home, its parents and the project | `:182-206`, `:280-293` |
| G9 | The project writable, or read-only (`project_read_only`, Marv's own checks) | `--bind` / `--ro-bind` root | `:288-290` |
| G10 | Extra writable folders (#98, approved worktree records) | `--bind` | `:294-297`; `writable.rs` |
| G11 | Protected paths read-only on top of the project | `--ro-bind` each | `:298-302`; `protected.rs:277` |
| G12 | `.git` that doesn't exist can't be created (placeholder) | `--tmpfs p --remount-ro p`, `rmdir` after | `:303-306`; `bash.rs:600-603` |
| G13 | Protected files a command creates are found and removed | `scan_created` after the command | `bash.rs:584`, `:604-614` |
| G14 | Marv's runtime folder is empty wherever it is | `--tmpfs` over each `hidden` | `:314-327` |
| G15 | Marv's config folder is hidden wherever it is, `skills/` shown again read-only | `--tmpfs`, `--ro-bind skills` | `:332-345` |
| G16 | Home-folder project: the home read-only; every hidden top-level entry hidden; toolchains shown again; `HOME_MASKS` hidden again; every socket or FIFO a bounded scan finds replaced by `/dev/null` | `home_masks` | `:346-348`, `:420-506` |
| G17 | Credential files inside toolchains read as empty (symlink targets too) | `--ro-bind /dev/null`, last | `:350-360`; `home.rs:104-122` |
| G18 | No network unless the call asked; the sandbox still has its own loopback | `--unshare-net` | `:361-363` |
| G19 | A cleared environment: only PATH, HOME, LANG, TERM=dumb, TMPDIR, `BUN_INSTALL_CACHE_DIR` | `--clearenv`, `--setenv` | `:365-378` |
| G20 | Its own PID namespace: can't see or signal the user's processes, and killing bwrap kills everything | `--unshare-pid` | `:366` |
| G21 | Dies with Marv, even on SIGKILL | `--die-with-parent` (`PR_SET_PDEATHSIG`) | `:366`; `processes/mod.rs:29-31` |
| G22 | Can't inject keystrokes into Marv's terminal (`TIOCSTI`) | `--new-session` | `:366` |
| G23 | Runs in the project | `--chdir` | `:379` |
| G24 | A mount that can't be made safely means the command never starts | `SandboxError::Mount` → `[sandbox failed to start: …]` | `bash.rs:183-188` |

### 3.3 Where it's used

- **bash** (`crates/marv-tools/src/bash.rs`): `run_command` (:149-255) builds the argv through `Sandbox::command` or, while processes run and the call has no network, `command_argv` with `relay_for` (:98-117). It spawns with `env_clear()` and PATH only (:189-191). A sandboxed command is killed with `child.start_kill()`, trusting bwrap to take the namespace down; an unsandboxed one gets `process_group(0)` and `killpg` (:199-200, :219-229). The tool adds `git_guard` (:562), nested `.git` read-only (:579-582), the protected snapshot before and the scan after (:584, :604-614), and the notes (stand-in home, network, background).
- **Processes** (`crates/marv-session/src/processes/mod.rs`): each process is a long-lived bwrap with its own network namespace; the relay inside listens on `/run/marv/fwd.sock` (:410-424). Marv finds the sandbox's first pid in `/proc/<bwrap>/task/<bwrap>/children` (:164-166), reads its listening ports from `/proc/<pid>/net/tcp{,6}` (:598-640), and with network, filters by socket inodes found under `/proc/*/fd` in the same PID namespace (:169-189). Each port is forwarded from the host's `127.0.0.1` only (:645-680). Stop is SIGTERM then SIGKILL to bwrap (:521-548); `kill_running` SIGKILLs all on exit (:199-203). The socket folder is `$XDG_RUNTIME_DIR/marv/<pid>-<6 hex>/p<n>`, else `temp_dir()/marv-<uid>/marv` (:266-297).
- **The mesh**: `mesh_dir` is `$XDG_RUNTIME_DIR/marv/mesh`, only when `hidden_in_sandbox` agrees (`discovery.rs:71-75`); plugs live in `$XDG_RUNTIME_DIR/marv/plugs/<session>` (`marv-session/src/mesh.rs:69-76`), made 0700 (`switchboard.rs:326-335`), each socket 0600 (:339-349). A subagent's sandbox gets its plug at `/run/marv/mesh.sock` (`mesh.rs:390`). Discovery refuses a folder that isn't 0700 and the user's, and a socket that isn't 0600 and the user's (`discovery.rs:138-154`, :190). A door checks a caller by `peer_cred().pid()` against the process listening on `<from>.sock`, never by the message (:228, :407).
- **The writable refusal** treats `$XDG_RUNTIME_DIR/marv` as off-limits (`writable.rs:114-119`).
- **Guards**: `tests/guards/tests/sandbox.rs`, `process.rs`, `mesh.rs`, `radio.rs` and others run real bwrap and skip without it (`require_bwrap!`). `where_the_sandbox_is_required_it_really_runs` (`sandbox.rs:21-26`) fails when `MARV_REQUIRE_SANDBOX=1` and the sandbox can't start; the Linux job sets it (`ci.yml:28`), the macOS job doesn't yet.

## 4. Seatbelt in one page

Seatbelt is the kernel sandbox behind the App Sandbox. `/usr/bin/sandbox-exec -p <profile> [-D KEY=VALUE…] -- cmd…` applies a profile written in SBPL (a Scheme dialect) to `cmd` and everything it starts; a child can't leave it or loosen it. The rules that matter here:

- **Deny by default, or allow by default.** `(deny default)` refuses every operation no rule allows; `(allow default)` the reverse.
- **The last matching rule wins.** A later `deny` overrides an earlier `allow` for the same operation and path, and the other way round. Codex depends on this ("Keep these denies last so no broader allowance can reopen…"), and so does this design: the profile is built in a fixed section order (§6.2).
- **Paths are real paths.** Rules match the resolved path of the file being opened, so `/tmp/x` is seen as `/private/tmp/x` and `/var/folders/…` as `/private/var/folders/…`. Every path in a rule must be canonical. Firmlinks (`/Users` → `/System/Volumes/Data/Users`) are transparent: the path seen is `/Users/…`, which is what `realpath` returns. **To verify** on the runner (§9.2).
- **Filters:** `(literal p)`, `(subpath p)` (p and everything under it), `(regex #"…")`, and `(param "KEY")` for a value passed with `-D`. `require-all`, `require-any` and `require-not` combine them.
- **Operations used here:** `file-read*`, `file-write*` (create, data, unlink, rename, mode, …), `file-map-executable`, `process-exec`, `process-fork`, `signal`, `process-info*`, `sysctl-read`, `mach-lookup` (XPC and Mach services), `network-outbound`, `network-inbound`, `network-bind` (with `(remote ip "host:port")`, `(local ip …)`, `(remote unix-socket (path-literal …))`), `pseudo-tty`, `system-fcntl`.

There is no mount, no tmpfs, no namespace. Everything bwrap does by *showing something else at a path* has to become *refusing the path*.

## 5. A `SandboxBackend` trait

### 5.1 Shape

The bwrap code mixes two jobs: deciding the policy (which folders, after which checks) and spelling it as arguments. The policy half moves to shared code; each backend renders it.

```rust
/// What one command's sandbox allows, decided once for every backend. Built by `Sandbox::plan` from the session's
/// `Sandbox` and the command's `CommandMounts` (today's `command_argv`, lib.rs:166-191). All paths absolute;
/// `real_*` fields canonical (`FsProbe::realpath`).
pub struct Plan {
    pub root: PathBuf, pub real_root: PathBuf,
    pub home: PathBuf, pub real_home: PathBuf,
    pub home_project: bool,              // home.rs:69
    pub project_read_only: bool,
    pub network: bool,
    pub path: String, pub lang: String,
    pub toolchains: Vec<PathBuf>,        // existing TOOLCHAINS under the home
    pub read_only: Vec<Extra>,           // resolve_extra'd: real path, given path, around-the-project
    pub writable: Vec<Extra>,
    pub protected: Vec<PathBuf>,         // protected_mounts
    pub placeholders: Vec<PathBuf>,      // paths that must stay absent
    pub hidden: Vec<Hide>,               // runtime folder, config folder, home-project masks: path + Dir|File
    pub shown_again: Vec<PathBuf>,       // config skills, toolchains under a hidden entry
    pub credentials: Vec<PathBuf>,       // credential_masks, deduplicated
    pub mesh_socket: Option<PathBuf>,
    pub binds: Vec<Bind>,
    pub env: Vec<(String, String)>,      // G19's list
}

pub trait SandboxBackend: Send + Sync + std::fmt::Debug {
    /// "bubblewrap" or "Seatbelt", for notes and `/sandbox`.
    fn name(&self) -> &'static str;
    /// Whether it can start a sandbox here (probed once, as `sandbox_available`).
    fn available(&self) -> bool;
    /// The full argv: `bwrap <args> -- inner…` or `/usr/bin/sandbox-exec -p … -D… -- marv __guard -- inner…`.
    fn argv(&self, plan: &Plan, inner: &[OsString], fs: &dyn FsProbe) -> Result<Vec<OsString>, SandboxError>;
    /// Where a bind (or the plug) appears inside: bwrap `bind.dest`, Seatbelt `bind.source` (no mounts).
    fn guest_path(&self, bind: &Bind) -> PathBuf;
    /// Whether every sandbox of this backend hides `dir` (replaces `hidden_in_sandbox`).
    fn hides(&self, dir: &Path, home: &Path) -> bool;
    /// How to end a running command and everything it started (§6.10).
    fn kill(&self, child_pid: u32);
    /// After the command: bwrap removes placeholder mount points; Seatbelt removes the private TMPDIR.
    fn after(&self, plan: &Plan);
}
```

- **Selection** is by target: `Bwrap` on Linux, `Seatbelt` on macOS, none elsewhere (`available()` false). `Sandbox` holds an `Arc<dyn SandboxBackend>`; `Sandbox::active` becomes `enabled && backend.available()`.
- **Both renderers compile everywhere.** `bwrap::sandbox_args` and `seatbelt::profile` are pure functions of `(Plan, &dyn FsProbe)`; only `available`, `kill` and the spawn are platform-gated. So Linux CI runs the Seatbelt unit tests and the macOS job runs the bwrap ones.
- **What stays shared** (unchanged, used by both): `CommandMounts`, `Bind`, `FsProbe`, `resolve_extra`, `credential_masks`, `TOOLCHAINS`, `CREDENTIALS`, `HOME_MASKS`, `protected.rs` (rules, `protected_mounts`, snapshot, scan, revert), `git_guard.rs`, `writable.rs`, `host_env.rs`, `paths.rs`, `notes.rs`, `taint.rs`.
- **What moves out of bwrap.rs into the plan:** the policy parts of `sandbox_args` (`home_project` at :248, the extras and their order at :280-297, the `hidden` filter at :314-327, the config folder's conditions at :332-345, steps 1-3 of `home_masks` at :434-468). bwrap keeps how it spells them: tmpfs vs `/dev/null`, mount order, `run_mounts` (`/run` has no macOS meaning) and the socket scan (step 4 of `home_masks`, :469-504), which Seatbelt doesn't need (§6.5).
- **The relay and the plug stop assuming `/run`.** `relay::sandbox_argv` (relay.rs:83-86) and `relay_for` (bash.rs:98-117) ask `backend.guest_path` for the exe and socket paths instead of `RELAY_EXE` and `SOCKET_DIR`. The plug client reads `MARV_MESH_SOCKET` from its environment and falls back to `SANDBOX_SOCKET` (`plug.rs:34`); both backends set it.
- **Kill moves into the backend.** `bash.rs:219-229` and `processes/mod.rs:205-209` call `backend.kill(pid)`: bwrap keeps `start_kill`/SIGKILL to bwrap; Seatbelt does `killpg` (§6.10).

### 5.2 Tests stay FsProbe-based

Today's bwrap unit tests (`bwrap.rs:521` onwards) build `SandboxOptions` against a `Fake` file system and assert on the argument list. That stays the pattern:

- `Sandbox::plan(&self, root, mounts, &dyn FsProbe) -> Result<Plan, SandboxError>` is tested once, for the policy (what is hidden, refused, shown again), against `Fake`.
- `bwrap::sandbox_args(&Plan, &Fake)` keeps its tests unchanged: the refactor is proven by every existing bwrap test passing byte-for-byte.
- `seatbelt::profile(&Plan, &Fake) -> Profile { text: String, params: Vec<(String, PathBuf)> }` gets its own tests (§9.1). `Fake::links` gains `/tmp → /private/tmp` and `/var → /private/var` so canonicalization is tested on Linux.

## 6. The Seatbelt mapping

### 6.1 Deny-default, with all reads allowed and then narrowed

**Recommendation: `(deny default)`, then `(allow file-read*)` for the whole disk, then the denies and allows below, in a fixed order.** This is what Codex does (`seatbelt_base_policy.sbpl`: "start with closed-by-default", then `(allow file-read*)` when the policy has full-disk read), and what Chromium's renderer policy does for everything but files.

Why not allow-default with denies:

- **On macOS the dangerous doors aren't files.** On Linux the ways out of a sandbox are sockets under `/run` and the home, which bwrap hides. On macOS they are Mach and XPC services, reached by name through launchd: `open` and LaunchServices start apps outside the sandbox, `osascript` sends Apple Events (`tell application "Terminal" to do script …` runs a command in the user's terminal, under whatever Automation permission Terminal or Marv already has), `launchctl` submits jobs, `pbcopy`/`pbpaste` reach the pasteboard, `security` reaches the keychain through `securityd`. With allow-default each needs its own deny and the list is open-ended; with deny-default one rule closes them all and only a short, reviewed list is opened.
- **Unix sockets are network operations in Seatbelt.** `connect()` on a Unix socket is `network-outbound (remote unix-socket …)`, so deny-default refuses every socket on the machine (the SSH agent at `/private/tmp/com.apple.launchd.*/Listeners`, Docker's, Herdr's, the mesh's) unless a rule names it. That replaces the home-project socket scan (G16 step 4) with something stronger.
- **Reads, by contrast, are best allowed and narrowed.** Bwrap shows the whole system read-only (G1); a command needs `/usr`, `/Library/Developer`, `/Applications/Xcode.app`, `/opt/homebrew`, `/nix`, and an allowlist of those would break toolchains forever. So `file-read*` starts open and the home, the runtime folder, the config folder and the credentials are denied after it.

The cost of deny-default is a curated list of `mach-lookup`, `sysctl-read` and `ipc` rules for ordinary tools to work. Codex's base policy is the starting point (copied with attribution, it's Apache-2.0); the list grows only with a failing tool and a test.

### 6.2 Profile sections, in order

Later sections override earlier ones. Parameters (`-D KEY=path`) carry every path that can be a parameter; only the regex rules embed a path in the text (§6.3).

| § | Section | Content |
|---|---|---|
| 1 | Base | `(version 1)` `(deny default)`; `process-fork`, `process-exec`; `signal` and `process-info*` with `(target same-sandbox)`; Codex's `sysctl-read` list **without** `kern.proc.all` (G20); `mach-lookup` for `com.apple.system.opendirectoryd.libinfo` (user lookups: git, ssh), `com.apple.system.logger`; `ipc-posix-sem`; `/dev/null`, `/dev/zero`, `/dev/random`, `/dev/urandom`, `/dev/fd/*`, `/dev/dtracehelper` as Codex allows them. **No** `pseudo-tty` and no `/dev/tty*` (G22) |
| 2 | Reads, broad | `(allow file-read* file-map-executable)` |
| 3 | Home, closed | Not a home project: `(deny file-read* (subpath (param "HOME")))` with `(allow file-read-metadata (literal (param "HOME")))` so `cd ~` and `test -d ~` work. Home project: `(deny file-read* (regex #"^<home>/\.[^/]+(/.*)?$"))`, every hidden top-level entry (G16 step 1) |
| 4 | Reads, shown again | `subpath` of: the project, each toolchain, each read-only extra, each writable extra, the config folder's `skills/`, the private TMPDIR; `literal` of Marv's own binary when it's under the home (the guard and relay run it) |
| 5 | Reads, closed again | `HOME_MASKS` (home project, G16 step 3), the config folder (G15, before its `skills/` allow is repeated after it), the runtime folder (G14), `/private/tmp` and the user's `/private/var/folders/<xx>/<id>` (G2, except the private TMPDIR, allowed again), each credential file (G17), as `literal` of the path and of its real path |
| 6 | Writes | `(allow file-write* …)` for: the project (unless read-only or a home project), each writable extra, the private TMPDIR, `/dev/null`, `/dev/fd/*` |
| 7 | Writes, closed again | For each protected path and each nested `.git`: `literal` and `subpath` of the path and its real path (both, as Codex does: `subpath` alone leaves a gap for `mkdir .codex`); placeholders the same way (G12); the name rules of §6.6 as regexes (blocking *new* `.envrc`, `AGENTS.md`, `.git` at any depth, G13); `file-write-unlink` of the project root and of every ancestor of a protected path inside it (a rename of the parent can't carry the protected entry away, as Codex's `PROTECTED_ANCESTOR_n`) |
| 8 | Network | §6.8 |
| 9 | Last | `(deny mach-lookup (xpc-service-name-prefix ""))` (Codex: no XPC service even if a rule above matched by global name); `(deny system-fcntl (fcntl-command 80 110))` (`F_MAKECOMPRESSED`, `F_TRANSFEREXTENTS` change a file through a read-only descriptor; Codex) |

### 6.3 Parameters, canonicalization and escaping

- **Every path goes through `FsProbe::realpath` before it reaches a rule.** On macOS that turns `/tmp` into `/private/tmp`, `/var` into `/private/var`, `/etc` into `/private/etc`, and follows the user's own symlinks (a project at `~/code → /Volumes/work/code` is allowed at `/Volumes/work/code`). A path that doesn't exist yet (a placeholder) is the canonical parent plus the missing rest, which is `paths::real_with_missing` (`paths.rs:51`).
- **Both forms where the path as written matters.** Credentials and protected paths get a rule for the path as written and one for its real path, so a symlinked `~/.cargo/credentials.toml` (stow, chezmoi) is denied wherever it leads. This replaces `credential_masks`' "mask the target wherever a toolchain shows it" (`home.rs:104-122`) with something simpler: deny the target's literal real path.
- **`-D` for every literal and subpath.** Keys are fixed names plus an index: `PROJECT`, `HOME`, `TMP`, `RUNTIME`, `CONFIG`, `SKILLS`, `EXE`, `PLUG`, `TOOLCHAIN_n`, `READ_ONLY_n`, `WRITABLE_n`, `PROTECTED_n`, `PROTECTED_n_REAL`, `CREDENTIAL_n`, `CREDENTIAL_n_REAL`, `ANCESTOR_n`, `PORT_n` is not a path and is rendered as a number. Values never enter the profile text, so a project path with a quote or a parenthesis can't change the profile.
- **Regexes need the path in the text.** `regex` takes no parameter. The builder escapes the canonical path for both the SBPL string and the regex (`\`, `"`, and `. * + ? ( ) [ ] { } ^ $ |`), and **refuses** a path with a control character or a newline (`SandboxError::Mount`, "the project path has a character the macOS sandbox can't express"). G24 holds: such a command doesn't start.
- **APFS is case-insensitive by default**, and protected names are matched case-insensitively (`protected.rs:101`, `is_secret_name` at :128). SBPL regexes have no `(?i)`, so name rules are written as character classes (`[Aa][Gg][Ee][Nn][Tt][Ss]\.[Mm][Dd]`). Unicode normalization is also insensitive on APFS; protected names are ASCII, so that only matters for the path prefix, which comes from `realpath` and is exact.
- **The profile size.** One rule per protected path at any depth can grow; `-p` is limited by `ARG_MAX` (1 MB on macOS). Over 256 KB, the builder writes the profile to the private TMPDIR and passes `-f` instead.

### 6.4 The home, the toolchains and the credentials (G6, G7, G16, G17)

- **The home is unreadable, not empty.** bwrap shows an empty, writable tmpfs at `$HOME`; Seatbelt can only refuse it. `ls ~` fails with "Operation not permitted" instead of listing nothing, and a write to `~/.cache` fails instead of vanishing after the command. `HOME` stays the real home (toolchains are found at their real paths, as with bwrap). Tools that want a writable cache get `XDG_CACHE_HOME=$TMPDIR/cache` and `BUN_INSTALL_CACHE_DIR=$TMPDIR/bun-cache` (as G19's `/tmp/bun-cache`, for the same reason: no cache shared across commands). The bash description's "the home folder is an empty stand-in" (bash.rs:392) becomes platform text, fixed per build so the tool spec stays byte-stable.
- **Toolchains** are allowed again by `subpath` after the home deny, read-only (no write rule names them).
- **Credentials are denied, not emptied.** `cat ~/.cargo/credentials.toml` fails instead of printing nothing. The guarantee is that the content can't be read, and that holds. The deny comes after the toolchain allow (§6.2 section 5 after 4), which is the order unit tests check.
- **A home-folder project** (G16): reads of the home are allowed (it's the project), every hidden top-level entry is denied by one regex, toolchains are allowed again, then `HOME_MASKS`, the config folder, the runtime folder and credentials are denied again. No write rule covers the home, so it's read-only. The socket scan isn't needed: no `network-outbound (remote unix-socket)` rule covers the home, so no socket in it can be connected to, scanned or not, at any depth (G16 is met with no 20,000-entry limit). FIFOs are files: writing one is `file-write*`, denied.
- **A home that can't be listed** stops the command today (`bwrap.rs:426-431`). With a regex instead of a listing, Seatbelt doesn't need the listing, so that error goes away on macOS; the rule's intent (never show what can't be checked) holds because the regex covers every hidden entry, listed or not.

### 6.5 Marv's own folders (G4, G5, G14, G15)

- **The runtime folder** (§7) is denied for reads and writes by `subpath`, and no `unix-socket` rule covers it except the one plug (below). It's outside the project and the home, so the `shown_again` cases of `bwrap.rs:314-318` (a runtime folder under the project or a PATH folder) need no special handling: the deny is in section 5, after every allow.
- **The plug** is allowed by literal path: `(allow network-outbound (remote unix-socket (path-literal (param "PLUG"))))`. The sandbox reaches it at its real host path, which it learns from `MARV_MESH_SOCKET` (§5.1). It can connect but can't list, read or replace the folder. bwrap shows it at `/run/marv/mesh.sock`; on macOS `/run` doesn't exist and can't be created (the system volume is read-only), so a fixed guest path isn't possible.
- **The relay's binary** (G5) is Marv's own, allowed by `literal` for read and exec. Process socket folders: §8.
- **The config folder** is denied by `subpath` for reads and writes wherever it is, `skills/` allowed again for reads, unless it holds the project (same rule as `bwrap.rs:336`).

### 6.6 `.git`, protected paths, placeholders and the after-scan (G11, G12, G13)

- **Existing protected paths** get write denies (`literal` + `subpath`, path and real path). Unlike a read-only mount, a path rule also stops a *new* entry at that path: deleting `.envrc` and writing a new one is refused, and so is `mkdir .git` where none exists (G12, without a placeholder mount point to `rmdir` afterwards).
- **Name rules at any depth**, generated from `PROTECTED_PATHS` (`protected.rs:56`) as regexes under the project: `(^|/)\.git(/|$)`, `\.env[^/]*$`, `\.envrc$`, `bunfig\.toml$`, `\.mcp\.json$`, `(AGENTS|CLAUDE)\.md$`, `\.marv(/|$)`, `\.vscode(/|$)`, `\.idea(/|$)`, plus the `core.hooksPath` folder by path. These stop a command from *creating* a protected entry, which bwrap can't (that's why `scan_created` exists).
- **`.git` for an approved `git_write` command** is writable: the `.git` rules are left out of that command's profile, as bwrap leaves out the read-only `.git` (bash.rs:562: `git_guard` only when `confined`). Nested `.git` folders follow `nested_git_dirs` as today.
- **`package.json` lifecycle scripts** are a content rule (`lifecycle_scripts_changed`), not a path; no sandbox expresses it, today or here. Unchanged.
- **The after-scan stays.** `protected_snapshot`/`scan_created`/`revert_created` keep running after every sandboxed command on both backends. On macOS it should find nothing; it's the defence in depth if a rule is wrong, and a guard proves the regexes by checking the scan's result is empty.
- **Hard links** are the one way around a path rule: `ln .envrc x; echo … >> x` changes `.envrc`'s content through a path no rule names. bwrap stops it because a hard link can't cross a mount (`EXDEV`). Seatbelt checks `file-write-create` on `x` (allowed, it's in the project); whether it also checks the link's source is **to verify**. If it doesn't, the profile adds `(deny file-link …)` for the protected rules if the operation exists, and otherwise the after-scan checks `nlink > 1` on protected files and reports them. The escape test in §9.3 decides.

### 6.7 Writable and read-only extras, the project read-only (G8, G9, G10)

- `resolve_extra` (`bwrap.rs:182-206`) runs unchanged in the plan: the same refusals (the home or a parent of it, the project as read-only). The "around the project goes first" ordering bwrap needs (:280-293) has no Seatbelt meaning: a read-only folder around the project is a read allow, and the project's write allow is separate.
- `writable::resolve` runs unchanged at startup. bwrap's reason for leaving an entry inside the project out (a parent swapped for a symlink would bind wherever it leads, `writable.rs:27-30`) doesn't apply to Seatbelt, which checks the resolved path at every access, but the entry stays left out so both behave the same.
- `project_read_only` and the home project simply have no write rule for the project.

### 6.8 Network (G18)

- **Without network**, the profile has no `network-*` rule for IP at all. Seatbelt has no per-sandbox loopback: `127.0.0.1` is the host's, so allowing it would let a command reach the user's local services (a database, a dev server, an Ollama that can `pull` from anywhere, a local HTTP proxy that is the internet). **Recommendation: deny all IP, loopback included**, with two exceptions:
  - the ports of the user's background processes, for a bash command while they run: `(allow network-outbound (remote ip "localhost:<port>"))` per forwarded port. This replaces the relay for bash commands on macOS (bash.rs:98-117);
  - a background process's own listening port (§8).
- **The cost:** a test suite that starts a server on `127.0.0.1:0` and connects to it fails without network on macOS, where Linux's own loopback lets it pass. The network note (`NETWORK_NOTE`, notes.rs:36) is extended to say so, and #81's retry asks for `network: true`. Open question 1.
- **With network:** `(allow network-outbound (remote ip))`, `(allow network-bind (local ip))`, `(allow network-inbound (local ip "localhost:*"))`, the DNS socket `(remote unix-socket (path-literal "/private/var/run/mDNSResponder"))`, and the Mach services Codex's `seatbelt_network_policy.sbpl` lists for DNS configuration and TLS trust (`com.apple.SystemConfiguration.configd`, `…DNSConfiguration`, `com.apple.trustd.agent`, `com.apple.SecurityServer`, `com.apple.networkd`, `com.apple.ocspd`). Unix sockets stay closed: unlike bwrap's shared network namespace with its abstract sockets (`bwrap.rs:25-26`), a networked macOS command still can't reach the SSH agent or Docker's socket.
- **What `localhost` matches** (both `127.0.0.1` and `::1`, and whether a bind to `0.0.0.0` is refused by a `(local ip "localhost:*")` rule) is **to verify**.

### 6.9 Environment (G19, G23)

The spawn already does `env_clear()` (`bash.rs:190`, `processes/mod.rs:437`); `sandbox-exec` passes its environment on unchanged. So the plan's `env` list is set on the `Command` instead of as `--setenv`: `PATH`, `HOME`, `LANG`, `TERM=dumb`, `TMPDIR=<private>`, `BUN_INSTALL_CACHE_DIR`, `XDG_CACHE_HOME`, and `MARV_MESH_SOCKET` when plugged. `current_dir(root)` replaces `--chdir`. Same result, nothing else inherited (no `DYLD_*`, no keys).

### 6.10 Process lifetime (G20, G21, G22)

There's no PID namespace and no `PR_SET_PDEATHSIG`. The design:

1. **`marv __guard`**, a hidden subcommand like `__relay` (relay.rs:33), is what `sandbox-exec` runs: `sandbox-exec -p … -- <marv> __guard <marv-pid> -- bash -c …`. It runs inside the sandbox (its binary is allowed by `literal`), so nothing new runs unsandboxed.
2. **It makes a session.** `nix::unistd::setsid()` (safe, no `unsafe` needed) gives a new session and process group whose id is the guard's pid, which is `sandbox-exec`'s pid (exec keeps it), which is the `Child::id()` Marv already has. No controlling terminal; with stdin `/dev/null` and no `/dev/tty*` rule (section 1), `TIOCSTI` has nothing to aim at (G22).
3. **It watches Marv.** It spawns the command, then polls `getppid()` every 200 ms. When it's no longer Marv's pid (Marv died, even by SIGKILL, and the guard was re-parented to launchd), it does `killpg(0, SIGKILL)` and exits (G21). Polling is portable and safe; `kqueue` with `EVFILT_PROC`/`NOTE_EXIT` through `nix::sys::event` is the low-latency alternative if 200 ms proves too slow.
4. **Marv kills the group.** `backend.kill(pid)` is `killpg(pid, SIGKILL)` for a timeout, a stop or `kill_running` (`processes/mod.rs:199-203`). The guard's exit code is the command's.
5. **Seeing and signalling other processes** (G20): `signal` and `process-info*` are allowed only `(target same-sandbox)`, and `kern.proc.all` isn't in the sysctl list, so `kill`, `ps` and `lsof` can't reach the user's processes. A pid may still be guessed; nothing can be done with it.

**Known limit:** a process that calls `setsid()` or `setpgid()` itself leaves the group, and if it also double-forks it's re-parented to launchd and the guard can't find it. It's still in the sandbox (Seatbelt is inherited and can't be dropped), so it can only do what the command could, but it outlives the command. bwrap's PID namespace closes this; Seatbelt can't. Mitigation in §6.12; open question 5 asks about a process-tree sweep.

### 6.11 Availability and errors (G24)

- `sandbox-exec` is always run as `/usr/bin/sandbox-exec`, never from PATH (it's on the SIP-protected system volume; Codex does the same, `MACOS_PATH_TO_SEATBELT_EXECUTABLE`).
- `available()` runs, once per process, the real base profile (section 1 and 9 of §6.2 with an empty plan) on `/usr/bin/true`. A missing binary, a profile the kernel no longer accepts, or a Marv that is itself already inside a Seatbelt sandbox (nested `sandbox_apply` fails with "Operation not permitted": Marv run from Codex or Claude Code's sandbox) all make it false, and Marv behaves as on a Linux machine without bwrap (bash.rs:513-514: the command runs without a sandbox, with the warning, and the checks don't run at all).
- A profile `sandbox-exec` rejects prints `sandbox-exec: …` on stderr with exit 65 or 71; bash's existing "a sandbox that never started" path (bash.rs:250-253) shows it as `[sandbox failed to start: …]`.

### 6.12 What Seatbelt can't match exactly

This table goes into `docs/SECURITY.md` under "Sandbox", as "On macOS (Seatbelt)".

| bwrap guarantee | Seatbelt | Mitigation | Residual |
|---|---|---|---|
| G6 stand-in home: empty, writable, thrown away | The home is unreadable and unwritable | `XDG_CACHE_HOME` and bun's cache in the private TMPDIR; the home note says reads and writes there are refused | Tools that insist on writing `~/.something` fail with EPERM instead of working on a throwaway copy |
| G17 credentials read as empty files | Reads refused | Deny by literal path and real path | A tool that expects the file to exist and be empty errors out |
| G2 private `/tmp` | `/tmp` is shared with the host | A private TMPDIR per command (0700, made by Marv, removed after); `/private/tmp` and the user's `/var/folders/…` denied for reads and writes | Tools that hard-code `/tmp` fail with EPERM; open question 2 |
| G20 own PID namespace | None | `signal`/`process-info*` same-sandbox only, no `kern.proc.all` | Pids of other processes are guessable but unusable |
| G21 `--die-with-parent` | None | `marv __guard`: own process group, polls its parent, `killpg` on Marv's death; `kill_running` on exit | A process that leaves the group and double-forks survives the command, still sandboxed |
| G22 `--new-session` | No flag | `setsid()` in the guard, stdin `/dev/null`, no `/dev/tty*` and no `pseudo-tty` rule | None known |
| G18 own loopback without network | None: loopback is the host's | Deny all IP without network; allow only forwarded process ports by number | In-sandbox client/server tests need `network: true` (asks) |
| G11, G12 read-only mounts | Path-based write denies | Literal + subpath + name regexes, case-insensitive; after-scan kept | Hard links to an existing protected file: to verify (§6.6) |
| G16 home-project socket scan, bounded | Every Unix socket refused unless named | None needed | Stronger than bwrap: no depth or count limit |
| G18 with network, abstract sockets reachable | No abstract sockets; Unix sockets stay closed with network | None needed | Stronger than bwrap |
| — (Linux has no equivalent) | Mach/XPC services: `open`, `osascript`, `launchctl`, pasteboard, keychain | Deny-default; a short `mach-lookup` list; `xpc-service-name-prefix ""` denied last | Every added service is a review item |
| — | `sandbox-exec` is deprecated (§6.13) | Probe on every start; guards on every CI run | A macOS release could remove it; Marv then runs commands unsandboxed with the warning, as without bwrap |

### 6.13 `sandbox-exec`: deprecated, shipped, relied on

- **Status.** Its man page has said "The sandbox-exec command is DEPRECATED" since about macOS 10.12, pointing developers to the App Sandbox. It still ships in every release since, including the ones the CI runners use, and the kernel side (`Sandbox.kext`, the SBPL engine) is the same one the App Sandbox and Apple's own daemons use, so it is maintained even though the command-line front end isn't documented.
- **Who relies on it.** OpenAI's Codex CLI (all of its macOS sandboxing), Anthropic's sandbox runtime for Claude Code, Google's Gemini CLI (`sandbox-exec` profiles), Bazel's `darwin-sandbox` strategy, Nix's macOS build sandbox, and Homebrew's build sandbox. Chromium uses the same SBPL engine through the private `sandbox_init_with_parameters` API rather than the binary. A removal would break all of these at once, which makes a silent removal unlikely and a loud one well announced.
- **Risk to Marv.** If it goes away, `available()` turns false and macOS commands run without a sandbox, with the existing warning (open question 7 asks whether macOS should refuse instead). If the SBPL changes meaning, the guard tests on the macOS job fail first. The App Sandbox is not an alternative for a CLI that runs arbitrary tools, and Endpoint Security needs an entitlement and a system extension; neither is planned.
- **One packaging constraint:** a process inside the App Sandbox can't use `sandbox-exec`. The desktop app (`app/`) must not be App Sandboxed if it runs the agent itself (open question 9).

## 7. The runtime folder on macOS

### 7.1 Requirements

The folder holds the mesh doors, the plugs and the process sockets. It must be private to the user (0700, the user's, not a symlink), invisible and unwritable inside every sandbox, and short enough that the longest socket path fits `sun_path`: 104 bytes on macOS, so at most 103 bytes of path.

The longest socket today is a plug: `<base>/plugs/<session>/<plug>.sock`, with a session (mesh name) of up to 43 bytes (`MAX_SESSION_NAME`, discovery.rs:66) and a plug id of up to 48 (`MAX_SEGMENT`, receptacle.rs:27; `peer-` plus a 43-byte name). That's `base + 7 + 43 + 1 + 48 + 5 = base + 104` bytes: over the limit on macOS for any base, and over Linux's 107 too with `/run/user/1000/marv` (19 bytes → 123). Today it only bites with long `MARV_MESH_NAME`s (generated names are at most 25 bytes), but it's a latent bug on Linux as well.

### 7.2 Options

| Base | Length | Hidden in the sandbox | Notes |
|---|---|---|---|
| `$TMPDIR` (`/var/folders/…/T/marv`) | ~54 | Yes (denied) | Ruled out: the plug path is ~158 bytes |
| `~/Library/Caches/marv` | 22 + home | Yes (home denied) | Length depends on the user name; a home-folder project needs it denied explicitly |
| `~/.marv/run` | 11 + home | Yes | Inside the config folder: `MARV_CONFIG_DIR` can be long, and the config folder holds files the runtime shouldn't share a lifetime with |
| **`/tmp/marv-<uid>`** | 13–20 | Yes (`/private/tmp` denied, §6.12) | Fixed length; already the processes' fallback on Linux (`processes/mod.rs:274`); cleared at boot, like `/run/user` |

**Recommendation: `/tmp/marv-<uid>` on macOS**, used as the path string `/tmp/marv-<uid>` for `bind()` (shorter) and as `/private/tmp/marv-<uid>` in SBPL rules.

- **Private:** made with mode 0700; then `private_folder`'s checks (`discovery.rs:138-145`): a folder, not a symlink, owned by `getuid()`. If another user made `/tmp/marv-501` first, the owner check fails and the mesh and processes are off with a message naming the folder. That's a denial of service by a local user, not a takeover (the sticky bit stops them deleting or replacing ours).
- **Housekeeping:** macOS's daily `clean-tmps` deletes regular files not accessed in three days and empty folders; sockets aren't regular files. **To verify** against the runner's `/etc/periodic/daily/110.clean-tmps`. If it does touch them, the radio's 30 s re-list re-binds a missing door.
- **Override:** `MARV_RUNTIME_DIR` from the real environment, checked like the default (absolute, short enough, private, and hidden by the backend), for users who want it elsewhere. Open question 3.

### 7.3 Layout and code changes

- One function, `marv_sandbox::runtime_dir(env) -> Option<PathBuf>`: Linux `$XDG_RUNTIME_DIR/marv` when `backend.hides()` agrees (today's rule), macOS `/tmp/marv-<uid>`. It replaces the three copies of the rule: `discovery::mesh_dir` (discovery.rs:71-75), `marv_session::mesh::start` (mesh.rs:69-71) and `ProcessManager::parent_dir` (processes/mod.rs:266-276), and `writable::resolve`'s `runtime` argument (writable.rs:114, which today adds `marv` itself). `Sandbox::from_env`'s `hidden` (lib.rs:144-148) uses it too.
- **Shorter plug folders, on both platforms:** `<runtime>/plugs/<session>/` becomes `<runtime>/p/<pid>/`. The folder only needs to be unique per process; the plug's name stays the socket's name. Longest: `20 + 3 + 7 + 1 + 48 + 5 = 84` bytes on macOS, `19 + 3 + 7 + 1 + 53 = 83` on Linux. Doors stay `<runtime>/mesh/<name>.sock` (at most 74 bytes). Process sockets stay `<runtime>/<pid>-<6 hex>/p<n>/fwd.sock` (about 50).
- A check at the one place sockets are bound (`switchboard.rs:347`, `discovery.rs:377`) refuses a path over the platform's limit with a clear error instead of `bind`'s `EINVAL`.
- **`own_uid` without `/proc`:** `nix::unistd::getuid().as_raw()`. `nix` 0.31 is already a workspace dependency (marv-session has the `user` feature); marv-mesh adds it. Safe, so `unsafe_code = "forbid"` (Cargo.toml:41) holds.
- **The pid check works on macOS.** tokio's `UnixStream::peer_cred()` fills `pid()` there from `LOCAL_PEERPID`, so `marv/wire` and `marv/radio.join`'s check by pid (discovery.rs:228, :407) carry over. **To verify** with the existing door tests on the runner.
- **Messages** that name `XDG_RUNTIME_DIR` (mesh_cmd.rs:38, headless.rs:509, tui.rs:1723 and :2026) name the runtime folder per platform.
- **Tests** use a short base: `/tmp/mx-<random>` on macOS instead of `temp_dir()` (the CI job already counts `mx-*` folders in `/tmp`).

## 8. Processes and the port relay (#152, in short)

The Linux design rests on three things macOS doesn't have: a network namespace per process, `/proc/<pid>/net/tcp` to find its ports, and `/proc/<pid>/task/…/children` and `/proc/*/fd` to find its pid and sockets (processes/mod.rs:164-189, :598-640).

| Option | How | For | Against |
|---|---|---|---|
| A. Direct loopback | No relay. The process may `network-bind` and `network-inbound` on `localhost:*` only; its ports are on the host's 127.0.0.1 at once. Marv finds them with `lsof -a -g <pgid> -iTCP -sTCP:LISTEN -Fn` (or the `libproc` crate, safe). Bash commands reach them by the per-port rule of §6.8 | Simple, no relay binary in the sandbox | Ports can't be refused before they open (`MAX_PORTS` becomes "listed, not forwarded" with a note); whether a bind to `0.0.0.0` is refused is to verify; two processes can reach each other only through Marv's per-port rules |
| B. Keep the relay over a Unix socket | The relay listens on `fwd.sock` in the process's folder (allowed by `path-literal`); the server listens on loopback inside, which on macOS is still the host's | Same code as Linux | The isolation the relay exists for (a netns) isn't there, so it's a relay with nothing to relay around |
| C. Not on macOS yet | The process door isn't offered on macOS until #152 | Fails closed, no new surface | A missing feature |

**Recommendation: C for the first release of the Seatbelt backend, then A in #152.** The guarantees to keep are SECURITY.md's (lines 40-44): can't write outside the project, change `.git` or read the home (the profile gives those), can't reach the host's network or `127.0.0.1` without `network: true` (§6.8), ports on `127.0.0.1` only (to verify), and dies on stop, exit, panic and SIGKILL (the guard, §6.10).

## 9. Test plan

### 9.1 Unit tests of the profile builder (Linux and macOS)

In `crates/marv-sandbox/src/seatbelt.rs`, against `Fake` (§5.2):

- **The base:** starts with `(version 1)` and `(deny default)`; no `(allow default)`, no `pseudo-tty`, no `kern.proc.all`, no unlisted `mach-lookup`; ends with the XPC and `fcntl` denies.
- **Order invariants**, checked by position in the text: each credential deny after every read allow that covers it; the config deny after the toolchain allows, its `skills/` allow after the deny; the runtime deny after the project allow (a runtime folder inside the project, as `tests/guards/tests/mesh.rs:208` tests for bwrap); protected write denies after the project's write allow.
- **Canonical params:** with `Fake` mapping `/var → /private/var` and `/tmp → /private/tmp`, every `-D` value is canonical; credentials and protected paths have both forms.
- **Escaping:** a project path with `"`, `\`, `(`, `.`, `*`, a space and non-ASCII produces a profile whose regexes match only that path (checked with the `regex` crate on the generated pattern); a path with a newline or control character is refused.
- **Case-insensitive names:** the generated pattern for `AGENTS.md` matches `agents.md`, `Agents.MD`, `pkg/AGENTS.md`, not `AGENTS.md.bak`.
- **Each plan field** has a test: network on and off (and forwarded ports), project read-only, home project (one regex, no socket scan), read-only and writable extras, placeholders, `git_write` (no `.git` rules), plug, a symlinked credential.
- **Parity:** one table of scenarios is run through `Sandbox::plan` once and through both renderers, so a new plan field without a Seatbelt rule fails a test that lists every field.

### 9.2 Probes on the macOS runner

Small tests, in `crates/marv-sandbox/tests/seatbelt_probe.rs` and `#[cfg(target_os = "macos")]`, that pin the behaviour this spec marks **to verify**: the last rule wins; rules see `/private/tmp`, and `/Users` as `/Users`; `localhost` covers `127.0.0.1` and `::1`; a bind to `0.0.0.0` under a `localhost:*` rule is refused; a hard link to a write-denied file can't be written through; `peer_cred().pid()` is `Some`; `clean-tmps` leaves sockets alone. Each one failing changes the design here before the code relies on it.

### 9.3 Guards on `macos-14` with `MARV_REQUIRE_SANDBOX=1`

- **CI:** the macOS job gets `MARV_REQUIRE_SANDBOX: "1"` and a step before the tests, `/usr/bin/sandbox-exec -p '(version 1)(allow default)' /usr/bin/true`, so a broken runner fails loudly. `require_bwrap!` becomes `require_sandbox!` (`tests/guards/src/lib.rs:39`), and `where_the_sandbox_is_required_it_really_runs` (sandbox.rs:21) asserts `Sandbox::active()` with the backend's name in the message. Once green, `continue-on-error` goes (ci.yml:90) and the job is required.
- **The existing guards run on both.** Where they test a mechanism rather than an outcome, the assertion becomes the outcome: `system_is_read_only_and_run_is_empty_but_for_the_mesh_plug` (sandbox.rs:34) checks "the system can't be written, the plug can be reached, nothing else in the runtime folder can" instead of `ls /run`; `no_network_cleared_environment_and_own_pid_namespace` (:70) checks a connection fails, the key is absent and `kill -0` of a host pid fails, instead of `/proc/net/dev`. Each keeps its "Proven:" line, re-proven on macOS by removing the matching SBPL rule.
- **Escape tests, macOS-specific** (`tests/guards/tests/seatbelt.rs`), each one the macOS twin of a Linux guard or a door Linux doesn't have:
  - `osascript -e 'tell application "Finder" to get name'`, `open -a TextEdit`, `launchctl submit`, `pbpaste`, `security find-generic-password` all fail;
  - connecting to `$SSH_AUTH_SOCK` (a test listener in `/private/tmp`) and to a socket in a home-folder project fails, with and without network;
  - a socket in the runtime folder can't be listed or connected to, the plug can;
  - reading `~/.ssh/x`, `~/.marv/config.json`, `MARV_CONFIG_DIR/config.json` elsewhere, a credential file and its symlink target fails; `~/.marv/skills/x` and a toolchain file read fine;
  - writing `/usr/local/x`, a file outside the project, `~/.zshrc` in a home-folder project, `/tmp/x` fails; `$TMPDIR/x` works and is gone after;
  - creating `.envrc`, `pkg/AGENTS.md`, `pkg/agents.MD`, `.git` (in a project without one), `.marv/x`, `.vscode/tasks.json` fails, and `scan_created` finds nothing;
  - `mv` of a folder holding `AGENTS.md`, and `ln .envrc x && echo >> x`, don't change `.envrc`;
  - without network: `curl https://example.com`, `nc 127.0.0.1 <host listener>`, a bind to `0.0.0.0` fail; with a forwarded process port, that port works and no other;
  - `kill -0 <Marv's pid>` and `ps -A` don't show or reach host processes;
  - a command's `sleep 600 &` dies at the timeout, and a sandboxed child survives none of: stop, Marv's exit, SIGKILL of Marv (the guard); a `setsid` double-fork escapee is checked to still be sandboxed (it can't write outside the project) and is the one documented survivor;
  - writing `/dev/tty` and opening `/dev/ttys000` fail;
  - an API key in Marv's environment and `DYLD_INSERT_LIBRARIES` don't reach the command.
- **Portability fixes for the 63 failures** land first and get no new guards: `getuid`, canonical comparisons in `hidden_in_sandbox` and in `TempDir` (canonicalize at creation), short test runtime folders, `wc -l | tr -d ' '` and a Python `os.setsid()` in fixtures.

## 10. Implementation steps

Each step is one PR, green on Linux, and adds what it fixes to the macOS job.

1. **Portability without the sandbox** (#150): `own_uid` via `getuid`; canonical paths on both sides of every comparison (`hidden_in_sandbox`, guard and crate `TempDir`s); BSD-safe fixtures; short test runtime folders. The macOS job's 63 failures go to zero; sandbox tests still skip there.
2. **The runtime folder** (#150): `runtime_dir`, `/tmp/marv-<uid>` on macOS, `p/<pid>` plug folders on both, the length check, messages. Mesh, radio and plug tests pass on macOS (outside the sandbox).
3. **The trait, Linux only** (#151): `Plan`, `Sandbox::plan`, `SandboxBackend`, `Bwrap`; `guest_path` for the relay and `MARV_MESH_SOCKET` for the plug; `kill` in the backend. Every bwrap unit test and guard passes unchanged.
4. **The profile builder** (#151): `seatbelt::profile` with §9.1's tests, run on Linux CI.
5. **`marv __guard`** (#151): session, parent watch, `killpg`; unit-tested on both platforms outside a sandbox.
6. **Wire Seatbelt in** (#151): `Seatbelt` backend on macOS, private TMPDIR lifecycle, the probes of §9.2, then the guards of §9.3 with `MARV_REQUIRE_SANDBOX=1`. The process door isn't offered on macOS (§8, C).
7. **Make the macOS job required** and add the release targets (`release.yml:76-82`, commented out today).
8. **Processes on macOS** (#152): option A of §8 with its guards (`tests/guards/tests/process.rs` on both).
9. **Docs** with each step: `docs/SECURITY.md` (the §6.12 table, the runtime folder rule on line 59 per platform), `docs/subsystems/sandbox.md`, `docs/MESH.md` (the runtime folder), `PORTING.md`, the bash tool's description per platform.

## 11. Open questions

1. **Loopback without network.** Deny it all, as recommended (in-sandbox client/server tests then need `network: true`, which asks), or allow `localhost:*` and accept that a command can reach the user's local services?
2. **`/tmp`.** Deny `/private/tmp` entirely, with a private `TMPDIR` per command (recommended), or allow writes to a per-command folder under `/private/tmp` that tools hard-coding `/tmp` won't use anyway?
3. **Runtime folder.** `/tmp/marv-<uid>` (recommended) or a home path; and should `MARV_RUNTIME_DIR` exist as an override?
4. **The process door on macOS** before #152: off (recommended), or ship option A with the first backend?
5. **Escapees from the process group.** Accept as a documented limit (recommended for now), or add a process-tree sweep in the guard (needs a crate such as `libproc` or `sysinfo`)?
6. **Secrets in the project** (`.env*`, `*.pem`, `id_*`): Seatbelt can deny their reads with one regex, which `secrets_are_masked_in_the_sandbox` (sandbox.rs:603, ignored) wants and bwrap doesn't do yet. Do it on both together later (recommended), or on macOS first?
7. **No `sandbox-exec`.** Same as Linux without bwrap (commands run unsandboxed, with the warning) as recommended, or refuse to run bash on macOS without it?
8. **Supported macOS versions.** CI has `macos-14` (Sonoma, arm64). Add `macos-15` and the newest runner, and an Intel runner (`macos-13`) for the x86_64 build?
9. **The desktop app.** Confirm it will never be App Sandboxed while it runs the agent in-process (sandbox-exec can't run inside the App Sandbox).
