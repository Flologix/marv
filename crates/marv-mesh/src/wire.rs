//! Wires and the wiring table: who may reach which receptacle, how. New in
//! marv-rs (docs/MESH.md, "Words" and "Rules").
//!
//! A [`Wire`] is a grant from one plug to one receptacle, in one mode. The
//! [`WireTable`] decides every message with [`WireTable::allows`]:
//!
//! - **Default deny.** An empty table allows nothing; nothing is wired until the
//!   user wires it.
//! - **By name only.** A wire matches on the exact plug id, the exact
//!   receptacle name and the exact mode. No wildcards, no prefixes, no hosts or
//!   ports; a `call` wire grants no `send`, a `send` wire no `watch`.
//! - **No self-wiring.** A plug can't hold a wire to a receptacle it owns (its
//!   own inbox), and only the user grants: [`Grant`] has no agent approver, so
//!   there's no way for a plug to wire itself (rule 5, no ambient authority).
//! - **Cross-process wires (v2) need both ends**: a wire to another Marv's
//!   receptacle counts only once that Marv's user approved it too.
//! - Yolo never wires anything: a grant always names a user approval.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::MeshError;
use crate::limits::WireLimits;
use crate::plug::PlugId;
use crate::receptacle::ReceptacleName;

/// How a wire may be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WireMode {
    /// Request and response (`tools/call`).
    Call,
    /// Fire-and-forget into an inbox.
    Send,
    /// Subscribe to events (notifications), such as "agent B changed a file you read".
    Watch,
}

impl fmt::Display for WireMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            WireMode::Call => "call",
            WireMode::Send => "send",
            WireMode::Watch => "watch",
        })
    }
}

impl std::str::FromStr for WireMode {
    type Err = MeshError;
    fn from_str(s: &str) -> crate::Result<Self> {
        match s {
            "call" => Ok(WireMode::Call),
            "send" => Ok(WireMode::Send),
            "watch" => Ok(WireMode::Watch),
            other => Err(MeshError::BadName(format!("\"{other}\" isn't a wire mode: call, send or watch"))),
        }
    }
}

/// Who approved a wire. Every variant is the user; there is deliberately no
/// variant for an agent or for yolo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Approver {
    /// An approval in the TUI.
    Tui,
    /// `marv mesh wire a -> b:inbox`.
    Cli,
    /// Headless `--approve mesh:<receptacle>`.
    Headless,
}

/// The user's approval of a wire: this end's, and for a cross-process wire the other end's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub local: Approver,
    /// The other Marv's approval (v2). Required when [`Wire::cross_process`].
    pub remote: Option<Approver>,
}

impl Grant {
    pub fn local(by: Approver) -> Self {
        Self { local: by, remote: None }
    }
}

/// A grant from one plug to one receptacle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wire {
    pub from: PlugId,
    pub to: ReceptacleName,
    pub mode: WireMode,
    pub grant: Grant,
    pub limits: WireLimits,
    /// The receptacle is on another Marv's switchboard (v2) or the AgentMesh (v3).
    pub cross_process: bool,
}

impl Wire {
    pub fn new(from: PlugId, to: ReceptacleName, mode: WireMode, grant: Grant) -> Self {
        let cross_process = to.is_remote();
        Self { from, to, mode, grant, limits: WireLimits::default(), cross_process }
    }

    /// Whether both ends that must approve have.
    pub fn approved(&self) -> bool {
        !self.cross_process || self.grant.remote.is_some()
    }

    /// The wire is to a receptacle its own plug owns (an agent's own inbox).
    fn is_self(&self, owner: Option<&PlugId>) -> bool {
        owner == Some(&self.from) || self.to.inbox_owner() == Some(self.from.as_str())
    }
}

/// The switchboard's wiring table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireTable {
    wires: Vec<Wire>,
}

impl WireTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a wire the user approved. `owner` is the receptacle's owner, when it
    /// has one. A wire for the same plug, receptacle and mode is replaced (new limits).
    pub fn add(&mut self, wire: Wire, owner: Option<&PlugId>) -> crate::Result<()> {
        if wire.is_self(owner) {
            return Err(MeshError::SelfWire(format!("{} -> {}", wire.from, wire.to)));
        }
        self.remove(&wire.from, &wire.to, wire.mode);
        self.wires.push(wire);
        Ok(())
    }

    /// Unwires; true when there was such a wire.
    pub fn remove(&mut self, from: &PlugId, to: &ReceptacleName, mode: WireMode) -> bool {
        let before = self.wires.len();
        self.wires.retain(|w| !(&w.from == from && &w.to == to && w.mode == mode));
        self.wires.len() != before
    }

    /// Removes every wire from or to an agent (it ended).
    pub fn unplug(&mut self, agent: &PlugId) {
        self.wires.retain(|w| &w.from != agent && w.to.inbox_owner() != Some(agent.as_str()));
    }

    /// The wire that allows `from` to use `receptacle` in `mode`, if any.
    pub fn find(&self, from: &PlugId, receptacle: &ReceptacleName, mode: WireMode) -> Option<&Wire> {
        self.wires.iter().find(|w| &w.from == from && &w.to == receptacle && w.mode == mode && w.approved() && !w.is_self(None))
    }

    /// The one question the switchboard asks of every message. Default deny.
    pub fn allows(&self, from: &PlugId, receptacle: &ReceptacleName, mode: WireMode) -> bool {
        self.find(from, receptacle, mode).is_some()
    }

    /// [`Self::find`], as the error the model reads when there's no wire.
    pub fn check(&self, from: &PlugId, receptacle: &ReceptacleName, mode: WireMode) -> crate::Result<&Wire> {
        self.find(from, receptacle, mode).ok_or_else(|| MeshError::NotWired {
            from: from.to_string(),
            receptacle: receptacle.to_string(),
            mode,
        })
    }

    /// Every usable wire from a plug (`mesh_list`, `tools/list`).
    pub fn from_plug<'a>(&'a self, from: &'a PlugId) -> impl Iterator<Item = &'a Wire> + 'a {
        self.wires.iter().filter(move |w| &w.from == from && w.approved())
    }

    pub fn all(&self) -> &[Wire] {
        &self.wires
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> PlugId {
        PlugId::new(s).unwrap()
    }
    fn name(s: &str) -> ReceptacleName {
        ReceptacleName::new(s).unwrap()
    }
    fn wire(from: &str, to: &str, mode: WireMode) -> Wire {
        Wire::new(id(from), name(to), mode, Grant::local(Approver::Tui))
    }

    #[test]
    fn default_deny() {
        let table = WireTable::new();
        for mode in [WireMode::Call, WireMode::Send, WireMode::Watch] {
            assert!(!table.allows(&id("worker-1"), &name("agent.parent.inbox"), mode));
        }
        let err = table.check(&id("worker-1"), &name("agent.parent.inbox"), WireMode::Send).unwrap_err();
        assert_eq!(
            err.to_string(),
            "not wired: worker-1 has no send wire to agent.parent.inbox (the user wires it: /mesh wire, or marv mesh wire)"
        );
    }

    #[test]
    fn a_wire_allows_exactly_its_plug_receptacle_and_mode() {
        let mut table = WireTable::new();
        table.add(wire("worker-1", "agent.parent.inbox", WireMode::Send), Some(&id("parent"))).unwrap();
        assert!(table.allows(&id("worker-1"), &name("agent.parent.inbox"), WireMode::Send));
        // Another mode, another plug, a look-alike name: all denied.
        assert!(!table.allows(&id("worker-1"), &name("agent.parent.inbox"), WireMode::Call));
        assert!(!table.allows(&id("worker-1"), &name("agent.parent.inbox"), WireMode::Watch));
        assert!(!table.allows(&id("worker-2"), &name("agent.parent.inbox"), WireMode::Send));
        assert!(!table.allows(&id("worker-1"), &name("agent.parent.inbox2"), WireMode::Send));
        assert!(!table.allows(&id("worker-1"), &name("agent.parent"), WireMode::Send));
        // Wires are one-way.
        assert!(!table.allows(&id("parent"), &name("agent.worker-1.inbox"), WireMode::Send));
    }

    #[test]
    fn no_self_wiring() {
        let mut table = WireTable::new();
        let err = table.add(wire("worker-1", "agent.worker-1.inbox", WireMode::Send), None).unwrap_err();
        assert!(matches!(err, MeshError::SelfWire(_)));
        // Also refused when ownership comes from the receptacle, not its name.
        assert!(table.add(wire("dev", "process.dev", WireMode::Call), Some(&id("dev"))).is_err());
        assert!(!table.allows(&id("worker-1"), &name("agent.worker-1.inbox"), WireMode::Send));
        assert!(table.all().is_empty());
    }

    #[test]
    fn cross_process_wires_need_both_ends() {
        let mut table = WireTable::new();
        table.add(wire("parent", "mesh.remote.weather", WireMode::Call), None).unwrap();
        assert!(!table.allows(&id("parent"), &name("mesh.remote.weather"), WireMode::Call));
        let both = Wire::new(
            id("parent"),
            name("mesh.remote.weather"),
            WireMode::Call,
            Grant { local: Approver::Cli, remote: Some(Approver::Tui) },
        );
        table.add(both, None).unwrap();
        assert!(table.allows(&id("parent"), &name("mesh.remote.weather"), WireMode::Call));
        assert_eq!(table.all().len(), 1, "replaced, not duplicated");
    }

    #[test]
    fn unwiring_and_unplugging() {
        let mut table = WireTable::new();
        table.add(wire("worker-1", "agent.parent.inbox", WireMode::Send), None).unwrap();
        table.add(wire("parent", "agent.worker-1.inbox", WireMode::Send), None).unwrap();
        table.add(wire("worker-2", "agent.parent.inbox", WireMode::Send), None).unwrap();
        assert!(table.remove(&id("worker-2"), &name("agent.parent.inbox"), WireMode::Send));
        assert!(!table.remove(&id("worker-2"), &name("agent.parent.inbox"), WireMode::Send));
        table.unplug(&id("worker-1"));
        assert!(table.all().is_empty());
    }

    #[test]
    fn modes_parse() {
        assert_eq!("watch".parse::<WireMode>().unwrap(), WireMode::Watch);
        assert!("all".parse::<WireMode>().is_err());
    }
}
