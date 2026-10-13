//! The session's side of the mesh (new in marv-rs; `docs/MESH.md`, `docs/subsystems/mesh.md` v1).
//!
//! - The session (one per process: the TUI's, or `marv -p`'s) owns the [`MeshHandle`]: one `Switchboard`, on the
//!   host, outside every sandbox. Its trajectory sink is [`crate::trajectory::MeshSink`] (rule 3: every message
//!   logged). Subagents get a plug with no wires.
//! - The model reaches the mesh only through [`MeshTool`] (`mesh_list` Read, `mesh_call`/`mesh_send` Egress;
//!   provenance Mesh, so results taint the session), over the agent's `PlugClient`.
//! - Wires are granted only by a person: [`WireApprover`] (the TUI's approval, `marv mesh wire`, or headless
//!   `--approve mesh:<receptacle>` pre-approvals via [`headless_grants`]). Yolo never wires; nothing here lets an
//!   agent wire itself.
//! - Inbox messages are drained before the next turn's request and appended fenced (`Envelope::fenced`), tagged
//!   `Provenance::Mesh` (taint).

use std::sync::Arc;

use async_trait::async_trait;
use marv_core::{Provenance, ToolKind};
use marv_mesh::approval::{WireRequest, parse_approve_flag};
use marv_mesh::switchboard::Switchboard;
use marv_mesh::tools::{MESH_CALL, MESH_LIST, MESH_SEND, kind_of, mesh_specs};
use marv_mesh::{PlugId, ReceptacleName};
use marv_tools::{Tool, ToolContext, ToolError, ToolOutput};
use serde_json::Value;

/// The process's switchboard. `route`/`serve` take `&mut self` (upstream gap: `serve` holding `&mut` for the life of
/// the socket loop blocks every other use; it should take `Arc<Self>` with interior mutability).
pub type MeshHandle = Arc<tokio::sync::Mutex<Switchboard>>;

/// Asks a person for a wire. `true` grants it. Never answered by yolo.
#[async_trait]
pub trait WireApprover: Send + Sync {
    async fn approve_wire(&self, request: WireRequest) -> bool;
}

/// `--approve mesh:<receptacle>` flags → the receptacles pre-approved for headless `send`s, and the other flags (for
/// `buildApprover`). A malformed receptacle name is an error naming the flag.
pub fn headless_grants(flags: &[String]) -> Result<(Vec<ReceptacleName>, Vec<String>), String> {
    let (mut receptacles, mut rest) = (Vec::new(), Vec::new());
    for flag in flags {
        match parse_approve_flag(flag) {
            Some(Ok(name)) => receptacles.push(name),
            Some(Err(err)) => return Err(format!("--approve {flag}: {err}")),
            None => rest.push(flag.clone()),
        }
    }
    Ok((receptacles, rest))
}

/// One of the three mesh tools for an agent's plug.
pub struct MeshTool {
    /// `mesh_list`, `mesh_call` or `mesh_send`.
    pub name: &'static str,
    pub plug: PlugId,
    pub mesh: MeshHandle,
}

impl MeshTool {
    /// The three, in `mesh_specs()` order.
    pub fn all(plug: &PlugId, mesh: &MeshHandle) -> Vec<Self> {
        [MESH_LIST, MESH_CALL, MESH_SEND].into_iter().map(|name| Self { name, plug: plug.clone(), mesh: mesh.clone() }).collect()
    }
}

#[async_trait]
impl Tool for MeshTool {
    type Input = serde_json::Map<String, Value>;
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        match self.name {
            MESH_LIST => marv_mesh::tools::LIST_DESCRIPTION,
            MESH_CALL => marv_mesh::tools::CALL_DESCRIPTION,
            _ => marv_mesh::tools::SEND_DESCRIPTION,
        }
    }
    fn schema(&self) -> Value {
        mesh_specs().into_iter().find(|s| s.name == self.name).map(|s| s.parameters).unwrap_or(Value::Null)
    }
    fn kind(&self) -> ToolKind {
        kind_of(self.name).unwrap_or(ToolKind::Egress)
    }
    fn provenance(&self) -> Provenance {
        Provenance::Mesh
    }
    fn label(&self, input: &Self::Input) -> String {
        input.get("receptacle").and_then(Value::as_str).unwrap_or(self.name).to_string()
    }
    fn uses_network(&self, _input: &Self::Input) -> bool {
        self.name != MESH_LIST
    }
    async fn run(&self, _input: &Self::Input, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        todo!("port: mesh v1: PlugClient list/call/send for this plug; results come fenced (Envelope::fenced)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mesh_flags_are_split_from_the_rest() {
        let (grants, rest) = headless_grants(&["git_write".into(), "mesh:agent.parent.inbox".into()]).unwrap();
        assert_eq!(grants.len(), 1);
        assert_eq!(rest, vec!["git_write"]);
        assert!(headless_grants(&["mesh:Not A Name".into()]).is_err());
    }
}
