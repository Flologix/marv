//! Plugs: an agent's handle onto the switchboard. New in marv-rs (docs/MESH.md, "Words").
//!
//! - Inside a sandbox, a plug is a Unix socket bind-mounted at
//!   [`SANDBOX_SOCKET`]. A bind-mounted socket is a file, not a network, so it
//!   works with `--unshare-net`; `marv-sandbox` keeps `/run` empty except for it.
//! - Each plug gets **its own** host socket (the switchboard knows who's
//!   calling from which socket it accepted on), so a sender can't claim to be
//!   another agent: identity comes from the socket, never from the message.
//! - The model never sees the socket: it reaches the mesh through
//!   `mesh_list`/`mesh_call`/`mesh_send` ([`crate::tools`]), which run in Marv
//!   (outside the sandbox) and speak MCP to the switchboard through
//!   [`PlugClient`]. Any MCP-speaking agent (Claude Code, opencode, jcode) can
//!   plug in from a Herdr or tmux pane through a small stdio-to-socket shim.
//! - Subagents don't inherit their parent's wires (docs/SECURITY.md).

use std::fmt;
use std::path::{Path, PathBuf};

use marv_mcp::jsonrpc::{
    Implementation, InitializeParams, Message, Notification, PROTOCOL_VERSION, Request, RequestId, decode_line, encode_line,
    method,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use crate::MeshError;
use crate::receptacle::{ReceptacleName, valid_segment};
use crate::wire::WireMode;

/// Where a sandboxed agent finds its plug.
pub const SANDBOX_SOCKET: &str = "/run/marv/mesh.sock";

/// An agent's name on the mesh (`parent`, `worker-1`): one receptacle-name segment.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PlugId(String);

impl PlugId {
    pub fn new(id: impl Into<String>) -> crate::Result<Self> {
        let id = id.into();
        if !valid_segment(&id) {
            return Err(MeshError::BadName(format!("\"{id}\" isn't an agent name: lowercase letters, digits, - and _")));
        }
        Ok(Self(id))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for PlugId {
    type Error = MeshError;
    fn try_from(s: String) -> crate::Result<Self> {
        Self::new(s)
    }
}
impl From<PlugId> for String {
    fn from(p: PlugId) -> String {
        p.0
    }
}
impl fmt::Display for PlugId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A plug handed out by the switchboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plug {
    pub id: PlugId,
    /// The host-side socket the switchboard listens on for this plug only
    /// (in the switchboard's private 0700 runtime folder).
    pub host_socket: PathBuf,
}

/// A bind mount for `marv-sandbox`: `--bind <host> <guest>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketMount {
    pub host: PathBuf,
    pub guest: PathBuf,
}

impl Plug {
    /// What the sandbox mounts so the agent inside finds its plug at [`SANDBOX_SOCKET`].
    pub fn mount(&self) -> SocketMount {
        SocketMount { host: self.host_socket.clone(), guest: PathBuf::from(SANDBOX_SOCKET) }
    }
}

/// One entry of `mesh_list`: a receptacle this plug is wired to, and how.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reachable {
    pub receptacle: ReceptacleName,
    pub modes: Vec<crate::wire::WireMode>,
    pub description: String,
}

/// `tools/call`'s `_meta` key for the wire mode (`"send"`; absent: `call`).
pub const MODE_META: &str = "marv/mode";
/// `tools/list`'s `_meta` key for the modes a plug is wired for, per receptacle.
pub const MODES_META: &str = "marv/modes";

/// An MCP client over a plug's socket (`marv_mcp::jsonrpc`, newline-delimited). One request at a time; the
/// switchboard answers in order.
pub struct PlugClient {
    pub socket: PathBuf,
    read: BufReader<OwnedReadHalf>,
    write: OwnedWriteHalf,
    next_id: i64,
}

impl PlugClient {
    /// Connects and introduces itself (`initialize` as `marv-plug`, then `initialized`). The server's
    /// `instructions`, if any, are never read.
    pub async fn connect(socket: &Path) -> crate::Result<Self> {
        let stream = UnixStream::connect(socket)
            .await
            .map_err(|e| MeshError::Io(format!("can't reach the switchboard at {}: {e}", socket.display())))?;
        let (read, write) = stream.into_split();
        let mut client = Self { socket: socket.to_path_buf(), read: BufReader::new(read), write, next_id: 0 };
        let params = serde_json::to_value(InitializeParams {
            protocol_version: PROTOCOL_VERSION.into(),
            capabilities: json!({}),
            client_info: Implementation { name: "marv-plug".into(), version: env!("CARGO_PKG_VERSION").into() },
        })
        .expect("initialize params serialize");
        client.request(method::INITIALIZE, Some(params)).await?;
        client.write_line(&Message::Notification(Notification::new(method::INITIALIZED, None))).await?;
        Ok(client)
    }

    async fn write_line(&mut self, message: &Message) -> crate::Result<()> {
        self.write.write_all(encode_line(message).as_bytes()).await.map_err(|e| MeshError::Io(format!("the switchboard: {e}")))
    }

    /// One request and its response's result.
    async fn request(&mut self, name: &str, params: Option<Value>) -> crate::Result<Value> {
        self.next_id += 1;
        let id = RequestId::Number(self.next_id);
        self.write_line(&Message::Request(Request::new(id.clone(), name, params))).await?;
        loop {
            let mut line = String::new();
            let n = self.read.read_line(&mut line).await.map_err(|e| MeshError::Io(format!("the switchboard: {e}")))?;
            if n == 0 {
                return Err(MeshError::Io("the switchboard closed the connection".into()));
            }
            match decode_line(&line) {
                Ok(Message::Response(r)) if r.id == id => {
                    return match (r.result, r.error) {
                        (_, Some(err)) => Err(MeshError::Io(format!("the switchboard: {}", err.message))),
                        (Some(result), None) => Ok(result),
                        (None, None) => Ok(Value::Null),
                    };
                }
                // Notifications (watch, later) and anything else: not this request's answer.
                _ => continue,
            }
        }
    }

    /// `tools/list`: the receptacles this plug is wired to.
    pub async fn list(&mut self) -> crate::Result<Vec<Reachable>> {
        let result = self.request(method::TOOLS_LIST, None).await?;
        let tools = result.get("tools").and_then(Value::as_array).cloned().unwrap_or_default();
        Ok(tools
            .iter()
            .filter_map(|t| {
                Some(Reachable {
                    receptacle: ReceptacleName::new(t.get("name")?.as_str()?).ok()?,
                    modes: serde_json::from_value(t.get("_meta")?.get(MODES_META)?.clone()).ok()?,
                    description: t.get("description").and_then(Value::as_str).unwrap_or_default().to_string(),
                })
            })
            .collect())
    }

    /// `tools/call` in `mode`: the result's text, or the switchboard's refusal as the error.
    async fn call_tool(&mut self, receptacle: &ReceptacleName, mode: WireMode, args: Value) -> crate::Result<String> {
        let params = json!({"name": receptacle, "arguments": args, "_meta": {MODE_META: mode}});
        let result = self.request(method::TOOLS_CALL, Some(params)).await?;
        let text = result
            .get("content")
            .and_then(Value::as_array)
            .map(|blocks| blocks.iter().filter_map(|b| b.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"))
            .unwrap_or_default();
        if result.get("isError").and_then(Value::as_bool).unwrap_or(false) {
            return Err(MeshError::Io(text));
        }
        Ok(text)
    }

    /// `tools/call` on a `call` wire: request and response.
    pub async fn call(&mut self, receptacle: &ReceptacleName, args: Value) -> crate::Result<String> {
        self.call_tool(receptacle, WireMode::Call, args).await
    }

    /// Fire-and-forget into an inbox on a `send` wire: the switchboard acks once it's queued, without waiting for
    /// the reader.
    pub async fn send(&mut self, receptacle: &ReceptacleName, text: &str) -> crate::Result<()> {
        self.call_tool(receptacle, WireMode::Send, json!({"text": text})).await.map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plug_ids_are_one_segment_and_mount_at_run_marv() {
        assert!(PlugId::new("worker-1").is_ok());
        assert!(PlugId::new("a.b").is_err());
        assert!(PlugId::new("").is_err());
        let plug = Plug { id: PlugId::new("w").unwrap(), host_socket: "/run/user/1000/marv/s/w.sock".into() };
        assert_eq!(plug.mount().guest, PathBuf::from("/run/marv/mesh.sock"));
    }
}
