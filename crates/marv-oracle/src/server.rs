//! The fake OpenAI-compatible server: replays a [`script`](crate::script) and records every request.
//!
//! No Marv file ports to this (Marv's own tests stub `fetch`). It speaks just enough HTTP/1.1 for
//! Marv's adapter: `POST <base>/chat/completions` answers with the next scripted reply, `GET
//! <base>/models` with an empty list, anything else with 404. Each response closes its connection,
//! so a stream's end is the socket's end, which is also how a dropped stream looks to the client.
//!
//! It binds `127.0.0.1` on a free port and never talks to the network: the oracle never calls a real
//! model and never spends money.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use serde_json::{Value, json};

use crate::script::{Reply, Response};

/// A request the server received.
#[derive(Debug, Clone, PartialEq)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    /// The body exactly as sent: request bytes are what the prompt cache keys on.
    pub body: String,
}

#[derive(Default)]
struct State {
    replies: Vec<Reply>,
    /// How many chat requests have been answered.
    answered: usize,
    requests: Vec<Recorded>,
}

pub struct FakeServer {
    addr: SocketAddr,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl FakeServer {
    /// Starts serving `replies` on a free local port.
    pub fn start(replies: Vec<Reply>) -> anyhow::Result<FakeServer> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let state = Arc::new(Mutex::new(State { replies, ..State::default() }));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (state, stop) = (state.clone(), stop.clone());
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    let state = state.clone();
                    // One thread per connection: a client may hold one open while it makes another.
                    std::thread::spawn(move || {
                        let _ = handle(stream, &state);
                    });
                }
            })
        };
        Ok(FakeServer { addr, state, stop, thread: Some(thread) })
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// The base URL a config points at (`…/v1`, like OpenRouter's `…/api/v1`).
    pub fn base_url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    /// Every request so far, in arrival order.
    pub fn requests(&self) -> Vec<Recorded> {
        self.state.lock().unwrap().requests.clone()
    }

    /// Scripted replies no request asked for: a scenario whose binary stopped early.
    pub fn unused(&self) -> usize {
        let state = self.state.lock().unwrap();
        state.replies.len().saturating_sub(state.answered)
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop so it sees the flag.
        let _ = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn handle(stream: TcpStream, state: &Mutex<State>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (Some(method), Some(path)) = (parts.next(), parts.next()) else { return Ok(()) };
    let (method, path) = (method.to_string(), path.to_string());
    let mut length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            break;
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let body = String::from_utf8_lossy(&body).into_owned();

    let mut out = stream;
    let route = path.split('?').next().unwrap_or("").to_string();
    if method == "POST" && route.ends_with("/chat/completions") {
        let model =
            serde_json::from_str::<Value>(&body).ok().and_then(|b| b["model"].as_str().map(String::from)).unwrap_or_default();
        let response = {
            let mut state = state.lock().unwrap();
            state.requests.push(Recorded { method, path, body });
            let n = state.answered;
            state.answered += 1;
            match state.replies.get(n) {
                Some(reply) => reply.response(&model),
                None => Response::Status {
                    status: 500,
                    retry_after: None,
                    body: json!({"error": {"message": format!("oracle: the script has no reply for request {}", n + 1)}})
                        .to_string(),
                },
            }
        };
        write_response(&mut out, response)
    } else {
        state.lock().unwrap().requests.push(Recorded { method: method.clone(), path, body });
        if method == "GET" && route.ends_with("/models") {
            write_status(&mut out, 200, None, &json!({"data": []}).to_string())
        } else {
            write_status(&mut out, 404, None, &json!({"error": {"message": "oracle: not found"}}).to_string())
        }
    }
}

fn write_response(out: &mut TcpStream, response: Response) -> std::io::Result<()> {
    match response {
        Response::Status { status, retry_after, body } => write_status(out, status, retry_after.as_deref(), &body),
        Response::Stream(events) => {
            out.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n",
            )?;
            for data in events {
                out.write_all(format!("data: {data}\n\n").as_bytes())?;
                out.flush()?;
            }
            out.shutdown(std::net::Shutdown::Both)
        }
    }
}

fn write_status(out: &mut TcpStream, status: u16, retry_after: Option<&str>, body: &str) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let retry = retry_after.map(|r| format!("retry-after: {r}\r\n")).unwrap_or_default();
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{retry}connection: close\r\n\r\n",
        body.len()
    );
    out.write_all(head.as_bytes())?;
    out.write_all(body.as_bytes())?;
    out.flush()?;
    out.shutdown(std::net::Shutdown::Both)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::script::parse_script;

    fn request(server: &FakeServer, method: &str, path: &str, body: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", server.port())).unwrap();
        write!(stream, "{method} {path} HTTP/1.1\r\nhost: x\r\ncontent-length: {}\r\n\r\n{body}", body.len()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    #[test]
    fn replays_the_script_in_order_and_records_bodies() {
        let server = FakeServer::start(parse_script("{\"text\": \"one\"}\n{\"text\": \"two\"}").unwrap()).unwrap();
        let first = request(&server, "POST", "/v1/chat/completions", r#"{"model":"m","n":1}"#);
        assert!(first.starts_with("HTTP/1.1 200"), "{first}");
        assert!(first.contains(r#""content":"one""#) && first.ends_with("data: [DONE]\n\n"), "{first}");
        assert!(request(&server, "POST", "/v1/chat/completions", "{}").contains(r#""content":"two""#));
        let requests = server.requests();
        assert_eq!(requests[0].body, r#"{"model":"m","n":1}"#);
        assert_eq!(requests.len(), 2);
        assert_eq!(server.unused(), 0);
    }

    #[test]
    fn a_request_past_the_script_gets_a_500() {
        let server = FakeServer::start(Vec::new()).unwrap();
        let response = request(&server, "POST", "/v1/chat/completions", "{}");
        assert!(response.starts_with("HTTP/1.1 500") && response.contains("no reply for request 1"), "{response}");
    }

    #[test]
    fn scripted_status_with_retry_after() {
        let server = FakeServer::start(parse_script(r#"{"status": 429, "retry_after": "2"}"#).unwrap()).unwrap();
        let response = request(&server, "POST", "/v1/chat/completions", "{}");
        assert!(response.starts_with("HTTP/1.1 429") && response.contains("retry-after: 2\r\n"), "{response}");
    }

    #[test]
    fn models_is_an_empty_list_and_the_rest_404() {
        let server = FakeServer::start(Vec::new()).unwrap();
        assert!(request(&server, "GET", "/v1/models", "").ends_with(r#"{"data":[]}"#));
        assert!(request(&server, "GET", "/v1/other", "").starts_with("HTTP/1.1 404"));
        assert_eq!(server.requests().len(), 2);
    }
}
