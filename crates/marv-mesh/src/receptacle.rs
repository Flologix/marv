//! Receptacles: named services on the switchboard. New in marv-rs (docs/MESH.md, "Words").
//!
//! A name is the only address a grant is evaluated on, never a host or a port
//! (borrowed from Google's AgentMesh). Names are dot-separated segments of
//! lowercase letters, digits, `-` and `_`, so they compare exactly: no case
//! folding, no wildcards, no prefixes. `agent.worker-1.inbox` grants nothing
//! on `agent.worker-1.inbox2`.
//!
//! | Receptacle | What it is | Milestone |
//! |---|---|---|
//! | `agent.<name>.inbox` | an agent's message inbox | v1 |
//! | `herdr.pane.send` / `herdr.pane.status` | a Herdr pane | v2 |
//! | `github.read` | the Marv #47 door | later |
//! | `process.<name>` | a background dev server (Marv #51) | later |
//! | `mesh.remote.<service>` | a service on an AgentMesh network | v3 |

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::MeshError;
use crate::plug::PlugId;
use crate::wire::WireMode;

/// Longest receptacle name.
pub const MAX_NAME: usize = 128;
const MAX_SEGMENT: usize = 48;

/// A checked receptacle name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ReceptacleName(String);

/// One segment: `[a-z0-9_-]`, starting with a letter or digit.
pub(crate) fn valid_segment(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= MAX_SEGMENT
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-' || *c == b'_')
}

impl ReceptacleName {
    pub fn new(name: impl Into<String>) -> crate::Result<Self> {
        let name = name.into();
        if name.len() > MAX_NAME || name.split('.').count() < 2 || !name.split('.').all(valid_segment) {
            return Err(MeshError::BadName(format!(
                "\"{name}\" isn't a receptacle name: dot-separated lowercase words like agent.worker-1.inbox"
            )));
        }
        Ok(Self(name))
    }

    /// `agent.<name>.inbox`.
    pub fn inbox(agent: &PlugId) -> Self {
        Self(format!("agent.{}.inbox", agent.as_str()))
    }

    /// `mesh.remote.<service>` (v3, the AgentMesh bridge).
    pub fn remote(service: &str) -> crate::Result<Self> {
        Self::new(format!("mesh.remote.{service}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The agent whose inbox this is (`agent.<name>.inbox`), if it is one.
    pub fn inbox_owner(&self) -> Option<&str> {
        self.0.strip_prefix("agent.")?.strip_suffix(".inbox").filter(|n| !n.contains('.'))
    }

    pub fn is_remote(&self) -> bool {
        self.0.starts_with("mesh.remote.")
    }
}

impl TryFrom<String> for ReceptacleName {
    type Error = MeshError;
    fn try_from(s: String) -> crate::Result<Self> {
        Self::new(s)
    }
}
impl From<ReceptacleName> for String {
    fn from(n: ReceptacleName) -> String {
        n.0
    }
}
impl fmt::Display for ReceptacleName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What serves a receptacle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReceptacleKind {
    /// An agent's inbox: messages queue until its next turn reads them.
    Inbox,
    /// A Herdr pane adapter (v2, [`crate::herdr::HerdrAdapter`]).
    HerdrPane,
    /// Another Marv's switchboard on this machine (v2), by session id.
    Peer { session: String },
    /// A service on the AgentMesh network (v3, [`crate::agentmesh_bridge`]).
    Remote { service: String },
    /// A host-side door Marv itself implements (`github.read`, `process.<name>`).
    Builtin,
}

/// A receptacle offered on a switchboard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receptacle {
    pub name: ReceptacleName,
    /// The plug that owns it (an inbox's agent); `None` for host-side services.
    /// A plug can't hold a wire to a receptacle it owns (no self-wiring).
    pub owner: Option<PlugId>,
    pub kind: ReceptacleKind,
    /// One line for `mesh_list` and the approval. Written by Marv, or fenced
    /// as data when it came from elsewhere (a remote service's description).
    pub description: String,
    /// The modes it supports (an inbox takes `send` and `watch`, not `call`).
    pub modes: Vec<WireMode>,
    /// MCP input schema for `call` (`tools/list` on the switchboard).
    pub input_schema: serde_json::Value,
}

impl Receptacle {
    /// An agent's inbox.
    pub fn inbox(agent: PlugId) -> Self {
        Self {
            name: ReceptacleName::inbox(&agent),
            description: format!("{}'s inbox: messages it reads at its next turn", agent.as_str()),
            owner: Some(agent),
            kind: ReceptacleKind::Inbox,
            modes: vec![WireMode::Send, WireMode::Watch],
            input_schema: serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}),
        }
    }

    pub fn supports(&self, mode: WireMode) -> bool {
        self.modes.contains(&mode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_checked_and_exact() {
        assert!(ReceptacleName::new("agent.worker-1.inbox").is_ok());
        assert!(ReceptacleName::new("herdr.pane.send").is_ok());
        for bad in ["inbox", "Agent.x.inbox", "agent..inbox", "agent.x.inbox.", "agent.*.inbox", "a.b c", "host:8080.x", ".x"] {
            assert!(ReceptacleName::new(bad).is_err(), "{bad}");
        }
        let id = PlugId::new("worker-1").unwrap();
        let inbox = ReceptacleName::inbox(&id);
        assert_eq!(inbox.as_str(), "agent.worker-1.inbox");
        assert_eq!(inbox.inbox_owner(), Some("worker-1"));
        assert_eq!(ReceptacleName::new("herdr.pane.send").unwrap().inbox_owner(), None);
        assert!(ReceptacleName::remote("weather").unwrap().is_remote());
    }

    #[test]
    fn names_deserialize_only_when_valid() {
        assert!(serde_json::from_str::<ReceptacleName>("\"agent.a.inbox\"").is_ok());
        assert!(serde_json::from_str::<ReceptacleName>("\"AGENT\"").is_err());
    }
}
