//! The splash: the MARV logo in a top-to-bottom gradient, the version, the folder and the tagline.
//! Ports `src/ui/Splash.tsx` (fully).
//!
//! Shown for `duration` before continuing on its own; any key skips it (the app handles the key and
//! asks [`Splash::deadline`] when to wake, so the splash costs no timer of its own). A terminal
//! narrower than the logo + 4 gets a bold "Marv" instead. Everything is centred. The cwd goes
//! through `printable()` (a folder name can contain escapes).

use std::time::{Duration, Instant};

use marv_core::printable;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

use super::theme;

pub const LOGO: [&str; 6] = [
    "███╗   ███╗ █████╗ ██████╗ ██╗   ██╗",
    "████╗ ████║██╔══██╗██╔══██╗██║   ██║",
    "██╔████╔██║███████║██████╔╝██║   ██║",
    "██║╚██╔╝██║██╔══██║██╔══██╗╚██╗ ██╔╝",
    "██║ ╚═╝ ██║██║  ██║██║  ██║ ╚████╔╝ ",
    "╚═╝     ╚═╝╚═╝  ╚═╝╚═╝  ╚═╝  ╚═══╝  ",
];

/// The logo's width in columns (Marv measured `LOGO[0].length`, which is the same here: every
/// character is one column).
pub fn logo_width() -> u16 {
    LOGO[0].width() as u16
}

#[derive(Debug, Clone)]
pub struct Splash {
    pub version: String,
    pub cwd: String,
    until: Instant,
}

impl Splash {
    /// Shown from `now` for `duration`.
    pub fn new(version: impl Into<String>, cwd: impl Into<String>, now: Instant, duration: Duration) -> Self {
        Self { version: version.into(), cwd: cwd.into(), until: now + duration }
    }

    /// When it continues on its own.
    pub fn deadline(&self) -> Instant {
        self.until
    }

    pub fn done(&self, now: Instant) -> bool {
        now >= self.until
    }

    /// The splash's lines, before centring.
    pub fn lines(&self, columns: u16) -> Vec<Line<'static>> {
        let dim = Style::new().fg(theme::DIM);
        let mut lines = Vec::new();
        if columns >= logo_width() + 4 {
            for (i, row) in LOGO.iter().enumerate() {
                lines.push(Line::styled(*row, Style::new().fg(theme::GRADIENT[i % theme::GRADIENT.len()])));
            }
        } else {
            lines.push(Line::styled("Marv", Style::new().fg(theme::ACCENT).add_modifier(Modifier::BOLD)));
        }
        lines.push(Line::default());
        lines.push(Line::styled(format!("a terminal coding agent · v{}", self.version), dim));
        lines.push(Line::styled(printable(&self.cwd), dim));
        lines.push(Line::default());
        lines.push(Line::styled("\"Take me to your codebase.\"", Style::new().fg(theme::ACCENT).add_modifier(Modifier::ITALIC)));
        lines.push(Line::default());
        lines.push(Line::styled("press any key to continue", dim.add_modifier(Modifier::ITALIC)));
        lines
    }
}

impl Widget for &Splash {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let lines = self.lines(area.width);
        // paddingY 1 around the block, and the block centred in the screen.
        let height = lines.len() as u16 + 2;
        let top = area.y + area.height.saturating_sub(height) / 2 + 1;
        for (i, line) in lines.iter().enumerate() {
            let y = top + i as u16;
            if y >= area.bottom() {
                break;
            }
            let width = line.width() as u16;
            let x = area.x + area.width.saturating_sub(width) / 2;
            buf.set_line(x, y, line, area.right().saturating_sub(x));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(buf: &Buffer) -> String {
        let area = buf.area;
        (0..area.height)
            .map(|y| (0..area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn logo_rows_are_one_width() {
        for row in LOGO {
            assert_eq!(row.width() as u16, logo_width());
        }
    }

    #[test]
    fn wide_terminal_shows_the_logo_narrow_shows_the_name() {
        let splash = Splash::new("9.9.9", "~/x\x1b]0;evil\x07", Instant::now(), Duration::from_millis(1200));
        let mut buf = Buffer::empty(Rect::new(0, 0, 60, 20));
        (&splash).render(buf.area, &mut buf);
        let shown = text(&buf);
        assert!(shown.contains("███╗   ███╗"));
        assert!(shown.contains("a terminal coding agent · v9.9.9"));
        assert!(shown.contains("~/x"));
        assert!(!shown.contains("evil"));

        let mut narrow = Buffer::empty(Rect::new(0, 0, 30, 20));
        (&splash).render(narrow.area, &mut narrow);
        let shown = text(&narrow);
        assert!(shown.contains("Marv"));
        assert!(!shown.contains("███╗"));
    }

    #[test]
    fn continues_after_its_duration() {
        let t0 = Instant::now();
        let splash = Splash::new("1", "/", t0, Duration::from_millis(1200));
        assert!(!splash.done(t0));
        assert!(splash.done(t0 + Duration::from_millis(1200)));
    }
}
