//! A distinct, coloured block for Marv's own notices (startup problems, MCP servers that failed).
//! Ports `src/ui/NoticeView.tsx` (tests: `tests/noticeview.test.tsx`).
//!
//! A left bar (a border, not a one-line marker: the bar runs the whole height of the block, however
//! long it wraps), a bold title in the tone's colour, one `•` bullet per item with detail lines
//! indented under it in a fixed 2-column marker (hanging indent). Transcript only, never sent to
//! the model. Title, items and details go through `printable()`.

use ratatui::style::Color;
use ratatui::text::Line;

use super::theme;
use crate::view::{Notice, NoticeTone};

/// A notice's tone maps to a colour: info uses the accent, warning and error their own.
pub fn tone_color(tone: NoticeTone) -> Color {
    match tone {
        NoticeTone::Info => theme::ACCENT,
        NoticeTone::Warning => theme::WARNING,
        NoticeTone::Error => theme::ERROR,
    }
}

/// The worst of a set of tones (error beats warning beats info): the startup notice's tone.
pub fn worst_tone(tones: impl IntoIterator<Item = NoticeTone>) -> NoticeTone {
    tones.into_iter().max().unwrap_or(NoticeTone::Info)
}

pub fn render(notice: &Notice, width: u16) -> Vec<Line<'static>> {
    let _ = (notice, width);
    todo!("port: NoticeView (left bar, title, bullets, details; printable)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worst_tone_wins() {
        assert_eq!(worst_tone([NoticeTone::Info, NoticeTone::Error, NoticeTone::Warning]), NoticeTone::Error);
        assert_eq!(worst_tone([]), NoticeTone::Info);
        assert_eq!(tone_color(NoticeTone::Warning), theme::WARNING);
    }
}
