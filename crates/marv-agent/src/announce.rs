//! Detects a reply that announces its next step but makes no tool call ("Now checking issue
//! #7 before commenting:"). The model ended the turn there, so the run stopped before doing
//! what it said (`reason: end`) — 2 of 6 GLM runs of `mcp-comment` did this while opencode
//! did none. Marv (#90) then sends one automatic continue per turn, like a user would.
//!
//! marv-rs only (no Marv TS counterpart); the agent.rs header records the wiring.
//!
//! The detector is deliberately conservative — it must never fire on a normal summary or a
//! question to the user, since a false continue costs a request and a confusing nudge:
//! - a dangling `:` at the very end of the reply (the observed form),
//! - a last sentence starting "let me …", optionally after "now"/"next"/"first"/"then" (but
//!   not "let me know/summarize/recap…", which address the user),
//! - a last sentence starting "now " with a known progressive verb ("now checking …"),
//! - a last sentence that starts the work: "starting with …", "proceeding to …", "I'll start
//!   by …" ([`START_PHRASES`]; DeepSeek V4.1 Flash ended a `rename-across-files` run on "Starting
//!   with step 1." after a numbered plan and no tool call, 2026-10-31).
//!
//! Questions (`?`), code blocks, long sentences (a summary, not an announcement) and
//! everything else never fire. Misses are fine: the finish_reason in the request record is
//! the reliable diagnosis, and the list of `now`-verbs can grow when the bench shows one.

/// What the model is told after a reply that summarized the conversation unasked and called no tool.
pub const SELF_SUMMARY_NOTE: &str = "(Automatic note from Marv, the program running you) Your last reply was a summary of the conversation, which nobody asked for: nothing was compacted, the conversation goes on, and the task isn't finished. Don't summarize; continue the task with your next step.";

/// How many self-summaries a turn continues past: long tasks showed the quirk more than once.
pub const MAX_SELF_SUMMARY_CONTINUES: u32 = 3;

/// Whether a finished reply (no tool calls) is the model summarizing the conversation on its own, in the shape of a
/// compaction summary ("<analysis>\nLet me work through the conversation chronologically…"). DeepSeek V4.1 Flash did
/// it mid-task in 9 of its 46 failed Terminal-Bench runs and in none of its 36 passes (2026-10-31), with no compaction
/// asked for: the reply has no tool call, so it ended the run. Judged on how the reply starts only, so a report that
/// mentions a summary further down never fires.
pub fn summarizes_itself(text: &str) -> bool {
    let start = text.trim_start();
    if start.starts_with("<analysis>") || start.starts_with("<summary>") {
        return true;
    }
    let first = start.lines().next().unwrap_or("").trim().to_lowercase();
    [
        "let me work through the conversation",
        "let me chronologically",
        "let me go through the conversation chronologically",
        "让我按时间顺序",
    ]
    .iter()
    .any(|p| first.starts_with(p))
}

/// What the model is told after a reply that announced a step but called no tool (#90).
pub const ANNOUNCED_STEP_NOTE: &str = "(Automatic note from Marv, the program running you) Your last reply announced your next step but made no tool call, so the run stopped. Take the step you announced now.";

/// How long an announcing sentence may be. A real announcement is a few words; anything
/// longer that ends in `:` or "let me" is a summary or a heading, not a next step.
const MAX_ANNOUNCEMENT: usize = 200;

/// "now <verb>ing" — a closed list, so the detector can't run off into prose. Extend when the
/// bench shows a form it misses; a miss only means no nudge, never a wrong one.
const NOW_VERBS: &[&str] = &[
    "adding",
    "analyzing",
    "browsing",
    "building",
    "checking",
    "compiling",
    "comparing",
    "counting",
    "creating",
    "debugging",
    "editing",
    "examining",
    "fetching",
    "fixing",
    "implementing",
    "inspecting",
    "installing",
    "loading",
    "looking",
    "measuring",
    "merging",
    "opening",
    "parsing",
    "posting",
    "profiling",
    "reading",
    "refactoring",
    "removing",
    "renaming",
    "reviewing",
    "running",
    "saving",
    "searching",
    "testing",
    "tracing",
    "updating",
    "verifying",
    "writing",
];

/// A last sentence starting with one of these starts the work it names. Lower case, matched at the
/// sentence's start only; a closed list for the same reason as [`NOW_VERBS`].
const START_PHRASES: &[&str] = &[
    "starting with ",
    "starting on ",
    "starting by ",
    "beginning with ",
    "proceeding with ",
    "proceeding to ",
    "moving on to ",
    "i'll start ",
    "i will start ",
    "i'll begin ",
    "i will begin ",
    "i'll now ",
    "i will now ",
];

/// Whether a finished reply (no tool calls) ends by announcing its next action.
pub fn announces_next_step(text: &str) -> bool {
    // A reply cut off inside a code block ends wherever the code stopped; its last line isn't
    // prose. An odd number of fences means we're inside a block.
    if text.matches("```").count() % 2 == 1 {
        return false;
    }
    let line = text.lines().rev().find(|l| !l.trim().is_empty()).map_or("", |l| l.trim());
    if line.is_empty() {
        return false;
    }
    // Trailing emphasis must not hide a dangling colon ("…and comment:**" is still one).
    let ends_with_colon = line.trim_end_matches(['*', '_', '`', ' ', '\t']).ends_with(':');
    if ends_with_colon && line.chars().count() <= MAX_ANNOUNCEMENT {
        return true;
    }
    let sentence = last_sentence(line);
    if sentence.is_empty() || sentence.chars().count() > MAX_ANNOUNCEMENT {
        return false;
    }
    let lower = sentence.trim_start_matches(['*', '_', '`', '#', '>', '-', ' ', '\t']).to_lowercase();
    // A question to the user ("Shall I comment on #7?", "Let me know if …?") is never a step.
    // Trailing emphasis must not hide the mark ("…, shall I continue?**"). Judged on the line:
    // `last_sentence` has already trimmed the sentence's own `?` (until 2026-10-31 this looked at
    // the trimmed sentence and never fired; "Let me check the build?" nudged).
    if line.trim_end_matches(['*', '_', '`', ' ', '\t', '"', '\'', ')']).ends_with('?') {
        return false;
    }
    // "Now let me …" / "Next, let me …" is the same announcement with a lead-in (the GLM form
    // without its colon). "let me know/summarize/recap" addresses the user, not a next step.
    let let_me = ["now", "next", "first", "then"]
        .iter()
        .find_map(|lead| lower.strip_prefix(lead).map(|rest| rest.trim_start_matches(',').trim_start()))
        .filter(|rest| rest.starts_with("let me "))
        .unwrap_or(&lower);
    if let Some(rest) = let_me.strip_prefix("let me ") {
        return !["know", "summarize", "summarise", "recap"].iter().any(|w| rest.starts_with(w));
    }
    if START_PHRASES.iter().any(|p| lower.starts_with(p)) {
        return true;
    }
    match lower.strip_prefix("now ").and_then(|rest| rest.split(|c: char| !c.is_ascii_alphabetic()).next()) {
        Some(word) => NOW_VERBS.contains(&word),
        None => false,
    }
}

/// The final sentence of a line: whatever follows the last `.`, `!` or `?` (the announcement
/// is what the reply ends with; earlier sentences are context, like "Both modes work."). The
/// line's own terminal punctuation (a `.` at the very end, closing quotes, `…`) ends the
/// sentence rather than starting an empty one, so "Let me check the test." stays one sentence.
fn last_sentence(line: &str) -> &str {
    let line = line.trim_end_matches(['.', '!', '?', '"', '\'', '’', '…', ')', '*', '_', '`', ' ', '\t']);
    let start = line
        .char_indices()
        .rev()
        .find_map(|(i, c)| {
            let followed_by_space = line.as_bytes().get(i + 1).is_none_or(|b| *b == b' ' || *b == b'\t');
            (matches!(c, '.' | '!' | '?') && followed_by_space).then(|| i + 1)
        })
        .unwrap_or(0);
    line[start..].trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fires_on_the_announced_steps_that_ended_the_glm_runs() {
        assert!(announces_next_step("Both modes work. Now let me look at issue #7 and comment:"));
        assert!(announces_next_step("Both modes verified. Now checking issue #7 before commenting:"));
    }

    #[test]
    fn fires_on_a_last_sentence_that_announces() {
        assert!(announces_next_step("Let me check the failing test."));
        assert!(announces_next_step("Let me get issue #7"));
        assert!(announces_next_step("Everything builds again.\nNow running the full suite."));
        assert!(announces_next_step("**Now reviewing the output.**"));
        assert!(announces_next_step("All green.\n- now checking the failing test"));
        assert!(announces_next_step("…so the fix is in. I'll now update the changelog:"));
        // The colon form needs no known verb: a dangling colon IS the announcement.
        assert!(announces_next_step("Files to change: a.rs, b.rs:"));
    }

    #[test]
    fn fires_on_a_last_sentence_that_starts_the_work() {
        // The DeepSeek V4.1 Flash reply that ended a rename-across-files run (2026-10-31).
        let plan = "I will start by searching for all occurrences of `fetchUserData` across the project files using `grep`.\n\nPlan:\n1. Search for all occurrences of `fetchUserData` in the project.\n2. Rename it in the identified files.\n\nStarting with step 1.";
        assert!(announces_next_step(plan));
        assert!(announces_next_step("Plan is clear. Proceeding to edit src/a.ts."));
        assert!(announces_next_step("I'll start by reading the failing test."));
        assert!(announces_next_step("Moving on to the README."));
        // A summary that merely mentions starting isn't one.
        assert!(!announces_next_step("Done. I started with the parser and finished with the tests."));
        assert!(!announces_next_step("I'll start on the docs if you want me to?"));
        assert!(!announces_next_step("Let me check the build?"));
    }

    #[test]
    fn never_fires_on_a_summary() {
        assert!(!announces_next_step("Done. Both modes work as expected."));
        assert!(!announces_next_step("Here's what I changed:\n\n- a.rs: fixed the parse\n- b.rs: added the test"));
        assert!(!announces_next_step("Summary of changes:\n\nFiles: a.rs, b.rs."));
        assert!(!announces_next_step("All checks pass. The end."));
        assert!(!announces_next_step("## Done\n\nEverything from the issue is in place."));
    }

    #[test]
    fn never_fires_on_a_question_to_the_user() {
        assert!(!announces_next_step("Should I also update the docs?"));
        assert!(!announces_next_step("Let me know if you want me to continue."));
        assert!(!announces_next_step("Both modes verified. Shall I comment on #7 now?"));
        // Waiting on the user, or stopping, is the end of the turn, not a step to take.
        assert!(!announces_next_step("All done. Now waiting for your feedback."));
        assert!(!announces_next_step("Now stopping here."));
        assert!(!announces_next_step("Now let me know if anything else is needed."));
    }

    #[test]
    fn fires_on_let_me_after_a_lead_in_and_not_on_a_summary() {
        // The observed GLM form without its dangling colon.
        assert!(announces_next_step("Both modes work. Now let me look at issue #7 and comment."));
        assert!(announces_next_step("Next, let me fix b."));
        assert!(announces_next_step("Now, let me check the tests."));
        assert!(!announces_next_step("Let me summarize: everything passes."));
        assert!(!announces_next_step("Let me recap what changed."));
    }

    #[test]
    fn skips_code_blocks_and_oddities() {
        // Inside a code block the last line isn't prose.
        assert!(!announces_next_step("The fix:\n\n```rust\nfn main() {}\n"));
        assert!(!announces_next_step("The fix:\n\n```rust\nfn main() {}\n```"));
        assert!(!announces_next_step(""));
        assert!(!announces_next_step("   \n\t\n"));
        assert!(!announces_next_step("| a | b: |"));
    }

    #[test]
    fn a_long_last_line_is_a_summary_not_an_announcement() {
        let long = format!("Now checking {}:", "x".repeat(300));
        assert!(!announces_next_step(&long));
    }

    #[test]
    fn self_summaries_fire_and_reports_dont() {
        assert!(summarizes_itself(
            "<analysis>\nLet me work through the conversation chronologically.\n\n**The user's single task message:**"
        ));
        assert!(summarizes_itself("  <summary>\n1. Primary request"));
        assert!(summarizes_itself("Let me chronologically analyze this conversation.\n\n**Message 1**"));
        assert!(summarizes_itself("让我按时间顺序梳理一下这段对话。"));
        assert!(!summarizes_itself("Done. `/app/run.py` provides run_tasks.\n\n## Summary\n- added the cancel path"));
        assert!(!summarizes_itself("Here's a summary of what I changed: a.rs, b.rs."));
        assert!(!summarizes_itself("Let me check the failing test."));
    }
}
