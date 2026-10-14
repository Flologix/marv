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
}
