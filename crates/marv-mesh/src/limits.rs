//! Size and rate limits per wire, so a looping agent can't flood another
//! (docs/MESH.md, rule 4). New in marv-rs.
//!
//! Each wire has its own [`WireLimits`] and its own [`RateLimiter`] (a token
//! bucket): a message over the size limit is refused outright, never cut (a
//! cut message could change meaning); one over the rate is refused with how
//! long to wait. Time is passed in (ms), so the limiter is pure and testable.

use serde::{Deserialize, Serialize};

use crate::MeshError;

/// A message's body, at most (bytes of its JSON). Big enough for a report, small
/// enough that one message can't fill another agent's context.
pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 32 * 1024;
pub const DEFAULT_PER_MINUTE: u32 = 30;
pub const DEFAULT_BURST: u32 = 10;
/// An inbox holds at most this many unread messages; the oldest are kept, new ones refused.
pub const DEFAULT_INBOX_DEPTH: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireLimits {
    pub max_message_bytes: usize,
    /// Sustained rate.
    pub per_minute: u32,
    /// Messages that may go at once before the rate applies.
    pub burst: u32,
}

impl Default for WireLimits {
    fn default() -> Self {
        Self { max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES, per_minute: DEFAULT_PER_MINUTE, burst: DEFAULT_BURST }
    }
}

impl WireLimits {
    pub fn check_size(&self, receptacle: &str, size: usize) -> crate::Result<()> {
        if size > self.max_message_bytes {
            return Err(MeshError::TooLarge { receptacle: receptacle.into(), size, limit: self.max_message_bytes });
        }
        Ok(())
    }
}

/// A token bucket: `burst` tokens, refilled at `per_minute`.
#[derive(Debug, Clone, PartialEq)]
pub struct RateLimiter {
    limits: WireLimits,
    tokens: f64,
    last_ms: u64,
}

impl RateLimiter {
    pub fn new(limits: WireLimits, now_ms: u64) -> Self {
        Self { limits, tokens: limits.burst as f64, last_ms: now_ms }
    }

    fn refill(&mut self, now_ms: u64) {
        let elapsed = now_ms.saturating_sub(self.last_ms) as f64;
        self.tokens = (self.tokens + elapsed * self.limits.per_minute as f64 / 60_000.0).min(self.limits.burst as f64);
        self.last_ms = now_ms.max(self.last_ms);
    }

    /// Takes one message's token, or says how long until there is one.
    pub fn take(&mut self, receptacle: &str, now_ms: u64) -> crate::Result<()> {
        self.refill(now_ms);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            return Ok(());
        }
        let per_ms = self.limits.per_minute as f64 / 60_000.0;
        let retry_ms = if per_ms > 0.0 { ((1.0 - self.tokens) / per_ms).ceil() as u64 } else { u64::MAX };
        Err(MeshError::RateLimited { receptacle: receptacle.into(), retry_ms })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_messages_are_refused_not_cut() {
        let l = WireLimits::default();
        assert!(l.check_size("agent.a.inbox", DEFAULT_MAX_MESSAGE_BYTES).is_ok());
        assert!(matches!(l.check_size("agent.a.inbox", DEFAULT_MAX_MESSAGE_BYTES + 1), Err(MeshError::TooLarge { .. })));
    }

    #[test]
    fn a_burst_then_the_sustained_rate() {
        let limits = WireLimits { max_message_bytes: 10, per_minute: 60, burst: 3 };
        let mut r = RateLimiter::new(limits, 0);
        for _ in 0..3 {
            r.take("x.y", 0).unwrap();
        }
        let Err(MeshError::RateLimited { retry_ms, .. }) = r.take("x.y", 0) else { panic!() };
        assert_eq!(retry_ms, 1000);
        assert!(r.take("x.y", 999).is_err());
        assert!(r.take("x.y", 2000).is_ok());
        // Refill never exceeds the burst.
        r = RateLimiter::new(limits, 0);
        for _ in 0..3 {
            r.take("x.y", 1_000_000).unwrap();
        }
        assert!(r.take("x.y", 1_000_000).is_err());
    }
}
