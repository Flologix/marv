//! A tool call the model is still writing. Ports Marv's `src/tool-draft.ts`.
//!
//! Its arguments arrive as fragments of JSON, and a `write_file`'s content can take minutes (18k tokens for one page,
//! in a trajectory). These read what's arrived so far, unfinished JSON and all, so the screen can say what's being
//! written instead of looking hung. Used by the TUI's waiting line (`LoopEvent::ToolCallDelta`).

use regex::Regex;

/// The value of `key` in unfinished JSON, decoded as far as it has arrived; `None` if it hasn't started.
fn partial_string(json: &str, key: &str) -> Option<String> {
    let re = Regex::new(&format!(r#""{}"\s*:\s*"((?:[^"\\]|\\.)*)"#, regex::escape(key))).ok()?;
    let raw = re.captures(json)?.get(1)?.as_str();
    // An escape cut in half: a lone backslash at the end, or \u with fewer than 4 hex digits.
    let cut = Regex::new(r"\\(u[0-9a-fA-F]{0,3})?$").ok()?;
    let raw = cut.replace(raw, "");
    serde_json::from_str::<String>(&format!("\"{raw}\"")).ok()
}

/// "Writing src/app.js…": what the call is about to do, for the waiting line.
pub fn draft_label(name: &str, args: &str) -> String {
    let path =
        partial_string(args, "path").map(|p| p.split('\n').next().unwrap_or_default().to_string()).filter(|p| !p.is_empty());
    match name {
        "write_file" => format!("Writing {}…", path.as_deref().unwrap_or("a file")),
        "edit_file" => format!("Editing {}…", path.as_deref().unwrap_or("a file")),
        "bash" => "Writing a command…".into(),
        "" => "Preparing a tool call…".into(),
        other => format!("Preparing {other}…"),
    }
}

/// The code written so far (a file's content, an edit's new text, a command), or "" for anything else.
pub fn draft_preview(name: &str, args: &str) -> String {
    let key = match name {
        "write_file" => "content",
        "edit_file" => "new_string",
        "bash" => "command",
        _ => return String::new(),
    };
    partial_string(args, key).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_the_file_once_its_path_has_arrived() {
        assert_eq!(draft_label("write_file", r#"{"path": "src/app.js", "content": "x"#), "Writing src/app.js…");
        assert_eq!(draft_label("edit_file", r#"{"path":"a.ts","old_string":""#), "Editing a.ts…");
        assert_eq!(draft_label("write_file", r#"{"pa"#), "Writing a file…");
    }

    #[test]
    fn other_tools() {
        assert_eq!(draft_label("bash", r#"{"command": "bun te"#), "Writing a command…");
        assert_eq!(draft_label("agent", "{"), "Preparing agent…");
        assert_eq!(draft_label("", ""), "Preparing a tool call…");
    }

    #[test]
    fn an_escaped_path_is_decoded_and_only_its_first_line_kept() {
        assert_eq!(draft_label("write_file", r#"{"path": "dir/a \"b\".js""#), r#"Writing dir/a "b".js…"#);
        assert_eq!(draft_label("write_file", r#"{"path": "a\nb""#), "Writing a…");
    }

    #[test]
    fn previews_decode_unfinished_values() {
        assert_eq!(
            draft_preview("write_file", r#"{"path":"a.js","content":"const a = 1;\nconst b = \"x"#),
            "const a = 1;\nconst b = \"x"
        );
        assert_eq!(
            draft_preview("edit_file", r#"{"path":"a.js","old_string":"a","new_string":"if (x) {\n\treturn 1;"#),
            "if (x) {\n\treturn 1;"
        );
        assert_eq!(draft_preview("bash", r#"{"command":"bun test && echo ok"}"#), "bun test && echo ok");
    }

    #[test]
    fn an_escape_cut_in_half_isnt_shown_half_decoded() {
        assert_eq!(draft_preview("write_file", r#"{"content":"a\"#), "a");
        assert_eq!(draft_preview("write_file", r#"{"content":"a\u00"#), "a");
        assert_eq!(draft_preview("write_file", r#"{"content":"a\u00e9b"#), "aéb");
    }

    #[test]
    fn nothing_yet_or_no_code_is_empty_and_a_complete_value_stops_at_its_quote() {
        assert_eq!(draft_preview("write_file", r#"{"path":"a.js""#), "");
        assert_eq!(draft_preview("read_file", r#"{"path":"a.js"}"#), "");
        assert_eq!(draft_preview("write_file", r#"{"content":"done","path":"a.js"}"#), "done");
    }
}
