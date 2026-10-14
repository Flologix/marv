//! Cutting long tool output for the model: the start and the end are kept, the middle is dropped.
//!
//! New module (no Marv file of its own): the cut from `formatOutput` in Marv's `src/tools/bash.ts`, shared because
//! MCP results (`src/mcp/tool.ts`) are capped at the same 30k characters.
//!
//! - 30k characters reach the model: the first 10k and the last 20k (errors and summaries are usually at the end).
//! - Counted in `char`s (Marv: UTF-16 units), and never split inside a character.
//! - Planned (#61): spill the cut middle to a file under the session's folder and name it in the marker, so the model
//!   can `read_file` the part it needs instead of re-running the command with `| head`.

/// What the model sees of a long output.
pub const MAX_MODEL_CHARS: usize = 30_000;
/// How much of the start is kept; the rest of the budget goes to the end.
pub const HEAD_CHARS: usize = 10_000;

/// `text` with its middle cut when longer than `max` characters: `head` characters, a marker, then the last
/// `max - head`.
pub fn cut_middle(text: &str, max: usize, head: usize) -> String {
    let total = text.chars().count();
    if total <= max {
        return text.to_string();
    }
    let byte_at = |n: usize| text.char_indices().nth(n).map_or(text.len(), |(i, _)| i);
    let tail = max - head;
    format!("{}\n\n… ({} characters omitted) …\n\n{}", &text[..byte_at(head)], total - max, &text[byte_at(total - tail)..])
}

/// [`cut_middle`] with the model's budget.
pub fn cut_for_model(text: &str) -> String {
    cut_middle(text, MAX_MODEL_CHARS, HEAD_CHARS)
}

/// Planned (#61): write the cut middle to a file and return the marker that names it.
pub fn spill_cut(_text: &str, _session_dir: &std::path::Path) -> std::io::Result<String> {
    todo!("port: #61 spill cut output to a file the model can read_file")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_start_and_the_end_of_very_long_output() {
        let output: String = (0..5000).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let text = cut_for_model(&output);
        assert!(text.chars().count() < 32_000);
        assert!(text.starts_with("line 0\n"));
        assert!(text.ends_with("line 4999"));
        assert!(text.contains(&format!("… ({} characters omitted) …", output.len() - MAX_MODEL_CHARS)));
    }

    #[test]
    fn short_output_is_untouched_and_multibyte_text_is_cut_on_char_boundaries() {
        assert_eq!(cut_middle("abc", 5, 2), "abc");
        assert_eq!(cut_middle("ééééééé", 4, 2), "éé\n\n… (3 characters omitted) …\n\néé");
    }
}
