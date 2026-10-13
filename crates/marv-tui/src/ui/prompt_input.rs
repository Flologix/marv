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
//! - Editing is ink-text-input's: ←/→ move the cursor (also with ctrl held), Backspace deletes
//!   before it; ctrl/alt + a letter types nothing (ctrl+o, ctrl+a). Home/End and Delete are extras.
//!
//! What changes in Rust: the component owns its value (Marv's was controlled by the App); the App
//! clears it after accepting a submission, like Marv's `handleSubmit` did.

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
    /// Replaces the text from outside (history, Tab, the App clearing it): the cursor goes to the end.
    pub fn set(&mut self, value: impl Into<String>) {
        self.value = value.into();
        self.cursor = self.value.chars().count();
        self.menu_index = 0;
    }

    /// Adds a submission to the history (not twice in a row), as the App does on submit.
    pub fn remember(&mut self, text: &str) {
        if self.history.last().map(String::as_str) != Some(text) {
            self.history.push(text.to_string());
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
        match key.code {
            KeyCode::Enter => {
                self.history_index = None;
                self.dismissed_at = None;
                // With the menu open, Enter runs the highlighted command ("/se" → "/setup").
                let text = highlighted.map_or_else(|| self.value.clone(), |c| format!("/{}", c.name));
                return Some(PromptOutcome::Submit(text));
            }
            KeyCode::Up if !self.history.is_empty() => {
                let next = self.history_index.map_or(self.history.len() - 1, |i| i.saturating_sub(1));
                self.history_index = Some(next);
                self.set(self.history[next].clone());
            }
            KeyCode::Down => {
                if let Some(i) = self.history_index {
                    if i + 1 >= self.history.len() {
                        self.history_index = None;
                        self.set("");
                    } else {
                        self.history_index = Some(i + 1);
                        self.set(self.history[i + 1].clone());
                    }
                }
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.value.chars().count()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.value.chars().count(),
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
            KeyCode::Char(c) if !ctrl_or_alt => self.type_text(&c.to_string()),
            _ => {}
        }
        None
    }

    /// Pasted text (bracketed paste): typed in one go, newlines kept as spaces so a paste can't submit.
    pub fn paste(&mut self, text: &str) {
        let text: String = text.chars().map(|c| if c == '\n' || c == '\r' { ' ' } else { c }).collect();
        self.type_text(&text);
    }

    /// The text rows inside the box: `> ` then the text, wrapped by cell with a hanging indent,
    /// the cursor in inverse video.
    fn text_rows(&self, inner_width: u16, busy: bool) -> Vec<Line<'static>> {
        let accent = Style::new().fg(theme::ACCENT);
        let cursor_style = Style::new().add_modifier(Modifier::REVERSED);
        if self.value.is_empty() {
            let placeholder = if busy { "" } else { PLACEHOLDER };
            let mut chars = placeholder.chars();
            let first = chars.next().map_or_else(|| " ".to_string(), |c| c.to_string());
            let line = Line::from(vec![
                Span::styled("> ", accent),
                Span::styled(first, cursor_style.fg(theme::DIM)),
                Span::styled(chars.collect::<String>(), Style::new().fg(theme::DIM)),
            ]);
            return vec![truncate_end(line, inner_width)];
        }
        let width = inner_width.saturating_sub(2).max(1) as usize;
        let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
        let mut used = 0;
        let clean: Vec<String> = self.value.chars().map(|c| printable(&c.to_string())).collect();
        for i in 0..=clean.len() {
            let (text, w) = match clean.get(i) {
                Some(t) => (t.clone(), t.width()),
                None => (" ".to_string(), 1), // the cursor past the end
            };
            if w == 0 && i != self.cursor {
                continue;
            }
            let (text, w) = if w == 0 { (" ".to_string(), 1) } else { (text, w) };
            if used + w > width {
                rows.push(Vec::new());
                used = 0;
            }
            let style = if i == self.cursor { cursor_style } else { Style::new() };
            rows.last_mut().expect("never empty").push(Span::styled(text, style));
            used += w;
        }
        rows.into_iter()
            .enumerate()
            .map(|(i, spans)| {
                let gutter = if i == 0 { Span::styled("> ", accent) } else { Span::raw("  ") };
                Line::from([vec![gutter], spans].concat())
            })
            .collect()
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
        let rows = self.text_rows(area.width.saturating_sub(4), busy);
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
        .map(|(n, d)| CommandInfo { name: n.into(), description: d.into() })
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
            c.push(CommandInfo { name: n.into(), description: String::new() });
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

    #[test]
    fn ctrl_arrows_arent_typed_and_still_move_the_cursor() {
        let mut h = Harness::new(&[]);
        h.typed("ab");
        h.key(KeyCode::Left, KeyModifiers::CONTROL);
        h.typed("x");
        h.key(KeyCode::Right, KeyModifiers::CONTROL);
        h.typed("y");
        h.press(&[KeyCode::Enter]);
        assert_eq!(h.submitted, ["axby"]);
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
    fn pasted_escapes_are_not_drawn() {
        let mut h = Harness::new(&[]);
        h.prompt.paste("hi\x1b[2Jthere\nnext");
        assert_eq!(h.prompt.value, "hi\x1b[2Jthere next");
        assert!(h.frame().contains("> hi[2Jthere next"));
    }
}
