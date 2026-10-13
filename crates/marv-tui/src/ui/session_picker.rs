//! `/resume`: pick one of this project's saved sessions, newest first. Ports `src/ui/SessionPicker.tsx`.
//! A round accent box titled "Resume a session", a [`Select`](super::select::Select) of at most
//! [`MAX_SHOWN`] sessions labelled with their title and hinted `5 minutes ago · 12 messages · model`,
//! and the footer `↑/↓ to move · Enter to resume · Esc to cancel` (+ `· 15 most recent shown`).
//! Titles come from the user's first message but pass `printable()` anyway (saved files can be edited).

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Padding, Paragraph, Widget};

use super::select::{Select, SelectItem, truncate_end};
use super::theme;
use crate::view::SessionSummary;

pub const MAX_SHOWN: usize = 15;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerOutcome {
    Pick(String),
    Cancel,
}

pub struct SessionPicker {
    pub total: usize,
    pub select: Select<String>,
}

/// "just now", "5 minutes ago", "3 hours ago", "yesterday", "2 days ago" (Marv `timeAgo` in sessions.ts).
pub fn time_ago(then_ms: u64, now_ms: u64) -> String {
    let minutes = now_ms.saturating_sub(then_ms) / 60_000;
    let plural = |n: u64, unit: &str| format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" });
    if minutes < 1 {
        return "just now".into();
    }
    if minutes < 60 {
        return plural(minutes, "minute");
    }
    // Math.round: halves up.
    let hours = (minutes + 30) / 60;
    if hours < 24 {
        return plural(hours, "hour");
    }
    let days = (hours + 12) / 24;
    if days == 1 { "yesterday".into() } else { format!("{days} days ago") }
}

impl SessionPicker {
    pub fn new(sessions: &[SessionSummary], now_ms: u64) -> Self {
        let items = sessions
            .iter()
            .take(MAX_SHOWN)
            .map(|s| {
                let messages = format!("{} message{}", s.messages, if s.messages == 1 { "" } else { "s" });
                SelectItem::new(s.id.clone(), s.title.clone()).hint(format!(
                    "{} · {messages} · {}",
                    time_ago(s.updated_at, now_ms),
                    s.model
                ))
            })
            .collect();
        Self { total: sessions.len(), select: Select::new(items, None) }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Option<PickerOutcome> {
        if key.code == KeyCode::Esc {
            return Some(PickerOutcome::Cancel);
        }
        self.select.on_key(key).map(PickerOutcome::Pick)
    }

    fn lines(&self) -> Vec<Line<'static>> {
        let mut lines = vec![Line::styled("Resume a session", Style::new().add_modifier(Modifier::BOLD)), Line::default()];
        lines.extend(self.select.render());
        lines.push(Line::default());
        let more = if self.total > MAX_SHOWN { format!(" · {MAX_SHOWN} most recent shown") } else { String::new() };
        lines.push(Line::styled(format!("↑/↓ to move · Enter to resume · Esc to cancel{more}"), Style::new().fg(theme::DIM)));
        lines
    }

    /// Rows it needs: one per line plus the border.
    pub fn height(&self) -> u16 {
        self.lines().len() as u16 + 2
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let inner_width = area.width.saturating_sub(4);
        let lines: Vec<Line> = self.lines().into_iter().map(|l| truncate_end(l, inner_width)).collect();
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(theme::ACCENT))
            .padding(Padding::horizontal(1));
        Paragraph::new(lines).block(block).render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    const MIN: u64 = 60_000;

    fn summary(i: usize) -> SessionSummary {
        SessionSummary {
            id: format!("s{i}"),
            title: format!("Session {i}\x1b[2J"),
            updated_at: 0,
            messages: i,
            model: "qwen".into(),
        }
    }

    fn screen(picker: &SessionPicker) -> String {
        let area = Rect::new(0, 0, 80, picker.height());
        let mut buf = Buffer::empty(area);
        picker.render(area, &mut buf);
        (0..area.height)
            .map(|y| (0..area.width).map(|x| buf[(x, y)].symbol()).collect::<String>().trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn time_ago_like_marv() {
        assert_eq!(time_ago(0, 59_999), "just now");
        assert_eq!(time_ago(0, MIN), "1 minute ago");
        assert_eq!(time_ago(0, 5 * MIN), "5 minutes ago");
        assert_eq!(time_ago(0, 89 * MIN), "1 hour ago");
        assert_eq!(time_ago(0, 90 * MIN), "2 hours ago");
        assert_eq!(time_ago(0, 30 * 60 * MIN), "yesterday");
        assert_eq!(time_ago(0, 60 * 60 * MIN), "3 days ago");
        assert_eq!(time_ago(10, 0), "just now"); // a clock that went back
    }

    #[test]
    fn lists_sessions_with_their_age_messages_and_model_and_picks_one() {
        let mut picker = SessionPicker::new(&[summary(1), summary(2)], 5 * MIN);
        let frame = screen(&picker);
        assert!(frame.contains("Resume a session"));
        assert!(frame.contains("❯ Session 1 · 5 minutes ago · 1 message · qwen"), "{frame}");
        assert!(frame.contains("  Session 2 · 5 minutes ago · 2 messages · qwen"));
        assert!(!frame.contains("most recent shown"));
        picker.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(picker.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), Some(PickerOutcome::Pick("s2".into())));
        assert_eq!(picker.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)), Some(PickerOutcome::Cancel));
    }

    #[test]
    fn shows_at_most_15_and_says_so() {
        let sessions: Vec<_> = (0..20).map(summary).collect();
        let picker = SessionPicker::new(&sessions, 0);
        assert_eq!(picker.select.items.len(), MAX_SHOWN);
        assert!(screen(&picker).contains("Esc to cancel · 15 most recent shown"));
    }
}
