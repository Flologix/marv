//! A subagent's own transcript. Ports `src/ui/AgentView.tsx` and the log it shows, `src/agent-log.ts`
//! (tests: `tests/agent-log.test.ts`).
//!
//! A click on a subagent's entry opens its view: its task, what it said and thought, its tool calls,
//! live while it runs (the same `MessageView`s, a streaming reply, the waiting line, a live draft).
//! [`AgentViewHeader`] is pinned above it: `● agent <title> · running|finished` and "esc to go back".
//! The log is built from the same events as the main transcript (`UiEvent::Subagent`) but kept
//! apart: only the subagent's final report reaches the parent's conversation, and none of this
//! reaches any model. Reasoning collapses to `✻ Thought for Ns` as in the main transcript; what
//! streamed before an error or stop isn't lost. The title goes through `printable()`.

use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::view::{Message, UiEvent};

/// A subagent's log (Marv `AgentLog`).
#[derive(Debug, Clone, Default)]
pub struct AgentLog {
    /// "general-purpose · Read notes"
    pub title: String,
    /// Finished entries.
    pub messages: Vec<Message>,
    /// The reply streaming in, and any reasoning before it.
    pub streaming: String,
    pub thinking: String,
    /// A tool call still streaming in (the latest), shown as it's written: (index, name, args).
    pub draft: Option<(usize, String, String)>,
    pub running: bool,
    /// Bumped on every change so the cached layout knows to redo it (Marv's `version`).
    pub version: u64,
}

impl AgentLog {
    pub fn new(title: String, prompt: String) -> Self {
        let _ = (title, prompt);
        todo!("port: createAgentLog (the task as its first user message)")
    }

    /// Folds one event into the log (Marv `applyEvent`).
    pub fn apply(&mut self, event: &UiEvent, now: Instant) {
        let _ = (event, now);
        todo!("port: applyEvent (STEP_ENDS drop the draft, STOPPED messages)")
    }
}

/// Pinned above a subagent's view: whose it is, and the way back.
pub struct AgentViewHeader<'a> {
    pub log: &'a AgentLog,
}

impl AgentViewHeader<'_> {
    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let _ = (area, buf);
        todo!("port: AgentViewHeader (printable title, truncate-end)")
    }
}
