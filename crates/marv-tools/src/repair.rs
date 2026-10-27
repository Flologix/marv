//! Repairs for malformed tool calls (issue #10): fixes so obvious that doing them beats bouncing the call back to
//! the model, which then burns a step re-sending what it meant. Only unambiguous cases are repaired; anything else
//! stays an error the model can recover from. Every repair says what it did (a `Marv:` note on the result), so the
//! model learns the right form.
//!
//! Ports Marv's `src/tools/repair.ts`. Two kinds, applied in `run_tool`:
//! - [`repair_json_args`]: raw control characters inside JSON strings (literal tabs and newlines), which a JSON
//!   parser rejects out of hand. Outside strings they are legal whitespace (a pretty-printed call), so they don't
//!   count, and broken JSON of any other kind is left to the ordinary error.
//! - a tool's `repair(raw, ctx)`: bash's `timeout` given in milliseconds ([`repair_timeout_seconds`]) and a project
//!   path written with a leading slash ([`repair_path_field`]).
//!
//! - marv-rs: [`repair_tool_name`], a tool name with the model's chat-template markup or prose leaked into it (GLM
//!   in a benchmark run called `"\nThe existing routes.json has … <|tool_call>call:read_file"`):
//!   the known tool name at its end, after a markup boundary, is the call it meant. `run_tool` runs that tool with a
//!   note; the call in the history keeps the name the model sent (prompt cache).
//!
//! Schema-guided repair (#65: a number sent as a string, a single value where the schema wants an array, an unknown
//! field) is generic, not a tool's own: [`crate::schema_repair`], run by `run_tool` before the tool's `repair`.

use std::path::Path;

use serde_json::Value;

use crate::types::Repaired;

/// A repaired call's explanation, added to the result the model reads.
pub type RepairNote = Option<String>;

#[derive(Debug, Clone, PartialEq)]
pub struct JsonRepair {
    /// The parsed arguments, as the parser would have given them.
    pub value: Value,
    pub note: RepairNote,
}

/// The label for a control character in the note, for the model to avoid it.
fn describe_code(code: u32) -> String {
    match code {
        9 => "tab".into(),
        10 => "newline".into(),
        13 => "carriage return".into(),
        _ => format!("U+{code:02X}"),
    }
}

/// Escapes raw control characters inside JSON string literals, so a call that put a literal tab or newline in a
/// string parses. Valid JSON, JSON without such characters, and JSON still broken after escaping give None.
pub fn repair_json_args(args: &str) -> Option<JsonRepair> {
    if serde_json::from_str::<Value>(args).is_ok() {
        return None; // valid: nothing to repair
    }
    let mut out = String::with_capacity(args.len() + 16);
    let mut found: Vec<u32> = Vec::new();
    let (mut in_string, mut escaped) = (false, false);
    for ch in args.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            } else if (ch as u32) < 0x20 {
                found.push(ch as u32);
                // The short escapes where JSON has one; any other control character (a vertical tab, NUL…) as \u00XX.
                match ch {
                    '\u{8}' => out.push_str("\\b"),
                    '\t' => out.push_str("\\t"),
                    '\n' => out.push_str("\\n"),
                    '\u{c}' => out.push_str("\\f"),
                    '\r' => out.push_str("\\r"),
                    _ => out.push_str(&format!("\\u{:04x}", ch as u32)),
                }
                continue;
            }
        } else if ch == '"' {
            in_string = true;
        }
        out.push(ch);
    }
    if found.is_empty() {
        return None; // not ours to guess (truncated output, a missing brace)
    }
    let value = serde_json::from_str::<Value>(&out).ok()?; // escaping didn't make it valid: leave the honest error
    let mut names: Vec<String> = Vec::new();
    for code in &found {
        let name = describe_code(*code);
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let n = found.len();
    Some(JsonRepair {
        value,
        note: Some(format!(
            "Marv: the arguments contained {n} raw control character{} inside JSON strings ({}), which JSON doesn't allow; they were escaped and the call ran. Quote them as \\t, \\n or \\r next time.",
            if n == 1 { "" } else { "s" },
            names.join(", ")
        )),
    })
}

/// A path a tool was given, as the project means it: written with a leading slash, missing there, but present
/// beside the project's files (`/src/pagination.js` while the file is `src/pagination.js`). Absolute paths that exist
/// and anything that doesn't exist either way are left alone: an error the model can act on, not a guess.
pub fn repair_project_path(root: &Path, path: &str) -> (String, RepairNote) {
    let unchanged = || (path.to_string(), None);
    // An empty name, "//", "//x" (POSIX's implementation-defined root), a climb out of the project and NUL are not
    // readable as a project path.
    let Some(relative) = path.strip_prefix('/') else { return unchanged() };
    if relative.is_empty() || relative.starts_with('/') || relative.split('/').any(|p| p == "..") || relative.contains('\0') {
        return unchanged();
    }
    if Path::new(path).exists() {
        return unchanged(); // it exists where it was written: meant as written
    }
    if !root.join(relative).exists() {
        return unchanged();
    }
    (
        relative.to_string(),
        Some(format!(
            "Marv: the path was written with a leading slash ({path}): used {relative}, which is where the file is in this project."
        )),
    )
}

/// A timeout far above the maximum that is a whole number of milliseconds (at least one second's worth) was meant in
/// milliseconds: no model means a fifth of a second of a 600 s maximum. Divided and clamped; anything else (750, 0,
/// 60) could still be seconds, so it fails with the ordinary error.
pub fn repair_timeout_seconds(timeout: f64, max_seconds: u64) -> (f64, RepairNote) {
    let max = max_seconds as f64;
    if timeout.fract() == 0.0 && timeout >= 1000.0 && timeout % 1000.0 == 0.0 && timeout > max {
        let seconds = (timeout / 1000.0).min(max);
        return (
            seconds,
            Some(format!("Marv: the timeout was given in milliseconds ({timeout}): used {seconds} s. Give timeout in seconds.")),
        );
    }
    (timeout, None)
}

/// Appends a repair's note to the tool's output (both are read by the model).
pub fn with_note(output: &str, note: Option<&str>) -> String {
    match note {
        Some(note) => format!("{output}\n\n{note}"),
        None => output.to_string(),
    }
}

/// The tool a garbled name meant (marv-rs): `name` ends with exactly one of `known` after leaked markup or prose
/// (`…<|tool_call>call:read_file`, `functions.bash`, `edit_file<|tool_call_end|>`, `\nSome text read_file`), and
/// the note that says so. Only that shape: the name must end with the tool's whole name, right after `call:`, `>`,
/// whitespace, `functions.`/`tools.`, or before markup tags that were stripped from the end. Anything else (a
/// misspelling, a prefix like `my_read_file`) stays unknown: guessing would run a tool the model didn't name.
pub fn repair_tool_name<'a>(name: &str, known: impl IntoIterator<Item = &'a str>) -> Option<(String, String)> {
    let mut rest = name.trim_end();
    // Trailing tags (`<|tool_call_end|>`, `</tool_call>`, DeepSeek's `<｜tool▁call▁end｜>`).
    let mut stripped = false;
    while rest.ends_with('>') {
        let Some(open) = rest.rfind('<') else { break };
        rest = rest[..open].trim_end();
        stripped = true;
    }
    let tail_len: usize =
        rest.chars().rev().take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').map(char::len_utf8).sum();
    let (prefix, candidate) = rest.split_at(rest.len() - tail_len);
    if candidate.is_empty() || candidate == name {
        return None;
    }
    let boundary = stripped && prefix.is_empty()
        || prefix.ends_with("call:")
        || prefix.ends_with('>')
        || prefix.ends_with(char::is_whitespace)
        || prefix.ends_with("functions.")
        || prefix.ends_with("tools.");
    if !boundary {
        return None;
    }
    let tool = known.into_iter().find(|k| *k == candidate)?;
    let shown: String = {
        let chars: Vec<char> = name.chars().filter(|c| !c.is_control()).collect();
        let tail: String = chars[chars.len().saturating_sub(60)..].iter().collect();
        if chars.len() > 60 { format!("…{tail}") } else { tail }
    };
    let note = format!(
        "Marv: the tool name {shown:?} had other text in it; it was run as {tool}. Next time, send only the tool's name as the name, and nothing else."
    );
    Some((tool.to_string(), note))
}

/// The `repair` of the tools that take a project `path` (read_file, glob, grep, edit_file, write_file). No object,
/// or no string path (glob and grep's is optional), is nothing to repair.
pub fn repair_path_field(raw: &Value, root: &Path) -> Option<Repaired> {
    let path = raw.as_object()?.get("path")?.as_str()?;
    let (fixed, note) = repair_project_path(root, path);
    note.as_ref()?;
    let mut input = raw.clone();
    input["path"] = Value::String(fixed);
    Some(Repaired { input, note })
}

/// The `repair` of bash: `timeout` in milliseconds.
pub fn repair_timeout_field(raw: &Value, max_seconds: u64) -> Option<Repaired> {
    let timeout = raw.as_object()?.get("timeout")?.as_f64()?;
    let (seconds, note) = repair_timeout_seconds(timeout, max_seconds);
    note.as_ref()?;
    let mut input = raw.clone();
    input["timeout"] = serde_json::json!(seconds as u64);
    Some(Repaired { input, note })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn raw_control_characters_inside_strings_are_escaped() {
        let saved = repair_json_args("{\"a\": \"x\ty\"}").unwrap();
        assert_eq!(saved.value, json!({"a": "x\ty"})); // the string's value is what the model meant
        assert!(
            saved.note.unwrap().starts_with("Marv: the arguments contained 1 raw control character inside JSON strings (tab)")
        );
        let two = repair_json_args("{\"old\": \"\tconst a;\", \"new\": \"\tconst b;\"}").unwrap();
        assert!(two.note.unwrap().contains("2 raw control characters"));
    }

    #[test]
    fn escapes_are_left_alone_and_whitespace_outside_strings_doesnt_rescue_broken_json() {
        assert!(repair_json_args(r#"{"a": "x\ty"}"#).is_none()); // backslash-t is already valid
        // Control characters without a short escape (vertical tab, NUL) are encoded as \u00XX, so they parse too.
        assert_eq!(repair_json_args("{\"a\": \"x\u{b}y\u{0}z\"}").unwrap().value, json!({"a": "x\u{b}y\u{0}z"}));
        assert!(repair_json_args("{\n\"a\": 1}").is_none()); // legal whitespace between tokens
        assert!(repair_json_args("{not json\t}").is_none());
        assert!(repair_json_args("{not json").is_none());
    }

    #[test]
    fn a_millisecond_timeout_is_divided_and_capped() {
        assert_eq!(repair_timeout_seconds(100_000.0, 600).0, 100.0);
        assert!(
            repair_timeout_seconds(100_000.0, 600)
                .1
                .unwrap()
                .contains("Marv: the timeout was given in milliseconds (100000): used 100 s")
        );
        assert_eq!(repair_timeout_seconds(1000.0, 600).0, 1.0);
        assert_eq!(repair_timeout_seconds(900_000.0, 600).0, 600.0);
        assert_eq!(repair_timeout_seconds(750.0, 600), (750.0, None)); // could still be seconds
        assert_eq!(repair_timeout_seconds(2.0, 600), (2.0, None));
        assert_eq!(
            repair_timeout_field(&json!({"command": "x", "timeout": 100000}), 600).unwrap().input,
            json!({"command": "x", "timeout": 100})
        );
    }

    #[test]
    fn a_leading_slash_path_that_exists_in_the_project_is_repaired() {
        let root = std::env::temp_dir().join(format!("marv-repair-{}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/pagination.js"), "x").unwrap();
        let (path, note) = repair_project_path(&root, "/src/pagination.js");
        assert_eq!(path, "src/pagination.js");
        assert!(
            note.unwrap()
                .contains("Marv: the path was written with a leading slash (/src/pagination.js): used src/pagination.js")
        );
        assert_eq!(repair_project_path(&root, "/etc/hosts").1, None); // exists as written
        assert_eq!(repair_project_path(&root, "/missing/thing.ts").1, None);
        assert_eq!(repair_project_path(&root, "/../secret.txt").1, None);
        assert_eq!(repair_project_path(&root, "//src/pagination.js").1, None);
        assert_eq!(repair_project_path(&root, "src/pagination.js").1, None);
        let field = repair_path_field(&json!({"pattern": "**/*.js", "path": "/src"}), &root).unwrap();
        assert_eq!(field.input["path"], "src");
        assert!(repair_path_field(&json!({"pattern": "x"}), &root).is_none());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn notes_are_appended_after_a_blank_line() {
        assert_eq!(with_note("out", Some("Marv: x")), "out\n\nMarv: x");
        assert_eq!(with_note("out", None), "out");
    }

    // marv-rs: a tool name with leaked markup or prose.

    const KNOWN: &[&str] = &["read_file", "edit_file", "bash", "glob"];

    #[test]
    fn a_tool_name_with_leaked_markup_is_the_tool_at_its_end() {
        let glm = "\nThe existing routes.json has two routes; let me read it.<|tool_call>call:read_file";
        let (tool, note) = repair_tool_name(glm, KNOWN.iter().copied()).unwrap();
        assert_eq!(tool, "read_file");
        assert!(note.starts_with("Marv: the tool name") && note.contains("it was run as read_file"), "{note}");
        assert!(!note.contains('\n'), "control characters stay out of the note");
        for (name, tool) in [
            ("functions.bash", "bash"),
            ("edit_file<|tool_call_end|>", "edit_file"),
            ("glob</tool_call>", "glob"),
            ("<tool_call>read_file", "read_file"),
            ("Now reading it read_file", "read_file"),
        ] {
            assert_eq!(repair_tool_name(name, KNOWN.iter().copied()).map(|r| r.0).as_deref(), Some(tool), "{name}");
        }
    }

    #[test]
    fn anything_else_stays_unknown() {
        for name in
            ["read_file", "readfile", "my_read_file", "read_files", "call:write_file", "<|tool_call>call:deploy", "", "  "]
        {
            assert_eq!(repair_tool_name(name, KNOWN.iter().copied()), None, "{name:?}");
        }
    }
}
