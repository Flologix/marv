//! SIGHUP and SIGTERM for the TUI and `marv -p` (#196). Without a handler either one ended Marv at once, as SIGKILL
//! would: a closed terminal (SIGHUP) or a `kill` skipped the way out, so a checkpoint prune's `git gc` (a process
//! group of its own) outlived Marv, the terminal wasn't restored, and the session wasn't flushed.
//!
//! [`Quit::listen`] catches both. The TUI leaves as `/exit` does (`App::with_quit`), `marv -p` stops its turn and
//! closes the session; `main` gives that [`DEADLINE`], then gives up on it, and ends the runtime within
//! [`RUNTIME_SHUTDOWN`] (its tasks dropped: a git still running is killed with its group). The exit code is
//! 128 + the signal's number, as a shell reports a process that signal ended.

use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

/// From the signal to giving up on the way out.
pub const DEADLINE: Duration = Duration::from_secs(5);
/// What's left on the runtime after a signal (a blocking write to a stuck terminal) gets this long.
pub const RUNTIME_SHUTDOWN: Duration = Duration::from_secs(1);

/// Whether SIGHUP or SIGTERM arrived, and which.
#[derive(Clone, Default)]
pub struct Quit {
    token: CancellationToken,
    signal: Arc<AtomicI32>,
}

impl Quit {
    /// Catches SIGHUP and SIGTERM from now on (in a Tokio runtime). Each one counts once; a second does nothing more.
    pub fn listen() -> Self {
        let quit = Self::default();
        for kind in [tokio::signal::unix::SignalKind::hangup(), tokio::signal::unix::SignalKind::terminate()] {
            let Ok(mut stream) = tokio::signal::unix::signal(kind) else { continue };
            let quit = quit.clone();
            tokio::spawn(async move {
                if stream.recv().await.is_some() {
                    let _ = quit.signal.compare_exchange(0, kind.as_raw_value(), Ordering::SeqCst, Ordering::SeqCst);
                    quit.token.cancel();
                }
            });
        }
        quit
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_sigterm_is_caught_and_gives_143() {
        let quit = Quit::listen();
        assert_eq!(quit.signal(), None);
        nix::sys::signal::raise(nix::sys::signal::Signal::SIGTERM).unwrap();
        tokio::time::timeout(Duration::from_secs(5), quit.arrived()).await.expect("caught");
        assert_eq!(quit.exit_code(), Some(143));
    }
}
