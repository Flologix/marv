//! The waiting line, and live tool drafts. Ports `src/ui/ThinkingView.tsx` (tests:
//! `tests/thinking-view.test.tsx`).
//!
//! While a reply is awaited: a spinner, the label ("Thinking…", "Compacting the conversation…"),
//! the time since it appeared (one wait per reply, like the `✻ Thought for 8s` line after it) and an
//! estimated token count (4 chars a token: providers report usage only at the end), plus the last
//! [`PREVIEW_LINES`] of the reasoning, dim italic, so a long think visibly makes progress instead of
//! looking like a hang. Only the last [`TAIL_CHARS`] are cleaned and split: a draft can be a
//! 100k-character file.
//!
//! **The reasoning preview eases (marv-rs; Marv cut each line at the width).** Reasoning is long
//! paragraphs: cut at the width, a row sat frozen on a paragraph's start until its newline, then all
//! three rows jumped. Here reasoning is word-wrapped to the width and the last [`PREVIEW_LINES`]
//! wrapped rows are shown, so with the app's paced reveal the text types along a row and the rows
//! scroll up one at a time. Once the preview has text it keeps all [`PREVIEW_LINES`] rows (blank ones
//! below while it fills), so the prompt under it doesn't jump as rows arrive. Code (drafts) keeps
//! its lines and indentation, cut at the width.
//!
//! [`DraftView`]: a tool call still streaming (a write_file's content streams for minutes; a
//! trajectory had 18k tokens) shows `Writing src/app.js… (8s · ~400 tokens)` and the code's last
//! lines with their indentation (tabs → 2 spaces). Ollama sends nothing while writing a call, so it
//! gets only the timer.
//!
//! [`draft_label`] and [`draft_preview`] port `src/tool-draft.ts`. Its home is marv-session's
//! `tool_draft` (the engine reads drafts too), but `UiEvent::ToolCallDelta` carries only the raw
//! fragments and marv-tui doesn't depend on the engine, so the TUI keeps this copy (without
//! `regex`), with the same tests.
//!
//! CPU: Marv re-rendered every second (counter) plus ink-spinner's own interval. Here the view
//! exposes its [`deadline`](ThinkingView::deadline) (next spinner frame, ~80 ms, only while shown)
//! and the app wakes only for that.

use std::time::{Duration, Instant};

use marv_core::printable;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::markdown::wrap_text;
use super::select::truncate_end;
use super::status_bar::tokens;
use super::theme;

pub const PREVIEW_LINES: usize = 3;
pub const TAIL_CHARS: usize = 4000;
/// ink-spinner's "dots" frames and interval.
pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
pub const SPINNER_MS: Duration = Duration::from_millis(80);

/// How long it's been: "12s", then "1m 05s".
pub fn elapsed(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 { format!("{s}s") } else { format!("{}m {:02}s", s / 60, s % 60) }
}

/// Marv's `estimateTokens`: 4 characters (UTF-16 units, as JS counts) a token, rounded up.
pub fn estimate_tokens(text: &str) -> u64 {
    (text.encode_utf16().count() as u64).div_ceil(4)
}

#[derive(Debug, Clone)]
pub struct ThinkingView {
    pub since: Instant,
    pub label: String,
    /// `code`: the preview is code, so its lines keep their indentation.
    pub code: bool,
}

impl ThinkingView {
    pub fn new(label: impl Into<String>, now: Instant) -> Self {
        Self { since: now, label: label.into(), code: false }
    }

    /// `thought` raw (reasoning or a draft's code; cleaned here); `count`: the text whose tokens are
    /// counted, when that isn't `thought` (a tool call's raw JSON). A blank line follows.
    pub fn render(&self, thought: &str, count: Option<&str>, now: Instant, width: u16) -> Vec<Line<'static>> {
        let dim = Style::new().fg(theme::DIM);
        let waited = now.saturating_duration_since(self.since);
        let frame = SPINNER[(waited.as_millis() / SPINNER_MS.as_millis()) as usize % SPINNER.len()];
        // An estimate: the real count arrives with the reply's usage, when this view is already gone.
        let estimate = estimate_tokens(count.unwrap_or(thought).trim());
        let mut shown = Vec::new();
        if waited >= Duration::from_secs(1) {
            shown.push(elapsed(waited));
        }
        if estimate > 0 {
            shown.push(format!("~{} tokens", tokens(estimate)));
        }
        let suffix = if shown.is_empty() { String::new() } else { format!(" ({})", shown.join(" · ")) };
        let mut lines = vec![truncate_end(
            Line::from(vec![
                Span::styled(frame, Style::new().fg(theme::ACCENT)),
                Span::styled(format!(" {}{suffix}", printable(&self.label)), dim),
            ]),
            width,
        )];
        // Only the end is shown, so only the end is cleaned up: a draft can be a 100k-character file.
        let start = thought.char_indices().rev().nth(TAIL_CHARS.saturating_sub(1)).map_or(0, |(i, _)| i);
        let tail = printable(&thought[start..]);
        let style = dim.add_modifier(Modifier::ITALIC);
        let hard = tail
            .split('\n')
            .map(|l| if self.code { l.replace('\t', "  ").trim_end().to_string() } else { l.trim().to_string() })
            .filter(|l| !l.trim().is_empty());
        let rows: Vec<Line<'static>> = if self.code {
            hard.map(|row| truncate_end(Line::from(Span::styled(format!("  {row}"), style)), width)).collect()
        } else {
            hard.flat_map(|row| wrap_text(&row, style, width.saturating_sub(2).max(1)))
                .map(|row| {
                    let mut spans = vec![Span::styled("  ", style)];
                    spans.extend(row.spans);
                    Line::from(spans)
                })
                .collect()
        };
        let shown = &rows[rows.len().saturating_sub(PREVIEW_LINES)..];
        lines.extend(shown.iter().cloned());
        if !shown.is_empty() && !self.code {
            lines.extend((shown.len()..PREVIEW_LINES).map(|_| Line::default()));
        }
        lines.push(Line::default());
        lines
    }

    /// The next spinner frame after `now`.
    pub fn deadline(&self, now: Instant) -> Instant {
        next_frame(self.since, now)
    }
}

/// The next spinner frame after `now` for a waiting line shown since `since` (a multiple of
/// [`SPINNER_MS`] after it). The seconds counter changes on a frame too, so this is its only timer.
pub fn next_frame(since: Instant, now: Instant) -> Instant {
    let ms = SPINNER_MS.as_millis();
    let waited = now.saturating_duration_since(since).as_millis();
    since + Duration::from_millis(((waited / ms + 1) * ms) as u64)
}

/// A tool call the model is still writing: "Writing src/app.js…", the time and tokens so far, and
/// the last lines of the code. A long write_file streams for minutes, and without this the screen
/// sat at "Thinking…".
pub struct DraftView;

impl DraftView {
    /// `name` and the unfinished JSON `args` so far (raw). `view` gives the clock.
    pub fn render(view: &ThinkingView, name: &str, args: &str, now: Instant, width: u16) -> Vec<Line<'static>> {
        let view = ThinkingView { since: view.since, label: draft_label(name, args), code: true };
        view.render(&draft_preview(name, args), Some(args), now, width)
    }
}

/// The value of `key` in unfinished JSON, decoded as far as it has arrived; None if it hasn't
/// started (Marv `partialString`). An escape cut in half (a lone backslash at the end, or `\u` with
/// fewer than 4 hex digits) waits for the rest.
fn partial_string(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let mut search = 0;
    let rest = loop {
        let at = json[search..].find(&needle)? + search + needle.len();
        if let Some(rest) = json[at..].trim_start().strip_prefix(':').and_then(|r| r.trim_start().strip_prefix('"')) {
            break rest;
        }
        search = at;
    };
    let mut out = String::new();
    let mut chars = rest.chars();
    let hex4 = |chars: &mut std::str::Chars| -> Option<u32> {
        let hex: String = chars.by_ref().take(4).collect();
        u32::from_str_radix(&hex, 16).ok().filter(|_| hex.len() == 4)
    };
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('b') => out.push('\u{8}'),
                Some('f') => out.push('\u{c}'),
                Some('u') => {
                    let Some(unit) = hex4(&mut chars) else { break };
                    let c = if (0xd800..0xdc00).contains(&unit) {
                        // A surrogate pair: the low half follows as another \u escape.
                        let low = chars.as_str().strip_prefix("\\u").and_then(|r| u32::from_str_radix(r.get(..4)?, 16).ok());
                        match low.filter(|l| (0xdc00..0xe000).contains(l)) {
                            Some(low) => {
                                chars.nth(5);
                                char::from_u32(0x10000 + ((unit - 0xd800) << 10) + (low - 0xdc00))
                            }
                            None => break,
                        }
                    } else {
                        char::from_u32(unit)
                    };
                    match c {
                        Some(c) => out.push(c),
                        None => break,
                    }
                }
                Some(other) => out.push(other),
                None => break,
            },
            c => out.push(c),
        }
    }
    Some(out)
}

/// "Writing src/app.js…": what the call is about to do, for the waiting line (Marv `draftLabel`).
pub fn draft_label(name: &str, args: &str) -> String {
    let path =
        partial_string(args, "path").map(|p| p.split('\n').next().unwrap_or_default().to_string()).filter(|p| !p.is_empty());
    let path = path.as_deref().unwrap_or("a file");
    match name {
        "write_file" => format!("Writing {path}…"),
        "edit_file" => format!("Editing {path}…"),
        "bash" => "Writing a command…".into(),
        "" => "Preparing a tool call…".into(),
        name => format!("Preparing {name}…"),
    }
}

/// The code written so far (a file's content, an edit's new text, a command), or "" for anything
/// else (Marv `draftPreview`).
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

    fn rows(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>()).collect()
    }

    #[test]
    fn reasoning_wraps_and_shows_its_last_rows_and_keeps_its_height() {
        let t = Instant::now();
        let view = ThinkingView::new("Thinking…", t);
        // One long paragraph: its end shows, wrapped, not its start cut at the width.
        let thought = "one two three four five six seven eight nine ten eleven twelve";
        let shown = rows(&view.render(thought, None, t, 16));
        assert_eq!(shown.len(), 1 + PREVIEW_LINES + 1, "{shown:?}");
        assert!(shown[PREVIEW_LINES].contains("twelve"), "{shown:?}");
        assert!(!shown.iter().any(|r| r.contains("one two")), "{shown:?}");
        // While it fills, blank rows hold the height.
        let short = rows(&view.render("just started", None, t, 40));
        assert_eq!(short.len(), 1 + PREVIEW_LINES + 1, "{short:?}");
        assert_eq!(short[1].trim(), "just started");
        assert!(short[2].is_empty() && short[3].is_empty());
        // No reasoning yet: just the waiting line.
        assert_eq!(view.render("", None, t, 40).len(), 2);
    }

    fn text(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.to_string()).collect()
    }

    #[test]
    fn elapsed_formats_like_marv() {
        assert_eq!(elapsed(Duration::ZERO), "0s");
        assert_eq!(elapsed(Duration::from_millis(12_400)), "12s");
        assert_eq!(elapsed(Duration::from_millis(59_999)), "59s");
        assert_eq!(elapsed(Duration::from_secs(65)), "1m 05s");
        assert_eq!(elapsed(Duration::from_secs(600)), "10m 00s");
    }

    #[test]
    fn counts_the_seconds_since_it_appeared_and_the_thoughts_tokens() {
        let t = Instant::now();
        let view = ThinkingView::new("Thinking…", t);
        let first = text(&view.render("", None, t, 80));
        assert!(first[0].contains("Thinking…"));
        assert!(!first[0].contains('(')); // nothing to show before the first second
        assert!(text(&view.render("", None, t + Duration::from_millis(1100), 80))[0].ends_with("Thinking… (1s)"));
        let long = "x".repeat(4800);
        assert!(
            text(&view.render(&long, None, t + Duration::from_millis(1100), 80))[0].ends_with("Thinking… (1s · ~1.2k tokens)")
        );
    }

    #[test]
    fn previews_the_last_lines_of_the_reasoning_cleaned() {
        let t = Instant::now();
        let view = ThinkingView::new("Thinking…", t);
        let lines = view.render("one\n\n  two\nthree\x1b]52;c;aGk=\x07\nfour  ", None, t, 80);
        assert_eq!(text(&lines)[1..], ["  two", "  three", "  four", ""]);
        assert!(lines[1].spans[0].style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn the_spinner_moves_every_80ms_and_says_when() {
        let t = Instant::now();
        let view = ThinkingView::new("Thinking…", t);
        assert!(text(&view.render("", None, t, 80))[0].starts_with(SPINNER[0]));
        assert!(text(&view.render("", None, t + Duration::from_millis(170), 80))[0].starts_with(SPINNER[2]));
        assert_eq!(view.deadline(t + Duration::from_millis(170)), t + Duration::from_millis(240));
        assert_eq!(view.deadline(t), t + SPINNER_MS);
    }

    #[test]
    fn a_draft_shows_what_it_writes_and_the_code_so_far_with_its_indentation() {
        let t = Instant::now();
        let args = r#"{"path": "page.js", "content": "const a = 1;\nfunction draw() {\n\treturn a;"#;
        let lines =
            text(&DraftView::render(&ThinkingView::new("ignored", t), "write_file", args, t + Duration::from_secs(8), 80));
        assert!(lines[0].ends_with("Writing page.js… (8s · ~19 tokens)"), "{lines:?}");
        assert_eq!(lines[1..], ["  const a = 1;", "  function draw() {", "    return a;", ""]);
    }

    // tool-draft.test.ts (as marv-session's tool_draft tests)

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
        assert_eq!(draft_preview("bash", r#"{"command":"echo 😀 é\u00"#), "echo 😀 é");
    }

    #[test]
    fn an_escape_cut_in_half_isnt_shown_half_decoded() {
        assert_eq!(draft_preview("write_file", r#"{"content":"a\"#), "a");
        assert_eq!(draft_preview("write_file", r#"{"content":"a\u00"#), "a");
        assert_eq!(draft_preview("write_file", r#"{"content":"aéb"#), "aéb");
    }

    #[test]
    fn nothing_yet_or_no_code_is_empty_and_a_complete_value_stops_at_its_quote() {
        assert_eq!(draft_preview("write_file", r#"{"path":"a.js""#), "");
        assert_eq!(draft_preview("read_file", r#"{"path":"a.js"}"#), "");
        assert_eq!(draft_preview("write_file", r#"{"content":"done","path":"a.js"}"#), "done");
    }
}
