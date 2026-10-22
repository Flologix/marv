//! A local stand-in for OpenRouter and Ollama: a tiny HTTP/1.1 server on 127.0.0.1 where each
//! test decides how every request is answered (Marv's tests use `Bun.serve` the same way). Never
//! the network: tests point the adapters at this.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// One request as the server saw it.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub path: String,
    /// Names lowercased.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// How a response's body ends.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum End {
    /// The last chunk is sent: a clean end.
    Complete,
    /// The connection is closed mid-body: the client sees a transport error.
    Drop,
    /// Nothing more is sent and the connection stays open.
    Hang,
    /// The connection is reset (TCP RST) mid-body, the way a broken TLS session or a middlebox ends it.
    Reset,
    /// The connection is reset right after the request arrives, before any response: the send itself fails.
    ResetBeforeReply,
    /// After the chunks, nothing is sent for this long, then the body ends cleanly (an idle stream).
    StallThenComplete(Duration),
}

#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// Sent one by one, as separate chunks.
    pub chunks: Vec<Vec<u8>>,
    pub end: End,
    /// A wait before each chunk after the first (a stream that trickles in).
    pub pace: Duration,
}

impl Reply {
    pub fn new(status: u16, content_type: &str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), content_type.into())],
            chunks: vec![body.into()],
            end: End::Complete,
            pace: Duration::ZERO,
        }
    }

    pub fn json(status: u16, value: Value) -> Self {
        Self::new(status, "application/json", value.to_string())
    }

    pub fn text(status: u16, body: &str) -> Self {
        Self::new(status, "text/plain", body)
    }

    /// Server-sent events, one chunk each: a string is sent as is (`[DONE]`), anything else as JSON.
    pub fn sse(events: &[Value]) -> Self {
        let chunks = events
            .iter()
            .map(|e| match e {
                Value::String(s) => format!("data: {s}\n\n").into_bytes(),
                other => format!("data: {other}\n\n").into_bytes(),
            })
            .collect();
        Self {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            chunks,
            end: End::Complete,
            pace: Duration::ZERO,
        }
    }

    /// Ollama's NDJSON: one JSON object per line, one chunk each.
    pub fn ndjson(lines: &[Value]) -> Self {
        let chunks = lines.iter().map(|l| format!("{l}\n").into_bytes()).collect();
        Self {
            status: 200,
            headers: vec![("content-type".into(), "application/x-ndjson".into())],
            chunks,
            end: End::Complete,
            pace: Duration::ZERO,
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn paced(mut self, pace: Duration) -> Self {
        self.pace = pace;
        self
    }

    pub fn ending(mut self, end: End) -> Self {
        self.end = end;
        self
    }
}

type Handler = dyn Fn(&Request) -> Reply + Send + Sync;

/// The server runs until dropped.
pub struct FakeServer {
    pub port: u16,
    requests: Arc<Mutex<Vec<Request>>>,
    task: tokio::task::JoinHandle<()>,
}

impl FakeServer {
    pub async fn start(handler: impl Fn(&Request) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let handler: Arc<Handler> = Arc::new(handler);
        let seen = requests.clone();
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            while let Ok((socket, _)) = listener.accept().await {
                connections.spawn(serve(socket, handler.clone(), seen.clone()));
            }
        });
        Self { port, requests, task }
    }

    /// `http://127.0.0.1:<port>`
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    pub fn last(&self) -> Request {
        self.requests().pop().expect("no request yet")
    }

    /// How many requests went to `path`.
    pub fn count(&self, path: &str) -> usize {
        self.requests().iter().filter(|r| r.path == path).count()
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        // Dropping the accept loop drops its JoinSet, which aborts open (hanging) connections.
        self.task.abort();
    }
}

async fn serve(mut socket: TcpStream, handler: Arc<Handler>, seen: Arc<Mutex<Vec<Request>>>) {
    let Some(request) = read_request(&mut socket).await else { return };
    seen.lock().unwrap().push(request.clone());
    let reply = handler(&request);
    if reply.end == End::ResetBeforeReply {
        let _ = socket.set_zero_linger();
        return;
    }
    let mut head = format!("HTTP/1.1 {} {}\r\n", reply.status, reason(reply.status));
    for (name, value) in &reply.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("transfer-encoding: chunked\r\nconnection: close\r\n\r\n");
    if socket.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    for (i, chunk) in reply.chunks.iter().enumerate() {
        if i > 0 && !reply.pace.is_zero() {
            tokio::time::sleep(reply.pace).await;
        }
        let mut framed = format!("{:x}\r\n", chunk.len()).into_bytes();
        framed.extend_from_slice(chunk);
        framed.extend_from_slice(b"\r\n");
        if socket.write_all(&framed).await.is_err() || socket.flush().await.is_err() {
            return;
        }
    }
    match reply.end {
        End::Complete => {
            let _ = socket.write_all(b"0\r\n\r\n").await;
            let _ = socket.shutdown().await;
        }
        End::Drop => {
            let _ = socket.shutdown().await;
        }
        End::Hang => tokio::time::sleep(Duration::from_secs(600)).await,
        End::Reset => {
            // macOS throws away what the client hasn't read yet when the RST arrives, so the chunks sent above could
            // vanish and the test would see a reset with no partial reply. Give the client time to read them first.
            tokio::time::sleep(Duration::from_millis(300)).await;
            // Dropping a socket with a zero linger sends RST instead of FIN.
            let _ = socket.set_zero_linger();
        }
        End::ResetBeforeReply => unreachable!("handled before the head"),
        End::StallThenComplete(wait) => {
            tokio::time::sleep(wait).await;
            let _ = socket.write_all(b"0\r\n\r\n").await;
            let _ = socket.shutdown().await;
        }
    }
}

async fn read_request(socket: &mut TcpStream) -> Option<Request> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break at;
        }
        let n = socket.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let (method, path) = (first.next()?.to_string(), first.next()?.to_string());
    let headers: Vec<(String, String)> =
        lines.filter_map(|l| l.split_once(':')).map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string())).collect();
    let length: usize = headers.iter().find(|(n, _)| n == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < length {
        let n = socket.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Some(Request { method, path, headers, body })
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Status",
    }
}
