//! Retry helpers shared by both adapters. Ports `src/provider/retry.ts` (Marv #36).
//!
//! Both [`crate::openai_compat`] and [`crate::ollama`] retry when a connection drops (before or
//! mid-stream) or the server returns a temporary error. These are the shared pieces:
//! - [`MAX_ATTEMPTS`] = 3 (the original plus 2 retries), [`BACKOFFS`] = 1 s, 4 s, 10 s.
//! - Retriable statuses: 429, 502, 503, 504. **Not** 500 (Ollama's unparsable tool call is a 500
//!   the Ollama adapter retries once by itself; other 500s, like out of memory, aren't retried).
//! - `Retry-After` is honoured only as whole positive seconds, capped at 30; HTTP dates, decimals
//!   and junk fall back to the backoff.
//! - Esc (cancel) stops a wait at once, and the adapter then ends without an error.
//! - Tests don't wait in real time: Marv scales every wait with a global `retryTiming.scale`;
//!   here a [`RetryTiming`] is passed in through the adapter's options instead (no global state).
//!
//! Planned (Marv #64, not in Marv yet): jitter, more attempts for 429/overloaded (about 6, capped
//! by total wait), `Retry-After` honoured up to a few minutes, and **continue after a dropped
//! stream** instead of starting over ([`drop_recovery`]).

use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use tokio_util::sync::CancellationToken;

/// Maximum number of attempts (including the original one).
pub const MAX_ATTEMPTS: u32 = 3;
/// Backoff delays in seconds: 1 s, 4 s, 10 s.
pub const BACKOFFS: [u64; 3] = [1, 4, 10];
/// The most a `Retry-After` header can make Marv wait, in seconds.
pub const MAX_RETRY_AFTER: u64 = 30;

/// Multiplies every retry wait (backoffs and Retry-After). Tests set it small.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RetryTiming {
    pub scale: f64,
}

impl Default for RetryTiming {
    fn default() -> Self {
        Self { scale: 1.0 }
    }
}

impl RetryTiming {
    /// The real wait for `seconds`, after scaling.
    pub fn wait(&self, seconds: u64) -> Duration {
        Duration::from_secs_f64(seconds as f64 * self.scale)
    }
}

/// Whether an HTTP status is worth retrying: a temporary server error (502, 503, 504) or a rate
/// limit (429).
pub fn retriable_http_status(status: u16) -> bool {
    matches!(status, 429 | 502 | 503 | 504)
}

static NETWORK_ERROR: LazyLock<[Regex; 10]> = LazyLock::new(|| {
    [
        Regex::new(r"(?i)connection (refused|reset|closed|dropped)").unwrap(),
        // Case-sensitive in Marv too.
        Regex::new(r"socket").unwrap(),
        Regex::new(r"(?i)timed?[-\s]?out").unwrap(),
        Regex::new(r"(?i)dns|resolve|eai_again|enotfound").unwrap(),
        Regex::new(r"(?i)name or service not known").unwrap(),
        Regex::new(r"(?i)could not resolve").unwrap(),
        Regex::new(r"(?i)transport|econnrefused|econnreset|enetunreach|ehostunreach").unwrap(),
        // Bun's fetch errors (kept: the oracle replays Marv's messages).
        Regex::new(r"(?i)unable to connect").unwrap(),
        Regex::new(r"(?i)connection failed").unwrap(),
        Regex::new(r"(?i)the socket connection was closed").unwrap(),
    ]
});

/// Whether an error message looks like a network-level failure that retrying may fix: connection
/// refused or reset, socket closed, DNS failure, timeout.
///
/// Port note: with reqwest, prefer `err.is_connect() || err.is_timeout() || err.is_body()` and
/// fall back to this text match for the error chain's messages (hyper/rustls wording differs
/// from Bun's).
pub fn is_network_error(message: &str) -> bool {
    NETWORK_ERROR.iter().any(|re| re.is_match(message))
}

/// Sleeps for `wait`, but returns early when `cancel` fires. Returns `true` if cancelled (the
/// caller should return without an error), `false` otherwise.
pub async fn sleep_abortable(wait: Duration, cancel: &CancellationToken) -> bool {
    if cancel.is_cancelled() {
        return true;
    }
    tokio::select! {
        _ = cancel.cancelled() => true,
        _ = tokio::time::sleep(wait) => false,
    }
}

/// Parses a `Retry-After` header value. Returns a whole number of seconds (capped at 30) if the
/// value is a positive integer; `None` for HTTP dates, decimals or junk.
pub fn parse_retry_after(value: Option<&str>) -> Option<u64> {
    let trimmed = value?.trim();
    // Marv checks `String(parseInt(v)) === v`: digits only, no sign, no leading zeros.
    if trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_digit()) || trimmed.starts_with('0') {
        return None;
    }
    let seconds: u64 = trimmed.parse().unwrap_or(u64::MAX);
    Some(seconds.min(MAX_RETRY_AFTER))
}

/// What to do when a stream drops mid-reply (Marv #64, planned).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropRecovery {
    /// Redo the whole request; the loop discards the partial reply (today's behaviour).
    Restart,
    /// Keep the partial text as the assistant turn and send a continue request with a Marv note.
    Continue,
}

/// Marv #64: a drop **after** text arrived and with **no tool call in progress** continues from
/// the partial text; a half tool call can't be continued, so anything else restarts.
pub fn drop_recovery(text_received: bool, tool_call_in_progress: bool) -> DropRecovery {
    if text_received && !tool_call_in_progress { DropRecovery::Continue } else { DropRecovery::Restart }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ported from tests/retry.test.ts.

    #[test]
    fn retry_after_whole_seconds_capped_at_30_dates_and_junk_fall_back() {
        assert_eq!(parse_retry_after(Some("5")), Some(5));
        assert_eq!(parse_retry_after(Some("3600")), Some(30));
        assert_eq!(parse_retry_after(Some("Wed, 21 Oct 2026 07:28:00 GMT")), None);
        assert_eq!(parse_retry_after(Some("1.5")), None);
        assert_eq!(parse_retry_after(Some("0")), None);
        assert_eq!(parse_retry_after(None), None);
    }

    #[test]
    fn which_failures_are_worth_retrying() {
        assert!([429, 502, 503, 504].into_iter().all(retriable_http_status));
        assert!(![400, 401, 404, 500].into_iter().any(retriable_http_status));
        assert!(is_network_error("The socket connection was closed unexpectedly."));
        assert!(is_network_error("Unable to connect. Is the computer able to access the url?"));
        assert!(!is_network_error("Invalid API key"));
    }

    #[tokio::test]
    async fn an_aborted_wait_ends_at_once_and_says_so() {
        let cancel = CancellationToken::new();
        let start = std::time::Instant::now();
        let waiting = sleep_abortable(Duration::from_secs(10), &cancel);
        cancel.cancel();
        assert!(waiting.await);
        assert!(start.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn timing_scales_waits() {
        assert_eq!(RetryTiming { scale: 0.001 }.wait(BACKOFFS[2]), Duration::from_millis(10));
    }

    #[test]
    fn a_drop_continues_only_after_text_and_outside_a_tool_call() {
        assert_eq!(drop_recovery(true, false), DropRecovery::Continue);
        assert_eq!(drop_recovery(true, true), DropRecovery::Restart);
        assert_eq!(drop_recovery(false, false), DropRecovery::Restart);
    }
}
