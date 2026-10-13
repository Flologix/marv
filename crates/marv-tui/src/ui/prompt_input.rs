//! The prompt: a round box with `> ` and the text, plus the `/` command menu. Ports
//! `src/ui/PromptInput.tsx` (tests: `tests/prompt.test.tsx`, `tests/commands.test.ts`).
//!
//! - ↑/↓ browse history (oldest first; past the newest returns to an empty line); replacing the
//!   text from outside puts the cursor at the end (Marv remounted ink-text-input for that).
//! - Typing `/` opens a menu of [`match_commands`]: ↑/↓ select, Enter runs the highlighted one
//!   ("/se" → "/setup"), Tab or → completes it plus a space so arguments can follow, Esc hides the
//!   menu until the text changes. Hidden while busy.
//! - Placeholder "Type a message, or / for commands"; the border dims while busy.
//! - The user's own text is drawn as typed, but pasted text can carry escapes: `printable()` it too.

use crossterm::event::KeyEvent;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::view::CommandInfo;

/// Commands matching what's typed so far ("/se" → setup), prefix matches first. Once there's a space
/// the user is typing arguments, and only argument completions match: entries whose name has words
/// ("mcp login github"), once everything before their last word is typed ("/mcp login "), so
/// "/mcp " + Enter still runs /mcp.
pub fn match_commands<'a>(value: &str, commands: &'a [CommandInfo]) -> Vec<&'a CommandInfo> {
    let _ = (value, commands);
    todo!("port: matchCommands")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptOutcome {
    Submit(String),
}

#[derive(Debug, Clone, Default)]
pub struct PromptInput {
    pub value: String,
    /// Cursor position, in chars.
    pub cursor: usize,
    /// Previous submissions, oldest first.
    pub history: Vec<String>,
    /// None = not browsing history (editing a fresh line).
    pub history_index: Option<usize>,
    pub menu_index: usize,
    /// Esc hides the menu until the text changes again.
    pub dismissed_at: Option<String>,
}

impl PromptInput {
    pub fn on_key(&mut self, key: KeyEvent, busy: bool, commands: &[CommandInfo]) -> Option<PromptOutcome> {
        let _ = (key, busy, commands);
        todo!("port: PromptInput input (text editing, history, menu)")
    }

    pub fn height(&self, width: u16, busy: bool, commands: &[CommandInfo]) -> u16 {
        let _ = (width, busy, commands);
        todo!("port: measure prompt + menu")
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer, busy: bool, commands: &[CommandInfo]) {
        let _ = (area, buf, busy, commands);
        todo!("port: PromptInput box and menu")
    }
}
