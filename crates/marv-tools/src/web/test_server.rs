//! A local HTTP/1.1 server for the web tests (Marv's tests use `Bun.serve`): the tests never fetch the real internet.
//! One response per connection, then the connection closes.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// What the server was asked.
#[derive(Debug, Clone)]
pub struct Request {
    pub path: String,
    /// `?…`, or empty.
    pub search: String,
    pub headers: Vec<(String, String)>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone())
    }
}

pub enum Body {
    Bytes(Vec<u8>),
    /// `chunk` bytes every `every`, forever (until the client hangs up, which is counted).
    Endless {
        chunk: usize,
        every: Duration,
    },
}

pub struct Response {
    pub status: u16,
    pub reason: &'static str,
    pub headers: Vec<(String, String)>,
    pub body: Body,
    /// Waits this long before answering.
    pub delay: Duration,
}

impl Response {
    pub fn new(status: u16, body: impl Into<Vec<u8>>) -> Self {
        let reason = match status {
            200 => "OK",
            301 => "Moved Permanently",
            302 => "Found",
            403 => "Forbidden",
            404 => "Not Found",
            409 => "Conflict",
            _ => "",
        };
        Self { status, reason, headers: Vec::new(), body: Body::Bytes(body.into()), delay: Duration::ZERO }
    }
    pub fn text(body: impl Into<Vec<u8>>, content_type: &str) -> Self {
        Self::new(200, body).header("content-type", content_type)
    }
    pub fn json(value: serde_json::Value) -> Self {
        Self::text(value.to_string(), "application/json")
    }
    pub fn redirect(location: &str, status: u16) -> Self {
        Self::new(status, "").header("location", location)
    }
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
    pub fn status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }
    pub fn reason(mut self, reason: &'static str) -> Self {
        self.reason = reason;
        self
    }
    pub fn delayed(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

type Handler = dyn Fn(&Request) -> Response + Send + Sync;

pub struct TestServer {
    pub port: u16,
    /// Requests answered so far.
    pub requests: Arc<AtomicUsize>,
    /// Endless bodies the client stopped reading (the connection was closed under them).
    pub cancelled: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl TestServer {
    pub async fn start(handler: impl Fn(&Request) -> Response + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handler: Arc<Handler> = Arc::new(handler);
        let requests = Arc::new(AtomicUsize::new(0));
        let cancelled = Arc::new(AtomicUsize::new(0));
        let (counted, cut) = (requests.clone(), cancelled.clone());
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { return };
                let (handler, counted, cut) = (handler.clone(), counted.clone(), cut.clone());
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut buf = [0u8; 4096];
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => head.extend_from_slice(&buf[..n]),
                        }
                        if !head[0].is_ascii_uppercase() {
                            return; // not HTTP (a TLS handshake): hang up, as a plain-http server does
                        }
                    }
                    let text = String::from_utf8_lossy(&head).to_string();
                    let mut lines = text.split("\r\n");
                    let target = lines.next().unwrap_or("").split(' ').nth(1).unwrap_or("/").to_string();
                    let (path, search) = match target.find('?') {
                        Some(i) => (target[..i].to_string(), target[i..].to_string()),
                        None => (target, String::new()),
                    };
                    let headers = lines
                        .take_while(|l| !l.is_empty())
                        .filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_string(), v.trim().to_string())))
                        .collect();
                    counted.fetch_add(1, Ordering::SeqCst);
                    let response = handler(&Request { path, search, headers });
                    tokio::time::sleep(response.delay).await;
                    let mut out = format!("HTTP/1.1 {} {}\r\nconnection: close\r\n", response.status, response.reason);
                    for (k, v) in &response.headers {
                        out.push_str(&format!("{k}: {v}\r\n"));
                    }
                    match response.body {
                        Body::Bytes(bytes) => {
                            out.push_str(&format!("content-length: {}\r\n\r\n", bytes.len()));
                            let _ = socket.write_all(out.as_bytes()).await;
                            let _ = socket.write_all(&bytes).await;
                        }
                        Body::Endless { chunk, every } => {
                            out.push_str("\r\n");
                            let _ = socket.write_all(out.as_bytes()).await;
                            let bytes = vec![b'a'; chunk];
                            loop {
                                tokio::time::sleep(every).await;
                                if socket.write_all(&bytes).await.is_err() {
                                    cut.fetch_add(1, Ordering::SeqCst);
                                    return;
                                }
                            }
                        }
                    }
                    let _ = socket.shutdown().await;
                });
            }
        });
        Self { port, requests, cancelled, task }
    }

    /// `http://localhost:<port>`
    pub fn base(&self) -> String {
        format!("http://localhost:{}", self.port)
    }

    pub fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
