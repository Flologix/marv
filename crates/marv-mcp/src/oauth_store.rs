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
//!   update. The lock and the private-file writer live in `marv-fs`.

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

impl Login {
    /// A login with nothing in it yet, saved under the server's name.
    pub fn new(server: impl Into<String>) -> Self {
        Self { server: server.into(), client: None, tokens: None, saved_at: None, discovery: None }
    }
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
    /// Written back as the server sent it: a whole number stays one (`3600`, not `3600.0`).
    #[serde(default, skip_serializing_if = "Option::is_none", serialize_with = "whole_if_whole")]
    pub expires_in: Option<f64>,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

fn whole_if_whole<S: serde::Serializer>(v: &Option<f64>, s: S) -> Result<S::Ok, S::Error> {
    match v {
        Some(n) if n.fract() == 0.0 && n.abs() < 9.0e15 => s.serialize_i64(*n as i64),
        Some(n) => s.serialize_f64(*n),
        None => s.serialize_none(),
    }
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
    /// a file it can't read. `f` may refuse too (a login cancelled while it
    /// waited for the lock): then nothing is written.
    pub async fn update(
        &self,
        key: &str,
        f: impl FnOnce(Option<Login>) -> crate::Result<Option<Login>> + Send,
    ) -> crate::Result<()> {
        let work = || async {
            let mut file = match self.read() {
                Read::File(file) => file,
                Read::Unusable => return Err(UnusableTokenStore { path: self.path.display().to_string() }.into()),
            };
            match f(file.logins.remove(key))? {
                Some(entry) => file.logins.insert(key.to_string(), entry),
                None => None,
            };
            let text = serde_json::to_string_pretty(&file)?;
            marv_fs::write_private(&self.path, &text).await?;
            Ok(())
        };
        marv_fs::with_file_lock(&self.path, marv_fs::LockOptions::default(), work).await.map_err(lock_error)?
    }

    pub async fn remove(&self, key: &str) -> crate::Result<()> {
        self.update(key, |_| Ok(None)).await
    }
}

/// A lock that couldn't be had, in the lock's own words.
pub(crate) fn lock_error(e: marv_fs::LockError) -> crate::McpError {
    match e {
        marv_fs::LockError::Io(e) => e.into(),
        busy => crate::McpError::Server(busy.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::temp_dir;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;

    const YOURS: &str = "personal https://x.test/mcp";

    fn store() -> McpTokenStore {
        McpTokenStore::new(temp_dir("oauth").join("mcp-tokens.json"))
    }

    fn tokens() -> StoredTokens {
        serde_json::from_value(json!({"access_token": "a1", "refresh_token": "r1", "token_type": "Bearer", "expires_in": 3600, "issuer": "https://as.test/"})).unwrap()
    }

    fn with(server: &str, saved_at: u64) -> impl FnOnce(Option<Login>) -> crate::Result<Option<Login>> + Send {
        let server = server.to_string();
        move |e| {
            let mut e = e.unwrap_or(Login { server: String::new(), client: None, tokens: None, saved_at: None, discovery: None });
            e.server = server;
            e.tokens = Some(tokens());
            e.saved_at = Some(saved_at);
            Ok(Some(e))
        }
    }

    #[test]
    fn keys_keep_project_logins_apart_from_yours() {
        assert_eq!(login_key(ServerSource::Personal, "https://x/", "p-1"), "personal https://x/");
        assert_eq!(login_key(ServerSource::Project, "https://x/", "p-1"), "project:p-1 https://x/");
        // A project's server never sees your login for the same URL; one host, two resources: two logins.
        assert_ne!(login_key(ServerSource::Project, "https://x.test/mcp", "p"), YOURS);
        assert_ne!(
            login_key(ServerSource::Project, "https://x.test/mcp", "p"),
            login_key(ServerSource::Project, "https://x.test/mcp", "q")
        );
        assert_ne!(login_key(ServerSource::Personal, "https://x.test/other", "p"), YOURS);
    }

    #[test]
    fn unreadable_is_no_logins_and_extra_fields_survive() {
        let store = store();
        let path = store.path.clone();
        assert!(store.all().is_empty() && store.usable());
        for bad in ["{", "{\"v\": 2, \"logins\": {}}", "null", "{\"v\":1,\"logins\":[]}", "\"x\""] {
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
    }

    #[tokio::test]
    async fn round_trip_written_0600_with_the_servers_fields_kept() {
        let s = store();
        s.update(YOURS, with("x", 1)).await.unwrap();
        let got = s.get(YOURS).unwrap();
        assert_eq!(got.tokens, Some(tokens()));
        assert_eq!(got.server, "x");
        assert_eq!(std::fs::metadata(&s.path).unwrap().permissions().mode() & 0o777, 0o600);
        // Numbers come back as the server wrote them.
        assert!(std::fs::read_to_string(&s.path).unwrap().contains("\"expires_in\": 3600,"));
    }

    #[tokio::test]
    async fn an_update_never_writes_over_a_file_it_cant_use() {
        let s = store();
        for content in
            ["{not json", "null", r#"{"v":1,"logins":[]}"#, r#"{"v":2,"logins":{"personal https://z.test/":{}}}"#, "\"x\""]
        {
            std::fs::write(&s.path, content).unwrap();
            let err = s.update(YOURS, with("x", 1)).await.unwrap_err();
            assert!(err.to_string().contains("won't write over it"), "{err}");
            assert_eq!(std::fs::read_to_string(&s.path).unwrap(), content);
        }
        // An empty file holds nothing to lose: an update starts afresh.
        std::fs::write(&s.path, "").unwrap();
        s.update(YOURS, with("x", 1)).await.unwrap();
        assert_eq!(s.get(YOURS).unwrap().tokens.unwrap().access_token, "a1");
    }

    #[tokio::test]
    async fn remove_deletes_one_entry_and_keeps_the_others() {
        let s = store();
        s.update(YOURS, with("x", 1)).await.unwrap();
        s.update("personal https://y.test/mcp", with("y", 1)).await.unwrap();
        s.remove(YOURS).await.unwrap();
        assert!(s.get(YOURS).is_none());
        assert_eq!(s.get("personal https://y.test/mcp").unwrap().server, "y");
    }

    #[tokio::test]
    async fn a_refusing_update_writes_nothing() {
        let s = store();
        s.update(YOURS, with("x", 1)).await.unwrap();
        let before = std::fs::read_to_string(&s.path).unwrap();
        let err = s.update(YOURS, |_| Err(crate::McpError::Server("cancelled: /mcp logout".into()))).await.unwrap_err();
        assert_eq!(err.to_string(), "cancelled: /mcp logout");
        assert_eq!(std::fs::read_to_string(&s.path).unwrap(), before);
    }

    #[tokio::test]
    async fn concurrent_updates_and_two_stores_on_one_file_all_land() {
        let a = store();
        let b = McpTokenStore::new(a.path.clone());
        let (r1, r2, r3) = tokio::join!(
            a.update(YOURS, with("x", 1)),
            b.update("personal https://y.test/mcp", with("y", 2)),
            a.update("personal https://z.test/mcp", with("z", 3)),
        );
        for r in [r1, r2, r3] {
            r.unwrap();
        }
        let file: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&a.path).unwrap()).unwrap();
        let keys: Vec<_> = file["logins"].as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, vec![YOURS, "personal https://y.test/mcp", "personal https://z.test/mcp"]);
    }

    #[tokio::test]
    async fn a_stale_lock_is_taken_over_and_a_live_one_waited_for() {
        let s = store();
        let lock = std::path::PathBuf::from(format!("{}.lock", s.path.display()));
        std::fs::write(&lock, "12345").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        std::fs::File::options().write(true).open(&lock).unwrap().set_modified(old).unwrap();
        s.update(YOURS, with("x", 1)).await.unwrap();
        assert_eq!(s.get(YOURS).unwrap().server, "x");
        assert!(!lock.exists());

        std::fs::write(&lock, "12345").unwrap();
        let waiting = tokio::spawn({
            let s = s.clone();
            async move { s.update(YOURS, with("y", 2)).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!waiting.is_finished());
        std::fs::remove_file(&lock).unwrap();
        waiting.await.unwrap().unwrap();
        assert_eq!(s.get(YOURS).unwrap().server, "y");
    }
}
