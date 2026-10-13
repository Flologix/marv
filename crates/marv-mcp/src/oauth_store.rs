//! Where MCP logins are kept (Marv #18). Ports `src/mcp/oauth-store.ts`.
//!
//! ```text
//! ~/.marv/mcp-tokens.json   (0600, written through the private-file writer)
//! { "v": 1, "logins": { "personal https://mcp.example.com/": { "server": "x", "client": {…}, "tokens": {…}, … } } }
//! ```
//!
//! - **Keyed by owner and URL**, not by the server's name: renaming a server
//!   keeps its login, and two names for one URL share it. Your servers share
//!   one login per URL across projects; a project's server gets its own, per
//!   project. Trusting a project's `.mcp.json` means "run this server", not "act
//!   as my account": a repository that names your server's URL must not get
//!   your login by the trust you gave it for something else ([`login_key`]).
//! - **Read every time**: another Marv process may have refreshed a token.
//! - An unreadable file or a wrong shape reads as "no logins" (never a crash),
//!   while [`McpTokenStore::update`] refuses with [`UnusableTokenStore`] rather
//!   than write over it. An empty file starts afresh.
//! - **Shared and lock-protected**: every read-modify-write holds
//!   `mcp-tokens.json.lock` (Marv's `src/file-lock.ts`: queued in-process,
//!   O_EXCL across processes with a token unique to the hold, mtime refreshed
//!   while held, a lock untouched for 10 s taken over under a second
//!   `<lock>.takeover` guard), so two Marv processes can't drop each other's
//!   update. The lock lives with the private-file writer in `marv-session`
//!   until it gets a shared home.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::config::ServerSource;

/// One login. The OAuth objects are stored as the server handed them over
/// (extra fields kept), so they're loose maps with the fields Marv reads typed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Login {
    /// The server's name when it was saved: only for display.
    pub server: String,
    /// The client registered for Marv (`client_id`, plus the loopback `port` its redirect URI names).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<StoredClient>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<StoredTokens>,
    /// When `tokens` was saved (ms since the epoch), so `/mcp` can say when it expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_at: Option<u64>,
    /// Discovery state, so a startup with tokens makes no discovery requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery: Option<StoredDiscovery>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredClient {
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredTokens {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<f64>,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredDiscovery {
    pub authorization_server_url: String,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct StoreFile {
    v: StoreVersion,
    logins: BTreeMap<String, Login>,
}

/// Only version 1 is known; anything else is unusable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
struct StoreVersion;

impl TryFrom<u8> for StoreVersion {
    type Error = String;
    fn try_from(v: u8) -> Result<Self, String> {
        if v == 1 { Ok(StoreVersion) } else { Err(format!("unknown version {v}")) }
    }
}
impl From<StoreVersion> for u8 {
    fn from(_: StoreVersion) -> u8 {
        1
    }
}

/// The store key: owner, then the URL as expanded (one host can serve several
/// resources, each with its own tokens). `project_key` is Marv's `projectKey(root)`.
pub fn login_key(source: ServerSource, url: &str, project_key: &str) -> String {
    match source {
        ServerSource::Personal => format!("personal {url}"),
        ServerSource::Project => format!("project:{project_key} {url}"),
    }
}

/// Why `update` refused: the file exists but can't be read, so writing would destroy it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "{path} can't be read or is a format this Marv doesn't know (a newer version, or a hand edit): Marv won't write over it. Fix or remove the file, then log in again."
)]
pub struct UnusableTokenStore {
    pub path: String,
}

enum Read {
    File(StoreFile),
    Unusable,
}

#[derive(Debug, Clone)]
pub struct McpTokenStore {
    pub path: PathBuf,
}

impl McpTokenStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    fn read(&self) -> Read {
        match std::fs::read_to_string(&self.path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Read::File(StoreFile::default()),
            Err(_) => Read::Unusable,
            // Empty (nothing to lose): no logins, and an update may start afresh.
            Ok(text) if text.trim().is_empty() => Read::File(StoreFile::default()),
            Ok(text) => serde_json::from_str(&text).map_or(Read::Unusable, Read::File),
        }
    }

    fn logins(&self) -> BTreeMap<String, Login> {
        match self.read() {
            Read::File(f) => f.logins,
            Read::Unusable => BTreeMap::new(),
        }
    }

    pub fn get(&self, key: &str) -> Option<Login> {
        self.logins().remove(key)
    }

    /// Every login (for `/mcp`'s display of who is logged in where).
    pub fn all(&self) -> BTreeMap<String, Login> {
        self.logins()
    }

    /// Whether the file can be updated (it reads, or there's nothing to lose).
    pub fn usable(&self) -> bool {
        matches!(self.read(), Read::File(_))
    }

    /// Replaces one entry with what `f` makes of it (`None` removes it), under
    /// the file lock; refuses with [`UnusableTokenStore`] rather than write over
    /// a file it can't read.
    pub async fn update(&self, _key: &str, _f: impl FnOnce(Option<Login>) -> Option<Login> + Send) -> crate::Result<()> {
        todo!("port: McpTokenStore.update (withFileLock, read, refuse unusable, writePrivate)")
    }

    pub async fn remove(&self, key: &str) -> crate::Result<()> {
        self.update(key, |_| None).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_keep_project_logins_apart_from_yours() {
        assert_eq!(login_key(ServerSource::Personal, "https://x/", "p-1"), "personal https://x/");
        assert_eq!(login_key(ServerSource::Project, "https://x/", "p-1"), "project:p-1 https://x/");
    }

    #[test]
    fn unreadable_is_no_logins_and_extra_fields_survive() {
        let path = std::env::temp_dir().join(format!("marv-tokens-{}.json", std::process::id()));
        let store = McpTokenStore::new(&path);
        assert!(store.all().is_empty() && store.usable());
        for bad in ["{", "{\"v\": 2, \"logins\": {}}", "null"] {
            std::fs::write(&path, bad).unwrap();
            assert!(store.all().is_empty() && !store.usable(), "{bad}");
        }
        std::fs::write(&path, "  \n").unwrap();
        assert!(store.usable());
        let text = r#"{"v":1,"logins":{"personal u":{"server":"x","tokens":{"access_token":"a","issuer":"i"},"savedAt":5}}}"#;
        std::fs::write(&path, text).unwrap();
        let login = store.get("personal u").unwrap();
        assert_eq!(login.tokens.as_ref().unwrap().rest["issuer"], "i");
        assert_eq!(login.saved_at, Some(5));
        let _ = std::fs::remove_file(path);
    }
}
