//! What the process door's tests run in place of `marv` (they can't run the real binary from a library's tests): the
//! relay, a tiny server to forward, and a parent that starts a process and waits to be killed. Used by
//! `src/bin/marv-process-fixture.rs` and the guards' fixture. Not part of Marv.

use std::process::ExitCode;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// `<fixture> __relay <spec>` | `serve <host> <text> <port>…` | `parent <root> <runtime dir> <command>`.
pub fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some(marv_tools::relay::RELAY_SUBCOMMAND) => marv_tools::relay::main(args.get(1).map_or("", String::as_str)),
        Some("serve") if args.len() >= 4 => {
            let ports: Vec<u16> = args[3..].iter().filter_map(|p| p.parse().ok()).collect();
            runtime().block_on(serve(&args[1], &args[2], &ports));
            ExitCode::SUCCESS
        }
        Some("parent") if args.len() == 4 => {
            runtime().block_on(parent(&args[1], &args[2], &args[3]));
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("usage: __relay <spec> | serve <host> <text> <port>… | parent <root> <runtime dir> <command>");
            ExitCode::from(2)
        }
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a runtime")
}

/// Answers every HTTP request on each port with `text`, forever.
pub async fn serve(host: &str, text: &str, ports: &[u16]) {
    for &port in ports {
        let listener = tokio::net::TcpListener::bind((host, port)).await.expect("the port");
        let text = text.to_string();
        tokio::spawn(async move {
            loop {
                let Ok((mut conn, _)) = listener.accept().await else { continue };
                let text = text.clone();
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut chunk = [0u8; 1024];
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                        match tokio::time::timeout(Duration::from_secs(2), conn.read(&mut chunk)).await {
                            Ok(Ok(n)) if n > 0 => head.extend_from_slice(&chunk[..n]),
                            _ => break,
                        }
                    }
                    let reply = format!("HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n{text}", text.len());
                    let _ = conn.write_all(reply.as_bytes()).await;
                    let _ = conn.shutdown().await;
                });
            }
        });
    }
    println!("listening");
    std::future::pending::<()>().await;
}

/// Starts one process with a [`super::ProcessManager`] and then just waits, so a test can SIGKILL this process (Marv
/// dying hard, with no chance to clean up) and check that nothing it started survives.
pub async fn parent(root: &str, runtime_dir: &str, command: &str) {
    let mut options = super::ProcessManagerOptions::new(root);
    options.runtime_dir = Some(runtime_dir.into());
    options.start_wait = Duration::from_millis(300);
    let manager = super::ProcessManager::new(options);
    if let Err(e) = manager.start(command, Some("survivor"), false).await {
        eprintln!("{e}");
        return;
    }
    println!("started");
    std::future::pending::<()>().await;
}
