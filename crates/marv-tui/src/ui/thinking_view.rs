//! The waiting line, and live tool drafts. Ports `src/ui/ThinkingView.tsx`.
//!
//! While a reply is awaited: a spinner, the label ("Thinking…", "Compacting the conversation…"),
//! the time since it appeared (one wait per reply, like the `✻ Thought for 8s` line after it) and an
//! estimated token count (4 chars a token: providers report usage only at the end), plus the last
//! [`PREVIEW_LINES`] of the reasoning, dim italic, so a long think visibly makes progress instead of
//! looking like a hang. Only the last [`TAIL_CHARS`] are cleaned and split: a draft can be a
//! 100k-character file.
//!
//! [`DraftView`]: a tool call still streaming (a write_file's content streams for minutes; a
//! trajectory had 18k tokens) shows `Writing src/app.js… (8s · ~400 tokens)` and the code's last
//! lines with their indentation (tabs → 2 spaces). Ollama sends nothing while writing a call, so it
//! gets only the timer.
//!
//! CPU: Marv re-rendered every second (counter) plus ink-spinner's own interval. Here the view
//! exposes its [`deadline`](ThinkingView::deadline) (next spinner frame, ~80 ms, only while shown)
//! and the app wakes only for that.

use std::time::{Duration, Instant};

use ratatui::text::Line;

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
    /// counted, when that isn't `thought` (a tool call's raw JSON).
    pub fn render(&self, thought: &str, count: Option<&str>, now: Instant) -> Vec<Line<'static>> {
        let _ = (thought, count, now);
        todo!("port: ThinkingView (printable on the tail, spinner frame from now - since)")
    }

    /// The next spinner frame.
    pub fn deadline(&self, now: Instant) -> Instant {
        let _ = now;
        todo!("port: next multiple of SPINNER_MS after since")
    }
}

/// A tool call the model is still writing.
pub struct DraftView;

impl DraftView {
    /// `name` and the unfinished JSON `args` so far (raw).
    pub fn render(view: &ThinkingView, name: &str, args: &str, now: Instant) -> Vec<Line<'static>> {
        let _ = (view, name, args, now);
        todo!("port: DraftView (draftLabel/draftPreview from tool-draft.ts)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_formats_like_marv() {
        assert_eq!(elapsed(Duration::from_millis(12_900)), "12s");
        assert_eq!(elapsed(Duration::from_secs(65)), "1m 05s");
    }
}
