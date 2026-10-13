//! marv-mesh: deliberate, wired, auditable lines between sandboxed agents.
//! New in marv-rs (no TS original except [`herdr`], which ports `src/herdr.ts`);
//! designed in `docs/MESH.md`, plan in `docs/subsystems/mesh.md`.
//!
//! The words, as a wall socket:
//! - a [`switchboard::Switchboard`] per Marv process, on the host, outside every
//!   sandbox: owns the wiring table, routes every message, the only thing that
//!   touches other processes or the network;
//! - a [`receptacle::Receptacle`] is a named service (`agent.<name>.inbox`,
//!   `herdr.pane.send`, `mesh.remote.<service>`, …); **names are the only
//!   addresses**, never hosts or ports;
//! - a [`plug::Plug`] is an agent's handle onto the switchboard: inside a sandbox,
//!   a Unix socket bind-mounted at `/run/marv/mesh.sock` (a file, so it works
//!   with `--unshare-net`);
//! - a [`wire::Wire`] is a grant from one plug to one receptacle, in one
//!   [`wire::WireMode`] (`call`, `send`, `watch`).
//!
//! The rules ([`wire::WireTable::allows`] is where they're decided):
//! 1. **Nothing is wired by default.** A wire needs the user (TUI approval,
//!    `marv mesh wire a -> b:inbox`, or headless `--approve mesh:<receptacle>`);
//!    yolo never wires anything.
//! 2. **Mesh text is data**: every delivered message is
//!    [`marv_core::Provenance::Mesh`], taints the session, and is fenced as a
//!    message from another agent ([`message::Envelope::fenced`]).
//! 3. **Every message is logged** to the trajectory with sender, receptacle and
//!    size ([`switchboard::MeshRecord`]).
//! 4. **Size and rate limits per wire** ([`limits`]).
//! 5. **No ambient authority**: a plug can do nothing beyond its wires, and an
//!    agent can't wire itself.
//!
//! Wire format: MCP (JSON-RPC 2.0) over the socket, using `marv_mcp::jsonrpc`.
//! Each receptacle a plug is wired to is an MCP tool on the switchboard;
//! `mesh_call` is `tools/call`; `watch` uses notifications. The model reaches it
//! only through [`tools`] (`mesh_list`, `mesh_call`, `mesh_send`).

pub mod agentmesh_bridge;
pub mod approval;
pub mod discovery;
pub mod herdr;
pub mod limits;
pub mod message;
pub mod plug;
pub mod receptacle;
pub mod switchboard;
pub mod tools;
pub mod wire;

pub use message::Envelope;
pub use plug::PlugId;
pub use receptacle::{Receptacle, ReceptacleName};
pub use switchboard::Switchboard;
pub use wire::{Grant, Wire, WireMode, WireTable};

/// Why the mesh refused or failed. Messages are Marv's own sentences: the model
/// reads them as tool errors, so they never quote the other side's text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MeshError {
    /// No wire allows this (default deny). Names only what was asked for.
    #[error("not wired: {from} has no {mode} wire to {receptacle} (the user wires it: /mesh wire, or marv mesh wire)")]
    NotWired { from: String, receptacle: String, mode: WireMode },
    #[error("{0}")]
    BadName(String),
    #[error("an agent can't wire itself: {0}")]
    SelfWire(String),
    #[error("{receptacle}: message of {size} bytes is over this wire's limit of {limit}")]
    TooLarge { receptacle: String, size: usize, limit: usize },
    #[error("{receptacle}: too many messages on this wire, try again in {retry_ms} ms")]
    RateLimited { receptacle: String, retry_ms: u64 },
    #[error("no receptacle named {0}")]
    NoReceptacle(String),
    #[error("{0}")]
    Io(String),
}

pub type Result<T, E = MeshError> = std::result::Result<T, E>;
