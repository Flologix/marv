//! Scrubbing what differs between two runs of the same scenario: temp paths, the server's port,
//! today's date. Everything else in a run is decided by the script, so it must match exactly.
//!
//! Event lines are compared as JSON values (`JSON.stringify` and serde order keys differently, and
//! the order of an event's fields means nothing). Request bodies are kept as bytes: the prompt cache
//! keys on them, so a byte that moves is a real difference.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// Literal replacements, applied longest first so a path is never half-replaced by its parent's.
#[derive(Debug, Clone, Default)]
pub struct Normalizer {
    pairs: Vec<(String, String)>,
}

impl Normalizer {
    pub fn new() -> Normalizer {
        let mut normalizer = Normalizer::default();
        // Marv writes today's date (UTC) into the system prompt. Yesterday and tomorrow too, so a run
        // that crosses midnight, or a binary that uses local time, still matches.
        let today = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() / 86_400).unwrap_or(0) as i64;
        for day in [today - 1, today, today + 1] {
            normalizer.add(&civil_date(day), "<date>");
        }
        normalizer
    }

    pub fn add(&mut self, from: &str, to: &str) {
        if from.is_empty() {
            return;
        }
        self.pairs.push((from.to_string(), to.to_string()));
        self.pairs.sort_by_key(|pair| std::cmp::Reverse(pair.0.len()));
    }

    pub fn text(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (from, to) in &self.pairs {
            if out.contains(from.as_str()) {
                out = out.replace(from.as_str(), to);
            }
        }
        out
    }

    /// The `stream-json` log: one canonical JSON line per event (keys sorted). A line that isn't
    /// JSON is kept as it is, so a binary that prints something else shows up in the diff.
    pub fn events(&self, stdout: &str) -> String {
        let mut out = String::new();
        for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
            let line = self.text(line);
            match serde_json::from_str::<Value>(&line) {
                Ok(mut value) => {
                    // marv-rs #63: a compaction says what started it; Marv's event doesn't.
                    if value.get("type").and_then(Value::as_str) == Some("compaction")
                        && let Some(event) = value.as_object_mut()
                    {
                        event.remove("trigger");
                        event.remove("level");
                    }
                    out.push_str(&value.to_string())
                }
                Err(_) => out.push_str(&line),
            }
            out.push('\n');
        }
        out
    }
}

/// `YYYY-MM-DD` for a day count since 1970-01-01 (Howard Hinnant's `civil_from_days`).
pub fn civil_date(days: i64) -> String {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(20_739), "2026-10-13");
        assert_eq!(civil_date(11_016), "2000-02-29");
    }

    #[test]
    fn longest_path_first() {
        let mut n = Normalizer::default();
        n.add("/tmp/x", "<tmp>");
        n.add("/tmp/x/project", "<project>");
        assert_eq!(n.text("cd /tmp/x/project && ls /tmp/x"), "cd <project> && ls <tmp>");
    }

    #[test]
    fn todays_date_is_scrubbed() {
        let today = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() / 86_400;
        let line = format!("Today's date: {}", civil_date(today as i64));
        assert_eq!(Normalizer::new().text(&line), "Today's date: <date>");
    }

    #[test]
    fn events_are_canonical_json_lines() {
        let n = Normalizer::default();
        assert_eq!(
            n.events("{\"type\":\"done\",\"reason\":\"end\"}\n\nnot json\n"),
            "{\"reason\":\"end\",\"type\":\"done\"}\nnot json\n"
        );
        // marv-rs #63's trigger isn't Marv's.
        assert_eq!(
            n.events("{\"type\":\"compaction\",\"compacted\":true,\"trigger\":\"overflow\",\"level\":1}\n"),
            "{\"compacted\":true,\"type\":\"compaction\"}\n"
        );
    }
}
