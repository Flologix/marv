//! The relay that carries TCP connections across a sandbox's network namespace (Marv #51, the process door).
//!
//! Ports Marv's `src/processes/relay.ts`. bwrap can't join or share part of a network namespace, but a Unix socket in
//! a folder bind-mounted into the sandbox is reachable from both sides.
//!
//! - **Forward mode** (a background process's sandbox, `listen`): listen on a Unix socket; each connection's first
//!   line names a port (`<port>\n`, at most [`MAX_LINE`] bytes, 1024–65535 only: the same rule as the host side,
//!   whatever reaches the socket), and the rest is piped to this sandbox's 127.0.0.1:<port> (then ::1, for a server
//!   bound to localhost). Bytes sent with the port line, before the server answers, aren't lost.
//! - **Reverse mode** (a bash command's sandbox while processes run, `connect`): listen on this sandbox's
//!   127.0.0.1:<port> for each forward and pipe each connection to its socket, so the command's
//!   `curl localhost:<port>` reaches a running process. A port the sandbox can't take isn't relayed; the command runs
//!   anyway.
//! - Then it runs `argv` with its own stdio, passes SIGTERM/SIGINT/SIGHUP on, and exits with its exit code (128 + n
//!   when a signal ended it).
//!
//! marv-rs: a hidden subcommand, `marv __relay <spec>` ([`RELAY_SUBCOMMAND`]), with Marv's own binary bind-mounted
//! read-only at [`RELAY_EXE`] inside the sandbox (it may live under the hidden home), like `__run-checks`. Marv sent
//! the relay's source to `bun -e` instead, which broke in the published bundle (its `require` became a `__require`
//! that didn't exist there): a compiled subcommand has no such trap. It lives here, not in marv-session with the
//! process manager, because bash needs it too and marv-tools can't depend on marv-session.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};

/// `marv __relay <spec>`.
pub const RELAY_SUBCOMMAND: &str = "__relay";
/// Where Marv's binary is mounted in a sandbox that runs the relay.
pub const RELAY_EXE: &str = "/run/marv-bin";
/// Where a process's socket folder is mounted (`/run/marv/fwd.sock` in its own sandbox, `/run/marv/<id>/fwd.sock` in a
/// bash command's).
pub const SOCKET_DIR: &str = "/run/marv";
/// The socket's name in its folder.
pub const SOCKET_NAME: &str = "fwd.sock";
/// The longest port line read before giving up on a connection.
pub const MAX_LINE: usize = 16;
/// Ports below this are never forwarded.
pub const MIN_PORT: u16 = 1024;

/// One port reached through a Unix socket: a connection to the socket sends `<port>\n` first, then the bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayForward {
    pub port: u16,
    pub socket: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelaySpec {
    /// Forward mode: the Unix socket to listen on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<String>,
    /// Reverse mode: a TCP listener on 127.0.0.1:<port> for each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub connect: Vec<RelayForward>,
    /// What to run once the relay listens, with the relay's stdio; the relay exits with its exit code.
    pub argv: Vec<String>,
}

static EXE: OnceLock<PathBuf> = OnceLock::new();

/// The binary that answers `__relay` (mounted at [`RELAY_EXE`]): this one, unless a test set its fixture first.
pub fn relay_exe() -> Option<PathBuf> {
    EXE.get().cloned().or_else(|| std::env::current_exe().ok())
}

/// Tests: the binary that answers `__relay` instead of the test binary itself. The first call wins.
pub fn set_relay_exe(path: impl Into<PathBuf>) {
    let _ = EXE.set(path.into());
}

/// The relay's mount: Marv's binary, read-only at [`RELAY_EXE`].
pub fn exe_bind() -> Option<marv_sandbox::Bind> {
    relay_exe().map(|source| marv_sandbox::Bind { source, dest: RELAY_EXE.into(), writable: false })
}

/// What a sandbox runs for `spec`: `/run/marv-bin __relay <spec>`.
pub fn sandbox_argv(spec: &RelaySpec) -> Vec<OsString> {
    let json = serde_json::to_string(spec).unwrap_or_default();
    vec![RELAY_EXE.into(), RELAY_SUBCOMMAND.into(), json.into()]
}

/// `marv __relay <spec>`: listens, runs the command, exits with its code.
pub fn main(spec: &str) -> ExitCode {
    let Ok(spec) = serde_json::from_str::<RelaySpec>(spec) else {
        eprintln!("marv __relay: the spec isn't valid");
        return ExitCode::from(127);
    };
    if spec.argv.is_empty() {
        eprintln!("marv __relay: nothing to run");
        return ExitCode::from(127);
    }
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("marv __relay: {e}");
            return ExitCode::from(127);
        }
    };
    let code = runtime.block_on(run(spec));
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

async fn run(spec: RelaySpec) -> i32 {
    if let Some(path) = &spec.listen {
        match UnixListener::bind(path) {
            Ok(listener) => {
                tokio::spawn(serve_forward(listener));
            }
            Err(e) => eprintln!("marv __relay: can't listen on {path}: {e}"),
        }
    }
    for forward in &spec.connect {
        // A port this sandbox can't take isn't relayed; the command runs anyway.
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", forward.port)).await {
            tokio::spawn(serve_reverse(listener, forward.port, PathBuf::from(&forward.socket)));
        }
    }
    let mut child = match tokio::process::Command::new(&spec.argv[0]).args(&spec.argv[1..]).spawn() {
        Ok(child) => child,
        Err(e) => {
            eprintln!("marv __relay: couldn't start {}: {e}", spec.argv[0]);
            return 127;
        }
    };
    let pid = child.id().and_then(|p| i32::try_from(p).ok()).map(nix::unistd::Pid::from_raw);
    let signals = [
        (tokio::signal::unix::SignalKind::terminate(), nix::sys::signal::Signal::SIGTERM),
        (tokio::signal::unix::SignalKind::interrupt(), nix::sys::signal::Signal::SIGINT),
        (tokio::signal::unix::SignalKind::hangup(), nix::sys::signal::Signal::SIGHUP),
    ];
    for (kind, signal) in signals {
        if let (Ok(mut stream), Some(pid)) = (tokio::signal::unix::signal(kind), pid) {
            tokio::spawn(async move {
                while stream.recv().await.is_some() {
                    let _ = nix::sys::signal::kill(pid, signal);
                }
            });
        }
    }
    match child.wait().await {
        Ok(status) => exit_code(status),
        Err(_) => 1,
    }
}

/// A process's exit code, 128 + n when signal n ended it.
pub fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(9))
}

/// Forward mode: every connection to the socket, to the port its first line names.
pub async fn serve_forward(listener: UnixListener) {
    loop {
        let Ok((client, _)) = listener.accept().await else { continue };
        tokio::spawn(forward_one(client));
    }
}

/// The port a first line names, when it's a forwardable one.
pub fn port_line(line: &[u8]) -> Option<u16> {
    if line.is_empty() || line.len() > 5 || !line.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let port: u32 = std::str::from_utf8(line).ok()?.parse().ok()?;
    u16::try_from(port).ok().filter(|p| *p >= MIN_PORT)
}

async fn forward_one(mut client: UnixStream) {
    let mut head = Vec::new();
    let mut chunk = [0u8; 4096];
    let newline = loop {
        match client.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => head.extend_from_slice(&chunk[..n]),
        }
        if let Some(i) = head.iter().position(|b| *b == b'\n') {
            break i;
        }
        if head.len() > MAX_LINE {
            return;
        }
    };
    // Only unprivileged ports: the same rule as the host side, whatever reaches the socket.
    let Some(port) = port_line(&head[..newline]) else { return };
    let Some(mut upstream) = connect_local(port).await else { return };
    // What came with the port line, before the server answered, goes first.
    if upstream.write_all(&head[newline + 1..]).await.is_err() {
        return;
    }
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
}

/// This sandbox's 127.0.0.1:<port>, then ::1 (a server bound to localhost may be on either).
async fn connect_local(port: u16) -> Option<TcpStream> {
    for host in ["127.0.0.1", "::1"] {
        if let Ok(stream) = TcpStream::connect((host, port)).await {
            return Some(stream);
        }
    }
    None
}

/// Reverse mode (and the host side of a forwarded port): every connection to the listener, through `socket`.
pub async fn serve_reverse(listener: TcpListener, port: u16, socket: PathBuf) {
    loop {
        let Ok((client, _)) = listener.accept().await else { continue };
        tokio::spawn(through_socket(client, port, socket.clone()));
    }
}

/// One TCP connection, piped to `socket` after its `<port>\n` line.
pub async fn through_socket(mut client: TcpStream, port: u16, socket: impl AsRef<Path>) {
    let Ok(mut upstream) = UnixStream::connect(socket.as_ref()).await else { return };
    if upstream.write_all(format!("{port}\n").as_bytes()).await.is_err() {
        return;
    }
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use std::time::Duration;

    /// A server on the host's 127.0.0.1 (a free port) that answers each connection with what it read, uppercased,
    /// once it has `want` bytes.
    async fn shouting(want: usize) -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (mut conn, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut got = Vec::new();
                    let mut chunk = [0u8; 256];
                    while got.len() < want {
                        match conn.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => got.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let _ = conn.write_all(&got.to_ascii_uppercase()).await;
                });
            }
        });
        port
    }

    /// Writes `chunks` to the Unix socket (each its own write, a moment apart) and collects everything until it closes.
    async fn exchange(socket: &Path, chunks: &[&[u8]]) -> String {
        let mut conn = UnixStream::connect(socket).await.unwrap();
        for chunk in chunks {
            let _ = conn.write_all(chunk).await;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let mut got = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), conn.read_to_end(&mut got)).await;
        String::from_utf8_lossy(&got).into_owned()
    }

    #[test]
    fn port_lines_are_1024_to_65535_in_digits_only() {
        assert_eq!(port_line(b"4789"), Some(4789));
        assert_eq!(port_line(b"65535"), Some(65535));
        for bad in [&b"80"[..], b"0", b"65536", b"abc", b"4789x", b"", b"004789", b" 4789"] {
            assert_eq!(port_line(bad), None, "{}", String::from_utf8_lossy(bad));
        }
    }

    #[tokio::test]
    async fn forward_mode_pipes_a_socket_connection_to_the_port_keeping_bytes_sent_before_it_connects() {
        let dir = TempDir::new("relay-fwd");
        let socket = dir.path().join(SOCKET_NAME);
        tokio::spawn(serve_forward(UnixListener::bind(&socket).unwrap()));
        let port = shouting(9).await;
        // The port line and part of the request in one write, then more in a second one: all of it reaches the server.
        let reply = exchange(&socket, &[format!("{port}\nhello").as_bytes(), b" you"]).await;
        assert_eq!(reply, "HELLO YOU");
    }

    #[tokio::test]
    async fn a_port_line_that_isnt_1024_to_65535_or_a_number_closes_the_connection() {
        let dir = TempDir::new("relay-bad");
        let socket = dir.path().join(SOCKET_NAME);
        tokio::spawn(serve_forward(UnixListener::bind(&socket).unwrap()));
        let port = shouting(2).await;
        assert_eq!(exchange(&socket, &[format!("{port}\nok").as_bytes()]).await, "OK"); // control
        for line in ["80", "0", "65536", "abc", &format!("{port}x")] {
            assert_eq!(exchange(&socket, &[format!("{line}\nok").as_bytes()]).await, "", "{line}");
        }
        // A first line that never ends.
        assert_eq!(exchange(&socket, &["9".repeat(40).as_bytes()]).await, "");
    }

    #[tokio::test]
    async fn reverse_mode_sends_the_port_line_then_the_bytes_to_the_socket() {
        let dir = TempDir::new("relay-rev");
        let socket = dir.path().join(SOCKET_NAME);
        let unix = UnixListener::bind(&socket).unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut conn, _) = unix.accept().await.unwrap();
            let mut got = vec![0u8; 9];
            conn.read_exact(&mut got).await.unwrap();
            let _ = tx.send(String::from_utf8_lossy(&got).into_owned());
            conn.write_all(b"host says hi").await.unwrap();
        });
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(serve_reverse(listener, port, socket));
        let mut client = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        client.write_all(b"GET").await.unwrap();
        let mut reply = String::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), client.read_to_string(&mut reply)).await;
        assert_eq!(rx.await.unwrap(), format!("{port}\nGET"));
        assert_eq!(reply, "host says hi");
    }

    #[test]
    fn the_sandbox_runs_the_relay_from_its_mount_with_the_spec_as_json() {
        let spec = RelaySpec { listen: Some("/run/marv/fwd.sock".into()), connect: vec![], argv: vec!["bash".into()] };
        let argv = sandbox_argv(&spec);
        assert_eq!(argv[..2], [OsString::from("/run/marv-bin"), OsString::from("__relay")]);
        assert_eq!(serde_json::from_str::<RelaySpec>(argv[2].to_str().unwrap()).unwrap(), spec);
        assert_eq!(argv[2], OsString::from(r#"{"listen":"/run/marv/fwd.sock","argv":["bash"]}"#));
    }
}
