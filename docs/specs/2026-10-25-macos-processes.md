# macOS: background processes without Linux namespaces

- Issue: FlologixAI/marv #152 (part of the macOS port, #150; follows #166, the Seatbelt backend)
- Status: **draft, awaiting maintainer approval.** The decisions are in §10.
- Date: 2026-10-25, against `main` at `3a31512` (after 0.10.0).
- Builds on: `docs/specs/2026-10-17-macos-sandbox.md` (approved 2026-10-19). Its §8 and decision 4 already chose "off until #152, then option A". This spec works A out in detail, checks B again, and lists what must be proven on the macOS runner before A ships.
- Scope:
  - `crates/marv-sandbox`: the Seatbelt profile (§6.8's process rules), `SandboxBackend::processes`, a port probe for macOS;
  - `crates/marv-session`: `processes/mod.rs` (port discovery, lifetime, no relay on macOS);
  - `crates/marv-tools`: `bash.rs` (reaching processes without the relay), `process.rs` (description per platform);
  - `tests/guards/tests/process.rs`, `docs/SECURITY.md`, `docs/subsystems/{session,sandbox}.md`.

## 1. Problem and evidence

### 1.1 What `main` does on macOS today

The process door is **off on macOS**. Nothing about it runs there, so there's no partial version to describe:

- `SandboxBackend::processes()` is `false` for Seatbelt (`seatbelt_backend.rs:249`). The session offers the door only when `init.processes && backend.processes() && backend.available()` (`session.rs:1099`). On macOS the `process` tool isn't in the tool list, `/processes` lists nothing, and the prompt gets `SANDBOX_LIMITS` (`prompt.rs:122`), not the door's paragraph.
- The profile builder refuses any plan with `binds` (`seatbelt.rs:342-347`: "Background processes aren't available in the macOS sandbox yet"). The relay needs binds, so neither a process's sandbox nor a bash command with forwards could start.
- A bash command can't keep a server running either: the guard `killpg`s its group after the command exits (`bash.rs` `ExitWatch`, `guard.rs` header), and without network the profile has no IP rule, loopback included (`seatbelt.rs:587-592`).

So on macOS today a model can only start a server inside one bash command, check it there, and stop it. That's the same limit Linux has without bwrap.

### 1.2 What is Linux-only in `processes/mod.rs`

| Where | What | Why it can't work on macOS |
|---|---|---|
| `:163-166` `sandbox_pid` | `/proc/<bwrap>/task/<bwrap>/children`: the first process inside the sandbox | No `/proc` |
| `:168-189` `sandbox_sockets` | `/proc/*/ns/pid` and `/proc/*/fd`: the socket inodes of everything in the sandbox's PID namespace (`network: true` only) | No `/proc`, no PID namespace |
| `:620-664` `poll` | `/proc/<pid>/net/tcp{,6}`: the sandbox's listening ports | No `/proc`. On Linux, without network, these rows are the sandbox's own network namespace |
| `:666-700` `forward` + `relay.rs` | Marv listens on the host's `127.0.0.1:<port>` and pipes each connection through `fwd.sock` into the namespace | It works as code, but it relays around a namespace macOS doesn't have (§3, B) |
| `:550-563` `stop` | `terminate` and then `kill` on bwrap's pid. bwrap's PID namespace takes everything down | On Seatbelt, `kill` is the guard's `killpg`. Anything the process left behind in its group after the guard exited isn't killed, because the watcher at `:509` reaps the guard with `child.wait()` and nothing kills the group afterwards (bash does this, `processes` doesn't) |
| `:1-31` module doc | `--die-with-parent`, own netns | Seatbelt has the guard's parent watch instead |

### 1.3 What already works on macOS

Most of what the issue asked to check is already portable. It is required on the three macOS CI runners (`ci.yml:98-114`, `cargo test --workspace`):

- **`relay.rs:131-161`**: `tokio::signal::unix` and `std::os::unix::process::ExitStatusExt` are Unix APIs, not Linux ones. The relay builds and its unit tests pass on macOS. `exit_code` is already used on macOS by the guard (`guard.rs`), and by the process watcher (`processes/mod.rs:516`). Only Windows would need `cfg` work, which is a separate issue.
- **`git.rs:76-118`** (`GitGroup`, `process_group(0)`, `killpg`) and **`bash.rs:207-217`** (the unsandboxed path's `process_group(0)` and `marv_sandbox::kill_group`): `CommandExt::process_group` and `nix::sys::signal::killpg` are POSIX, and they behave the same on macOS. The sandboxed path already uses `backend.kill`, which is the guard's `killpg` on Seatbelt (`seatbelt_backend.rs:227-230`), plus the post-exit group kill (`ExitWatch`, `wait_exited` with kqueue `NOTE_EXIT`, `backend.rs:150-198`). Nothing to do here for macOS.
- **The runtime folder** `/tmp/marv-<uid>` (`runtime.rs`, sandbox spec §7) exists, is checked with `private_folder`, and is hidden from every Seatbelt sandbox. `ProcessManager::parent_dir` (`:278-296`) already uses `runtime_dir_for` on both platforms. Process sockets fit `sun_path` (about 50 bytes).
- **Lifetime**: the guard gives each sandbox its own group, polls `getppid()` and `killpg`s when Marv dies (G21). `kill_running` goes through `backend().kill`, so Marv's exit and a panic already take the groups down.
- **The profile builder** already has the bash half of option A: `Invocation::ports` renders `(allow network-outbound (remote ip "localhost:<port>"))` per port (`seatbelt.rs:172`, `:587-592`). The backend still always passes `ports: Vec::new()` (`seatbelt_backend.rs:188`).

## 2. Goals and non-goals

**Goals:**
- The `process` tool on macOS, with the process guarantees that `tests/guards/tests/process.rs` proves on Linux kept or, where Seatbelt can't keep one, the gap documented in the §6.12 table:
  - writes only in the project;
  - `.git` can't change;
  - the home can't be read;
  - without network: no internet and no user service on loopback;
  - ports reachable on loopback only;
  - dies on stop, Marv's exit, a panic and SIGKILL.
- A sandboxed bash command reaches the running processes' ports (`curl localhost:<port>`), as on Linux.
- No Linux behaviour change.

**Non-goals:**
- Windows.
- Process-to-process reach. One process can't connect to another on Linux either: its sandbox has no reverse relay (`processes/mod.rs:439-443`, `connect: vec![]`). §8 has it as a later item for both platforms.
- A process-tree sweep for processes that leave the group (sandbox spec decision 5 stays).

## 3. Options

### A. Direct loopback: the process binds the host's localhost

The process's profile adds:
- `(allow network-bind (local ip "localhost:*"))`;
- `(allow network-inbound (local ip "localhost:*"))`.

It gets **no** `network-outbound` IP rule. Its server listens right on the host's `127.0.0.1`/`::1`, so there's no relay and no host listener. Marv finds the ports the process's group listens on and lists them. Bash commands reach them through the per-port outbound rule that already exists.

- **For:**
  - the least code;
  - no Marv binary or socket inside the sandbox;
  - the browser connects straight to the server: no relay hop, WebSockets and HMR as normal;
  - port conflicts show up as the server's own `EADDRINUSE`.
- **Against:**
  - Marv can't refuse a port before it opens (§5.2);
  - a server that binds `0.0.0.0` or `::` must be refused by Seatbelt, or A leaks to the LAN. **To verify: probe 1**;
  - a process can't connect to its own port (no outbound rule; §5.3).

### B. Keep network denied and keep the Unix-socket relay

The issue's second option. The question is how a dev server listens at all without a network namespace.

- **B1, the relay as on Linux.** The server must still bind TCP, and on macOS any TCP bind lands on the host's loopback. So B1 needs the same `network-bind` and `network-inbound` rules as A. The relay also has to *connect* to the server from inside the sandbox, which is `network-outbound`. A profile is fixed when `sandbox-exec` starts, and the server's port isn't known then, so that has to be `(remote ip "localhost:*")`. That rule reaches every user service on loopback (databases, Ollama, a local proxy): the very thing sandbox spec decision 1 closed. B1 is A plus a hole plus a relay.
- **B2, servers on a Unix socket only.** Allow binding only `fwd.sock`, and the server listens there. Node's `server.listen(path)`, gunicorn and uvicorn can do that. Vite, Next.js, webpack-dev-server, `rails s`, `python -m http.server` and most dev servers can't. Not a general answer.
- **B3, a shim that turns `bind(127.0.0.1:p)` into the socket.** This is Linux's `LD_PRELOAD`, which is `DYLD_INSERT_LIBRARIES` on macOS:
  - SIP strips `DYLD_*` when it runs a protected binary. `bash` is `/bin/bash` and the guard runs `bash -c`, so the variable is gone before the server starts;
  - hardened-runtime binaries without the `allow-dyld-environment-variables` entitlement ignore it, and the notarized Node from nodejs.org is one of them;
  - it would mean shipping a C dylib, and it is `unsafe` by nature (the workspace forbids it, `Cargo.toml:41`).

  Ruled out.
- **B4, socket activation.** Marv binds `127.0.0.1:p` outside and passes the descriptor in, in the `LISTEN_FDS` style. It works only for servers that take an inherited socket (gunicorn `--bind fd://3`, systemd-aware daemons), and the port must be known before the start. Most dev servers don't do this. Also to verify: whether Seatbelt checks `accept` on an inherited socket against `network-inbound`.

**Conclusion on B:** keeping the relay doesn't keep network denied. Every variant that works for ordinary dev servers needs at least A's bind and inbound rules. B1 also needs outbound to all of loopback.

### C. Stay off on macOS

This is today's state, and it fails closed. It is a missing feature, and the model's only route to a running server is `network: true` in a bash command that ends with the command.

## 4. Recommendation: A, with five additions

A, as decided on 2026-10-19, plus five things the earlier spec didn't cover:

1. **Wildcard binds refused, and the model told.** If probe 1 shows that `(local ip "localhost:*")` refuses `0.0.0.0`/`::` (expected: `EPERM`), the macOS process description and the start result say "listen on 127.0.0.1 or localhost (e.g. `--host 127.0.0.1`), not 0.0.0.0". The start result also gets a note when the log shows `EPERM`/"Operation not permitted" on a bind and no port opened. `python -m http.server` and some Next.js setups bind every interface by default. **If probe 1 shows that wildcard binds are allowed, A doesn't ship** (§10, decision 1).
2. **Low ports refused by rule.** Since macOS 10.14 a normal user may bind ports below 1024 (**to verify: probe 3**). On Linux, `MIN_PORT` only stops forwarding, because the bind stays inside the namespace. On macOS, `127.0.0.1:80` would be the host's. The profile adds `(deny network-bind (local ip "localhost:<n>"))` for 1-1023, after the allow. That's about 45 KB of rules, under `PROFILE_FILE_OVER` (256 KB); probe 4 measures the start time. SBPL has no port ranges.
3. **The post-exit group kill, as bash has.** The process watcher sees the guard's exit with `wait_exited` (not reaping it), calls `backend.kill(pid)` for what is still in the group, and then `child.wait()`s. Without this, a leftover listener keeps a port open on the host's loopback with no entry in Marv. On Linux, bwrap's namespace already did this, and the extra kill is a no-op.
4. **The other address family held by Marv** (§5.1). For each port the process listens on, Marv binds the *other* loopback address (`::1` for `127.0.0.1` and the reverse) and holds it while the port is listed. If that bind fails, someone else is there: the port is listed for the user, but bash commands don't get a rule for it, and a note says why.
5. **No relay and no socket folder on macOS.** The process's own sandbox runs `bash -c` under the guard, with no binds. `ProcessManager` doesn't make `<pid>-<hex>/p<n>/` on macOS. `ProcessForward.dir` isn't used, and bash gets ports instead of binds.

### 4.1 The profile

The plan gets two fields:
- `Plan::listen_localhost: bool`: set for a process without network. Seatbelt renders the rules of A and the low-port denies. Bubblewrap ignores it, because the namespace already gives it.
- `Plan::loopback_ports: Vec<u16>`: for bash commands. Seatbelt fills `Invocation::ports` from it. Bubblewrap ignores it, because it uses the relay.

A new backend method, `fn direct_loopback(&self) -> bool` (Seatbelt true, bubblewrap false), decides two things:
- whether `bash.rs::relay_for` builds binds (bubblewrap) or the plan carries `loopback_ports` (Seatbelt);
- whether `ProcessManager::launch` uses the relay or the bare command.

`processes()` turns true for Seatbelt once A is in.

With `network: true`, the process keeps today's networked profile: outbound IP, `network-bind (local ip)` on any address, and inbound on localhost. That's the same as Linux, where a networked process shares the host's network and a `0.0.0.0` bind is the user's choice through the approval.

### 4.2 Finding the ports

The process group's id is the guard's pid, which is the `Child::id()` Marv already has. There is no `sandbox_pid` step.

- **Recommended: the `libproc` crate** (a macOS-only dependency of `marv-session`, MIT):
  1. `pids_by_type(ProcFilter::ByProgramGroup { pgrpid })` for the group's members;
  2. `listpidinfo::<ListFDs>` for each member's descriptors;
  3. `pidfdinfo::<SocketFDInfo>` for each socket, keeping TCP sockets in `LISTEN` state.

  It runs in-process and costs microseconds. It has no `unsafe` on our side, so `unsafe_code = "forbid"` holds. API names to be checked against the crate version.
- **Alternative: `lsof -a -g <pgid> -iTCP -sTCP:LISTEN -Fn -P -n`** (`/usr/sbin/lsof`, by full path). It needs no new dependency, but it spawns a process every 500 ms per running process (up to 8 a second) and takes tens of milliseconds each time.

Either one runs in Marv, outside the sandbox. Seatbelt limits what the sandboxed process may inspect, not what its parent may (to verify: probe 5).

`listening_ports`' rules stay the same: ports `>= MIN_PORT`, each once, sorted, and the first `MAX_PORTS` listed. Ports over the limit get the note "port N is open on your localhost but isn't listed: at most 4 ports per process". It isn't "not forwarded", because it *is* reachable from the host (§5.2).

### 4.3 What the model and the user see

- **The process description** (`process.rs:32`) gets a macOS variant, compiled everywhere like bash's two descriptions (`bash.rs:494-498`), so the oracle and golden tests can check both. "Forwarded to the user's localhost" becomes "listen on the user's localhost directly; bind 127.0.0.1 or localhost, not 0.0.0.0". The start card's line `its ports forwarded to your 127.0.0.1` (`process.rs:213`) becomes `its ports on your localhost`.
- **The prompt's door paragraph** (`prompt.rs:136`) gets the same change. While the door is off, `SANDBOX_LIMITS`'s "a sandboxed command's `localhost` is its own" is wrong on macOS today, where the sandbox has no loopback at all. That gets fixed in the same PR.
- **Notes** keep their wording where it still fits. "port N is in use on the user's machine" isn't used on macOS, because the server reports `EADDRINUSE` itself.

## 5. Security

### 5.1 Compared with Linux

| Property | Linux (bwrap + relay) | macOS, option A | Residual |
|---|---|---|---|
| Reaches the internet without network | No (own netns) | No (no outbound IP rule) | None |
| Reaches the user's loopback services | No | No (no outbound IP rule) | None |
| Reachable from the host's loopback | Yes, through Marv's forwarder | Yes, directly | Same: any local process, the browser too, can connect. A page it serves can try DNS rebinding or CSRF against other local services from the user's browser, as on Linux |
| Reachable from the LAN | No (forwarder on `127.0.0.1`) | No, **if probe 1 holds** | A ships only if it does |
| Can take a port the user's software expects (squatting, e.g. 11434 before Ollama starts) | Yes: Marv forwards any free port of 1024+ | Yes, on the same terms | Same. Documented |
| Ports below 1024 | Bound only inside the namespace, never forwarded | Denied by rule (addition 2) | Only after probe 3/4 |
| More than `MAX_PORTS` | Forwarded only up to 4 | All open on loopback, 4 listed | Extra ports are reachable from the host, not refused (decision 3) |
| Unix sockets | Only the relay's | Only its private `TMPDIR` (the profile as for bash) | None |
| Stale port in a bash rule | The relay reaches only the process's netns | A port the process closed, which another program then took, is reachable by a bash command started before Marv's next poll (≤500 ms) | Small window. The rule lists ports, not processes |
| Same port, other address family | n/a | `localhost:<p>` in SBPL matches both `127.0.0.1` and `::1` (to verify, probe 2). A process binding `::1:5432` while the user's Postgres has `127.0.0.1:5432` would make Marv open `localhost:5432` to bash commands, reaching Postgres | Closed by addition 4: Marv holds the other family, or withholds the rule |
| Escapees (`setsid` + double fork) | Die with the namespace | Survive the command, still sandboxed. Their ports aren't in the group, so they aren't listed but stay open on loopback until they exit | Sandbox spec decision 5, and now a port too. Documented |
| Dies on stop / exit / panic / SIGKILL | bwrap | Guard `killpg` + post-exit kill (addition 3) + parent watch | The guard limits of sandbox spec §6.10 |

**Seatbelt outbound to localhost:** A gives a process without network **no** `network-outbound` IP rule at all. Accepted connections are inbound and need nothing more. The one thing it loses is connecting to itself (§5.3). Giving it `(remote ip "localhost:*")` would undo sandbox decision 1, so this spec doesn't.

### 5.2 Why ports can't be refused up front

On Linux, a port is useful only once Marv forwards it, so `MIN_PORT`, `MAX_PORTS` and "in use" are all decided before anyone can connect. On macOS, the bind *is* the exposure: by the time the poll sees a port, it's reachable. The profile can refuse whole classes of port (the wildcard address, the low ports) but not "the fifth port". Seatbelt rules are static and have no count. Decision 3 asks whether a process over the limit should be stopped rather than noted.

### 5.3 Limits a model will hit

- **A server that calls itself** (SSR fetching `http://localhost:3000/api`, `wait-on tcp:5173` in a `concurrently` script, a health check in the start command) gets "Operation not permitted". On Linux, its own namespace allows it. The way out is `network: true` (it asks). The start result's `EPERM` note mentions it. A `ports` hint on `start`, which would allow outbound to named ports that are free at the start, is in §8.
- **Port conflicts with the user's machine.** There's no namespace, so ports are shared. A dev server on 3000 while the user runs something on 3000 fails with `EADDRINUSE`, or picks the next port (Vite and Next do), and the poll lists whichever it got. On Linux the server always binds, and Marv's forward is what fails, with a note. Two Marv sessions starting servers on the same port behave the same way. This is arguably clearer than on Linux, since the server's own message says what happened.

## 6. The runtime folder

Nothing changes for A. With no relay, macOS makes no per-process socket folder, so the processes use the runtime folder only for what it already holds (mesh, plugs, file locks). `fallback_dir` (`<temp>/marv-<uid>/marv`) stays for Linux without a runtime folder. On macOS, `runtime_dir_for` always answers `/tmp/marv-<uid>` unless that folder fails its checks, and then the door stays off with the message, as the mesh does.

## 7. To verify on the macOS runner (before the code relies on it)

Each item is a probe in `crates/marv-sandbox` tests (macOS only), the way #166's probes were.

1. With `listen_localhost`: a bind to `127.0.0.1:0` and to `[::1]:0` works, and a bind to `0.0.0.0:0` and `[::]:0` is refused. **Blocking:** if wildcard binds pass, A doesn't ship (decision 1).
2. `(remote ip "localhost:<p>")` reaches both `127.0.0.1:<p>` and `[::1]:<p>`. This decides addition 4's exact form.
3. An unsandboxed user process can bind `127.0.0.1:80` on macOS 14 and 15. If it can't, addition 2 is defence in depth only.
4. The 1023 per-port denies work, and `sandbox-exec` start time with them stays under +20 ms (median of 50).
5. Marv (outside) can read a sandboxed child's descriptors through `proc_pidfdinfo` (or `lsof -g`), and the process can't read Marv's.
6. An accepted connection needs only `network-inbound` (a `curl` from the host to the process works). The process's own `connect` to its port is refused.
7. `clean-tmps` (sandbox spec §7.2) is unaffected: there are no socket folders now.

## 8. Later, on both platforms

- **Process-to-process reach** (a front end proxying to an API process). Linux needs a reverse relay in the process's sandbox. macOS needs outbound rules to the ports of processes already running at the start, since profiles are fixed at launch.
- **A `ports` hint on `start`** for servers that call themselves. On macOS: outbound to those ports, refused if they're taken at start, with both families held by Marv. On Linux: nothing needed.
- **Escapees' ports:** finding listeners by sandbox rather than by group. There's no public API for it today.

## 9. Tests

- **Unit (Linux CI, the profile builder is pure):**
  - `listen_localhost` renders bind and inbound on `localhost:*`, no outbound IP, and the 1-1023 denies after the allow;
  - `loopback_ports` renders one outbound rule per port;
  - bubblewrap ignores both;
  - with `network: true`, the profile is unchanged.
- **Unit (`processes`):** the port parser for the chosen source (libproc rows, or `lsof -F` text from a fixture), `MAX_PORTS` notes, the other-family hold and its note.
- **Guards** (`tests/guards/tests/process.rs`, today `require_bwrap!()`): run on both platforms with a `require_sandbox!()` and one body per guarantee:
  - can't write outside the project, can't change `.git`, can't read the home: as today;
  - **can't reach the host's localhost without network**: as today, plus, on macOS, can't reach a host listener on the *same port number* in the other family;
  - **a port is on loopback only**: on macOS the server binding `0.0.0.0` fails, and binding `127.0.0.1` is reachable from the host but not on another interface;
  - **a sandboxed bash command reaches it** (`curl localhost:<p>`), and doesn't reach a host port that isn't listed;
  - **dies on stop**, including a child left in the group after the main command exited (addition 3);
  - **dies when Marv is SIGKILLed** (the fixture parent, as today).

  Each is proven by a scripted revert, as #166's were.
- **End to end:** `crates/marv-cli/tests/process_door.rs` (scripted model starts a dev server, bash curls it, the host reaches it, nothing is left after exit) runs on the macOS job.

## 10. Decisions for the maintainer

1. **Option A, conditional on probe 1.** A ships only if Seatbelt refuses wildcard binds under `(local ip "localhost:*")`. If it doesn't, C stays and this spec comes back. *Recommended: yes.*
2. **Low ports denied by profile rule** (1023 `deny network-bind` lines), if probe 4's cost is acceptable. *Recommended: yes.* The alternative is to note them only, as for ports over the limit.
3. **Ports over `MAX_PORTS`:** listed as "open but not listed" with a note (recommended), or the process stopped. Stopping is stricter, but it kills a dev server that opens an extra debug or HMR port.
4. **The other address family:** Marv holds the other loopback address for each listed port, or withholds the bash rule if it can't (recommended). The alternative is no bash rule for any port whose other family is taken, without holding it.
5. **Port discovery:** the `libproc` crate (recommended: in-process, cheap, a new macOS-only dependency) or `lsof` (no dependency, a spawn per poll).
6. **Self-connection** stays a documented limit, with `network: true` as the way out, and the `ports` hint deferred to §8. *Recommended: yes.*
7. **`SANDBOX_LIMITS` wording on macOS** ("its `localhost` is its own" is wrong there) gets fixed in PR 1 even though it's a prompt change: one cache break, macOS only. *Recommended: yes.*

## 11. PRs

1. **Profile and probes** (`marv-sandbox`): `Plan::listen_localhost`, `Plan::loopback_ports`, `direct_loopback()`, the rules and the low-port denies, the backend filling `Invocation::ports`, the §7 probes on the macOS job, and the `SANDBOX_LIMITS` fix. The door stays off. Linux unchanged (golden profile tests).
2. **Processes on macOS** (`marv-session`, `marv-tools`): discovery, the post-exit group kill (both platforms, a no-op under bwrap), the other-family hold, no relay or socket folder on macOS, bash's `loopback_ports` path, descriptions and notes per platform, and `processes()` true for Seatbelt. Guards in `tests/guards/tests/process.rs` on both platforms, `process_door.rs` on macOS.
3. **Docs** with PR 2:
   - `docs/SECURITY.md`: the process rows in the "On macOS (Seatbelt)" table, replacing line 62's "not offered yet";
   - `docs/subsystems/session.md` and `docs/subsystems/sandbox.md`;
   - `docs/subsystems/cli.md:44` (the "no process door yet" aside);
   - the CHANGELOG.

The issue closes with PR 2.
