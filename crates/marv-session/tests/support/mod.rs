//! What the process door's sandbox tests share.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use marv_session::processes::{ProcessManager, ProcessManagerOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Skips a test that needs bwrap on a machine without it.
#[macro_export]
macro_rules! require_bwrap {
    () => {
        if !marv_sandbox::sandbox_available() {
            eprintln!("skipped: bubblewrap isn't available here");
            return;
        }
    };
}

/// The relay in place of `marv __relay`.
pub const FIXTURE: &str = env!("CARGO_BIN_EXE_marv-process-fixture");

/// A project, a fake home and a runtime folder, removed on drop.
pub struct Dirs {
    base: PathBuf,
    pub root: PathBuf,
    pub home: PathBuf,
    pub runtime: PathBuf,
}

impl Dirs {
    pub fn new(tag: &str) -> Self {
        let base = std::env::temp_dir().join(format!("marv-proc-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (root, home, runtime) = (base.join("root"), base.join("home"), base.join("rt"));
        for dir in [&root, &home, &runtime] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let base = std::fs::canonicalize(base).unwrap();
        Self { root: base.join("root"), home: base.join("home"), runtime: base.join("rt"), base }
    }
}

impl Drop for Dirs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

pub fn options(dirs: &Dirs, start_wait_ms: u64) -> ProcessManagerOptions {
    marv_tools::relay::set_relay_exe(FIXTURE);
    let mut options = ProcessManagerOptions::new(&dirs.root);
    options.home = Some(dirs.home.clone());
    options.runtime_dir = Some(dirs.runtime.clone());
    options.poll = Duration::from_millis(100);
    options.start_wait = Duration::from_millis(start_wait_ms);
    options
}

pub fn manager(dirs: &Dirs, start_wait_ms: u64) -> ProcessManager {
    ProcessManager::new(options(dirs, start_wait_ms))
}

/// A free port on the host's 127.0.0.1 right now.
pub fn free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port()
}

/// A command that serves `text` on each port, from inside the process's sandbox (the fixture is at /run/marv-bin).
pub fn serve(ports: &[u16], text: &str) -> String {
    let ports: Vec<String> = ports.iter().map(u16::to_string).collect();
    format!("/run/marv-bin serve 127.0.0.1 '{text}' {}", ports.join(" "))
}

/// The body of `GET /` on the host's 127.0.0.1:<port>; an error when nothing answers.
pub async fn get(port: u16) -> std::io::Result<String> {
    let wait = Duration::from_secs(5);
    let mut conn = tokio::time::timeout(wait, tokio::net::TcpStream::connect(("127.0.0.1", port))).await??;
    conn.write_all(b"GET / HTTP/1.0\r\nHost: x\r\n\r\n").await?;
    let mut reply = Vec::new();
    tokio::time::timeout(wait, conn.read_to_end(&mut reply)).await??;
    let reply = String::from_utf8_lossy(&reply).into_owned();
    match reply.split_once("\r\n\r\n") {
        Some((head, body)) if head.starts_with("HTTP/") => Ok(body.to_string()),
        _ => Err(std::io::Error::other(format!("no HTTP reply: {reply:?}"))),
    }
}

pub fn pgrep(marker: &str) -> String {
    let out = std::process::Command::new("pgrep").args(["-f", marker]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

pub async fn until(check: impl Fn() -> bool, within: Duration) -> bool {
    let end = tokio::time::Instant::now() + within;
    while tokio::time::Instant::now() < end {
        if check() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    check()
}

pub fn exists(path: &Path) -> bool {
    path.exists()
}
