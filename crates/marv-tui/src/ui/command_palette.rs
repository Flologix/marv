//! The ctrl+k command palette (#44). New in marv-rs: Marv's issue #44 planned it on top of
//! `PromptInput`'s `/` menu; this is that plan for the ratatui build.
//!
//! What it adds over the `/` menu:
//! - it opens at any time, over a half-typed prompt (the prompt keeps its text: Esc goes back to it)
//!   and while a reply streams, for the entries that don't need an idle session (the others are
//!   dimmed and say "after the reply");
//! - settings with their current values ([`SettingInfo`]): a toggle flips with Enter and a value
//!   opens its picker or prefills its command, both through the command line the engine gave, so a
//!   change is saved and announced exactly as the slash command does it;
//! - the keyboard shortcuts ([`SHORTCUTS`]), listed and not run;
//! - one fuzzy search ([`fuzzy_score`]) over names and descriptions, grouped by kind ([`search`]).
//!
//! Its entries ([`entries`]) come from the same [`CommandInfo`] list as the `/` menu, so a new
//! command or skill shows up in both. Argument completions ("mcp login github") are offered only
//! once something is typed, as the `/` menu offers them only after their command.
//!
//! The App never opens it over an approval (the approval keeps the keyboard) and closes it when one
//! arrives, the same way an approval takes over from the prompt.

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
use crate::view::{CommandInfo, CommandKind, SettingAction, SettingInfo};

/// The most list rows (headings included) shown at once; the list scrolls with the selection.
pub const PALETTE_ROWS: usize = 14;
/// The label column is never wider than this; longer labels are cut.
const LABEL_MAX: usize = 28;
/// Said on an entry that needs an idle session while a reply streams.
pub const AFTER_REPLY: &str = "after the reply";

/// The keys, listed in the palette (not run). Keep in step with README's Keys table and `/help`.
pub const SHORTCUTS: &[(&str, &str)] = &[
    ("ctrl+k", "This palette: commands, skills, settings and keys"),
    ("Esc", "Stop a reply or a running tool (in a subagent's view: go back)"),
    ("Enter while Marv works", "Queue your message: sent as the next turn when this one finishes"),
    ("alt+enter or ctrl+s", "Send now: while Marv works, your message goes in at its next step"),
    ("↑ / ctrl+x", "With messages queued and an empty prompt: edit the newest / drop it"),
    ("ctrl+c", "Stop / clear the input / press twice to quit"),
    ("ctrl+o", "Show or hide details: what subagents did, and whole diffs"),
    ("PgUp/PgDn", "Scroll the conversation (the mouse wheel too)"),
    ("↑/↓", "Previous inputs"),
    ("/", "The command menu: Enter runs, Tab or → completes"),
    ("Drag", "Select text; it's copied when you let go"),
    ("Click a subagent", "Open its own transcript, live"),
];

/// The palette's groups, in the order they're listed when nothing is typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EntryKind {
    Command,
    Skill,
    Setting,
    Shortcut,
}

impl EntryKind {
    pub fn heading(self) -> &'static str {
        match self {
            Self::Command => "Commands",
            Self::Skill => "Skills",
            Self::Setting => "Settings",
            Self::Shortcut => "Shortcuts",
        }
    }
}

/// What Enter on an entry does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryAction {
    /// Runs this command line ("/cost", "/yolo off", "/model").
    Run(String),
    /// Puts this in the prompt to finish ("/steps ").
    Prefill(String),
    /// A shortcut: listed, not run.
    Show,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub kind: EntryKind,
    /// As shown: "/cost", "Yolo: on", "ctrl+o".
    pub label: String,
    /// What the search matches besides the description: the label without its slash.
    pub name: String,
    pub description: String,
    pub action: EntryAction,
    /// Can be chosen while a reply streams.
    pub while_busy: bool,
    /// Only offered once something is typed (argument completions).
    pub on_search: bool,
}

impl Entry {
    /// Tab: the command (plus a space) into the prompt, to add arguments. Commands and skills only.
    fn completion(&self) -> Option<String> {
        matches!(self.kind, EntryKind::Command | EntryKind::Skill).then(|| format!("{} ", self.label))
    }
}

/// Every entry: the `/` menu's commands and skills (the same list), then the settings, then the keys.
pub fn entries(commands: &[CommandInfo], settings: &[SettingInfo]) -> Vec<Entry> {
    let commands = commands.iter().map(|c| Entry {
        kind: match c.kind {
            CommandKind::Command => EntryKind::Command,
            CommandKind::Skill => EntryKind::Skill,
        },
        label: format!("/{}", c.name),
        name: c.name.clone(),
        description: c.description.clone(),
        action: EntryAction::Run(format!("/{}", c.name)),
        while_busy: c.while_busy,
        on_search: c.name.contains(' '),
    });
    let settings = settings.iter().map(|s| Entry {
        kind: EntryKind::Setting,
        label: format!("{}: {}", s.name, s.value),
        name: format!("{} {}", s.name, s.value),
        description: s.description.clone(),
        action: match &s.action {
            SettingAction::Run(line) => EntryAction::Run(line.clone()),
            SettingAction::Prefill(text) => EntryAction::Prefill(text.clone()),
        },
        while_busy: s.while_busy,
        on_search: false,
    });
    let shortcuts = SHORTCUTS.iter().map(|(key, what)| Entry {
        kind: EntryKind::Shortcut,
        label: (*key).to_string(),
        name: (*key).to_string(),
        description: (*what).to_string(),
        action: EntryAction::Show,
        while_busy: true,
        on_search: false,
    });
    commands.chain(settings).chain(shortcuts).collect()
}

/// `needle` starts a word of `hay` (at its start, or after something that isn't a letter or digit).
fn starts_a_word(hay: &str, needle: &str) -> bool {
    hay.match_indices(needle).any(|(i, _)| hay[..i].chars().next_back().is_none_or(|c| !c.is_alphanumeric()))
}

/// How loosely `needle`'s chars appear in order in `hay`: the chars skipped between the first and
/// the last, or None when they don't.
fn subsequence_gap(hay: &str, needle: &str) -> Option<usize> {
    let mut wanted = needle.chars().peekable();
    let (mut start, mut gap) = (None, 0);
    for (i, c) in hay.chars().enumerate() {
        match wanted.peek() {
            None => break,
            Some(&w) if w == c => {
                start.get_or_insert(i);
                wanted.next();
            }
            Some(_) if start.is_some() => gap += 1,
            Some(_) => {}
        }
    }
    wanted.peek().is_none().then_some(gap)
}

/// How well `query` matches an entry, higher is better; None when it doesn't. Every word of the
/// query must match the name or the description: whole name, then the name's start, a word of the
/// name, anywhere in the name, a word of the description, anywhere in the description, and last
/// the name's letters in order ("thk" → think). A leading `/` is ignored ("/co" → /cost).
pub fn fuzzy_score(query: &str, name: &str, description: &str) -> Option<u32> {
    let name = name.to_lowercase();
    let description = description.to_lowercase();
    let mut total = 0;
    for word in query.to_lowercase().split_whitespace() {
        let word = word.trim_start_matches('/');
        if word.is_empty() {
            continue;
        }
        total += if name == word {
            120
        } else if name.starts_with(word) {
            100
        } else if starts_a_word(&name, word) {
            80
        } else if name.contains(word) {
            60
        } else if starts_a_word(&description, word) {
            40
        } else if description.contains(word) {
            30
        } else {
            // Letters in order, the last resort: only for more than one letter.
            let gap = (word.chars().count() > 1).then(|| subsequence_gap(&name, word)).flatten()?;
            20u32.saturating_sub(gap as u32).max(1)
        };
    }
    Some(total)
}

/// The entries matching `query`, grouped by kind: each group's entries best first, the groups by
/// their best entry (ties in [`EntryKind`] order). With nothing typed: everything but argument
/// completions, in the given order and the kinds' order. Indices into `entries`.
pub fn search(entries: &[Entry], query: &str) -> Vec<(EntryKind, Vec<usize>)> {
    let typed = !query.trim().trim_start_matches('/').is_empty();
    let mut scored: Vec<(usize, u32)> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| typed || !e.on_search)
        .filter_map(|(i, e)| fuzzy_score(query, &e.name, &e.description).map(|s| (i, s)))
        .collect();
    // Stable: equal scores keep the list's order.
    scored.sort_by_key(|&(_, score)| std::cmp::Reverse(score));
    let mut groups: Vec<(EntryKind, u32, Vec<usize>)> = Vec::new();
    for (i, score) in scored {
        let kind = entries[i].kind;
        match groups.iter_mut().find(|g| g.0 == kind) {
            Some(group) => group.2.push(i),
            None => groups.push((kind, score, vec![i])),
        }
    }
    groups.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    groups.into_iter().map(|(kind, _, items)| (kind, items)).collect()
}

/// What the palette asks the App to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteOutcome {
    /// Back to the prompt as it was.
    Close,
    /// Close, then run this command line.
    Run(String),
    /// Close, and put this in the prompt.
    Prefill(String),
}

/// One row of the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Heading(EntryKind),
    Item(usize),
}

#[derive(Debug, Clone)]
pub struct CommandPalette {
    pub query: String,
    entries: Vec<Entry>,
    /// The highlighted match, counted over the matches in list order.
    selected: usize,
    /// A word about the last Enter that couldn't run (a shortcut, or busy), shown under the list.
    note: Option<String>,
}

impl CommandPalette {
    pub fn new(entries: Vec<Entry>) -> Self {
        Self { query: String::new(), entries, selected: 0, note: None }
    }

    fn groups(&self) -> Vec<(EntryKind, Vec<usize>)> {
        search(&self.entries, &self.query)
    }

    fn rows(&self) -> Vec<Row> {
        self.groups()
            .into_iter()
            .flat_map(|(kind, items)| std::iter::once(Row::Heading(kind)).chain(items.into_iter().map(Row::Item)))
            .collect()
    }

    /// The matches in list order (what ↑/↓ walk).
    fn matches(&self) -> Vec<usize> {
        self.groups().into_iter().flat_map(|(_, items)| items).collect()
    }

    /// The highlighted entry's index, if anything matches.
    fn highlighted_index(&self) -> Option<usize> {
        let matches = self.matches();
        (!matches.is_empty()).then(|| matches[self.selected.min(matches.len() - 1)])
    }

    /// The highlighted entry, if anything matches.
    pub fn highlighted(&self) -> Option<&Entry> {
        self.highlighted_index().map(|i| &self.entries[i])
    }

    fn typed(&mut self) {
        self.selected = 0;
        self.note = None;
    }

    /// Pasted text joins the search, on one line.
    pub fn paste(&mut self, text: &str) {
        self.query.extend(text.chars().map(|c| if c == '\n' || c == '\r' { ' ' } else { c }));
        self.typed();
    }

    pub fn on_key(&mut self, key: KeyEvent, busy: bool) -> Option<PaletteOutcome> {
        let ctrl_or_alt = key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc => return Some(PaletteOutcome::Close),
            // ctrl+k again closes it, like Esc.
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => return Some(PaletteOutcome::Close),
            KeyCode::Up | KeyCode::Down => {
                let n = self.matches().len();
                if n > 0 {
                    let at = self.selected.min(n - 1);
                    self.selected = if key.code == KeyCode::Up { (at + n - 1) % n } else { (at + 1) % n };
                }
                self.note = None;
            }
            KeyCode::Enter => {
                let entry = self.highlighted()?;
                if busy && !entry.while_busy {
                    self.note = Some(format!("{} can run {AFTER_REPLY} (Esc, then Esc again stops it).", entry.label));
                    return None;
                }
                return match &entry.action {
                    EntryAction::Run(line) => Some(PaletteOutcome::Run(line.clone())),
                    EntryAction::Prefill(text) => Some(PaletteOutcome::Prefill(text.clone())),
                    EntryAction::Show => {
                        self.note = Some(format!("Shortcuts are listed, not run: close this and press {}.", entry.label));
                        None
                    }
                };
            }
            // Tab: the command into the prompt, to add arguments (as Tab does in the `/` menu).
            KeyCode::Tab => return self.highlighted().and_then(Entry::completion).map(PaletteOutcome::Prefill),
            KeyCode::Backspace => {
                self.query.pop();
                self.typed();
            }
            KeyCode::Char(c) if !ctrl_or_alt => {
                self.query.push(c);
                self.typed();
            }
            _ => {}
        }
        None
    }

    /// The list rows on screen: at most `max` (and [`PALETTE_ROWS`]), scrolled to the highlight.
    fn window(&self, max: usize) -> Vec<Row> {
        let rows = self.rows();
        let max = max.clamp(1, PALETTE_ROWS);
        let at = self.highlighted_index().and_then(|i| rows.iter().position(|r| *r == Row::Item(i))).unwrap_or(0);
        let top = (at + 1).saturating_sub(max);
        rows[top..(top + max).min(rows.len())].to_vec()
    }

    /// The box, the list (at most `max_rows` in all), then the hint.
    pub fn height(&self, max_rows: u16) -> u16 {
        let room = (max_rows as usize).saturating_sub(4).max(1);
        let rows = self.window(room);
        (3 + rows.len().max(1) + 1) as u16
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer, busy: bool) {
        let accent = Style::new().fg(theme::ACCENT);
        let dim = Style::new().fg(theme::DIM);
        // The search box: `❯ query▌`.
        let box_area = Rect { height: 3.min(area.height), ..area };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(accent)
            .title(Span::styled(" ctrl+k ", dim))
            .padding(Padding::horizontal(1));
        let inner = block.inner(box_area);
        block.render(box_area, buf);
        let query = if self.query.is_empty() {
            vec![
                Span::styled("S", Style::new().add_modifier(Modifier::REVERSED).fg(theme::DIM)),
                Span::styled("earch commands, skills, settings and keys", dim),
            ]
        } else {
            vec![Span::raw(printable(&self.query)), Span::styled(" ", Style::new().add_modifier(Modifier::REVERSED))]
        };
        let line = Line::from([vec![Span::styled("❯ ", accent)], query].concat());
        buf.set_line(inner.x, inner.y, &truncate_end(line, inner.width), inner.width);

        // The list, paddingX 2, then the hint or the note.
        let room = (area.height as usize).saturating_sub(4).max(1);
        let rows = self.window(room);
        let highlighted = self.highlighted_index();
        let label_width =
            rows.iter().filter_map(|r| if let Row::Item(i) = r { Some(self.entries[*i].label.width()) } else { None }).max();
        let label_width = label_width.unwrap_or(0).min(LABEL_MAX) + 2;
        let width = area.width.saturating_sub(4);
        let mut lines: Vec<Line<'static>> = rows
            .iter()
            .map(|row| match row {
                Row::Heading(kind) => Line::styled(kind.heading(), dim.add_modifier(Modifier::BOLD)),
                Row::Item(i) => {
                    let e = &self.entries[*i];
                    let active = highlighted == Some(*i);
                    let blocked = busy && !e.while_busy;
                    let style = match (active, blocked) {
                        (_, true) => dim,
                        (true, false) => accent,
                        (false, false) => Style::new(),
                    };
                    let label = truncate_end(Line::raw(printable(&e.label)), LABEL_MAX as u16).to_string();
                    let pad = label_width.saturating_sub(label.width());
                    let mut spans = vec![
                        Span::styled(if active { "❯ " } else { "  " }, if active { accent } else { style }),
                        Span::styled(format!("{label}{}", " ".repeat(pad)), style),
                    ];
                    if blocked {
                        spans.push(Span::styled(format!("({AFTER_REPLY}) "), dim.add_modifier(Modifier::ITALIC)));
                    }
                    spans.push(Span::styled(printable(&e.description), dim));
                    truncate_end(Line::from(spans), width)
                }
            })
            .collect();
        if lines.is_empty() {
            lines.push(Line::styled("Nothing matches.", dim));
        }
        // A note names the entry, and a skill's name comes from its SKILL.md: printable() it like the list.
        let hint =
            self.note.as_deref().map(printable).unwrap_or_else(|| "↑/↓ select · Enter choose · Tab complete · Esc back".into());
        lines.push(truncate_end(Line::styled(hint, dim), width));
        for (i, line) in lines.iter().enumerate() {
            let y = box_area.bottom() + i as u16;
            if y >= area.bottom() {
                break;
            }
            buf.set_line(area.x + 2, y, line, width);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(name: &str, description: &str, while_busy: bool) -> CommandInfo {
        CommandInfo { while_busy, ..CommandInfo::new(name, description) }
    }

    fn commands() -> Vec<CommandInfo> {
        vec![
            command("help", "Show available commands", true),
            command("cost", "Show tokens used and cost so far", true),
            command("undo", "Put back the files the last turn changed (shows them first)", false),
            command("think", "Toggle model thinking (/think on, /think off)", true),
            command("mcp", "Show MCP servers", false),
            command("mcp login github", "Log in to github (OAuth, in your browser)", false),
            CommandInfo { kind: CommandKind::Skill, ..CommandInfo::new("deploy", "Ship the app to staging") },
        ]
    }

    fn settings() -> Vec<SettingInfo> {
        vec![
            SettingInfo {
                name: "Yolo".into(),
                value: "on".into(),
                description: "Run what the sandbox confines without asking".into(),
                action: SettingAction::Run("/yolo off".into()),
                while_busy: true,
            },
            SettingInfo {
                name: "Model".into(),
                value: "z-ai/glm-5.3-flash".into(),
                description: "Switch model".into(),
                action: SettingAction::Run("/model".into()),
                while_busy: false,
            },
            SettingInfo {
                name: "Step limit".into(),
                value: "25".into(),
                description: "Show or set the step limit".into(),
                action: SettingAction::Prefill("/steps ".into()),
                while_busy: false,
            },
        ]
    }

    fn palette() -> CommandPalette {
        CommandPalette::new(entries(&commands(), &settings()))
    }

    fn labels(p: &CommandPalette) -> Vec<(EntryKind, Vec<String>)> {
        p.groups().into_iter().map(|(k, items)| (k, items.into_iter().map(|i| p.entries[i].label.clone()).collect())).collect()
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_in(p: &mut CommandPalette, text: &str) {
        for c in text.chars() {
            p.on_key(key(KeyCode::Char(c)), false);
        }
    }

    fn frame(p: &CommandPalette, busy: bool) -> String {
        let width = 80;
        let area = Rect::new(0, 0, width, p.height(30));
        let mut buf = Buffer::empty(area);
        p.render(area, &mut buf, busy);
        (0..area.height)
            .map(|y| {
                let mut row = String::new();
                let mut x = 0;
                while x < width {
                    let s = buf[(x, y)].symbol();
                    row.push_str(s);
                    x += (s.width() as u16).max(1);
                }
                row.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    // A skill's name comes from its SKILL.md: the busy note that names it is printable() like the list.
    #[test]
    fn the_busy_note_naming_a_skill_draws_no_terminal_escapes() {
        let skill = CommandInfo { kind: CommandKind::Skill, ..CommandInfo::new("evil\x1b]0;pwned\x07", "a skill") };
        let mut p = CommandPalette::new(entries(&[skill], &[]));
        type_in(&mut p, "evil");
        assert_eq!(p.on_key(key(KeyCode::Enter), true), None);
        let drawn = frame(&p, true);
        assert!(drawn.contains("can run"), "{drawn}");
        assert!(!drawn.contains('\x1b') && !drawn.contains('\x07'), "{drawn:?}");
    }

    #[test]
    fn the_entries_are_the_menus_commands_and_skills_then_settings_with_their_values_then_the_keys() {
        let e = entries(&commands(), &settings());
        let kinds: Vec<_> = e.iter().map(|e| (e.kind, e.label.as_str())).collect();
        assert_eq!(&kinds[..2], [(EntryKind::Command, "/help"), (EntryKind::Command, "/cost")]);
        assert!(kinds.contains(&(EntryKind::Skill, "/deploy")));
        assert!(kinds.contains(&(EntryKind::Setting, "Yolo: on")));
        assert!(kinds.contains(&(EntryKind::Setting, "Model: z-ai/glm-5.3-flash")));
        assert_eq!(e.iter().filter(|e| e.kind == EntryKind::Shortcut).count(), SHORTCUTS.len());
        // A command runs its line; a toggle runs the engine's flip; a value prefills; a key is only shown.
        let action = |label: &str| e.iter().find(|e| e.label == label).unwrap().action.clone();
        assert_eq!(action("/cost"), EntryAction::Run("/cost".into()));
        assert_eq!(action("Yolo: on"), EntryAction::Run("/yolo off".into()));
        assert_eq!(action("Step limit: 25"), EntryAction::Prefill("/steps ".into()));
        assert_eq!(action("ctrl+o"), EntryAction::Show);
        assert!(SHORTCUTS.iter().any(|(k, _)| *k == "ctrl+k"));
    }

    #[test]
    fn a_new_command_shows_up_in_the_palette_as_in_the_slash_menu() {
        let mut list = commands();
        list.push(CommandInfo::new("brand-new", "Something new"));
        assert!(crate::ui::prompt_input::match_commands("/brand", &list).iter().any(|c| c.name == "brand-new"));
        let p = CommandPalette::new(entries(&list, &[]));
        assert!(p.entries.iter().any(|e| e.label == "/brand-new"));
    }

    #[test]
    fn fuzzy_score_prefers_the_name_then_the_description_and_needs_every_word() {
        let s = |q| fuzzy_score(q, "cost", "Show tokens used and cost so far");
        assert_eq!(s("cost"), Some(120));
        assert_eq!(s("/co"), Some(100));
        assert!(s("tokens").unwrap() < s("co").unwrap());
        assert!(s("tokens used").is_some());
        assert_eq!(s("tokens nope"), None);
        // Letters in order, as a last resort, and only for more than one letter.
        assert!(fuzzy_score("thk", "think", "").is_some());
        assert_eq!(fuzzy_score("x", "think", ""), None);
        // A word's start beats the middle of one.
        assert!(fuzzy_score("limit", "step limit", "").unwrap() > fuzzy_score("imit", "step limit", "").unwrap());
        // Nothing typed matches everything.
        assert_eq!(fuzzy_score("  ", "anything", ""), Some(0));
    }

    #[test]
    fn nothing_typed_lists_every_group_in_order_without_argument_completions() {
        let p = palette();
        let groups = labels(&p);
        let kinds: Vec<_> = groups.iter().map(|g| g.0).collect();
        assert_eq!(kinds, [EntryKind::Command, EntryKind::Skill, EntryKind::Setting, EntryKind::Shortcut]);
        assert_eq!(groups[0].1, ["/help", "/cost", "/undo", "/think", "/mcp"]);
    }

    #[test]
    fn a_search_finds_by_description_and_groups_by_kind_best_group_first() {
        let mut p = palette();
        type_in(&mut p, "cost");
        assert_eq!(labels(&p)[0], (EntryKind::Command, vec!["/cost".to_string()]));
        // By description, across kinds: "github" finds the login completion now that something's typed.
        let mut p = palette();
        type_in(&mut p, "github");
        assert_eq!(labels(&p), [(EntryKind::Command, vec!["/mcp login github".to_string()])]);
        // A setting by its value.
        let mut p = palette();
        type_in(&mut p, "glm");
        assert_eq!(p.highlighted().unwrap().label, "Model: z-ai/glm-5.3-flash");
        // "staging" is only in the skill's description.
        let mut p = palette();
        type_in(&mut p, "staging");
        assert_eq!(labels(&p), [(EntryKind::Skill, vec!["/deploy".to_string()])]);
    }

    #[test]
    fn arrows_walk_the_matches_across_groups_and_wrap() {
        let mut p = palette();
        type_in(&mut p, "show");
        let order: Vec<String> = p.matches().iter().map(|&i| p.entries[i].label.clone()).collect();
        assert!(order.len() >= 3, "{order:?}");
        p.on_key(key(KeyCode::Down), false);
        assert_eq!(p.highlighted().unwrap().label, order[1]);
        p.on_key(key(KeyCode::Up), false);
        p.on_key(key(KeyCode::Up), false);
        assert_eq!(p.highlighted().unwrap().label, *order.last().unwrap());
        // Typing starts again from the best match.
        p.on_key(key(KeyCode::Backspace), false);
        assert_eq!(p.highlighted().unwrap().label, p.entries[p.matches()[0]].label);
    }

    #[test]
    fn enter_runs_a_command_flips_a_toggle_and_prefills_a_value() {
        let mut p = palette();
        type_in(&mut p, "cost");
        assert_eq!(p.on_key(key(KeyCode::Enter), false), Some(PaletteOutcome::Run("/cost".into())));
        let mut p = palette();
        type_in(&mut p, "yolo");
        assert_eq!(p.on_key(key(KeyCode::Enter), false), Some(PaletteOutcome::Run("/yolo off".into())));
        let mut p = palette();
        type_in(&mut p, "step limit");
        assert_eq!(p.on_key(key(KeyCode::Enter), false), Some(PaletteOutcome::Prefill("/steps ".into())));
        // Tab completes a command for its arguments.
        let mut p = palette();
        type_in(&mut p, "think");
        assert_eq!(p.on_key(key(KeyCode::Tab), false), Some(PaletteOutcome::Prefill("/think ".into())));
    }

    #[test]
    fn shortcuts_are_listed_not_run() {
        let mut p = palette();
        type_in(&mut p, "ctrl+o");
        assert_eq!(p.highlighted().unwrap().kind, EntryKind::Shortcut);
        assert_eq!(p.on_key(key(KeyCode::Enter), false), None);
        assert!(frame(&p, false).contains("listed, not run"), "{}", frame(&p, false));
    }

    #[test]
    fn while_busy_only_what_doesnt_need_an_idle_session_runs_and_the_rest_says_why() {
        let mut p = palette();
        type_in(&mut p, "undo");
        assert_eq!(p.on_key(key(KeyCode::Enter), true), None);
        let shown = frame(&p, true);
        assert!(shown.contains("(after the reply)"), "{shown}");
        assert!(shown.contains("/undo can run after the reply"), "{shown}");
        let mut p = palette();
        type_in(&mut p, "yolo");
        assert_eq!(p.on_key(key(KeyCode::Enter), true), Some(PaletteOutcome::Run("/yolo off".into())));
    }

    #[test]
    fn esc_and_ctrl_k_close_it() {
        let mut p = palette();
        assert_eq!(p.on_key(key(KeyCode::Esc), false), Some(PaletteOutcome::Close));
        assert_eq!(p.on_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL), false), Some(PaletteOutcome::Close));
        // Other ctrl+letters type nothing.
        p.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL), false);
        assert_eq!(p.query, "");
    }

    #[test]
    fn it_draws_the_search_grouped_rows_and_the_keys() {
        let mut p = palette();
        let shown = frame(&p, false);
        assert!(shown.contains("❯ Search commands, skills, settings and keys"), "{shown}");
        assert!(shown.contains("Commands"), "{shown}");
        assert!(shown.contains("❯ /help"), "{shown}");
        assert!(shown.contains("↑/↓ select · Enter choose · Tab complete · Esc back"), "{shown}");
        type_in(&mut p, "yolo");
        let shown = frame(&p, false);
        assert!(shown.contains("❯ yolo"), "{shown}");
        assert!(shown.contains("Settings"), "{shown}");
        assert!(shown.contains("❯ Yolo: on"), "{shown}");
        type_in(&mut p, "zzzz");
        assert!(frame(&p, false).contains("Nothing matches."));
    }

    #[test]
    fn a_long_list_scrolls_to_keep_the_highlight_in_view() {
        let many: Vec<CommandInfo> = (0..40).map(|i| CommandInfo::new(format!("c{i:02}"), "x")).collect();
        let mut p = CommandPalette::new(entries(&many, &[]));
        assert!(p.height(100) as usize <= PALETTE_ROWS + 4);
        for _ in 0..30 {
            p.on_key(key(KeyCode::Down), false);
        }
        let shown = frame(&p, false);
        assert!(shown.contains("❯ /c30"), "{shown}");
        assert!(!shown.contains("/c00"), "{shown}");
    }

    #[test]
    fn pasted_text_and_escapes_are_searched_but_not_drawn_raw() {
        let mut p = palette();
        p.paste("co\x1b[2J\nst");
        assert_eq!(p.query, "co\x1b[2J st");
        assert!(!frame(&p, false).contains('\x1b'));
    }
}
