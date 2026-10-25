//! SIGHUP and SIGTERM for the TUI and `marv -p` (#196). Without a handler either one ended Marv at once, as SIGKILL
//! would: a closed terminal (SIGHUP) or a `kill` skipped the way out, so a checkpoint prune's `git gc` (a process
//! group of its own) outlived Marv, the terminal wasn't restored, and the session wasn't flushed.
//!
//! [`Quit::listen`] catches both. The TUI leaves as `/exit` does (`App::with_quit`), `marv -p` stops its turn and
//! closes the session; `main` gives that [`DEADLINE`], then gives up on it, and ends the runtime within
//! [`RUNTIME_SHUTDOWN`] (its tasks dropped: a git still running is killed with its group). The exit code is
//! 128 + the signal's number, as a shell reports a process that signal ended.
//!
//! - **An ignored signal stays ignored** (`nohup marv -p …` leaves SIGHUP ignored): catching it would end a run its
//!   caller meant to outlive the terminal. The disposition is read once at the start with `sigaction(sig, NULL, &old)`
//!   ([`ignored`]), Marv's one `unsafe` call (the workspace lint is `deny`, and only this function allows it, which a
//!   test checks): there's no safe query in nix, `/proc` is Linux-only, and macOS's `ps` has no `sigignore` column.
//! - **A hard bound** ([`backstop`]): `main`'s deadline needs its future polled, and a draw blocked writing to a
//!   terminal that stopped reading holds that thread. So the first signal also starts a thread of its own which, after
//!   [`DEADLINE`] + [`RUNTIME_SHUTDOWN`] + [`BACKSTOP_MARGIN`], or at once on a second signal, kills what Marv started
//!   (background processes, gits, HTML converters), gives the terminal's restore 200 ms (bounded: the thread stuck
//!   writing may hold stdout) and exits with 128 + n. Through `std::process::exit`, not a reset to SIG_DFL and a
//!   re-raise (which would show a parent a real signal death): that would be more `unsafe`, and 128 + n is what Marv
//!   reports on every path it handles a signal.

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
    marv_tui::render_options::within(Duration::from_millis(200), marv_tui::render_options::restore_if_entered);
    std::process::exit(128 + signal);
}

/// Whether `signal` is ignored in this process (inherited as SIG_IGN, as `nohup` leaves SIGHUP). A query that fails
/// (it can't, for these signals) counts as not ignored.
#[allow(unsafe_code)]
fn ignored(signal: i32) -> bool {
    use nix::libc;
    let mut old = std::mem::MaybeUninit::<libc::sigaction>::zeroed();
    // SAFETY: with a null new action, sigaction(2) changes nothing and only writes the current one into `old`, which is
    // valid and writable for a whole `sigaction`. `old` starts zeroed (all its fields are integers, a mask or a
    // function address, for which zero is valid), so it's initialized whether or not the call wrote it.
    let (status, old) = unsafe { (libc::sigaction(signal, std::ptr::null(), old.as_mut_ptr()), old.assume_init()) };
    status == 0 && old.sa_sigaction == libc::SIG_IGN
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

    // Under the test harness neither is ignored (and nothing here may change that: it's the one process).
    #[test]
    fn hup_and_term_are_not_ignored_by_default() {
        assert!(!ignored(nix::libc::SIGHUP));
        assert!(!ignored(nix::libc::SIGTERM));
    }

    // The workspace lint is `deny`, not `forbid`, only so that `ignored` can allow its one call: any other
    // `allow(unsafe_code)` in the workspace's sources fails here.
    #[test]
    fn ignored_is_the_only_unsafe_code() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut found = Vec::new();
        let mut dirs = vec![root.join("crates"), root.join("app")];
        while let Some(dir) = dirs.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name();
                if path.is_dir() && name != "target" && name != "node_modules" {
                    dirs.push(path);
                } else if path.extension().is_some_and(|e| e == "rs")
                    && std::fs::read_to_string(&path).is_ok_and(|t| t.contains(concat!("allow(", "unsafe_code)")))
                {
                    found.push(path.strip_prefix(&root).unwrap().display().to_string());
                }
            }
        }
        assert_eq!(found, ["crates/marv-cli/src/signals.rs"]);
        let this = std::fs::read_to_string(root.join("crates/marv-cli/src/signals.rs")).unwrap();
        assert_eq!(this.matches(concat!("#[allow(", "unsafe_code)]")).count(), 1, "one function");
        assert_eq!(this.matches(concat!("unsafe", " {")).count(), 1, "one block");
    }
}
