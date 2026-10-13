//! Across machines: the AgentMesh bridge (mesh v3). New in marv-rs (docs/MESH.md, topology 3).
//!
//! The switchboard connects to a local `agentmesh-node`, which exposes the mesh
//! as an MCP server (Streamable HTTP on `127.0.0.1:8080`), using
//! `marv_mcp`'s client. It:
//! - republishes chosen remote services as `mesh.remote.<service>` receptacles;
//! - publishes chosen local receptacles to the mesh.
//!
//! AgentMesh adds identity (OIDC), relays and mesh-wide policy; **Marv's wire
//! approvals still apply on top**: a remote receptacle is reachable only over a
//! wire the user approved (both ends, [`crate::wire::Wire::cross_process`]).
//! Remote descriptions and results are stranger-written: `Provenance::Mesh`,
//! fenced like any mesh message. The bridge only talks to a loopback endpoint;
//! the node is what reaches the network.

use url::Url;

use crate::receptacle::{Receptacle, ReceptacleName};

/// Where `agentmesh-node` serves MCP by default.
pub const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:8080/mcp";

/// Config: which services cross, each way. Nothing crosses by default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeConfig {
    pub endpoint: Url,
    /// Remote service names to republish as `mesh.remote.<service>`.
    pub import: Vec<String>,
    /// Local receptacles to publish to the mesh.
    pub export: Vec<ReceptacleName>,
}

/// Only a loopback endpoint: the local node is the one that reaches the network.
pub fn endpoint_ok(url: &Url) -> bool {
    let loopback = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(d)) => d == "localhost",
        None => false,
    };
    loopback && matches!(url.scheme(), "http" | "https")
}

/// A remote tool name → its receptacle (`weather.forecast` → `mesh.remote.weather.forecast`),
/// refused when it isn't a valid name (no case folding, no guessing).
pub fn remote_receptacle(service: &str) -> crate::Result<ReceptacleName> {
    ReceptacleName::remote(service)
}

pub struct AgentMeshBridge {
    pub config: BridgeConfig,
}

impl AgentMeshBridge {
    pub fn new(config: BridgeConfig) -> crate::Result<Self> {
        if !endpoint_ok(&config.endpoint) {
            return Err(crate::MeshError::BadName(format!(
                "{}: the AgentMesh bridge only connects to a local agentmesh-node (127.0.0.1)",
                config.endpoint
            )));
        }
        Ok(Self { config })
    }

    /// Connects to the node and lists the imported services as receptacles.
    pub async fn connect(&mut self) -> crate::Result<Vec<Receptacle>> {
        todo!("port: mesh v3: marv_mcp client over Streamable HTTP to the node; tools/list filtered by config.import")
    }

    /// Calls a remote service over an approved wire.
    pub async fn call(&self, _receptacle: &ReceptacleName, _args: serde_json::Value) -> crate::Result<serde_json::Value> {
        todo!("port: mesh v3: tools/call on the node")
    }

    /// Publishes the exported receptacles to the mesh.
    pub async fn publish(&self) -> crate::Result<()> {
        todo!("port: mesh v3: register config.export with the node")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bridge_only_talks_to_a_local_node() {
        assert!(endpoint_ok(&Url::parse(DEFAULT_ENDPOINT).unwrap()));
        assert!(!endpoint_ok(&Url::parse("https://mesh.example.com/mcp").unwrap()));
        let cfg = |u: &str| BridgeConfig { endpoint: Url::parse(u).unwrap(), import: vec![], export: vec![] };
        assert!(AgentMeshBridge::new(cfg("http://10.0.0.2:8080/mcp")).is_err());
        assert_eq!(remote_receptacle("weather.forecast").unwrap().as_str(), "mesh.remote.weather.forecast");
        assert!(remote_receptacle("Weather").is_err());
    }
}
