//! The banner at the top of the session: the martian, with the welcome text centred beside it.
//! Ports `src/ui/Welcome.tsx` (fully; test from `tests/martian.test.tsx`).
//!
//! A round accent border, 2 columns of padding, the martian, a 3-column gap, then (vertically
//! centred) "Welcome to Marv v1.2.3", "/help for commands", "cwd: …" and, when anything was loaded,
//! one line like "AGENTS.md loaded · 3 skills (/skills) · 1 memory (/memory) · 2 MCP servers (/mcp)".
//! The cwd goes through `printable()`. The martian's pose comes from the app's
//! [`MartianAnimation`](super::martian::MartianAnimation); the app only wakes for it while this
//! banner is inside the viewport.

use marv_core::printable;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Widget};

use super::martian::{self, FrameName, MartianWidget};
use super::theme;
use crate::view::WelcomeInfo;

/// Rows the banner takes (border + martian + border), not counting the blank line after it.
pub const HEIGHT: u16 = martian::HEIGHT + 2;

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// "AGENTS.md loaded · 3 skills (/skills) · …", or None when nothing was loaded.
pub fn loaded_line(info: &WelcomeInfo) -> Option<String> {
    let mut parts = Vec::new();
    if info.instructions {
        parts.push("AGENTS.md loaded".to_string());
    }
    if info.skills > 0 {
        parts.push(format!("{} (/skills)", plural(info.skills, "skill", "skills")));
    }
    if info.memories > 0 {
        parts.push(format!("{} (/memory)", plural(info.memories, "memory", "memories")));
    }
    if info.mcp_servers > 0 {
        parts.push(format!("{} (/mcp)", plural(info.mcp_servers, "MCP server", "MCP servers")));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// The banner, with the martian in `frame`.
pub struct Welcome<'a> {
    pub info: &'a WelcomeInfo,
    pub frame: FrameName,
}

impl Welcome<'_> {
    fn text(&self) -> Vec<Line<'static>> {
        let dim = Style::new().fg(theme::DIM);
        let mut lines = vec![
            Line::from(vec![
                Span::styled("Welcome to Marv", Style::new().add_modifier(Modifier::BOLD)),
                Span::styled(format!(" v{}", self.info.version), dim),
            ]),
            Line::styled("/help for commands", dim),
            Line::styled(format!("cwd: {}", printable(&self.info.cwd)), dim),
        ];
        if let Some(loaded) = loaded_line(self.info) {
            lines.push(Line::styled(loaded, dim));
        }
        lines
    }
}

impl Widget for Welcome<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = Rect { height: area.height.min(HEIGHT), ..area };
        let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(theme::ACCENT));
        let inner = block.inner(area);
        block.render(area, buf);
        // paddingX 2.
        let inner = Rect { x: inner.x + 2, width: inner.width.saturating_sub(4), ..inner };
        MartianWidget::new(self.frame).render(Rect { width: inner.width.min(martian::WIDTH), ..inner }, buf);
        // gap 3, then the text centred vertically beside the martian.
        let text_x = inner.x + martian::WIDTH + 3;
        if text_x >= inner.right() {
            return;
        }
        let lines = self.text();
        let top = inner.y + inner.height.saturating_sub(lines.len() as u16) / 2;
        for (i, line) in lines.iter().enumerate() {
            let y = top + i as u16;
            if y >= inner.bottom() {
                break;
            }
            buf.set_line(text_x, y, line, inner.right() - text_x);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(buf: &Buffer) -> Vec<String> {
        let area = buf.area;
        (0..area.height).map(|y| (0..area.width).map(|x| buf[(x, y)].symbol().to_string()).collect()).collect()
    }

    #[test]
    fn shows_the_martian_beside_the_text() {
        let info = WelcomeInfo { version: "9.9.9".into(), cwd: "~/x".into(), ..Default::default() };
        let mut buf = Buffer::empty(Rect::new(0, 0, 60, HEIGHT));
        Welcome { info: &info, frame: FrameName::Idle }.render(buf.area, &mut buf);
        let rows = rows(&buf);
        assert!(rows.iter().any(|r| r.contains("▄██ ███ ██▄")));
        let text_row = rows.iter().find(|r| r.contains("Welcome to Marv")).unwrap();
        let martian_at = text_row.find(['▄', '█', '▀']).unwrap();
        assert!(martian_at < text_row.find("Welcome to Marv v9.9.9").unwrap());
        assert!(rows.iter().any(|r| r.contains("cwd: ~/x")));
        assert!(rows[0].starts_with('╭'));
    }

    #[test]
    fn loaded_line_pluralises() {
        let mut info = WelcomeInfo::default();
        assert_eq!(loaded_line(&info), None);
        info.instructions = true;
        info.skills = 1;
        info.memories = 2;
        info.mcp_servers = 1;
        assert_eq!(
            loaded_line(&info).unwrap(),
            "AGENTS.md loaded · 1 skill (/skills) · 2 memories (/memory) · 1 MCP server (/mcp)"
        );
    }
}
