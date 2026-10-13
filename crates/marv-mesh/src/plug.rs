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

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::receptacle::{valid_segment, ReceptacleName};
use crate::MeshError;

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

/// An MCP client over a plug's socket (`marv_mcp::jsonrpc`, newline-delimited).
pub struct PlugClient {
    pub socket: PathBuf,
}

impl PlugClient {
    pub async fn connect(_socket: &Path) -> crate::Result<Self> {
        todo!("port: mesh v1: connect a UnixStream, MCP initialize as client 'marv-plug'")
    }

    /// `tools/list`: the receptacles this plug is wired to.
    pub async fn list(&mut self) -> crate::Result<Vec<Reachable>> {
        todo!("port: mesh v1: tools/list → Reachable")
    }

    /// `tools/call` on a `call` wire: request and response.
    pub async fn call(&mut self, _receptacle: &ReceptacleName, _args: Value) -> crate::Result<String> {
        todo!("port: mesh v1: tools/call, result text (switchboard already fenced/limited it)")
    }

    /// Fire-and-forget into an inbox on a `send` wire.
    pub async fn send(&mut self, _receptacle: &ReceptacleName, _text: &str) -> crate::Result<()> {
        todo!("port: mesh v1: tools/call with mode send; switchboard acks without waiting for the reader")
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
