//! The transcript: the welcome banner, then every message. Ports `src/ui/Transcript.tsx`.
//!
//! In the alternate screen every frame is redrawn, so the whole transcript stays in memory (never
//! printed once like Ink's `<Static>`, which would scroll off and be lost). Each entry's lines are
//! cached per (width, show_steps) like Marv's `memo` — only entries with steps or a diff depend on
//! ctrl+o, so toggling re-lays-out just those — and [`super::scroll_view`] culls to the viewport.
//! A subagent's entry records which transcript rows it covers ([`agent_entry_at`]) so a click can
//! open its view. Text is raw here; `message_view` cleans it.

use ratatui::text::Line;

use crate::view::{Message, WelcomeInfo};

#[derive(Debug, Clone, PartialEq)]
pub enum TranscriptItem {
    Welcome,
    Message(Box<Message>),
}

/// One entry's laid-out lines, cached until its message, the width or show_steps changes.
#[derive(Debug, Clone, Default)]
pub struct EntryCache {
    pub width: u16,
    pub show_steps: bool,
    pub lines: Vec<Line<'static>>,
}

#[derive(Debug, Clone, Default)]
pub struct Transcript {
    pub items: Vec<TranscriptItem>,
    pub cache: Vec<Option<EntryCache>>,
    pub welcome: WelcomeInfo,
}

impl Transcript {
    /// Each entry's height at `width` (filling the cache): the scroll view's input.
    pub fn heights(&mut self, width: u16, show_steps: bool) -> Vec<usize> {
        let _ = (width, show_steps);
        todo!("port: lay out changed entries through message_view::render")
    }
}

/// The id of the subagent entry covering a transcript row, from the last layout.
pub fn agent_entry_at(row: usize, entries: &[(String, std::ops::Range<usize>)]) -> Option<String> {
    entries.iter().find(|(_, rows)| rows.contains(&row)).map(|(id, _)| id.clone())
}
