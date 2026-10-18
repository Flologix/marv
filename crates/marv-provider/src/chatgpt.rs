//! Sign in with ChatGPT (marv-rs #159, not in Marv): a person's ChatGPT plan pays for Marv's requests, through
//! OpenAI's program for open-source agents (developers.openai.com/siwc, "token sharing for open-source agents").
//!
//! - **Registration is dynamic.** The first sign-in names `client_id=dynamic_agent_client`, Marv's name
//!   (`agent_name_hint`) and this machine's host id (`ext_agent_host_id`, a `urn:uuid:` made once and kept); the
//!   callback carries the client id OpenAI issued for this person, which every later request uses. No other app's
//!   client id is ever sent (the Codex CLI's included).
//! - **OAuth 2 with PKCE (S256)**, in the person's browser, back to a loopback listener ([`listen`]):
//!   `http://127.0.0.1:<port>/auth/callback` (127.0.0.1, never `localhost`: RFC 8252 §8.3 and OpenAI's rule), bound to
//!   127.0.0.1 only, for one sign-in: a callback counts only with that sign-in's `state` (compared in constant time),
//!   the first one that has it ends the listener, and it gives up after [`LOGIN_TIMEOUT`]. A sandboxed command shares
//!   the loopback network: without the state it can't feed Marv a code of its own.
//! - **The ID token is verified** as the docs require: RS256 against OpenAI's JWKS, issuer, audience (the issued
//!   client id), expiry and the sign-in's nonce; its `sub` is the account. The grant must include
//!   `chatgpt.tokens.use.direct` ([`PLAN_SCOPE`]), or the plan isn't usable and nothing is saved.
//! - **Tokens** live in `<config dir>/chatgpt.json` ([`AUTH_FILE`]), 0600 in a 0700 folder, written atomically. The
//!   config folder is hidden from every sandboxed command, and the tokens never go in an environment, a log, the
//!   transcript or a trajectory: [`StoredAuth`]'s `Debug` leaves them out.
//! - **Refresh** when the access token has under [`REFRESH_MARGIN`] left, or after a 401, under the file's lock
//!   (`marv_fs::with_file_lock`): the refresh token rotates on every use, so two Marv processes must not race it. A
//!   refused refresh token (`invalid_grant` and kin) clears the tokens: the person signs in again.
//! - **Sign-out** revokes the refresh token and clears the tokens, keeping the issued client id and host id (the next
//!   sign-in reuses them, as the docs ask).
//! - Requests go to the public Responses API ([`API_BASE_URL`], never ChatGPT's own backend) through
//!   [`crate::responses`], with `store: false`, `stream: true` and none of the fields the route refuses
//!   (`max_output_tokens`, sampling). Models are the account's: [`list_models`].

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::models::ModelInfo;
use crate::responses::Credentials;

/// OpenAI's authorization server: the ID token's issuer.
pub const ISSUER: &str = "https://auth.openai.com";
/// Where requests go: the public API, never ChatGPT's backend.
pub const API_BASE_URL: &str = "https://api.openai.com/v1";
/// The resource every authorize, exchange and refresh names.
pub const RESOURCE: &str = "https://api.openai.com/v1";
/// Identity scopes plus the plan-usage ones.
pub const SCOPES: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
/// Without this in the grant, the plan can't be used.
pub const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
/// The client id of a first sign-in: OpenAI issues this person's own in the callback.
pub const DYNAMIC_CLIENT: &str = "dynamic_agent_client";
/// Shown to the person on OpenAI's consent page (`agent_name_hint`).
pub const AGENT_NAME: &str = "Marv";
/// The callback's path; only the port may change between sign-ins.
pub const CALLBACK_PATH: &str = "/auth/callback";
/// The port tried first (OpenAI's example); any free one otherwise.
pub const PREFERRED_PORT: u16 = 1455;
/// A sign-in nobody finishes stops listening after this long.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
/// Refresh when the access token has less than this left.
pub const REFRESH_MARGIN: Duration = Duration::from_secs(300);
/// The tokens' file in Marv's config folder.
pub const AUTH_FILE: &str = "chatgpt.json";
/// A refresh refused with one of these means the sign-in is over.
const SIGNED_OUT_ERRORS: &[&str] = &[
    "invalid_grant",
    "invalid_refresh_token",
    "token_expired",
    "refresh_token_expired",
    "refresh_token_invalidated",
    "refresh_token_reused",
];

/// The context windows of ChatGPT models Marv knows, by slug prefix (the longest match wins). The account's model
/// list doesn't give one, and without it compaction would wait for `compactTokens` alone. GPT-5's family takes 272K
/// input tokens on the API. Update as models come.
pub const CONTEXT_WINDOWS: &[(&str, u64)] = &[("gpt-5", 272_000)];

/// The window assumed for a ChatGPT model not in [`CONTEXT_WINDOWS`]: small enough to be safe for any current one.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 128_000;

/// The context window to assume for `slug`.
pub fn context_window(slug: &str) -> u64 {
    CONTEXT_WINDOWS
        .iter()
        .filter(|(prefix, _)| slug.starts_with(prefix))
        .max_by_key(|(prefix, _)| prefix.len())
        .map_or(DEFAULT_CONTEXT_WINDOW, |&(_, window)| window)
}

/// What to say when there's no usable sign-in.
pub const SIGN_IN_AGAIN: &str = "Sign in with ChatGPT again: run /setup.";

/// Where the authorization server and the API are (tests point both at a local server).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    pub issuer: String,
    pub api: String,
    /// The first wait before a revocation is tried again (doubled each time).
    pub backoff: Duration,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self { issuer: ISSUER.into(), api: API_BASE_URL.into(), backoff: Duration::from_millis(500) }
    }
}

/// How many times a revocation is tried (network failures and 5xx).
pub const REVOKE_ATTEMPTS: u32 = 3;

impl Endpoints {
    fn authorize(&self) -> String {
        format!("{}/api/accounts/authorize", self.issuer)
    }
    fn token(&self) -> String {
        format!("{}/api/accounts/oauth/token", self.issuer)
    }
    fn revoke(&self) -> String {
        format!("{}/api/accounts/oauth/revoke", self.issuer)
    }
    fn jwks(&self) -> String {
        format!("{}/.well-known/jwks.json", self.issuer)
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn random_bytes<const N: usize>() -> [u8; N] {
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; N];
    ring::rand::SystemRandom::new().fill(&mut bytes).expect("the OS random number generator");
    bytes
}

fn base64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// A fresh random value for `state` and `nonce`: 32 bytes, base64url.
pub fn random_token() -> String {
    base64url(&random_bytes::<32>())
}

/// A PKCE pair (RFC 7636, S256): the verifier (kept in memory) and its challenge (sent).
pub fn pkce() -> (String, String) {
    let verifier = random_token();
    let challenge = base64url(ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes()).as_ref());
    (verifier, challenge)
}

/// A version 4 UUID as a `urn:uuid:`, this machine's host id.
pub fn new_host_id() -> String {
    let mut b = random_bytes::<16>();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("urn:uuid:{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

/// Constant-time comparison.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// One registration in `chatgpt.json`: an issued client id, the account it was issued for, and that account's
/// tokens (none while signed out).
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredAuth {
    /// This machine's `ext_agent_host_id` (the file's; filled in on load).
    #[serde(default, skip_serializing)]
    pub host_id: String,
    /// The client id OpenAI issued at this registration (kept across sign-outs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// When the access token expires, ms since the epoch.
    #[serde(default)]
    pub expires_at: u64,
    /// The token response's `earliest_refresh_at`, ms since the epoch: no routine refresh before it.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub refresh_after: u64,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub saved_at: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// Never the tokens: a `{:?}` in a log or an error mustn't carry them.
impl std::fmt::Debug for StoredAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredAuth")
            .field("client_id", &self.client_id)
            .field("email", &self.email)
            .field("signed_in", &self.signed_in())
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

impl StoredAuth {
    pub fn signed_in(&self) -> bool {
        self.access_token.is_some() && self.refresh_token.is_some()
    }

    /// Signed in, with plan usage granted.
    pub fn usable(&self) -> bool {
        self.signed_in() && self.scopes.iter().any(|s| s == PLAN_SCOPE)
    }

    /// What setup shows for it: the email, else a short form of the subject.
    pub fn label(&self) -> String {
        self.email.clone().unwrap_or_else(|| "your ChatGPT account".into())
    }

    /// Drops the tokens (the ID token too: no `id_token_hint` after a sign-out), keeping the client id and who it was.
    fn clear_tokens(&mut self) {
        (self.id_token, self.access_token, self.refresh_token) = (None, None, None);
        (self.expires_at, self.refresh_after) = (0, 0);
        self.scopes.clear();
    }
}

/// `chatgpt.json`: this machine's host id, every registration (one per issued client id, each bound to one account,
/// as OpenAI's docs ask: never two registrations' tokens mixed), and which one is in use.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthFile {
    #[serde(default)]
    pub host_id: String,
    /// The client id of the registration in use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    #[serde(default)]
    pub accounts: Vec<StoredAuth>,
}

impl AuthFile {
    fn account(&self, client_id: &str) -> Option<&StoredAuth> {
        self.accounts.iter().find(|a| a.client_id.as_deref() == Some(client_id))
    }

    fn active(&self) -> Option<StoredAuth> {
        let account = self.account(self.active.as_deref()?)?;
        Some(StoredAuth { host_id: self.host_id.clone(), ..account.clone() })
    }

    /// Adds or replaces `auth`'s registration (by client id) and makes it the one in use.
    fn put(&mut self, auth: StoredAuth) {
        let client_id = auth.client_id.clone();
        self.accounts.retain(|a| a.client_id != client_id);
        self.accounts.push(StoredAuth { host_id: String::new(), saved_at: now_ms(), ..auth });
        self.active = client_id;
    }
}

/// `<config dir>/chatgpt.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthStore {
    pub path: PathBuf,
}

impl AuthStore {
    pub fn new(config_dir: &Path) -> Self {
        Self { path: config_dir.join(AUTH_FILE) }
    }

    /// The whole file; empty when there's none (or it isn't readable as Marv wrote it).
    pub fn file(&self) -> AuthFile {
        std::fs::read_to_string(&self.path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    /// The registration in use, if any.
    pub fn load(&self) -> Option<StoredAuth> {
        self.file().active()
    }

    /// Every registration, the one in use first.
    pub fn accounts(&self) -> Vec<StoredAuth> {
        let file = self.file();
        let mut accounts = file.accounts.clone();
        accounts.sort_by_key(|a| a.client_id != file.active);
        accounts.into_iter().map(|a| StoredAuth { host_id: file.host_id.clone(), ..a }).collect()
    }

    async fn write(&self, file: &AuthFile) -> Result<(), String> {
        let text = serde_json::to_string_pretty(file).map_err(|e| e.to_string())? + "\n";
        marv_fs::private_file::write_private(&self.path, &text)
            .await
            .map_err(|e| format!("couldn't save the sign-in to {} ({e})", self.path.display()))
    }

    /// Saves `auth` as its registration (by client id), in use from now on; privately (0600) and atomically. The
    /// host id is the file's (made now if there is none yet).
    pub async fn save(&self, auth: &StoredAuth) -> Result<(), String> {
        let mut file = self.file();
        if file.host_id.is_empty() {
            file.host_id = if auth.host_id.is_empty() { new_host_id() } else { auth.host_id.clone() };
        }
        file.put(auth.clone());
        self.write(&file).await
    }

    /// This machine's host id: the saved one, or a new one saved now (kept across sign-outs and registrations).
    pub async fn host_id(&self) -> Result<String, String> {
        let mut file = self.file();
        if file.host_id.is_empty() {
            file.host_id = new_host_id();
            self.write(&file).await?;
        }
        Ok(file.host_id)
    }

    /// Makes the registration `client_id` the one in use (another saved account).
    pub async fn activate(&self, client_id: &str) -> Result<StoredAuth, String> {
        let work = async {
            let mut file = self.file();
            let account = file.account(client_id).cloned().ok_or("That ChatGPT account isn't saved any more.")?;
            file.active = Some(client_id.to_string());
            self.write(&file).await?;
            Ok(account)
        };
        self.locked(work).await?
    }

    /// Forgets the registration `client_id` (OpenAI no longer knows it): the next sign-in registers again.
    async fn forget(&self, client_id: &str) -> Result<(), String> {
        let mut file = self.file();
        file.accounts.retain(|a| a.client_id.as_deref() != Some(client_id));
        if file.active.as_deref() == Some(client_id) {
            file.active = None;
        }
        self.write(&file).await
    }

    /// Runs `work` holding the file's lock (refresh and sign-in must not race another Marv's).
    async fn locked<T: Send, F: std::future::Future<Output = T> + Send>(&self, work: F) -> Result<T, String> {
        marv_fs::with_file_lock(&self.path, marv_fs::LockOptions::default(), || work).await.map_err(|e| e.to_string())
    }

    /// Whether the registration in use is signed in with plan usage (the status line, `/setup`).
    pub fn signed_in(&self) -> bool {
        self.load().is_some_and(|a| a.usable())
    }
}

// ---- The loopback listener ----

/// What one request to the listener came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Callback {
    /// The sign-in's callback: the code, the issued client id and the granted scopes, when the server sent them.
    Code { code: String, client_id: Option<String>, scope: Option<String> },
    /// The sign-in's callback, refusing (`error=`): only a short code is kept, never the server's text.
    Refused(String),
    /// Not this sign-in's callback (another path, no or another `state`): answered and ignored.
    Ignored(&'static str),
}

/// Decides about one request target (`/auth/callback?code=…&state=…`).
pub fn check_callback(target: &str, state: &str) -> Callback {
    let Ok(url) = Url::parse("http://127.0.0.1").and_then(|base| base.join(target)) else { return Callback::Ignored("path") };
    if url.path() != CALLBACK_PATH {
        return Callback::Ignored("path");
    }
    let param = |name: &str| url.query_pairs().find(|(k, _)| k == name).map(|(_, v)| v.into_owned());
    // Without this sign-in's state, nothing a sandboxed command (which shares the network) or another page sends counts.
    if !param("state").is_some_and(|s| same(s.as_bytes(), state.as_bytes())) {
        return Callback::Ignored("state");
    }
    if let Some(error) = param("error") {
        let ok =
            !error.is_empty() && error.len() <= 64 && error.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b));
        return Callback::Refused(if ok { error } else { "error".into() });
    }
    match param("code").filter(|c| !c.is_empty()) {
        Some(code) => Callback::Code { code, client_id: param("client_id").filter(|c| !c.is_empty()), scope: param("scope") },
        None => Callback::Refused("no_code".into()),
    }
}

/// What the browser is shown: (status, html). Nothing from the request is echoed.
fn callback_page(callback: &Callback) -> (u16, &'static str) {
    match callback {
        Callback::Code { .. } => (200, "Signed in. You can close this tab and go back to Marv."),
        Callback::Refused(_) => (400, "The sign-in was refused. Marv says why; you can close this tab."),
        Callback::Ignored("path") => (404, "Not found."),
        Callback::Ignored(_) => (400, "This isn't the sign-in Marv is waiting for."),
    }
}

/// A loopback listener for one sign-in.
pub struct Listener {
    pub port: u16,
    listener: tokio::net::TcpListener,
}

/// Listens on 127.0.0.1 only: [`PREFERRED_PORT`] first, else any free port.
pub async fn listen() -> std::io::Result<Listener> {
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", PREFERRED_PORT)).await {
        Ok(l) => l,
        Err(_) => tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?,
    };
    let port = listener.local_addr()?.port();
    Ok(Listener { port, listener })
}

impl Listener {
    /// The address the browser is sent back to.
    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}{CALLBACK_PATH}", self.port)
    }

    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }

    /// Answers requests until one carries `state` (then the listener closes: it takes one callback), `timeout`
    /// passes, or `cancel` fires. Each request gets 10 s and 16 KB.
    pub async fn wait(self, state: &str, timeout: Duration, cancel: &CancellationToken) -> Result<Callback, String> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let accepted = tokio::select! {
                _ = cancel.cancelled() => return Err("the sign-in was cancelled".into()),
                _ = tokio::time::sleep_until(deadline) => {
                    return Err(format!("the sign-in timed out (nothing came back within {} minutes)", timeout.as_secs() / 60));
                }
                a = self.listener.accept() => a,
            };
            let Ok((stream, _)) = accepted else { continue };
            match tokio::time::timeout(Duration::from_secs(10), answer(stream, state)).await {
                Ok(Ok(Callback::Ignored(_))) | Ok(Err(_)) | Err(_) => continue,
                Ok(Ok(callback)) => return Ok(callback),
            }
        }
    }
}

/// One browser request: its target is checked, the page says what happened, nothing else is read.
async fn answer(mut stream: tokio::net::TcpStream, state: &str) -> std::io::Result<Callback> {
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
    let callback = match line.split(' ').collect::<Vec<_>>()[..] {
        ["GET", target, ..] => check_callback(target, state),
        _ => Callback::Ignored("path"),
    };
    let (status, text) = callback_page(&callback);
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Bad Request",
    };
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>Marv</title><body style=\"font-family:sans-serif;margin:3em\"><p>{text}</p>"
    );
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\ncache-control: no-store\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(callback)
}

// ---- The sign-in ----

/// A sign-in under way: open [`SignIn::url`] in the browser, then [`SignIn::finish`].
pub struct SignIn {
    url: String,
    /// The same address without the `id_token_hint` (what's shown: the hint is a token).
    shown_url: String,
    listener: Listener,
    store: AuthStore,
    endpoints: Endpoints,
    /// The registration being signed in again; `None` for a new registration.
    previous: Option<StoredAuth>,
    host_id: String,
    state: String,
    nonce: String,
    verifier: String,
}

/// Which registration a sign-in is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignInTarget {
    /// The registration in use, or a new one when there's none.
    Current,
    /// A saved registration, by its client id.
    Account(String),
    /// A new registration: another ChatGPT account (or the same one, registered again).
    New,
}

/// What to say when OpenAI no longer knows a registration (Marv has forgotten it).
pub const REGISTER_AGAIN: &str =
    "OpenAI no longer knows this Marv's registration, so Marv forgot it. Sign in again: it registers anew.";

/// Starts a sign-in: listens on the loopback and builds the authorize address. A new registration names
/// `dynamic_agent_client` and Marv; signing in again reuses the registration's issued client id and names the
/// account. Either way the host id is this machine's, kept across registrations.
pub async fn start_sign_in(store: AuthStore, endpoints: Endpoints, target: SignInTarget) -> Result<SignIn, String> {
    let listener = listen().await.map_err(|e| format!("couldn't listen on 127.0.0.1 for the sign-in ({e})"))?;
    let host_id = store.host_id().await?;
    let previous = match &target {
        SignInTarget::Current => store.load(),
        SignInTarget::Account(id) => Some(
            store
                .accounts()
                .into_iter()
                .find(|a| a.client_id.as_deref() == Some(id.as_str()))
                .ok_or("That ChatGPT account isn't saved any more.")?,
        ),
        SignInTarget::New => None,
    }
    .filter(|a| a.client_id.is_some());
    let (state, nonce) = (random_token(), random_token());
    let (verifier, challenge) = pkce();
    let mut url = Url::parse(&endpoints.authorize()).map_err(|e| e.to_string())?;
    let mut hint = None;
    {
        let mut q = url.query_pairs_mut();
        match &previous {
            Some(account) => {
                q.append_pair("client_id", account.client_id.as_deref().unwrap_or_default());
                hint = account.id_token.clone();
                if let Some(email) = &account.email {
                    q.append_pair("login_hint", email);
                }
            }
            None => {
                q.append_pair("client_id", DYNAMIC_CLIENT);
                q.append_pair("agent_name_hint", AGENT_NAME);
            }
        }
        q.append_pair("ext_agent_host_id", &host_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", &listener.redirect_uri())
            .append_pair("scope", SCOPES)
            .append_pair("resource", RESOURCE)
            .append_pair("state", &state)
            .append_pair("nonce", &nonce)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
    }
    let shown_url = url.to_string();
    if let Some(hint) = hint {
        url.query_pairs_mut().append_pair("id_token_hint", &hint);
    }
    Ok(SignIn { url: url.into(), shown_url, listener, store, endpoints, previous, host_id, state, nonce, verifier })
}

/// Who signed in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub email: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    #[serde(default)]
    id_token: Option<String>,
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    scopes: Option<Vec<String>>,
    /// No routine refresh before this (Unix seconds, or ms).
    #[serde(default)]
    earliest_refresh_at: Option<Value>,
}

impl TokenResponse {
    /// The granted scopes (the token response's, which decide plan usage).
    fn scopes(&self) -> Option<Vec<String>> {
        self.scopes.clone().or_else(|| self.scope.as_ref().map(|s| s.split_whitespace().map(str::to_string).collect()))
    }

    /// `earliest_refresh_at` in ms since the epoch (a number of seconds or ms; anything else is ignored).
    fn refresh_after(&self) -> u64 {
        match self.earliest_refresh_at.as_ref().and_then(Value::as_u64) {
            Some(n) if n > 100_000_000_000 => n,
            Some(n) => n * 1000,
            None => 0,
        }
    }
}

fn http() -> &'static reqwest::Client {
    static CLIENT: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default()
    });
    &CLIENT
}

/// A token request's failure: the OAuth error code only (never the body, which could echo a token).
async fn token_request(endpoints: &Endpoints, form: &[(&str, &str)]) -> Result<TokenResponse, String> {
    let response = http().post(endpoints.token()).form(form).send().await.map_err(|e| format!("couldn't reach OpenAI ({e})"))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        let code = serde_json::from_str::<Value>(&text).ok().and_then(|v| {
            let e = v.get("error")?;
            e.as_str().map(str::to_string).or_else(|| e.get("code")?.as_str().map(str::to_string))
        });
        return Err(code.filter(|c| c.len() <= 64).unwrap_or_else(|| format!("status {}", status.as_u16())));
    }
    serde_json::from_str(&text).map_err(|_| "OpenAI's token answer wasn't readable".to_string())
}

impl SignIn {
    /// The authorize address to open in the browser (it may carry the account's `id_token_hint`: never show or log
    /// it; [`SignIn::shown_url`] is for that).
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The authorize address without the `id_token_hint`, to show in case the browser didn't open.
    pub fn shown_url(&self) -> &str {
        &self.shown_url
    }

    pub fn port(&self) -> u16 {
        self.listener.port
    }

    /// Waits for the callback (at most [`LOGIN_TIMEOUT`]), exchanges the code, verifies the ID token and saves the
    /// tokens. Nothing is saved unless every check passes.
    pub async fn finish(self, cancel: CancellationToken) -> Result<Account, String> {
        self.finish_within(LOGIN_TIMEOUT, cancel).await
    }

    pub async fn finish_within(self, timeout: Duration, cancel: CancellationToken) -> Result<Account, String> {
        let Self { listener, store, endpoints, previous, host_id, state, nonce, verifier, .. } = self;
        let redirect_uri = listener.redirect_uri();
        let kept = previous.as_ref().and_then(|p| p.client_id.clone());
        let (code, issued) = match listener.wait(&state, timeout, &cancel).await? {
            Callback::Code { code, client_id, .. } => (code, client_id),
            // A registration OpenAI no longer knows (revoked, deleted): forget it, so the next sign-in registers.
            Callback::Refused(why) if kept.is_some() && matches!(why.as_str(), "invalid_client" | "unauthorized_client") => {
                store.forget(kept.as_deref().unwrap_or_default()).await?;
                return Err(REGISTER_AGAIN.into());
            }
            Callback::Refused(why) => return Err(format!("OpenAI refused the sign-in ({why})")),
            Callback::Ignored(_) => unreachable!("wait never returns an ignored request"),
        };
        let client_id = match (&kept, issued) {
            (None, Some(issued)) if issued != DYNAMIC_CLIENT => issued,
            (None, _) => return Err("OpenAI didn't issue Marv a client id: the sign-in is incomplete. Try again.".into()),
            (Some(kept), Some(issued)) if *kept != issued => {
                return Err("OpenAI answered for another client than this sign-in's: nothing was saved.".into());
            }
            (Some(kept), _) => kept.clone(),
        };
        let form = [
            ("grant_type", "authorization_code"),
            ("client_id", client_id.as_str()),
            ("code", code.as_str()),
            ("code_verifier", verifier.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("resource", RESOURCE),
        ];
        let tokens = match token_request(&endpoints, &form).await {
            Ok(tokens) => tokens,
            Err(code) if code == "invalid_client" && kept.is_some() => {
                store.forget(&client_id).await?;
                return Err(REGISTER_AGAIN.into());
            }
            Err(why) => return Err(format!("OpenAI didn't accept the sign-in ({why})")),
        };
        let id_token = tokens.id_token.clone().ok_or("OpenAI sent no ID token")?;
        let claims = verify_id_token(&id_token, &endpoints, &client_id, &nonce).await?;
        if previous.as_ref().and_then(|p| p.subject.as_ref()).is_some_and(|s| *s != claims.sub) {
            return Err("That's another ChatGPT account than this client was issued for: nothing was saved.".into());
        }
        // The token response's granted scopes decide plan usage (not the callback's).
        let scopes = tokens.scopes().unwrap_or_default();
        if !scopes.iter().any(|s| s == PLAN_SCOPE) {
            return Err("Your ChatGPT account didn't allow plan usage for Marv, so it can't be used. Nothing was saved.".into());
        }
        let refresh_after = tokens.refresh_after();
        let auth = StoredAuth {
            host_id,
            client_id: Some(client_id),
            subject: Some(claims.sub),
            email: claims.email.clone(),
            id_token: Some(id_token),
            access_token: Some(tokens.access_token),
            refresh_token: tokens.refresh_token,
            expires_at: now_ms() + tokens.expires_in.unwrap_or(3600) * 1000,
            refresh_after,
            scopes,
            saved_at: 0,
        };
        if auth.refresh_token.is_none() {
            return Err("OpenAI sent no refresh token: nothing was saved.".into());
        }
        let saving = store.save(&auth);
        store.locked(saving).await??;
        Ok(Account { email: claims.email })
    }
}

// ---- The ID token ----

#[derive(Debug, Deserialize)]
pub struct IdClaims {
    pub iss: String,
    pub sub: String,
    pub aud: Value,
    pub exp: u64,
    #[serde(default)]
    pub nonce: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
}

#[derive(Deserialize)]
struct JwtHeader {
    alg: String,
    #[serde(default)]
    kid: Option<String>,
}

#[derive(Deserialize)]
struct Jwks {
    keys: Vec<Jwk>,
}

#[derive(Deserialize)]
struct Jwk {
    #[serde(default)]
    kid: Option<String>,
    #[serde(default)]
    kty: String,
    #[serde(default)]
    n: String,
    #[serde(default)]
    e: String,
}

/// Checks an ID token against OpenAI's published keys and the sign-in: RS256 signature, issuer, audience (the
/// issued client id), expiry (a minute's skew) and nonce. Returns its claims.
pub async fn verify_id_token(token: &str, endpoints: &Endpoints, client_id: &str, nonce: &str) -> Result<IdClaims, String> {
    let jwks: Jwks = async { http().get(endpoints.jwks()).send().await?.error_for_status()?.json().await }
        .await
        .map_err(|e: reqwest::Error| format!("couldn't read OpenAI's signing keys ({e})"))?;
    let keys: Vec<(Option<String>, Vec<u8>, Vec<u8>)> =
        jwks.keys.into_iter().filter(|k| k.kty == "RSA").filter_map(|k| Some((k.kid, decode(&k.n)?, decode(&k.e)?))).collect();
    check_id_token(token, &keys, &endpoints.issuer, client_id, nonce, now_ms() / 1000)
}

fn decode(part: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(part.trim_end_matches('=')).ok()
}

/// [`verify_id_token`] with the keys given as (kid, modulus, exponent) and the time in seconds (pure: tests).
pub fn check_id_token(
    token: &str,
    keys: &[(Option<String>, Vec<u8>, Vec<u8>)],
    issuer: &str,
    client_id: &str,
    nonce: &str,
    now: u64,
) -> Result<IdClaims, String> {
    let bad = |why: &str| Err(format!("OpenAI's ID token didn't check out ({why}): nothing was saved."));
    let parts: Vec<&str> = token.split('.').collect();
    let [header, payload, signature] = parts[..] else { return bad("not a JWT") };
    let Some(header) = decode(header).and_then(|h| serde_json::from_slice::<JwtHeader>(&h).ok()) else {
        return bad("header");
    };
    if header.alg != "RS256" {
        return bad("algorithm");
    }
    let Some(signature) = decode(signature) else { return bad("signature") };
    let signed = format!("{}.{}", parts[0], parts[1]);
    let verified =
        keys.iter().filter(|(kid, ..)| header.kid.is_none() || kid.is_none() || *kid == header.kid).any(|(_, n, e)| {
            ring::signature::RsaPublicKeyComponents { n, e }
                .verify(&ring::signature::RSA_PKCS1_2048_8192_SHA256, signed.as_bytes(), &signature)
                .is_ok()
        });
    if !verified {
        return bad("signature");
    }
    let Some(claims) = decode(payload).and_then(|p| serde_json::from_slice::<IdClaims>(&p).ok()) else {
        return bad("claims");
    };
    if claims.iss != issuer {
        return bad("issuer");
    }
    let audience = match &claims.aud {
        Value::String(a) => a == client_id,
        Value::Array(list) => list.iter().any(|a| a.as_str() == Some(client_id)),
        _ => false,
    };
    if !audience {
        return bad("audience");
    }
    if claims.exp + 60 < now {
        return bad("expired");
    }
    if !claims.nonce.as_deref().is_some_and(|n| same(n.as_bytes(), nonce.as_bytes())) {
        return bad("nonce");
    }
    if claims.sub.is_empty() {
        return bad("subject");
    }
    Ok(claims)
}

// ---- Using the sign-in ----

/// The saved sign-in as a request's [`Credentials`]: the access token, refreshed when it's about to expire.
pub struct ChatGptCredentials {
    pub store: AuthStore,
    pub endpoints: Endpoints,
}

impl ChatGptCredentials {
    pub fn new(store: AuthStore) -> Arc<Self> {
        Arc::new(Self { store, endpoints: Endpoints::default() })
    }

    /// The access token now: the saved one while it has [`REFRESH_MARGIN`] left (unless it's `stale`, the one a
    /// server just refused), else a refreshed one, under the file's lock.
    async fn token(&self, stale: Option<&str>) -> Result<String, String> {
        let usable = |auth: &StoredAuth| {
            let now = now_ms();
            // Fresh enough, or not yet allowed a routine refresh (`earliest_refresh_at`) and still valid a while.
            let fresh = auth.expires_at > now + REFRESH_MARGIN.as_millis() as u64
                || (now < auth.refresh_after && auth.expires_at > now + 30_000);
            auth.access_token.clone().filter(|t| fresh && Some(t.as_str()) != stale)
        };
        let auth = self.store.load().filter(StoredAuth::signed_in).ok_or_else(|| format!("not signed in. {SIGN_IN_AGAIN}"))?;
        if !auth.scopes.iter().any(|s| s == PLAN_SCOPE) {
            return Err(format!("this sign-in doesn't allow plan usage. {SIGN_IN_AGAIN}"));
        }
        if let Some(token) = usable(&auth) {
            return Ok(token);
        }
        let work = async {
            // Another Marv may have refreshed while this one waited for the lock.
            let mut auth =
                self.store.load().filter(StoredAuth::signed_in).ok_or_else(|| format!("not signed in. {SIGN_IN_AGAIN}"))?;
            if let Some(token) = usable(&auth) {
                return Ok(token);
            }
            let (Some(client_id), Some(refresh)) = (auth.client_id.clone(), auth.refresh_token.clone()) else {
                return Err(format!("not signed in. {SIGN_IN_AGAIN}"));
            };
            let form = [
                ("grant_type", "refresh_token"),
                ("client_id", client_id.as_str()),
                ("refresh_token", refresh.as_str()),
                ("resource", RESOURCE),
            ];
            match token_request(&self.endpoints, &form).await {
                Ok(tokens) => {
                    // Everything together: the access token, its expiry, the scopes and the rotated refresh token.
                    auth.refresh_after = tokens.refresh_after();
                    auth.scopes = tokens.scopes().unwrap_or(auth.scopes);
                    auth.expires_at = now_ms() + tokens.expires_in.unwrap_or(3600) * 1000;
                    auth.refresh_token = tokens.refresh_token.or(auth.refresh_token);
                    auth.id_token = tokens.id_token.or(auth.id_token);
                    auth.access_token = Some(tokens.access_token.clone());
                    self.store.save(&auth).await?;
                    Ok(tokens.access_token)
                }
                // A registration OpenAI no longer knows: forgotten, so the next sign-in registers anew.
                Err(code) if code == "invalid_client" => {
                    self.store.forget(&client_id).await?;
                    Err(REGISTER_AGAIN.into())
                }
                Err(code) if SIGNED_OUT_ERRORS.contains(&code.as_str()) => {
                    auth.clear_tokens();
                    let _ = self.store.save(&auth).await;
                    Err(format!("your ChatGPT sign-in has ended ({code}). {SIGN_IN_AGAIN}"))
                }
                Err(why) => Err(format!("couldn't refresh the ChatGPT sign-in ({why}). Try again in a moment.")),
            }
        };
        self.store.locked(work).await?
    }
}

#[async_trait]
impl Credentials for ChatGptCredentials {
    async fn bearer(&self) -> Result<String, String> {
        self.token(None).await
    }

    async fn refused(&self, token: &str) -> Result<String, String> {
        self.token(Some(token)).await
    }
}

/// Signs out: revokes the refresh token and clears the tokens (keeping the client id and host id). `Ok(true)` when
/// OpenAI confirmed the revocation; `Ok(false)` when the tokens were cleared here only (the person can disconnect
/// Marv in ChatGPT's settings).
pub async fn sign_out(store: &AuthStore, endpoints: &Endpoints) -> Result<bool, String> {
    let work = async {
        let Some(mut auth) = store.load() else { return Ok(true) };
        let mut confirmed = true;
        if let (Some(refresh), Some(client_id)) = (&auth.refresh_token, &auth.client_id) {
            let form = [("token", refresh.as_str()), ("token_type_hint", "refresh_token"), ("client_id", client_id.as_str())];
            confirmed = false;
            // A network failure or a 5xx is tried again with backoff while the refresh token is still here.
            for attempt in 0..REVOKE_ATTEMPTS {
                if attempt > 0 {
                    tokio::time::sleep(endpoints.backoff * (1 << (attempt - 1))).await;
                }
                match http().post(endpoints.revoke()).form(&form).send().await {
                    Ok(r) if r.status().is_success() => {
                        confirmed = true;
                        break;
                    }
                    Ok(r) if !r.status().is_server_error() => break,
                    _ => {}
                }
            }
        }
        auth.clear_tokens();
        store.save(&auth).await?;
        Ok(confirmed)
    };
    store.locked(work).await?
}

/// The models this account may use ([`crate::models::list_models`] with the sign-in's token).
pub async fn list_models(credentials: &ChatGptCredentials) -> Result<Vec<ModelInfo>, String> {
    let token = credentials.bearer().await?;
    crate::models::list_models(crate::ProviderKind::ChatGpt, &credentials.endpoints.api, Some(&token)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_callback_counts_only_with_the_state_on_the_callback_path() {
        let ok =
            check_callback("/auth/callback?code=c1&state=S&client_id=oaiapp_1&scope=openid%20chatgpt.tokens.use.direct", "S");
        assert_eq!(
            ok,
            Callback::Code {
                code: "c1".into(),
                client_id: Some("oaiapp_1".into()),
                scope: Some("openid chatgpt.tokens.use.direct".into())
            }
        );
        assert_eq!(check_callback("/auth/callback?code=c1&state=T", "S"), Callback::Ignored("state"));
        assert_eq!(check_callback("/auth/callback?code=c1", "S"), Callback::Ignored("state"));
        assert_eq!(check_callback("/callback?code=c1&state=S", "S"), Callback::Ignored("path"));
        assert_eq!(check_callback("/auth/callback?error=access_denied&state=S", "S"), Callback::Refused("access_denied".into()));
        // The server's text is never shown, only a short code.
        assert_eq!(check_callback("/auth/callback?error=%3Cscript%3E&state=S", "S"), Callback::Refused("error".into()));
        assert_eq!(check_callback("/auth/callback?state=S", "S"), Callback::Refused("no_code".into()));
    }

    #[test]
    fn pkce_host_ids_and_tokens_are_fresh_and_well_formed() {
        let (verifier, challenge) = pkce();
        assert_eq!(challenge, base64url(ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes()).as_ref()));
        assert_ne!(pkce().0, verifier);
        let id = new_host_id();
        assert!(id.starts_with("urn:uuid:") && id.len() == 45 && id.as_bytes()[23] == b'4', "{id}");
        assert_eq!(random_token().len(), 43);
    }

    #[test]
    fn debug_never_shows_a_token() {
        let auth = StoredAuth {
            access_token: Some("at-secret".into()),
            refresh_token: Some("rt-secret".into()),
            id_token: Some("id-secret".into()),
            ..StoredAuth::default()
        };
        let shown = format!("{auth:?}");
        assert!(!shown.contains("secret"), "{shown}");
        assert!(shown.contains("signed_in: true"));
    }
}
