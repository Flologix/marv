//! Clock helpers (new in marv-rs; Marv used `Date.now()` and `toISOString()` inline).
//!
//! - [`now_ms`]: milliseconds since the epoch, like `Date.now()`.
//! - [`iso`] / [`iso_date`]: `toISOString()` and its first 10 characters, without a date crate. The system prompt's
//!   "Today's date" and session ids use them, so they must match Marv's format byte for byte.

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch (`Date.now()`).
pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// (year, month 1-12, day 1-31) of a day count since 1970-01-01 (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `new Date(ms).toISOString()`: `2026-10-13T08:05:09.042Z`.
pub fn iso(ms: u64) -> String {
    let days = (ms / 86_400_000) as i64;
    let rest = ms % 86_400_000;
    let (y, m, d) = civil_from_days(days);
    let (h, min, s, milli) = (rest / 3_600_000, rest / 60_000 % 60, rest / 1000 % 60, rest % 1000);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}.{milli:03}Z")
}

/// `toISOString().slice(0, 10)`: `2026-10-13` (UTC, as Marv's prompt).
pub fn iso_date(ms: u64) -> String {
    iso(ms)[..10].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_like_to_iso_string() {
        assert_eq!(iso(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso(951_782_400_000), "2000-02-29T00:00:00.000Z");
        assert_eq!(iso(1_791_878_709_042), "2026-10-13T08:05:09.042Z");
        assert_eq!(iso_date(1_791_878_709_042), "2026-10-13");
    }
}
