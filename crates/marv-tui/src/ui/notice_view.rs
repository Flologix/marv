//! A distinct, coloured block for Marv's own notices (startup problems, MCP servers that failed).
//! Ports `src/ui/NoticeView.tsx` (tests: `tests/noticeview.test.tsx`).
//!
//! A left bar (a border, not a one-line marker: the bar runs the whole height of the block, however
//! long it wraps), a bold title in the tone's colour, one `•` bullet per item with detail lines
//! indented under it in a fixed 2-column marker (hanging indent). Transcript only, never sent to
//! the model. Title, items and details go through `printable()`. A blank line follows the block
//! (Marv's `marginBottom`).

use marv_core::printable;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::markdown::{beside, wrap_text};
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
    let color = tone_color(notice.tone);
    // The bar and its padding take 2 columns; bullets and details another 2.
    let inner = width.saturating_sub(2);
    let text_width = inner.saturating_sub(2);
    let mut body = wrap_text(&printable(&notice.title), Style::new().fg(color).add_modifier(Modifier::BOLD), inner);
    for item in &notice.items {
        let text = wrap_text(&printable(&item.text), Style::new(), text_width);
        body.extend(beside(text, Span::styled("• ", Style::new().fg(color)), Span::raw("  ")));
        for detail in item.detail.iter().flatten() {
            let detail = wrap_text(&printable(detail), Style::new().fg(theme::DIM), text_width);
            body.extend(beside(detail, Span::raw("  "), Span::raw("  ")));
        }
    }
    let bar = Span::styled("│ ", Style::new().fg(color));
    let mut lines = beside(body, bar.clone(), bar);
    lines.push(Line::default());
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::NoticeItem;
    use unicode_width::UnicodeWidthStr;

    fn notice() -> Notice {
        Notice {
            title: "Startup · 2 notes".into(),
            tone: NoticeTone::Warning,
            items: vec![
                NoticeItem {
                    text: "AGENTS.md is long: the model sees the first 20,000 of 89,192 characters".into(),
                    detail: Some(vec!["Up front: lines 1-61".into(), "Put what matters most first.".into()]),
                },
                NoticeItem { text: "1 PATH folder is hidden from the sandbox: ~/.cache/.bun/bin".into(), detail: None },
            ],
        }
    }

    /// The column `needle` starts at.
    fn col(line: &str, needle: &str) -> usize {
        line[..line.find(needle).unwrap()].chars().count()
    }

    fn plain(width: u16, notice: &Notice) -> Vec<String> {
        render(notice, width).iter().map(|l| l.to_string()).filter(|l| !l.trim().is_empty()).collect()
    }

    #[test]
    fn worst_tone_wins() {
        assert_eq!(worst_tone([NoticeTone::Info, NoticeTone::Error, NoticeTone::Warning]), NoticeTone::Error);
        assert_eq!(worst_tone([]), NoticeTone::Info);
        assert_eq!(tone_color(NoticeTone::Warning), theme::WARNING);
    }

    #[test]
    fn shows_the_title_one_bullet_per_item_and_indented_detail_lines() {
        let lines = plain(80, &notice());
        assert!(lines[0].contains("│ Startup · 2 notes"));
        for text in ["• AGENTS.md is long", "Up front: lines 1-61", "Put what matters most first.", "• 1 PATH folder is hidden"]
        {
            assert!(lines.iter().any(|l| l.contains(text)), "{text} in {lines:?}");
        }
    }

    #[test]
    fn wrapped_bullets_keep_a_hanging_indent() {
        let lines = plain(30, &notice());
        let headline = lines.iter().find(|l| l.contains("AGENTS.md is long")).unwrap();
        let bullet_indent = col(headline, "AGENTS.md");
        assert!(headline.starts_with("│ • "));
        // The bar runs the whole height of the block: every line starts with it.
        for line in &lines {
            assert!(line.starts_with('│'), "{line:?}");
            assert!(line.width() <= 30);
            if !line.contains('•') && !line.contains("Startup") {
                // A continuation or a detail: indented under the bullet's text.
                let indented = 1 + line.chars().skip(1).take_while(|c| *c == ' ').count();
                assert!(indented >= bullet_indent, "{line:?}");
            }
        }
    }

    #[test]
    fn detail_lines_sit_under_their_item() {
        let lines = plain(40, &notice());
        let detail = col(lines.iter().find(|l| l.contains("Up front")).unwrap(), "Up front");
        let headline = col(lines.iter().find(|l| l.contains("AGENTS.md")).unwrap(), "AGENTS.md");
        assert!(detail >= headline);
    }

    #[test]
    fn the_tones_colour_shows_on_the_bar_title_and_bullets_dim_for_details() {
        for (tone, color) in
            [(NoticeTone::Info, theme::ACCENT), (NoticeTone::Warning, theme::WARNING), (NoticeTone::Error, theme::ERROR)]
        {
            let lines = render(&Notice { tone, ..notice() }, 60);
            let rows = lines.iter().filter(|l| !l.to_string().trim().is_empty()).count();
            let tinted = lines.iter().flat_map(|l| &l.spans).filter(|s| s.style.fg == Some(color)).count();
            // The bar on every row, the title, and each bullet.
            assert_eq!(tinted, rows + 1 + 2);
            let detail = lines.iter().flat_map(|l| &l.spans).find(|s| s.content.contains("Up front")).unwrap();
            assert_eq!(detail.style.fg, Some(theme::DIM));
        }
    }

    #[test]
    fn every_line_stays_within_the_terminal_at_any_width() {
        for width in [25, 40, 60, 80, 121] {
            for line in render(&notice(), width) {
                assert!(line.width() <= width as usize);
            }
        }
    }

    #[test]
    fn notice_text_is_cleaned() {
        let evil = Notice {
            title: "t\x1b]52;c;aGk=\x07".into(),
            tone: NoticeTone::Info,
            items: vec![NoticeItem { text: "i\x1b[2J".into(), detail: Some(vec!["d\u{202e}x".into()]) }],
        };
        let text: String = render(&evil, 40).iter().map(|l| l.to_string()).collect();
        assert!(!text.contains('\x1b') && !text.contains('\x07') && !text.contains('\u{202e}'));
    }
}
