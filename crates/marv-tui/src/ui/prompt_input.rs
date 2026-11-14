//! The prompt: a round box with `> ` and the text, plus the `/` command menu. Ports
//! `src/ui/PromptInput.tsx` (tests: `tests/prompt.test.tsx`, `tests/commands.test.ts`).
//!
//! - ↑/↓ browse history (oldest first; past the newest returns to an empty line); replacing the
//!   text from outside puts the cursor at the end (Marv remounted ink-text-input for that).
//! - Typing `/` opens a menu of [`match_commands`]: ↑/↓ select, Enter runs the highlighted one
//!   ("/se" → "/setup"), Tab or → completes it plus a space so arguments can follow, Esc hides the
//!   menu until the text changes. Hidden while busy. ctrl+k opens the App's command palette over it
//!   (`command_palette`, #44), built from the same commands; the text here stays as it was.
//! - Placeholder "Type a message, or / for commands"; the border dims while busy.
//! - New in marv-rs: Alt+Enter (or ctrl+s) is [`PromptOutcome::SendNow`] ("send now": while a turn runs, the App sends the line
//!   into it at its next step instead of waiting for it to end). Enter while a turn runs queues the line for after it
//!   (#184, the App's): [`queue_rows`] lists the queue above the box.
//! - New in marv-rs: after a turn, a suggested next message ([`PromptInput::suggest`]) shows in the empty prompt
//!   instead of the placeholder. Tab or → takes it into the line; typing anything drops it. Enter on the empty line
//!   still sends nothing: a suggestion is only ever sent after the user took it and pressed Enter.
//! - The user's own text is drawn as typed, but pasted text can carry escapes: `printable()` it too.
//! - Pastes (marv-rs #214; bracketed paste, `render_options`). A paste never submits, and its newlines are kept.
//!   One of several lines, or longer than [`PASTE_CHIP_CHARS`], shows as a chip, `[Pasted text #1, 4 lines]`
//!   (Claude Code's `[Pasted text #1 +4 lines]`, kept simple): one placeholder char in `value` (from
//!   [`CHIP_BASE`], a private-use plane nobody types; typed or pasted chars from that plane become U+FFFD) that
//!   stands for its text in `pastes`. So ←/→ step over a chip, and Backspace/Delete remove it, as one char. The
//!   submitted text has every chip replaced by what was pasted ([`PromptInput::text`]); ↑ brings a submission
//!   back with its chips. A short one-line paste is typed in as before.
//! - Editing, as in a text field and readline: ←/→ move the cursor, by words with ctrl or alt (alt+b/alt+f too, what
//!   a Mac's option+←/→ sends); Home/End and ctrl+a/ctrl+e go to the start and end of the line the cursor is on;
//!   Backspace/Delete delete around it, a word with ctrl or alt (ctrl+w and alt+d too); ctrl+u deletes to the line's
//!   start. ↑/↓ move between the text's rows (wrapped, or the lines "send now" gave back) and browse the history only
//!   from its first and last row. Any other ctrl/alt + a letter types nothing (ctrl+o).
//! - **Selecting:** shift with any of those moves selects; so does a drag over the text ([`PromptInput::press`],
//!   `drag`, `release`, routed by the App), and its release copies what it covers, as the transcript's does. A click
//!   moves the cursor. Typing or a paste replaces the selection; Backspace and Delete take it; ctrl+x cuts it
//!   ([`PromptOutcome::Copy`]) and the App's ctrl+c copies it ([`PromptInput::take_selection`]). A chip is one char,
//!   so a selection holds it whole, and what's copied is its text.
//! - **ctrl+z** undoes, **ctrl+y** or ctrl+shift+z redoes: one edit at a time (a typed word, a paste, a cut), the last
//!   [`UNDO_STEPS`] of them. A new line ([`PromptInput::set`]: submitted, recalled, cleared) starts over.
//!
//! What changes in Rust: the component owns its value (Marv's was controlled by the App); the App
//! clears it after accepting a submission, like Marv's `handleSubmit` did. Marv's ink-text-input had no selection
//! and no undo, and its ctrl+←/→ moved one char: the editing above is a deliberate difference; `prompt.test.tsx`'s
//! cases all still hold except that one.

use std::cell::RefCell;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use marv_core::printable;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Padding, Widget};
use unicode_width::UnicodeWidthStr;

use super::select::truncate_end;
use super::theme;
use crate::view::CommandInfo;

pub const PLACEHOLDER: &str = "Type a message, or / for commands";

/// A one-line paste longer than this shows as a chip too. Long enough that a pasted login callback address (the
/// App's `login_callback`) or a path stays plain text.
pub const PASTE_CHIP_CHARS: usize = 1000;
/// The first chip placeholder: Supplementary Private Use Area-B (plane 16). Nerd Fonts' icons live in the BMP's
/// private area and plane 15, never here.
pub const CHIP_BASE: u32 = 0x10_0000;
const CHIP_LAST: u32 = 0x10_FFFD;

fn is_chip(c: char) -> bool {
    (CHIP_BASE..=CHIP_LAST).contains(&(c as u32))
}

/// Text from the user with any char that would read as a chip replaced.
fn no_chips(text: &str) -> String {
    text.chars().map(|c| if is_chip(c) { char::REPLACEMENT_CHARACTER } else { c }).collect()
}

/// What a chip shows: `[Pasted text #1, 4 lines]`, or the length for one long line.
fn chip_label(number: usize, text: &str) -> String {
    match text.lines().count() {
        0 | 1 => format!("[Pasted text #{number}, {} chars]", text.chars().count()),
        lines => format!("[Pasted text #{number}, {lines} lines]"),
    }
}

/// How many edits ctrl+z can take back.
pub const UNDO_STEPS: usize = 50;

/// What ctrl+z goes back to: the line and its cursor before an edit. Chips stay valid: `pastes` only grows while
/// the line lives, and a new line clears the undo with it.
#[derive(Debug, Clone)]
struct Snapshot {
    value: String,
    cursor: usize,
}

/// Where the last draw put the text, for the mouse and for ↑/↓ between rows. Empty when the prompt wasn't drawn
/// (a panel over it).
#[derive(Debug, Clone, Default)]
struct Shown {
    /// The box, its border included: a press inside it is the prompt's.
    area: Rect,
    /// The screen cell of the text's first row, first column (past the border, the padding and the gutter).
    x: u16,
    y: u16,
    rows: Vec<RowMap>,
}

/// One drawn row of the text: the char each cell shows (a chip's label is all its one char), and where a click
/// past its end puts the cursor.
#[derive(Debug, Clone, Default)]
struct RowMap {
    cells: Vec<usize>,
    end: usize,
}

impl RowMap {
    fn holds(&self, cursor: usize) -> bool {
        self.end == cursor || self.cells.contains(&cursor)
    }
}

/// What a char is, for word moves: a word is a run of one kind. A chip is a word of its own kind.
fn kind(c: char) -> u8 {
    if c.is_whitespace() {
        0
    } else if is_chip(c) {
        3
    } else if c.is_alphanumeric() || c == '_' {
        1
    } else {
        2
    }
}

/// A submission with chips, so ↑ can bring it back as it was typed.
#[derive(Debug, Clone)]
struct Recall {
    text: String,
    value: String,
    pastes: Vec<String>,
}

/// Commands matching what's typed so far ("/se" → setup), prefix matches first. Once there's a space
/// the user is typing arguments, and only argument completions match: entries whose name has words
/// ("mcp login github"), once everything before their last word is typed ("/mcp login "), so
/// "/mcp " + Enter still runs /mcp.
pub fn match_commands<'a>(value: &str, commands: &'a [CommandInfo]) -> Vec<&'a CommandInfo> {
    let Some(typed) = value.strip_prefix('/') else { return Vec::new() };
    let typed = typed.to_lowercase();
    if typed.chars().any(char::is_whitespace) {
        return commands
            .iter()
            .filter(|c| {
                let name = c.name.to_lowercase();
                let parent = &name[..name.rfind(' ').map_or(0, |i| i + 1)];
                !parent.is_empty() && typed.starts_with(parent) && name.starts_with(&typed)
            })
            .collect();
    }
    let single: Vec<&CommandInfo> = commands.iter().filter(|c| !c.name.contains(' ')).collect();
    let prefix = single.iter().filter(|c| c.name.starts_with(&typed));
    let other = single.iter().filter(|c| !c.name.starts_with(&typed) && c.name.contains(&typed));
    prefix.chain(other).copied().collect()
}

/// At most this many queued messages are listed above the prompt (#184); the rest are counted.
pub const QUEUE_SHOWN: usize = 3;

/// The queued messages above the prompt (#184): oldest first, one row each (its first line, cut to the width), then
/// "+N more" past [`QUEUE_SHOWN`]. Empty when nothing is queued.
pub fn queue_rows(queued: &[String], width: u16) -> Vec<Line<'static>> {
    let dim = Style::new().fg(theme::DIM);
    let mut rows: Vec<Line<'static>> = queued
        .iter()
        .take(QUEUE_SHOWN)
        .map(|text| {
            let first = printable(text.trim().lines().next().unwrap_or(""));
            let more = if text.trim().lines().nth(1).is_some() { " …" } else { "" };
            truncate_end(Line::from(vec![Span::styled("  queued › ", dim), Span::raw(format!("{first}{more}"))]), width)
        })
        .collect();
    if queued.len() > QUEUE_SHOWN {
        rows.push(Line::from(Span::styled(format!("  +{} more queued", queued.len() - QUEUE_SHOWN), dim)));
    }
    rows
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptOutcome {
    Submit(String),
    /// Alt+Enter: "send now" (the App sends it into the running turn, at its next step; idle, it's a plain submit).
    SendNow(String),
    /// ctrl+x cut this (the chips expanded): it goes to the terminal's clipboard.
    Copy(String),
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
    /// The suggested next message, shown while the line is empty (already `printable()`: Tab takes what was shown).
    pub suggestion: Option<String>,
    /// The user typed, pasted or pressed Esc since the last turn ended ([`Self::expect_suggestion`]): a suggestion
    /// that arrives now is dropped, even if the line is empty again.
    pub suggestion_closed: bool,
    /// What each chip in `value` stands for: chip `CHIP_BASE + i` is `pastes[i]`, shown as `#i+1`. Emptied with the
    /// line ([`Self::set`]).
    pastes: Vec<String>,
    /// The last submission's chips (taken by [`Self::remember`]) and the history entries that had chips.
    submitted: Option<Recall>,
    recalls: Vec<Recall>,
    /// The selection's other end (the cursor is one end): `None`, or the same place as the cursor, selects nothing.
    anchor: Option<usize>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// The last edit was a typed word char: the next one joins its undo step.
    typing: bool,
    /// Where the press that started a drag in the box put the cursor, until the release.
    dragging: Option<usize>,
    shown: RefCell<Shown>,
}

impl PromptInput {
    /// Replaces the text from outside (history, Tab, the App clearing it): the cursor goes to the end.
    pub fn set(&mut self, value: impl Into<String>) {
        self.value = String::new();
        self.pastes.clear();
        self.cursor = 0;
        self.menu_index = 0;
        self.anchor = None;
        self.undo.clear();
        self.redo.clear();
        self.typing = false;
        let value = value.into();
        // Text from outside with several lines (the palette's prefill never has any) is one chip, so the box stays
        // one line high; otherwise it's typed in.
        let value = no_chips(&value);
        if value.contains('\n') { self.add_chip(value) } else { self.type_text(&value) }
    }

    /// "Send now" text the turn never read, given back: at the start of the line, ahead of what's there (chips
    /// included, untouched), one message per line. Its line breaks stay as typed text (each draws as a new row), not
    /// a chip as [`Self::set`] would make: it's the person's own message, back to change or send.
    pub fn give_back(&mut self, text: &str) {
        let text = no_chips(text);
        if text.is_empty() {
            return;
        }
        let before = self.snapshot();
        self.value = if self.value.trim().is_empty() { text } else { format!("{text}\n{}", self.value) };
        self.cursor = self.value.chars().count();
        self.menu_index = 0;
        self.anchor = None;
        self.push_undo(before, false);
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot { value: self.value.clone(), cursor: self.cursor }
    }

    /// An edit changed the line from `before`: ctrl+z goes back there. A typed word char right after another joins
    /// its step, so ctrl+z takes back a word, not a letter.
    fn push_undo(&mut self, before: Snapshot, typed: bool) {
        self.redo.clear();
        if !(typed && self.typing) {
            self.undo.push(before);
            if self.undo.len() > UNDO_STEPS {
                self.undo.remove(0);
            }
        }
        self.typing = typed;
    }

    /// ctrl+z (`back`) or ctrl+y: the line as it was before the last edit, or after the one undone.
    fn undo_redo(&mut self, back: bool) {
        let (from, to) = if back { (&mut self.undo, &mut self.redo) } else { (&mut self.redo, &mut self.undo) };
        let Some(snapshot) = from.pop() else { return };
        to.push(Snapshot { value: std::mem::replace(&mut self.value, snapshot.value), cursor: self.cursor });
        self.cursor = snapshot.cursor.min(self.value.chars().count());
        self.anchor = None;
        self.typing = false;
        self.menu_index = 0;
    }

    /// The selection, `[start, end)` in chars, when it holds something.
    fn selection(&self) -> Option<(usize, usize)> {
        let anchor = self.anchor.filter(|&a| a != self.cursor)?;
        Some((anchor.min(self.cursor), anchor.max(self.cursor)))
    }

    /// Whether some text is selected (the App's ctrl+x cuts it rather than dropping a queued message).
    pub fn has_selection(&self) -> bool {
        self.selection().is_some()
    }

    /// The selected text with its chips expanded, and the selection dropped: the App's ctrl+c copies it.
    pub fn take_selection(&mut self) -> Option<String> {
        let text = self.selected_text();
        self.anchor = None;
        text
    }

    fn selected_text(&self) -> Option<String> {
        let (start, end) = self.selection()?;
        let part: String = self.value.chars().skip(start).take(end - start).collect();
        Some(self.expand(&part, &self.pastes))
    }

    /// Deletes `[start, end)` and puts the cursor there.
    fn delete_range(&mut self, start: usize, end: usize) {
        let (from, to) = (self.byte_at(start), self.byte_at(end));
        self.value.replace_range(from..to, "");
        self.cursor = start;
        self.anchor = None;
        self.menu_index = 0;
    }

    /// Deletes the selection, if there's one: whether there was.
    fn delete_selection(&mut self) -> bool {
        let Some((start, end)) = self.selection() else {
            self.anchor = None;
            return false;
        };
        self.delete_range(start, end);
        true
    }

    /// Where a word move from the cursor lands: back (`-1`) to the start of the word before it, or on (`1`) to the
    /// end of the word after it (spaces skipped first).
    fn word_from(&self, direction: isize) -> usize {
        let chars: Vec<char> = self.value.chars().collect();
        let mut at = self.cursor.min(chars.len());
        if direction < 0 {
            while at > 0 && kind(chars[at - 1]) == 0 {
                at -= 1;
            }
            let word = at.checked_sub(1).map(|i| kind(chars[i]));
            while at > 0 && Some(kind(chars[at - 1])) == word {
                at -= 1;
            }
        } else {
            while at < chars.len() && kind(chars[at]) == 0 {
                at += 1;
            }
            let word = chars.get(at).map(|&c| kind(c));
            while at < chars.len() && Some(kind(chars[at])) == word {
                at += 1;
            }
        }
        at
    }

    /// The start of the run of non-spaces before the cursor (spaces right before it skipped first).
    fn spaced_word_back(&self) -> usize {
        let chars: Vec<char> = self.value.chars().collect();
        let mut at = self.cursor.min(chars.len());
        while at > 0 && chars[at - 1].is_whitespace() {
            at -= 1;
        }
        while at > 0 && !chars[at - 1].is_whitespace() {
            at -= 1;
        }
        at
    }

    /// The start (`false`) or end of the line the cursor is on: lines are split by the `\n`s "send now" gives back.
    fn line_edge(&self, end: bool) -> usize {
        let chars: Vec<char> = self.value.chars().collect();
        let mut at = self.cursor.min(chars.len());
        if end {
            while at < chars.len() && chars[at] != '\n' {
                at += 1;
            }
        } else {
            while at > 0 && chars[at - 1] != '\n' {
                at -= 1;
            }
        }
        at
    }

    /// Where ↑ (`-1`) or ↓ takes the cursor: the same column of the drawn row above or below (a shorter row, its
    /// end). `None` on the first or last row, or before the box was drawn: there ↑/↓ are the history's.
    fn row_move(&self, direction: isize) -> Option<usize> {
        let shown = self.shown.borrow();
        let row = shown.rows.iter().position(|r| r.holds(self.cursor))?;
        let target = shown.rows.get(row.checked_add_signed(direction)?)?;
        let column = shown.rows[row].cells.iter().position(|&i| i == self.cursor).unwrap_or(shown.rows[row].cells.len());
        Some(target.cells.get(column).copied().unwrap_or(target.end))
    }

    /// The cursor's place for a key that moves it, or `None` for a key that doesn't.
    fn motion(&self, key: KeyEvent) -> Option<usize> {
        let word = key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let len = self.value.chars().count();
        match key.code {
            KeyCode::Left if word => Some(self.word_from(-1)),
            KeyCode::Right if word => Some(self.word_from(1)),
            KeyCode::Char('b') if alt => Some(self.word_from(-1)),
            KeyCode::Char('f') if alt => Some(self.word_from(1)),
            KeyCode::Left => Some(self.cursor.saturating_sub(1)),
            KeyCode::Right => Some((self.cursor + 1).min(len)),
            KeyCode::Home => Some(self.line_edge(false)),
            KeyCode::End => Some(self.line_edge(true)),
            KeyCode::Char('a') if ctrl => Some(self.line_edge(false)),
            KeyCode::Char('e') if ctrl => Some(self.line_edge(true)),
            KeyCode::Up if !word => self.row_move(-1),
            KeyCode::Down if !word => self.row_move(1),
            _ => None,
        }
    }

    /// The text with every chip replaced by what was pasted: what Enter submits.
    pub fn text(&self) -> String {
        self.expand(&self.value, &self.pastes)
    }

    fn expand(&self, value: &str, pastes: &[String]) -> String {
        let mut out = String::with_capacity(value.len());
        for c in value.chars() {
            match pastes.get((c as u32).wrapping_sub(CHIP_BASE) as usize).filter(|_| is_chip(c)) {
                Some(text) => out.push_str(text),
                None => out.push(c),
            }
        }
        out
    }

    /// Puts a chip for `text` at the cursor.
    fn add_chip(&mut self, text: String) {
        let Some(chip) = char::from_u32(CHIP_BASE + self.pastes.len() as u32).filter(|&c| is_chip(c)) else {
            return self.type_text(&no_chips(&text));
        };
        self.pastes.push(text);
        self.type_text(&chip.to_string());
    }

    /// Offers `text` as the next message, while the line is empty and the user hasn't typed, pasted or pressed Esc
    /// since the turn ended (what they did meanwhile wins). Only its printable text is kept: what Tab puts in the line
    /// is what was shown.
    pub fn suggest(&mut self, text: &str) {
        let text = printable(text);
        // Never a slash command: Enter would run it, and a suggestion is only ever a message to the model.
        let command = text.trim_start().starts_with('/');
        if self.value.is_empty() && !self.suggestion_closed && !text.trim().is_empty() && !command {
            self.suggestion = Some(text);
        }
    }

    /// A turn ended: the suggestion asked for now may be shown when it comes.
    pub fn expect_suggestion(&mut self) {
        self.suggestion_closed = false;
    }

    /// Typing, pasting or Esc: no suggestion now, and none that's still on its way.
    fn close_suggestion(&mut self) {
        self.suggestion = None;
        self.suggestion_closed = true;
    }

    /// Adds a submission to the history (not twice in a row), as the App does on submit.
    pub fn remember(&mut self, text: &str) {
        if let Some(recall) = self.submitted.take().filter(|r| r.text.trim() == text) {
            self.recalls.retain(|r| r.text.trim() != text);
            self.recalls.push(recall);
        }
        if self.history.last().map(String::as_str) != Some(text) {
            self.history.push(text.to_string());
        }
        // At most one per distinct history entry (deduped above); never more entries than the history has.
        let extra = self.recalls.len().saturating_sub(self.history.len());
        self.recalls.drain(..extra);
    }

    /// Forgets the last submission's chips without remembering it: a login callback holds an authorization code,
    /// and nothing of it is kept.
    pub fn forget_submission(&mut self) {
        self.submitted = None;
    }

    /// Whether the last submission's chips are still held (tests: a login callback never is).
    #[cfg(test)]
    pub(crate) fn holds_submission(&self) -> bool {
        self.submitted.is_some()
    }

    /// Puts history entry `i` back in the line, with its chips if it had any.
    fn recall(&mut self, i: usize) {
        let text = self.history[i].clone();
        match self.recalls.iter().rev().find(|r| r.text.trim() == text).cloned() {
            Some(recall) => {
                self.set("");
                self.value = recall.value;
                self.pastes = recall.pastes;
                self.cursor = self.value.chars().count();
            }
            None => self.set(text),
        }
    }

    fn byte_at(&self, chars: usize) -> usize {
        self.value.char_indices().nth(chars).map_or(self.value.len(), |(i, _)| i)
    }

    fn type_text(&mut self, text: &str) {
        let at = self.byte_at(self.cursor);
        self.value.insert_str(at, text);
        self.cursor += text.chars().count();
        self.menu_index = 0;
    }

    /// The menu's matches, or none when it's closed (busy, nothing matches, or dismissed).
    pub fn menu<'a>(&self, busy: bool, commands: &'a [CommandInfo]) -> Vec<&'a CommandInfo> {
        if busy || self.dismissed_at.as_deref() == Some(self.value.as_str()) {
            return Vec::new();
        }
        match_commands(&self.value, commands)
    }

    pub fn on_key(&mut self, key: KeyEvent, busy: bool, commands: &[CommandInfo]) -> Option<PromptOutcome> {
        if self.value.is_empty()
            && !busy
            && matches!(key.code, KeyCode::Tab | KeyCode::Right)
            && let Some(suggestion) = self.suggestion.take()
        {
            self.set(suggestion);
            self.close_suggestion();
            return None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Char('z' | 'Z') if ctrl => {
                self.undo_redo(!shift);
                return None;
            }
            KeyCode::Char('y') if ctrl => {
                self.undo_redo(false);
                return None;
            }
            _ => {}
        }
        let before = self.snapshot();
        let typed = matches!(key.code, KeyCode::Char(c) if !c.is_whitespace())
            && !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let outcome = self.edit(key, busy, commands);
        if self.value != before.value {
            self.push_undo(before.clone(), typed);
        } else {
            self.typing = false;
        }
        if self.value != before.value || !self.value.is_empty() || key.code == KeyCode::Esc {
            self.close_suggestion();
        }
        outcome
    }

    fn edit(&mut self, key: KeyEvent, busy: bool, commands: &[CommandInfo]) -> Option<PromptOutcome> {
        let matches = self.menu(busy, commands);
        let highlighted = (!matches.is_empty()).then(|| matches[self.menu_index.min(matches.len() - 1)]);
        if let Some(highlighted) = highlighted {
            let n = matches.len();
            match key.code {
                KeyCode::Up => {
                    self.menu_index = (self.menu_index + n - 1) % n;
                    return None;
                }
                KeyCode::Down => {
                    self.menu_index = (self.menu_index + 1) % n;
                    return None;
                }
                // Tab or → completes the highlighted command (plus a space), so you can type its arguments.
                KeyCode::Tab | KeyCode::Right => {
                    self.set(format!("/{} ", highlighted.name));
                    return None;
                }
                KeyCode::Esc => {
                    self.dismissed_at = Some(self.value.clone());
                    return None;
                }
                _ => {}
            }
        }

        let ctrl_or_alt = key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        // A move: with shift it selects (from where the cursor was); without, ←/→ over a selection go to its edge
        // and any other move just drops it.
        if let Some(to) = self.motion(key) {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                self.anchor.get_or_insert(self.cursor);
                self.cursor = to;
            } else {
                self.cursor = match (self.selection(), key.code) {
                    (Some((start, _)), KeyCode::Left) if !ctrl_or_alt => start,
                    (Some((_, end)), KeyCode::Right) if !ctrl_or_alt => end,
                    _ => to,
                };
                self.anchor = None;
            }
            return None;
        }
        match key.code {
            KeyCode::Esc if self.selection().is_some() => self.anchor = None,
            KeyCode::Char('x') if ctrl => {
                let text = self.selected_text()?;
                self.delete_selection();
                return Some(PromptOutcome::Copy(text));
            }
            KeyCode::Backspace | KeyCode::Delete if self.selection().is_some() => {
                self.delete_selection();
            }
            // A word back (ctrl+w, alt or ctrl+Backspace), or on (alt+d, ctrl or alt+Delete); ctrl+u: to the line's
            // start.
            KeyCode::Backspace if ctrl_or_alt => self.delete_range(self.word_from(-1), self.cursor),
            // ctrl+w as readline's: back to the space before (a path or `--flag=x` goes whole).
            KeyCode::Char('w') if ctrl => self.delete_range(self.spaced_word_back(), self.cursor),
            KeyCode::Char('u') if ctrl => self.delete_range(self.line_edge(false), self.cursor),
            KeyCode::Delete if ctrl_or_alt => self.delete_range(self.cursor, self.word_from(1)),
            KeyCode::Char('d') if alt => self.delete_range(self.cursor, self.word_from(1)),
            KeyCode::Enter => {
                self.history_index = None;
                self.dismissed_at = None;
                // With the menu open, Enter runs the highlighted command ("/se" → "/setup").
                let text = highlighted.map_or_else(|| self.text(), |c| format!("/{}", c.name));
                self.submitted = (highlighted.is_none() && !self.pastes.is_empty()).then(|| Recall {
                    text: text.clone(),
                    value: self.value.clone(),
                    pastes: self.pastes.clone(),
                });
                // Alt+Enter on a message: send it now (the pastes expanded, as Enter sends them). A command is a
                // command either way.
                if key.modifiers.contains(KeyModifiers::ALT) && highlighted.is_none() {
                    return Some(PromptOutcome::SendNow(text));
                }
                return Some(PromptOutcome::Submit(text));
            }
            // ctrl+s: the same as Alt+Enter, for terminals that keep Alt+Enter (Windows Terminal: full screen). Raw
            // mode turns off XON/XOFF, so it arrives as a key.
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.history_index = None;
                self.dismissed_at = None;
                if let Some(c) = highlighted {
                    return Some(PromptOutcome::Submit(format!("/{}", c.name)));
                }
                let text = self.text();
                self.submitted = (!self.pastes.is_empty()).then(|| Recall {
                    text: text.clone(),
                    value: self.value.clone(),
                    pastes: self.pastes.clone(),
                });
                return Some(PromptOutcome::SendNow(text));
            }
            KeyCode::Up if !self.history.is_empty() => {
                let next = self.history_index.map_or(self.history.len() - 1, |i| i.saturating_sub(1));
                self.history_index = Some(next);
                self.recall(next);
            }
            KeyCode::Down => {
                if let Some(i) = self.history_index {
                    if i + 1 >= self.history.len() {
                        self.history_index = None;
                        self.set("");
                    } else {
                        self.history_index = Some(i + 1);
                        self.recall(i + 1);
                    }
                }
            }
            KeyCode::Backspace if self.cursor > 0 => {
                let at = self.byte_at(self.cursor - 1);
                self.value.remove(at);
                self.cursor -= 1;
                self.menu_index = 0;
            }
            KeyCode::Delete if self.cursor < self.value.chars().count() => {
                let at = self.byte_at(self.cursor);
                self.value.remove(at);
                self.menu_index = 0;
            }
            KeyCode::Char(c) if !ctrl_or_alt => {
                self.delete_selection();
                self.type_text(&no_chips(&c.to_string()));
            }
            _ => {}
        }
        None
    }

    /// Pasted text (bracketed paste): never submits. Several lines, or a long line, go in as a chip with the
    /// newlines kept (CRLF and CR read as LF); a short line is typed in (without a trailing newline).
    pub fn paste(&mut self, text: &str) {
        let text = no_chips(&text.replace("\r\n", "\n").replace('\r', "\n"));
        if text.is_empty() {
            return;
        }
        let before = self.snapshot();
        self.delete_selection();
        let line = text.trim_end_matches('\n');
        if line.contains('\n') || line.chars().count() > PASTE_CHIP_CHARS {
            self.add_chip(text);
        } else {
            self.type_text(line);
        }
        self.push_undo(before, false);
        self.close_suggestion();
    }

    /// A press at a screen cell: in the box it's the prompt's, and moves the cursor there (a drag from it selects).
    /// Elsewhere it drops the prompt's selection and is the transcript's. Whether it was the prompt's.
    pub fn press(&mut self, x: u16, y: u16) -> bool {
        self.anchor = None;
        self.dragging = None;
        let Some(at) = self.index_at(x, y, true) else { return false };
        self.cursor = at;
        self.dragging = Some(at);
        self.typing = false;
        true
    }

    /// A drag after a press in the box: selects from the press to the cell under the pointer (above the box, to the
    /// text's start; below it, to its end). Whether it was the prompt's.
    pub fn drag(&mut self, x: u16, y: u16) -> bool {
        let Some(start) = self.dragging else { return false };
        if let Some(at) = self.index_at(x, y, false) {
            // Forward onto a chip's label: the chip is in, whole (its label is all one char).
            let on_chip = self.value.chars().nth(at).is_some_and(is_chip);
            self.anchor = Some(start);
            self.cursor = if on_chip && at >= start { at + 1 } else { at };
        }
        true
    }

    /// Whether a press in the box started what the mouse is doing (the release is the prompt's, not the transcript's).
    pub fn dragging(&self) -> bool {
        self.dragging.is_some()
    }

    /// The release after a press in the box: what the drag selected, to copy (the selection stays, for ctrl+x or
    /// typing over it). A click selects nothing.
    pub fn release(&mut self) -> Option<String> {
        self.dragging.take()?;
        self.selected_text()
    }

    /// The char a screen cell shows, as a cursor place. `inside`: only a cell in the box counts (a press); else a
    /// point above or below the text clamps to its start or end (a drag).
    fn index_at(&self, x: u16, y: u16, inside: bool) -> Option<usize> {
        let shown = self.shown.borrow();
        let area = shown.area;
        let in_box = x >= area.x && x < area.right() && y >= area.y && y < area.bottom();
        if (inside && !in_box) || shown.rows.is_empty() {
            return None;
        }
        if y < shown.y {
            return Some(0);
        }
        let Some(row) = shown.rows.get((y - shown.y) as usize) else {
            return Some(self.value.chars().count());
        };
        let column = x.saturating_sub(shown.x) as usize;
        Some(row.cells.get(column).copied().unwrap_or(row.end))
    }

    /// The prompt isn't drawn this frame (a panel is open over it): the mouse can't reach its text.
    pub fn hide(&mut self) {
        *self.shown.get_mut() = Shown::default();
    }

    /// The text rows inside the box: `> ` then the text, wrapped by cell with a hanging indent,
    /// the cursor in inverse video.
    fn text_rows(&self, inner_width: u16, busy: bool) -> Vec<Line<'static>> {
        self.layout(inner_width, busy).0
    }

    /// [`Self::text_rows`], and the char each of their cells shows (past the gutter), for the mouse and ↑/↓.
    fn layout(&self, inner_width: u16, busy: bool) -> (Vec<Line<'static>>, Vec<RowMap>) {
        let accent = Style::new().fg(theme::ACCENT);
        let cursor_style = Style::new().add_modifier(Modifier::REVERSED);
        let selected_style = Style::new().bg(theme::DIM).fg(ratatui::style::Color::Black);
        let selection = self.selection();
        if self.value.is_empty() {
            // The model's words: printable() like everything it writes.
            let suggested = self.suggestion.as_deref().map(|s| format!("{}  (tab)", printable(s)));
            let placeholder = if busy { "" } else { suggested.as_deref().unwrap_or(PLACEHOLDER) };
            let mut chars = placeholder.chars();
            let first = chars.next().map_or_else(|| " ".to_string(), |c| c.to_string());
            let line = Line::from(vec![
                Span::styled("> ", accent),
                Span::styled(first, cursor_style.fg(theme::DIM)),
                Span::styled(chars.collect::<String>(), Style::new().fg(theme::DIM)),
            ]);
            return (vec![truncate_end(line, inner_width)], vec![RowMap { cells: vec![0], end: 0 }]);
        }
        let width = inner_width.saturating_sub(2).max(1) as usize;
        let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
        let mut maps: Vec<RowMap> = vec![RowMap::default()];
        // A row the text wrapped out of: a click past its end stays on it, at its last char.
        let wrap = |maps: &mut Vec<RowMap>, at: usize| {
            let last = maps.last_mut().expect("never empty");
            last.end = last.cells.last().copied().unwrap_or(at);
            maps.push(RowMap::default());
        };
        let mut used = 0;
        let chip_style = Style::new().fg(theme::DIM);
        let chars: Vec<char> = self.value.chars().collect();
        // Each char of the value as the cells it shows (a chip as its label) and their style.
        let clean: Vec<(Vec<String>, Style)> = chars
            .iter()
            .map(|&c| match self.pastes.get((c as u32).wrapping_sub(CHIP_BASE) as usize).filter(|_| is_chip(c)) {
                Some(text) => {
                    let number = (c as u32 - CHIP_BASE) as usize + 1;
                    (chip_label(number, text).chars().map(String::from).collect(), chip_style)
                }
                None => (vec![printable(&c.to_string())], Style::new()),
            })
            .collect();
        for i in 0..=clean.len() {
            // A line break (only text given back by "send now" has one: typing never makes one, and a paste's stay
            // inside its chip) starts a new row; the cursor on it shows as a space at the end of its line.
            if chars.get(i) == Some(&'\n') {
                if i == self.cursor {
                    rows.last_mut().expect("never empty").push(Span::styled(" ", cursor_style));
                    maps.last_mut().expect("never empty").cells.push(i);
                }
                maps.last_mut().expect("never empty").end = i;
                maps.push(RowMap::default());
                rows.push(Vec::new());
                used = 0;
                continue;
            }
            let (cells, style) = match clean.get(i) {
                Some((cells, style)) => (cells.clone(), *style),
                None => (vec![" ".to_string()], Style::new()), // the cursor past the end
            };
            let style = if i == self.cursor {
                style.patch(cursor_style)
            } else if selection.is_some_and(|(start, end)| (start..end).contains(&i)) {
                style.patch(selected_style)
            } else {
                style
            };
            // A chip that fits on a row isn't split across two.
            let total: usize = cells.iter().map(|c| c.width()).sum();
            if cells.len() > 1 && used > 0 && used + total > width && total <= width {
                rows.push(Vec::new());
                wrap(&mut maps, i);
                used = 0;
            }
            for text in cells {
                let w = text.width();
                if w == 0 && i != self.cursor {
                    continue;
                }
                let (text, w) = if w == 0 { (" ".to_string(), 1) } else { (text, w) };
                if used + w > width {
                    rows.push(Vec::new());
                    wrap(&mut maps, i);
                    used = 0;
                }
                rows.last_mut().expect("never empty").push(Span::styled(text, style));
                maps.last_mut().expect("never empty").cells.extend(std::iter::repeat_n(i, w));
                used += w;
            }
        }
        maps.last_mut().expect("never empty").end = chars.len();
        let lines = rows
            .into_iter()
            .enumerate()
            .map(|(i, spans)| {
                let gutter = if i == 0 { Span::styled("> ", accent) } else { Span::raw("  ") };
                Line::from([vec![gutter], spans].concat())
            })
            .collect();
        (lines, maps)
    }

    /// The menu rows: `❯ /name  description`, then the keys.
    fn menu_rows(&self, width: u16, busy: bool, commands: &[CommandInfo]) -> Vec<Line<'static>> {
        let matches = self.menu(busy, commands);
        if matches.is_empty() {
            return Vec::new();
        }
        let highlighted = self.menu_index.min(matches.len() - 1);
        let name_width = matches.iter().map(|c| c.name.width()).max().unwrap_or(0) + 1;
        let inner = width.saturating_sub(4);
        let mut rows: Vec<Line<'static>> = matches
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let active = i == highlighted;
                let style = if active { Style::new().fg(theme::ACCENT) } else { Style::new() };
                let name = format!("/{}", c.name);
                let pad = (name_width + 2).saturating_sub(name.width());
                let line = Line::from(vec![
                    Span::styled(if active { "❯ " } else { "  " }, style),
                    Span::styled(format!("{name}{}", " ".repeat(pad)), style),
                    Span::styled(printable(&c.description), Style::new().fg(theme::DIM)),
                ]);
                truncate_end(line, inner)
            })
            .collect();
        rows.push(truncate_end(
            Line::styled("  ↑/↓ select · Enter run · Tab/→ complete · Esc hide", Style::new().fg(theme::DIM)),
            inner,
        ));
        rows
    }

    pub fn height(&self, width: u16, busy: bool, commands: &[CommandInfo]) -> u16 {
        let text = self.text_rows(width.saturating_sub(4), busy).len();
        (text + 2 + self.menu_rows(width, busy, commands).len()) as u16
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer, busy: bool, commands: &[CommandInfo]) {
        let (rows, maps) = self.layout(area.width.saturating_sub(4), busy);
        let box_area = Rect { height: (rows.len() as u16 + 2).min(area.height), ..area };
        let border = if busy { theme::DIM } else { theme::ACCENT };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(border))
            .padding(Padding::horizontal(1));
        let inner = block.inner(box_area);
        block.render(box_area, buf);
        for (i, line) in rows.iter().enumerate().take(inner.height as usize) {
            buf.set_line(inner.x, inner.y + i as u16, line, inner.width);
        }
        // The text's cells start past the gutter ("> "). An empty line shows the placeholder: a click there is at 0.
        let maps = if self.value.is_empty() { vec![RowMap { cells: Vec::new(), end: 0 }] } else { maps };
        *self.shown.borrow_mut() =
            Shown { area: box_area, x: inner.x + 2, y: inner.y, rows: maps.into_iter().take(inner.height as usize).collect() };
        // The menu, paddingX 2.
        let menu_top = box_area.bottom();
        for (i, line) in self.menu_rows(area.width, busy, commands).iter().enumerate() {
            let y = menu_top + i as u16;
            if y >= area.bottom() {
                break;
            }
            buf.set_line(area.x + 2, y, line, area.width.saturating_sub(4));
        }
    }

    /// Display width of the text, for tests.
    #[cfg(test)]
    fn shown(&self) -> String {
        use unicode_width::UnicodeWidthChar;
        self.value.chars().filter(|c| c.width().unwrap_or(0) > 0).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commands() -> Vec<CommandInfo> {
        [
            ("help", "Show available commands"),
            ("model", "Switch model"),
            ("setup", "Change provider"),
            ("think", "Toggle thinking"),
        ]
        .into_iter()
        .map(|(n, d)| CommandInfo::new(n, d))
        .collect()
    }

    /// Like the App: submitting clears the input and remembers it.
    struct Harness {
        prompt: PromptInput,
        commands: Vec<CommandInfo>,
        submitted: Vec<String>,
    }

    impl Harness {
        fn new(history: &[&str]) -> Self {
            let prompt = PromptInput { history: history.iter().map(|s| s.to_string()).collect(), ..Default::default() };
            Self { prompt, commands: commands(), submitted: vec![] }
        }

        fn key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
            if let Some(PromptOutcome::Submit(text)) = self.prompt.on_key(KeyEvent::new(code, modifiers), false, &self.commands) {
                self.prompt.set("");
                self.submitted.push(text);
            }
        }

        fn press(&mut self, codes: &[KeyCode]) {
            for &code in codes {
                self.key(code, KeyModifiers::NONE);
            }
        }

        fn typed(&mut self, text: &str) {
            for c in text.chars() {
                self.key(KeyCode::Char(c), KeyModifiers::NONE);
            }
        }

        fn frame(&self) -> String {
            let width = 60;
            let area = Rect::new(0, 0, width, self.prompt.height(width, false, &self.commands));
            let mut buf = Buffer::empty(area);
            self.prompt.render(area, &mut buf, false, &self.commands);
            (0..area.height).map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>()).collect::<Vec<_>>().join("\n")
        }
    }

    #[test]
    fn typing_slash_lists_every_command_with_its_description() {
        let mut h = Harness::new(&[]);
        h.typed("/");
        for c in commands() {
            assert!(h.frame().contains(&format!("/{}", c.name)));
            assert!(h.frame().contains(&c.description));
        }
    }

    #[test]
    fn narrows_as_you_type_and_enter_runs_the_highlighted_command() {
        let mut h = Harness::new(&[]);
        h.typed("/se");
        assert!(h.frame().contains("/setup"));
        assert!(!h.frame().contains("/help"));
        h.press(&[KeyCode::Enter]);
        assert_eq!(h.submitted, ["/setup"]);
    }

    #[test]
    fn up_down_move_through_the_menu_instead_of_input_history() {
        let mut h = Harness::new(&["old message"]);
        h.typed("/");
        h.press(&[KeyCode::Down, KeyCode::Down, KeyCode::Enter]);
        assert_eq!(h.submitted, ["/setup"]);
        h.typed("/");
        h.press(&[KeyCode::Up, KeyCode::Enter]); // wraps around to the last one
        assert_eq!(h.submitted.last().unwrap(), "/think");
    }

    #[test]
    fn tab_completes_the_command_so_you_can_add_arguments_with_the_cursor_at_the_end() {
        let mut h = Harness::new(&[]);
        h.typed("/th");
        h.press(&[KeyCode::Tab]);
        assert!(h.frame().contains("> /think"));
        assert!(!h.frame().contains("Toggle thinking")); // menu closes once there's a space
        h.typed("on");
        h.press(&[KeyCode::Enter]);
        assert_eq!(h.submitted, ["/think on"]);
    }

    #[test]
    fn right_completes_the_highlighted_command_too() {
        let mut h = Harness::new(&[]);
        h.typed("/");
        h.press(&[KeyCode::Down, KeyCode::Right]); // highlight /model, then complete it
        assert!(h.frame().contains("> /model"));
        assert!(!h.frame().contains("Switch model")); // the menu closed
        h.typed("qwen3.5:9b");
        h.press(&[KeyCode::Enter]);
        assert_eq!(h.submitted, ["/model qwen3.5:9b"]);
    }

    #[test]
    fn esc_hides_the_menu_and_enter_then_submits_exactly_what_was_typed() {
        let mut h = Harness::new(&[]);
        h.typed("/h");
        h.press(&[KeyCode::Esc]);
        assert!(!h.frame().contains("Show available commands"));
        h.press(&[KeyCode::Enter]);
        assert_eq!(h.submitted, ["/h"]);
        // Typing again brings it back.
        h.typed("/h");
        assert!(h.frame().contains("Show available commands"));
    }

    #[test]
    fn no_menu_for_ordinary_messages_and_up_recalls_history_with_the_cursor_at_the_end() {
        let mut h = Harness::new(&["hello"]);
        h.press(&[KeyCode::Up]);
        h.typed("!");
        assert!(!h.frame().contains("/help"));
        h.press(&[KeyCode::Enter]);
        assert_eq!(h.submitted, ["hello!"]);
    }

    #[test]
    fn history_goes_back_and_forth_and_past_the_newest_to_an_empty_line() {
        let mut h = Harness::new(&["one", "two"]);
        h.press(&[KeyCode::Up, KeyCode::Up, KeyCode::Up]);
        assert_eq!(h.prompt.value, "one");
        h.press(&[KeyCode::Down]);
        assert_eq!(h.prompt.value, "two");
        h.press(&[KeyCode::Down]);
        assert_eq!(h.prompt.value, "");
        assert_eq!(h.prompt.history_index, None);
    }

    #[test]
    fn match_commands_puts_prefix_matches_first_then_other_matches() {
        let c = commands();
        let names = |v: &str| match_commands(v, &c).iter().map(|c| c.name.clone()).collect::<Vec<_>>();
        assert_eq!(names("/"), ["help", "model", "setup", "think"]);
        assert_eq!(names("/e"), ["help", "model", "setup"]);
        assert_eq!(names("/s"), ["setup"]);
        assert!(names("/model x").is_empty()); // has arguments: menu closed
        assert!(names("hi").is_empty());
    }

    #[test]
    fn an_argument_completion_is_offered_once_its_command_and_subcommand_are_typed() {
        let mut c = commands();
        for n in ["mcp", "mcp login github", "mcp login linear", "mcp logout github"] {
            c.push(CommandInfo::new(n, ""));
        }
        let names = |v: &str| match_commands(v, &c).iter().map(|c| c.name.clone()).collect::<Vec<_>>();
        assert_eq!(names("/mcp login "), ["mcp login github", "mcp login linear"]);
        assert_eq!(names("/mcp login l"), ["mcp login linear"]);
        assert_eq!(names("/mcp logout "), ["mcp logout github"]);
        // Not before: "/mcp " + Enter must still run /mcp, and "/m" lists commands, not arguments.
        assert!(names("/mcp ").is_empty());
        assert_eq!(names("/m"), ["model", "mcp"]);
        assert_eq!(names("/mcp login github"), ["mcp login github"]);
    }

    #[test]
    fn ctrl_letter_combinations_dont_type_the_letter() {
        let mut h = Harness::new(&[]);
        h.key(KeyCode::Char('o'), KeyModifiers::CONTROL);
        h.key(KeyCode::Char('a'), KeyModifiers::CONTROL);
        h.typed("x");
        h.press(&[KeyCode::Enter]);
        assert_eq!(h.submitted, ["x"]);
    }

    // Marv's ctrl+←/→ moved one char (ink-text-input); here they move by words, as in a text field.
    #[test]
    fn ctrl_arrows_arent_typed_and_move_by_words() {
        let mut h = Harness::new(&[]);
        h.typed("ab cd");
        h.key(KeyCode::Left, KeyModifiers::CONTROL);
        h.typed("x");
        h.key(KeyCode::Right, KeyModifiers::CONTROL);
        h.typed("y");
        h.press(&[KeyCode::Enter]);
        assert_eq!(h.submitted, ["ab xcdy"]);
    }

    #[test]
    fn word_moves_and_word_and_line_deletes() {
        let mut h = Harness::new(&[]);
        h.typed("git commit --amend now");
        h.key(KeyCode::Char('b'), KeyModifiers::ALT); // option+← on a Mac
        h.key(KeyCode::Left, KeyModifiers::ALT);
        assert_eq!(h.prompt.cursor, "git commit --".len(), "punctuation is a word of its own");
        h.key(KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert_eq!(h.prompt.value, "git commit amend now");
        h.key(KeyCode::Char('d'), KeyModifiers::ALT);
        assert_eq!(h.prompt.value, "git commit  now");
        h.key(KeyCode::Backspace, KeyModifiers::ALT);
        assert_eq!(h.prompt.value, "git  now");
        h.key(KeyCode::Char('f'), KeyModifiers::ALT);
        h.key(KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!(h.prompt.value, "");
        // Home/End and ctrl+a/e are the line's edges, not the text's.
        h.prompt.give_back("one\ntwo");
        h.key(KeyCode::Char('a'), KeyModifiers::CONTROL);
        h.typed(">");
        h.key(KeyCode::Up, KeyModifiers::NONE); // not drawn yet: ↑ is the history's (empty), nothing moves
        assert_eq!(h.prompt.value, "one\n>two");
        h.press(&[KeyCode::Left, KeyCode::Left, KeyCode::Home]);
        h.typed("<");
        h.key(KeyCode::Char('e'), KeyModifiers::CONTROL);
        h.typed("!");
        assert_eq!(h.prompt.value, "<one!\n>two");
        // ctrl+w goes back to the space (readline's), alt+Backspace to the punctuation.
        h.prompt.set("open src/ui/a.rs");
        h.key(KeyCode::Backspace, KeyModifiers::ALT);
        assert_eq!(h.prompt.value, "open src/ui/a.");
        h.key(KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert_eq!(h.prompt.value, "open ");
    }

    #[test]
    fn shift_selects_and_typing_backspace_and_ctrl_x_act_on_the_selection() {
        let mut h = Harness::new(&[]);
        h.typed("hello big world");
        h.key(KeyCode::Left, KeyModifiers::CONTROL);
        h.key(KeyCode::Left, KeyModifiers::CONTROL | KeyModifiers::SHIFT);
        h.key(KeyCode::Left, KeyModifiers::SHIFT);
        assert!(h.prompt.has_selection());
        h.typed("_");
        assert_eq!(h.prompt.value, "hello_world");
        // ← over a selection goes to its start, → to its end, without moving past it.
        h.key(KeyCode::Home, KeyModifiers::SHIFT);
        h.press(&[KeyCode::Right]);
        assert_eq!((h.prompt.cursor, h.prompt.has_selection()), (6, false));
        h.key(KeyCode::End, KeyModifiers::SHIFT);
        h.press(&[KeyCode::Backspace]);
        assert_eq!(h.prompt.value, "hello_");
        // ctrl+x cuts, chips expanded; with nothing selected it does nothing.
        h.prompt.paste("a\nb");
        h.key(KeyCode::Home, KeyModifiers::SHIFT);
        let cut = h.prompt.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL), false, &h.commands);
        assert_eq!(cut, Some(PromptOutcome::Copy("hello_a\nb".into())));
        assert_eq!(h.prompt.value, "");
        assert_eq!(h.prompt.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL), false, &h.commands), None);
        // take_selection (the App's ctrl+c) copies and drops it, the text stays.
        h.typed("keep");
        h.key(KeyCode::Left, KeyModifiers::SHIFT);
        assert_eq!(h.prompt.take_selection().as_deref(), Some("p"));
        assert!(!h.prompt.has_selection());
        assert_eq!(h.prompt.value, "keep");
    }

    #[test]
    fn ctrl_z_takes_back_a_word_or_a_paste_and_ctrl_y_puts_it_again() {
        let mut h = Harness::new(&[]);
        h.typed("run the");
        h.prompt.paste("x\ny");
        h.typed("tests");
        h.key(KeyCode::Char('z'), KeyModifiers::CONTROL);
        assert_eq!(h.prompt.text(), "run thex\ny");
        h.key(KeyCode::Char('z'), KeyModifiers::CONTROL);
        assert_eq!(h.prompt.text(), "run the", "the paste is one step");
        h.key(KeyCode::Char('z'), KeyModifiers::CONTROL);
        assert_eq!(h.prompt.text(), "run ");
        h.key(KeyCode::Char('y'), KeyModifiers::CONTROL);
        h.key(KeyCode::Char('Z'), KeyModifiers::CONTROL | KeyModifiers::SHIFT);
        assert_eq!(h.prompt.text(), "run thex\ny", "redo, and the chip still stands for its paste");
        // A new edit drops what could be redone; a new line drops the undo.
        h.typed("!");
        h.key(KeyCode::Char('y'), KeyModifiers::CONTROL);
        assert_eq!(h.prompt.text(), "run thex\ny!");
        h.press(&[KeyCode::Enter]);
        h.key(KeyCode::Char('z'), KeyModifiers::CONTROL);
        assert_eq!(h.prompt.value, "");
        // Only the last UNDO_STEPS edits are kept.
        for i in 0..UNDO_STEPS + 5 {
            h.typed(if i % 2 == 0 { "a" } else { " " });
        }
        for _ in 0..UNDO_STEPS + 5 {
            h.key(KeyCode::Char('z'), KeyModifiers::CONTROL);
        }
        assert!(!h.prompt.value.is_empty());
    }

    #[test]
    fn up_and_down_move_between_rows_and_reach_the_history_only_at_the_ends() {
        let mut h = Harness::new(&["older"]);
        h.prompt.give_back("first line\nsecond");
        h.frame();
        h.press(&[KeyCode::Up]);
        h.frame();
        assert_eq!(h.prompt.cursor, "first ".len(), "the same column on the row above");
        h.press(&[KeyCode::Down]);
        assert_eq!(h.prompt.cursor, "first line\nsecond".len(), "a shorter row: its end");
        h.frame();
        h.press(&[KeyCode::Up, KeyCode::Up]);
        assert_eq!(h.prompt.value, "older", "from the first row, ↑ is the history's");
        // A wrapped line's rows too.
        h.prompt.set("x".repeat(80));
        h.frame();
        h.key(KeyCode::Up, KeyModifiers::SHIFT);
        assert!(h.prompt.has_selection());
        assert_eq!(h.prompt.value, "x".repeat(80));
    }

    #[test]
    fn a_drag_in_the_box_selects_and_its_release_copies_and_a_click_moves_the_cursor() {
        let mut h = Harness::new(&[]);
        h.typed("see ");
        h.prompt.paste("a\nb");
        h.typed(" now");
        let frame = h.frame();
        let row = frame.lines().position(|l| l.contains("> see")).unwrap() as u16;
        let x0 = frame.lines().nth(row as usize).unwrap().chars().position(|c| c == 's').unwrap() as u16;
        assert!(!h.prompt.press(x0, row + 5), "below the box: not the prompt's");
        assert!(h.prompt.press(x0 + 2, row));
        assert_eq!(h.prompt.release(), None, "a click selects nothing");
        assert_eq!(h.prompt.cursor, 2);
        // From "e" in "see" to inside the chip's label: the chip comes whole.
        assert!(h.prompt.press(x0 + 2, row));
        assert!(h.prompt.drag(x0 + 9, row));
        let frame = h.frame();
        assert!(frame.contains("> see [Pasted text #1, 2 lines] now"), "{frame}");
        assert_eq!(h.prompt.release().as_deref(), Some("e a\nb"));
        assert!(h.prompt.press(x0 + 2, row));
        h.prompt.drag(x0 + 40, row);
        assert_eq!(h.prompt.release().as_deref(), Some("e a\nb now"), "past the end: to the end");
        h.typed("!");
        assert_eq!(h.prompt.text(), "se!", "typing replaces what the drag selected");
        // Not drawn (a panel over it): the mouse can't reach it.
        h.prompt.hide();
        assert!(!h.prompt.press(x0, row));
    }

    #[test]
    fn backspace_and_delete_edit_around_the_cursor() {
        let mut h = Harness::new(&[]);
        h.typed("héllo");
        h.press(&[KeyCode::Left, KeyCode::Backspace, KeyCode::Home, KeyCode::Delete]);
        assert_eq!(h.prompt.value, "élo");
        assert_eq!(h.prompt.shown(), "élo");
    }

    #[test]
    fn placeholder_while_empty_and_a_long_line_wraps_with_a_hanging_indent() {
        let mut h = Harness::new(&[]);
        assert!(h.frame().contains("> Type a message, or / for commands"));
        h.typed(&"x".repeat(80));
        let frame = h.frame();
        assert_eq!(h.prompt.height(60, false, &h.commands), 4);
        assert!(frame.lines().nth(2).unwrap().starts_with("│   xxx"), "{frame}");
    }

    #[test]
    fn a_suggestion_shows_in_the_empty_prompt_and_tab_takes_it_but_never_sends_it() {
        let mut h = Harness::new(&[]);
        h.prompt.suggest("run the tests\x1b[2J");
        assert!(h.frame().contains("> run the tests  (tab)"), "the escape isn't drawn: {}", h.frame());
        h.press(&[KeyCode::Enter]);
        assert_eq!(h.submitted, [""], "Enter sends the empty line (which the App ignores), not the suggestion");
        h.press(&[KeyCode::Tab]);
        assert_eq!(h.prompt.value, "run the tests", "Tab takes what was shown, never the escape");
        assert_eq!(h.prompt.suggestion, None);
        h.prompt.set("");
        h.prompt.expect_suggestion();
        // Typing drops it, and it doesn't come back when the line is emptied again.
        h.prompt.suggest("commit this");
        h.typed("n");
        assert_eq!(h.prompt.suggestion, None);
        h.press(&[KeyCode::Backspace]);
        assert!(h.frame().contains("> Type a message"), "{}", h.frame());
        // → takes it too; one that arrives after typing started is dropped.
        h.prompt.expect_suggestion();
        h.prompt.suggest("commit this");
        h.press(&[KeyCode::Right]);
        assert_eq!(h.prompt.value, "commit this");
        h.prompt.suggest("something else");
        assert_eq!(h.prompt.suggestion, None);
    }

    // Greptile #83: typing and erasing before the suggestion arrives, a paste, or Esc all mean "not now": the late
    // reply isn't shown on the emptied line. Esc also hides one already shown. The next turn's end opens it again.
    #[test]
    fn a_suggestion_arriving_after_typing_erasing_paste_or_esc_is_dropped() {
        let mut h = Harness::new(&[]);
        h.prompt.expect_suggestion();
        h.typed("r");
        h.press(&[KeyCode::Backspace]);
        assert_eq!(h.prompt.value, "");
        h.prompt.suggest("run the tests");
        assert_eq!(h.prompt.suggestion, None, "typed and erased");

        h.prompt.expect_suggestion();
        h.prompt.paste("x");
        h.press(&[KeyCode::Backspace]);
        h.prompt.suggest("run the tests");
        assert_eq!(h.prompt.suggestion, None, "pasted and erased");

        h.prompt.expect_suggestion();
        h.prompt.suggest("run the tests");
        h.press(&[KeyCode::Esc]);
        assert_eq!(h.prompt.suggestion, None, "Esc hides it");
        assert!(h.frame().contains("> Type a message"), "{}", h.frame());
        h.prompt.suggest("run the tests");
        assert_eq!(h.prompt.suggestion, None, "and one still on its way");

        h.prompt.expect_suggestion();
        h.prompt.suggest("run the tests");
        assert_eq!(h.prompt.suggestion.as_deref(), Some("run the tests"), "the next turn's suggestion shows");
        h.prompt.suggestion = None;
        h.prompt.suggest(" /yolo");
        assert_eq!(h.prompt.suggestion, None, "never a command");
    }

    #[test]
    fn pasted_escapes_are_not_drawn() {
        let mut h = Harness::new(&[]);
        h.prompt.paste("hi\x1b[2Jthere next\n");
        assert_eq!(h.prompt.value, "hi\x1b[2Jthere next", "a short line goes in as typed, its newline dropped");
        assert!(h.frame().contains("> hi[2Jthere next"));
    }

    // #214: a multi-line paste keeps its newlines, shows as one chip, and never submits.
    #[test]
    fn send_now_with_a_paste_chip_sends_the_pasted_text_and_given_back_lines_show_as_rows() {
        let mut h = Harness::new(&[]);
        h.typed("also ");
        h.prompt.paste("line 1\nline 2\n");
        for modifiers in [KeyModifiers::ALT, KeyModifiers::NONE] {
            let key = if modifiers == KeyModifiers::ALT {
                KeyEvent::new(KeyCode::Enter, modifiers)
            } else {
                KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)
            };
            let outcome = h.prompt.on_key(key, true, &h.commands);
            assert_eq!(outcome, Some(PromptOutcome::SendNow("also line 1\nline 2\n".into())), "{modifiers:?}");
        }
        // Text given back by "send now" (one message per line), ahead of a chip already there: each line its own row,
        // and the chip still stands for its paste.
        h.prompt.set("");
        h.prompt.paste("a\nb");
        h.prompt.give_back("wait, use pnpm\nand the lint");
        assert_eq!(h.prompt.text(), "wait, use pnpm\nand the lint\na\nb");
        let frame = h.frame();
        let rows: Vec<&str> = frame.lines().map(|l| l.trim_matches(|c: char| c == '│' || c == ' ')).collect();
        assert!(rows.contains(&"> wait, use pnpm"), "{frame}");
        assert!(rows.contains(&"and the lint"), "{frame}");
        assert!(rows.iter().any(|r| r.starts_with("[Pasted text #1, 2 lines]")), "{frame}");
    }

    #[test]
    fn a_multi_line_paste_is_a_chip_and_enter_sends_the_whole_text() {
        let mut h = Harness::new(&[]);
        h.typed("look at ");
        h.prompt.paste("line 1\r\nline 2\rline 3\nline 4\n");
        assert!(h.submitted.is_empty(), "a paste never submits");
        assert_eq!(h.prompt.value.chars().count(), "look at ".len() + 1, "the chip is one char");
        assert!(h.frame().contains("> look at [Pasted text #1, 4 lines]"), "{}", h.frame());
        assert_eq!(h.prompt.text(), "look at line 1\nline 2\nline 3\nline 4\n");
        h.typed(" please");
        h.press(&[KeyCode::Enter]);
        assert_eq!(h.submitted, ["look at line 1\nline 2\nline 3\nline 4\n please"]);
    }

    #[test]
    fn a_long_line_is_a_chip_and_a_short_one_isnt() {
        let mut h = Harness::new(&[]);
        let long = "y".repeat(PASTE_CHIP_CHARS + 1);
        h.prompt.paste(&long);
        assert!(h.frame().contains(&format!("[Pasted text #1, {} chars]", PASTE_CHIP_CHARS + 1)), "{}", h.frame());
        assert_eq!(h.prompt.text(), long);
        h.prompt.set("");
        h.prompt.paste("http://127.0.0.1:5555/callback?code=c&state=s");
        assert_eq!(h.prompt.value, "http://127.0.0.1:5555/callback?code=c&state=s");
    }

    #[test]
    fn the_cursor_backspace_and_delete_treat_a_chip_as_one_unit() {
        let mut h = Harness::new(&[]);
        h.typed("a");
        h.prompt.paste("one\ntwo");
        h.typed("b");
        h.press(&[KeyCode::Left, KeyCode::Left]); // over b, then over the whole chip
        h.typed("<");
        assert_eq!(h.prompt.text(), "a<one\ntwob");
        h.press(&[KeyCode::Delete]); // the chip after the cursor, at once
        assert_eq!(h.prompt.text(), "a<b");
        h.prompt.paste("three\nfour");
        assert_eq!(h.prompt.text(), "a<three\nfourb");
        h.press(&[KeyCode::Backspace]); // the chip before the cursor
        assert_eq!(h.prompt.text(), "a<b");
        assert!(!h.frame().contains("Pasted"), "{}", h.frame());
    }

    #[test]
    fn several_chips_are_numbered_and_each_expands_to_its_own_text() {
        let mut h = Harness::new(&[]);
        h.prompt.paste("a\nb");
        h.typed(" and ");
        h.prompt.paste("c\nd\ne");
        let frame = h.frame();
        assert!(frame.contains("> [Pasted text #1, 2 lines] and"), "{frame}");
        assert!(frame.contains("│   [Pasted text #2, 3 lines]"), "a chip isn't split across rows: {frame}");
        h.press(&[KeyCode::Enter]);
        assert_eq!(h.submitted, ["a\nb and c\nd\ne"]);
        // An emptied line starts counting again.
        h.prompt.paste("f\ng");
        assert!(h.frame().contains("[Pasted text #1, 2 lines]"), "{}", h.frame());
    }

    #[test]
    fn up_brings_a_submission_back_with_its_chips() {
        let mut h = Harness::new(&[]);
        h.typed("see ");
        h.prompt.paste("x\ny");
        let Some(PromptOutcome::Submit(text)) = h.prompt.on_key(KeyEvent::from(KeyCode::Enter), false, &h.commands) else {
            panic!("no submit")
        };
        // As the App does: clear, then remember the trimmed text.
        h.prompt.set("");
        h.prompt.remember(text.trim());
        h.prompt.remember("plain");
        h.press(&[KeyCode::Up, KeyCode::Up]);
        assert!(h.frame().contains("> see [Pasted text #1, 2 lines]"), "{}", h.frame());
        assert_eq!(h.prompt.text(), "see x\ny");
        h.press(&[KeyCode::Down]);
        assert_eq!(h.prompt.text(), "plain");
        // Multi-line text with no chips recorded (set from outside) is one chip, not raw newlines in the box.
        h.prompt.set("p\nq");
        assert!(h.frame().contains("> [Pasted text #1, 2 lines]"), "{}", h.frame());
        assert_eq!(h.prompt.text(), "p\nq");
    }

    #[test]
    fn typed_or_pasted_chip_placeholders_are_not_chips() {
        let mut h = Harness::new(&[]);
        let fake = char::from_u32(CHIP_BASE).unwrap();
        h.prompt.paste(&format!("a{fake}"));
        h.key(KeyCode::Char(fake), KeyModifiers::NONE);
        assert_eq!(h.prompt.text(), "a\u{FFFD}\u{FFFD}");
        // Multi-line text set from outside too.
        h.prompt.set(format!("x{fake}\ny"));
        assert_eq!(h.prompt.text(), "x\u{FFFD}\ny");
    }

    #[test]
    fn recalled_chips_are_kept_once_per_history_entry() {
        let mut h = Harness::new(&[]);
        for _ in 0..3 {
            for text in ["a\nb", "c\nd"] {
                h.prompt.paste(text);
                h.press(&[KeyCode::Enter]);
                let last = h.submitted.last().unwrap().clone();
                h.prompt.remember(last.trim());
            }
        }
        assert_eq!(h.prompt.recalls.len(), 2);
        assert!(h.prompt.recalls.len() <= h.prompt.history.len());
    }
}
