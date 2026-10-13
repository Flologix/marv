//! The session's MCP connections. Ports `src/mcp/manager.ts`.
//!
//! Starts the servers (yours, and the project's once trusted), lists their
//! tools, and keeps them as [`McpTool`]s and specs.
//!
//! - **The tool list is fixed once the servers have settled** (prompt cache):
//!   `ready()` resolves when every server Marv was allowed to start has
//!   connected or failed; a turn waits for it before its first request. Only
//!   the user adds tools later (`/mcp trust`, `/mcp add`, `/mcp login`).
//!   `/mcp remove` never takes tools away mid-conversation: a removed server's
//!   tools stay listed and fail clearly until [`McpManager::forget_removed`].
//! - Connects in parallel, 60 s each (`npx -y` downloading the first time);
//!   `tools/list` paginates at most 50 pages and stops at a repeated cursor.
//! - Stdio servers get only a safe base environment plus their `env` (no API
//!   keys, [`server_env`]); their stderr is piped, never inherited (it would
//!   scribble over the TUI), and the last 20 lines show in `/mcp`.
//! - Transports: stdio and Streamable HTTP (`jsonrpc::Transport`).
//! - A remote server that wants an OAuth login is `needs-login` until the user
//!   runs `/mcp login`; startup and tool calls use stored tokens only
//!   (a passive provider, and none at all without stored tokens, so nobody is
//!   sent discovery or registration in the background).
//! - Local servers are closed on exit, and killed as a last resort.
//!
//! **Differences from Marv (security fixes, 2026-10-12 audit):**
//! - **`npx`/`uvx` never run in the project folder**, even for a trusted project
//!   server ([`cwd_for`]): `npx -y <pkg>` resolves a `<pkg>` planted in the
//!   project's `node_modules` (by a command that ran without asking, or the
//!   clone itself), and `uvx` reads the project's `pyproject.toml`/`uv.toml`.
//!   Marv only did this for your own servers.
//! - **A server's `instructions` are ignored**: `initialize`'s `instructions`
//!   never reach the system prompt or the conversation.
//! - Tool kinds follow the session's taint ([`McpTool::kind`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use marv_core::ToolSpec;
use tokio_util::sync::CancellationToken;

use crate::config::{McpServerConfig, McpTransport, ServerSource};
use crate::oauth::{CallbackCheck, LoginNeeded, Revoked};
use crate::oauth_store::{Login, McpTokenStore};
use crate::tool::McpTool;
use crate::trust::McpTrust;

/// Long enough for `npx -y some-server` to download it the first time.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(120);
pub const STDERR_LINES: usize = 20;
/// Pages of `tools/list` read at most (hundreds of tools would swamp the context anyway).
pub const MAX_PAGES: usize = 50;
/// A login gives up after this long (the user may have walked away from the browser).
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// The environment variables a stdio server gets besides its own `env`
/// (the MCP SDK's `getDefaultEnvironment` on Unix).
pub const SAFE_ENV: [&str; 6] = ["HOME", "LOGNAME", "PATH", "SHELL", "TERM", "USER"];

/// Package runners that resolve packages from the working directory.
pub const PACKAGE_RUNNERS: [&str; 5] = ["npx", "uvx", "bunx", "pnpx", "uv"];

/// - `Removed`: taken out with `/mcp remove`; its tools stay listed (the specs can't shrink) and their calls fail.
/// - `NeedsLogin`: a remote server that wants an OAuth login the user hasn't given (or that ran out).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpState {
    Untrusted,
    Connecting,
    Connected,
    Failed,
    Removed,
    NeedsLogin,
}

/// A remote server's login, for `/mcp`: never the token itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpLoginStatus {
    pub logged_in: bool,
    /// When the access token runs out (ms since the epoch).
    pub expires_at: Option<u64>,
    /// The authorization server's host ("log in at github.com").
    pub host: Option<String>,
}

/// What `/mcp` shows for one server.
#[derive(Debug, Clone, PartialEq)]
pub struct McpServerStatus {
    pub name: String,
    pub source: ServerSource,
    /// As written in the config (`${VAR}`s unexpanded, so no secrets).
    pub target: String,
    pub reads: Vec<String>,
    pub runs_project_files: Vec<String>,
    pub state: McpState,
    /// Model-facing tool names, once connected.
    pub tools: Vec<String>,
    pub error: Option<String>,
    /// The end of a local server's stderr.
    pub stderr: Option<String>,
    /// Its tools not offered because another server's tool already has that name.
    pub skipped: Vec<String>,
    /// Remote servers that can log in (no Authorization header of their own).
    pub login: Option<McpLoginStatus>,
}

/// A server whose config sends its own Authorization header: it logs in with that, never with OAuth.
pub fn own_authorization(t: &McpTransport) -> bool {
    matches!(t, McpTransport::Http { headers, .. } if headers.keys().any(|k| k.eq_ignore_ascii_case("authorization")))
}

/// What `/mcp` says about a stored login.
pub fn login_status(entry: Option<&Login>) -> McpLoginStatus {
    let Some(entry) = entry else { return McpLoginStatus::default() };
    let host = entry.discovery.as_ref().and_then(|d| url::Url::parse(&d.authorization_server_url).ok()).and_then(|u| {
        let host = u.host_str()?.to_string();
        Some(match u.port() {
            Some(p) => format!("{host}:{p}"),
            None => host,
        })
    });
    let expires_at = match (&entry.tokens, entry.saved_at) {
        (Some(t), Some(saved)) => t.expires_in.filter(|e| *e > 0.0).map(|e| saved + (e * 1000.0) as u64),
        _ => None,
    };
    McpLoginStatus { logged_in: entry.tokens.is_some(), expires_at, host }
}

/// Where a local server starts.
///
/// Yours start in your home folder, never the project: they skip the trust
/// check, and a package runner would pick up what's planted in the project. A
/// project's servers were trusted for that project and usually run its files,
/// so they start there, **except package runners** (`npx`, `uvx`, …), which
/// always start in your home folder (security fix, see module docs). Yours can
/// still get the project as `${MARV_PROJECT_DIR}`.
pub fn cwd_for(config: &McpServerConfig, root: &Path, home: &Path) -> PathBuf {
    let runner = match &config.transport {
        McpTransport::Stdio { command, .. } => {
            let base = Path::new(command).file_name().and_then(|n| n.to_str()).unwrap_or(command);
            PACKAGE_RUNNERS.contains(&base)
        }
        McpTransport::Http { .. } => false,
    };
    match config.source {
        ServerSource::Project if !runner => root.to_path_buf(),
        _ => home.to_path_buf(),
    }
}

/// A stdio server's environment: [`SAFE_ENV`] from Marv's own, plus its `env`.
pub fn server_env(outer: &crate::Env, own: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> =
        SAFE_ENV.iter().filter_map(|k| outer.get(*k).map(|v| (k.to_string(), v.clone()))).collect();
    env.extend(own.iter().map(|(k, v)| (k.clone(), v.clone())));
    env
}

/// What a call that lost its client tells the model: the server's state in Marv's words.
pub fn state_error(name: &str, state: McpState, error: Option<&str>) -> crate::McpError {
    match state {
        McpState::NeedsLogin => LoginNeeded { server: name.into() }.into(),
        McpState::Removed => {
            crate::McpError::Server("the server was removed with /mcp remove (its tools go away in the next conversation)".into())
        }
        McpState::Failed => crate::McpError::Server(format!("the server isn't available: {}", error.unwrap_or("it failed"))),
        _ => crate::McpError::Server("the server is reconnecting after a login: call it again in a moment.".into()),
    }
}

/// A login's failure, in Marv's words where it can tell what went wrong.
pub fn login_error(message: &str) -> String {
    if message.to_lowercase().contains("dynamic client registration") {
        return "this server needs a registered client: add oauth.clientId to its config (see the README's MCP section)".into();
    }
    format!("the login failed: {}", crate::text::truncate(message, 301))
}

pub struct ManagerOptions {
    pub root: PathBuf,
    /// Marv's `projectKey(root)` (trust and login keys).
    pub project_key: String,
    pub home: PathBuf,
    /// Marv's version, for `clientInfo`.
    pub version: String,
    pub trust: Option<McpTrust>,
    /// Where logins are kept. Without it, servers that want a login just need one.
    pub logins: Option<McpTokenStore>,
    /// Marv's own environment (only [`SAFE_ENV`] reaches stdio servers).
    pub env: crate::Env,
    pub connect_timeout: Duration,
    pub login_timeout: Duration,
}

/// What `/mcp login` hands the UI while it waits.
pub struct McpLogin {
    /// Where to send a pasted callback address.
    pub paste: Box<dyn FnMut(&str) -> CallbackCheck + Send>,
    /// The outcome.
    pub done: tokio::sync::oneshot::Receiver<crate::Result<McpServerStatus>>,
}

struct Connection {
    config: McpServerConfig,
    state: McpState,
    tools: Vec<McpTool>,
    skipped: Vec<String>,
    error: Option<String>,
    stderr: Vec<String>,
    login: Option<McpLoginStatus>,
}

/// The session's MCP servers. One per Marv process (the TUI hands it to its
/// session; the SDK's session owns its own).
pub struct McpManager {
    opts: ManagerOptions,
    connections: Vec<Connection>,
    /// Every tool of every connected server, in config order. Replaced, never changed.
    tools: Arc<Vec<McpTool>>,
    specs: Arc<Vec<ToolSpec>>,
    settled: bool,
    closed: bool,
    /// The login under way (one at a time).
    pending: Option<(String, CancellationToken)>,
}

impl McpManager {
    pub fn new(servers: Vec<McpServerConfig>, opts: ManagerOptions) -> Self {
        // "connecting" until start() has checked trust: calling them "untrusted" before that would let /mcp trust
        // start your own servers a second time.
        let connections = servers
            .into_iter()
            .map(|config| Connection {
                config,
                state: McpState::Connecting,
                tools: vec![],
                skipped: vec![],
                error: None,
                stderr: vec![],
                login: None,
            })
            .collect();
        Self { opts, connections, tools: Arc::default(), specs: Arc::default(), settled: true, closed: false, pending: None }
    }

    /// The current tools and specs (the arrays are replaced, never changed, so a turn holds a consistent set).
    pub fn tools(&self) -> Arc<Vec<McpTool>> {
        self.tools.clone()
    }
    pub fn specs(&self) -> Arc<Vec<ToolSpec>> {
        self.specs.clone()
    }
    /// Whether every server Marv may start has connected or failed.
    pub fn settled(&self) -> bool {
        self.settled
    }

    /// Starts every trusted server; never fails (a failing trust check means "not trusted").
    pub async fn start(&mut self) {
        let _ = (&self.opts, &mut self.connections, self.closed, &self.pending);
        todo!("port: McpManager.start (trust check per server, parallel connect, rebuild, settled)")
    }

    /// Resolves once the servers have settled (a turn waits on it: `status: waiting_for_mcp`).
    pub async fn ready(&self) {
        todo!("port: McpManager.ready (watch channel on settled)")
    }

    /// `/mcp add`: starts one more server now, in your home folder. Refuses a
    /// name in use, or one removed in this conversation (its tools stay listed).
    pub async fn add(&mut self, _config: McpServerConfig) -> crate::Result<McpServerStatus> {
        todo!("port: McpManager.add")
    }

    /// `/mcp remove`: stops it; its tools stay listed and fail clearly. Cancels its pending login.
    pub async fn remove(&mut self, _name: &str) -> crate::Result<()> {
        todo!("port: McpManager.remove")
    }

    /// A new conversation: tools of removed servers aren't offered any more.
    pub fn forget_removed(&mut self) {
        if !self.connections.iter().any(|c| c.state == McpState::Removed) {
            return;
        }
        self.connections.retain(|c| c.state != McpState::Removed);
        self.rebuild();
    }

    /// Project servers waiting for the user's trust.
    pub fn untrusted(&self) -> Vec<&McpServerConfig> {
        self.connections.iter().filter(|c| c.state == McpState::Untrusted).map(|c| &c.config).collect()
    }

    /// `/mcp trust`: trusts the waiting project servers (this exact config) and starts them.
    pub async fn trust_all(&mut self) -> crate::Result<Vec<McpServerStatus>> {
        todo!("port: McpManager.trustAll")
    }

    /// Remote servers that can log in (no Authorization header of their own), by name.
    pub fn loginable(&self) -> Vec<String> {
        self.connections.iter().filter(|c| Self::can_login(c)).map(|c| c.config.name.clone()).collect()
    }

    fn can_login(c: &Connection) -> bool {
        matches!(c.config.transport, McpTransport::Http { .. })
            && !own_authorization(&c.config.transport)
            && c.state != McpState::Removed
    }

    /// `/mcp login <name>`: refuses untrusted (before any network contact),
    /// stdio and own-header servers; one login at a time; [`LOGIN_TIMEOUT`].
    pub fn login(&mut self, _name: &str, _authorize: Box<dyn Fn(url::Url) + Send + Sync>) -> crate::Result<McpLogin> {
        todo!("port: McpManager.login (listener, interactive provider, expectCallback, finishAuth, reconnect)")
    }

    /// `/mcp logout <name>`: revokes (best effort), removes the stored login, `needs-login`.
    pub async fn logout(&mut self, _name: &str) -> crate::Result<Revoked> {
        todo!("port: McpManager.logout")
    }

    pub fn status(&self) -> Vec<McpServerStatus> {
        self.connections
            .iter()
            .map(|c| {
                let tools: Vec<String> = c.tools.iter().map(|t| t.name.clone()).filter(|n| !c.skipped.contains(n)).collect();
                McpServerStatus {
                    name: c.config.name.clone(),
                    source: c.config.source,
                    target: c.config.display.clone(),
                    reads: c.config.reads.clone(),
                    runs_project_files: c.config.runs_project_files.clone(),
                    state: c.state,
                    tools,
                    error: c.error.clone(),
                    stderr: (!c.stderr.is_empty()).then(|| c.stderr.join("\n")),
                    skipped: c.skipped.clone(),
                    login: Self::can_login(c).then(|| c.login.clone().unwrap_or_default()),
                }
            })
            .collect()
    }

    /// New arrays, so anything holding the old ones keeps a consistent set.
    /// Across servers the first one keeps a tool name; the others say so in `/mcp`.
    fn rebuild(&mut self) {
        let mut seen = std::collections::HashSet::new();
        let mut all = Vec::new();
        for c in &mut self.connections {
            c.skipped = c.tools.iter().filter(|t| seen.contains(&t.name)).map(|t| t.name.clone()).collect();
            for t in &c.tools {
                if seen.insert(t.name.clone()) {
                    all.push(t.clone());
                }
            }
        }
        self.specs = Arc::new(all.iter().map(|t| t.spec.clone()).collect());
        self.tools = Arc::new(all);
    }

    /// Stops every server (on exit); cancels a pending login.
    pub async fn close(&mut self) {
        todo!("port: McpManager.close")
    }

    /// Last resort on exit, synchronous: local servers must not outlive Marv.
    pub fn kill(&mut self) {
        todo!("port: McpManager.kill (SIGKILL each stdio child)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_server;
    use serde_json::json;

    fn server(source: ServerSource, raw: serde_json::Value) -> McpServerConfig {
        parse_server("s", &raw, source, &|_| None, "f").unwrap()
    }

    #[test]
    fn package_runners_never_start_in_the_project() {
        let (root, home) = (Path::new("/p"), Path::new("/h"));
        assert_eq!(cwd_for(&server(ServerSource::Project, json!({"command": "node", "args": ["s.js"]})), root, home), root);
        assert_eq!(cwd_for(&server(ServerSource::Project, json!({"command": "npx", "args": ["-y", "pkg"]})), root, home), home);
        assert_eq!(
            cwd_for(&server(ServerSource::Project, json!({"command": "/usr/bin/uvx", "args": ["pkg"]})), root, home),
            home
        );
        assert_eq!(cwd_for(&server(ServerSource::Personal, json!({"command": "node"})), root, home), home);
    }

    #[test]
    fn stdio_servers_get_no_api_keys() {
        let outer: crate::Env = [("PATH", "/bin"), ("OPENROUTER_API_KEY", "sk"), ("HOME", "/h")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let env = server_env(&outer, &[("TOKEN".to_string(), "t".to_string())].into());
        assert_eq!(env.keys().collect::<Vec<_>>(), vec!["HOME", "PATH", "TOKEN"]);
    }

    #[test]
    fn own_header_servers_dont_oauth() {
        let s = server(ServerSource::Personal, json!({"type": "http", "url": "https://x", "headers": {"authorization": "t"}}));
        assert!(own_authorization(&s.transport));
        let m = McpManager::new(
            vec![s, server(ServerSource::Personal, json!({"type": "http", "url": "https://y"}))],
            ManagerOptions {
                root: "/p".into(),
                project_key: "p".into(),
                home: "/h".into(),
                version: "0".into(),
                trust: None,
                logins: None,
                env: Default::default(),
                connect_timeout: CONNECT_TIMEOUT,
                login_timeout: LOGIN_TIMEOUT,
            },
        );
        assert_eq!(m.loginable().len(), 1);
        assert_eq!(m.status()[1].login, Some(McpLoginStatus::default()));
    }

    #[test]
    fn messages_in_marvs_words() {
        assert_eq!(
            state_error("x", McpState::NeedsLogin, None).to_string(),
            "x needs a new login: the user can run /mcp login x"
        );
        assert!(login_error("Incompatible auth server: does not support dynamic client registration").contains("oauth.clientId"));
    }
}
