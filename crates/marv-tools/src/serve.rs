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
//! - One request per connection (`Connection: close`), headers read up to [`MAX_HEAD`] bytes within [`HEAD_TIMEOUT`].

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// `marv __serve <folder> <port>`.
pub const SERVE_SUBCOMMAND: &str = "__serve";
/// The longest request head read.
pub const MAX_HEAD: usize = 16 * 1024;
/// How long a client has to send its request head.
pub const HEAD_TIMEOUT: Duration = Duration::from_secs(10);

/// One answer: status line text, content type, body.
#[derive(Debug, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    pub reason: &'static str,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl Reply {
    fn text(status: u16, reason: &'static str) -> Self {
        Self { status, reason, content_type: "text/plain; charset=utf-8", body: format!("{status} {reason}\n").into_bytes() }
    }
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

/// A file under `root` (already canonical), after following symlinks: `None` when it isn't a file there.
fn file_under(root: &Path, relative: &[String]) -> Option<PathBuf> {
    let mut path = root.to_path_buf();
    path.extend(relative);
    let mut real = std::fs::canonicalize(&path).ok()?;
    if real.is_dir() {
        real = std::fs::canonicalize(real.join("index.html")).ok()?;
    }
    (real.starts_with(root) && real.is_file()).then_some(real)
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
    match std::fs::read(&file) {
        Ok(body) => Reply { status: 200, reason: "OK", content_type: content_type(&file), body },
        Err(_) => Reply::text(404, "Not Found"),
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
    let reply = if target.is_empty() { Reply::text(400, "Bad Request") } else { respond(&root, method, target) };
    let mut out = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        reply.status,
        reply.reason,
        reply.content_type,
        reply.body.len()
    )
    .into_bytes();
    if method != "HEAD" {
        out.extend_from_slice(&reply.body);
    }
    let _ = stream.write_all(&out).await;
    let _ = stream.shutdown().await;
}

/// Serves `root` (canonical) on `listener` until the task ends.
pub async fn serve(listener: TcpListener, root: PathBuf) {
    loop {
        let Ok((stream, _)) = listener.accept().await else { continue };
        tokio::spawn(handle(stream, root.clone()));
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
        assert_eq!(get(&dir, "/").body, b"<h1>app</h1>");
        assert_eq!(get(&dir, "/docs/").body, b"<h1>docs</h1>");
        assert_eq!(get(&dir, "/docs").body, b"<h1>docs</h1>");
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
        assert_eq!(get(&dir, "/assets/app.js?v=3#x").body, b"console.log(1)", "the query is dropped");
        assert_eq!(get(&dir, "/assets/a%70p.js").status, 200, "percent-decoded");
    }

    #[test]
    fn a_route_without_an_extension_falls_back_to_the_index_and_a_missing_file_is_a_404() {
        let dir = site();
        let route = get(&dir, "/users/42/settings");
        assert_eq!((route.status, route.body.as_slice()), (200, b"<h1>app</h1>".as_slice()));
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
            assert_ne!(get(&dir, target).body, b"SECRET=1", "{target}");
            let reply = get(&dir, target);
            assert!(reply.status == 404 || reply.body == b"<h1>app</h1>", "{target}: {reply:?}");
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
        let head = ask("HEAD / HTTP/1.1\r\n\r\n").await;
        assert!(head.starts_with("HTTP/1.1 200 OK") && head.ends_with("\r\n\r\n"), "{head}");
        let missing = ask("GET /x.png HTTP/1.1\r\n\r\n").await;
        assert!(missing.starts_with("HTTP/1.1 404 Not Found"), "{missing}");
        let bad = ask("\r\n\r\n").await;
        assert!(bad.starts_with("HTTP/1.1 400"), "{bad}");
        task.abort();
    }
}
