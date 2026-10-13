//! Asking the user for a wire. New in marv-rs (docs/MESH.md, rule 1).
//!
//! An agent can *ask* for a wire (a `mesh_call` on an unwired receptacle shows
//! up as a [`WireRequest`]), but only the user grants it: in the TUI, with
//! `marv mesh wire a -> b:inbox`, or headless with `--approve mesh:<receptacle>`.
//! Yolo never wires anything. The approval shows the full request: who, to
//! what, which mode, the limits.

use crate::plug::PlugId;
use crate::receptacle::ReceptacleName;
use crate::wire::{Approver, Grant, Wire, WireMode};
use crate::MeshError;

/// A wire someone wants: what the approval shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireRequest {
    pub from: PlugId,
    pub to: ReceptacleName,
    pub mode: WireMode,
    /// Who asked: the agent itself, or the user's command.
    pub asked_by: PlugId,
}

impl WireRequest {
    /// The approval's title.
    pub fn title(&self) -> String {
        format!("Wire {} -> {} ({})", self.from, self.to, self.mode)
    }

    /// The wire, once the user said yes.
    pub fn grant(self, by: Approver) -> Wire {
        Wire::new(self.from, self.to, self.mode, Grant::local(by))
    }
}

/// `--approve mesh:<receptacle>` (headless): the receptacle it pre-approves.
/// Other `--approve` kinds aren't the mesh's (`None`).
pub fn parse_approve_flag(value: &str) -> Option<crate::Result<ReceptacleName>> {
    value.strip_prefix("mesh:").map(ReceptacleName::new)
}

/// `marv mesh wire a -> b:inbox` / `a -> herdr.pane.send:call`: the arguments
/// as (from, receptacle, mode). `b:inbox` is short for `agent.b.inbox` with mode `send`.
pub fn parse_wire_spec(spec: &str) -> crate::Result<(PlugId, ReceptacleName, WireMode)> {
    let usage = || MeshError::BadName(format!("\"{spec}\": write a wire as <agent> -> <receptacle>[:<mode>], e.g. worker-1 -> parent:inbox"));
    let (from, to) = spec.split_once("->").ok_or_else(usage)?;
    let from = PlugId::new(from.trim())?;
    let to = to.trim();
    let (target, mode) = match to.rsplit_once(':') {
        Some((agent, "inbox")) => return Ok((from, ReceptacleName::inbox(&PlugId::new(agent)?), WireMode::Send)),
        Some((name, mode)) => (name, mode.parse()?),
        None => (to, WireMode::Call),
    };
    Ok((from, ReceptacleName::new(target)?, mode))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_specs() {
        let (f, r, m) = parse_wire_spec("worker-1 -> parent:inbox").unwrap();
        assert_eq!((f.as_str(), r.as_str(), m), ("worker-1", "agent.parent.inbox", WireMode::Send));
        let (_, r, m) = parse_wire_spec("parent -> herdr.pane.status:watch").unwrap();
        assert_eq!((r.as_str(), m), ("herdr.pane.status", WireMode::Watch));
        assert_eq!(parse_wire_spec("a -> mesh.remote.x").unwrap().2, WireMode::Call);
        assert!(parse_wire_spec("a b").is_err());
        assert!(parse_wire_spec("a -> b:everything").is_err());
    }

    #[test]
    fn approve_flags() {
        assert_eq!(parse_approve_flag("mesh:agent.p.inbox").unwrap().unwrap().as_str(), "agent.p.inbox");
        assert!(parse_approve_flag("network").is_none());
        assert!(parse_approve_flag("mesh:*").unwrap().is_err(), "no wildcards");
    }
}
