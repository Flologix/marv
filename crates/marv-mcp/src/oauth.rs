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

use std::time::Duration;

use serde_json::Value;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::config::McpOAuthConfig;
use crate::oauth_store::{Login, McpTokenStore};

pub const REPOSITORY: &str = "https://github.com/FlologixAI/marv";
pub const LOOPBACK: [&str; 3] = ["127.0.0.1", "localhost", "[::1]"];
/// `probe_login` and `revoke` give up after this long.
pub const NETWORK_TIMEOUT: Duration = Duration::from_secs(5);

/// A server needs the user to log in (again). Its message is Marv's own
/// sentence, never a server's or an OAuth error body: it's what the model
/// reads when a tool call fails.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{server} needs a new login: the user can run /mcp login {server}")]
pub struct LoginNeeded {
    pub server: String,
}

/// A login's `state`: 32 random bytes, base64url.
pub fn new_state() -> String {
    todo!("port: newState (32 bytes from the OS RNG, base64url without padding)")
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
    pub on_authorize: Option<Box<dyn Fn(Url) + Send + Sync>>,
    /// Interactive: the login's cancellation. Once cancelled, nothing is saved.
    pub cancel: Option<CancellationToken>,
}

/// Marv's side of an OAuth client: backed by the token store.
pub struct McpOAuthProvider {
    pub opts: ProviderOptions,
    /// PKCE verifier: memory only.
    verifier: Option<String>,
    /// The refresh token `tokens()` last handed out.
    last_refresh: Option<String>,
    /// The refresh token the last refused refresh request sent: the one to drop.
    failed_refresh: Option<String>,
    /// Interactive: the tokens this login got (a login starts afresh).
    fresh: Option<Value>,
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

    /// Always defined (see the module docs).
    pub fn redirect_url(&self) -> String {
        redirect_url_for(self.port())
    }

    /// RFC 7591 client metadata: a public client (`token_endpoint_auth_method:
    /// none`, PKCE stands in for a secret) unless the config gives a secret.
    pub fn client_metadata(&self) -> Value {
        let oauth = self.opts.oauth.clone().unwrap_or_default();
        let mut meta = serde_json::json!({
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

    pub fn state(&self) -> &str {
        &self.state
    }

    /// The configured client, or the one registered for Marv (from the store).
    pub async fn client_information(&self) -> crate::Result<Option<Value>> {
        let _ = (&self.verifier, &self.last_refresh, &self.failed_refresh, &self.fresh);
        todo!("port: clientInformation (config clientId/secret first, else store.client)")
    }

    /// Passive: raises [`LoginNeeded`] (never registers in the background).
    pub async fn save_client_information(&mut self, _info: Value) -> crate::Result<()> {
        todo!("port: saveClientInformation (passive → LoginNeeded; interactive → store with port, unless cancelled)")
    }

    /// Re-reads the store; interactive returns only this login's own tokens.
    pub async fn tokens(&mut self) -> crate::Result<Option<Value>> {
        todo!("port: tokens (re-read store, remember refresh token handed out)")
    }

    pub async fn save_tokens(&mut self, _tokens: Value) -> crate::Result<()> {
        todo!("port: saveTokens (skip when cancelled; store with savedAt)")
    }

    /// Passive: raises [`LoginNeeded`]. Interactive: rewrites `scope` from the
    /// config, checks [`authorization_url_ok`], hands the URL to `on_authorize`.
    pub fn redirect_to_authorization(&self, _url: Url) -> crate::Result<()> {
        todo!("port: redirectToAuthorization")
    }

    pub fn save_code_verifier(&mut self, verifier: String) {
        self.verifier = Some(verifier);
    }

    pub fn code_verifier(&self) -> crate::Result<&str> {
        self.verifier.as_deref().ok_or_else(|| crate::McpError::Server("no PKCE verifier: the login didn't start here".into()))
    }

    /// Cached in the store, so a startup with tokens makes no discovery requests.
    pub async fn discovery_state(&self) -> crate::Result<Option<Value>> {
        todo!("port: discoveryState")
    }

    pub async fn save_discovery_state(&self, _state: Value) -> crate::Result<()> {
        todo!("port: saveDiscoveryState")
    }

    /// `Tokens` drops only the refresh token that failed (see module docs).
    pub async fn invalidate_credentials(&mut self, _scope: Invalidate) -> crate::Result<()> {
        todo!("port: invalidateCredentials")
    }

    /// The HTTP client hook: notes the refresh token a refused refresh sent.
    pub fn note_refused_refresh(&mut self, refresh_token: String) {
        self.failed_refresh = Some(refresh_token);
    }
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

/// What a login waits for: one callback carrying its `state`.
#[derive(Debug)]
pub struct CallbackExpectation {
    state: String,
    used: bool,
    /// Set by an `error=` callback or a code-less one: the login fails with it.
    pub failure: Option<String>,
}

impl CallbackExpectation {
    pub fn new(state: impl Into<String>) -> Self {
        Self { state: state.into(), used: false, failure: None }
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
            self.failure = Some(format!("the authorization server refused the login ({why})"));
            return CallbackCheck::Refused { why };
        }
        match param("code").filter(|c| !c.is_empty()) {
            Some(code) => CallbackCheck::Ok { code },
            None => {
                self.failure = Some("the authorization server came back without a code".into());
                refused("no code")
            }
        }
    }

    /// The login was cancelled: later callbacks are `used`.
    pub fn cancel(&mut self) {
        self.used = true;
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

/// The loopback listener: 127.0.0.1 only, `/callback` only.
pub struct Listener {
    pub port: u16,
    /// The preferred port was taken: the client has to be registered again.
    pub fell_back: bool,
}

/// Listens on 127.0.0.1, `preferred` first, else any free port.
pub async fn listen(_preferred: Option<u16>) -> crate::Result<Listener> {
    todo!("port: listen (tokio TcpListener on 127.0.0.1, minimal HTTP GET handling, callback_page)")
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

/// Opens the URL with `xdg-open`/`open`: argv only, never a shell. Failures are ignored.
pub fn open_browser(_url: &Url, _platform: &str) {
    todo!("port: openBrowser (spawn detached, stdio null)")
}

/// Whether a remote server would ask for a login: an unauthenticated RFC 9728
/// metadata read. Never registers, never fails, gives up after 5 s.
pub async fn probe_login(_url: &str, _cancel: CancellationToken) -> bool {
    todo!("port: probeLogin")
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
pub async fn revoke(_login: &Login, _oauth: Option<&McpOAuthConfig>) -> Revoked {
    todo!("port: revoke")
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
}
