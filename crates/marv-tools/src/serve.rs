//! `marv __serve <folder> <port>`: the static file server behind the process tool's `serve` action. marv-rs only.
//!
//! The agent hand-wrote a `node -e` server to look at `dist/` because `serve` wasn't installed. This is that, built in:
//! a small HTTP/1.1 server over Tokio that runs as a hidden subcommand of Marv's own binary, inside the process's
//! sandbox (no network of its own, the project the only thing it can read, its port forwarded to the user's
//! `127.0.0.1` like any process's), never in Marv's process.
//!
//! - `GET` and `HEAD` only; the query and fragment are dropped, the path is percent-decoded. `/` and any folder
//!   answer with its `index.html`.
//! - Nothing outside the folder: a `..` segment, a NUL, a backslash, or a symlink that leads out is a 404, and so is any
//!   hidden entry (a segment starting with `.`: `.env`, `.git`, `.well-known` too), which a static site has no reason
//!   to share.
//! - A path with no extension that isn't a file is an app route: it answers the root's `index.html` (a single-page
//!   app's fallback). One with an extension that isn't a file is a 404 (a missing script mustn't answer with HTML).
//! - Content types for html, js, css, svg, json, png, jpg, gif, webp, ico, wasm, maps, fonts, txt and xml; anything
//!   else `application/octet-stream`. `Cache-Control: no-cache`, so a rebuilt `dist/` shows on reload.
//! - A request whose `Host` isn't `localhost`, `127.0.0.1` or `[::1]` (with or without a port) is a 403, so a page the
//!   user has open in their browser can't read the site through a name it controls that resolves to 127.0.0.1 (DNS
//!   rebinding).
//! - A hidden entry is never served, whatever the URL says: a link inside the folder that leads to one (`a -> .env`) is
//!   a 404 too (the check is on the real path).
//! - One request per connection (`Connection: close`), headers read up to [`MAX_HEAD`] bytes within [`HEAD_TIMEOUT`].
//!   At most [`MAX_CONNECTIONS`] at once, each write within [`WRITE_TIMEOUT`], a file streamed (up to [`MAX_FILE`]).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

/// `marv __serve <folder> <port>`.
pub const SERVE_SUBCOMMAND: &str = "__serve";
/// The longest request head read.
pub const MAX_HEAD: usize = 16 * 1024;
/// How long a client has to send its request head.
pub const HEAD_TIMEOUT: Duration = Duration::from_secs(10);

/// How many connections are served at once; the rest wait to be accepted.
pub const MAX_CONNECTIONS: usize = 64;
/// How long one write to a client may take: a client that stops reading doesn't hold a connection.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the accept loop rests after an error (no free descriptors) instead of spinning.
pub const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);
/// The largest file served (a bigger one is a 413): the server's job is a site's `dist/`.
pub const MAX_FILE: u64 = 256 * 1024 * 1024;

/// What an answer carries.
#[derive(Debug, PartialEq, Eq)]
pub enum Body {
    Bytes(Vec<u8>),
    /// A file, streamed from disk: its path and its length when it was found.
    File {
        path: PathBuf,
        len: u64,
    },
}

/// One answer: status line text, content type, body.
#[derive(Debug, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    pub reason: &'static str,
    pub content_type: &'static str,
    pub body: Body,
}

impl Reply {
    fn text(status: u16, reason: &'static str) -> Self {
        Self {
            status,
            reason,
            content_type: "text/plain; charset=utf-8",
            body: Body::Bytes(format!("{status} {reason}\n").into_bytes()),
        }
    }

    pub fn len(&self) -> u64 {
        match &self.body {
            Body::Bytes(bytes) => bytes.len() as u64,
            Body::File { len, .. } => *len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The body's bytes, a file read whole (the tests').
    pub fn bytes(&self) -> Vec<u8> {
        match &self.body {
            Body::Bytes(bytes) => bytes.clone(),
            Body::File { path, .. } => std::fs::read(path).unwrap_or_default(),
        }
    }
}

/// Whether a `Host` header names this machine's loopback: `localhost`, `127.0.0.1` or `[::1]`, with or without a port.
/// Anything else (a name an attacker's page resolves to 127.0.0.1) is refused.
pub fn host_is_local(host: Option<&str>) -> bool {
    let Some(host) = host.map(str::trim) else { return false };
    let (name, port) = if host.starts_with('[') {
        let Some(end) = host.find(']') else { return false };
        let rest = &host[end + 1..];
        let port = if rest.is_empty() {
            None
        } else if let Some(port) = rest.strip_prefix(':') {
            Some(port)
        } else {
            return false;
        };
        (&host[..=end], port)
    } else {
        match host.split_once(':') {
            Some((name, port)) => (name, Some(port)),
            None => (host, None),
        }
    };
    let port_ok = port.is_none_or(|p| !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit()));
    port_ok && (name.eq_ignore_ascii_case("localhost") || name == "127.0.0.1" || name == "[::1]")
}

/// The `Content-Type` for a file name, by extension.
pub fn content_type(path: &Path) -> &'static str {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" | "cjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "json" | "map" | "webmanifest" => "application/json; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "wasm" => "application/wasm",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "txt" | "md" => "text/plain; charset=utf-8",
        "xml" => "application/xml; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The URL path's segments, or `None` for one that isn't served: `..`, a hidden entry, a NUL or a backslash.
fn segments(target: &str) -> Option<Vec<String>> {
    let path = target.split(['?', '#']).next().unwrap_or("");
    let decoded = percent_decode(path)?;
    if decoded.contains(['\0', '\\']) || !decoded.starts_with('/') {
        return None;
    }
    let mut out = Vec::new();
    for segment in decoded.split('/') {
        match segment {
            "" | "." => {}
            s if s.starts_with('.') => return None,
            s => out.push(s.to_string()),
        }
    }
    Some(out)
}

/// A file under `root` (already canonical), after following symlinks: `None` when it isn't a file there, or when the
/// real path under `root` has a hidden component (`dist/a -> .env`: the URL said nothing hidden).
fn file_under(root: &Path, relative: &[String]) -> Option<PathBuf> {
    let mut path = root.to_path_buf();
    path.extend(relative);
    let mut real = std::fs::canonicalize(&path).ok()?;
    if real.is_dir() {
        real = std::fs::canonicalize(real.join("index.html")).ok()?;
    }
    let inside = real.strip_prefix(root).ok()?;
    let hidden = inside.components().any(|c| c.as_os_str().to_string_lossy().starts_with('.'));
    (!hidden && real.is_file()).then_some(real)
}

/// What `method target` answers, for a folder `root` (canonical).
pub fn respond(root: &Path, method: &str, target: &str) -> Reply {
    if method != "GET" && method != "HEAD" {
        return Reply::text(405, "Method Not Allowed");
    }
    let Some(parts) = segments(target) else { return Reply::text(404, "Not Found") };
    let found = file_under(root, &parts).or_else(|| {
        // An app route (no extension, not a file): the root's index.html. A name with an extension is a missing file.
        let has_extension = parts.last().is_some_and(|last| Path::new(last).extension().is_some());
        (!has_extension).then(|| file_under(root, &[])).flatten()
    });
    let Some(file) = found else { return Reply::text(404, "Not Found") };
    match std::fs::metadata(&file) {
        Ok(meta) if meta.len() > MAX_FILE => Reply::text(413, "Payload Too Large"),
        Ok(meta) => Reply {
            status: 200,
            reason: "OK",
            content_type: content_type(&file),
            body: Body::File { path: file, len: meta.len() },
        },
        Err(_) => Reply::text(404, "Not Found"),
    }
}

/// The value of the `Host` header in a request head, if there is one.
fn host_of(head: &str) -> Option<&str> {
    head.lines().skip(1).take_while(|l| !l.is_empty()).find_map(|l| {
        let (name, value) = l.split_once(':')?;
        name.eq_ignore_ascii_case("host").then_some(value.trim())
    })
}

/// A write that gives up after [`WRITE_TIMEOUT`].
async fn write_in_time(stream: &mut TcpStream, bytes: &[u8]) -> bool {
    matches!(tokio::time::timeout(WRITE_TIMEOUT, stream.write_all(bytes)).await, Ok(Ok(())))
}

/// Streams `len` bytes of `path` to `stream`: a chunk at a time, each write within [`WRITE_TIMEOUT`].
async fn stream_file(stream: &mut TcpStream, path: &Path, len: u64) -> bool {
    let Ok(file) = tokio::fs::File::open(path).await else { return false };
    let mut file = file.take(len);
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        match file.read(&mut chunk).await {
            Ok(0) => return true,
            Ok(n) => {
                if !write_in_time(stream, &chunk[..n]).await {
                    return false;
                }
            }
            Err(_) => return false,
        }
    }
}

async fn handle(mut stream: TcpStream, root: PathBuf) {
    let mut head = Vec::new();
    let mut chunk = [0u8; 2048];
    let read = async {
        while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < MAX_HEAD {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => head.extend_from_slice(&chunk[..n]),
            }
        }
    };
    let _ = tokio::time::timeout(HEAD_TIMEOUT, read).await;
    let text = String::from_utf8_lossy(&head);
    let mut line = text.lines().next().unwrap_or("").split(' ');
    let (method, target) = (line.next().unwrap_or(""), line.next().unwrap_or(""));
    let reply = if target.is_empty() {
        Reply::text(400, "Bad Request")
    } else if !host_is_local(host_of(&text)) {
        Reply::text(403, "Forbidden")
    } else {
        respond(&root, method, target)
    };
    let mut out = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        reply.status,
        reply.reason,
        reply.content_type,
        reply.len()
    )
    .into_bytes();
    let wrote = match (&reply.body, method) {
        (Body::Bytes(bytes), m) if m != "HEAD" => {
            out.extend_from_slice(bytes);
            write_in_time(&mut stream, &out).await
        }
        (Body::File { path, len }, m) if m != "HEAD" => {
            write_in_time(&mut stream, &out).await && stream_file(&mut stream, path, *len).await
        }
        _ => write_in_time(&mut stream, &out).await,
    };
    if wrote {
        let _ = tokio::time::timeout(WRITE_TIMEOUT, stream.shutdown()).await;
    }
}

/// Serves `root` (canonical) on `listener` until the task ends.
pub async fn serve(listener: TcpListener, root: PathBuf) {
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        // A free slot first: past the limit, connections wait in the listener's backlog.
        let Ok(slot) = slots.clone().acquire_owned().await else { return };
        let Ok((stream, _)) = listener.accept().await else {
            tokio::time::sleep(ACCEPT_BACKOFF).await;
            continue;
        };
        let root = root.clone();
        tokio::spawn(async move {
            handle(stream, root).await;
            drop(slot);
        });
    }
}

/// `marv __serve <folder> <port>`: listens on 127.0.0.1 and serves until it's stopped.
pub fn main(folder: &str, port: u16) -> ExitCode {
    let root = match std::fs::canonicalize(folder) {
        Ok(root) if root.is_dir() => root,
        _ => {
            eprintln!("marv __serve: {folder} isn't a folder");
            return ExitCode::from(1);
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("marv __serve: {e}");
            return ExitCode::from(1);
        }
    };
    runtime.block_on(async {
        let listener = match TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("marv __serve: can't listen on 127.0.0.1:{port}: {e}");
                return ExitCode::from(1);
            }
        };
        println!("Serving {} on http://localhost:{port}", root.display());
        serve(listener, root).await;
        ExitCode::SUCCESS
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    fn site() -> TempDir {
        let dir = TempDir::new("serve");
        let p = dir.path();
        std::fs::create_dir_all(p.join("assets")).unwrap();
        std::fs::create_dir_all(p.join("docs")).unwrap();
        std::fs::write(p.join("index.html"), "<h1>app</h1>").unwrap();
        std::fs::write(p.join("assets/app.js"), "console.log(1)").unwrap();
        std::fs::write(p.join("assets/app.js.map"), "{}").unwrap();
        std::fs::write(p.join("docs/index.html"), "<h1>docs</h1>").unwrap();
        std::fs::write(p.join("logo.svg"), "<svg/>").unwrap();
        std::fs::write(p.join(".env"), "SECRET=1").unwrap();
        dir
    }

    fn get(dir: &TempDir, target: &str) -> Reply {
        respond(&std::fs::canonicalize(dir.path()).unwrap(), "GET", target)
    }

    #[test]
    fn root_and_folders_answer_their_index_and_files_their_content_type() {
        let dir = site();
        assert_eq!(get(&dir, "/").bytes(), b"<h1>app</h1>");
        assert_eq!(get(&dir, "/docs/").bytes(), b"<h1>docs</h1>");
        assert_eq!(get(&dir, "/docs").bytes(), b"<h1>docs</h1>");
        for (target, kind) in [
            ("/", "text/html; charset=utf-8"),
            ("/assets/app.js", "text/javascript; charset=utf-8"),
            ("/assets/app.js.map", "application/json; charset=utf-8"),
            ("/logo.svg", "image/svg+xml"),
        ] {
            let reply = get(&dir, target);
            assert_eq!((reply.status, reply.content_type), (200, kind), "{target}");
        }
        for (name, kind) in [
            ("a.css", "text/css; charset=utf-8"),
            ("a.json", "application/json; charset=utf-8"),
            ("a.png", "image/png"),
            ("a.jpg", "image/jpeg"),
            ("a.JPEG", "image/jpeg"),
            ("a.wasm", "application/wasm"),
            ("a.bin", "application/octet-stream"),
        ] {
            assert_eq!(content_type(Path::new(name)), kind, "{name}");
        }
        assert_eq!(get(&dir, "/assets/app.js?v=3#x").bytes(), b"console.log(1)", "the query is dropped");
        assert_eq!(get(&dir, "/assets/a%70p.js").status, 200, "percent-decoded");
    }

    #[test]
    fn a_route_without_an_extension_falls_back_to_the_index_and_a_missing_file_is_a_404() {
        let dir = site();
        let route = get(&dir, "/users/42/settings");
        assert_eq!((route.status, route.bytes().as_slice()), (200, b"<h1>app</h1>".as_slice()));
        assert_eq!(get(&dir, "/assets/missing.js").status, 404, "a missing script isn't HTML");
        assert_eq!(get(&dir, "/nothing.html").status, 404);
        // No index.html: a route is a 404 too.
        let bare = TempDir::new("serve-bare");
        assert_eq!(get(&bare, "/anything").status, 404);
    }

    #[test]
    fn nothing_outside_the_folder_or_hidden_is_served() {
        let dir = site();
        for target in [
            "/../secret",
            "/assets/../../secret",
            "/%2e%2e/secret",
            "/%2E%2E%2Fsecret",
            "/assets/..%2f..%2fsecret",
            "/.env",
            "/%2eenv",
            "/assets/%2e%2e/.env",
            "/a%00b",
            "/a\\b",
            "/%zz",
            "no-slash",
        ] {
            assert_ne!(get(&dir, target).bytes(), b"SECRET=1", "{target}");
            let reply = get(&dir, target);
            assert!(reply.status == 404 || reply.bytes() == b"<h1>app</h1>", "{target}: {reply:?}");
        }
        assert_eq!(get(&dir, "/.env").status, 404);
        // A symlink that leads out of the folder is a 404.
        let outside = TempDir::new("serve-outside");
        std::fs::write(outside.path().join("key.txt"), "KEY").unwrap();
        std::os::unix::fs::symlink(outside.path().join("key.txt"), dir.path().join("key.txt")).unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("out")).unwrap();
        assert_eq!(get(&dir, "/key.txt").status, 404);
        assert_eq!(get(&dir, "/out/key.txt").status, 404);
        // Only GET and HEAD.
        assert_eq!(respond(&std::fs::canonicalize(dir.path()).unwrap(), "POST", "/").status, 405);
    }

    #[test]
    fn only_the_loopback_names_are_local_hosts() {
        for host in ["localhost", "LocalHost:4173", "127.0.0.1", "127.0.0.1:80", "[::1]", "[::1]:4173", " localhost "] {
            assert!(host_is_local(Some(host)), "{host}");
        }
        for host in [
            "",
            "evil.example",
            "localhost.evil.example",
            "evil.example:80",
            "127.0.0.2",
            "0.0.0.0",
            "[::2]",
            "[::1",
            "[::1]x",
            "localhost:",
            "localhost:abc",
            "localhost:123456",
            "[::1]:",
            "::1",
            "localhost@evil",
            "127.0.0.1.evil.example",
        ] {
            assert!(!host_is_local(Some(host)), "{host:?}");
        }
        assert!(!host_is_local(None));
        assert_eq!(host_of("GET / HTTP/1.1\r\nAccept: */*\r\nHOST: localhost:1\r\n\r\n"), Some("localhost:1"));
        assert_eq!(host_of("GET / HTTP/1.1\r\n\r\nHost: localhost\r\n"), None, "a header after the head isn't one");
    }

    #[test]
    fn a_link_to_a_hidden_file_inside_the_folder_and_a_file_over_the_cap_are_not_served() {
        let dir = site();
        std::fs::create_dir_all(dir.path().join("dist")).unwrap();
        std::fs::create_dir_all(dir.path().join("dist/.git")).unwrap();
        std::fs::write(dir.path().join("dist/index.html"), "<h1>dist</h1>").unwrap();
        std::fs::write(dir.path().join("dist/.env"), "SECRET").unwrap();
        std::fs::write(dir.path().join("dist/.git/config.txt"), "TOKEN").unwrap();
        std::os::unix::fs::symlink(dir.path().join("dist/.env"), dir.path().join("dist/a.txt")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("dist/.git"), dir.path().join("dist/g")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("logo.svg"), dir.path().join("dist/logo.svg")).unwrap();
        let root = std::fs::canonicalize(dir.path().join("dist")).unwrap();
        let root_get = |target: &str| respond(&root, "GET", target);
        assert_eq!(root_get("/.env").status, 404, "the URL itself");
        assert_eq!(root_get("/a.txt").status, 404, "a.txt -> .env");
        assert_eq!(root_get("/g/config.txt").status, 404, "g -> .git");
        assert_eq!(root_get("/logo.svg").status, 404, "a link out of the folder (to a visible file)");
        assert_eq!(root_get("/").bytes(), b"<h1>dist</h1>");
        // A folder under a hidden one (the served folder itself) is fine: only what's below the root counts.
        let hidden_root = TempDir::new(".serve-hidden-root");
        std::fs::write(hidden_root.path().join("index.html"), "ok").unwrap();
        assert_eq!(get(&hidden_root, "/").bytes(), b"ok");
        // A sparse file over the cap is a 413, not read.
        let big = std::fs::File::create(dir.path().join("big.bin")).unwrap();
        big.set_len(MAX_FILE + 1).unwrap();
        assert_eq!(get(&dir, "/big.bin").status, 413);
    }

    #[tokio::test]
    async fn a_big_file_is_streamed_whole_and_a_stuck_client_doesnt_stop_the_others() {
        let dir = site();
        let body: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(dir.path().join("big.bin"), &body).unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(serve(listener, root));
        // A client that connects and says nothing holds one slot, not the server.
        let _idle = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).await.unwrap();
        let mut stream = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).await.unwrap();
        stream.write_all(b"GET /big.bin HTTP/1.1\r\nHost: localhost\r\n\r\n").await.unwrap();
        let mut got = Vec::new();
        stream.read_to_end(&mut got).await.unwrap();
        let split = got.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        assert!(String::from_utf8_lossy(&got[..split]).contains("Content-Length: 3000000\r\n"));
        assert_eq!(&got[split..], body.as_slice());
        task.abort();
    }

    #[tokio::test]
    async fn it_answers_over_a_real_socket_and_head_has_no_body() {
        let dir = site();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(serve(listener, root));
        let ask = |request: &'static str| async move {
            let mut stream = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).await.unwrap();
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut text = String::new();
            stream.read_to_string(&mut text).await.unwrap();
            text
        };
        let got = ask("GET /assets/app.js HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
        assert!(got.starts_with("HTTP/1.1 200 OK\r\n"), "{got}");
        assert!(got.contains("Content-Type: text/javascript; charset=utf-8\r\n") && got.contains("Content-Length: 14\r\n"));
        assert!(got.ends_with("\r\n\r\nconsole.log(1)"), "{got}");
        let head = ask("HEAD / HTTP/1.1\r\nHost: 127.0.0.1:4173\r\n\r\n").await;
        assert!(head.starts_with("HTTP/1.1 200 OK") && head.ends_with("\r\n\r\n"), "{head}");
        let missing = ask("GET /x.png HTTP/1.1\r\nhost: [::1]\r\n\r\n").await;
        assert!(missing.starts_with("HTTP/1.1 404 Not Found"), "{missing}");
        let bad = ask("\r\n\r\n").await;
        assert!(bad.starts_with("HTTP/1.1 400"), "{bad}");
        // DNS rebinding: a name that isn't the machine's loopback (or no Host at all) is a 403, whatever it asks for.
        for request in [
            "GET / HTTP/1.1\r\nHost: evil.example\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: evil.example:4173\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: localhost.evil.example\r\n\r\n",
            "GET / HTTP/1.1\r\n\r\n",
        ] {
            let refused = ask(request).await;
            assert!(refused.starts_with("HTTP/1.1 403 Forbidden"), "{request}: {refused}");
            assert!(!refused.contains("<h1>app</h1>"));
        }
        task.abort();
    }
}
