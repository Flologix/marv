//! Asks before a tool changes something. Ports `src/ui/Approval.tsx` (tests: `tests/approval.test.tsx`).
//!
//! Shown in place of the prompt while the agent waits: a round border (warning colour when the
//! preview has a warning), `[agent]` when a subagent asks, the bold title, then the payload in
//! full (docs/SECURITY.md: approvals show the full payload): `$ command`, text, and the diff (at
//! most [`MAX_DIFF_LINES`]), a note, `⚠ warning`, "N more waiting" (parallel subagents queue
//! behind the head), and the choice: Yes / Yes, and don't ask again for <scope> this session /
//! No, and tell Marv what to do instead (esc). `once` requests offer only Yes / No.
//!
//! Esc declines this and everything waiting and stops the run, except while a subagent's view is
//! open (`escape_cancels: false`): Esc then only closes the view. Every string from the request
//! goes through `printable()`: a Trojan-Source command must not look like something else.

use crossterm::event::KeyEvent;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::select::Select;
use crate::view::{ApprovalRequest, Decision};

pub const MAX_DIFF_LINES: usize = 24;

/// What a key did at the approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalOutcome {
    Decide(Decision),
    /// Esc: no to this and to everything waiting behind it.
    CancelAll,
}

pub struct ApprovalView {
    pub request: ApprovalRequest,
    /// Other requests queued behind this one.
    pub waiting: usize,
    pub escape_cancels: bool,
    pub select: Select<Decision>,
}

impl ApprovalView {
    pub fn new(request: ApprovalRequest, waiting: usize, escape_cancels: bool) -> Self {
        let _ = (request, waiting, escape_cancels);
        todo!("port: Approval's items (Yes / Always with scope.description / No; once → Yes / No)")
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Option<ApprovalOutcome> {
        let _ = key;
        todo!("port: Approval's useInput + Select")
    }

    /// Rows it needs at `width` (the bottom panel sizes itself to this).
    pub fn height(&self, width: u16) -> u16 {
        let _ = width;
        todo!("port: measure")
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let _ = (area, buf);
        todo!("port: Approval box (printable on every request string, DiffView)")
    }
}
