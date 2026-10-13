//! Runs the project's checks inside the sandbox. Ports Marv's `src/checks/runner.ts`.
//!
//! Prints one JSON line per violation (`{check, file?, line?, message, why?}`), `{check, error, detail?}` for a check
//! that failed, and `{done: true}` last, so output that stops early (a crash, the capture limit) can be told apart.
//! A script's stdout is read as it arrives (at most 1 MB, at most 500 violations kept per check, stderr's first 16k
//! chars), 30 s per script. Pattern checks are matched in the sandbox, so a hostile pattern can only time out.
//!
//! Port note: Marv's runner was TypeScript source sent to `bun -e`, self-contained because the sandbox can't see
//! Marv's files. marv-rs is one static binary: the plan is a hidden `marv __run-checks` subcommand, the binary
//! bind-mounted read-only into the sandbox, with [`RunnerSpec`] as JSON on stdin.

use serde::{Deserialize, Serialize};

/// Per script: stdout read at most this much.
pub const MAX_SCRIPT_OUTPUT: usize = 1024 * 1024;
pub const MAX_VIOLATIONS_PER_CHECK: usize = 500;
pub const MAX_STDERR_CHARS: usize = 16_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunnerSpec {
    /// The checks to run, as JSON (`load::ProjectCheck` fields).
    pub checks: serde_json::Value,
    pub timeout_ms: u64,
}

/// The runner's body: reads the spec, runs every check, prints the JSON lines and `{"done":true}` last.
pub fn run_checks(_spec: &RunnerSpec) -> std::io::Result<()> {
    todo!("port: runChecks (pattern matching over globbed files, bun scripts with timeouts and caps, JSON lines out)")
}
