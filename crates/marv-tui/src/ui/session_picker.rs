//! `/resume`: pick one of this project's saved sessions, newest first. Ports `src/ui/SessionPicker.tsx`.
//! A round accent box titled "Resume a session", a [`Select`](super::select::Select) of at most
//! [`MAX_SHOWN`] sessions labelled with their title and hinted `5 minutes ago · 12 messages · model`,
//! and the footer `↑/↓ to move · Enter to resume · Esc to cancel` (+ `· 15 most recent shown`).
//! Titles come from the user's first message but pass `printable()` anyway (saved files can be edited).

use crossterm::event::KeyEvent;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::select::Select;
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

/// "just now", "5 minutes ago", "3 hours ago", "2 days ago" (Marv `timeAgo` in sessions.ts).
pub fn time_ago(then_ms: u64, now_ms: u64) -> String {
    let _ = (then_ms, now_ms);
    todo!("port: timeAgo")
}

impl SessionPicker {
    pub fn new(sessions: &[SessionSummary], now_ms: u64) -> Self {
        let _ = (sessions, now_ms);
        todo!("port: SessionPicker items")
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Option<PickerOutcome> {
        let _ = key;
        todo!("port: Esc cancels, Select picks")
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let _ = (area, buf);
        todo!("port: SessionPicker box")
    }
}
