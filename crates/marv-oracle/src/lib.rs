//! The oracle: proof that marv-rs behaves like Marv (see `tests/oracle/README.md`).
//!
//! ts-rust ported the TypeScript compiler by diffing its output against the original's; the oracle
//! does the same for what the agent *does*. Each scenario is a scripted model ([`script`]) served by
//! a fake OpenAI-compatible server ([`server`]), a prompt and a starting project. [`runner`] runs a
//! `marv` binary on it (`marv -p … --output stream-json`), and [`compare`] diffs the event log, the
//! request bodies (as canonical JSON), the project afterwards and the exit code against `expect/`,
//! recorded once from TypeScript Marv. [`prefix`] checks the run's own requests against the
//! prompt-cache prefix rule, byte for byte. [`tracker`] is the MCP fixture (Marv's
//! `evals/mcp/tracker.ts`).
//!
//! Seeded from Marv's `evals/` (17 tasks, `evals/run.ts`): the editing tasks become scripted runs,
//! plus the loop's edge cases (Ollama's duplicate `call_0`, a cut-off, a text loop, the step limit,
//! a 429, mid-turn compaction, …).
//!
//! ```sh
//! MARV_ORACLE_BIN=~/Projects/marv/bin/marv MARV_ORACLE_RECORD=1 cargo test -p marv-oracle -- --ignored  # record
//! MARV_ORACLE_BIN=target/debug/marv cargo test -p marv-oracle -- --ignored                               # check
//! ```

pub mod compare;
pub mod normalize;
pub mod prefix;
pub mod runner;
pub mod scenario;
pub mod script;
pub mod server;
pub mod tracker;

use std::path::Path;

use compare::{Mismatch, compare, load_expect, write_expect};
use scenario::Scenario;

/// How one scenario went.
#[derive(Debug)]
pub enum Verdict {
    Pass,
    Recorded,
    Fail {
        mismatches: Vec<Mismatch>,
        stderr: String,
        notes: Vec<String>,
    },
    /// The run couldn't happen (no binary, a broken scenario, no recording).
    Error(String),
}

/// Where the binary comes from: `MARV_ORACLE_BIN`, else this workspace's `target/debug/marv`.
pub fn binary() -> std::path::PathBuf {
    match std::env::var_os("MARV_ORACLE_BIN") {
        Some(bin) => std::path::PathBuf::from(bin),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/marv"),
    }
}

/// Every scenario under `root` through `bin`, in parallel (each has its own server and folders).
pub fn check_all(scenarios: &[Scenario], bin: &Path, record: bool) -> Vec<(String, Verdict)> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = scenarios.iter().map(|s| scope.spawn(move || (s.name.clone(), check(s, bin, record)))).collect();
        handles.into_iter().map(|h| h.join().expect("a scenario thread panicked")).collect()
    })
}

/// Runs `scenario` through `bin`; with `record`, saves the run as its `expect/` instead of diffing.
pub fn check(scenario: &Scenario, bin: &Path, record: bool) -> Verdict {
    let outcome = match runner::run(scenario, bin) {
        Ok(outcome) => outcome,
        Err(err) => return Verdict::Error(err.to_string()),
    };
    let mut notes = Vec::new();
    if outcome.timed_out {
        notes.push("timed out and was killed".to_string());
    }
    if outcome.unused_replies > 0 {
        notes.push(format!("{} scripted replies were never asked for", outcome.unused_replies));
    }
    if !outcome.other_requests.is_empty() {
        notes.push(format!("other requests: {}", outcome.other_requests.join(", ")));
    }
    // The run's own requests must extend one another, whichever Marv sent them.
    let breaks = prefix::prefix_breaks(&outcome.raw_requests, &scenario.run.prefix_resets);
    let prefix_mismatch = (!breaks.is_empty()).then(|| Mismatch {
        part: "requests (prefix rule)".into(),
        diff: breaks
            .iter()
            .map(|b| format!("request {} doesn't extend request {}: {}\n", b.request, b.request - 1, b.detail))
            .collect(),
    });
    if record {
        // A recording of a broken run is worse than none.
        if outcome.timed_out || outcome.unused_replies > 0 || prefix_mismatch.is_some() {
            return Verdict::Fail { mismatches: prefix_mismatch.into_iter().collect(), stderr: outcome.stderr, notes };
        }
        return match write_expect(&scenario.expect(), &outcome) {
            Ok(()) => Verdict::Recorded,
            Err(err) => Verdict::Error(err.to_string()),
        };
    }
    let expected = match load_expect(&scenario.expect()) {
        Ok(expected) => expected,
        Err(err) => return Verdict::Error(format!("no recording: {err}")),
    };
    let mut mismatches = compare(&expected, &outcome);
    mismatches.extend(prefix_mismatch);
    if mismatches.is_empty() { Verdict::Pass } else { Verdict::Fail { mismatches, stderr: outcome.stderr, notes } }
}

/// A report of one verdict, safe to print: diffs and stderr carry model and tool text.
pub fn report(name: &str, verdict: &Verdict) -> String {
    marv_core::printable(&report_text(name, verdict))
}

fn report_text(name: &str, verdict: &Verdict) -> String {
    match verdict {
        Verdict::Pass => format!("ok       {name}\n"),
        Verdict::Recorded => format!("recorded {name}\n"),
        Verdict::Error(err) => format!("ERROR    {name}: {err}\n"),
        Verdict::Fail { mismatches, stderr, notes } => {
            let mut out = format!("FAIL     {name}\n");
            for note in notes {
                out.push_str(&format!("  note: {note}\n"));
            }
            for m in mismatches {
                out.push_str(&format!("  --- {}\n", m.part));
                for line in m.diff.lines() {
                    out.push_str(&format!("  {line}\n"));
                }
            }
            if !stderr.trim().is_empty() {
                let tail: Vec<&str> = stderr.lines().rev().take(20).collect();
                out.push_str("  --- stderr (last lines)\n");
                for line in tail.into_iter().rev() {
                    out.push_str(&format!("  {line}\n"));
                }
            }
            out
        }
    }
}
