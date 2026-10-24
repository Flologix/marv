//! #215 phase 2 (spec `docs/specs/2026-10-23-tool-batching.md` §5): a once-per-turn nudge to batch reads.
//!
//! Models often read one file, one search, one page per step, even when they already know the next few they need, so
//! a long turn spends a request (and a full prompt resend) on each. Read-only calls in one reply run at the same time
//! (#58). After [`BATCH_NUDGE_AFTER`] steps in a row that each made exactly one read-only call, the loop adds
//! [`BATCH_NUDGE_NOTE`] once per turn.
//!
//! marv-rs only (no Marv TS counterpart); the agent.rs header records the wiring:
//! - it lives in the loop's own `Phase::AfterStep`, not in the session's `after_step` hook, whose notes are fenced as
//!   project text (marv-session's `provenance.rs`): this note is Marv's own;
//! - it costs no extra step: it goes in after the step's results, with the next request, which goes out anyway;
//! - it shares a user message with any other note due at the same point (never two user turns in a row).
//!
//! **Read-only** ([`is_read_only_call`]): `read_file`, `grep`, `glob`, or `bash` whose command passes
//! [`marv_core::command::is_read_only_command`]. That list is a heuristic, never a security boundary.

use marv_core::ToolCall;

use crate::agent::ToolRun;

/// Steps in a row, each with exactly one read-only call, before the nudge.
pub const BATCH_NUDGE_AFTER: u32 = 4;

/// What the model is told (a user message from Marv, in Marv's own voice like the loop's other notes).
pub const BATCH_NUDGE_NOTE: &str = "(Automatic note from Marv, the program running you) The last few steps each read one thing. If you already know the next files, searches or pages you need (up to about four), send those calls together in one reply: read_file, grep and glob calls in one reply run at the same time.";

/// Tools that only read, for the nudge (bash is judged by its command).
const READ_TOOLS: [&str; 3] = ["read_file", "grep", "glob"];

/// Whether a call only reads: `read_file`, `grep`, `glob`, or a read-only bash command.
pub fn is_read_only_call(call: &ToolCall) -> bool {
    if READ_TOOLS.contains(&call.name.as_str()) {
        return true;
    }
    call.name == "bash"
        && serde_json::from_str::<serde_json::Value>(&call.arguments)
            .ok()
            .and_then(|args| args.get("command").and_then(|c| c.as_str()).map(marv_core::command::is_read_only_command))
            .unwrap_or(false)
}

/// Whether a finished step counts toward the nudge: exactly one call, read-only, that ran. A call answered with a
/// "not run" record (#81's failing-command note, a loop guard) doesn't count.
pub fn is_single_read(calls: &[ToolCall], runs: &[ToolRun]) -> bool {
    matches!((calls, runs), ([call], [run]) if is_read_only_call(call) && run.result.summary.as_deref() != Some("not run"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, arguments: &str) -> ToolCall {
        ToolCall { id: "c1".into(), name: name.into(), arguments: arguments.into() }
    }

    #[test]
    fn reads_and_read_only_commands_count() {
        assert!(is_read_only_call(&call("read_file", r#"{"path":"a.rs"}"#)));
        assert!(is_read_only_call(&call("grep", r#"{"pattern":"x"}"#)));
        assert!(is_read_only_call(&call("glob", r#"{"pattern":"*.rs"}"#)));
        assert!(is_read_only_call(&call("bash", r#"{"command":"ls src && git status"}"#)));
    }

    #[test]
    fn writes_and_other_commands_dont() {
        assert!(!is_read_only_call(&call("bash", r#"{"command":"rm -rf target"}"#)));
        assert!(!is_read_only_call(&call("bash", r#"{"command":"cat a > b"}"#)));
        assert!(!is_read_only_call(&call("bash", "not json")));
        assert!(!is_read_only_call(&call("edit_file", r#"{"path":"a.rs"}"#)));
        assert!(!is_read_only_call(&call("agent", r#"{"prompt":"x"}"#)));
    }

    #[test]
    fn the_note_is_in_marvs_voice() {
        assert!(BATCH_NUDGE_NOTE.starts_with("(Automatic note from Marv, the program running you) "));
    }
}
