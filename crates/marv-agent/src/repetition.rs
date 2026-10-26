//! Repetition detection: catch a model stuck in a loop repeating text or tool calls. Ports
//! `src/repetition.ts` (Marv #42).
//!
//! - **Text:** the same sentence (trimmed, lowercased) [`TEXT_REPEATS`] (8)+ times in a row,
//!   across the last ~4 KB of the reply ([`TEXT_WINDOW`]), so a burst near the end is caught early
//!   and earlier text doesn't keep a busy reply from ever triggering. Sentences split on `. ! ?`
//!   followed by whitespace or the end, and on newlines. Only "sentence-like" units count (≥ 20
//!   characters, ≥ 2 words), so code fragments and list markers ("- milk") never trigger.
//! - **Tool calls:** a period-1 (A,A,A) or period-2 (A,B,A,B,A) cycle across steps, from a short
//!   history ([`TOOL_HISTORY`] = 8 steps). A step with a write-like tool (`edit_file`,
//!   `write_file`, `bash`) resets it: a re-read after a change is legitimate. Only steps with the
//!   same number of calls compare. The agent loop folds each call's *last result* into its key,
//!   so the same call returning something new each time (the next item of a queue) is new work.
//! - Arguments are normalized with keys sorted **at every depth**: Marv once filtered nested
//!   objects with a key list, so `{"filter":{"id":1}}` and `{"filter":{"id":2}}` compared equal.
//! - **Failing commands** (marv-rs, #81): bash resets the tool-call history above, and a model retrying a command
//!   that can't succeed varies its tail (`&& du -sh`, `|| true`, `| tail -2`), so neither guard saw a `git clone`
//!   re-sent 11 times. [`FailureLoopState`] remembers each bash command by [`marv_core::command::command_key`] (plus
//!   its `network`/`sandbox: false` flags) with its failure's signature ([`failure_signature`]: its first error-like
//!   line). Once a command failed the same way [`FAILURE_REPEATS`] (2) times in a row with no call since that may
//!   have changed things (any tool but bash and the [`READ_ONLY_TOOLS`]), another try is a loop. Low on false positives: a different error, a success, or a file
//!   edit resets it (a model re-running tests after an edit is never caught), and other commands in between (probes
//!   like `echo test`) neither count nor reset.
//!
//! Port note: Marv measures lengths in UTF-16 units; here the text thresholds count `char`s.

use serde_json::Value;

/// Consecutive identical sentences that make a text loop.
pub const TEXT_REPEATS: usize = 8;
/// How much of the reply's end is checked, in characters.
pub const TEXT_WINDOW: usize = 4096;
/// A unit shorter than this is never "sentence-like".
pub const MIN_SENTENCE_CHARS: usize = 20;
/// Steps of tool calls remembered.
pub const TOOL_HISTORY: usize = 8;
/// Tools whose use resets the tool-loop history.
pub const WRITE_TOOLS: [&str; 3] = ["edit_file", "write_file", "bash"];

/// Splits text into sentence-like units: on `. ! ?` followed by whitespace or the end, and on newlines.
pub fn split_sentences(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((i, ch)) = chars.next() {
        let next = chars.peek().map(|&(_, c)| c);
        let ends = ch == '\n' || (matches!(ch, '.' | '!' | '?') && next.is_none_or(char::is_whitespace));
        if ends {
            let end = i + ch.len_utf8();
            let unit = text[start..end].trim();
            if !unit.is_empty() {
                parts.push(unit.to_string());
            }
            start = end;
        }
    }
    let rest = text[start..].trim();
    if !rest.is_empty() {
        parts.push(rest.to_string());
    }
    if parts.is_empty() {
        // Marv's fallback (only reachable for whitespace-only text, which yields nothing either).
        return text.split('\n').filter(|s| !s.is_empty()).map(str::to_string).collect();
    }
    parts
}

/// Whether the reply text so far shows degenerate repetition (see the module notes).
pub fn detect_text_repetition(text: &str) -> bool {
    let len = text.chars().count();
    if len < MIN_SENTENCE_CHARS {
        return false;
    }
    let tail = if len > TEXT_WINDOW {
        let (at, _) = text.char_indices().nth(len - TEXT_WINDOW).expect("in range");
        &text[at..]
    } else {
        text
    };
    let units: Vec<String> = split_sentences(tail).into_iter().map(|u| u.trim().to_lowercase()).collect();
    if units.len() < TEXT_REPEATS {
        return false;
    }
    let mut consecutive = 1;
    for i in (0..units.len() - 1).rev() {
        if units[i] == units[i + 1] && is_sentence_like(&units[i]) {
            consecutive += 1;
            if consecutive >= TEXT_REPEATS {
                return true;
            }
        } else {
            consecutive = 1;
        }
    }
    false
}

/// At least 20 characters and at least 2 whitespace-separated words.
fn is_sentence_like(unit: &str) -> bool {
    unit.chars().count() >= MIN_SENTENCE_CHARS && unit.split_whitespace().count() >= 2
}

/// A normalized key for a tool call, compared across steps.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolCallKey {
    pub name: String,
    /// Arguments, parsed and re-printed with keys sorted at every depth; the raw text when not JSON.
    pub args: String,
}

/// State kept across steps for tool-call loop detection.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolLoopState {
    /// Recent steps' calls (at most [`TOOL_HISTORY`]).
    pub history: Vec<Vec<ToolCallKey>>,
    /// Whether the model was already warned about a loop.
    pub warned: bool,
}

/// Prints `value` as compact JSON with every object's keys sorted, at every depth (nothing dropped).
/// Done by hand so it doesn't depend on whether serde_json's `preserve_order` feature is on.
fn canonical(value: &Value, out: &mut String) {
    match value {
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                canonical(&map[key], out);
            }
            out.push('}');
        }
        leaf => out.push_str(&leaf.to_string()),
    }
}

pub fn make_tool_key(name: &str, arguments: &str) -> ToolCallKey {
    let args = match serde_json::from_str::<Value>(arguments) {
        Ok(value) => {
            let mut out = String::new();
            canonical(&value, &mut out);
            out
        }
        Err(_) => arguments.to_string(),
    };
    ToolCallKey { name: name.to_string(), args }
}

/// A period-1 or period-2 cycle at the end of `history`. The most recent steps decide: what came
/// before a loop started (its first call, which had no earlier result to repeat) doesn't hide it.
fn cycle_detected(history: &[Vec<ToolCallKey>]) -> bool {
    let n = history.len();
    // Period 1: the last 3 steps are the same.
    if n >= 3 && history[n - 1] == history[n - 2] && history[n - 2] == history[n - 3] {
        return true;
    }
    // Period 2: the last 5 steps go A,B,A,B,A (with A ≠ B).
    if n >= 5 && history[n - 1] != history[n - 2] {
        return (n - 5..n - 2).all(|i| history[i] == history[i + 2]);
    }
    false
}

/// Adds a step's calls to the state. Returns true when a loop is detected.
pub fn tool_loop_detected(state: &mut ToolLoopState, calls: Vec<ToolCallKey>) -> bool {
    // A write tool resets the entire history: re-reading a file after editing it is legitimate.
    if calls.iter().any(|c| WRITE_TOOLS.contains(&c.name.as_str())) {
        state.history = vec![calls];
        return false;
    }
    // Only steps with the same number of calls are comparable.
    if state.history.last().is_some_and(|prev| prev.len() != calls.len()) {
        state.history = vec![calls];
        return false;
    }
    state.history.push(calls);
    // Enough to spot periods 1 and 2, cheap.
    if state.history.len() > TOOL_HISTORY {
        state.history.remove(0);
    }
    cycle_detected(&state.history)
}

/// Failures of the same command, with the same signature, that make another try a loop.
pub const FAILURE_REPEATS: usize = 2;
/// Tools that only read: a call to one keeps the failures. Any other tool besides bash (`edit_file`, `write_file`, a
/// subagent that edits, `process` starting the server a test needed, an MCP tool) may have changed what the command
/// fails on, so a call to it forgets every failure (the next try may fare differently).
pub const READ_ONLY_TOOLS: [&str; 6] = ["read_file", "glob", "grep", "web_fetch", "plan", "skill"];
/// A signature is cut to this many characters.
const SIGNATURE_CHARS: usize = 160;

/// How bash's network note (`marv_sandbox::notes::NETWORK_NOTE`, #23) starts; marv-agent doesn't depend on
/// marv-sandbox, so marv-session's tests hold the two together.
pub const NETWORK_NOTE_START: &str = "Marv: this command ran in the sandbox without network";

/// A line that says something failed, whatever the exit code (`git clone … || true` still printed it).
fn strong_error(lower: &str) -> bool {
    const STARTS: [&str; 5] = ["fatal:", "error:", "error[", "npm err!", "err!"];
    const CONTAINS: [&str; 13] = [
        "could not resolve",
        "temporary failure in name resolution",
        "name or service not known",
        "network is unreachable",
        "connection refused",
        "connection timed out",
        "failed to connect",
        "unable to access",
        "no route to host",
        "permission denied",
        "no such file or directory",
        "command not found",
        "no one to ask",
    ];
    STARTS.iter().any(|s| lower.starts_with(s)) || CONTAINS.iter().any(|c| lower.contains(c))
}

/// A line that may say what failed, counted only when the command did fail ("0 failed" in a passing run isn't one).
fn weak_error(lower: &str) -> bool {
    const WORDS: [&str; 8] = ["error", "failed", "failure", "cannot", "can't", "unable", "not found", "denied"];
    WORDS.iter().any(|w| lower.contains(w))
}

/// Lowercased, digits as `#`, whitespace collapsed: a line number or a duration doesn't make a new error.
fn normalize_line(line: &str) -> String {
    let mapped: String = line.chars().map(|c| if c.is_ascii_digit() { '#' } else { c.to_ascii_lowercase() }).collect();
    mapped.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// What a bash result's failure looks like, or `None` when it didn't fail. Marv's own `Marv:` lines are skipped. Its
/// first line that says something failed whatever the exit code (`fatal:`, `could not resolve host`, …); for a failed
/// one (an exit code other than 0, a timeout, or no ending at all: refused, declined), else its first line with an
/// error word, else its ending and last line. Each as `(normalized, shown)`.
pub fn failure_signature(result: &str) -> Option<(String, String)> {
    let lines: Vec<&str> = result.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with("Marv:")).collect();
    let ending = lines.iter().rev().find(|l| l.starts_with("[exit code ") || l.starts_with("[timed out")).copied();
    // A command that exited 0 worked (`grep -i error app.log` prints errors and succeeds), unless bash's network note
    // says `|| true` hid a failure like the missing network's.
    if ending == Some("[exit code 0]") && !result.contains(NETWORK_NOTE_START) {
        return None;
    }
    let pick = |line: &str| {
        let shown: String = line.chars().take(SIGNATURE_CHARS).collect();
        Some((normalize_line(&shown), shown))
    };
    if let Some(line) = lines.iter().find(|l| strong_error(&l.to_lowercase())) {
        return pick(line);
    }
    if ending == Some("[exit code 0]") {
        return None;
    }
    if let Some(line) = lines.iter().find(|l| weak_error(&l.to_lowercase())) {
        return pick(line);
    }
    let last = lines.iter().rev().find(|l| Some(**l) != ending).copied().unwrap_or("");
    pick(&format!("{} {last}", ending.unwrap_or("")))
}

/// A bash call's command.
fn command_of(arguments: &str) -> Option<String> {
    serde_json::from_str::<Value>(arguments).ok()?.get("command")?.as_str().map(str::to_string)
}

/// The failing-command key of a call: bash only, by its command's key and the flags that change where it runs.
pub fn command_call_key(name: &str, arguments: &str) -> Option<String> {
    if name != "bash" {
        return None;
    }
    let args = serde_json::from_str::<Value>(arguments).ok()?;
    let command = args.get("command")?.as_str()?;
    let net = if args.get("network").and_then(Value::as_bool) == Some(true) { "net:" } else { "" };
    let host = if args.get("sandbox").and_then(Value::as_bool) == Some(false) { "host:" } else { "" };
    Some(format!("{net}{host}{}", marv_core::command::command_key(command)))
}

/// One command's failures in a row: (normalized signature, the line shown, how many times).
#[derive(Debug, Clone, PartialEq)]
struct Failures {
    signature: String,
    shown: String,
    count: usize,
}

/// Failing bash commands since the last call that may have changed things (see the module notes).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FailureLoopState {
    failures: std::collections::HashMap<String, Failures>,
}

impl FailureLoopState {
    /// Notes a call's result: a call that may change things (an edit, a subagent, `process`) forgets everything; a bash command's failure counts (the same signature as
    /// last time: one more; another: the first), its success forgets it.
    pub fn record(&mut self, name: &str, arguments: &str, result: &str) {
        if name != "bash" {
            if !READ_ONLY_TOOLS.contains(&name) {
                self.failures.clear();
            }
            return;
        }
        let Some(key) = command_call_key(name, arguments) else { return };
        // Only Marv's own words (the note of a call this guard didn't run): nothing ran, nothing to learn.
        if result.lines().map(str::trim).all(|l| l.is_empty() || l.starts_with("Marv:")) {
            return;
        }
        match failure_signature(result) {
            // A success that may have changed things (`npm install`) gives every command a fresh start; a probe
            // that only reads (`echo test`, `whoami`) changes nothing.
            None if !command_of(arguments).is_some_and(|c| marv_core::command::is_read_only_command(&c)) => {
                self.failures.clear();
            }
            None => {
                self.failures.remove(&key);
            }
            Some((signature, shown)) => {
                let count = self.failures.get(&key).filter(|f| f.signature == signature).map_or(0, |f| f.count);
                self.failures.insert(key, Failures { signature, shown, count: count + 1 });
            }
        }
    }

    /// For a call that would be another try of a command that already failed the same way [`FAILURE_REPEATS`]
    /// times: how many times, and the line its failure showed.
    pub fn repeated(&self, name: &str, arguments: &str) -> Option<(usize, String)> {
        let key = command_call_key(name, arguments)?;
        self.failures.get(&key).filter(|f| f.count >= FAILURE_REPEATS).map(|f| (f.count, f.shown.clone()))
    }
}

/// The result of a call that wasn't run because its command keeps failing the same way.
pub fn failing_command_note(count: usize, shown: &str) -> String {
    format!(
        "Marv: not run. This command already failed {count} times in a row the same way (\"{shown}\"), with nothing changed in between, so another try would fail too (a different tail on the command doesn't change that). Use what it said: change the approach, do without it, or tell the user what's in the way."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ported from tests/repetition.test.ts.

    #[test]
    fn splits_on_period_followed_by_space() {
        assert_eq!(split_sentences("Hello world. How are you? I'm fine."), ["Hello world.", "How are you?", "I'm fine."]);
    }

    #[test]
    fn splits_on_newlines_too() {
        assert_eq!(split_sentences("Line one.\nLine two.\nLine three."), ["Line one.", "Line two.", "Line three."]);
    }

    #[test]
    fn a_run_on_without_punctuation_is_one_unit() {
        assert_eq!(
            split_sentences("Fix the test scripts now Fix the test scripts now"),
            ["Fix the test scripts now Fix the test scripts now"]
        );
    }

    #[test]
    fn empty_input_returns_empty() {
        assert!(split_sentences("").is_empty());
    }

    #[test]
    fn a_sentence_repeated_50_times_without_newlines_is_caught() {
        assert!(detect_text_repetition(&"Fix the test scripts now. ".repeat(50)));
    }

    #[test]
    fn a_normal_paragraph_with_two_repeated_sentences_is_not_caught() {
        let text = "I looked at the file and it needs work. Fix the test scripts now. Fix the test scripts now. The rest of the function looks okay.";
        assert!(!detect_text_repetition(text));
    }

    #[test]
    fn a_sentence_repeated_7_times_is_not_caught() {
        assert!(!detect_text_repetition(&["Fix the test scripts now."; 7].join(". ")));
    }

    #[test]
    fn different_sentences_are_not_caught() {
        let text = "First step: install dependencies. Second step: configure the module. Third step: run the tests. Fourth step: verify the output.";
        assert!(!detect_text_repetition(text));
    }

    #[test]
    fn short_units_are_ignored() {
        assert!(!detect_text_repetition(&["- milk"; 10].join("\n")));
    }

    #[test]
    fn blank_lines_do_not_trigger() {
        let text = format!("Some text{}", "\n".repeat(10));
        assert!(!detect_text_repetition(&text));
    }

    #[test]
    fn code_with_different_lines_is_not_caught() {
        let text = (1..=9).map(|i| format!("const v{i} = {i};")).collect::<Vec<_>>().join("\n");
        assert!(!detect_text_repetition(&text));
    }

    #[test]
    fn fewer_than_8_units_does_not_trigger() {
        assert!(!detect_text_repetition("One line."));
        assert!(!detect_text_repetition(""));
    }

    #[test]
    fn a_sentence_repeated_8_times_with_newlines_is_caught() {
        assert!(detect_text_repetition(&["Fix the test scripts now."; 8].join("\n")));
    }

    #[test]
    fn a_burst_at_the_end_of_a_long_reply_is_caught() {
        let text = format!(
            "{}{}",
            "Varied words go here and there. ".chars().cycle().take(10_000).collect::<String>().replace('.', ","),
            "Fix the test scripts now. ".repeat(10)
        );
        assert!(detect_text_repetition(&text));
    }

    #[test]
    fn parsed_json_arguments_are_normalized() {
        assert_eq!(
            make_tool_key("read_file", r#"{"path": "a.ts"}"#),
            ToolCallKey { name: "read_file".into(), args: r#"{"path":"a.ts"}"#.into() }
        );
    }

    #[test]
    fn unparseable_json_is_kept_as_is() {
        assert_eq!(make_tool_key("bash", "not json").args, "not json");
    }

    #[test]
    fn differently_ordered_keys_are_normalized() {
        assert_eq!(
            make_tool_key("read_file", r#"{"path": "a.ts", "offset": 1}"#),
            make_tool_key("read_file", r#"{"offset": 1, "path": "a.ts"}"#)
        );
    }

    #[test]
    fn nested_fields_count_and_key_order_at_any_depth_does_not() {
        let key = |args: &str| make_tool_key("q", args).args;
        assert_ne!(key(r#"{"filter":{"id":1}}"#), key(r#"{"filter":{"id":2}}"#));
        assert_eq!(
            key(r#"{"a":1,"filter":{"x":1,"y":[{"b":2,"a":1}]}}"#),
            key(r#"{"filter":{"y":[{"a":1,"b":2}],"x":1},"a":1}"#)
        );
    }

    fn read(args: &str) -> ToolCallKey {
        ToolCallKey { name: "read_file".into(), args: args.into() }
    }

    #[test]
    fn a_single_step_is_nothing() {
        let mut state = ToolLoopState::default();
        assert!(!tool_loop_detected(&mut state, vec![read("a")]));
        assert_eq!(state.history, vec![vec![read("a")]]);
    }

    #[test]
    fn period_1_is_detected() {
        let mut state = ToolLoopState::default();
        tool_loop_detected(&mut state, vec![read("a")]);
        tool_loop_detected(&mut state, vec![read("a")]);
        assert!(tool_loop_detected(&mut state, vec![read("a")]));
    }

    #[test]
    fn period_2_is_detected() {
        let mut state = ToolLoopState::default();
        for args in ["95", "98", "95", "98"] {
            assert!(!tool_loop_detected(&mut state, vec![read(args)]));
        }
        assert!(tool_loop_detected(&mut state, vec![read("95")]));
    }

    #[test]
    fn period_3_does_not_trigger() {
        let mut state = ToolLoopState::default();
        for i in 0..6 {
            tool_loop_detected(&mut state, vec![read(["a", "b", "c"][i % 3])]);
        }
        assert!(state.history.len() <= TOOL_HISTORY);
        assert!(!tool_loop_detected(&mut state, vec![read("a")]));
    }

    #[test]
    fn a_write_tool_resets_the_history() {
        let mut state = ToolLoopState::default();
        tool_loop_detected(&mut state, vec![read("a")]);
        tool_loop_detected(&mut state, vec![read("a")]);
        tool_loop_detected(&mut state, vec![ToolCallKey { name: "write_file".into(), args: "{}".into() }]);
        assert_eq!(state.history.len(), 1);
        tool_loop_detected(&mut state, vec![read("a")]);
        assert_eq!(state.history.len(), 2);
        assert_eq!(state.history[0][0].name, "write_file");
    }

    #[test]
    fn a_step_with_a_different_number_of_calls_resets() {
        let mut state = ToolLoopState::default();
        tool_loop_detected(&mut state, vec![read("a")]);
        tool_loop_detected(&mut state, vec![read("a")]);
        tool_loop_detected(&mut state, vec![read("a"), read("b")]);
        assert_eq!(state.history.len(), 1);
    }

    // Issue #81: failing commands whose tail differs.

    fn bash(command: &str) -> String {
        serde_json::json!({ "command": command }).to_string()
    }
    const OFFLINE: &str = "Cloning into 'marv'...\nfatal: unable to access 'https://github.com/Flologix/marv/': Could not resolve host: github.com\n\n[exit code 128]";

    #[test]
    fn the_signature_is_the_first_line_that_says_what_failed() {
        let (normalized, shown) = failure_signature(OFFLINE).unwrap();
        assert!(shown.starts_with("fatal: unable to access"));
        assert_eq!(normalized, shown.to_lowercase());
        // Exit 0 isn't a failure, unless bash's network note says `|| true` hid one.
        let hidden = OFFLINE.replace("[exit code 128]", "[exit code 0]");
        assert!(failure_signature(&hidden).is_none());
        let noted = OFFLINE.replace("[exit code 128]", &format!("{NETWORK_NOTE_START}.\n\n[exit code 0]"));
        assert_eq!(failure_signature(&noted).unwrap().1, shown);
        // Line numbers and durations don't make it new.
        assert_eq!(
            failure_signature("error: x.rs:12 broke\n\n[exit code 1]").unwrap().0,
            failure_signature("error: x.rs:13 broke\n\n[exit code 1]").unwrap().0
        );
        // Marv's notes aren't the command's output.
        assert!(failure_signature("ok\n\n[exit code 0]\n\nMarv: this command failed before").is_none());
    }

    #[test]
    fn a_passing_run_has_no_signature_even_with_error_words_in_it() {
        assert!(failure_signature("test result: ok. 12 passed; 0 failed\n\n[exit code 0]").is_none());
        assert!(failure_signature("(no output)\n\n[exit code 0]").is_none());
        // A failed one without error words is its ending and last line; one without an ending (refused) its line.
        assert_eq!(failure_signature("(no output)\n\n[exit code 1]").unwrap().1, "[exit code 1] (no output)");
        assert!(failure_signature("bash needs the user's approval, and there's no one to ask.").is_some());
    }

    #[test]
    fn the_same_failure_twice_makes_the_next_try_a_loop_whatever_its_tail() {
        let mut state = FailureLoopState::default();
        state.record("bash", &bash("git clone https://github.com/Flologix/marv 2>&1 | tail -2"), OFFLINE);
        assert!(state.repeated("bash", &bash("git clone https://github.com/Flologix/marv")).is_none());
        // A probe in between neither counts nor resets.
        state.record("bash", &bash("echo test"), "test\n\n[exit code 0]");
        state.record("bash", &bash("git clone https://github.com/Flologix/marv && du -sh marv"), OFFLINE);
        let (count, shown) = state.repeated("bash", &bash("git clone https://github.com/Flologix/marv || true")).unwrap();
        assert_eq!(count, 2);
        assert!(shown.contains("Could not resolve host"));
        assert!(failing_command_note(count, &shown).starts_with("Marv: not run. This command already failed 2 times"));
        // With network it's another command; so is another clone.
        assert!(
            state.repeated("bash", r#"{"command": "git clone https://github.com/Flologix/marv", "network": true}"#).is_none()
        );
        assert!(state.repeated("bash", &bash("git clone https://github.com/Flologix/other")).is_none());
        assert!(state.repeated("read_file", r#"{"path": "a"}"#).is_none());
    }

    #[test]
    fn an_edit_a_success_or_another_error_resets_it() {
        let test = bash("cargo test");
        let failing = "error[E0308]: mismatched types\n\n[exit code 101]";
        let twice = || {
            let mut state = FailureLoopState::default();
            state.record("bash", &test, failing);
            state.record("bash", &test, failing);
            assert!(state.repeated("bash", &test).is_some());
            state
        };
        // Re-running the tests after an edit is never a loop.
        let mut edited = twice();
        edited.record("edit_file", r#"{"path": "src/a.rs"}"#, "Edited src/a.rs");
        assert!(edited.repeated("bash", &test).is_none());
        let mut passed = twice();
        passed.record("bash", &test, "test result: ok\n\n[exit code 0]");
        assert!(passed.repeated("bash", &test).is_none());
        let mut other = twice();
        other.record("bash", &test, "error[E0425]: cannot find value\n\n[exit code 101]");
        assert!(other.repeated("bash", &test).is_none());
        other.record("bash", &test, "error[E0425]: cannot find value\n\n[exit code 101]");
        assert!(other.repeated("bash", &test).is_some());
    }

    // Greptile on #111: the guard must never block useful work.

    #[test]
    fn a_successful_read_of_error_text_never_counts() {
        let mut state = FailureLoopState::default();
        let grep = bash("grep -i error app.log");
        for _ in 0..4 {
            state.record("bash", &grep, "error: disk full at 10:42\nerror: retrying\n\n[exit code 0]");
        }
        assert!(state.repeated("bash", &grep).is_none());
        assert!(failure_signature("fatal: something\n\n[exit code 0]").is_none());
    }

    #[test]
    fn a_clone_hidden_by_or_true_still_counts_when_it_carries_the_network_note() {
        let hidden = format!(
            "fatal: unable to access 'https://github.com/x/y/': Could not resolve host: github.com\n\n{NETWORK_NOTE_START}, and its failure looks like it may come from that.\n\n[exit code 0]"
        );
        let mut state = FailureLoopState::default();
        let clone = bash("git clone https://github.com/x/y || true");
        state.record("bash", &clone, &hidden);
        state.record("bash", &clone, &hidden);
        assert!(state.repeated("bash", &bash("git clone https://github.com/x/y")).is_some());
    }

    #[test]
    fn a_successful_command_that_may_change_things_resets_every_count_a_probe_doesnt() {
        let test = bash("npm test");
        let failing = "error: 3 tests failed\n\n[exit code 1]";
        let twice = || {
            let mut state = FailureLoopState::default();
            state.record("bash", &test, failing);
            state.record("bash", &test, failing);
            state
        };
        let mut probed = twice();
        probed.record("bash", &bash("echo test"), "test\n\n[exit code 0]");
        probed.record("bash", &bash("whoami"), "me\n\n[exit code 0]");
        assert!(probed.repeated("bash", &test).is_some());
        let mut installed = twice();
        installed.record("bash", &bash("npm install"), "added 3 packages\n\n[exit code 0]");
        assert!(installed.repeated("bash", &test).is_none());
        // So does any tool that may change things, like a subagent that fixed the code or a server started for the
        // tests; a read doesn't.
        for tool in ["agent", "process", "mcp_call"] {
            let mut other = twice();
            other.record(tool, r#"{"prompt": "fix the tests"}"#, "Done: fixed src/a.rs");
            assert!(other.repeated("bash", &test).is_none(), "{tool}");
        }
        let mut read = twice();
        read.record("read_file", r#"{"path": "src/a.rs"}"#, "fn a() {}");
        read.record("grep", r#"{"pattern": "a"}"#, "src/a.rs:1: fn a() {}");
        assert!(read.repeated("bash", &test).is_some());
        // A failed install changed nothing that counts.
        let mut failed = twice();
        failed.record("bash", &bash("npm install"), "npm ERR! network\n\n[exit code 1]");
        assert!(failed.repeated("bash", &test).is_some());
    }
}
