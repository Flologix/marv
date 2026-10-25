//! SIGHUP and SIGTERM for the TUI and `marv -p` (#196). Without a handler either one ended Marv at once, as SIGKILL
//! would: a closed terminal (SIGHUP) or a `kill` skipped the way out, so a checkpoint prune's `git gc` (a process
//! group of its own) outlived Marv, the terminal wasn't restored, and the session wasn't flushed.
//!
//! [`Quit::listen`] catches both. The TUI leaves as `/exit` does (`App::with_quit`), `marv -p` stops its turn and
//! closes the session; `main` gives that [`DEADLINE`], then gives up on it, and ends the runtime within
//! [`RUNTIME_SHUTDOWN`] (its tasks dropped: a git still running is killed with its group). The exit code is
//! 128 + the signal's number, as a shell reports a process that signal ended.
//!
//! - **An ignored SIGHUP stays ignored** (`nohup marv -p …`): catching it would end a run its caller meant to outlive
//!   the terminal. The disposition is read without `unsafe`: `/proc/self/status`'s `SigIgn` on Linux, `ps -o
//!   sigignore=` elsewhere (macOS).
//! - **A hard bound** ([`backstop`]): `main`'s deadline needs its future polled, and a draw blocked writing to a
//!   terminal that stopped reading holds that thread. So the first signal also starts a thread of its own which, after
//!   [`DEADLINE`] + [`RUNTIME_SHUTDOWN`] + [`BACKSTOP_MARGIN`], or at once on a second signal, kills what Marv started
//!   (background processes, gits, HTML converters) and exits with 128 + n. Through `std::process::exit`, not a reset
//!   to SIG_DFL and a re-raise (which would show a parent a real signal death): changing a disposition needs
//!   `unsafe`, which marv-rs forbids, and 128 + n is what Marv reports for every signal it handles.

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

/// From the signal to giving up on the way out. The TUI's steps after a signal add up to about 9.5 s at most (the
/// restore 0.5, the flush 2, the title 0.5, background processes 2.5, MCP 2, Herdr 1, the radio's goodbye 1).
pub const DEADLINE: Duration = Duration::from_secs(10);
/// What's left on the runtime after a signal (a blocking write to a stuck terminal) gets this long.
pub const RUNTIME_SHUTDOWN: Duration = Duration::from_secs(1);
/// The backstop waits this much longer than `main` would.
pub const BACKSTOP_MARGIN: Duration = Duration::from_secs(1);

/// Whether SIGHUP or SIGTERM arrived, and which.
#[derive(Clone, Default)]
pub struct Quit {
    token: CancellationToken,
    signal: Arc<AtomicI32>,
    /// Tells the backstop a second signal came.
    again: Arc<Mutex<Option<std::sync::mpsc::Sender<()>>>>,
    /// Whether the first signal starts the [`backstop`] (not in this file's tests: it would end the test process).
    backstop: bool,
}

impl Quit {
    /// Catches SIGTERM, and SIGHUP unless it's ignored, from now on (in a Tokio runtime). The first one counts; a
    /// second forces the exit ([`backstop`]).
    pub fn listen() -> Self {
        Self::listen_with(true)
    }

    fn listen_with(backstop: bool) -> Self {
        let quit = Self { backstop, ..Self::default() };
        for kind in [tokio::signal::unix::SignalKind::hangup(), tokio::signal::unix::SignalKind::terminate()] {
            if ignored(kind.as_raw_value()) {
                continue;
            }
            let Ok(mut stream) = tokio::signal::unix::signal(kind) else { continue };
            let quit = quit.clone();
            tokio::spawn(async move {
                while stream.recv().await.is_some() {
                    quit.arrive(kind.as_raw_value());
                }
            });
        }
        quit
    }

    fn arrive(&self, signal: i32) {
        if self.signal.compare_exchange(0, signal, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
            if self.backstop {
                let (again, wait) = std::sync::mpsc::channel();
                *self.again.lock().unwrap_or_else(|e| e.into_inner()) = Some(again);
                let _ = std::thread::Builder::new().name("marv-quit".into()).spawn(move || backstop(&wait, signal));
            }
            self.token.cancel();
        } else if let Some(again) = &*self.again.lock().unwrap_or_else(|e| e.into_inner()) {
            let _ = again.send(());
        }
    }

    /// Completes once a signal arrived.
    pub async fn arrived(&self) {
        self.token.cancelled().await;
    }

    /// The signal's number, once one arrived.
    pub fn signal(&self) -> Option<i32> {
        Some(self.signal.load(Ordering::SeqCst)).filter(|&n| n != 0)
    }

    /// The exit code for a Marv a signal ended: 128 + its number.
    pub fn exit_code(&self) -> Option<i32> {
        self.signal().map(|n| 128 + n)
    }

    /// Completes [`DEADLINE`] after a signal: the way out took too long.
    pub async fn expired(&self) {
        self.arrived().await;
        tokio::time::sleep(DEADLINE).await;
    }
}

/// Waits for a second signal or the backstop's deadline, then kills what Marv started and exits with 128 + `signal`.
/// A Marv whose way out finished first has exited already, this thread with it.
fn backstop(again: &std::sync::mpsc::Receiver<()>, signal: i32) {
    let until = Instant::now() + DEADLINE + RUNTIME_SHUTDOWN + BACKSTOP_MARGIN;
    if let Err(std::sync::mpsc::RecvTimeoutError::Disconnected) = again.recv_timeout(until - Instant::now()) {
        // The listeners went with the runtime's shutdown: only the deadline is left.
        std::thread::sleep(until.saturating_duration_since(Instant::now()));
    }
    marv_session::git::kill_running();
    marv_session::processes::kill_running();
    marv_tools::web::convert::kill_running();
    std::process::exit(128 + signal);
}

/// Whether `signal` is ignored in this process (inherited as SIG_IGN, as `nohup` leaves SIGHUP). Unknown: not ignored.
fn ignored(signal: i32) -> bool {
    let mask = match std::fs::read_to_string("/proc/self/status") {
        Ok(status) => proc_ignored(&status),
        Err(_) => ps_ignored(std::process::id()),
    };
    mask.is_some_and(|mask| (1..=64).contains(&signal) && mask >> (signal - 1) & 1 == 1)
}

/// The ignored-signal mask from a `/proc/<pid>/status` (`SigIgn:`, hex; bit n-1 for signal n).
fn proc_ignored(status: &str) -> Option<u64> {
    let line = status.lines().find_map(|l| l.strip_prefix("SigIgn:"))?;
    u64::from_str_radix(line.trim(), 16).ok()
}

/// The ignored-signal mask of process `pid`, from `ps -o sigignore=` (hex on Linux and macOS).
fn ps_ignored(pid: u32) -> Option<u64> {
    let out = std::process::Command::new("ps")
        .args(["-o", "sigignore=", "-p", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let text = text.trim();
    u64::from_str_radix(text.strip_prefix("0x").unwrap_or(text), 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_sigterm_is_caught_and_gives_143() {
        let quit = Quit::listen_with(false);
        assert_eq!(quit.signal(), None);
        nix::sys::signal::raise(nix::sys::signal::Signal::SIGTERM).unwrap();
        tokio::time::timeout(Duration::from_secs(5), quit.arrived()).await.expect("caught");
        assert_eq!(quit.exit_code(), Some(143));
    }

    #[test]
    fn the_ignored_mask_is_read_from_proc_status() {
        let status = "Name:\tmarv\nSigBlk:\t0000000000000000\nSigIgn:\t0000000000001001\nSigCgt:\t0000000000000000\n";
        let mask = proc_ignored(status).unwrap();
        assert_eq!(mask & 1, 1, "SIGHUP (1)");
        assert_eq!(mask >> 12 & 1, 1, "SIGPIPE (13)");
        assert_eq!(mask >> 14 & 1, 0, "SIGTERM (15)");
        assert_eq!(proc_ignored("Name:\tmarv\n"), None);
    }

    // The path macOS takes (no /proc), on whatever runs the tests: a shell that ignores SIGHUP, as nohup leaves it.
    #[test]
    fn ps_reports_an_ignored_sighup() {
        let mut shell = std::process::Command::new("sh").args(["-c", "trap '' HUP; exec sleep 30"]).spawn().unwrap();
        // The trap is set before exec: wait until sleep runs (ps then names it).
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut mask = None;
        while Instant::now() < deadline {
            let comm = std::process::Command::new("ps").args(["-o", "comm=", "-p", &shell.id().to_string()]).output();
            if comm.is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("sleep")) {
                mask = ps_ignored(shell.id());
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = shell.kill();
        let _ = shell.wait();
        let mask = mask.expect("ps gave a mask");
        assert_eq!(mask & 1, 1, "SIGHUP ignored: {mask:x}");
        assert_eq!(mask >> 14 & 1, 0, "SIGTERM not: {mask:x}");
    }
}
