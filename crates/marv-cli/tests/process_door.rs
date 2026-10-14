//! The process door end to end (#24, Marv #51), with the real `marv` binary as its own relay (`marv __relay`) and a
//! scripted fake model (the oracle's server): `marv -p --approve process` starts a dev server with the process tool,
//! a sandboxed `bash` command curls it on localhost, the test reaches it on the host's 127.0.0.1 while Marv runs, and
//! nothing is left running after Marv exits. Skipped where bwrap doesn't work, or without python3 (the dev server).

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use marv_oracle::script::Reply;
use marv_oracle::server::FakeServer;
use serde_json::{Value, json};

fn bwrap_works() -> bool {
    Command::new("bwrap")
        .args(["--ro-bind", "/", "/", "--dev", "/dev", "--unshare-net", "true"])
        .status()
        .is_ok_and(|s| s.success())
}

fn reply(v: Value) -> Reply {
    serde_json::from_value(v).unwrap()
}
fn call(id: &str, name: &str, args: Value) -> Reply {
    reply(json!({"tool_calls": [{"id": id, "name": name, "arguments": args}]}))
}

fn free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port()
}

/// `GET /hello.txt` on the host's 127.0.0.1:<port>, as a browser would; None when nothing answers.
fn get(port: u16) -> Option<String> {
    use std::io::{Read, Write};
    let mut conn = std::net::TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_secs(2)).ok()?;
    conn.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    conn.write_all(b"GET /hello.txt HTTP/1.0\r\n\r\n").ok()?;
    let mut reply = String::new();
    conn.read_to_string(&mut reply).ok()?;
    Some(reply)
}

struct Dir(PathBuf);
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn marv_p_starts_a_dev_server_bash_curls_it_the_host_reaches_it_and_nothing_outlives_marv() {
    if !bwrap_works() || Command::new("python3").arg("--version").output().is_err() {
        eprintln!("skipped: needs bubblewrap and python3");
        return;
    }
    let dir = std::env::temp_dir().join(format!("marv-process-door-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["proj", "home", "config", "run"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    let dir = Dir(dir.canonicalize().unwrap());
    let project = dir.0.join("proj");
    std::fs::write(project.join("hello.txt"), "HELLO-FROM-PROJECT").unwrap();
    let port = free_port();
    let marker = format!("marv-door-e2e-{}", std::process::id());
    // The server prints the marker in its command line, so pgrep finds it; the model waits in bash until the test has
    // checked the host side.
    let serve = format!("python3 -m http.server {port} --bind 127.0.0.1 # {marker}");
    let wait = "for i in $(seq 1 300); do [ -e checked ] && break; sleep 0.1; done; echo waited";
    let server = FakeServer::start(
        vec![
            call("c1", "process", json!({"action": "start", "command": serve, "name": "web"})),
            call("c2", "bash", json!({"command": format!("curl -s --max-time 5 localhost:{port}/hello.txt")})),
            call("c3", "bash", json!({"command": wait})),
            reply(json!({"text": "Done."})),
        ],
        Vec::new(),
    )
    .unwrap();
    let settings = Value::Object(marv_oracle::runner::default_config(&server.base_url()));
    std::fs::write(dir.0.join("config/config.json"), settings.to_string()).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_marv"))
        .args(["-p", "start the dev server", "--approve", "process", "--output", "json"])
        .current_dir(&project)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", dir.0.join("home"))
        .env("MARV_CONFIG_DIR", dir.0.join("config"))
        .env("XDG_RUNTIME_DIR", dir.0.join("run"))
        // No GitHub probe (`gh auth status`), which this test doesn't need: where `gh` is a wrapper that installs gh
        // on first use (mise), the probe's timeout kills the wrapper and not the installer, which keeps writing
        // ~40 MB into the fake home after it was removed (#89).
        .env("MARV_GITHUB", "off")
        .env("TERM", "dumb")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = Guard(Some(child));

    // While Marv runs: the user's browser reaches the server on their 127.0.0.1.
    let deadline = Instant::now() + Duration::from_secs(30);
    let page = loop {
        if let Some(page) = get(port).filter(|p| p.contains("HELLO-FROM-PROJECT")) {
            break page;
        }
        assert!(Instant::now() < deadline, "the forwarded port never answered");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(page.starts_with("HTTP/1.0 200"), "{page}");
    std::fs::write(project.join("checked"), "").unwrap();

    let out = child.0.take().unwrap().wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
    // The model read the forwarded address, and its sandboxed curl reached the page.
    let bodies: Vec<String> = server.requests().into_iter().filter(|r| r.method == "POST").map(|r| r.body).collect();
    let seen = bodies.last().unwrap();
    assert!(seen.contains(&format!("Forwarded to the user's machine: http://localhost:{port}")), "{seen}");
    // (The request body is JSON: its newlines are escaped.)
    assert!(seen.contains("HELLO-FROM-PROJECT\\n\\n[exit code 0]"), "{seen}");
    // Nothing outlives Marv.
    let left = Command::new("pgrep").args(["-f", &marker]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&left.stdout).trim(), "");
    assert!(get(port).is_none());
}

/// Kills the child if an assertion fails midway.
struct Guard(Option<std::process::Child>);
impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
