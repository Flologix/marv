//! Logging in to a remote MCP server that wants OAuth (Marv #18). Ports `src/mcp/oauth.ts`.
//!
//! Marv's TS used the MCP SDK for the protocol (RFC 9728 discovery, dynamic
//! registration, PKCE S256, code exchange, refresh on a 401); marv-rs runs it
//! itself (or through `rmcp`'s auth module once adopted), keeping the same
//! provider contract and UX:
//!
//! - **Only the user starts a login** (`/mcp login`). Startup, tool calls and
//!   the SDK use a [`ProviderMode::Passive`] provider: it refreshes and saves
//!   tokens but never opens a browser or registers a client (both raise
//!   [`LoginNeeded`]). [`ProviderMode::Interactive`] exists only inside
//!   `McpManager::login`.
//! - Tokens live in the shared, lock-protected store
//!   ([`crate::oauth_store::McpTokenStore`]); `tokens()` re-reads it every time
//!   (another Marv process may have rotated them). The PKCE verifier stays in
//!   memory.
//! - The redirect URI is always `http://127.0.0.1:<port>/callback` (RFC 8252
//!   §8.3: `localhost` can resolve elsewhere; an undefined redirect URI made the
//!   SDK try a client-credentials grant).
//! - A callback counts only with the login's `state` (32 random bytes, compared
//!   in constant time, used once): a sandboxed command shares the loopback
//!   network and could otherwise inject a code. `error=` is reported only as a
//!   short code; the browser page echoes nothing.
//! - `invalidate_credentials(Tokens)` drops only the refresh token that actually
//!   failed: the SSE GET and a POST can both 401 and refresh at once, and the
//!   stale one's `invalid_grant` must not delete what the other just saved.
//! - A cancelled login (logout, remove, a new login, close) saves nothing, even
//!   when its token exchange finishes late.

use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::McpError;
use crate::config::McpOAuthConfig;
use crate::oauth_store::{Login, McpTokenStore, StoredClient, StoredDiscovery, StoredTokens};

pub const REPOSITORY: &str = "https://github.com/FlologixAI/marv";
pub const LOOPBACK: [&str; 3] = ["127.0.0.1", "localhost", "[::1]"];
/// `probe_login` and `revoke` give up after this long.
pub const NETWORK_TIMEOUT: Duration = Duration::from_secs(5);
/// The MCP revision sent with discovery requests (`MCP-Protocol-Version`).
const PROTOCOL_HEADER: &str = "mcp-protocol-version";

/// A server needs the user to log in (again). Its message is Marv's own
/// sentence, never a server's or an OAuth error body: it's what the model
/// reads when a tool call fails.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{server} needs a new login: the user can run /mcp login {server}")]
pub struct LoginNeeded {
    pub server: String,
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).expect("the OS random number generator");
    bytes
}

fn base64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// A login's `state`: 32 random bytes, base64url, so a callback without it
/// (another page, a sandboxed command) is refused.
pub fn new_state() -> String {
    base64url(&random_bytes::<32>())
}

/// A PKCE pair (RFC 7636, S256): the verifier (kept in memory) and its challenge (sent).
pub fn pkce() -> (String, String) {
    let verifier = base64url(&random_bytes::<32>());
    let challenge = base64url(&Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// The redirect URI a login on `port` names: 127.0.0.1, never `localhost`.
pub fn redirect_url_for(port: Option<u16>) -> String {
    match port {
        Some(p) => format!("http://127.0.0.1:{p}/callback"),
        None => "http://127.0.0.1/callback".into(),
    }
}

fn is_loopback_host(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(d)) => d == "localhost",
        Some(url::Host::Ipv4(ip)) => ip == std::net::Ipv4Addr::LOCALHOST,
        Some(url::Host::Ipv6(ip)) => ip == std::net::Ipv6Addr::LOCALHOST,
        None => false,
    }
}

/// An authorization URL Marv will open or show: https, or http on a loopback host (tests' fake server).
pub fn authorization_url_ok(url: &Url) -> bool {
    url.scheme() == "https" || (url.scheme() == "http" && is_loopback_host(url))
}

/// The client OAuth requests go through. It follows no redirects: a token or
/// registration endpoint that redirects is refused rather than followed with
/// the code, the refresh token or a secret.
pub(crate) fn http() -> &'static reqwest::Client {
    static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .expect("an HTTP client")
    });
    &CLIENT
}

/// A login's cancellation: `/mcp remove`, `/mcp logout`, another login,
/// closing, the timeout. The first reason given is the one reported.
#[derive(Debug, Clone, Default)]
pub struct Abort {
    token: CancellationToken,
    reason: Arc<Mutex<Option<String>>>,
}

impl Abort {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn abort(&self, reason: impl Into<String>) {
        let mut slot = self.reason.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            *slot = Some(reason.into());
        }
        drop(slot);
        self.token.cancel();
    }

    pub fn aborted(&self) -> bool {
        self.token.is_cancelled()
    }

    pub fn reason(&self) -> String {
        self.reason.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_else(|| "cancelled".into())
    }

    /// `Err(reason)` once aborted (Marv's `signal.throwIfAborted()`).
    pub fn check(&self) -> crate::Result<()> {
        if self.aborted() { Err(McpError::Server(self.reason())) } else { Ok(()) }
    }

    /// Whether `other` is this same login's abort.
    pub fn same(&self, other: &Abort) -> bool {
        Arc::ptr_eq(&self.reason, &other.reason)
    }

    pub async fn aborted_now(&self) {
        self.token.cancelled().await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderMode {
    /// Startup, tool calls, the SDK: refresh only; never a browser, never a registration.
    Passive,
    /// Only while `/mcp login` runs.
    Interactive,
}

/// What [`McpOAuthProvider::invalidate_credentials`] drops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Invalidate {
    All,
    Client,
    Tokens,
    Verifier,
    Discovery,
}

/// Hands the authorization URL to the login UI, to open or show.
pub type Authorize = Arc<dyn Fn(Url) + Send + Sync>;

#[derive(Clone)]
pub struct ProviderOptions {
    pub store: McpTokenStore,
    /// The store key ([`crate::oauth_store::login_key`]: owner + URL).
    pub key: String,
    /// The server's name, for messages and the store's display field.
    pub server: String,
    pub mode: ProviderMode,
    /// The loopback port this login listens on (interactive), or the stored client's.
    pub port: Option<u16>,
    /// The config's `oauth` block: a client registered by hand, its port, a scope.
    pub oauth: Option<McpOAuthConfig>,
    /// The login's `state`; random when not given.
    pub state: Option<String>,
    /// Interactive: hands the authorization URL to the UI to open or show.
    pub on_authorize: Option<Authorize>,
    /// Interactive: the login's cancellation. Once aborted, nothing is saved: a
    /// token exchange that finishes late must not undo a logout or log a removed
    /// server back in.
    pub abort: Option<Abort>,
}

impl ProviderOptions {
    /// A passive provider: what startup and tool calls use.
    pub fn passive(store: McpTokenStore, key: String, server: String, port: Option<u16>, oauth: Option<McpOAuthConfig>) -> Self {
        Self { store, key, server, mode: ProviderMode::Passive, port, oauth, state: None, on_authorize: None, abort: None }
    }
}

/// Marv's side of an OAuth client: backed by the token store.
pub struct McpOAuthProvider {
    pub opts: ProviderOptions,
    /// PKCE verifier: memory only (a login lives in one process, and the verifier is what makes a stolen code useless).
    verifier: Option<String>,
    /// The refresh token `tokens()` last handed out: what `invalidate_credentials` drops when it can't tell which failed.
    last_refresh: Option<String>,
    /// The refresh token the last refused refresh request sent: the one to drop.
    failed_refresh: Option<String>,
    /// Interactive: the tokens this login got. A login starts afresh rather than reusing (or refreshing) stored ones.
    fresh: Option<StoredTokens>,
    state: String,
}

impl McpOAuthProvider {
    pub fn new(opts: ProviderOptions) -> Self {
        let state = opts.state.clone().unwrap_or_else(new_state);
        Self { opts, verifier: None, last_refresh: None, failed_refresh: None, fresh: None, state }
    }

    fn port(&self) -> Option<u16> {
        self.opts.oauth.as_ref().and_then(|o| o.callback_port).or(self.opts.port)
    }

    /// Always defined: an undefined redirect URI made the SDK try a non-interactive (client credentials) grant.
    pub fn redirect_url(&self) -> String {
        redirect_url_for(self.port())
    }

    /// RFC 7591 client metadata: a public client (`token_endpoint_auth_method:
    /// none`: nothing secret can live in a program the user runs, PKCE stands in
    /// for it) unless the config gives a secret.
    pub fn client_metadata(&self) -> Value {
        let oauth = self.opts.oauth.clone().unwrap_or_default();
        let mut meta = json!({
            "client_name": "Marv",
            "client_uri": REPOSITORY,
            "redirect_uris": [self.redirect_url()],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": if oauth.client_secret.is_some() { "client_secret_post" } else { "none" },
        });
        if let Some(scope) = oauth.scope {
            meta["scope"] = scope.into();
        }
        meta
    }

    /// The login's `state`: random, and the same for the whole login, so the callback can be checked against it.
    pub fn state(&self) -> &str {
        &self.state
    }

    fn passive(&self) -> bool {
        self.opts.mode == ProviderMode::Passive
    }

    fn check_abort(&self) -> crate::Result<()> {
        self.opts.abort.as_ref().map_or(Ok(()), Abort::check)
    }

    /// The configured client (config `clientId`/`clientSecret` first), or the one registered for Marv (from the store).
    pub fn client_information(&self) -> Option<StoredClient> {
        let oauth = self.opts.oauth.as_ref();
        if let Some(id) = oauth.and_then(|o| o.client_id.clone()) {
            let mut rest = Map::new();
            if let Some(secret) = oauth.and_then(|o| o.client_secret.clone()) {
                rest.insert("client_secret".into(), secret.into());
                rest.insert("token_endpoint_auth_method".into(), "client_secret_post".into());
            }
            return Some(StoredClient { client_id: id, port: None, rest });
        }
        let client = self.opts.store.get(&self.opts.key)?.client?;
        Some(StoredClient { port: None, ..client })
    }

    /// Passive: raises [`LoginNeeded`] (a background connection never registers
    /// Marv with a server the user didn't log in to). Interactive: stored with
    /// the port its redirect URI names, unless the login was cancelled.
    pub async fn save_client_information(&self, info: StoredClient) -> crate::Result<()> {
        // A configured client lives in the config (its secret too): nothing to store.
        if self.opts.oauth.as_ref().is_some_and(|o| o.client_id.is_some()) {
            return Ok(());
        }
        if self.passive() {
            return Err(LoginNeeded { server: self.opts.server.clone() }.into());
        }
        self.check_abort()?;
        let (server, port) = (self.opts.server.clone(), self.port());
        self.opts
            .store
            .update(&self.opts.key, |e| {
                self.check_abort()?;
                let mut e = e.unwrap_or_else(|| Login::new(&server));
                e.server = server;
                e.client = Some(StoredClient { port, ..info });
                Ok(Some(e))
            })
            .await
    }

    /// Read from the file every time: another Marv process may have refreshed
    /// (and rotated) the tokens. Interactive: only this login's own tokens.
    pub fn tokens(&mut self) -> Option<StoredTokens> {
        if !self.passive() {
            return self.fresh.clone();
        }
        let tokens = self.opts.store.get(&self.opts.key).and_then(|l| l.tokens);
        self.last_refresh = tokens.as_ref().and_then(|t| t.refresh_token.clone());
        tokens
    }

    /// Stored with when (for `/mcp`'s "expires at"); nothing once the login was cancelled.
    pub async fn save_tokens(&mut self, tokens: StoredTokens) -> crate::Result<()> {
        // Checked inside the update too: the abort may come while it waits for the file's lock.
        self.check_abort()?;
        if !self.passive() {
            self.fresh = Some(tokens.clone());
        }
        let server = self.opts.server.clone();
        let this = &*self;
        this.opts
            .store
            .update(&this.opts.key, |e| {
                this.check_abort()?;
                let mut e = e.unwrap_or_else(|| Login::new(&server));
                e.server = server;
                e.tokens = Some(tokens);
                e.saved_at = Some(now_ms());
                Ok(Some(e))
            })
            .await
    }

    /// Passive: raises [`LoginNeeded`]. Interactive: rewrites `scope` from the
    /// config, checks [`authorization_url_ok`], hands the URL to `on_authorize`.
    pub fn redirect_to_authorization(&self, mut url: Url) -> crate::Result<()> {
        let Some(authorize) = self.opts.on_authorize.as_ref().filter(|_| !self.passive()) else {
            return Err(LoginNeeded { server: self.opts.server.clone() }.into());
        };
        if !authorization_url_ok(&url) {
            let host = url.host_str().unwrap_or_default();
            return Err(McpError::Server(format!(
                "the authorization server sent a login address Marv won't open ({}://{host})",
                url.scheme()
            )));
        }
        if let Some(scope) = self.opts.oauth.as_ref().and_then(|o| o.scope.clone()) {
            let mut pairs: Vec<(String, String)> =
                url.query_pairs().filter(|(k, _)| k != "scope").map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
            pairs.push(("scope".into(), scope.clone()));
            if scope.split_whitespace().any(|s| s == "offline_access") && !pairs.iter().any(|(k, _)| k == "prompt") {
                pairs.push(("prompt".into(), "consent".into()));
            }
            url.query_pairs_mut().clear().extend_pairs(pairs);
        }
        authorize(url);
        Ok(())
    }

    /// In memory only.
    pub fn save_code_verifier(&mut self, verifier: String) {
        self.verifier = Some(verifier);
    }

    pub fn code_verifier(&self) -> crate::Result<&str> {
        self.verifier.as_deref().ok_or_else(|| McpError::Server("no login is in progress".into()))
    }

    /// Cached in the store, so a startup with tokens makes no discovery requests.
    pub fn discovery_state(&self) -> Option<StoredDiscovery> {
        self.opts.store.get(&self.opts.key).and_then(|l| l.discovery)
    }

    /// As `save_tokens`: a cancelled login writes nothing, checked again once the file's lock is held.
    pub async fn save_discovery_state(&self, state: StoredDiscovery) -> crate::Result<()> {
        self.check_abort()?;
        let server = self.opts.server.clone();
        self.opts
            .store
            .update(&self.opts.key, |e| {
                self.check_abort()?;
                let mut e = e.unwrap_or_else(|| Login::new(&server));
                e.server = server;
                e.discovery = Some(state);
                Ok(Some(e))
            })
            .await
    }

    /// `Tokens` drops only the refresh token that failed (see module docs).
    pub async fn invalidate_credentials(&mut self, scope: Invalidate) -> crate::Result<()> {
        if scope == Invalidate::Verifier {
            self.verifier = None;
            return Ok(());
        }
        let failed = self.failed_refresh.take().or_else(|| self.last_refresh.clone());
        self.opts
            .store
            .update(&self.opts.key, |e| {
                let Some(mut next) = e else { return Ok(None) };
                if matches!(scope, Invalidate::All | Invalidate::Client) {
                    next.client = None;
                }
                if matches!(scope, Invalidate::All | Invalidate::Discovery) {
                    next.discovery = None;
                }
                // Another process may have refreshed since this one read the tokens: its newer refresh token is
                // kept, and the retry refreshes with it. Only the one that failed here is dropped.
                let stale = next.tokens.as_ref().map(|t| t.refresh_token.clone()) == Some(failed.clone());
                if scope == Invalidate::All || (scope == Invalidate::Tokens && stale) {
                    next.tokens = None;
                    next.saved_at = None;
                }
                Ok(Some(next))
            })
            .await
    }

    /// The refresh hook: notes the refresh token a refused refresh sent. Two
    /// refreshes can run at once (two requests both got a 401): the one that
    /// sent the stale token fails, and must not delete the tokens the other one
    /// just saved.
    pub fn note_refused_refresh(&mut self, refresh_token: String) {
        self.failed_refresh = Some(refresh_token);
    }
}

/// What `auth` ended with: tokens saved, or the user was sent to the browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthResult {
    Authorized,
    Redirect,
}

/// The `resource_metadata` a 401's `WWW-Authenticate: Bearer` header names (RFC 9728 §5.1).
pub fn resource_metadata_of(www_authenticate: &str) -> Option<Url> {
    let at = www_authenticate.find("resource_metadata=")? + "resource_metadata=".len();
    let rest = &www_authenticate[at..];
    let value = match rest.strip_prefix('"') {
        Some(quoted) => &quoted[..quoted.find('"')?],
        None => rest.split([',', ' ']).next()?,
    };
    Url::parse(value).ok()
}

/// One JSON object from a metadata URL: `Ok(None)` for an answer that isn't
/// one (404 and the like: try the next URL), `Err` when the network failed.
async fn metadata(url: &Url) -> Result<Option<Map<String, Value>>, reqwest::Error> {
    let response = http().get(url.clone()).header(PROTOCOL_HEADER, crate::jsonrpc::PROTOCOL_VERSION).send().await?;
    if !response.status().is_success() {
        return Ok(None);
    }
    Ok(response.json::<Value>().await.ok().and_then(|v| if let Value::Object(m) = v { Some(m) } else { None }))
}

fn well_known(base: &Url, name: &str, path: &str) -> Option<Url> {
    let path = path.trim_end_matches('/');
    base.join(&format!("/.well-known/{name}{path}")).ok()
}

/// RFC 9728 protected-resource metadata: from the URL a 401 named, else the
/// well-known URL for the server's path, then for its root.
pub async fn discover_resource_metadata(
    server: &Url,
    named: Option<&Url>,
) -> Result<Option<(Url, Map<String, Value>)>, reqwest::Error> {
    let candidates: Vec<Url> = match named {
        Some(url) => vec![url.clone()],
        None => {
            let mut c = Vec::new();
            if server.path() != "/" {
                c.extend(well_known(server, "oauth-protected-resource", server.path()));
            }
            c.extend(well_known(server, "oauth-protected-resource", ""));
            c
        }
    };
    for url in candidates {
        if let Some(meta) = metadata(&url).await? {
            return Ok(Some((url, meta)));
        }
    }
    Ok(None)
}

/// RFC 8414 (or OpenID) metadata of an authorization server.
async fn discover_server_metadata(issuer: &Url) -> Option<Map<String, Value>> {
    let path = issuer.path().trim_end_matches('/');
    let urls: Vec<Option<Url>> = if path.is_empty() {
        vec![well_known(issuer, "oauth-authorization-server", ""), well_known(issuer, "openid-configuration", "")]
    } else {
        vec![
            well_known(issuer, "oauth-authorization-server", path),
            well_known(issuer, "openid-configuration", path),
            issuer.join(&format!("{path}/.well-known/openid-configuration")).ok(),
        ]
    };
    for url in urls.into_iter().flatten() {
        if let Ok(Some(meta)) = metadata(&url).await {
            return Some(meta);
        }
    }
    None
}

/// Why a token request failed: the server's OAuth error code, or anything else.
enum TokenFailure {
    OAuth(String),
    Other(McpError),
}

impl From<McpError> for TokenFailure {
    fn from(e: McpError) -> Self {
        TokenFailure::Other(e)
    }
}

/// A token endpoint request (the code exchange or a refresh), authenticated as the client.
async fn token_request(
    endpoint: &Url,
    client: &StoredClient,
    mut form: Vec<(&str, String)>,
) -> Result<StoredTokens, TokenFailure> {
    form.push(("client_id", client.client_id.clone()));
    if let Some(secret) = client.rest.get("client_secret").and_then(Value::as_str) {
        form.push(("client_secret", secret.to_string()));
    }
    let body = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(form).finish();
    let response = http()
        .post(endpoint.clone())
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| McpError::Server(format!("the token request failed: {e}")))?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(match body.get("error").and_then(Value::as_str) {
            Some(code) => TokenFailure::OAuth(code.to_string()),
            None => TokenFailure::Other(McpError::Server(format!("the token endpoint answered {}", status.as_u16()))),
        });
    }
    serde_json::from_value(body).map_err(|_| McpError::Server("the token endpoint's answer has no access_token".into()).into())
}

fn token_error(failure: TokenFailure) -> McpError {
    match failure {
        // The server's code only (a short word), never its description.
        TokenFailure::OAuth(code) => {
            McpError::Server(format!("the authorization server refused: {}", crate::text::truncate(&code, 64)))
        }
        TokenFailure::Other(e) => e,
    }
}

fn endpoint(meta: &Map<String, Value>, name: &str, issuer: &Url, fallback: &str) -> crate::Result<Url> {
    match meta.get(name).and_then(Value::as_str) {
        Some(url) => Url::parse(url).map_err(|_| McpError::Server(format!("the authorization server's {name} isn't a URL"))),
        None => issuer.join(fallback).map_err(|_| McpError::Server(format!("no {name}"))),
    }
}

/// The OAuth flow for one server (the MCP SDK's `auth()`): discovery (cached
/// in the store), the client (configured, stored, or registered: interactive
/// only), then the code exchange when `code` is given, else a refresh when
/// there's a refresh token, else the authorization redirect (PKCE S256).
pub async fn auth(
    p: &mut McpOAuthProvider,
    server: &Url,
    named_metadata: Option<&Url>,
    code: Option<&str>,
) -> crate::Result<AuthResult> {
    let net = |e: reqwest::Error| McpError::Server(format!("the login's discovery failed: {e}"));
    let cached = p.discovery_state().filter(|d| d.rest.contains_key("authorizationServerMetadata"));
    let (issuer, resource_meta, server_meta) = match cached {
        Some(d) => {
            let issuer =
                Url::parse(&d.authorization_server_url).map_err(|_| McpError::Server("stored discovery is broken".into()))?;
            let object = |k: &str| d.rest.get(k).and_then(Value::as_object).cloned();
            (issuer, object("resourceMetadata"), object("authorizationServerMetadata").unwrap_or_default())
        }
        None => {
            // A server without RFC 9728 metadata: its own origin is the authorization server (the older spec).
            let found = discover_resource_metadata(server, named_metadata).await.map_err(net)?;
            let issuer = found
                .as_ref()
                .and_then(|(_, m)| m.get("authorization_servers")?.as_array()?.first()?.as_str().map(str::to_string))
                .and_then(|u| Url::parse(&u).ok())
                .unwrap_or_else(|| server.join("/").unwrap_or_else(|_| server.clone()));
            let server_meta = discover_server_metadata(&issuer).await.unwrap_or_default();
            let mut rest = Map::new();
            if let Some((url, meta)) = &found {
                rest.insert("resourceMetadataUrl".into(), url.as_str().into());
                rest.insert("resourceMetadata".into(), Value::Object(meta.clone()));
            }
            rest.insert("authorizationServerMetadata".into(), Value::Object(server_meta.clone()));
            p.save_discovery_state(StoredDiscovery { authorization_server_url: issuer.to_string(), rest }).await?;
            (issuer, found.map(|(_, m)| m), server_meta)
        }
    };
    let resource = resource_meta.as_ref().and_then(|m| m.get("resource")?.as_str().map(str::to_string));
    let token_endpoint = endpoint(&server_meta, "token_endpoint", &issuer, "/token")?;

    let client = match p.client_information() {
        Some(client) => client,
        None => {
            if code.is_some() {
                return Err(McpError::Server("the login lost its client registration: start it again".into()));
            }
            // marv-rs: refused before any request (the SDK registered first and failed to save after).
            if p.passive() {
                return Err(LoginNeeded { server: p.opts.server.clone() }.into());
            }
            let Some(register) = server_meta.get("registration_endpoint").and_then(Value::as_str) else {
                return Err(McpError::Server("Incompatible auth server: does not support dynamic client registration".into()));
            };
            let register = Url::parse(register).map_err(|_| McpError::Server("the registration endpoint isn't a URL".into()))?;
            let response = http()
                .post(register)
                .json(&p.client_metadata())
                .send()
                .await
                .map_err(|e| McpError::Server(format!("dynamic client registration failed: {e}")))?;
            if !response.status().is_success() {
                return Err(McpError::Server(format!("dynamic client registration failed: HTTP {}", response.status().as_u16())));
            }
            let info: StoredClient = response
                .json()
                .await
                .map_err(|_| McpError::Server("dynamic client registration answered without a client_id".into()))?;
            p.save_client_information(info.clone()).await?;
            info
        }
    };

    if let Some(code) = code {
        let mut form = vec![
            ("grant_type", "authorization_code".to_string()),
            ("code", code.to_string()),
            ("code_verifier", p.code_verifier()?.to_string()),
            ("redirect_uri", p.redirect_url()),
        ];
        form.extend(resource.clone().map(|r| ("resource", r)));
        let tokens = token_request(&token_endpoint, &client, form).await.map_err(token_error)?;
        p.save_tokens(tokens).await?;
        return Ok(AuthResult::Authorized);
    }

    if let Some(refresh) = p.tokens().and_then(|t| t.refresh_token) {
        let mut form = vec![("grant_type", "refresh_token".to_string()), ("refresh_token", refresh.clone())];
        form.extend(resource.clone().map(|r| ("resource", r)));
        match token_request(&token_endpoint, &client, form).await {
            Ok(mut tokens) => {
                // A server that doesn't rotate leaves the refresh token out: keep the one that worked.
                tokens.refresh_token.get_or_insert(refresh);
                p.save_tokens(tokens).await?;
                return Ok(AuthResult::Authorized);
            }
            Err(TokenFailure::OAuth(code)) => {
                p.note_refused_refresh(refresh);
                match code.as_str() {
                    "invalid_client" | "unauthorized_client" => p.invalidate_credentials(Invalidate::All).await?,
                    "invalid_grant" => p.invalidate_credentials(Invalidate::Tokens).await?,
                    _ => return Err(token_error(TokenFailure::OAuth(code))),
                }
            }
            Err(other) => return Err(token_error(other)),
        }
    }

    // A new authorization: passive providers refuse here (LoginNeeded).
    if p.passive() {
        return Err(LoginNeeded { server: p.opts.server.clone() }.into());
    }
    let (verifier, challenge) = pkce();
    p.save_code_verifier(verifier);
    let mut url = endpoint(&server_meta, "authorization_endpoint", &issuer, "/authorize")?;
    let metadata = p.client_metadata();
    let scope = metadata["scope"].as_str().map(str::to_string).or_else(|| {
        let supported = resource_meta.as_ref()?.get("scopes_supported")?.as_array()?;
        let scopes: Vec<&str> = supported.iter().filter_map(Value::as_str).collect();
        (!scopes.is_empty()).then(|| scopes.join(" "))
    });
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("response_type", "code")
            .append_pair("client_id", &client.client_id)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("redirect_uri", &p.redirect_url())
            .append_pair("state", p.state());
        if let Some(scope) = &scope {
            q.append_pair("scope", scope);
        }
        if let Some(resource) = &resource {
            q.append_pair("resource", resource);
        }
    }
    p.redirect_to_authorization(url)?;
    Ok(AuthResult::Redirect)
}

/// The outcome of one callback address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallbackCheck {
    Ok {
        code: String,
    },
    /// `path`, `state`, `used`, `no code`, or the server's short error code.
    Refused {
        why: String,
    },
}

/// Constant-time comparison (Marv: `timingSafeEqual`).
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// What a login waits for: one callback carrying its `state`. `check`
/// decides about each address that arrives (from the browser, or pasted);
/// [`CallbackExpectation::outcome`] gets the first valid one's code, or why the
/// login failed.
#[derive(Debug)]
pub struct CallbackExpectation {
    state: String,
    used: bool,
    /// Set by an `error=` callback or a code-less one: the login fails with it.
    pub failure: Option<String>,
    outcome: tokio::sync::watch::Sender<Option<Result<String, String>>>,
}

impl CallbackExpectation {
    pub fn new(state: impl Into<String>) -> Self {
        Self { state: state.into(), used: false, failure: None, outcome: tokio::sync::watch::Sender::new(None) }
    }

    /// The code, or why there won't be one.
    pub fn outcome(&self) -> tokio::sync::watch::Receiver<Option<Result<String, String>>> {
        self.outcome.subscribe()
    }

    fn settle(&self, outcome: Result<String, String>) {
        self.outcome.send_if_modified(|slot| {
            if slot.is_some() {
                return false;
            }
            *slot = Some(outcome);
            true
        });
    }

    /// Decides about one address (from the listener, or pasted).
    pub fn check(&mut self, url: &Url) -> CallbackCheck {
        let refused = |why: &str| CallbackCheck::Refused { why: why.into() };
        if url.path() != "/callback" {
            return refused("path");
        }
        let param = |name: &str| url.query_pairs().find(|(k, _)| k == name).map(|(_, v)| v.into_owned());
        // Without the state, nothing a sandboxed command (which shares the network) or another page sends counts.
        if !param("state").is_some_and(|s| same(s.as_bytes(), self.state.as_bytes())) {
            return refused("state");
        }
        if self.used {
            return refused("used");
        }
        self.used = true;
        if let Some(error) = param("error") {
            // The server's text goes to the screen only as a short code (access_denied, …).
            let ok =
                !error.is_empty() && error.len() <= 64 && error.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b));
            let why = if ok { error } else { "error".into() };
            let failure = format!("the authorization server refused the login ({why})");
            self.settle(Err(failure.clone()));
            self.failure = Some(failure);
            return CallbackCheck::Refused { why };
        }
        match param("code").filter(|c| !c.is_empty()) {
            Some(code) => {
                self.settle(Ok(code.clone()));
                CallbackCheck::Ok { code }
            }
            None => {
                let failure = "the authorization server came back without a code".to_string();
                self.settle(Err(failure.clone()));
                self.failure = Some(failure);
                refused("no code")
            }
        }
    }

    /// The login was cancelled: later callbacks are `used`.
    pub fn cancel(&mut self, why: &str) {
        self.used = true;
        self.settle(Err(why.to_string()));
    }
}

/// A pasted message that's a login callback address alone: the App hands it to
/// the login instead of sending it to the model.
pub fn is_callback(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.chars().any(char::is_whitespace) {
        return false;
    }
    let Ok(url) = Url::parse(trimmed) else { return false };
    let has = |k: &str| url.query_pairs().any(|(q, _)| q == k);
    url.scheme() == "http" && is_loopback_host(&url) && url.path() == "/callback" && has("state") && (has("code") || has("error"))
}

/// What the browser is shown: (status, html). Nothing from the request is echoed.
pub fn callback_page(check: &CallbackCheck) -> (u16, String) {
    let page = |title: &str, text: &str| {
        format!(
            "<!doctype html><meta charset=\"utf-8\"><title>Marv: {title}</title><body style=\"font-family:sans-serif;margin:3em\"><h1>{title}</h1><p>{text}</p>"
        )
    };
    match check {
        CallbackCheck::Ok { .. } => (200, page("Logged in", "You can close this tab and go back to Marv.")),
        CallbackCheck::Refused { why } if why == "path" => (404, "Not found".into()),
        CallbackCheck::Refused { why } if why == "state" || why == "used" => {
            (400, page("Not this login", "This isn't the login Marv is waiting for."))
        }
        CallbackCheck::Refused { .. } => {
            (400, page("Login failed", "The login was refused. Marv says why; you can close this tab."))
        }
    }
}

/// Decides about one address that reached the listener.
pub type CallbackHandler = Arc<dyn Fn(&Url) -> CallbackCheck + Send + Sync>;

/// The loopback listener the browser is sent back to: 127.0.0.1 only,
/// `/callback` only. Stops when closed or dropped.
pub struct Listener {
    pub port: u16,
    /// The preferred port was taken: the client has to be registered again.
    pub fell_back: bool,
    stop: CancellationToken,
}

impl Listener {
    pub fn close(&self) {
        self.stop.cancel();
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

/// Listens on 127.0.0.1, `preferred` first (the port a client was registered
/// with), else any free port, and says it fell back.
pub async fn listen(check: CallbackHandler, preferred: Option<u16>) -> crate::Result<Listener> {
    use tokio::net::TcpListener;
    let (listener, fell_back) = match preferred {
        Some(port) => match TcpListener::bind(("127.0.0.1", port)).await {
            Ok(l) => (l, false),
            Err(_) => (TcpListener::bind(("127.0.0.1", 0)).await?, true),
        },
        None => (TcpListener::bind(("127.0.0.1", 0)).await?, false),
    };
    let port = listener.local_addr()?.port();
    let stop = CancellationToken::new();
    let stopped = stop.clone();
    tokio::spawn(async move {
        loop {
            let accepted = tokio::select! {
                _ = stopped.cancelled() => break,
                a = listener.accept() => a,
            };
            let Ok((stream, _)) = accepted else { continue };
            let check = check.clone();
            tokio::spawn(async move {
                let _ = tokio::time::timeout(Duration::from_secs(10), answer(stream, port, &*check)).await;
            });
        }
    });
    Ok(Listener { port, fell_back, stop })
}

/// One browser request: its target is checked, the page says what happened, and nothing else is read.
async fn answer(
    mut stream: tokio::net::TcpStream,
    port: u16,
    check: &(dyn Fn(&Url) -> CallbackCheck + Send + Sync),
) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut head = Vec::new();
    let mut buf = [0u8; 2048];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < 16 * 1024 {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        head.extend_from_slice(&buf[..n]);
    }
    let line = String::from_utf8_lossy(&head).lines().next().unwrap_or_default().to_string();
    let target = line.split(' ').nth(1).unwrap_or("/");
    let verdict = Url::parse(&format!("http://127.0.0.1:{port}"))
        .and_then(|base| base.join(target))
        .map_or(CallbackCheck::Refused { why: "path".into() }, |url| check(&url));
    let (status, body) = callback_page(&verdict);
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Bad Request",
    };
    let kind = if status == 404 { "text/plain" } else { "text/html; charset=utf-8" };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: {kind}\r\ncontent-length: {}\r\ncache-control: no-store\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

/// Whether a browser can open here: not over SSH, not without a display.
pub fn can_open_browser(env: &crate::Env, platform: &str) -> bool {
    if env.contains_key("SSH_CONNECTION") || env.contains_key("SSH_TTY") {
        return false;
    }
    match platform {
        "macos" | "darwin" => true,
        "linux" => env.contains_key("DISPLAY") || env.contains_key("WAYLAND_DISPLAY"),
        _ => false,
    }
}

/// Opens the URL in the browser: the URL is one argument, never through a
/// shell. Failures are ignored: the link is shown anyway.
pub fn open_browser(url: &Url, platform: &str) {
    open_browser_with(url, platform, &|argv| {
        let _ = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    })
}

/// [`open_browser`] with its spawner given (tests).
pub fn open_browser_with(url: &Url, platform: &str, spawn: &dyn Fn(&[String])) {
    let opener = if matches!(platform, "macos" | "darwin") { "open" } else { "xdg-open" };
    spawn(&[opener.to_string(), url.as_str().to_string()]);
}

/// Whether a remote server would ask for a login (for `/mcp add`, before it's
/// installed): an unauthenticated RFC 9728 metadata read. Never registers,
/// never fails, gives up after 5 s or when `cancel` fires (Esc during `/mcp add`).
pub async fn probe_login(url: &str, cancel: CancellationToken) -> bool {
    probe_login_within(url, NETWORK_TIMEOUT, cancel).await
}

pub async fn probe_login_within(url: &str, timeout: Duration, cancel: CancellationToken) -> bool {
    let Ok(server) = Url::parse(url) else { return false };
    let probe = async {
        match discover_resource_metadata(&server, None).await {
            Ok(Some((_, meta))) => meta.get("authorization_servers").and_then(Value::as_array).is_some_and(|a| !a.is_empty()),
            _ => false,
        }
    };
    tokio::select! {
        found = tokio::time::timeout(timeout, probe) => found.unwrap_or(false),
        _ = cancel.cancelled() => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Revoked {
    Revoked,
    /// The server lists no revocation endpoint: the grant stays on its side.
    Unsupported,
    Failed,
}

/// RFC 7009 revocation of a login's refresh token, best effort, 5 s. The
/// endpoint comes from the stored metadata and must pass [`authorization_url_ok`].
///
/// It isn't required to be on the authorization server's host: it comes from
/// that server's own metadata (fetched by Marv during the login), and real
/// providers put it elsewhere (Google: accounts.google.com issues,
/// oauth2.googleapis.com/revoke revokes). The one way to point it somewhere
/// else is to edit `~/.marv/mcp-tokens.json`, and whoever can do that already
/// holds the tokens it would send.
pub async fn revoke(login: &Login, oauth: Option<&McpOAuthConfig>) -> Revoked {
    revoke_within(login, oauth, NETWORK_TIMEOUT).await
}

pub async fn revoke_within(login: &Login, oauth: Option<&McpOAuthConfig>, timeout: Duration) -> Revoked {
    let endpoint = login
        .discovery
        .as_ref()
        .and_then(|d| d.rest.get("authorizationServerMetadata")?.get("revocation_endpoint")?.as_str().map(str::to_string));
    let tokens = login.tokens.as_ref();
    let token = tokens.and_then(|t| t.refresh_token.clone()).or_else(|| tokens.map(|t| t.access_token.clone()));
    let (Some(endpoint), Some(token)) = (endpoint, token) else { return Revoked::Unsupported };
    let Ok(target) = Url::parse(&endpoint) else { return Revoked::Failed };
    if !authorization_url_ok(&target) {
        return Revoked::Failed;
    }
    let configured = oauth.and_then(|o| o.client_id.clone());
    let hint = if tokens.is_some_and(|t| t.refresh_token.is_some()) { "refresh_token" } else { "access_token" };
    let mut form = vec![("token", token), ("token_type_hint", hint.to_string())];
    if let Some(id) = configured.clone().or_else(|| login.client.as_ref().map(|c| c.client_id.clone())) {
        form.push(("client_id", id));
    }
    let secret = match configured {
        Some(_) => oauth.and_then(|o| o.client_secret.clone()),
        None => login.client.as_ref().and_then(|c| c.rest.get("client_secret")?.as_str().map(str::to_string)),
    };
    if let Some(secret) = secret {
        form.push(("client_secret", secret));
    }
    let body = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(form).finish();
    let request = http().post(target).header("content-type", "application/x-www-form-urlencoded").body(body).send();
    match tokio::time::timeout(timeout, request).await {
        Ok(Ok(response)) if response.status().is_success() => Revoked::Revoked,
        _ => Revoked::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_and_authorization_urls() {
        assert_eq!(redirect_url_for(Some(33418)), "http://127.0.0.1:33418/callback");
        assert!(authorization_url_ok(&Url::parse("https://a.example/authorize").unwrap()));
        assert!(authorization_url_ok(&Url::parse("http://127.0.0.1:9/authorize").unwrap()));
        assert!(!authorization_url_ok(&Url::parse("http://a.example/authorize").unwrap()));
        assert!(!authorization_url_ok(&Url::parse("javascript:alert(1)").unwrap()));
    }

    #[test]
    fn callbacks_need_the_state_and_count_once() {
        let mut exp = CallbackExpectation::new("s3cret");
        let u = |q: &str| Url::parse(&format!("http://127.0.0.1:5/{q}")).unwrap();
        assert_eq!(exp.check(&u("other?state=s3cret&code=c")), CallbackCheck::Refused { why: "path".into() });
        assert_eq!(exp.check(&u("callback?state=wrong&code=c")), CallbackCheck::Refused { why: "state".into() });
        assert_eq!(exp.check(&u("callback?state=s3cret&code=c")), CallbackCheck::Ok { code: "c".into() });
        assert_eq!(exp.check(&u("callback?state=s3cret&code=d")), CallbackCheck::Refused { why: "used".into() });
        let mut exp = CallbackExpectation::new("s");
        assert_eq!(exp.check(&u("callback?state=s&error=<script>")), CallbackCheck::Refused { why: "error".into() });
        assert_eq!(exp.failure.as_deref(), Some("the authorization server refused the login (error)"));
    }

    #[test]
    fn a_pasted_callback_is_recognised_only_alone() {
        assert!(is_callback(" http://127.0.0.1:4/callback?state=s&code=c "));
        assert!(is_callback("http://localhost/callback?state=s&error=x"));
        assert!(!is_callback("see http://127.0.0.1/callback?state=s&code=c"));
        assert!(!is_callback("https://127.0.0.1/callback?state=s&code=c"));
        assert!(!is_callback("http://evil.example/callback?state=s&code=c"));
        assert!(!is_callback("http://127.0.0.1/callback?code=c"));
    }

    #[test]
    fn pages_echo_nothing() {
        let (status, html) = callback_page(&CallbackCheck::Refused { why: "access_denied".into() });
        assert_eq!(status, 400);
        assert!(!html.contains("access_denied"));
    }

    #[test]
    fn browsers_open_only_with_a_display_and_not_over_ssh() {
        let env = |pairs: &[&str]| pairs.iter().map(|k| (k.to_string(), "1".to_string())).collect::<crate::Env>();
        assert!(can_open_browser(&env(&["WAYLAND_DISPLAY"]), "linux"));
        assert!(!can_open_browser(&env(&[]), "linux"));
        assert!(!can_open_browser(&env(&["DISPLAY", "SSH_TTY"]), "linux"));
        assert!(can_open_browser(&env(&[]), "macos"));
    }

    #[test]
    fn login_needed_is_marvs_sentence() {
        assert_eq!(LoginNeeded { server: "x".into() }.to_string(), "x needs a new login: the user can run /mcp login x");
    }

    // ---- ports of tests/mcp-oauth.test.ts ----

    use crate::oauth_store::{StoredClient, StoredDiscovery, StoredTokens};
    use serde_json::json;

    const KEY: &str = "personal https://x.test/mcp";

    fn new_store() -> McpTokenStore {
        McpTokenStore::new(crate::testutil::temp_dir("oauth").join("mcp-tokens.json"))
    }

    fn provider(
        mode: ProviderMode,
        port: Option<u16>,
        oauth: Option<McpOAuthConfig>,
        store: Option<McpTokenStore>,
    ) -> McpOAuthProvider {
        McpOAuthProvider::new(ProviderOptions {
            store: store.unwrap_or_else(new_store),
            key: KEY.into(),
            server: "x".into(),
            mode,
            port,
            oauth,
            state: None,
            on_authorize: None,
            abort: None,
        })
    }

    fn seen() -> (Authorize, Arc<Mutex<Vec<Url>>>) {
        let seen = Arc::new(Mutex::new(vec![]));
        let s = seen.clone();
        (Arc::new(move |u| s.lock().unwrap().push(u)), seen)
    }

    fn tokens(v: serde_json::Value) -> StoredTokens {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn redirect_url_is_loopback_and_the_metadata_a_public_client() {
        let p = provider(ProviderMode::Interactive, Some(50214), None, None);
        assert_eq!(p.redirect_url(), "http://127.0.0.1:50214/callback");
        let meta = p.client_metadata();
        assert_eq!(meta["client_name"], "Marv");
        assert_eq!(meta["redirect_uris"], json!(["http://127.0.0.1:50214/callback"]));
        assert_eq!(meta["grant_types"], json!(["authorization_code", "refresh_token"]));
        assert_eq!(meta["response_types"], json!(["code"]));
        assert_eq!(meta["token_endpoint_auth_method"], "none");
        // Passive, with no port known: still defined.
        assert_eq!(provider(ProviderMode::Passive, None, None, None).redirect_url(), "http://127.0.0.1/callback");
    }

    #[tokio::test]
    async fn passive_never_redirects_or_registers() {
        let store = new_store();
        let p = provider(ProviderMode::Passive, None, None, Some(store.clone()));
        let login_needed = |r: crate::Result<()>| matches!(r, Err(McpError::LoginNeeded(_)));
        assert!(login_needed(p.redirect_to_authorization(Url::parse("https://as.test/authorize").unwrap())));
        let client = StoredClient { client_id: "x".into(), port: None, rest: Default::default() };
        assert!(login_needed(p.save_client_information(client).await));
        assert!(store.get(KEY).is_none());
    }

    #[test]
    fn interactive_hands_an_https_or_loopback_url_to_the_ui_and_refuses_anything_else() {
        let (authorize, seen) = seen();
        let mut p = provider(ProviderMode::Interactive, Some(1), None, None);
        p.opts.on_authorize = Some(authorize);
        p.redirect_to_authorization(Url::parse("https://as.test/authorize?x=1").unwrap()).unwrap();
        p.redirect_to_authorization(Url::parse("http://127.0.0.1:4000/authorize").unwrap()).unwrap();
        for bad in ["file:///etc/passwd", "http://evil.test/authorize", "javascript:alert(1)"] {
            assert!(p.redirect_to_authorization(Url::parse(bad).unwrap()).is_err(), "{bad}");
        }
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_registered_client_is_stored_with_its_port() {
        let store = new_store();
        let p = provider(ProviderMode::Interactive, Some(50214), None, Some(store.clone()));
        let info: StoredClient =
            serde_json::from_value(json!({"client_id": "dyn", "redirect_uris": ["http://127.0.0.1:50214/callback"]})).unwrap();
        p.save_client_information(info.clone()).await.unwrap();
        let stored = store.get(KEY).unwrap().client.unwrap();
        assert_eq!((stored.client_id.as_str(), stored.port), ("dyn", Some(50214)));
        assert_eq!(p.client_information(), Some(info));
    }

    #[tokio::test]
    async fn a_configured_client_secret_and_port() {
        let plain = provider(
            ProviderMode::Passive,
            None,
            Some(McpOAuthConfig { client_id: Some("c".into()), ..Default::default() }),
            None,
        );
        assert_eq!(serde_json::to_value(plain.client_information().unwrap()).unwrap(), json!({"client_id": "c"}));
        let oauth = McpOAuthConfig {
            client_id: Some("c".into()),
            client_secret: Some("s".into()),
            callback_port: Some(33418),
            ..Default::default()
        };
        let store = new_store();
        let secret = provider(ProviderMode::Interactive, Some(9), Some(oauth), Some(store.clone()));
        assert_eq!(
            serde_json::to_value(secret.client_information().unwrap()).unwrap(),
            json!({"client_id": "c", "client_secret": "s", "token_endpoint_auth_method": "client_secret_post"})
        );
        assert_eq!(secret.redirect_url(), "http://127.0.0.1:33418/callback");
        assert_eq!(secret.client_metadata()["token_endpoint_auth_method"], "client_secret_post");
        // Saving it doesn't store the secret.
        secret.save_client_information(secret.client_information().unwrap()).await.unwrap();
        assert!(store.get(KEY).is_none());
    }

    #[test]
    fn a_config_scope_overrides_the_scope_in_the_authorization_url() {
        let (authorize, seen) = seen();
        let oauth = McpOAuthConfig { scope: Some("repo read:org".into()), ..Default::default() };
        let mut p = provider(ProviderMode::Interactive, Some(1), Some(oauth), None);
        p.opts.on_authorize = Some(authorize.clone());
        assert_eq!(p.client_metadata()["scope"], "repo read:org");
        p.redirect_to_authorization(Url::parse("https://as.test/authorize?scope=everything&state=s").unwrap()).unwrap();
        let mut q = provider(ProviderMode::Interactive, Some(1), None, None);
        q.opts.on_authorize = Some(authorize);
        q.redirect_to_authorization(Url::parse("https://as.test/authorize?scope=everything").unwrap()).unwrap();
        let seen = seen.lock().unwrap();
        let param = |u: &Url, k: &str| u.query_pairs().find(|(q, _)| q == k).map(|(_, v)| v.into_owned());
        assert_eq!(param(&seen[0], "scope").as_deref(), Some("repo read:org"));
        assert_eq!(param(&seen[0], "state").as_deref(), Some("s"));
        assert_eq!(param(&seen[1], "scope").as_deref(), Some("everything"));
        // offline_access asks for consent (a refresh token).
        let (authorize, seen) = self::seen();
        let mut r = provider(
            ProviderMode::Interactive,
            Some(1),
            Some(McpOAuthConfig { scope: Some("a offline_access".into()), ..Default::default() }),
            None,
        );
        r.opts.on_authorize = Some(authorize);
        r.redirect_to_authorization(Url::parse("https://as.test/authorize").unwrap()).unwrap();
        assert_eq!(param(&seen.lock().unwrap()[0], "prompt").as_deref(), Some("consent"));
    }

    #[tokio::test]
    async fn tokens_rereads_the_store_and_save_tokens_records_when() {
        let store = new_store();
        let mut p = provider(ProviderMode::Passive, None, None, Some(store.clone()));
        assert!(p.tokens().is_none());
        store
            .update(KEY, |_| {
                let mut l = Login::new("x");
                l.tokens = Some(tokens(json!({"access_token": "a1", "token_type": "Bearer", "refresh_token": "r1"})));
                Ok(Some(l))
            })
            .await
            .unwrap();
        assert_eq!(p.tokens().unwrap().access_token, "a1");
        let before = now_ms();
        p.save_tokens(tokens(json!({"access_token": "a2", "token_type": "Bearer", "refresh_token": "r2", "expires_in": 60})))
            .await
            .unwrap();
        let saved = store.get(KEY).unwrap();
        assert_eq!(saved.tokens.unwrap().access_token, "a2");
        assert!(saved.saved_at.unwrap() >= before);
    }

    async fn rotate(store: &McpTokenStore, refresh: &str) {
        let refresh = refresh.to_string();
        store
            .update(KEY, move |e| {
                let mut l = e.unwrap_or_else(|| Login::new("x"));
                l.tokens = Some(tokens(json!({"access_token": "a", "token_type": "Bearer", "refresh_token": refresh})));
                Ok(Some(l))
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn invalidating_tokens_keeps_a_refresh_token_another_process_saved_meanwhile() {
        let store = new_store();
        rotate(&store, "r1").await;
        let mut p = provider(ProviderMode::Passive, None, None, Some(store.clone()));
        assert_eq!(p.tokens().unwrap().refresh_token.as_deref(), Some("r1"));
        // Another Marv refreshed (rotating r1 → r2) after this one read r1.
        rotate(&store, "r2").await;
        p.invalidate_credentials(Invalidate::Tokens).await.unwrap();
        assert_eq!(p.tokens().unwrap().refresh_token.as_deref(), Some("r2"));
        // Now r2 is the one that failed: it goes.
        p.invalidate_credentials(Invalidate::Tokens).await.unwrap();
        assert!(p.tokens().is_none());
        assert_eq!(store.get(KEY).unwrap().server, "x");
    }

    #[tokio::test]
    async fn the_stale_one_of_two_refreshes_keeps_the_newer_tokens() {
        let store = new_store();
        rotate(&store, "r1").await;
        let mut p = provider(ProviderMode::Passive, None, None, Some(store.clone()));
        p.tokens(); // both attempts read r1
        p.save_tokens(tokens(json!({"access_token": "a2", "token_type": "Bearer", "refresh_token": "r2"}))).await.unwrap();
        p.tokens(); // a request's header read r2 meanwhile
        // The second attempt sent r1, which the server refused.
        p.note_refused_refresh("r1".into());
        p.invalidate_credentials(Invalidate::Tokens).await.unwrap();
        assert_eq!(store.get(KEY).unwrap().tokens.unwrap().refresh_token.as_deref(), Some("r2"));
    }

    #[tokio::test]
    async fn the_verifier_stays_in_memory_and_discovery_is_stored() {
        let store = new_store();
        let mut p = provider(ProviderMode::Interactive, Some(1), None, Some(store.clone()));
        assert!(p.code_verifier().is_err());
        p.save_code_verifier("v".into());
        assert_eq!(p.code_verifier().unwrap(), "v");
        let discovery = StoredDiscovery { authorization_server_url: "https://as.test/".into(), rest: Default::default() };
        p.save_discovery_state(discovery.clone()).await.unwrap();
        assert_eq!(provider(ProviderMode::Passive, None, None, Some(store.clone())).discovery_state(), Some(discovery));
        assert!(!serde_json::to_string(&store.get(KEY)).unwrap().contains("\"v\""));
    }

    #[test]
    fn state_is_random_and_stable_for_one_login() {
        let p = provider(ProviderMode::Interactive, Some(1), None, None);
        assert_eq!(p.state(), p.state());
        assert_ne!(p.state(), provider(ProviderMode::Interactive, Some(1), None, None).state());
        assert!(p.state().len() >= 40);
        let (verifier, challenge) = pkce();
        assert!(verifier.len() >= 43 && challenge.len() == 43 && verifier != challenge);
    }

    #[tokio::test]
    async fn a_cancelled_login_saves_no_discovery_tokens_or_client() {
        let store = new_store();
        let abort = Abort::new();
        let mut p = provider(ProviderMode::Interactive, Some(1), None, Some(store.clone()));
        p.opts.abort = Some(abort.clone());
        abort.abort("cancelled: /mcp logout");
        let d = StoredDiscovery { authorization_server_url: "https://auth.example/".into(), rest: Default::default() };
        assert_eq!(p.save_discovery_state(d).await.unwrap_err().to_string(), "cancelled: /mcp logout");
        assert!(p.save_tokens(tokens(json!({"access_token": "a"}))).await.is_err());
        assert!(
            p.save_client_information(StoredClient { client_id: "c".into(), port: None, rest: Default::default() })
                .await
                .is_err()
        );
        assert!(store.get(KEY).is_none());
    }

    #[test]
    fn the_resource_metadata_a_401_names() {
        let h = r#"Bearer error="invalid_token", resource_metadata="https://x.test/.well-known/oauth-protected-resource/mcp""#;
        assert_eq!(resource_metadata_of(h).unwrap().as_str(), "https://x.test/.well-known/oauth-protected-resource/mcp");
        assert!(resource_metadata_of("Bearer").is_none());
    }

    #[tokio::test]
    async fn the_outcome_is_the_first_valid_code_or_why_not() {
        let at = |q: &str| Url::parse(&format!("http://127.0.0.1:1/callback?{q}")).unwrap();
        let mut cb = CallbackExpectation::new("s3cret");
        let outcome = cb.outcome();
        assert_eq!(cb.check(&at("code=c&state=nope")), CallbackCheck::Refused { why: "state".into() });
        assert_eq!(cb.check(&at("code=c")), CallbackCheck::Refused { why: "state".into() });
        assert!(outcome.borrow().is_none());
        assert_eq!(cb.check(&at("code=c&state=s3cret")), CallbackCheck::Ok { code: "c".into() });
        assert_eq!(*outcome.borrow(), Some(Ok("c".into())));
        let mut refused = CallbackExpectation::new("s3cret");
        assert_eq!(
            refused.check(&at("error=access_denied&state=s3cret")),
            CallbackCheck::Refused { why: "access_denied".into() }
        );
        assert!(refused.outcome().borrow().clone().unwrap().unwrap_err().contains("access_denied"));
        let mut cancelled = CallbackExpectation::new("s");
        cancelled.cancel("cancelled: /mcp remove");
        assert_eq!(cancelled.check(&at("code=c&state=s")), CallbackCheck::Refused { why: "used".into() });
        assert_eq!(*cancelled.outcome().borrow(), Some(Err("cancelled: /mcp remove".into())));
    }

    #[tokio::test]
    async fn the_listener_answers_the_browser_and_hands_over_the_code() {
        let exp = Arc::new(Mutex::new(CallbackExpectation::new("st")));
        let outcome = exp.lock().unwrap().outcome();
        let e = exp.clone();
        let listener = listen(Arc::new(move |u| e.lock().unwrap().check(u)), None).await.unwrap();
        assert!(listener.port > 0 && !listener.fell_back);
        let get = |path: &str| reqwest::get(format!("http://127.0.0.1:{}{path}", listener.port));
        assert_eq!(get("/elsewhere").await.unwrap().status(), 404);
        assert_eq!(get("/callback?code=x&state=other").await.unwrap().status(), 400);
        let ok = get("/callback?code=good&state=st").await.unwrap();
        assert_eq!(ok.status(), 200);
        assert!(ok.text().await.unwrap().contains("You can close this tab"));
        assert_eq!(*outcome.borrow(), Some(Ok("good".into())));
        listener.close();
    }

    #[tokio::test]
    async fn a_taken_preferred_port_falls_back_and_says_so() {
        let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = busy.local_addr().unwrap().port();
        let listener = listen(Arc::new(|_| CallbackCheck::Refused { why: "state".into() }), Some(port)).await.unwrap();
        assert_ne!(listener.port, port);
        assert!(listener.fell_back);
    }

    #[test]
    fn open_browser_passes_the_url_as_one_argument_no_shell() {
        let spawned = Mutex::new(vec![]);
        let spawn = |argv: &[String]| spawned.lock().unwrap().push(argv.to_vec());
        open_browser_with(&Url::parse("https://as.test/a?b=1&c=$(rm -rf ~)").unwrap(), "linux", &spawn);
        open_browser_with(&Url::parse("https://as.test/").unwrap(), "macos", &spawn);
        assert_eq!(
            spawned.into_inner().unwrap(),
            vec![
                vec!["xdg-open".to_string(), "https://as.test/a?b=1&c=$(rm%20-rf%20~)".to_string()],
                vec!["open".to_string(), "https://as.test/".to_string()],
            ]
        );
    }
}
