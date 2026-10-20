//! Finding other Marvs on this machine, and wiring across them (mesh v2). New in marv-rs (docs/MESH.md, topology 2).
//!
//! Each switchboard also listens on `$XDG_RUNTIME_DIR/marv/mesh/<session>.sock` (folder 0700, socket 0600: the user
//! only), its **door** ([`open_door`]). `<session>` is the process's mesh name ([`mesh_name`]): `MARV_MESH_NAME`, or
//! the project folder's name and four hex digits. It names the process, not the conversation, so it stays the same
//! across `/clear` and resume. Without `XDG_RUNTIME_DIR` there's no discovery: Marv won't fall back to a
//! world-readable `/tmp`. Sandboxes never see the folder (`/run` and `/tmp` are empty tmpfs in bwrap).
//!
//! The door speaks newline-delimited JSON-RPC, like the plugs:
//! - `marv/info`: the name, pid, folder, Herdr pane and agents (`marv mesh ls`, [`list`]);
//! - `marv/plug {name}`: a new plug with no wires, for the stdio↔socket shim (`marv mesh plug`) that lets another
//!   agent (Claude Code, opencode) in as an MCP client. Only new names: never an existing plug's socket (that would
//!   let the caller speak as that agent), never `parent`, `worker-*` or `peer-*`. The plug goes when the connection
//!   that asked for it closes;
//! - `marv/connect {from, to, mode}`: `marv mesh wire`. This Marv's user is asked, and for another Marv's receptacle
//!   that Marv's user too. Running `marv mesh wire` asks; it doesn't approve (see the port notes in mesh.md);
//! - `marv/wire {from, to, mode, by}`: another switchboard asks for the far end of a cross-process wire. The caller
//!   must be the process listening on `<from>.sock` (its pid, from the socket's peer credentials, never from the
//!   message), and `to` must be one of this Marv's inboxes. If this Marv's user approves, the caller gets a plug of its
//!   own here, `peer-<from>`, wired with both approvals (`Grant::remote`), and its socket's path;
//! - `marv/radio.join {from, presence}` (#124): another Marv on the radio asks for a link. Refused unless this Marv's
//!   radio is on; the caller is checked by pid exactly as for `marv/wire`. Accepted, the connection becomes the link
//!   ([`crate::radio`]).
//!
//! On the asking side another Marv's inbox is the receptacle `peer.<session>.agent.<agent>.inbox`, found on demand
//! ([`PeerResolver`]) and served by a [`PeerLink`]: its messages go out through that `peer-<session>` plug, so the
//! receiving switchboard checks, limits and logs them like any other plug's, and its model sees them fenced, from
//! `peer-<session>`. Both switchboards log every message.

use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use marv_mcp::jsonrpc::{Message, Request, RequestId, Response, code, decode_line, encode_line, method};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::sync::CancellationToken;

use crate::MeshError;
use crate::Result;
use crate::approval::WireRequest;
use crate::message::Envelope;
use crate::plug::{PlugClient, PlugId};
use crate::receptacle::{Receptacle, ReceptacleKind, ReceptacleName, valid_segment};
use crate::switchboard::{MAX_LINE_BYTES, PEER_PREFIX, Resolve, Service, Switchboard};
use crate::wire::{Approver, WireMode};

/// `marv/info`: who this switchboard is.
pub const INFO: &str = "marv/info";
/// `marv/plug`: a new, unwired plug (the shim).
pub const PLUG: &str = "marv/plug";
/// `marv/connect`: `marv mesh wire`.
pub const CONNECT: &str = "marv/connect";
/// `marv/wire`: the far end of a cross-process wire, asked by another switchboard.
pub const WIRE: &str = "marv/wire";

/// How long a door gets to answer `marv/info` (`marv mesh ls`, finding a peer). Approvals wait as long as a person
/// takes.
pub const DOOR_TIMEOUT: Duration = Duration::from_secs(2);
/// A remote plug is `peer-<session>`, one name segment: session names leave room for the prefix.
pub const MAX_SESSION_NAME: usize = 43;
const PEER_PLUG: &str = "peer-";

/// `$XDG_RUNTIME_DIR/marv/mesh`, or `None` without a runtime dir, or with one a sandboxed command could see
/// (`marv_sandbox::hidden_in_sandbox`).
pub fn mesh_dir(env: &marv_mcp::Env) -> Option<PathBuf> {
    let home = env.get("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
    let base = env.get("XDG_RUNTIME_DIR").filter(|d| marv_sandbox::hidden_in_sandbox(Path::new(d), &home))?;
    Some(Path::new(base).join("marv").join("mesh"))
}

/// A session's discovery socket. Session ids are one name segment, so the path can't escape the folder.
pub fn session_socket(dir: &Path, session: &str) -> Option<PathBuf> {
    valid_segment(session).then(|| dir.join(format!("{session}.sock")))
}

/// This process's name on the mesh: `MARV_MESH_NAME` when set (checked), else the project folder's name (lowercased,
/// other characters as `-`, at most 20) and four hex digits, e.g. `api-3f9a`.
pub fn mesh_name(env: &marv_mcp::Env, root: &Path) -> Result<String> {
    if let Some(name) = env.get("MARV_MESH_NAME").filter(|n| !n.is_empty()) {
        if valid_segment(name) && name.len() <= MAX_SESSION_NAME {
            return Ok(name.clone());
        }
        return Err(MeshError::BadName(format!(
            "MARV_MESH_NAME \"{name}\" isn't a mesh name: lowercase letters, digits, - and _, at most {MAX_SESSION_NAME}"
        )));
    }
    let folder = root.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    let mapped: String =
        folder.chars().map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' { c } else { '-' }).collect();
    let mut base: String = mapped.trim_matches(|c: char| !c.is_ascii_alphanumeric()).chars().take(20).collect();
    base = base.trim_end_matches(['-', '_']).to_string();
    if base.is_empty() {
        base = "marv".into();
    }
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    let suffix = (nanos ^ std::process::id().rotate_left(16)) & 0xffff;
    Ok(format!("{base}-{suffix:04x}"))
}

/// `peer.<session>.<remote>`: another Marv's receptacle, as this switchboard offers it.
pub fn peer_receptacle(session: &str, remote: &ReceptacleName) -> Result<ReceptacleName> {
    ReceptacleName::new(format!("{PEER_PREFIX}{session}.{remote}"))
}

/// The other Marv and its receptacle in a `peer.<session>.<remote>` name.
pub fn parse_peer(name: &ReceptacleName) -> Option<(String, ReceptacleName)> {
    let (session, remote) = name.as_str().strip_prefix(PEER_PREFIX)?.split_once('.')?;
    Some((session.to_string(), ReceptacleName::new(remote).ok()?))
}

/// The plug another Marv's messages arrive on here: `peer-<session>`.
pub fn peer_plug(session: &str) -> Result<PlugId> {
    PlugId::new(format!("{PEER_PLUG}{session}"))
}

/// Names a client may not take with `marv/plug`: Marv's own agents (`parent`, marv-session's `PARENT`, and
/// `worker-N`) and the peers'.
fn reserved(name: &str) -> bool {
    name == "parent" || name.starts_with("worker-") || name.starts_with(PEER_PLUG)
}

fn io(context: &str, err: std::io::Error) -> MeshError {
    MeshError::Io(format!("{context}: {err}"))
}

/// This process's effective uid, the owner of the files and sockets it makes. Through nix rather than `/proc/self`,
/// which macOS doesn't have (#150).
pub fn own_uid() -> Result<u32> {
    Ok(nix::unistd::geteuid().as_raw())
}

/// Makes `dir` this user's private folder (0700), refusing a symlink or someone else's folder.
pub fn private_folder(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(|e| io(&format!("can't make {}", dir.display()), e))?;
    let meta = std::fs::symlink_metadata(dir).map_err(|e| io(&dir.display().to_string(), e))?;
    if !meta.is_dir() || meta.uid() != own_uid()? {
        return Err(MeshError::Io(format!("{} isn't a folder of this user's", dir.display())));
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| io(&dir.display().to_string(), e))
}

/// A socket this user owns, mode 0600: anything else in the folder is never connected to.
pub fn check_socket(path: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| io(&path.display().to_string(), e))?;
    if !meta.file_type().is_socket() || meta.uid() != own_uid()? || meta.permissions().mode() & 0o777 != 0o600 {
        return Err(MeshError::Io(format!("{} isn't a private socket of this user's", path.display())));
    }
    Ok(())
}

/// What a door says about its switchboard (`marv/info`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerInfo {
    pub name: String,
    pub pid: u32,
    /// The project folder.
    pub cwd: String,
    /// The Herdr pane it runs in (`HERDR_PANE_ID`), if any.
    pub pane: Option<String>,
    /// Its agents' plugs (`parent`, `worker-1`, `peer-<session>`, a shim's).
    pub agents: Vec<String>,
    pub version: String,
    /// Whether its radio is on (#124). Missing from older Marvs: off.
    #[serde(default)]
    pub radio: bool,
}

/// A switchboard found by `marv mesh ls`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerSwitchboard {
    pub session: String,
    pub socket: PathBuf,
    pub info: PeerInfo,
}

/// Every live switchboard in the folder, owned by this user: sockets that aren't 0600 and this user's, or that nobody
/// answers on (a Marv that crashed), are skipped and left alone. A missing folder is an empty list.
pub async fn list(dir: &Path) -> Result<Vec<PeerSwitchboard>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io(&dir.display().to_string(), e)),
    };
    let meta = std::fs::symlink_metadata(dir).map_err(|e| io(&dir.display().to_string(), e))?;
    if !meta.is_dir() || meta.uid() != own_uid()? || meta.permissions().mode() & 0o077 != 0 {
        return Err(MeshError::Io(format!("{} isn't a private folder of this user's: not listing it", dir.display())));
    }
    // Every door at once: an unresponsive Marv costs DOOR_TIMEOUT once, not once per socket after it.
    let probes = entries.flatten().filter_map(|entry| {
        let file = entry.file_name().to_string_lossy().into_owned();
        let session = file.strip_suffix(".sock").filter(|s| valid_segment(s))?.to_string();
        let socket = entry.path();
        Some(async move {
            let info = async {
                let mut door = DoorClient::connect(&socket).await?;
                door.info().await
            };
            match tokio::time::timeout(DOOR_TIMEOUT, info).await {
                Ok(Ok(info)) => Some(PeerSwitchboard { session, socket, info }),
                _ => None,
            }
        })
    });
    let mut found: Vec<PeerSwitchboard> = futures::future::join_all(probes).await.into_iter().flatten().collect();
    found.sort_by(|a, b| a.session.cmp(&b.session));
    Ok(found)
}

/// A client on another switchboard's door. One request at a time.
pub struct DoorClient {
    read: BufReader<OwnedReadHalf>,
    write: OwnedWriteHalf,
    next_id: i64,
    /// The pid of the process listening on the door (its socket's peer credentials).
    pub pid: Option<i32>,
}

impl DoorClient {
    /// Connects to a door after checking it's a private socket of this user's.
    pub async fn connect(socket: &Path) -> Result<Self> {
        check_socket(socket)?;
        let stream = UnixStream::connect(socket).await.map_err(|e| io(&format!("can't reach {}", socket.display()), e))?;
        let pid = stream.peer_cred().ok().and_then(|c| c.pid());
        let (read, write) = stream.into_split();
        Ok(Self { read: BufReader::new(read), write, next_id: 0, pid })
    }

    /// One request and its result; a JSON-RPC error comes back as `MeshError::Io` with the door's message.
    pub async fn request(&mut self, name: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = RequestId::Number(self.next_id);
        let line = encode_line(&Message::Request(Request::new(id.clone(), name, Some(params))));
        self.write.write_all(line.as_bytes()).await.map_err(|e| io("the other Marv", e))?;
        loop {
            let mut line = String::new();
            let n = (&mut self.read)
                .take(MAX_LINE_BYTES as u64 + 1)
                .read_line(&mut line)
                .await
                .map_err(|e| io("the other Marv", e))?;
            if n == 0 || line.len() > MAX_LINE_BYTES {
                return Err(MeshError::Io("the other Marv closed the connection".into()));
            }
            if let Ok(Message::Response(r)) = decode_line(&line)
                && r.id == id
            {
                return match (r.result, r.error) {
                    (_, Some(err)) => Err(MeshError::Io(err.message)),
                    (result, None) => Ok(result.unwrap_or(Value::Null)),
                };
            }
        }
    }

    pub async fn info(&mut self) -> Result<PeerInfo> {
        let value = self.request(INFO, json!({})).await?;
        serde_json::from_value(value).map_err(|_| MeshError::Io("the other Marv's answer isn't marv/info".into()))
    }
}

/// Who approves wires on this end for the door: the session's person (the TUI), or `--approve mesh:<r>` headless.
/// Never yolo, never an "always" scope.
#[async_trait]
pub trait PeerApprover: Send + Sync {
    async fn approve_wire(&self, request: WireRequest) -> Option<Approver>;
}

/// What the door says about this process.
#[derive(Debug, Clone)]
pub struct DoorInfo {
    pub cwd: PathBuf,
    pub pane: Option<String>,
}

/// The open door. Dropping it stops serving and removes the socket.
pub struct DoorGuard {
    pub socket: PathBuf,
    cancel: CancellationToken,
}

impl Drop for DoorGuard {
    fn drop(&mut self) {
        self.cancel.cancel();
        let _ = std::fs::remove_file(&self.socket);
    }
}

struct Door {
    sb: Arc<Switchboard>,
    dir: PathBuf,
    info: DoorInfo,
    approver: Arc<dyn PeerApprover>,
}

/// Whether nothing listens on `socket` any more (the connection is refused). The connect is non-blocking: a live Marv
/// whose backlog is full answers "would block" at once instead of holding this one up, and counts as alive.
fn nobody_listens(socket: &Path) -> bool {
    use socket2::{Domain, SockAddr, Socket, Type};
    let (Ok(probe), Ok(addr)) = (Socket::new(Domain::UNIX, Type::STREAM, None), SockAddr::unix(socket)) else { return false };
    if probe.set_nonblocking(true).is_err() {
        return false;
    }
    matches!(probe.connect(&addr), Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused)
}

/// Removes the sockets of Marvs that are gone: a door's socket outlives a Marv that was killed or crashed (the guard's
/// drop never ran), and the folder filled up with them. Only a refused connection counts as gone ([`nobody_listens`]);
/// a busy Marv keeps its socket. The folder is this user's and private ([`private_folder`]), so these are all
/// this user's Marvs. Called with the folder's lock held ([`open_door`]), so no other Marv is replacing a socket.
fn sweep_dead(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_socket = entry.file_type().is_ok_and(|t| std::os::unix::fs::FileTypeExt::is_socket(&t));
        if !is_socket || path.extension().is_none_or(|e| e != "sock") {
            continue;
        }
        if nobody_listens(&path) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// [`sweep_dead`] with the folder's lock held, as [`open_door`] does it: the radio's re-list, and a link that was
/// lost (#124). Blocks while another Marv holds the lock. The lock file is never made here: a door opened in the folder
/// made it, and a folder on its way out isn't filled again.
pub fn sweep_locked(dir: &Path) {
    let Ok(lock) = std::fs::OpenOptions::new().write(true).open(dir.join(".lock")) else { return };
    if lock.lock().is_ok() {
        sweep_dead(dir);
    }
}

/// The caller of a door is who listens on `<from>.sock`: checked by pid (the socket's peer credentials), never taken
/// from the message. `marv/wire` and `marv/radio.join` both ask this.
async fn confirm_caller(dir: &Path, from: &str, caller: Option<i32>) -> Result<()> {
    let socket = session_socket(dir, from).ok_or_else(|| MeshError::BadName(from.to_string()))?;
    let listening = DoorClient::connect(&socket).await.ok().and_then(|d| d.pid);
    if caller.is_none() || listening != caller {
        return Err(MeshError::Io(format!("can't confirm the caller is the Marv \"{from}\"")));
    }
    Ok(())
}

/// Opens `sb`'s door at `<dir>/<sb.session>.sock` (0600, in the 0700 folder), serves it until the guard drops, and
/// lets `sb` find the other Marvs' inboxes in `dir` ([`PeerResolver`]). Refuses when a live Marv already has the
/// name; a dead one's socket is replaced. Needs a Tokio runtime.
pub fn open_door(sb: Arc<Switchboard>, dir: &Path, info: DoorInfo, approver: Arc<dyn PeerApprover>) -> Result<DoorGuard> {
    let name = sb.session.clone();
    let socket = session_socket(dir, &name)
        .filter(|_| name.len() <= MAX_SESSION_NAME)
        .ok_or_else(|| MeshError::BadName(format!("\"{name}\" isn't a mesh name")))?;
    private_folder(dir)?;
    // Sweeping and replacing sockets happen under one lock: otherwise a sweep that found a dead socket could remove it
    // after another Marv replaced it with its live one. Released when this function returns, the door bound.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(dir.join(".lock"))
        .map_err(|e| io("the mesh folder's lock", e))?;
    lock.lock().map_err(|e| io("the mesh folder's lock", e))?;
    sweep_dead(dir);
    if std::fs::symlink_metadata(&socket).is_ok() {
        if !nobody_listens(&socket) {
            return Err(MeshError::Io(format!("a Marv named \"{name}\" is already on the mesh (set MARV_MESH_NAME)")));
        }
        let _ = std::fs::remove_file(&socket);
    }
    let listener =
        std::os::unix::net::UnixListener::bind(&socket).map_err(|e| io(&format!("can't listen on {}", socket.display()), e))?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| io(&socket.display().to_string(), e))?;
    listener.set_nonblocking(true).map_err(|e| io("the door", e))?;
    let listener = UnixListener::from_std(listener).map_err(|e| io("the door", e))?;
    sb.set_peer_dir(dir.to_path_buf());
    sb.add_resolver(Arc::new(PeerResolver { dir: dir.to_path_buf(), own: name }));
    let door = Arc::new(Door { sb, dir: dir.to_path_buf(), info, approver });
    let cancel = CancellationToken::new();
    tokio::spawn(door.accept(listener, cancel.clone()));
    Ok(DoorGuard { socket, cancel })
}

impl Door {
    async fn accept(self: Arc<Self>, listener: UnixListener, cancel: CancellationToken) {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                conn = listener.accept() => match conn {
                    Ok((stream, _)) => {
                        tokio::spawn(Arc::clone(&self).connection(stream, cancel.clone()));
                    }
                    Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
                },
            }
        }
    }

    /// One client: requests in order. Plugs it asked for (`marv/plug`) go when it leaves.
    async fn connection(self: Arc<Self>, stream: UnixStream, cancel: CancellationToken) {
        let caller = stream.peer_cred().ok().and_then(|c| c.pid());
        let (read, mut write) = stream.into_split();
        let mut read = BufReader::new(read);
        let mut made: Vec<PlugId> = Vec::new();
        loop {
            let mut line = Vec::new();
            let mut limited = (&mut read).take(MAX_LINE_BYTES as u64 + 1);
            let n = tokio::select! {
                _ = cancel.cancelled() => break,
                n = limited.read_until(b'\n', &mut line) => n,
            };
            match n {
                Ok(0) | Err(_) => break,
                Ok(_) if line.len() > MAX_LINE_BYTES => break,
                Ok(_) => {}
            }
            let Ok(text) = std::str::from_utf8(&line) else { break };
            let Ok(Message::Request(request)) = decode_line(text) else { continue };
            if request.method == crate::radio::JOIN {
                let id = request.id.clone();
                match self.radio_join(request.params.unwrap_or(Value::Null), caller).await {
                    Ok((radio, accepted)) => {
                        let answer = Response::ok(id, json!({"presence": accepted.presence}));
                        if write.write_all(encode_line(&Message::Response(answer)).as_bytes()).await.is_ok() {
                            for plug in made.drain(..) {
                                self.sb.unplug(&plug);
                            }
                            // From here the connection is the radio link.
                            return radio.adopt(accepted, read, write);
                        }
                        break;
                    }
                    Err(err) => {
                        let answer = Response::err(id, code::INVALID_PARAMS, err);
                        if write.write_all(encode_line(&Message::Response(answer)).as_bytes()).await.is_err() {
                            break;
                        }
                        continue;
                    }
                }
            }
            let response = self.handle(request, caller, &mut made).await;
            if write.write_all(encode_line(&Message::Response(response)).as_bytes()).await.is_err() {
                break;
            }
        }
        for plug in made {
            self.sb.unplug(&plug);
        }
    }

    async fn handle(&self, request: Request, caller: Option<i32>, made: &mut Vec<PlugId>) -> Response {
        let id = request.id.clone();
        let params = request.params.unwrap_or(Value::Null);
        let text = |key: &str| params.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
        let result = match request.method.as_str() {
            method::INITIALIZE => Ok(json!({
                "protocolVersion": marv_mcp::jsonrpc::PROTOCOL_VERSION,
                "capabilities": {},
                "serverInfo": {"name": "marv-door", "version": env!("CARGO_PKG_VERSION")},
            })),
            method::PING => Ok(json!({})),
            INFO => Ok(serde_json::to_value(self.info()).unwrap_or(Value::Null)),
            PLUG => self.plug(&text("name"), made),
            CONNECT => self.connect(&text("from"), &text("to"), &text("mode")).await,
            WIRE => self.wire(&text("from"), &text("to"), &text("mode"), &text("by"), caller).await,
            _ => return Response::err(id, code::METHOD_NOT_FOUND, "the door doesn't know that method"),
        };
        match result {
            Ok(value) => Response::ok(id, value),
            Err(err) => Response::err(id, code::INVALID_PARAMS, err.to_string()),
        }
    }

    fn info(&self) -> PeerInfo {
        PeerInfo {
            name: self.sb.session.clone(),
            pid: std::process::id(),
            cwd: self.info.cwd.display().to_string(),
            pane: self.info.pane.clone(),
            agents: self.sb.agents().into_iter().map(String::from).collect(),
            version: env!("CARGO_PKG_VERSION").into(),
            radio: self.sb.radio().is_some_and(|r| r.is_on()),
        }
    }

    /// A new plug with no wires, under a name nobody has.
    fn plug(&self, name: &str, made: &mut Vec<PlugId>) -> Result<Value> {
        let id = PlugId::new(name)?;
        if reserved(name) || self.sb.has_plug(&id) {
            return Err(MeshError::BadName(format!("\"{name}\" is taken: pick another name for the plug")));
        }
        let plug = self.sb.plug(id.clone())?;
        made.push(id);
        Ok(json!({"plug": plug.id, "socket": plug.host_socket}))
    }

    /// `marv mesh wire`: this end's person approves, then (for another Marv's receptacle) that Marv's.
    async fn connect(&self, from: &str, to: &str, mode: &str) -> Result<Value> {
        let from = PlugId::new(from)?;
        if !self.sb.has_plug(&from) {
            return Err(MeshError::NoReceptacle(format!("agent {from} on {}", self.sb.session)));
        }
        let to = ReceptacleName::new(to)?;
        let mode: WireMode = mode.parse()?;
        let receptacle = self.sb.lookup(&to).await.ok_or_else(|| MeshError::NoReceptacle(to.to_string()))?;
        if !receptacle.supports(mode) {
            return Err(MeshError::NotServed { receptacle: to.to_string(), mode });
        }
        let request = WireRequest { from: from.clone(), to: to.clone(), mode, asked_by: from };
        let Some(by) = self.approver.approve_wire(request.clone()).await else {
            return Ok(json!({"wired": false, "reason": format!("{}'s user said no", self.sb.session)}));
        };
        match self.sb.grant(request, by).await {
            Ok(()) => {
                Ok(json!({"wired": true, "by": by, "cross_process": matches!(receptacle.kind, ReceptacleKind::Peer { .. })}))
            }
            Err(err) => Ok(json!({"wired": false, "reason": err.to_string()})),
        }
    }

    /// Another switchboard asks for the far end of a wire to one of this Marv's inboxes.
    async fn wire(&self, from: &str, to: &str, mode: &str, by: &str, caller: Option<i32>) -> Result<Value> {
        let remote_by: Approver = serde_json::from_value(Value::from(by))
            .map_err(|_| MeshError::BadName(format!("\"{by}\" isn't who approved a wire")))?;
        if from == self.sb.session || !valid_segment(from) || from.len() > MAX_SESSION_NAME {
            return Err(MeshError::BadName(format!("\"{from}\" isn't another Marv's name")));
        }
        // The caller is who listens on <from>.sock: checked by pid, never taken from the message.
        confirm_caller(&self.dir, from, caller).await?;
        let to = ReceptacleName::new(to)?;
        let mode: WireMode = mode.parse()?;
        let receptacle = self.sb.receptacle(&to).ok_or_else(|| MeshError::NoReceptacle(to.to_string()))?;
        if receptacle.kind != ReceptacleKind::Inbox || !receptacle.supports(mode) {
            return Err(MeshError::NotServed { receptacle: to.to_string(), mode });
        }
        let plug_id = peer_plug(from)?;
        let request = WireRequest { from: plug_id.clone(), to, mode, asked_by: plug_id.clone() };
        let Some(by_here) = self.approver.approve_wire(request.clone()).await else {
            return Ok(json!({"granted": false}));
        };
        let plug = self.sb.plug(plug_id)?;
        let mut wire = request.grant(by_here);
        wire.cross_process = true;
        wire.grant.remote = Some(remote_by);
        self.sb.wire(wire)?;
        Ok(json!({"granted": true, "by": by_here, "socket": plug.host_socket}))
    }
}

impl Door {
    /// `marv/radio.join` (#124): only while this Marv's radio is on, only from the process listening on `<from>.sock`
    /// (by pid, as `marv/wire`), then the radio's own checks ([`crate::radio::Radio::accept_join`]).
    async fn radio_join(
        &self,
        params: Value,
        caller: Option<i32>,
    ) -> std::result::Result<(Arc<crate::radio::Radio>, crate::radio::Accepted), String> {
        // No radio at all (`marv -p`) is off; one that's off says so itself (`accept_join`).
        let radio = self.sb.radio().ok_or_else(|| format!("{}'s radio is off", self.sb.session))?;
        let from = params.get("from").and_then(Value::as_str).unwrap_or_default();
        if from == self.sb.session || !valid_segment(from) || from.len() > MAX_SESSION_NAME {
            return Err(format!("\"{from}\" isn't another Marv's name"));
        }
        confirm_caller(&self.dir, from, caller).await.map_err(|e| e.to_string())?;
        let accepted = radio.accept_join(from, params.get("presence").unwrap_or(&Value::Null))?;
        Ok((radio, accepted))
    }
}

/// Finds `peer.<session>.agent.<agent>.inbox`: a live Marv named `<session>` in the folder that has that agent.
pub struct PeerResolver {
    pub dir: PathBuf,
    /// This switchboard's own name: never a peer of itself.
    pub own: String,
}

#[async_trait]
impl Resolve for PeerResolver {
    async fn resolve(&self, name: &ReceptacleName) -> Option<(Receptacle, Arc<dyn Service>)> {
        let (session, remote) = parse_peer(name)?;
        let agent = remote.inbox_owner()?.to_string();
        if session == self.own {
            return None;
        }
        let socket = session_socket(&self.dir, &session)?;
        let info =
            tokio::time::timeout(DOOR_TIMEOUT, async { DoorClient::connect(&socket).await?.info().await }).await.ok()?.ok()?;
        if !info.agents.contains(&agent) {
            return None;
        }
        let template = Receptacle::inbox(PlugId::new(agent.clone()).ok()?);
        let receptacle = Receptacle {
            name: name.clone(),
            owner: None,
            kind: ReceptacleKind::Peer { session: session.clone() },
            description: format!("{agent}'s inbox on the Marv \"{session}\" ({}): messages it reads at its next turn", info.cwd),
            modes: vec![WireMode::Send],
            input_schema: template.input_schema,
        };
        let link = PeerLink { dir: self.dir.clone(), own: self.own.clone(), session, remote, socket: Mutex::default() };
        Some((receptacle, Arc::new(link)))
    }
}

/// Serves `peer.<session>.<remote>`: the far end's approval through its door, then messages through the plug it
/// gave this Marv (`peer-<own>`), so the far switchboard knows the sender by the socket.
pub struct PeerLink {
    pub dir: PathBuf,
    pub own: String,
    pub session: String,
    pub remote: ReceptacleName,
    /// The far end's plug socket for this Marv, once its user approved.
    socket: Mutex<Option<PathBuf>>,
}

#[async_trait]
impl Service for PeerLink {
    async fn deliver(&self, envelope: &Envelope) -> Result<Option<Value>> {
        let socket = self.socket.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let socket = socket.ok_or_else(|| MeshError::Io(format!("the Marv \"{}\" hasn't approved this wire", self.session)))?;
        let mut client = PlugClient::connect(&socket).await?;
        match envelope.mode {
            WireMode::Send => client.send(&self.remote, envelope.send_text()?).await.map(|()| None),
            WireMode::Call => client.call(&self.remote, envelope.body.clone()).await.map(|answer| Some(Value::String(answer))),
            WireMode::Watch => Err(MeshError::NotServed { receptacle: envelope.receptacle.to_string(), mode: envelope.mode }),
        }
    }

    /// Asks the far Marv's door; its user decides. Every plug of this Marv reaches it as `peer-<own>`: which of them
    /// may is this end's wire table's business.
    async fn approve_remote(&self, _from: &PlugId, mode: WireMode, local: Approver) -> Option<Result<Approver>> {
        let ask = async {
            let socket = session_socket(&self.dir, &self.session).ok_or_else(|| MeshError::BadName(self.session.clone()))?;
            let mut door = DoorClient::connect(&socket).await?;
            let answer = door.request(WIRE, json!({"from": self.own, "to": self.remote, "mode": mode, "by": local})).await?;
            if answer.get("granted").and_then(Value::as_bool) != Some(true) {
                return Err(MeshError::Io(format!("the Marv \"{}\" didn't wire it: its user said no", self.session)));
            }
            let by: Approver = serde_json::from_value(answer.get("by").cloned().unwrap_or_default())
                .map_err(|_| MeshError::Io("the other Marv's answer has no approver".into()))?;
            let socket = PathBuf::from(answer.get("socket").and_then(Value::as_str).unwrap_or_default());
            check_socket(&socket)?;
            *self.socket.lock().unwrap_or_else(|e| e.into_inner()) = Some(socket);
            Ok(by)
        };
        Some(ask.await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::switchboard::tests::{Log, Runtime};
    use crate::wire::Grant;

    fn id(s: &str) -> PlugId {
        PlugId::new(s).unwrap()
    }
    fn name(s: &str) -> ReceptacleName {
        ReceptacleName::new(s).unwrap()
    }

    #[test]
    fn sockets_live_under_the_runtime_dir_only() {
        let env = |v: &str| [("XDG_RUNTIME_DIR".to_string(), v.to_string())].into_iter().collect::<marv_mcp::Env>();
        assert_eq!(mesh_dir(&env("/run/user/1000")), Some(PathBuf::from("/run/user/1000/marv/mesh")));
        assert_eq!(mesh_dir(&env("relative")), None);
        assert_eq!(mesh_dir(&Default::default()), None);
        let dir = PathBuf::from("/run/user/1000/marv/mesh");
        assert_eq!(session_socket(&dir, "abc-1"), Some(dir.join("abc-1.sock")));
        assert_eq!(session_socket(&dir, "../x"), None);
    }

    #[test]
    fn mesh_names() {
        let env = |v: &str| [("MARV_MESH_NAME".to_string(), v.to_string())].into_iter().collect::<marv_mcp::Env>();
        assert_eq!(mesh_name(&env("api"), Path::new("/x")).unwrap(), "api");
        assert!(mesh_name(&env("Not/A Name"), Path::new("/x")).is_err());
        assert!(mesh_name(&env(&"a".repeat(44)), Path::new("/x")).is_err(), "leaves room for peer-");
        let named = mesh_name(&Default::default(), Path::new("/home/me/My Project!")).unwrap();
        assert!(named.starts_with("my-project-") && named.len() == "my-project-".len() + 4, "{named}");
        assert!(valid_segment(&named));
        assert!(mesh_name(&Default::default(), Path::new("/")).unwrap().starts_with("marv-"));
        let long = mesh_name(&Default::default(), Path::new("/a-very-long-folder-name-indeed-yes")).unwrap();
        assert!(long.len() <= 25, "{long}");
    }

    #[test]
    fn peer_names() {
        let inbox = name("agent.parent.inbox");
        let peer = peer_receptacle("api-3f9a", &inbox).unwrap();
        assert_eq!(peer.as_str(), "peer.api-3f9a.agent.parent.inbox");
        assert_eq!(parse_peer(&peer), Some(("api-3f9a".to_string(), inbox)));
        assert_eq!(parse_peer(&name("agent.parent.inbox")), None);
        assert_eq!(peer_plug("api-3f9a").unwrap().as_str(), "peer-api-3f9a");
        assert!(reserved("parent") && reserved("worker-3") && reserved("peer-x") && !reserved("claude"));
    }

    /// Says `answer` to every wire, keeping what it was asked.
    struct Person(Option<Approver>, Mutex<Vec<WireRequest>>);
    impl Person {
        fn new(answer: Option<Approver>) -> Arc<Self> {
            Arc::new(Self(answer, Mutex::default()))
        }
        fn asked(&self) -> Vec<String> {
            self.1.lock().unwrap().iter().map(WireRequest::title).collect()
        }
    }
    #[async_trait]
    impl PeerApprover for Person {
        async fn approve_wire(&self, request: WireRequest) -> Option<Approver> {
            self.1.lock().unwrap().push(request);
            self.0
        }
    }

    /// One Marv: a switchboard serving its plugs, with a parent, and its door in the shared mesh folder.
    struct Marv {
        sb: Arc<Switchboard>,
        log: Arc<Log>,
        _door: DoorGuard,
        _runtime: Runtime,
        cancel: CancellationToken,
    }
    impl Drop for Marv {
        fn drop(&mut self) {
            self.cancel.cancel();
        }
    }

    fn marv(session: &str, mesh: &Path, person: Arc<Person>) -> Marv {
        let log = Arc::new(Log::default());
        let runtime = Runtime::new();
        let sb = Arc::new(Switchboard::new(session, runtime.0.clone(), log.clone()));
        sb.plug(id("parent")).unwrap();
        let cancel = CancellationToken::new();
        tokio::spawn(Arc::clone(&sb).serve(cancel.clone()));
        let info = DoorInfo { cwd: PathBuf::from(format!("/work/{session}")), pane: Some(format!("w1:{session}")) };
        let door = open_door(sb.clone(), mesh, info, person).unwrap();
        Marv { sb, log, _door: door, _runtime: runtime, cancel }
    }

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    /// Whether nothing listens on `socket` within a second. A listener closed here can live on for a moment: a child
    /// that another test is spawning holds a copy of it until its exec.
    fn gone_soon(socket: &Path) -> bool {
        (0..100).any(|_| {
            let gone = nobody_listens(socket);
            if !gone {
                std::thread::sleep(Duration::from_millis(10));
            }
            gone
        })
    }

    #[tokio::test]
    async fn opening_a_door_sweeps_dead_marvs_sockets_only() {
        let mesh = Runtime::new();
        let _alpha = marv("alpha", &mesh.0, Person::new(None));
        drop(std::os::unix::net::UnixListener::bind(mesh.0.join("dead.sock")).unwrap());
        assert!(gone_soon(&mesh.0.join("dead.sock")));
        std::fs::write(mesh.0.join("notes.sock"), "not a socket").unwrap();
        let beta = marv("beta", &mesh.0, Person::new(None));
        assert!(!mesh.0.join("dead.sock").exists(), "nothing listens there: swept");
        assert!(mesh.0.join("alpha.sock").exists() && mesh.0.join("notes.sock").exists());
        assert_eq!(beta.sb.peer_dir(), Some(mesh.0.clone()));
        // A Marv that takes no connections (a listener nobody accepts on) is busy, not gone.
        let busy = std::os::unix::net::UnixListener::bind(mesh.0.join("busy.sock")).unwrap();
        assert!(!nobody_listens(&mesh.0.join("busy.sock")));
        drop(busy);
        assert!(gone_soon(&mesh.0.join("busy.sock")));
    }

    #[tokio::test]
    async fn ls_lists_live_private_doors_only() {
        let mesh = Runtime::new();
        let (a, b) = (marv("alpha", &mesh.0, Person::new(None)), marv("beta", &mesh.0, Person::new(None)));
        assert_eq!(mode(&mesh.0), 0o700);
        assert_eq!(mode(&mesh.0.join("alpha.sock")), 0o600);
        // A Marv that crashed left its socket: nobody answers there.
        drop(std::os::unix::net::UnixListener::bind(mesh.0.join("dead.sock")).unwrap());
        std::fs::set_permissions(mesh.0.join("dead.sock"), std::fs::Permissions::from_mode(0o600)).unwrap();
        // A socket anyone may use isn't connected to, live or not.
        let open = std::os::unix::net::UnixListener::bind(mesh.0.join("open.sock")).unwrap();
        std::fs::set_permissions(mesh.0.join("open.sock"), std::fs::Permissions::from_mode(0o666)).unwrap();
        std::fs::write(mesh.0.join("notes.txt"), "x").unwrap();
        let found = list(&mesh.0).await.unwrap();
        assert_eq!(found.iter().map(|p| p.session.as_str()).collect::<Vec<_>>(), ["alpha", "beta"]);
        assert_eq!(found[0].info.agents, ["parent"]);
        assert_eq!(found[0].info.cwd, "/work/alpha");
        assert_eq!(found[1].info.pane.as_deref(), Some("w1:beta"));
        assert_eq!(found[0].info.pid, std::process::id());
        // The name is taken while alpha lives.
        let again = Arc::new(Switchboard::new("alpha", Runtime::new().0.clone(), Arc::new(Log::default())));
        let info = DoorInfo { cwd: "/".into(), pane: None };
        assert!(open_door(again, &mesh.0, info.clone(), Person::new(None)).is_err());
        // Gone with its guard; a dead Marv's name can be taken again.
        drop(a);
        assert!(!mesh.0.join("alpha.sock").exists());
        assert_eq!(list(&mesh.0).await.unwrap().len(), 1);
        let dead = Arc::new(Switchboard::new("dead", Runtime::new().0.clone(), Arc::new(Log::default())));
        let _dead = open_door(dead, &mesh.0, info, Person::new(None)).unwrap();
        assert_eq!(list(&mesh.0).await.unwrap().len(), 2);
        drop((b, open));
        // No folder, nothing listed; a folder others can read isn't listed at all.
        assert!(list(&mesh.0.join("missing")).await.unwrap().is_empty());
        std::fs::set_permissions(&mesh.0, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(list(&mesh.0).await.is_err());
    }

    #[tokio::test]
    async fn a_cross_process_wire_needs_both_users_then_carries_messages() {
        let mesh = Runtime::new();
        let (pa, pb) = (Person::new(Some(Approver::Cli)), Person::new(Some(Approver::Tui)));
        let (a, b) = (marv("alpha", &mesh.0, pa.clone()), marv("beta", &mesh.0, pb.clone()));
        let to_b = name("peer.beta.agent.parent.inbox");
        // Found on demand: a peer's inbox, by its name.
        let r = a.sb.lookup(&to_b).await.unwrap();
        assert_eq!(r.kind, ReceptacleKind::Peer { session: "beta".into() });
        assert_eq!(r.modes, [WireMode::Send]);
        assert!(a.sb.lookup(&name("peer.beta.agent.nobody.inbox")).await.is_none());
        assert!(a.sb.lookup(&name("peer.alpha.agent.parent.inbox")).await.is_none(), "never its own peer");
        assert!(a.sb.lookup(&name("peer.gamma.agent.parent.inbox")).await.is_none());

        let ask = WireRequest { from: id("parent"), to: to_b.clone(), mode: WireMode::Send, asked_by: id("parent") };
        a.sb.grant(ask, Approver::Tui).await.unwrap();
        // Both ends hold a wire with both approvals; beta's user was asked about alpha's plug there.
        assert_eq!(pb.asked(), ["Wire peer-alpha -> agent.parent.inbox (send)"]);
        let wire = a.sb.table().find(&id("parent"), &to_b, WireMode::Send).cloned().unwrap();
        assert!(wire.cross_process);
        assert_eq!(wire.grant, Grant { local: Approver::Tui, remote: Some(Approver::Tui) });
        let far = b.sb.table().find(&id("peer-alpha"), &name("agent.parent.inbox"), WireMode::Send).cloned().unwrap();
        assert_eq!(far.grant, Grant { local: Approver::Tui, remote: Some(Approver::Tui) });

        a.sb.deliver(&id("parent"), to_b.as_str(), WireMode::Send, json!({"text": "hello beta"})).await.unwrap();
        let got = b.sb.drain_inbox(&id("parent"));
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].sender.as_str(), got[0].text().as_str()), ("peer-alpha", "hello beta"));
        assert!(got[0].fenced().contains("<mesh-message from=\"peer-alpha\" receptacle=\"agent.parent.inbox\">"));
        // Logged on both ends; only alpha's parent has the wire, not its other agents.
        assert_eq!(a.log.0.lock().unwrap().len(), 1);
        assert_eq!(b.log.0.lock().unwrap().len(), 1);
        a.sb.plug(id("worker-1")).unwrap();
        assert!(a.sb.deliver(&id("worker-1"), to_b.as_str(), WireMode::Send, json!({"text": "x"})).await.is_err());
        assert!(b.sb.drain_inbox(&id("parent")).is_empty());
        assert!(pa.asked().is_empty(), "alpha's door asked nothing: its own session asked for the wire");
    }

    #[tokio::test]
    async fn the_far_user_saying_no_leaves_it_unwired() {
        let mesh = Runtime::new();
        let (a, b) = (marv("alpha", &mesh.0, Person::new(None)), marv("beta", &mesh.0, Person::new(None)));
        let to_b = name("peer.beta.agent.parent.inbox");
        a.sb.lookup(&to_b).await.unwrap();
        let ask = WireRequest { from: id("parent"), to: to_b.clone(), mode: WireMode::Send, asked_by: id("parent") };
        let err = a.sb.grant(ask, Approver::Tui).await.unwrap_err();
        assert_eq!(err.to_string(), "the Marv \"beta\" didn't wire it: its user said no");
        assert!(a.sb.table().all().is_empty());
        assert!(b.sb.table().all().is_empty());
        assert!(!b.sb.has_plug(&id("peer-alpha")));
        // A one-ended wire built by hand doesn't count either: a peer receptacle is always cross-process.
        a.sb.wire(crate::Wire::new(id("parent"), to_b.clone(), WireMode::Send, Grant::local(Approver::Tui))).unwrap();
        let err = a.sb.deliver(&id("parent"), to_b.as_str(), WireMode::Send, json!({"text": "x"})).await.unwrap_err();
        assert!(matches!(err, MeshError::NotWired { .. }), "{err}");
    }

    #[tokio::test]
    async fn the_door_plugs_new_names_only_and_unplugs_them_on_leaving() {
        let mesh = Runtime::new();
        let a = marv("alpha", &mesh.0, Person::new(None));
        let mut door = DoorClient::connect(&mesh.0.join("alpha.sock")).await.unwrap();
        for taken in ["parent", "worker-1", "peer-beta", "Bad Name"] {
            assert!(door.request(PLUG, json!({"name": taken})).await.is_err(), "{taken}");
        }
        let answer = door.request(PLUG, json!({"name": "claude"})).await.unwrap();
        let socket = PathBuf::from(answer["socket"].as_str().unwrap());
        assert_eq!(mode(&socket), 0o600);
        assert!(door.request(PLUG, json!({"name": "claude"})).await.is_err(), "not twice");
        // Its socket is a plug like any other: nothing wired, nothing listed.
        let mut plug = PlugClient::connect(&socket).await.unwrap();
        assert!(plug.list().await.unwrap().is_empty());
        assert!(a.sb.has_plug(&id("claude")));
        drop(door);
        for _ in 0..100 {
            if !a.sb.has_plug(&id("claude")) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!a.sb.has_plug(&id("claude")));
    }

    #[tokio::test]
    async fn marv_mesh_wire_asks_each_end() {
        let mesh = Runtime::new();
        let (pa, pb) = (Person::new(Some(Approver::Tui)), Person::new(Some(Approver::Headless)));
        let (a, b) = (marv("alpha", &mesh.0, pa.clone()), marv("beta", &mesh.0, pb.clone()));
        a.sb.plug(id("worker-1")).unwrap();
        let mut door = DoorClient::connect(&mesh.0.join("alpha.sock")).await.unwrap();
        // Within one Marv: its user only.
        let local = door.request(CONNECT, json!({"from": "worker-1", "to": "agent.parent.inbox", "mode": "send"})).await.unwrap();
        assert_eq!(local["wired"], true);
        assert_eq!(local["cross_process"], false);
        // To another Marv: both users.
        let far =
            door.request(CONNECT, json!({"from": "parent", "to": "peer.beta.agent.parent.inbox", "mode": "send"})).await.unwrap();
        assert_eq!(far["wired"], true, "{far}");
        assert_eq!(far["cross_process"], true);
        assert_eq!(
            pa.asked(),
            ["Wire worker-1 -> agent.parent.inbox (send)", "Wire parent -> peer.beta.agent.parent.inbox (send)"]
        );
        assert_eq!(pb.asked(), ["Wire peer-alpha -> agent.parent.inbox (send)"]);
        assert!(b.sb.table().allows(&id("peer-alpha"), &name("agent.parent.inbox"), WireMode::Send));
        // Unknown agents and receptacles are errors, never asked about.
        assert!(door.request(CONNECT, json!({"from": "ghost", "to": "agent.parent.inbox", "mode": "send"})).await.is_err());
        assert!(door.request(CONNECT, json!({"from": "parent", "to": "peer.beta.agent.x.inbox", "mode": "send"})).await.is_err());
        assert!(door.request(CONNECT, json!({"from": "parent", "to": "agent.worker-1.inbox", "mode": "call"})).await.is_err());
        assert_eq!(pa.asked().len(), 2);
    }

    #[tokio::test]
    async fn the_far_end_checks_the_caller_by_pid() {
        let mesh = Runtime::new();
        let pb = Person::new(Some(Approver::Tui));
        let _b = marv("beta", &mesh.0, pb.clone());
        let mut door = DoorClient::connect(&mesh.0.join("beta.sock")).await.unwrap();
        let ask = |from: &str| json!({"from": from, "to": "agent.parent.inbox", "mode": "send", "by": "tui"});
        // Nobody listens as "ghost", and beta isn't its own peer.
        assert!(door.request(WIRE, ask("ghost")).await.is_err());
        assert!(door.request(WIRE, ask("beta")).await.is_err());
        // Someone else's process listens as "mallory": this caller isn't it.
        let Ok(mut other) = std::process::Command::new("python3")
            .arg("-c")
            .arg("import socket,sys,os,time\ns=socket.socket(socket.AF_UNIX)\ns.bind(sys.argv[1])\nos.chmod(sys.argv[1],0o600)\ns.listen(4)\nprint('up',flush=True)\nwhile True:\n  c,_=s.accept()")
            .arg(mesh.0.join("mallory.sock"))
            .stdout(std::process::Stdio::piped())
            .spawn()
        else {
            eprintln!("skipped the pid half: no python3");
            return;
        };
        let mut up = String::new();
        std::io::BufRead::read_line(&mut std::io::BufReader::new(other.stdout.take().unwrap()), &mut up).unwrap();
        let err = door.request(WIRE, ask("mallory")).await.unwrap_err();
        assert_eq!(err.to_string(), "can't confirm the caller is the Marv \"mallory\"");
        assert!(pb.asked().is_empty(), "never asked about a caller it couldn't confirm");
        let _ = other.kill();
        let _ = other.wait();
    }
}
