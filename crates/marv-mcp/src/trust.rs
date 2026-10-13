//! Which project MCP servers the user has agreed to run. Ports `src/mcp/trust.ts`.
//!
//! A server is a program Marv starts with the user's permissions, outside the
//! sandbox, so one listed in a project's `.mcp.json` (a cloned repository, or a
//! file an agent edited) only starts once the user trusts it. Trust is for that
//! project and that exact config ([`crate::config::McpServerConfig::key`], which
//! also covers the project files its command line runs): change either, and it
//! needs trusting again.
//!
//! ```text
//! ~/.marv/mcp-trust.json  { "<project key>": ["<server config key>", …] }
//! ```
//!
//! - The file is schema-checked: valid JSON of the wrong shape (a hand edit:
//!   `null`, a list) trusts nothing rather than crashing.
//! - Callers pass the project key (`projectKey` in Marv's `src/paths.ts`, owned
//!   by `marv-session`), so this crate needs no path logic.
//! - A failing trust check means "not trusted", never an error at startup.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::config::{McpServerConfig, ServerSource};

/// Project key → the keys of the server configs trusted there.
pub type Trusted = BTreeMap<String, Vec<String>>;

#[derive(Debug, Clone)]
pub struct McpTrust {
    pub path: PathBuf,
}

impl McpTrust {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The file's content; missing, unreadable or the wrong shape is "nothing trusted".
    pub fn read(&self) -> Trusted {
        std::fs::read_to_string(&self.path).ok().and_then(|t| serde_json::from_str::<Trusted>(&t).ok()).unwrap_or_default()
    }

    /// Whether `server` may start in the project whose key is `project_key`.
    /// Yours always may: you wrote it into your own config.
    pub fn is_trusted(&self, project_key: &str, server: &McpServerConfig) -> bool {
        if server.source == ServerSource::Personal {
            return true;
        }
        self.read().get(project_key).is_some_and(|keys| keys.contains(&server.key))
    }

    /// Adds `servers`' keys to the project's list (no duplicates), written 0600
    /// through the private-file writer (Marv's `writePrivate`).
    pub fn trust(&self, _project_key: &str, _servers: &[McpServerConfig]) -> crate::Result<()> {
        todo!("port: McpTrust.trust (read, merge keys as a set, writePrivate 0600 in a 0700 folder)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::McpTransport;

    fn server(source: ServerSource, key: &str) -> McpServerConfig {
        McpServerConfig {
            name: "s".into(),
            source,
            transport: McpTransport::Stdio { command: "x".into(), args: vec![], env: Default::default() },
            timeout: None,
            key: key.into(),
            display: String::new(),
            reads: vec![],
            runs_project_files: vec![],
        }
    }

    #[test]
    fn a_hand_edited_file_trusts_nothing_and_yours_always_run() {
        let path = std::env::temp_dir().join(format!("marv-trust-{}.json", std::process::id()));
        let trust = McpTrust::new(&path);
        for text in ["null", "[1]", "{\"p\": \"k\"}", "broken"] {
            std::fs::write(&path, text).unwrap();
            assert!(!trust.is_trusted("p", &server(ServerSource::Project, "k")), "{text}");
        }
        std::fs::write(&path, r#"{"p": ["k"]}"#).unwrap();
        assert!(trust.is_trusted("p", &server(ServerSource::Project, "k")));
        assert!(!trust.is_trusted("other", &server(ServerSource::Project, "k")));
        assert!(!trust.is_trusted("p", &server(ServerSource::Project, "changed")));
        assert!(trust.is_trusted("x", &server(ServerSource::Personal, "any")));
        let _ = std::fs::remove_file(path);
    }
}
