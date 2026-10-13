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
//! - **Shared, not locked by its users** (marv-rs): every method takes `&self` and the manager lives in an `Arc`
//!   (`new` returns one; `login` takes `&Arc<Self>` because the login outlives the call). Its state sits behind a
//!   short synchronous lock never held across an await, and `settled` is a watch channel, so `/mcp` can read
//!   `status()` while a turn waits on `ready()`. [`McpManager::set_on_change`] is Marv's `onChange`: told when a
//!   server's state changes (the UI's notices, the SDK's "needs login" warnings).
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
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use marv_core::ToolSpec;
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::client::{Client, HttpOptions, Stderr, StdioOptions};
use crate::config::{McpServerConfig, McpTransport, ServerSource};
use crate::deferred::remember_schema_order;
use crate::oauth::{
    Abort, Authorize, CallbackCheck, CallbackExpectation, CallbackHandler, LoginNeeded, McpOAuthProvider, ProviderMode,
    ProviderOptions, Revoked, listen, new_state, revoke,
};
use crate::oauth_store::{Login, McpTokenStore, login_key};
use crate::ordered::Ordered;
use crate::tool::acceptable_schema_ordered;
use crate::tool::{McpCallResult, McpCaller, McpTool, McpToolInfo, make_mcp_tool};
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
    /// Stable for the connection's life (a removed one's neighbours shift in the list).
    id: u64,
    config: McpServerConfig,
    state: McpState,
    /// The live client. A login's reconnect swaps it only once the new one is connected.
    client: Option<Arc<Client>>,
    tools: Vec<McpTool>,
    skipped: Vec<String>,
    error: Option<String>,
    stderr: Stderr,
    login: Option<McpLoginStatus>,
}

/// Told when a server's state changes (Marv's `onChange`).
pub type OnChange = Arc<dyn Fn() + Send + Sync>;

/// The state and the hook, shared with the tools' callers (a tool call may mark its server `needs-login`).
struct Shared {
    state: Mutex<State>,
    on_change: Mutex<Option<OnChange>>,
}

impl Shared {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Tells the `on_change` hook. Never call it with the state lock held.
    fn changed(&self) {
        let f = self.on_change.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(f) = f {
            f();
        }
    }
}

/// The session's MCP servers. One per Marv process (the TUI hands it to its
/// session; the SDK's session owns its own). Shared as an `Arc` (module docs).
pub struct McpManager {
    opts: ManagerOptions,
    shared: Arc<Shared>,
    /// Whether every server Marv may start has connected or failed; `ready()` waits on it.
    settled: tokio::sync::watch::Sender<bool>,
    /// One start (startup, `/mcp add`, `/mcp trust`, a login's reconnect) at a time, in the order they began: an
    /// add waits for startup, as Marv chained `ready`.
    sequence: tokio::sync::Mutex<()>,
}

/// What changes: one short lock, never held across an await.
struct State {
    connections: Vec<Connection>,
    next_id: u64,
    /// Every tool of every connected server, in config order. Replaced, never changed.
    tools: Arc<Vec<McpTool>>,
    specs: Arc<Vec<ToolSpec>>,
    closed: bool,
    /// Starts under way: settled once none is.
    starting: usize,
    /// The login under way (one at a time): `/mcp remove`, `/mcp logout`, a new `/mcp login` and `close()` abort
    /// it, from the wait for the browser to the reconnect. The abort is checked after the code arrives, before
    /// tokens are saved and before the reconnect, so a cancelled login never logs a removed server back in or
    /// undoes a logout.
    pending: Option<(String, Abort)>,
}

impl State {
    fn find(&mut self, id: u64) -> Option<&mut Connection> {
        self.connections.iter_mut().find(|c| c.id == id)
    }
    fn named(&self, name: &str) -> Option<&Connection> {
        self.connections.iter().find(|c| c.config.name == name && c.state != McpState::Removed)
    }
}

/// Ends one start when dropped (finished, or its future dropped): the tools are rebuilt, and the servers have
/// settled once no start is left.
struct Starting<'a>(&'a McpManager);

impl Drop for Starting<'_> {
    fn drop(&mut self) {
        let settled = {
            let mut state = self.0.shared.state();
            state.rebuild();
            state.starting -= 1;
            state.starting == 0
        };
        if settled {
            self.0.settled.send_replace(true);
        }
        self.0.shared.changed();
    }
}

/// What a server that wants a (new) login answers: a 401 its login couldn't fix, or the passive provider refusing
/// to open anything.
fn wants_login(err: &crate::McpError) -> bool {
    matches!(err, crate::McpError::LoginNeeded(_) | crate::McpError::Unauthorized)
}

fn fail(message: impl Into<String>) -> crate::McpError {
    crate::McpError::Server(message.into())
}

/// Runs `work` for at most `limit`: "<what> timed out after Ns".
async fn within<T>(limit: Duration, what: &str, work: impl Future<Output = crate::Result<T>>) -> crate::Result<T> {
    tokio::time::timeout(limit, work)
        .await
        .unwrap_or_else(|_| Err(fail(format!("{what} timed out after {}s", (limit.as_millis() as f64 / 1000.0).round()))))
}

/// How a tool reaches its server: through the connection's *current* client (a login reconnects the server, and
/// tools a running turn already holds use the new one).
struct ServerCaller {
    shared: Arc<Shared>,
    id: u64,
    server: String,
    /// The client the tools were listed with: used when the connection has none any more.
    client: Arc<Client>,
    timeout: Duration,
    own_authorization: bool,
}

#[async_trait::async_trait]
impl McpCaller for ServerCaller {
    async fn call(&self, tool: &str, args: Map<String, Value>, cancel: CancellationToken) -> crate::Result<McpCallResult> {
        let used = {
            let mut state = self.shared.state();
            let Some(c) = state.find(self.id) else {
                return Err(state_error(&self.server, McpState::Removed, None));
            };
            match c.state {
                McpState::Removed | McpState::NeedsLogin => return Err(state_error(&self.server, c.state, None)),
                _ => c.client.clone().unwrap_or_else(|| self.client.clone()),
            }
        };
        let err = match used.call_tool(tool, args, self.timeout, cancel).await {
            Ok(result) => return Ok(result),
            Err(err) => err,
        };
        let mut state = self.shared.state();
        let Some(c) = state.find(self.id) else { return Err(state_error(&self.server, McpState::Removed, None)) };
        // The client was replaced or closed under the call (a login's reconnect): Marv's sentence for the state
        // it's in now, never "Connection closed".
        let current = c.client.as_ref().is_some_and(|now| Arc::ptr_eq(now, &used));
        if !wants_login(&err) && (!current || c.state != McpState::Connected) {
            return Err(state_error(&self.server, c.state, c.error.as_deref()));
        }
        // The token ran out and couldn't be refreshed: the passive provider refused to open a browser. The model
        // reads Marv's sentence, never the server's or the OAuth error's text.
        if !wants_login(&err) || self.own_authorization {
            return Err(err);
        }
        if c.state == McpState::Connected {
            c.state = McpState::NeedsLogin;
            c.login = Some(McpLoginStatus { logged_in: false, ..c.login.clone().unwrap_or_default() });
            drop(state);
            self.shared.changed();
        }
        Err(LoginNeeded { server: self.server.clone() }.into())
    }
}

impl McpManager {
    pub fn new(servers: Vec<McpServerConfig>, opts: ManagerOptions) -> Arc<Self> {
        // "connecting" until start() has checked trust: calling them "untrusted" before that would let /mcp trust
        // start your own servers a second time.
        let any = !servers.is_empty();
        let connections: Vec<Connection> = servers
            .into_iter()
            .enumerate()
            .map(|(i, config)| Connection {
                id: i as u64,
                config,
                state: McpState::Connecting,
                client: None,
                tools: vec![],
                skipped: vec![],
                error: None,
                stderr: Stderr::default(),
                login: None,
            })
            .collect();
        let next_id = connections.len() as u64;
        let state = State {
            connections,
            next_id,
            tools: Arc::default(),
            specs: Arc::default(),
            closed: false,
            starting: 0,
            pending: None,
        };
        Arc::new(Self {
            opts,
            shared: Arc::new(Shared { state: Mutex::new(state), on_change: Mutex::new(None) }),
            // marv-rs: servers that haven't started aren't settled. Marv's start() said so synchronously; here the
            // session spawns it, and a turn could otherwise skip `waiting_for_mcp` before start() first runs.
            settled: tokio::sync::watch::Sender::new(!any),
            sequence: tokio::sync::Mutex::new(()),
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.shared.state()
    }

    /// Marv's `onChange`: `f` is told whenever a server's state changes (replacing any earlier one). Called without
    /// the manager's lock held, so it may read `status()`.
    pub fn set_on_change(&self, f: impl Fn() + Send + Sync + 'static) {
        *self.shared.on_change.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(f));
    }

    fn changed(&self) {
        self.shared.changed();
    }

    /// The current tools and specs (the arrays are replaced, never changed, so a turn holds a consistent set).
    pub fn tools(&self) -> Arc<Vec<McpTool>> {
        self.state().tools.clone()
    }
    pub fn specs(&self) -> Arc<Vec<ToolSpec>> {
        self.state().specs.clone()
    }
    /// Whether every server Marv may start has connected or failed.
    pub fn settled(&self) -> bool {
        *self.settled.borrow()
    }

    /// One more start under way: not settled until it ends.
    fn starting(&self) -> Starting<'_> {
        self.state().starting += 1;
        self.settled.send_replace(false);
        Starting(self)
    }

    /// Starts every trusted server; never fails (a failing trust check means "not trusted"). Project servers the
    /// user hasn't trusted wait for `/mcp trust`. Marks the servers unsettled when called, before it's awaited.
    pub fn start(&self) -> impl Future<Output = ()> + Send + '_ {
        let starting = self.starting();
        async move {
            let _starting = starting;
            let _turn = self.sequence.lock().await;
            let ids: Vec<u64> = {
                let mut state = self.state();
                let mut ids = vec![];
                for c in &mut state.connections {
                    let trusted = self.opts.trust.as_ref().is_none_or(|t| t.is_trusted(&self.opts.project_key, &c.config));
                    if trusted {
                        ids.push(c.id);
                    } else if c.state == McpState::Connecting {
                        c.state = McpState::Untrusted;
                    }
                }
                ids
            };
            self.changed();
            futures::future::join_all(ids.into_iter().map(|id| self.connect(id))).await;
        }
    }

    /// Resolves once the servers have settled (a turn waits on it: `status: waiting_for_mcp`).
    pub async fn ready(&self) {
        let mut settled = self.settled.subscribe();
        // The sender lives as long as `self`, so this only ends once settled.
        let _ = settled.wait_for(|settled| *settled).await;
    }

    fn status_of(&self, name: &str) -> crate::Result<McpServerStatus> {
        self.status().into_iter().rev().find(|s| s.name == name).ok_or_else(|| fail(format!("{name} is gone")))
    }

    /// `/mcp add`: starts one more server now, as startup would (yours: in your home folder, the safe base
    /// environment). A turn that starts meanwhile waits for it. Refuses a name in use, or one removed in this
    /// conversation (its tools stay listed). Never fails for a server that fails to start: its status says why.
    pub fn add(&self, config: McpServerConfig) -> impl Future<Output = crate::Result<McpServerStatus>> + Send + '_ {
        let added = (|| {
            let mut state = self.state();
            if state.closed {
                return Err(fail("Marv is closing."));
            }
            let name = config.name.clone();
            match state.connections.iter().find(|c| c.config.name == name) {
                // A server removed in this conversation keeps its tools listed (the list can't shrink under it),
                // and the new one's tools would take the same names: it can come back in the next conversation.
                Some(c) if c.state == McpState::Removed => {
                    return Err(fail(format!(
                        "\"{name}\" was removed in this conversation and its tools stay listed until the next one: /clear first, or pick another --name."
                    )));
                }
                Some(_) => return Err(fail(format!("There's already an MCP server named \"{name}\"."))),
                None => {}
            }
            let id = state.next_id;
            state.next_id += 1;
            state.connections.push(Connection {
                id,
                config,
                state: McpState::Connecting,
                client: None,
                tools: vec![],
                skipped: vec![],
                error: None,
                stderr: Stderr::default(),
                login: None,
            });
            Ok((id, name))
        })();
        let starting = added.is_ok().then(|| self.starting());
        async move {
            let (id, name) = added?;
            {
                let _starting = starting;
                let _turn = self.sequence.lock().await;
                self.connect(id).await;
            }
            self.status_of(&name)
        }
    }

    /// `/mcp remove`: stops it. Its tools stay in the tool list until the next conversation (an eager
    /// conversation's history may call them, and the list can't shrink under it), and their calls fail clearly.
    /// Cancels its pending login.
    pub async fn remove(&self, name: &str) -> crate::Result<()> {
        // One still starting is removed once it has settled, so connect() can't mark it connected afterwards.
        if self.state().named(name).is_some_and(|c| c.state == McpState::Connecting) {
            self.ready().await;
        }
        let client = {
            let mut state = self.state();
            let Some(id) = state.named(name).map(|c| c.id) else {
                return Err(fail(format!("There's no MCP server named \"{name}\" running.")));
            };
            if let Some((server, abort)) = &state.pending
                && server == name
            {
                abort.abort("cancelled: /mcp remove");
            }
            let c = state.find(id).expect("found above");
            c.state = McpState::Removed;
            c.error = None;
            c.client.clone()
        };
        self.changed();
        if let Some(client) = client {
            client.close().await;
        }
        Ok(())
    }

    /// A new conversation: tools of removed servers aren't offered any more.
    pub fn forget_removed(&self) {
        {
            let mut state = self.state();
            if !state.connections.iter().any(|c| c.state == McpState::Removed) {
                return;
            }
            state.connections.retain(|c| c.state != McpState::Removed);
            state.rebuild();
        }
        self.changed();
    }

    /// Project servers waiting for the user's trust.
    pub fn untrusted(&self) -> Vec<McpServerConfig> {
        self.state().connections.iter().filter(|c| c.state == McpState::Untrusted).map(|c| c.config.clone()).collect()
    }

    /// `/mcp trust`: trusts the waiting project servers (remembered for this exact config) and starts them.
    pub async fn trust_all(&self) -> crate::Result<Vec<McpServerStatus>> {
        // Only once startup has said which servers are untrusted; and marked as starting before anything else is
        // awaited, so a second /mcp trust right after this one finds nothing left to start.
        self.ready().await;
        let waiting: Vec<(u64, McpServerConfig)> = {
            let mut state = self.state();
            state
                .connections
                .iter_mut()
                .filter(|c| c.state == McpState::Untrusted)
                .map(|c| {
                    c.state = McpState::Connecting;
                    (c.id, c.config.clone())
                })
                .collect()
        };
        if let Some(trust) = &self.opts.trust {
            let configs: Vec<McpServerConfig> = waiting.iter().map(|(_, c)| c.clone()).collect();
            if let Err(e) = trust.trust(&self.opts.project_key, &configs).await {
                let mut state = self.state();
                for (id, _) in &waiting {
                    if let Some(c) = state.find(*id).filter(|c| c.state == McpState::Connecting) {
                        c.state = McpState::Untrusted;
                    }
                }
                return Err(e);
            }
        }
        {
            let _starting = self.starting();
            let _turn = self.sequence.lock().await;
            futures::future::join_all(waiting.iter().map(|(id, _)| self.connect(*id))).await;
        }
        let names: Vec<&str> = waiting.iter().map(|(_, c)| c.name.as_str()).collect();
        Ok(self.status().into_iter().filter(|s| names.contains(&s.name.as_str())).collect())
    }

    /// Remote servers that can log in (no Authorization header of their own), by name.
    pub fn loginable(&self) -> Vec<String> {
        self.state().connections.iter().filter(|c| self.can_login(c)).map(|c| c.config.name.clone()).collect()
    }

    fn can_login(&self, c: &Connection) -> bool {
        self.opts.logins.is_some()
            && matches!(c.config.transport, McpTransport::Http { .. })
            && !own_authorization(&c.config.transport)
            && c.state != McpState::Removed
    }

    /// `/mcp login <name>`: logs in to a remote server (OAuth) and reconnects it. `authorize` gets the
    /// authorization URL to open or show; the browser comes back to a loopback listener, or the user pastes the
    /// address it ended up on (`paste`). Fails at once for an unknown server; everything else is `done`'s error:
    /// untrusted (before any network contact), stdio and own-header servers; one login at a time;
    /// [`LOGIN_TIMEOUT`]. Takes the `Arc`: the login runs on after this returns.
    pub fn login(self: &Arc<Self>, name: &str, authorize: Authorize) -> crate::Result<McpLogin> {
        let abort = Abort::new();
        let id = {
            let mut state = self.state();
            let Some(id) = state.named(name).map(|c| c.id) else {
                return Err(fail(format!("There's no MCP server named \"{name}\".")));
            };
            if let Some((_, previous)) = state.pending.replace((name.to_string(), abort.clone())) {
                previous.abort("cancelled: another login started");
            }
            id
        };
        let login_state = new_state();
        let expectation = Arc::new(Mutex::new(CallbackExpectation::new(login_state.clone())));
        let (tx, done) = tokio::sync::oneshot::channel();
        let me = self.clone();
        let (exp, aborted) = (expectation.clone(), abort.clone());
        tokio::spawn(async move {
            let result = me.run_login(id, authorize, login_state, exp.clone(), &aborted).await;
            if let Err(e) = &result {
                exp.lock().unwrap_or_else(|e| e.into_inner()).cancel(&e.to_string());
            }
            {
                let mut state = me.state();
                if state.pending.as_ref().is_some_and(|(_, a)| a.same(&aborted)) {
                    state.pending = None;
                }
            }
            let _ = tx.send(result);
        });
        Ok(McpLogin {
            paste: Box::new(move |text| match url::Url::parse(text.trim()) {
                Ok(url) => expectation.lock().unwrap_or_else(|e| e.into_inner()).check(&url),
                Err(_) => CallbackCheck::Refused { why: "path".into() },
            }),
            done,
        })
    }

    async fn run_login(
        self: &Arc<Self>,
        id: u64,
        authorize: Authorize,
        login_state: String,
        expectation: Arc<Mutex<CallbackExpectation>>,
        abort: &Abort,
    ) -> crate::Result<McpServerStatus> {
        // Startup must have said which servers are trusted: a project's server gets no network contact before that.
        self.ready().await;
        self.require_trusted(id)?;
        abort.check()?;
        let Some((config, state)) = self.state().find(id).map(|c| (c.config.clone(), c.state)) else {
            return Err(fail("the server is gone"));
        };
        let name = config.name.clone();
        if state == McpState::Removed {
            return Err(fail(format!("{name} was removed with /mcp remove.")));
        }
        let McpTransport::Http { url, headers, oauth } = &config.transport else {
            return Err(fail(format!("{name} is a local server: only remote (http) servers log in.")));
        };
        if own_authorization(&config.transport) {
            return Err(fail(format!(
                "{name} sends its own Authorization header (in its config): remove it to log in with OAuth instead."
            )));
        }
        let Some(store) = self.opts.logins.clone() else { return Err(fail("Logins can't be stored here.")) };
        let server_url = url::Url::parse(url).map_err(|_| fail(format!("{url} isn't a URL")))?;
        let key = login_key(config.source, url, &self.opts.project_key);
        let stored = store.get(&key);

        let limit = self.opts.login_timeout;
        let interactive = async {
            // A registered client's redirect URI names its port, and not every server accepts another loopback port.
            let check: CallbackHandler = {
                let exp = expectation.clone();
                Arc::new(move |u| exp.lock().unwrap_or_else(|e| e.into_inner()).check(u))
            };
            let preferred = oauth.as_ref().and_then(|o| o.callback_port).or(stored.and_then(|s| s.client?.port));
            let listener = listen(check, preferred).await?;
            if listener.fell_back
                && let Some(port) = oauth.as_ref().and_then(|o| o.callback_port)
            {
                return Err(fail(format!("port {port} (oauth.callbackPort) is taken: close what uses it and try again.")));
            }
            // Its client was registered with the old port: register again with the new redirect URI.
            if listener.fell_back {
                store
                    .update(&key, |e| {
                        Ok(e.map(|mut e| {
                            e.client = None;
                            e
                        }))
                    })
                    .await?;
            }
            let redirected = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let on_authorize: Authorize = {
                let redirected = redirected.clone();
                Arc::new(move |u| {
                    redirected.store(true, std::sync::atomic::Ordering::SeqCst);
                    authorize(u)
                })
            };
            let provider = McpOAuthProvider::new(ProviderOptions {
                store: store.clone(),
                key: key.clone(),
                server: name.clone(),
                mode: ProviderMode::Interactive,
                port: Some(listener.port),
                oauth: oauth.clone(),
                state: Some(login_state.clone()),
                on_authorize: Some(on_authorize),
                abort: Some(abort.clone()),
            });
            let client = Client::http(HttpOptions { url: server_url.clone(), headers: headers.clone(), auth: Some(provider) });
            let outcome = async {
                let connected = tokio::select! {
                    r = within(self.opts.connect_timeout, "connecting", client.initialize(&self.opts.version, self.opts.connect_timeout * 2)) => r.map(|_| ()),
                    _ = abort.aborted_now() => Err(fail(abort.reason())),
                };
                if let Err(err) = connected {
                    abort.check()?;
                    if !(matches!(err, crate::McpError::Unauthorized) && redirected.load(std::sync::atomic::Ordering::SeqCst)) {
                        return Err(fail(login_error(&err.to_string())));
                    }
                }
                // Connected without asking for a login: the reconnect below is all there is to do.
                if redirected.load(std::sync::atomic::Ordering::SeqCst) {
                    let mut outcome = expectation.lock().unwrap_or_else(|e| e.into_inner()).outcome();
                    let code = tokio::select! {
                        got = outcome.wait_for(Option::is_some) => match got.ok().and_then(|g| g.clone()) {
                            Some(Ok(code)) => code,
                            Some(Err(why)) => return Err(fail(why)),
                            None => return Err(fail("the login stopped")),
                        },
                        _ = abort.aborted_now() => return Err(fail(abort.reason())),
                    };
                    abort.check()?;
                    // The provider checks the abort again before it saves the tokens the exchange returns.
                    if let Err(err) = client.finish_auth(&code).await {
                        abort.check()?;
                        return Err(fail(login_error(&err.to_string())));
                    }
                }
                Ok(())
            }
            .await;
            listener.close();
            client.close().await;
            outcome
        };
        match tokio::time::timeout(limit, interactive).await {
            Ok(result) => result?,
            Err(_) => {
                let minutes = limit.as_secs() >= 60;
                let shown = if minutes {
                    format!("{} minutes", (limit.as_secs_f64() / 60.0).round())
                } else {
                    format!("{} s", limit.as_secs_f64().round())
                };
                abort.abort(format!("the login timed out after {shown}"));
                return Err(fail(abort.reason()));
            }
        }
        abort.check()?;
        self.reconnect(id).await
    }

    /// Refuses a project server the user hasn't trusted: it gets no network contact (login, logout's revocation)
    /// and its state isn't touched. Asks the trust store itself, not only the state another path may have set.
    fn require_trusted(&self, id: u64) -> crate::Result<()> {
        let Some((state, config)) = self.state().find(id).map(|c| (c.state, c.config.clone())) else {
            return Err(fail("the server is gone"));
        };
        let trusted = state != McpState::Untrusted
            && self.opts.trust.as_ref().is_none_or(|t| t.is_trusted(&self.opts.project_key, &config));
        if !trusted {
            return Err(fail(format!(
                "{} is this project's server and isn't trusted yet: trust it first: /mcp trust",
                config.name
            )));
        }
        Ok(())
    }

    /// Reconnects one server (after a login), as add() starts one: a turn starting meanwhile waits for it. The old
    /// client stays until the new one is connected (connect() swaps and closes it), so a call in between never
    /// lands on a closed client.
    async fn reconnect(&self, id: u64) -> crate::Result<McpServerStatus> {
        let Some((state, name)) = self.state().find(id).map(|c| (c.state, c.config.name.clone())) else {
            return Err(fail("the server is gone"));
        };
        if state == McpState::Removed {
            return Err(fail(format!("{name} was removed with /mcp remove.")));
        }
        {
            let _starting = self.starting();
            let _turn = self.sequence.lock().await;
            self.connect(id).await;
        }
        self.status_of(&name)
    }

    /// `/mcp logout <name>`: revokes the refresh token where the authorization server can (best effort), forgets
    /// the login and disconnects. Its tools stay listed and fail with the login sentence until the next login.
    pub async fn logout(&self, name: &str) -> crate::Result<Revoked> {
        let Some((id, config)) = self.state().named(name).map(|c| (c.id, c.config.clone())) else {
            return Err(fail(format!("There's no MCP server named \"{name}\".")));
        };
        let (McpTransport::Http { url, oauth, .. }, Some(store)) = (&config.transport, &self.opts.logins) else {
            return Err(fail(format!("{name} doesn't log in with /mcp login.")));
        };
        if own_authorization(&config.transport) {
            return Err(fail(format!("{name} doesn't log in with /mcp login.")));
        }
        self.ready().await;
        self.require_trusted(id)?;
        if let Some((server, abort)) = &self.state().pending
            && server == name
        {
            abort.abort("cancelled: /mcp logout");
        }
        let key = login_key(config.source, url, &self.opts.project_key);
        let Some(entry) = store.get(&key).filter(|e| e.tokens.is_some()) else {
            return Err(fail(format!("{name} isn't logged in.")));
        };
        let revoked = revoke(&entry, oauth.as_ref()).await;
        store.remove(&key).await?;
        let client = {
            let mut state = self.state();
            let c = state.find(id).ok_or_else(|| fail("the server is gone"))?;
            c.state = McpState::NeedsLogin;
            c.error = None;
            c.login = Some(McpLoginStatus::default());
            c.client.take()
        };
        self.changed();
        if let Some(client) = client {
            client.close().await;
        }
        Ok(revoked)
    }

    pub fn status(&self) -> Vec<McpServerStatus> {
        self.state()
            .connections
            .iter()
            .map(|c| {
                let tools: Vec<String> = c.tools.iter().map(|t| t.name.clone()).filter(|n| !c.skipped.contains(n)).collect();
                let stderr = c.stderr.lock().unwrap_or_else(|e| e.into_inner());
                McpServerStatus {
                    name: c.config.name.clone(),
                    source: c.config.source,
                    target: c.config.display.clone(),
                    reads: c.config.reads.clone(),
                    runs_project_files: c.config.runs_project_files.clone(),
                    state: c.state,
                    tools,
                    error: c.error.clone(),
                    stderr: (!stderr.is_empty()).then(|| stderr.iter().cloned().collect::<Vec<_>>().join("\n")),
                    skipped: c.skipped.clone(),
                    login: self.can_login(c).then(|| c.login.clone().unwrap_or_default()),
                }
            })
            .collect()
    }

    /// A remote server's stored login, as a passive provider: it refreshes tokens but never opens a browser or
    /// registers a client. None without stored tokens, so a server nobody logged in to is never sent discovery or
    /// registration requests in the background: its 401 just means "needs login".
    fn stored_login(&self, config: &McpServerConfig) -> (Option<McpLoginStatus>, Option<McpOAuthProvider>) {
        let McpTransport::Http { url, oauth, .. } = &config.transport else { return (None, None) };
        let Some(store) = self.opts.logins.as_ref().filter(|_| !own_authorization(&config.transport)) else {
            return (None, None);
        };
        let key = login_key(config.source, url, &self.opts.project_key);
        let entry = store.get(&key);
        let status = login_status(entry.as_ref());
        let provider = entry.filter(|e| e.tokens.is_some()).map(|e| {
            McpOAuthProvider::new(ProviderOptions::passive(
                store.clone(),
                key,
                config.name.clone(),
                e.client.and_then(|c| c.port),
                oauth.clone(),
            ))
        });
        (Some(status), provider)
    }

    async fn connect(&self, id: u64) {
        let (config, stderr) = {
            let mut state = self.state();
            let closed = state.closed;
            let Some(c) = state.find(id) else { return };
            // Removed stays removed: nothing (a login's reconnect, a late start) brings a server back mid-conversation.
            if c.state == McpState::Removed {
                return;
            }
            if closed {
                c.state = McpState::Failed;
                c.error = Some("closed".into());
                return;
            }
            c.state = McpState::Connecting;
            c.error = None;
            (c.config.clone(), c.stderr.clone())
        };
        self.changed();
        let (login, provider) = self.stored_login(&config);
        if let Some(login) = login
            && let Some(c) = self.state().find(id)
        {
            c.login = Some(login);
        }
        let client = match &config.transport {
            McpTransport::Stdio { command, args, env } => Client::spawn_stdio(StdioOptions {
                command: command.clone(),
                args: args.clone(),
                env: server_env(&self.opts.env, env),
                cwd: cwd_for(&config, &self.opts.root, &self.opts.home),
                stderr,
                stderr_lines: STDERR_LINES,
            }),
            McpTransport::Http { url, headers, .. } => url::Url::parse(url)
                .map(|url| Client::http(HttpOptions { url, headers: headers.clone(), auth: provider }))
                .map_err(|_| fail(format!("{url} isn't a URL"))),
        }
        .map(Arc::new);
        let listed = match &client {
            Ok(client) => self.handshake(client).await,
            Err(e) => Err(fail(e.to_string())),
        };
        let own = own_authorization(&config.transport);
        let mut to_close: Vec<Arc<Client>> = vec![];
        {
            let mut state = self.state();
            match (listed, client, state.find(id)) {
                // Forgotten meanwhile (a removed server, a new conversation): nothing keeps it running.
                (_, client, None) => to_close.extend(client.ok()),
                (listed, client, Some(c)) => match (listed, client) {
                    // Removed (/mcp remove) while this was connecting: it stays removed, and the server it started is stopped.
                    (_, Ok(client)) if c.state == McpState::Removed => to_close.push(client),
                    (_, Err(_)) if c.state == McpState::Removed => {}
                    (Ok(_), Err(_)) => unreachable!("listed only with a client"),
                    (Ok(infos), Ok(client)) => {
                        // A tool listed twice (a server repeating a page) is still one tool: its first place, its last listing.
                        let mut unique: Vec<(McpToolInfo, Option<Ordered>)> = vec![];
                        for info in infos {
                            match unique.iter_mut().find(|u| u.0.name == info.0.name) {
                                Some(slot) => *slot = info,
                                None => unique.push(info),
                            }
                        }
                        let timeout = Duration::from_secs_f64(config.timeout.unwrap_or(DEFAULT_CALL_TIMEOUT.as_secs_f64()));
                        let caller: Arc<dyn McpCaller> = Arc::new(ServerCaller {
                            shared: self.shared.clone(),
                            id,
                            server: config.name.clone(),
                            client: client.clone(),
                            timeout,
                            own_authorization: own,
                        });
                        c.tools = unique
                            .into_iter()
                            .map(|(info, schema)| {
                                let tool = make_mcp_tool(&config, info, caller.clone());
                                if let Some(schema) = schema {
                                    remember_schema_order(&tool.spec, &acceptable_schema_ordered(&schema));
                                }
                                tool
                            })
                            .collect();
                        to_close.extend(c.client.replace(client.clone()));
                        c.state = McpState::Connected;
                        // A server that exits later: its tools stay listed (the specs can't change), and calls fail
                        // with a clear error. Not when a login replaced this client with a new one.
                        let (shared, watched) = (self.shared.clone(), Arc::downgrade(&client));
                        let gone = client.closed();
                        tokio::spawn(async move {
                            gone.cancelled().await;
                            let Some(client) = watched.upgrade() else { return };
                            let stopped = {
                                let mut state = shared.state();
                                match state.find(id) {
                                    Some(c)
                                        if c.state == McpState::Connected
                                            && c.client.as_ref().is_some_and(|x| Arc::ptr_eq(x, &client)) =>
                                    {
                                        c.state = McpState::Failed;
                                        c.error = Some("the server stopped".into());
                                        true
                                    }
                                    _ => false,
                                }
                            };
                            if stopped {
                                shared.changed();
                            }
                        });
                    }
                    (Err(err), client) => {
                        if wants_login(&err) && own {
                            c.state = McpState::Failed;
                            c.error = Some("the server refused the token in headers".into());
                        } else if wants_login(&err) {
                            c.state = McpState::NeedsLogin;
                            c.login = Some(McpLoginStatus { logged_in: false, ..c.login.clone().unwrap_or_default() });
                        } else {
                            c.state = McpState::Failed;
                            c.error = Some(err.to_string());
                        }
                        to_close.extend(client.ok());
                        // A login's reconnect that failed: the previous client is no use either.
                        to_close.extend(c.client.take());
                    }
                },
            }
        }
        for client in to_close {
            client.close().await;
        }
        self.changed();
    }

    /// Initialize, then the tool list. A server can keep saying there's another page (the same cursor, or an
    /// endless list): stop at a cursor seen before or after [`MAX_PAGES`], or startup would never settle.
    async fn handshake(&self, client: &Client) -> crate::Result<Vec<(McpToolInfo, Option<Ordered>)>> {
        let limit = self.opts.connect_timeout;
        // The requests' own timeouts are longer: Marv's sentence ("connecting timed out after 60s") wins.
        let slack = limit * 2;
        within(limit, "connecting", client.initialize(&self.opts.version, slack)).await?;
        // close() may have been called while this was connecting: don't leave the server running.
        if self.state().closed {
            return Err(fail("closed"));
        }
        let mut infos = vec![];
        let mut seen = std::collections::HashSet::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let (page, schemas) = within(limit, "listing tools", client.list_tools(cursor.as_deref(), slack)).await?;
            infos.extend(page.tools.into_iter().zip(schemas));
            match page.next_cursor {
                Some(next) if seen.insert(next.clone()) => cursor = Some(next),
                _ => break,
            }
        }
        Ok(infos)
    }

    /// Stops every server (on exit); cancels a pending login.
    pub async fn close(&self) {
        let clients: Vec<Arc<Client>> = {
            let mut state = self.state();
            state.closed = true;
            if let Some((_, abort)) = &state.pending {
                abort.abort("cancelled: Marv is closing");
            }
            state.connections.iter().filter_map(|c| c.client.clone()).collect()
        };
        futures::future::join_all(clients.iter().map(|c| c.close())).await;
    }

    /// Last resort on exit, synchronous: local servers must not outlive Marv.
    pub fn kill(&self) {
        for c in &self.state().connections {
            if let Some(client) = &c.client {
                client.kill();
            }
        }
    }
}

impl State {
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
                logins: Some(McpTokenStore::new(crate::testutil::temp_dir("own").join("t.json"))),
                env: Default::default(),
                connect_timeout: CONNECT_TIMEOUT,
                login_timeout: LOGIN_TIMEOUT,
            },
        );
        assert_eq!(m.loginable().len(), 1);
        assert_eq!(m.status()[1].login, Some(McpLoginStatus::default()));
    }

    fn manager() -> Arc<McpManager> {
        McpManager::new(
            vec![server(ServerSource::Personal, json!({"type": "http", "url": "https://y"}))],
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
        )
    }

    #[tokio::test]
    async fn shared_status_reads_while_a_turn_waits_for_ready() {
        let m = manager();
        m.settled.send_replace(false);
        let waiting = tokio::spawn({
            let m = m.clone();
            async move { m.ready().await }
        });
        tokio::task::yield_now().await;
        // `/mcp` reads through the same Arc while the turn waits.
        assert_eq!(m.status().len(), 1);
        assert!(!m.settled() && !waiting.is_finished());
        m.settled.send_replace(true);
        waiting.await.unwrap();
        m.ready().await; // settled: returns at once
    }

    #[test]
    fn on_change_is_told_and_can_read_status() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let m = manager();
        m.changed(); // no hook yet: nothing happens
        let told = Arc::new(AtomicUsize::new(0));
        m.set_on_change({
            let (m, told) = (Arc::downgrade(&m), told.clone());
            move || {
                assert_eq!(m.upgrade().unwrap().status().len(), 1); // not called under the lock
                told.fetch_add(1, Ordering::SeqCst);
            }
        });
        m.changed();
        assert_eq!(told.load(Ordering::SeqCst), 1);
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
