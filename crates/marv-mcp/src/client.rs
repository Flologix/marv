//! One MCP connection, client side: `initialize`, `tools/list` and `tools/call`
//! over stdio or Streamable HTTP. Not a port of one TS file: Marv used the MCP
//! SDK's `Client`, `StdioClientTransport` and `StreamableHTTPClientTransport`;
//! this is the slice of them Marv relied on.
//!
//! - **Stdio**: the server is a child process speaking one JSON message per
//!   line. It gets exactly the environment it's given (the manager passes
//!   `server_env`: no API keys), its stderr is piped and kept (the last
//!   [`crate::manager::STDERR_LINES`] lines), never inherited: it would scribble
//!   over the TUI. Closing ends its stdin, waits 2 s, then kills it.
//! - **Streamable HTTP**: each message is a POST; the answer is JSON or an SSE
//!   stream (progress notifications, then the response). The session id and
//!   the negotiated protocol version are sent back on later requests. No
//!   standalone GET stream: Marv reacts to no server-initiated message.
//! - **OAuth** (HTTP only): with a provider, each request carries its current
//!   access token; a 401 runs [`oauth::auth`] (refresh, or the login redirect)
//!   and retries once. A passive provider turns "needs a browser" into
//!   [`crate::oauth::LoginNeeded`]; anything else unfixable is
//!   [`McpError::Unauthorized`].
//! - **Timeouts**: a request gives up after its timeout; a progress
//!   notification restarts it, so a long tool that keeps saying how far it got
//!   isn't cut off (the SDK's `resetTimeoutOnProgress`). A request that's given
//!   up on (timeout, Esc) is cancelled on the server
//!   (`notifications/cancelled`).
//! - A server's `instructions` are dropped here, as soon as `initialize`
//!   answers (docs/SECURITY.md, "MCP").

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::McpError;
use crate::jsonrpc::{
    CallToolParams, Implementation, InitializeParams, InitializeResult, ListToolsResult, Message, Notification, PROTOCOL_VERSION,
    Request, RequestId, Response, RpcError, code, encode_line, method,
};
use crate::oauth::{self, AuthResult, McpOAuthProvider};
use crate::tool::McpCallResult;

/// The SDK's request-timeout error, as the model saw it from Marv.
pub const REQUEST_TIMED_OUT: &str = "MCP error -32001: Request timed out";
/// How long a closing server gets to exit after its stdin ends.
const EXIT_GRACE: Duration = Duration::from_secs(2);

/// What a stdio server's stderr keeps: its last lines, for `/mcp`.
pub type Stderr = Arc<Mutex<VecDeque<String>>>;

/// How to start a local server.
pub struct StdioOptions {
    pub command: String,
    pub args: Vec<String>,
    /// Its whole environment (nothing else is inherited).
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
    pub stderr: Stderr,
    /// Lines of stderr kept.
    pub stderr_lines: usize,
}

/// How to reach a remote server.
pub struct HttpOptions {
    pub url: Url,
    pub headers: BTreeMap<String, String>,
    /// Its login, if it has one (see [`crate::oauth`]).
    pub auth: Option<McpOAuthProvider>,
}

/// One request's limits.
#[derive(Clone, Default)]
pub struct RequestOptions {
    pub timeout: Option<Duration>,
    /// A progress notification restarts the timeout.
    pub reset_on_progress: bool,
    /// Esc: give up, and tell the server.
    pub cancel: Option<CancellationToken>,
}

type Pending = HashMap<RequestId, oneshot::Sender<crate::Result<Value>>>;

/// A live connection. Cheap to share: calls from several turns go through one client.
pub struct Client {
    inner: Arc<Inner>,
}

struct Inner {
    next_id: AtomicI64,
    pending: Mutex<Pending>,
    /// Progress tokens (the request's id) → the request waiting on them.
    progress: Mutex<HashMap<RequestId, mpsc::UnboundedSender<()>>>,
    /// Cancelled once the connection is gone (the process exited, or `close`).
    closed: CancellationToken,
    wire: Wire,
}

// One per connection: its size doesn't matter.
#[allow(clippy::large_enum_variant)]
enum Wire {
    Stdio { stdin: tokio::sync::Mutex<Option<ChildStdin>>, child: Mutex<Option<Child>> },
    Http(HttpWire),
}

struct HttpWire {
    url: Url,
    headers: BTreeMap<String, String>,
    http: reqwest::Client,
    session: Mutex<Option<String>>,
    protocol: Mutex<Option<String>>,
    auth: Option<tokio::sync::Mutex<McpOAuthProvider>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn closed_error() -> McpError {
    McpError::Server("Connection closed".into())
}

fn rpc_error(e: RpcError) -> McpError {
    McpError::Server(format!("MCP error {}: {}", e.code, e.message))
}

impl Client {
    /// Starts a local server. Fails only when the program can't be started.
    pub fn spawn_stdio(opts: StdioOptions) -> crate::Result<Client> {
        let mut child = tokio::process::Command::new(&opts.command)
            .args(&opts.args)
            .env_clear()
            .envs(&opts.env)
            .current_dir(&opts.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| McpError::Server(format!("couldn't start {}: {e}", opts.command)))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let inner = Arc::new(Inner {
            next_id: AtomicI64::new(0),
            pending: Mutex::default(),
            progress: Mutex::default(),
            closed: CancellationToken::new(),
            wire: Wire::Stdio { stdin: tokio::sync::Mutex::new(stdin), child: Mutex::new(Some(child)) },
        });
        if let Some(stdout) = stdout {
            let reader = inner.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout);
                let mut line = Vec::new();
                loop {
                    line.clear();
                    match lines.read_until(b'\n', &mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            // A line that isn't a message (a server logging to stdout) is skipped, as the SDK does.
                            if let Ok(message) = crate::jsonrpc::decode_line(&String::from_utf8_lossy(&line)) {
                                Inner::dispatch(&reader, message);
                            }
                        }
                    }
                }
                reader.shut();
            });
        }
        if let Some(stderr) = stderr {
            let (kept, max) = (opts.stderr, opts.stderr_lines);
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr);
                let mut line = Vec::new();
                while matches!(lines.read_until(b'\n', &mut line).await, Ok(n) if n > 0) {
                    let text = String::from_utf8_lossy(&line).trim_end_matches(['\r', '\n']).to_string();
                    line.clear();
                    if text.is_empty() {
                        continue;
                    }
                    let mut kept = lock(&kept);
                    kept.push_back(text);
                    while kept.len() > max {
                        kept.pop_front();
                    }
                }
            });
        }
        Ok(Client { inner })
    }

    /// A remote server's connection (nothing is sent until the first request).
    pub fn http(opts: HttpOptions) -> Client {
        let http = reqwest::Client::builder().connect_timeout(Duration::from_secs(30)).build().unwrap_or_default();
        Client {
            inner: Arc::new(Inner {
                next_id: AtomicI64::new(0),
                pending: Mutex::default(),
                progress: Mutex::default(),
                closed: CancellationToken::new(),
                wire: Wire::Http(HttpWire {
                    url: opts.url,
                    headers: opts.headers,
                    http,
                    session: Mutex::default(),
                    protocol: Mutex::default(),
                    auth: opts.auth.map(tokio::sync::Mutex::new),
                }),
            }),
        }
    }

    /// The MCP handshake: the protocol version the server answered with. Nothing else of its answer leaves here:
    /// a server's instructions are stranger-written text that would speak with the system prompt's authority
    /// (docs/SECURITY.md, "MCP"), so they never reach the prompt.
    pub async fn initialize(&self, version: &str, timeout: Duration) -> crate::Result<String> {
        let params = InitializeParams {
            protocol_version: PROTOCOL_VERSION.into(),
            capabilities: json!({}),
            client_info: Implementation { name: "marv".into(), version: version.into() },
        };
        let opts = RequestOptions { timeout: Some(timeout), ..Default::default() };
        let raw = self.request(method::INITIALIZE, Some(serde_json::to_value(params)?), opts).await?;
        let InitializeResult { protocol_version, .. } = serde_json::from_value(raw)
            .map_err(|e| McpError::Server(format!("the server's initialize answer isn't MCP: {e}")))?;
        if let Wire::Http(w) = &self.inner.wire {
            *lock(&w.protocol) = Some(protocol_version.clone());
        }
        self.notify(method::INITIALIZED, None).await?;
        Ok(protocol_version)
    }

    pub async fn list_tools(&self, cursor: Option<&str>, timeout: Duration) -> crate::Result<ListToolsResult> {
        let params = cursor.map(|c| json!({ "cursor": c }));
        let opts = RequestOptions { timeout: Some(timeout), ..Default::default() };
        let raw = self.request(method::TOOLS_LIST, params, opts).await?;
        serde_json::from_value(raw).map_err(|e| McpError::Server(format!("the server's tool list isn't MCP: {e}")))
    }

    /// One tool call; progress restarts `timeout`, `cancel` gives up (and tells the server).
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Map<String, Value>,
        timeout: Duration,
        cancel: CancellationToken,
    ) -> crate::Result<McpCallResult> {
        let params = serde_json::to_value(CallToolParams { name: name.into(), arguments })?;
        let opts = RequestOptions { timeout: Some(timeout), reset_on_progress: true, cancel: Some(cancel) };
        let raw = self.request(method::TOOLS_CALL, Some(params), opts).await?;
        serde_json::from_value(raw).map_err(|e| McpError::Server(format!("the server's result isn't MCP: {e}")))
    }

    /// A login's second step (the SDK transport's `finishAuth`): the code from the callback is exchanged for
    /// tokens, which the provider saves (unless the login was cancelled meanwhile).
    pub async fn finish_auth(&self, code: &str) -> crate::Result<()> {
        let Wire::Http(w) = &self.inner.wire else { return Err(McpError::Server("only remote servers log in".into())) };
        let Some(auth) = &w.auth else { return Err(McpError::Server("no login is in progress".into())) };
        let mut provider = auth.lock().await;
        oauth::auth(&mut provider, &w.url, None, Some(code)).await.map(|_| ())
    }

    /// Cancelled once the connection is gone (the server exited, or [`Client::close`]).
    pub fn closed(&self) -> CancellationToken {
        self.inner.closed.clone()
    }

    pub async fn request(&self, method: &str, mut params: Option<Value>, opts: RequestOptions) -> crate::Result<Value> {
        let inner = &self.inner;
        if inner.closed.is_cancelled() {
            return Err(closed_error());
        }
        let id = RequestId::Number(inner.next_id.fetch_add(1, Ordering::SeqCst));
        let (tx, mut rx) = oneshot::channel::<crate::Result<Value>>();
        lock(&inner.pending).insert(id.clone(), tx);
        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();
        if opts.reset_on_progress {
            // The server only reports progress when asked, with a token: the request's own id.
            let p = params.get_or_insert_with(|| json!({}));
            if let Value::Object(map) = p {
                map.insert("_meta".into(), json!({ "progressToken": id }));
            }
            lock(&inner.progress).insert(id.clone(), progress_tx);
        }
        let stop = CancellationToken::new();
        Inner::send(inner, Message::Request(Request::new(id.clone(), method, params)), Some(id.clone()), stop.clone());

        let timeout = opts.timeout.unwrap_or(Duration::from_secs(24 * 3600));
        let cancel = opts.cancel.unwrap_or_default();
        let mut deadline = tokio::time::Instant::now() + timeout;
        let mut progress_open = opts.reset_on_progress;
        let (result, gave_up) = loop {
            tokio::select! {
                r = &mut rx => break (r.unwrap_or_else(|_: oneshot::error::RecvError| Err(closed_error())), None),
                p = progress_rx.recv(), if progress_open => match p {
                    Some(()) => deadline = tokio::time::Instant::now() + timeout,
                    None => progress_open = false,
                },
                _ = tokio::time::sleep_until(deadline) => break (Err(McpError::Server(REQUEST_TIMED_OUT.into())), Some("timed out")),
                _ = cancel.cancelled() => break (Err(McpError::Server("the call was cancelled".into())), Some("cancelled")),
                _ = inner.closed.cancelled() => break (Err(closed_error()), None),
            }
        };
        lock(&inner.pending).remove(&id);
        lock(&inner.progress).remove(&id);
        if let Some(reason) = gave_up {
            stop.cancel();
            let note = json!({ "requestId": id, "reason": reason });
            Inner::send(
                inner,
                Message::Notification(Notification::new(method::CANCELLED, Some(note))),
                None,
                CancellationToken::new(),
            );
        }
        result
    }

    pub async fn notify(&self, method: &str, params: Option<Value>) -> crate::Result<()> {
        let message = Message::Notification(Notification::new(method, params));
        match &self.inner.wire {
            Wire::Stdio { .. } => self.inner.write_line(&message).await,
            // Sent and awaited: the server must have it before the next request (initialized).
            Wire::Http(w) => w.post(&self.inner, &message).await,
        }
    }

    /// Stops the connection: a local server's stdin ends, then it gets 2 s to exit before it's killed.
    pub async fn close(&self) {
        let inner = &self.inner;
        if let Wire::Stdio { stdin, child } = &inner.wire {
            stdin.lock().await.take();
            let waited = tokio::time::Instant::now();
            loop {
                let exited = lock(child).as_mut().is_none_or(|c| c.try_wait().map(|s| s.is_some()).unwrap_or(true));
                if exited {
                    break;
                }
                if waited.elapsed() >= EXIT_GRACE {
                    if let Some(c) = lock(child).as_mut() {
                        let _ = c.start_kill();
                    }
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        inner.shut();
    }

    /// Last resort, synchronous: kills a local server now.
    pub fn kill(&self) {
        if let Wire::Stdio { child, .. } = &self.inner.wire
            && let Some(c) = lock(child).as_mut()
        {
            let _ = c.start_kill();
        }
        self.inner.shut();
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // The last handle: the process is killed with it (kill_on_drop), and nothing waits on it any more.
        if Arc::strong_count(&self.inner) == 1 {
            self.inner.shut();
        }
    }
}

impl Inner {
    /// The connection is gone: everything waiting on it hears so.
    fn shut(&self) {
        self.closed.cancel();
        for (_, tx) in lock(&self.pending).drain() {
            let _ = tx.send(Err(closed_error()));
        }
    }

    fn fail(&self, id: &RequestId, error: McpError) {
        if let Some(tx) = lock(&self.pending).remove(id) {
            let _ = tx.send(Err(error));
        }
    }

    /// Sends without waiting. A request's failure to send is its answer.
    fn send(inner: &Arc<Inner>, message: Message, id: Option<RequestId>, stop: CancellationToken) {
        let inner = inner.clone();
        tokio::spawn(async move {
            let sent = match &inner.wire {
                Wire::Stdio { .. } => inner.write_line(&message).await,
                Wire::Http(w) => tokio::select! {
                    r = w.post(&inner, &message) => r,
                    _ = stop.cancelled() => Ok(()),
                    _ = inner.closed.cancelled() => Err(closed_error()),
                },
            };
            if let Some(id) = id {
                match sent {
                    Err(e) => inner.fail(&id, e),
                    // Streamable HTTP: the POST's answer carried no response for it.
                    Ok(()) if matches!(inner.wire, Wire::Http(_)) && !stop.is_cancelled() => {
                        inner.fail(&id, McpError::Server("the server's answer ended without a response".into()))
                    }
                    Ok(()) => {}
                }
            }
        });
    }

    async fn write_line(&self, message: &Message) -> crate::Result<()> {
        let Wire::Stdio { stdin, .. } = &self.wire else { return Ok(()) };
        let mut stdin = stdin.lock().await;
        let Some(pipe) = stdin.as_mut() else { return Err(closed_error()) };
        let line = encode_line(message);
        let written = async {
            pipe.write_all(line.as_bytes()).await?;
            pipe.flush().await
        };
        written.await.map_err(|_| closed_error())
    }

    /// One message from the server.
    fn dispatch(inner: &Arc<Inner>, message: Message) {
        match message {
            Message::Response(Response { id, result, error, .. }) => {
                if let Some(tx) = lock(&inner.pending).remove(&id) {
                    let _ = tx.send(match error {
                        Some(e) => Err(rpc_error(e)),
                        None => Ok(result.unwrap_or(Value::Null)),
                    });
                }
            }
            Message::Notification(n) if n.method == method::PROGRESS => {
                let token = n.params.as_ref().and_then(|p| p.get("progressToken")).cloned();
                if let Some(token) = token.and_then(|t| serde_json::from_value::<RequestId>(t).ok())
                    && let Some(tx) = lock(&inner.progress).get(&token)
                {
                    let _ = tx.send(());
                }
            }
            Message::Notification(_) => {}
            // The server asking Marv something: a ping is answered, anything else isn't offered.
            Message::Request(r) => {
                let reply = if r.method == method::PING {
                    Response::ok(r.id, json!({}))
                } else {
                    Response::err(r.id, code::METHOD_NOT_FOUND, "Method not found")
                };
                Inner::send(inner, Message::Response(reply), None, CancellationToken::new());
            }
        }
    }
}

impl HttpWire {
    /// POSTs one message, dispatching what the answer carries. A 401 runs the login's refresh (or redirect) once.
    async fn post(&self, inner: &Arc<Inner>, message: &Message) -> crate::Result<()> {
        let body = serde_json::to_string(message)?;
        let mut retried = false;
        loop {
            let mut request = self
                .http
                .post(self.url.clone())
                .header("accept", "application/json, text/event-stream")
                .header("content-type", "application/json");
            for (k, v) in &self.headers {
                request = request.header(k.as_str(), v.as_str());
            }
            if let Some(session) = lock(&self.session).clone() {
                request = request.header("mcp-session-id", session);
            }
            if let Some(protocol) = lock(&self.protocol).clone() {
                request = request.header("mcp-protocol-version", protocol);
            }
            if let Some(auth) = &self.auth
                && let Some(tokens) = auth.lock().await.tokens()
            {
                request = request.bearer_auth(tokens.access_token);
            }
            let response = request
                .body(body.clone())
                .send()
                .await
                .map_err(|e| McpError::Server(format!("couldn't reach {}: {}", self.url, without_url(&e))))?;
            let status = response.status();
            if status == reqwest::StatusCode::UNAUTHORIZED {
                let Some(auth) = &self.auth else { return Err(McpError::Unauthorized) };
                if retried {
                    return Err(McpError::Unauthorized);
                }
                let named = response
                    .headers()
                    .get("www-authenticate")
                    .and_then(|h| h.to_str().ok())
                    .and_then(oauth::resource_metadata_of);
                let mut provider = auth.lock().await;
                match oauth::auth(&mut provider, &self.url, named.as_ref(), None).await? {
                    AuthResult::Authorized => {
                        retried = true;
                        continue;
                    }
                    AuthResult::Redirect => return Err(McpError::Unauthorized),
                }
            }
            if !status.is_success() {
                let text = response.text().await.unwrap_or_default();
                return Err(McpError::Server(format!(
                    "Error POSTing to endpoint (HTTP {}): {}",
                    status.as_u16(),
                    crate::text::truncate(&crate::text::printable(text.trim()), 200)
                )));
            }
            if let Some(session) = response.headers().get("mcp-session-id").and_then(|h| h.to_str().ok()) {
                *lock(&self.session) = Some(session.to_string());
            }
            if status == reqwest::StatusCode::ACCEPTED {
                return Ok(());
            }
            let kind = response.headers().get("content-type").and_then(|h| h.to_str().ok()).unwrap_or("").to_lowercase();
            if kind.starts_with("text/event-stream") {
                return read_events(inner, response, message).await;
            }
            let text = response.text().await.map_err(|e| McpError::Server(format!("the server's answer broke off: {e}")))?;
            if text.trim().is_empty() {
                return Ok(());
            }
            let messages: Vec<Message> = match serde_json::from_str::<Value>(&text) {
                Ok(Value::Array(items)) => items.into_iter().filter_map(|v| serde_json::from_value(v).ok()).collect(),
                Ok(v) => serde_json::from_value(v).into_iter().collect(),
                Err(_) => return Err(McpError::Server("the server's answer isn't JSON".into())),
            };
            for m in messages {
                Inner::dispatch(inner, m);
            }
            return Ok(());
        }
    }
}

/// reqwest's error text without the URL (it may carry a token in its query).
fn without_url(e: &reqwest::Error) -> String {
    let mut text = e.to_string();
    if let Some(url) = e.url() {
        text = text.replace(url.as_str(), "the server");
    }
    text
}

/// A POST answered with an SSE stream: each event's data is one message. Ends when the stream does, or once the
/// response to the request arrived.
async fn read_events(inner: &Arc<Inner>, response: reqwest::Response, sent: &Message) -> crate::Result<()> {
    let wanted = if let Message::Request(r) = sent { Some(r.id.clone()) } else { None };
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| McpError::Server(format!("the server's event stream broke off: {e}")))?;
        buffer.push_str(&String::from_utf8_lossy(&chunk).replace("\r\n", "\n"));
        while let Some(end) = buffer.find("\n\n") {
            let event: String = buffer.drain(..end + 2).collect();
            let data: Vec<&str> =
                event.lines().filter_map(|l| l.strip_prefix("data:")).map(|d| d.strip_prefix(' ').unwrap_or(d)).collect();
            if data.is_empty() {
                continue;
            }
            let Ok(message) = serde_json::from_str::<Message>(&data.join("\n")) else { continue };
            let done = matches!(&message, Message::Response(r) if Some(&r.id) == wanted.as_ref());
            Inner::dispatch(inner, message);
            if done {
                return Ok(());
            }
        }
    }
    Ok(())
}
