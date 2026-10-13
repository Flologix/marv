//! `expect/`: writing a recording, and diffing a run against one.
//!
//! New in marv-rs (Marv's evals pass or fail on a hidden test; the oracle compares everything the
//! agent did). Four parts, each diffed on its own so a failure says which one moved:
//! `events.jsonl`, `requests.jsonl`, `files/` and `exit_code`.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;
use similar::TextDiff;

use crate::runner::{Outcome, read_tree};

/// A recording.
#[derive(Debug, Clone, PartialEq)]
pub struct Expected {
    pub events: String,
    pub requests: String,
    pub files: BTreeMap<String, Vec<u8>>,
    pub exit_code: Option<i32>,
}

/// One part that differs, with a readable diff.
#[derive(Debug, Clone)]
pub struct Mismatch {
    pub part: String,
    pub diff: String,
}

pub fn load_expect(dir: &Path) -> anyhow::Result<Expected> {
    let read = |file: &str| {
        std::fs::read_to_string(dir.join(file)).map_err(|err| anyhow::anyhow!("{}: {err}", dir.join(file).display()))
    };
    let exit_code = read("exit_code")?;
    let exit_code = match exit_code.trim() {
        "killed" => None,
        code => Some(code.parse().map_err(|_| anyhow::anyhow!("{}: bad exit_code {code:?}", dir.display()))?),
    };
    Ok(Expected {
        events: read("events.jsonl")?,
        requests: read("requests.jsonl")?,
        files: read_tree(&dir.join("files"), false)?,
        exit_code,
    })
}

/// Replaces `dir` with a recording of `outcome`.
pub fn write_expect(dir: &Path, outcome: &Outcome) -> anyhow::Result<()> {
    if dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    std::fs::create_dir_all(dir.join("files"))?;
    std::fs::write(dir.join("events.jsonl"), &outcome.events)?;
    std::fs::write(dir.join("requests.jsonl"), &outcome.requests)?;
    let code = outcome.exit_code.map_or("killed".to_string(), |c| c.to_string());
    std::fs::write(dir.join("exit_code"), code + "\n")?;
    let staged = dir.join("files");
    for (rel, bytes) in &outcome.files {
        let path = staged.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, bytes)?;
    }
    Ok(())
}

/// Every part of `actual` that differs from `expected`. Empty means it passed.
pub fn compare(expected: &Expected, actual: &Outcome) -> Vec<Mismatch> {
    let mut out = Vec::new();
    if expected.exit_code != actual.exit_code {
        let show = |c: Option<i32>| c.map_or("killed".to_string(), |c| c.to_string());
        out.push(Mismatch {
            part: "exit_code".into(),
            diff: format!("expected {}, got {}", show(expected.exit_code), show(actual.exit_code)),
        });
    }
    if expected.events != actual.events {
        out.push(Mismatch { part: "events.jsonl".into(), diff: unified(&expected.events, &actual.events) });
    }
    if expected.requests != actual.requests {
        out.push(Mismatch { part: "requests.jsonl".into(), diff: requests_diff(&expected.requests, &actual.requests) });
    }
    let names: std::collections::BTreeSet<&String> = expected.files.keys().chain(actual.files.keys()).collect();
    for name in names {
        let diff = match (expected.files.get(name), actual.files.get(name)) {
            (Some(a), Some(b)) if a == b => continue,
            (Some(_), None) => "missing".to_string(),
            (None, Some(_)) => "not expected (a new file)".to_string(),
            (Some(a), Some(b)) => match (std::str::from_utf8(a), std::str::from_utf8(b)) {
                (Ok(a), Ok(b)) => unified(&visible(a), &visible(b)),
                _ => format!("binary contents differ ({} vs {} bytes)", a.len(), b.len()),
            },
            (None, None) => continue,
        };
        out.push(Mismatch { part: format!("files/{name}"), diff });
    }
    out
}

/// Line endings and trailing whitespace made visible, so a CRLF or a stray space shows in the diff.
fn visible(text: &str) -> String {
    text.split_inclusive('\n')
        .map(|line| {
            let (body, end) = match line.strip_suffix("\r\n") {
                Some(body) => (body, "␍\n"),
                None => match line.strip_suffix('\n') {
                    Some(body) => (body, "\n"),
                    None => (line, "⏎(no newline)\n"),
                },
            };
            let trimmed = body.trim_end_matches([' ', '\t']);
            let trailing: String = body[trimmed.len()..].chars().map(|c| if c == '\t' { '→' } else { '·' }).collect();
            format!("{trimmed}{trailing}{end}")
        })
        .collect()
}

fn unified(expected: &str, actual: &str) -> String {
    TextDiff::from_lines(expected, actual).unified_diff().context_radius(3).header("expect", "actual").to_string()
}

/// Requests are compared as bytes. Where two bodies differ, they're diffed pretty-printed; where
/// they're the same JSON in other bytes, that is said outright: the prompt cache keys on bytes.
fn requests_diff(expected: &str, actual: &str) -> String {
    let (exp, act): (Vec<&str>, Vec<&str>) = (expected.lines().collect(), actual.lines().collect());
    let mut out = String::new();
    if exp.len() != act.len() {
        out.push_str(&format!("{} requests expected, {} sent\n", exp.len(), act.len()));
    }
    for i in 0..exp.len().max(act.len()) {
        let (a, b) = (exp.get(i).copied().unwrap_or(""), act.get(i).copied().unwrap_or(""));
        if a == b {
            continue;
        }
        if i >= act.len() || i >= exp.len() {
            out.push_str(&format!("request {}: {}\n", i + 1, if i >= act.len() { "not sent" } else { "not expected" }));
            continue;
        }
        let pretty = |s: &str| {
            serde_json::from_str::<Value>(s)
                .ok()
                .and_then(|v| serde_json::to_string_pretty(&v).ok())
                .map_or(s.to_string(), |p| p + "\n")
        };
        let (pa, pb) = (pretty(a), pretty(b));
        out.push_str(&format!("request {}:\n", i + 1));
        if pa == pb {
            let at = a.bytes().zip(b.bytes()).position(|(x, y)| x != y).unwrap_or(a.len().min(b.len()));
            let around = |s: &str| s.get(at.saturating_sub(40)..(at + 40).min(s.len())).unwrap_or("").to_string();
            out.push_str(&format!(
                "  same JSON, different bytes (key order or formatting) from byte {at}:\n  expect …{}…\n  actual …{}…\n",
                around(a),
                around(b)
            ));
        } else {
            out.push_str(&unified(&pa, &pb));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(events: &str, requests: &str, files: &[(&str, &str)], code: Option<i32>) -> Outcome {
        Outcome {
            events: events.into(),
            requests: requests.into(),
            files: files.iter().map(|(k, v)| (k.to_string(), v.as_bytes().to_vec())).collect(),
            exit_code: code,
            stderr: String::new(),
            other_requests: Vec::new(),
            unused_replies: 0,
            timed_out: false,
        }
    }

    fn expected(o: &Outcome) -> Expected {
        Expected { events: o.events.clone(), requests: o.requests.clone(), files: o.files.clone(), exit_code: o.exit_code }
    }

    #[test]
    fn identical_runs_pass() {
        let o = outcome("{\"type\":\"done\"}\n", "{}\n", &[("a.js", "x\n")], Some(0));
        assert!(compare(&expected(&o), &o).is_empty());
    }

    #[test]
    fn each_part_is_reported() {
        let want = outcome("{\"type\":\"done\"}\n", "{\"a\":1}\n", &[("a.js", "x\n"), ("gone.js", "")], Some(0));
        let got = outcome("{\"type\":\"error\"}\n", "{\"a\":2}\n", &[("a.js", "y\n"), ("new.js", "")], Some(1));
        let parts: Vec<String> = compare(&expected(&want), &got).into_iter().map(|m| m.part).collect();
        assert_eq!(parts, ["exit_code", "events.jsonl", "requests.jsonl", "files/a.js", "files/gone.js", "files/new.js"]);
    }

    #[test]
    fn key_order_in_a_request_is_called_out() {
        let want = outcome("", "{\"a\":1,\"b\":2}\n", &[], Some(0));
        let got = outcome("", "{\"b\":2,\"a\":1}\n", &[], Some(0));
        let diff = &compare(&expected(&want), &got)[0].diff;
        assert!(diff.contains("same JSON, different bytes"), "{diff}");
    }

    #[test]
    fn crlf_and_trailing_space_show() {
        assert_eq!(visible("a \r\nb\t\nc"), "a·␍\nb→\nc⏎(no newline)\n");
    }

    #[test]
    fn expect_round_trips() {
        let dir = std::env::temp_dir().join(format!("marv-oracle-unit-expect-{}", std::process::id()));
        let o = outcome("{}\n", "{}\n", &[("src/a.js", "a\r\n")], None);
        write_expect(&dir, &o).unwrap();
        let loaded = load_expect(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(loaded, expected(&o));
    }
}
