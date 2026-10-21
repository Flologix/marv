//! Background processes (Marv #51): servers, watchers, anything that should keep running after the bash command that
//! would have started it.
//!
//! Ports Marv's `src/processes.ts`. Each runs in its own long-lived bwrap sandbox, as a bash command does (only the
//! project writable, an empty home, no environment, `.git` read-only), but with its own network namespace kept: its
//! ports are forwarded to the host's 127.0.0.1, one by one, and nothing comes back. Sharing the host's network instead
//! would give every server outbound network and reach the user's own local services (databases, other dev servers).
//!
//! The forwarding goes through a Unix socket, the one thing that crosses a network namespace: the sandbox's relay
//! (`marv_tools::relay`, run as `marv __relay`) listens on a socket in a folder bind-mounted at `/run/marv`; Marv reads
//! the sandbox's listening ports from `/proc/<pid>/net/tcp{,6}` and, for each, listens on the same port of the host's
//! 127.0.0.1 (loopback only) and pipes every connection into the socket.
//!
//! - At most [`MAX_PROCESSES`] running, [`MAX_PORTS`] ports each, ports from [`MIN_PORT`]; logs keep the last
//!   [`MAX_LOG_CHARS`] (stdout and stderr together, cut at a whole line).
//! - `start` waits up to 10 s for the first port (or the end); an exited process stays listed with its code and logs
//!   until a start of the same name replaces it.
//! - `stop` ends its whole sandbox (SIGTERM, SIGKILL after 2 s): bwrap's `--die-with-parent` and its own PID namespace
//!   take everything inside down. `close_all` stops them all and refuses later starts. If Marv dies harder, the
//!   sandboxes die with it (`--die-with-parent`); [`kill_running`] is for Marv's exit path and a panic.
//! - A process started with `network: true` shares the host's network: its ports are already the user's (nothing is
//!   relayed), and only its own sockets count (`/proc/<pid>/net` shows the whole machine's there).
//!
//! marv-rs:
//! - The relay is a compiled hidden subcommand (`marv __relay`), Marv's binary mounted at `/run/marv-bin`; Marv sent
//!   the relay's source to `bun -e` (see `marv_tools::relay`).
//! - Protected files (docs/SECURITY.md): the sandbox mounts the existing ones read-only, as for bash; ones the process
//!   created are found by the same scan bash runs after a command, when it stops or exits, removed, and noted.
//! - `--die-with-parent` is the *thread* that started bwrap dying (`PR_SET_PDEATHSIG`): the start runs on an async
//!   task (a runtime worker or the `block_on` thread, which live as long as the runtime), never on a blocking-pool
//!   thread that could exit after an idle spell and take the process with it.

#[doc(hidden)]
pub mod fixture;

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use marv_sandbox::git_guard::git_guard;
use marv_sandbox::protected::{
    ProtectedSnapshot, created_note, nested_git_dirs, protected_snapshot, revert_created, scan_created,
};
use marv_sandbox::{Bind, CommandMounts, Prepared, Sandbox};
use marv_tools::relay::{RelaySpec, SOCKET_DIR, SOCKET_NAME};
use marv_tools::{ProcessForward, ProcessForwards, ProcessHost, ProcessInfo, ProcessState, StartResult, ToolError};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio_util::sync::CancellationToken;

pub use marv_tools::process::MAX_LOG_LINES;
pub use marv_tools::relay::MIN_PORT;

pub const MAX_PROCESSES: usize = 4;
pub const MAX_PORTS: usize = 4;
/// Logs kept per process (stdout and stderr together); older lines are dropped.
pub const MAX_LOG_CHARS: usize = 1_000_000;
const START_WAIT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(500);
const STOP_GRACE: Duration = Duration::from_secs(2);
const START_LINES: usize = 40;

/// Called when something changed (started, exited, a port forwarded): for the status bar.
pub type OnChange = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone)]
pub struct ProcessManagerOptions {
    pub root: PathBuf,
    /// The home folder the sandbox hides (default: the user's); tests pass a fake one.
    pub home: Option<PathBuf>,
    /// The folder this manager makes its own folder in, for its processes' sockets (default: Marv's runtime folder,
    /// `marv_sandbox::runtime_dir`). Its own is `<pid>-<random>`: two sessions in one program never share one, and each
    /// removes only its own.
    pub runtime_dir: Option<PathBuf>,
    pub on_change: Option<OnChange>,
    pub poll: Duration,
    pub start_wait: Duration,
    pub stop_grace: Duration,
    /// A one-shot session (`marv -p`): its processes end when Marv exits after the reply, and the process tool says so
    /// (#86).
    pub one_shot: bool,
}

impl ProcessManagerOptions {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            home: None,
            runtime_dir: None,
            on_change: None,
            poll: POLL,
            start_wait: START_WAIT,
            stop_grace: STOP_GRACE,
            one_shot: false,
        }
    }
}

/// One row of `/proc/<pid>/net/tcp{,6}`: the local port, whether it's listening, and the socket's inode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetRow {
    pub port: u16,
    pub listening: bool,
    pub inode: String,
}

pub fn parse_net_tcp(text: &str) -> Vec<NetRow> {
    let mut rows = Vec::new();
    for line in text.split('\n').skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // sl local_address rem_address st tx:rx tr:when retrnsmt uid timeout inode
        if fields.len() < 10 {
            continue;
        }
        let Some(port) = fields[1].split(':').nth(1).and_then(|p| u16::from_str_radix(p, 16).ok()) else { continue };
        rows.push(NetRow { port, listening: fields[3] == "0A", inode: fields[9].to_string() });
    }
    rows
}

/// The listening ports in a net/tcp text (only sockets in `inodes` when given), forwardable ones, sorted, each once.
pub fn listening_ports(text: &str, inodes: Option<&HashSet<String>>) -> Vec<u16> {
    let mut ports: Vec<u16> = parse_net_tcp(text)
        .into_iter()
        .filter(|r| r.listening && r.port >= MIN_PORT && inodes.is_none_or(|set| set.contains(&r.inode)))
        .map(|r| r.port)
        .collect();
    ports.sort_unstable();
    ports.dedup();
    ports
}

/// A name from the command: its words joined (`bun run dev` → `bun-run-dev`), short.
pub fn name_from(command: &str) -> String {
    let first = command.split([';', '&', '|', '\n']).next().unwrap_or("");
    let words: Vec<String> = first
        .split_whitespace()
        .map(|w| {
            let base = w.rsplit_once('/').map_or(w, |(_, b)| b);
            base.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')).collect()
        })
        .filter(|w: &String| !w.is_empty())
        .collect();
    let joined: String = words.join("-").chars().take(32).collect();
    let name = joined.trim_end_matches('-');
    if name.is_empty() { "process".into() } else { name.into() }
}

/// `^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$`
fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 64
        && chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn read(path: impl AsRef<Path>) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// The sandbox's first process (bwrap's child, in the sandbox's namespaces), or None before it exists.
fn sandbox_pid(bwrap: u32) -> Option<u32> {
    read(format!("/proc/{bwrap}/task/{bwrap}/children")).split_whitespace().next()?.parse().ok()
}

/// The socket inodes of every process in the sandbox's PID namespace (for a sandbox on the host's network).
fn sandbox_sockets(pid: u32) -> HashSet<String> {
    let mut inodes = HashSet::new();
    let Ok(ns) = std::fs::read_link(format!("/proc/{pid}/ns/pid")) else { return inodes };
    let Ok(procs) = std::fs::read_dir("/proc") else { return inodes };
    for p in procs.flatten() {
        let p = p.file_name();
        let Some(p) = p.to_str().filter(|p| p.bytes().all(|b| b.is_ascii_digit())) else { continue };
        if std::fs::read_link(format!("/proc/{p}/ns/pid")).ok().as_ref() != Some(&ns) {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(format!("/proc/{p}/fd")) else { continue };
        for fd in fds.flatten() {
            let Ok(target) = std::fs::read_link(fd.path()) else { continue };
            let target = target.to_string_lossy();
            if let Some(inode) = target.strip_prefix("socket:[").and_then(|t| t.strip_suffix(']')) {
                inodes.insert(inode.to_string());
            }
        }
    }
    inodes
}

/// The bwrap pids of every process running, in any manager: [`kill_running`] is for Marv's exit path.
static RUNNING: Mutex<Vec<u32>> = Mutex::new(Vec::new());

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Kills every background process still running, in every manager (Marv's exit, and a panic on the way out), through
/// the sandbox's backend (bwrap: SIGKILL).
pub fn kill_running() {
    for pid in lock(&RUNNING).drain(..) {
        marv_sandbox::backend().kill(pid);
    }
}

struct Entry {
    id: String,
    info: ProcessInfo,
    /// bwrap's pid.
    pid: Option<u32>,
    dir: PathBuf,
    log: Arc<Mutex<Vec<u8>>>,
    /// Each forwarded port's listener, closed by cancelling its token.
    listeners: Vec<(u16, CancellationToken)>,
    /// Cancels its open connections (and the listeners, and the poll).
    stop: CancellationToken,
    /// Ports already handled (forwarded, refused or over the limit), so each is tried once.
    seen: HashSet<u16>,
    exited: tokio::sync::watch::Receiver<bool>,
    /// What started it, for the backend's clean-up once it has ended (bwrap's placeholder mount points).
    sandboxed: Prepared,
    /// The project's protected files at the start, to find the ones the process adds.
    protected: Option<ProtectedSnapshot>,
    released: bool,
}

/// The processes' folder without a runtime folder: `<temp>/marv-<uid>/marv`. Each level gets the runtime folder's
/// checks (`marv_sandbox::private_folder`): made 0700, then it must be a folder of this user's, not a symlink, with
/// every folder on the way this user's or root's and not writable by others unless sticky; an existing one is made
/// 0700 again. Otherwise no process starts, and the error says which folder and why.
fn fallback_dir(temp: &Path) -> std::io::Result<PathBuf> {
    let base = temp.join(format!("marv-{}", nix::unistd::geteuid()));
    let dir = base.join("marv");
    for level in [&base, &dir] {
        marv_sandbox::private_folder(level).map_err(|e| {
            let named = e.to_string().contains(&*level.to_string_lossy());
            if named { e } else { std::io::Error::new(e.kind(), format!("{}: {e}", level.display())) }
        })?;
    }
    Ok(dir)
}

struct State {
    /// In start order (a replaced name goes to the end), as Marv's Map.
    entries: Vec<Entry>,
    count: u32,
    closed: bool,
    /// This manager's own socket folder, made at its first start.
    own_dir: Option<PathBuf>,
}

struct Shared {
    options: ProcessManagerOptions,
    sandbox: Sandbox,
    state: Mutex<State>,
}

/// The session's background processes. Cheap to clone.
#[derive(Clone)]
pub struct ProcessManager {
    shared: Arc<Shared>,
}

impl ProcessManager {
    pub fn new(options: ProcessManagerOptions) -> Self {
        let mut sandbox = Sandbox::from_env(true);
        if let Some(home) = &options.home {
            sandbox.home = home.clone();
        }
        let state = State { entries: Vec::new(), count: 0, closed: false, own_dir: None };
        Self { shared: Arc::new(Shared { options, sandbox, state: Mutex::new(state) }) }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        lock(&self.shared.state)
    }

    fn parent_dir(&self) -> std::io::Result<PathBuf> {
        use std::os::unix::fs::DirBuilderExt;
        if let Some(dir) = &self.shared.options.runtime_dir {
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
            return Ok(dir.clone());
        }
        // A short path: a Unix socket's path is limited to 107 bytes (103 on macOS). Marv's runtime folder
        // (`marv_sandbox::runtime_dir`, hidden in every sandbox), else as before a folder of the user's in the temp
        // folder ([`fallback_dir`]). A MARV_RUNTIME_DIR inside the project isn't used (#168): a sandboxed command could
        // have made it.
        match marv_sandbox::runtime_dir_for(&marv_sandbox::runtime_env(), Some(&self.shared.options.root)) {
            Some(dir) => {
                std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
                Ok(dir)
            }
            None => fallback_dir(&std::env::temp_dir()),
        }
    }

    /// Its own folder (0700), unique even among managers in one process: their process ids both start at p1.
    fn own_dir(&self, state: &mut State) -> std::io::Result<PathBuf> {
        use std::os::unix::fs::DirBuilderExt;
        if let Some(dir) = &state.own_dir {
            return Ok(dir.clone());
        }
        let parent = self.parent_dir()?;
        loop {
            let dir = parent.join(format!("{}-{}", std::process::id(), random_suffix()));
            match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => {
                    state.own_dir = Some(dir.clone());
                    return Ok(dir);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
    }

    pub fn list(&self) -> Vec<ProcessInfo> {
        self.state().entries.iter().map(|e| e.info.clone()).collect()
    }

    pub fn running(&self) -> usize {
        self.state().entries.iter().filter(|e| e.info.state == ProcessState::Running).count()
    }

    /// For a sandboxed bash command: the running processes' forwarded ports, and where each one's socket folder is.
    pub fn forwards(&self) -> Vec<ProcessForward> {
        self.state()
            .entries
            .iter()
            .filter(|e| e.info.state == ProcessState::Running && !e.info.network)
            .flat_map(|e| e.info.ports.iter().map(|&port| ProcessForward { port, id: e.id.clone(), dir: e.dir.clone() }))
            .collect()
    }

    /// Its last `lines` lines of output (at most [`MAX_LOG_LINES`]).
    pub fn logs(&self, name: &str, lines: usize) -> Result<String, ToolError> {
        let state = self.state();
        let Some(entry) = state.entries.iter().find(|e| e.info.name == name) else {
            return Err(ToolError::new(unknown(&state, name)));
        };
        let log = String::from_utf8_lossy(&lock(&entry.log)).into_owned();
        let all: Vec<&str> = log.strip_suffix('\n').unwrap_or(&log).split('\n').collect();
        let keep = lines.clamp(1, MAX_LOG_LINES);
        Ok(all[all.len().saturating_sub(keep)..].join("\n"))
    }

    pub async fn start(&self, command: &str, name: Option<&str>, network: bool) -> Result<StartResult, ToolError> {
        let taken = {
            let state = self.state();
            if state.closed {
                return Err(ToolError::new("Processes are shut down: Marv is closing."));
            }
            if let Some(name) = name
                && !valid_name(name)
            {
                return Err(ToolError::new(format!(
                    "A process name uses letters, digits, ., - and _ (at most 64), starting with a letter or digit: \"{name}\"."
                )));
            }
            let taken = name.and_then(|n| state.entries.iter().find(|e| e.info.name == n));
            if taken.is_some_and(|e| e.info.state == ProcessState::Running) {
                return Err(ToolError::new(format!(
                    "A process named \"{}\" is already running: stop it first, or pick another name.",
                    name.unwrap_or_default()
                )));
            }
            let running: Vec<&str> =
                state.entries.iter().filter(|e| e.info.state == ProcessState::Running).map(|e| e.info.name.as_str()).collect();
            if running.len() >= MAX_PROCESSES {
                return Err(ToolError::new(format!(
                    "At most {MAX_PROCESSES} processes run at once: stop one first ({}).",
                    running.join(", ")
                )));
            }
            taken.map(|e| e.info.name.clone())
        };
        // An exited process of that name is replaced.
        if let Some(taken) = taken {
            self.stop(&taken).await;
        }
        let id = self.launch(command, name, network)?;

        // Up to 10 s for the first port (or the end), checked more often than the steady poll.
        let deadline = tokio::time::Instant::now() + self.shared.options.start_wait;
        let mut exited = self.exited_of(&id);
        loop {
            let waiting = self
                .with_entry(&id, |e| e.info.state == ProcessState::Running && e.info.ports.is_empty() && e.info.notes.is_empty());
            if waiting != Some(true) || tokio::time::Instant::now() >= deadline {
                break;
            }
            self.poll(&id);
            if let Some(rx) = exited.as_mut() {
                let _ = tokio::time::timeout(Duration::from_millis(100), rx.wait_for(|done| *done)).await;
            } else {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        let state = self.state();
        let entry = state.entries.iter().find(|e| e.id == id).ok_or_else(|| ToolError::new("The process is gone."))?;
        let log = String::from_utf8_lossy(&lock(&entry.log)).into_owned();
        let output = log.split('\n').take(START_LINES).collect::<Vec<_>>().join("\n").trim_end().to_string();
        Ok(StartResult { info: entry.info.clone(), output })
    }

    /// Spawns its sandbox and the tasks that watch it; its id.
    fn launch(&self, command: &str, name: Option<&str>, network: bool) -> Result<String, ToolError> {
        let options = &self.shared.options;
        let root = &options.root;
        let mut state = self.state();
        let final_name = name.map_or_else(|| unique_name(&state, &name_from(command)), String::from);
        state.count += 1;
        let id = format!("p{}", state.count);
        let own = self.own_dir(&mut state).map_err(|e| ToolError::new(format!("Couldn't make the processes' folder: {e}")))?;
        let dir = own.join(&id);
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&dir)
                .map_err(|e| ToolError::new(format!("Couldn't make the process's folder: {e}")))?;
        }
        // As for a command that runs without asking: a long-lived process has no reason to write the repository, and a
        // hook planted there would run outside the sandbox (bash's git guard).
        let guard = git_guard(root);
        let mut read_only = guard.read_only.clone();
        read_only.extend(nested_git_dirs(root));
        let script = format!("exec 2>&1\n{command}");
        // With network it shares the host's: its ports are already the user's, so there's nothing to relay.
        let backend = self.shared.sandbox.backend();
        let (binds, inner): (Vec<Bind>, Vec<OsString>) = if network {
            (vec![], vec!["bash".into(), "-c".into(), script.into()])
        } else {
            let exe =
                marv_tools::relay::exe_bind().ok_or_else(|| ToolError::new("Couldn't find Marv's own binary for the relay."))?;
            let socket_dir = Bind { source: dir.clone(), dest: SOCKET_DIR.into(), writable: true };
            // Where the backend shows them inside (bwrap: their mount points), never assumed.
            let spec = RelaySpec {
                listen: Some(backend.guest_path(&socket_dir).join(SOCKET_NAME).to_string_lossy().into_owned()),
                connect: vec![],
                argv: vec!["bash".into(), "-c".into(), script],
            };
            let argv = marv_tools::relay::sandbox_argv_at(&backend.guest_path(&exe), &spec);
            (vec![exe, socket_dir], argv)
        };
        let mounts = CommandMounts {
            network,
            read_only,
            writable: vec![],
            placeholders: guard.placeholders,
            project_read_only: false,
            binds,
        };
        let sandboxed = self.shared.sandbox.prepare(root, &mounts, &inner).map_err(ToolError::from)?;
        let protected = Some(protected_snapshot(root));
        // Only the backend's environment, never Marv's. A spawn that fails drops `sandboxed`, which cleans up.
        let mut child = tokio::process::Command::from(sandboxed.to_command(root))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| ToolError::new(format!("Couldn't start the process: {e}")))?;
        let pid = child.id();
        if let Some(pid) = pid {
            lock(&RUNNING).push(pid);
        }
        let log: Arc<Mutex<Vec<u8>>> = Arc::default();
        let captures = [
            child.stdout.take().map(|s| tokio::spawn(capture(s, log.clone()))),
            child.stderr.take().map(|s| tokio::spawn(capture(s, log.clone()))),
        ];
        let (done, exited) = tokio::sync::watch::channel(false);
        let stop = CancellationToken::new();
        let info = ProcessInfo {
            name: final_name,
            command: command.to_string(),
            network,
            state: ProcessState::Running,
            exit_code: None,
            ports: vec![],
            notes: vec![],
            started_at: marv_tools::process::now_ms(),
        };
        state.entries.retain(|e| e.info.name != info.name);
        state.entries.push(Entry {
            id: id.clone(),
            info,
            pid,
            dir,
            log,
            listeners: vec![],
            stop: stop.clone(),
            seen: HashSet::new(),
            exited,
            sandboxed,
            protected,
            released: false,
        });
        drop(state);

        // Its end: the last output in first, then exited, released, and the change announced.
        let manager = self.clone();
        let watched = id.clone();
        tokio::spawn(async move {
            let status = child.wait().await;
            if let Some(pid) = pid {
                lock(&RUNNING).retain(|p| *p != pid);
            }
            for capture in captures.into_iter().flatten() {
                let _ = tokio::time::timeout(Duration::from_secs(1), capture).await;
            }
            let code = status.map(marv_tools::relay::exit_code).unwrap_or(1);
            manager.with_entry_mut(&watched, |e| {
                e.info.state = ProcessState::Exited;
                e.info.exit_code = Some(code);
            });
            manager.release(&watched);
            let _ = done.send(true);
            manager.changed();
        });
        // The steady poll for new ports, until it ends.
        let manager = self.clone();
        let polled = id.clone();
        let every = self.shared.options.poll;
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = stop.cancelled() => return,
                    () = tokio::time::sleep(every) => manager.poll(&polled),
                }
            }
        });
        self.changed();
        Ok(id)
    }

    /// Stops it (its whole sandbox), closes its forwarded ports and forgets it. False when there's none of that name.
    pub async fn stop(&self, name: &str) -> bool {
        let entry = {
            let mut state = self.state();
            let Some(i) = state.entries.iter().position(|e| e.info.name == name) else { return false };
            state.entries.remove(i)
        };
        let mut entry = entry;
        if entry.info.state == ProcessState::Running {
            // bwrap's sandbox dies with it (--die-with-parent kills everything inside; its own PID namespace has no way
            // out).
            let mut exited = entry.exited.clone();
            let backend = self.shared.sandbox.backend();
            if let Some(pid) = entry.pid {
                backend.terminate(pid);
            }
            let grace = self.shared.options.stop_grace;
            if tokio::time::timeout(grace, exited.wait_for(|d| *d)).await.is_err() {
                if let Some(pid) = entry.pid {
                    backend.kill(pid);
                }
                let _ = exited.wait_for(|d| *d).await;
            }
            // Its watcher marks the listed entry, which is gone from the list by now: this copy ended too.
            entry.info.state = ProcessState::Exited;
        }
        release_entry(&mut entry, &self.shared.options.root);
        self.changed();
        true
    }

    /// Stops every process (Marv's exit, the session's close); later starts refuse.
    pub async fn close_all(&self) {
        let names: Vec<String> = {
            let mut state = self.state();
            state.closed = true;
            state.entries.iter().map(|e| e.info.name.clone()).collect()
        };
        futures::future::join_all(names.iter().map(|n| self.stop(n))).await;
        // Only its own folder: another session's processes may be using the parent.
        if let Some(dir) = self.state().own_dir.clone() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// Tests only: a forwarder failing after it started (EMFILE, say), as its accept loop reports one.
    #[doc(hidden)]
    pub fn fail_forward(&self, name: &str, port: u16, error: &str) {
        let id = self.state().entries.iter().find(|e| e.info.name == name).map(|e| e.id.clone());
        if let Some(id) = id {
            self.drop_port(&id, port, error);
        }
    }

    fn changed(&self) {
        if let Some(on_change) = &self.shared.options.on_change {
            // The client's redraw failing is the client's problem.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| on_change()));
        }
    }

    fn with_entry<T>(&self, id: &str, f: impl FnOnce(&Entry) -> T) -> Option<T> {
        self.state().entries.iter().find(|e| e.id == id).map(f)
    }

    fn with_entry_mut<T>(&self, id: &str, f: impl FnOnce(&mut Entry) -> T) -> Option<T> {
        self.state().entries.iter_mut().find(|e| e.id == id).map(f)
    }

    fn exited_of(&self, id: &str) -> Option<tokio::sync::watch::Receiver<bool>> {
        self.with_entry(id, |e| e.exited.clone())
    }

    fn release(&self, id: &str) {
        let root = self.shared.options.root.clone();
        self.with_entry_mut(id, |e| release_entry(e, &root));
    }

    /// New listening ports, each forwarded once (or noted).
    fn poll(&self, id: &str) {
        let mut changed = false;
        {
            let mut state = self.state();
            let Some(entry) = state.entries.iter_mut().find(|e| e.id == id) else { return };
            if entry.info.state != ProcessState::Running {
                return;
            }
            let Some(pid) = entry.pid.and_then(sandbox_pid) else { return };
            let six = read(format!("/proc/{pid}/net/tcp6"));
            let text = format!("{}{}", read(format!("/proc/{pid}/net/tcp")), six.split_once('\n').map_or("", |(_, rest)| rest));
            // On the host's network, /proc/<pid>/net shows every socket on the machine: only the sandbox's own count.
            let ports = if entry.info.network {
                listening_ports(&text, Some(&sandbox_sockets(pid)))
            } else {
                listening_ports(&text, None)
            };
            for port in ports {
                if !entry.seen.insert(port) {
                    continue;
                }
                changed = true;
                if entry.info.ports.len() >= MAX_PORTS {
                    entry.info.notes.push(format!("port {port} isn't forwarded: at most {MAX_PORTS} ports per process."));
                    continue;
                }
                if entry.info.network {
                    entry.info.ports.push(port);
                    continue;
                }
                match self.forward(entry, port) {
                    Some(token) => {
                        entry.listeners.push((port, token));
                        entry.info.ports.push(port);
                    }
                    None => entry.info.notes.push(format!(
                        "port {port} is in use on the user's machine, so it isn't forwarded: start the server on another port."
                    )),
                }
            }
        }
        if changed {
            self.changed();
        }
    }

    /// Listens on the host's 127.0.0.1:<port> (loopback only) and pipes each connection through the process's socket.
    fn forward(&self, entry: &Entry, port: u16) -> Option<CancellationToken> {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).ok()?;
        listener.set_nonblocking(true).ok()?;
        let listener = tokio::net::TcpListener::from_std(listener).ok()?;
        let token = entry.stop.child_token();
        let socket = entry.dir.join(SOCKET_NAME);
        let (manager, id, closed) = (self.clone(), entry.id.clone(), token.clone());
        tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    () = closed.cancelled() => return,
                    accepted = listener.accept() => accepted,
                };
                match accepted {
                    Ok((client, _)) => {
                        let (socket, closed) = (socket.clone(), closed.clone());
                        tokio::spawn(async move {
                            tokio::select! {
                                () = closed.cancelled() => {}
                                () = marv_tools::relay::through_socket(client, port, socket) => {}
                            }
                        });
                    }
                    // From here on an error (EMFILE, say) can't be answered by retrying in a loop: the port goes
                    // instead, and the model is told why.
                    Err(e) => {
                        manager.drop_port(&id, port, &e.to_string());
                        return;
                    }
                }
            }
        });
        Some(token)
    }

    /// A forwarder that failed after it started: closed, its port no longer listed, and the model told why.
    fn drop_port(&self, id: &str, port: u16, error: &str) {
        let dropped = self.with_entry_mut(id, |e| {
            let Some(i) = e.listeners.iter().position(|(p, _)| *p == port) else { return false };
            e.listeners.remove(i).1.cancel();
            e.info.ports.retain(|p| *p != port);
            e.info.notes.push(format!("port {port} stopped being forwarded ({error}): restart the process to forward it again."));
            true
        });
        if dropped == Some(true) {
            self.changed();
        }
    }
}

/// What a process holds on the host: its poll, listeners, connections, socket folder and `.git` placeholder; and the
/// protected files it created, removed.
fn release_entry(entry: &mut Entry, root: &Path) {
    entry.stop.cancel();
    entry.listeners.clear();
    if entry.released {
        return;
    }
    if entry.info.state != ProcessState::Exited {
        // Still running (a stop that couldn't wait): the scan waits for the end.
        return;
    }
    entry.released = true;
    let _ = std::fs::remove_dir_all(&entry.dir);
    // What the sandbox left behind (bwrap: a placeholder's empty mount point, removed only while it's empty).
    entry.sandboxed.finish();
    if let Some(before) = entry.protected.take() {
        let mut created = scan_created(root, &before);
        revert_created(root, &mut created);
        if let Some(note) = created_note(&created) {
            entry.info.notes.push(note);
        }
    }
}

fn unknown(state: &State, name: &str) -> String {
    let names: Vec<&str> = state.entries.iter().map(|e| e.info.name.as_str()).collect();
    if names.is_empty() {
        format!("No process named \"{name}\". None is running.")
    } else {
        format!("No process named \"{name}\". Processes: {}.", names.join(", "))
    }
}

fn unique_name(state: &State, base: &str) -> String {
    let taken = |n: &str| state.entries.iter().any(|e| e.info.name == n);
    if !taken(base) {
        return base.to_string();
    }
    (2..).map(|n| format!("{base}-{n}")).find(|n| !taken(n)).unwrap_or_default()
}

fn random_suffix() -> String {
    use std::hash::{BuildHasher, Hasher};
    let n = std::collections::hash_map::RandomState::new().build_hasher().finish();
    format!("{:06x}", n & 0xff_ffff)
}

/// Keeps the last [`MAX_LOG_CHARS`] of a stream, cut at a whole line.
async fn capture(mut stream: impl AsyncRead + Unpin, log: Arc<Mutex<Vec<u8>>>) {
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                let mut log = lock(&log);
                log.extend_from_slice(&chunk[..n]);
                if log.len() > MAX_LOG_CHARS {
                    let cut = log.len() - MAX_LOG_CHARS;
                    let start = log[cut..].iter().position(|b| *b == b'\n').map_or(cut, |i| cut + i + 1);
                    log.drain(..start);
                }
            }
        }
    }
}

#[async_trait]
impl ProcessHost for ProcessManager {
    async fn start(&self, command: &str, name: Option<&str>, network: bool) -> Result<StartResult, ToolError> {
        ProcessManager::start(self, command, name, network).await
    }
    fn logs(&self, name: &str, lines: usize) -> Result<String, ToolError> {
        ProcessManager::logs(self, name, lines)
    }
    async fn stop(&self, name: &str) -> bool {
        ProcessManager::stop(self, name).await
    }
    fn list(&self) -> Vec<ProcessInfo> {
        ProcessManager::list(self)
    }
    fn one_shot(&self) -> bool {
        self.shared.options.one_shot
    }
}

impl ProcessForwards for ProcessManager {
    fn forwards(&self) -> Vec<ProcessForward> {
        ProcessManager::forwards(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TCP: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:12AD 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 111 1 0000000079661a00 100 0 0 10 0
   1: 0100007F:0050 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 112 1 0000000079661a00 100 0 0 10 0
   2: 0100007F:12AD 0100007F:D431 01 00000000:00000000 00:00000000 00000000  1000        0 113 1 0000000079661a00 100 0 0 10 0
";
    const TCP6: &str = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000001000000:1F90 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 211 1 000000003b38aaa7 100 0 0 10 0
";

    fn both() -> String {
        format!("{TCP}{}", TCP6.split_once('\n').unwrap().1)
    }

    #[test]
    fn listening_ports_state_0a_hex_v4_and_v6_at_least_1024_each_once() {
        let rows: Vec<(u16, bool)> = parse_net_tcp(TCP).into_iter().map(|r| (r.port, r.listening)).collect();
        assert_eq!(rows, [(4781, true), (80, true), (4781, false)]);
        assert_eq!(listening_ports(TCP, None), [4781]); // 80 is below 1024; the established row isn't listening
        assert_eq!(listening_ports(TCP6, None), [8080]);
        assert_eq!(listening_ports(&both(), None), [4781, 8080]);
    }

    #[test]
    fn only_the_given_sockets_for_a_sandbox_on_the_hosts_network() {
        let inodes: HashSet<String> = ["211".to_string()].into();
        assert_eq!(listening_ports(&both(), Some(&inodes)), [8080]);
    }

    #[test]
    fn a_name_from_the_command() {
        assert_eq!(name_from("bun run dev"), "bun-run-dev");
        assert_eq!(name_from("./node_modules/.bin/vite --port 5173 && echo"), "vite---port-5173");
        assert_eq!(name_from("$$$"), "process");
    }

    #[test]
    fn names_are_letters_digits_dot_dash_underscore_up_to_64() {
        assert!(valid_name("web") && valid_name("a.b_c-1") && valid_name(&"a".repeat(64)));
        assert!(!valid_name("-web") && !valid_name("") && !valid_name("a b") && !valid_name(&"a".repeat(65)));
    }

    /// A fresh folder under /tmp standing in for the temp folder.
    fn temp_root(tag: &str) -> PathBuf {
        let dir = PathBuf::from(format!("/tmp/mx-pf-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn the_fallback_folder_is_made_0700_at_every_level() {
        use std::os::unix::fs::PermissionsExt;
        let temp = temp_root("mk");
        let base = temp.join(format!("marv-{}", nix::unistd::geteuid()));
        let dir = fallback_dir(&temp).unwrap();
        assert_eq!(dir, base.join("marv"));
        assert_eq!((mode(&base), mode(&dir)), (0o700, 0o700));
        // Existing levels with wider modes are made 0700 again.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(fallback_dir(&temp).unwrap(), dir);
        assert_eq!((mode(&base), mode(&dir)), (0o700, 0o700));
        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn a_symlinked_level_of_the_fallback_folder_is_refused() {
        let temp = temp_root("ln");
        let elsewhere = temp.join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        let base = temp.join(format!("marv-{}", nix::unistd::geteuid()));
        // `marv-<uid>` a symlink: refused, nothing made where it leads.
        std::os::unix::fs::symlink(&elsewhere, &base).unwrap();
        let why = fallback_dir(&temp).unwrap_err().to_string();
        assert!(why.contains(&base.display().to_string()), "{why}");
        assert!(!elsewhere.join("marv").exists());
        // `marv` inside it a symlink: refused too.
        std::fs::remove_file(&base).unwrap();
        std::fs::create_dir(&base).unwrap();
        std::os::unix::fs::symlink(&elsewhere, base.join("marv")).unwrap();
        let why = fallback_dir(&temp).unwrap_err().to_string();
        assert!(why.contains(&base.join("marv").display().to_string()), "{why}");
        // A temp folder reached through a symlink: refused, not followed.
        let via = temp.join("via");
        std::os::unix::fs::symlink(&elsewhere, &via).unwrap();
        let why = fallback_dir(&via).unwrap_err().to_string();
        assert!(why.contains("symlink"), "{why}");
        let _ = std::fs::remove_dir_all(&temp);
    }

    #[tokio::test]
    async fn logs_keep_the_last_megabyte_from_a_whole_line() {
        let log: Arc<Mutex<Vec<u8>>> = Arc::default();
        let text: String = (1..=300_000).map(|n| format!("{n}\n")).collect();
        capture(text.as_bytes(), log.clone()).await;
        let kept = lock(&log);
        assert!(kept.len() <= MAX_LOG_CHARS && kept.len() > MAX_LOG_CHARS - 10);
        assert_ne!(kept[0], b'\n');
        assert!(kept.ends_with(b"299999\n300000\n"));
    }
}
