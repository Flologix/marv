//! Asks before a tool changes something. Ports `src/ui/Approval.tsx` (tests: `tests/approval.test.tsx`).
//!
//! Shown in place of the prompt while the agent waits: a round border (warning colour when the
//! preview has a warning), `[agent]` when a subagent asks, the bold title, then the payload in
//! full (docs/SECURITY.md: approvals show the full payload): `$ command`, text, and the diff (at
//! most [`MAX_DIFF_LINES`]), a note, the door's `⚠` line ([`door_warning`]), `⚠ warning`, "N more waiting" (parallel subagents queue
//! behind the head), and the choice: Yes / Yes, and don't ask again for <scope> this session /
//! No, and tell Marv what to do instead (esc). `once` requests offer only Yes / No.
//!
//! marv-rs (#185): a model's tool call (`request.reason`, set only by marv-tools' `run_tool`; not the step-limit
//! question, a mesh wire, /undo or /mcp add) also offers "No, and say why (Marv carries on)": Tab from anywhere in
//! the list, or Enter on it, opens a one-line input in the panel. Enter sends the reason as `Decision::Declined`:
//! the call isn't run, its result tells the model why, and the turn goes on. An empty input sends nothing,
//! Backspace on it goes back to the choices, and Esc still stops the run like a plain no.
//!
//! marv-rs: a mesh wire request (`tool: "mesh_wire"`, built by marv-session's `mesh::wire_approval`) comes through
//! the same queue: `once` (yes or no; a wire is never remembered as a scope), labeled with the agent that asked,
//! showing who, to what, which mode and the limits, with the taint warning. A `mesh_send`/`mesh_call` that needs a
//! new wire is one prompt for both (#77, tool [`MESH_WIRE_CALL`], built by `mesh::call_wire_approval`): the payload,
//! the wire, its limits and the warning, answered "Yes (wire it)" or "No", never "don't ask again".
//!
//! `preview.files` (/undo) lists files, each with its own diff (at most [`MAX_FILE_DIFF_LINES`]), in a list at most
//! the window's height less [`RESERVED_ROWS`] tall that PgUp/PgDn and the wheel scroll, Yes/No pinned below.
//!
//! Esc declines this and everything waiting and stops the run, except while a subagent's view is
//! open (`escape_cancels: false`): Esc then only closes the view. Every string from the request
//! goes through `printable()`: a Trojan-Source command must not look like something else.
//!
//! The command and text wrap (the full payload is shown); the diff is drawn by `diff_view` (lines
//! cut, not wrapped, numbered as in the transcript). Each queued request gets a fresh `ApprovalView`
//! (Marv keyed it by id), so the cursor starts on Yes for every one.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use marv_core::printable;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Padding, Widget};

use super::diff_view::{self, DiffOptions};
use super::select::{Select, SelectItem, wrap_all};
use super::theme;
use crate::view::{ApprovalRequest, Decision};

pub const MAX_DIFF_LINES: usize = 24;
/// marv-session's `mesh::MESH_WIRE_CALL`: a mesh call and the new wire it needs, in one prompt (#77).
pub const MESH_WIRE_CALL: &str = "mesh_wire_call";
/// Each of several files' diffs (/undo), so a few files fit on the screen at once.
pub const MAX_FILE_DIFF_LINES: usize = 8;
/// The rows the rest of the screen keeps when several files' diffs are shown: the panel's border, title, notes and
/// choices, the status bar, a little transcript. The file list gets what's left (at least [`MIN_FILES_ROWS`]) and
/// scrolls, so a big /undo never pushes Yes/No off the screen.
pub const RESERVED_ROWS: u16 = 16;
pub const MIN_FILES_ROWS: u16 = 4;
/// The reason input's length, in characters: the engine's own cut (`marv_core::MAX_DECLINE_REASON`).
pub use marv_core::MAX_DECLINE_REASON as MAX_REASON_CHARS;

/// One warning line per door out of the sandbox, in the warning colour: what saying yes lets out (Marv's
/// `DOOR_WARNINGS`). A door's own detail (the repository a github write is visible on) comes in its preview's warning,
/// below this line.
pub fn door_warning(door: marv_core::Door) -> &'static str {
    match door {
        marv_core::Door::Host => "Runs OUTSIDE the sandbox, as you, with your files, network and credentials.",
        marv_core::Door::Github => "Visible to other people on GitHub.",
        marv_core::Door::Process => "Keeps running after this turn, until stopped or Marv exits.",
        marv_core::Door::Herdr => "Acts in Herdr, OUTSIDE the sandbox: what starts in a pane runs as you, and keeps running.",
    }
}

/// What a key did at the approval.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// The window's height, which limits a file list (the App sets it as it draws; tests pick one).
    window_rows: u16,
    /// The file list's first shown row, and its last possible one as last drawn.
    files_top: usize,
    files_max: std::cell::Cell<usize>,
    /// The reason being typed after "No, and say why" (#185); None while the choices show.
    pub reason: Option<String>,
}

impl ApprovalView {
    pub fn new(request: ApprovalRequest, waiting: usize, escape_cancels: bool) -> Self {
        // #77: a call that needs a new wire is one prompt for both; yes wires it, and it never offers "always".
        let call_wire = request.tool == MESH_WIRE_CALL;
        let mut items = vec![SelectItem::new(Decision::Yes, if call_wire { "Yes (wire it)" } else { "Yes" })];
        if !request.once && !call_wire {
            items.push(SelectItem::new(
                Decision::Always,
                format!("Yes, and don't ask again for {} this session", request.scope.description),
            ));
        }
        let no = if request.once { "No" } else { "No, and tell Marv what to do instead" };
        items.push(SelectItem::new(Decision::No, no).hint("esc"));
        if request.reason {
            items.push(SelectItem::new(Decision::Declined(String::new()), "No, and say why (Marv carries on)").hint("tab"));
        }
        let window_rows = crossterm::terminal::size().map_or(24, |(_, rows)| rows);
        Self {
            request,
            waiting,
            escape_cancels,
            select: Select::new(items, None),
            window_rows,
            files_top: 0,
            files_max: std::cell::Cell::new(0),
            reason: None,
        }
    }

    /// Whether it offers declining with a reason (a model's tool call).
    fn offers_reason(&self) -> bool {
        self.select.items.iter().any(|i| matches!(i.value, Decision::Declined(_)))
    }

    /// A key while the reason is being typed: Enter sends it (never when empty), Backspace on an empty input goes
    /// back to the choices. Only printable characters are taken, on one line.
    fn on_reason_key(&mut self, key: KeyEvent) -> Option<ApprovalOutcome> {
        let reason = self.reason.as_mut()?;
        match key.code {
            KeyCode::Enter => {
                let text = reason.trim();
                if text.is_empty() {
                    return None;
                }
                return Some(ApprovalOutcome::Decide(Decision::Declined(text.to_string())));
            }
            KeyCode::Backspace => {
                if reason.pop().is_none() {
                    self.reason = None;
                }
            }
            KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                push_reason(reason, &c.to_string());
            }
            _ => {}
        }
        None
    }

    /// Pasted text while the reason is being typed: added on one line (newlines become spaces). False when the
    /// input isn't open, so the paste goes nowhere.
    pub fn paste(&mut self, text: &str) -> bool {
        match self.reason.as_mut() {
            Some(reason) => {
                push_reason(reason, text);
                true
            }
            None => false,
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Option<ApprovalOutcome> {
        if key.code == KeyCode::Esc {
            return self.escape_cancels.then_some(ApprovalOutcome::CancelAll);
        }
        if self.reason.is_some() {
            return self.on_reason_key(key);
        }
        if key.code == KeyCode::Tab && self.offers_reason() {
            self.reason = Some(String::new());
            return None;
        }
        // A file list (/undo) scrolls with PgUp/PgDn: the transcript stays put meanwhile.
        if self.has_files() && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
            let page = self.files_rows() as i32;
            self.scroll_files(if key.code == KeyCode::PageUp { -page } else { page });
            return None;
        }
        match self.select.on_key(key) {
            // The item opens the input; only Enter there sends a reason.
            Some(Decision::Declined(_)) => {
                self.reason = Some(String::new());
                None
            }
            chosen => chosen.map(ApprovalOutcome::Decide),
        }
    }

    /// Whether the request lists files (/undo): PgUp/PgDn and the wheel scroll them, not the transcript.
    pub fn has_files(&self) -> bool {
        self.request.preview.files.as_ref().is_some_and(|f| !f.is_empty())
    }

    pub fn set_window_rows(&mut self, rows: u16) {
        self.window_rows = rows;
    }

    /// Moves the file list by `rows` (negative: up), within what was last drawn.
    pub fn scroll_files(&mut self, rows: i32) {
        let top = (self.files_top as i64 + rows as i64).clamp(0, self.files_max.get() as i64);
        self.files_top = top as usize;
    }

    fn files_rows(&self) -> u16 {
        self.window_rows.saturating_sub(RESERVED_ROWS).max(MIN_FILES_ROWS)
    }

    /// The files' titles (with their notes) and diffs, wrapped at `width`.
    fn file_lines(&self, width: u16) -> Vec<Line<'static>> {
        let dim = Style::new().fg(theme::DIM);
        let mut lines = Vec::new();
        for file in self.request.preview.files.iter().flatten() {
            lines.push(Line::default());
            let mut title = vec![Span::raw(printable(&file.title))];
            if let Some(note) = &file.note {
                title.push(Span::styled(format!(" · {}", printable(note)), dim));
            }
            lines.push(Line::from(title));
            if let Some(diff) = file.diff.as_ref().filter(|d| !d.lines.is_empty()) {
                let options = DiffOptions { max: Some(MAX_FILE_DIFF_LINES), more: diff.more, indent: 2, ..Default::default() };
                lines.extend(diff_view::render(&diff.lines, width, options));
            }
        }
        wrap_all(&lines, width)
    }

    /// The box's content at an inner width of `width`.
    pub fn lines(&self, width: u16) -> Vec<Line<'static>> {
        let dim = Style::new().fg(theme::DIM);
        let request = &self.request;
        let preview = &request.preview;
        let mut lines = Vec::new();
        if let Some(agent) = &request.agent {
            lines.push(Line::styled(format!("[{}]", printable(agent)), dim));
        }
        lines.push(Line::styled(printable(&preview.title), Style::new().add_modifier(Modifier::BOLD)));

        let diff = preview.diff.as_deref().unwrap_or_default();
        if !diff.is_empty() || preview.command.is_some() || preview.text.is_some() {
            lines.push(Line::default());
            if let Some(command) = &preview.command {
                for (i, row) in printable(command).split('\n').enumerate() {
                    let prompt = if i == 0 { "$ " } else { "  " };
                    lines.push(Line::from(vec![
                        Span::styled(prompt, dim),
                        Span::styled(row.to_string(), Style::new().fg(theme::CODE)),
                    ]));
                }
            }
            if let Some(text) = &preview.text {
                lines.extend(printable(text).split('\n').map(|row| Line::raw(row.to_string())));
            }
            if !diff.is_empty() {
                lines.extend(diff_view::render(diff, width, DiffOptions { max: Some(MAX_DIFF_LINES), ..Default::default() }));
            }
        }
        if self.has_files() {
            // At most files_rows tall, scrolled with PgUp/PgDn or the wheel.
            let files = self.file_lines(width);
            let rows = self.files_rows() as usize;
            let max = files.len().saturating_sub(rows);
            self.files_max.set(max);
            let top = self.files_top.min(max);
            lines.extend(files.iter().skip(top).take(rows).cloned());
            if max > 0 {
                lines.push(Line::styled("PgUp/PgDn or the wheel scroll the files", dim));
            }
        }
        if let Some(note) = &preview.note {
            lines.push(Line::styled(printable(note), dim));
        }
        if let Some(door) = self.request.door {
            lines.push(Line::styled(format!("⚠ {}", door_warning(door)), Style::new().fg(theme::WARNING)));
        }
        if let Some(warning) = &preview.warning {
            lines.push(Line::styled(format!("⚠ {}", printable(warning)), Style::new().fg(theme::WARNING)));
        }
        if self.waiting > 0 {
            lines.push(Line::styled(format!("{} more waiting", self.waiting), dim));
        }
        lines.push(Line::default());
        if self.reason.is_some() {
            let mut lines = wrap_all(&lines, width);
            lines.extend(self.reason_lines(width));
            return lines;
        }
        lines.push(Line::raw("Do you want to proceed?"));
        lines.extend(self.select.render());
        wrap_all(&lines, width)
    }

    /// The reason input's block at `width`, wrapped: the question, what's typed, the hint. Empty when it isn't open.
    fn reason_lines(&self, width: u16) -> Vec<Line<'static>> {
        let Some(reason) = &self.reason else { return Vec::new() };
        let lines = [
            Line::raw("Why not? Marv reads this and carries on."),
            Line::from(vec![
                Span::styled("❯ ", Style::new().fg(theme::ACCENT)),
                Span::raw(printable(reason)),
                Span::styled("▏", Style::new().fg(theme::ACCENT)),
            ]),
            Line::styled("enter sends · backspace on empty goes back · esc stops", Style::new().fg(theme::DIM)),
        ];
        wrap_all(&lines, width)
    }

    /// Rows it needs at `width` (the bottom panel sizes itself to this).
    pub fn height(&self, width: u16) -> u16 {
        self.lines(width.saturating_sub(4)).len() as u16 + 2
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let warned = self.request.preview.warning.is_some() || self.request.door.is_some();
        let border = if warned { theme::WARNING } else { theme::ACCENT };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(border))
            .padding(Padding::horizontal(1));
        let inner = block.inner(area);
        block.render(area, buf);
        let lines = self.lines(inner.width);
        // Taller than the screen: keep the choice in view and cut the payload's end instead.
        let skip = lines.len().saturating_sub(inner.height as usize);
        // What stays pinned at the bottom: the blank line, then the reason block (its question, the input wrapped at
        // this width, the hint) or "Do you want to proceed?" and the choices.
        let pinned = match &self.reason {
            Some(_) => 1 + self.reason_lines(inner.width).len(),
            None => 2 + self.select.items.len(),
        };
        let keep_head = lines.len().saturating_sub(pinned + skip);
        let shown = lines[..keep_head].iter().chain(&lines[keep_head + skip..]);
        for (i, line) in shown.enumerate().take(inner.height as usize) {
            buf.set_line(inner.x, inner.y + i as u16, line, inner.width);
        }
    }
}

/// Adds `text` to the reason as the engine reads it (`marv_core::reason_chars`: one line, control characters
/// dropped), at most [`MAX_REASON_CHARS`].
fn push_reason(reason: &mut String, text: &str) {
    let room = MAX_REASON_CHARS.saturating_sub(reason.chars().count());
    reason.extend(marv_core::reason_chars(text).take(room));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::{DiffKind, DiffLine, Preview, Scope};

    fn edit() -> ApprovalRequest {
        let line = |kind, text: &str| DiffLine { kind, text: text.into(), old_line: None, new_line: None };
        ApprovalRequest {
            reason: true,
            tool: "edit_file".into(),
            label: "src/greet.ts".into(),
            scope: Scope { key: "files".into(), description: "file changes".into() },
            preview: Preview {
                title: "Edit src/greet.ts".into(),
                diff: Some(vec![
                    line(DiffKind::Ctx, "export function greet(name: string) {"),
                    line(DiffKind::Del, "  return `Hello, ${name}!`;"),
                    line(DiffKind::Add, "  return `Hi, ${name}!`;"),
                    line(DiffKind::Ctx, "}"),
                ]),
                ..Default::default()
            },
            agent: None,
            network: false,
            once: false,
            must_ask: false,
            door: None,
        }
    }

    fn command() -> ApprovalRequest {
        ApprovalRequest {
            reason: true,
            tool: "bash".into(),
            label: "bun test".into(),
            scope: Scope { key: "bash:bun test".into(), description: "this exact command".into() },
            preview: Preview {
                title: "Run a command".into(),
                command: Some("bun test".into()),
                note: Some("sandboxed · no network".into()),
                ..Default::default()
            },
            agent: None,
            network: false,
            once: false,
            must_ask: false,
            door: None,
        }
    }

    fn frame(view: &ApprovalView) -> String {
        let width = 80;
        let area = Rect::new(0, 0, width, view.height(width));
        let mut buf = Buffer::empty(area);
        view.render(area, &mut buf);
        (0..area.height).map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>()).collect::<Vec<_>>().join("\n")
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn numbered(n: usize, text: impl Fn(usize) -> String) -> Vec<DiffLine> {
        (1..=n).map(|i| DiffLine { kind: DiffKind::Add, text: text(i), old_line: None, new_line: Some(i) }).collect()
    }

    #[test]
    fn a_new_view_gives_the_next_request_a_fresh_prompt() {
        // (cursor back on Yes: the App makes one per queued request, as Marv keyed it by id)
        let mut view = ApprovalView::new(command(), 0, true);
        view.on_key(key(KeyCode::Down));
        assert!(frame(&view).contains("❯ Yes, and don't ask again"));
        let next = ApprovalView::new(edit(), 0, true);
        assert!(frame(&next).contains("❯ Yes  "));
    }

    #[test]
    fn once_yes_and_no_only_no_dont_ask_again() {
        // (/mcp add: each install is its own decision)
        let mut view = ApprovalView::new(ApprovalRequest { once: true, ..command() }, 0, true);
        assert!(!frame(&view).contains("don't ask again"));
        view.on_key(key(KeyCode::Down));
        assert!(frame(&view).contains("❯ No · esc"));
        assert_eq!(view.on_key(key(KeyCode::Enter)), Some(ApprovalOutcome::Decide(Decision::No)));
    }

    #[test]
    fn a_mesh_wire_request_shows_the_whole_wire_and_yes_or_no_only() {
        // What marv-session's `mesh::wire_approval` sends for `worker-1`'s first send to the parent.
        let wire = ApprovalRequest {
            reason: false,
            tool: "mesh_wire".into(),
            label: "Wire worker-1 -> agent.parent.inbox (send)".into(),
            scope: Scope {
                key: "mesh-wire:worker-1:agent.parent.inbox:send".into(),
                description: "worker-1 -> agent.parent.inbox".into(),
            },
            preview: Preview {
                title: "Wire worker-1 -> agent.parent.inbox (send)".into(),
                text: Some(
                    "worker-1 asks to send to agent.parent.inbox.\nLimits: 32768 bytes a message, 30 a minute (bursts of 10)."
                        .into(),
                ),
                note: Some("The wire lasts until either agent ends.".into()),
                warning: Some(
                    "What arrives through it is data from another agent: the receiving session counts as tainted.".into(),
                ),
                ..Default::default()
            },
            agent: Some("worker-1".into()),
            network: false,
            once: true,
            must_ask: false,
            door: None,
        };
        let mut view = ApprovalView::new(wire, 0, true);
        let f = frame(&view);
        for part in ["[worker-1]", "Wire worker-1 -> agent.parent.inbox (send)", "32768 bytes a message", "⚠ What arrives"] {
            assert!(f.contains(part), "{part}\n{f}");
        }
        assert!(!f.contains("don't ask again"), "{f}");
        assert_eq!(view.on_key(key(KeyCode::Enter)), Some(ApprovalOutcome::Decide(Decision::Yes)));
    }

    #[test]
    fn a_call_needing_a_new_wire_is_one_prompt_answered_yes_wire_it_or_no() {
        // What marv-session's `mesh::call_wire_approval` sends for `worker-1`'s first send to the parent.
        let call = ApprovalRequest {
            reason: false,
            tool: MESH_WIRE_CALL.into(),
            label: "Wire worker-1 -> agent.parent.inbox (send)".into(),
            scope: Scope {
                key: "mesh-wire:worker-1:agent.parent.inbox:send".into(),
                description: "worker-1 -> agent.parent.inbox".into(),
            },
            preview: Preview {
                title: "Send a message to agent.parent.inbox over a new wire".into(),
                text: Some("found 3 callers".into()),
                note: Some(
                    "Wire worker-1 -> agent.parent.inbox (send). Limits: 32768 bytes a message, 30 a minute (bursts of 10). \
                     The wire lasts until either agent ends."
                        .into(),
                ),
                warning: Some(
                    "What arrives through it is data from another agent: the receiving session counts as tainted.".into(),
                ),
                ..Default::default()
            },
            agent: Some("worker-1".into()),
            network: false,
            once: true,
            must_ask: false,
            door: None,
        };
        let mut view = ApprovalView::new(call.clone(), 0, true);
        let f = frame(&view);
        for part in
            ["[worker-1]", "over a new wire", "found 3 callers", "32768 bytes a message", "⚠ What arrives", "❯ Yes (wire it)"]
        {
            assert!(f.contains(part), "{part}\n{f}");
        }
        assert!(!f.contains("don't ask again"), "{f}");
        assert_eq!(view.select.items.len(), 2, "Yes (wire it) and No only");
        assert_eq!(view.on_key(key(KeyCode::Enter)), Some(ApprovalOutcome::Decide(Decision::Yes)));
        // Never "always", even if a request came without `once`.
        let view = ApprovalView::new(ApprovalRequest { once: false, ..call }, 0, true);
        assert!(!frame(&view).contains("don't ask again"));
        assert_eq!(view.select.items.len(), 2);
    }

    #[test]
    fn says_which_subagent_is_asking_and_how_many_more_are_waiting() {
        let f = frame(&ApprovalView::new(ApprovalRequest { agent: Some("implementer · Task 2".into()), ..edit() }, 2, true));
        assert!(f.contains("[implementer · Task 2]"));
        assert!(f.contains("2 more waiting"));
    }

    #[test]
    fn esc_cancels_everything_unless_a_subagents_view_is_open() {
        assert_eq!(ApprovalView::new(edit(), 0, true).on_key(key(KeyCode::Esc)), Some(ApprovalOutcome::CancelAll));
        assert_eq!(ApprovalView::new(edit(), 0, false).on_key(key(KeyCode::Esc)), None);
    }

    #[test]
    fn shows_the_change_as_a_diff_and_the_three_choices() {
        let f = frame(&ApprovalView::new(edit(), 0, true));
        assert!(f.contains("Edit src/greet.ts"));
        assert!(f.contains("-   return `Hello, ${name}!`;"));
        assert!(f.contains("+   return `Hi, ${name}!`;"));
        assert!(f.contains("Do you want to proceed?"));
        assert!(f.contains("Yes"));
        assert!(f.contains("don't ask again for file changes this session"));
        assert!(f.contains("No, and tell Marv what to do instead · esc"));
    }

    #[test]
    fn shows_a_command_with_its_sandbox_note() {
        let f = frame(&ApprovalView::new(command(), 0, true));
        assert!(f.contains("$ bun test"));
        assert!(f.contains("sandboxed · no network"));
    }

    #[test]
    fn shows_plain_text_like_a_memory_without_the_dollar_prompt() {
        let mut request = command();
        request.preview = Preview {
            title: "Remember (personal, all projects)".into(),
            text: Some("Prefers short answers.".into()),
            ..Default::default()
        };
        let f = frame(&ApprovalView::new(request, 0, true));
        assert!(f.contains("Prefers short answers."));
        assert!(!f.contains("$ Prefers"));
    }

    #[test]
    fn a_door_shows_its_warning_line_above_the_previews_own() {
        let mut request = command();
        request.door = Some(marv_core::Door::Github);
        request.preview.warning = Some("on FlologixAI/marv".into());
        let f = frame(&ApprovalView::new(request, 0, true));
        let door = f.find("⚠ Visible to other people on GitHub.").expect("door line");
        assert!(door < f.find("⚠ on FlologixAI/marv").expect("preview warning"), "{f}");
    }

    #[test]
    fn shows_a_warning_when_a_command_isnt_sandboxed() {
        let mut request = command();
        request.preview.note = None;
        request.preview.warning = Some("runs WITHOUT a sandbox".into());
        assert!(frame(&ApprovalView::new(request, 0, true)).contains("⚠ runs WITHOUT a sandbox"));
    }

    #[test]
    fn a_long_diff_is_cut_saying_how_much_more_there_is() {
        let mut request = edit();
        request.preview = Preview {
            title: "Create big.ts".into(),
            diff: Some(
                (0..60)
                    .map(|i| DiffLine { kind: DiffKind::Add, text: format!("line {i}"), old_line: None, new_line: None })
                    .collect(),
            ),
            ..Default::default()
        };
        let f = frame(&ApprovalView::new(request, 0, true));
        assert!(f.contains("line 0"));
        assert!(!f.contains("line 59"));
        assert!(f.contains("… 36 more lines"));
    }

    #[test]
    fn a_huge_new_files_preview_draws_its_first_lines() {
        let mut request = edit();
        request.preview = Preview {
            title: "Create huge.ts".into(),
            diff: Some(numbered(1_000_000, |i| format!("l{i}"))),
            ..Default::default()
        };
        let f = frame(&ApprovalView::new(request, 0, true));
        assert!(f.contains("│       1 + l1"), "{f}");
        assert!(f.contains("… 999976 more lines"));
    }

    #[test]
    fn a_numbered_diff_counts_exactly_what_it_leaves_out() {
        let mut request = edit();
        request.preview =
            Preview { title: "Create big.ts".into(), diff: Some(numbered(100, |i| format!("line {i}"))), ..Default::default() };
        let f = frame(&ApprovalView::new(request, 0, true));
        assert!(f.contains("│   1 + line 1")); // a column wide enough for line 100, though it isn't shown
        assert!(f.contains("│  24 + line 24"));
        assert!(f.contains("… 76 more lines"));
    }

    #[test]
    fn keys_decide() {
        for (keys, decision) in [
            (vec![KeyCode::Enter], Decision::Yes),
            (vec![KeyCode::Down, KeyCode::Enter], Decision::Always),
            (vec![KeyCode::Down, KeyCode::Down, KeyCode::Enter], Decision::No),
        ] {
            let mut view = ApprovalView::new(edit(), 0, true);
            let outcome = keys.into_iter().map(|k| view.on_key(key(k))).last().flatten();
            assert_eq!(outcome, Some(ApprovalOutcome::Decide(decision)));
        }
    }

    /// #185: "No, and say why" (tab, or Enter on it) opens a one-line input; Enter sends the reason, never an empty
    /// one, and Backspace on an empty input goes back to the choices. Esc still stops everything.
    #[test]
    fn no_and_say_why_types_a_reason() {
        let mut view = ApprovalView::new(command(), 0, true);
        assert!(frame(&view).contains("No, and say why (Marv carries on) · tab"));
        // Enter on the item opens the input; nothing is decided yet.
        let mut moved = ApprovalView::new(command(), 0, true);
        for _ in 0..3 {
            moved.on_key(key(KeyCode::Down));
        }
        assert_eq!(moved.on_key(key(KeyCode::Enter)), None);
        assert_eq!(moved.reason.as_deref(), Some(""));
        // Tab from anywhere opens it too.
        assert_eq!(view.on_key(key(KeyCode::Tab)), None);
        assert!(frame(&view).contains("Why not?"));
        assert_eq!(view.on_key(key(KeyCode::Enter)), None, "an empty reason sends nothing");
        for c in "use pnpm".chars() {
            view.on_key(key(KeyCode::Char(c)));
        }
        assert!(view.paste("\n not bun\x1b"), "a paste goes into the input, on one line");
        assert!(frame(&view).contains("❯ use pnpm  not bun"), "{}", frame(&view));
        assert_eq!(
            view.on_key(key(KeyCode::Enter)),
            Some(ApprovalOutcome::Decide(Decision::Declined("use pnpm  not bun".into())))
        );
        // Backspace on an empty input: back to the choices.
        let mut back = ApprovalView::new(command(), 0, true);
        back.on_key(key(KeyCode::Tab));
        back.on_key(key(KeyCode::Char('x')));
        back.on_key(key(KeyCode::Backspace));
        back.on_key(key(KeyCode::Backspace));
        assert!(back.reason.is_none() && frame(&back).contains("Do you want to proceed?"));
        assert!(!back.paste("x"), "no input open: the paste goes nowhere");
        // Esc while typing still stops everything, like a plain no.
        back.on_key(key(KeyCode::Tab));
        assert_eq!(back.on_key(key(KeyCode::Esc)), Some(ApprovalOutcome::CancelAll));
        // The step-limit question and a mesh wire don't offer it.
        // Only a model's call offers it: not the step-limit question, a mesh wire, /undo or /mcp add.
        for tool in ["continue", "mesh_wire", MESH_WIRE_CALL, "undo", "mcp_add"] {
            let mut view = ApprovalView::new(ApprovalRequest { tool: tool.into(), reason: false, ..command() }, 0, true);
            assert!(!frame(&view).contains("say why"), "{tool}");
            assert_eq!(view.on_key(key(KeyCode::Tab)), None);
            assert!(view.reason.is_none());
        }
        // At most MAX_REASON_CHARS.
        let mut long = ApprovalView::new(command(), 0, true);
        long.on_key(key(KeyCode::Tab));
        long.paste(&"a".repeat(MAX_REASON_CHARS + 10));
        assert_eq!(long.reason.as_ref().map(|r| r.chars().count()), Some(MAX_REASON_CHARS));
    }

    /// A long reason wraps; in a panel too short for everything, the whole reason block stays in view and the
    /// payload is cut instead.
    #[test]
    fn a_wrapped_reason_stays_in_view_when_the_payload_is_cut() {
        let mut request = command();
        request.preview.command = Some((1..=30).map(|i| format!("echo {i}")).collect::<Vec<_>>().join("\n"));
        let mut view = ApprovalView::new(request, 0, true);
        view.on_key(key(KeyCode::Tab));
        view.paste(&format!("{} END", "word ".repeat(30)));
        let (width, height) = (40, 18);
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        view.render(area, &mut buf);
        let f = (0..height).map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>()).collect::<Vec<_>>().join("\n");
        for part in ["Why not?", "❯ word", "END", "enter sends"] {
            assert!(f.contains(part), "{part}\n{f}");
        }
        assert!(f.contains("echo 1") && !f.contains("echo 30"), "the payload is what's cut\n{f}");
    }

    #[test]
    fn every_request_string_is_printable() {
        let mut request = command();
        request.preview.command = Some("rm -rf /tmp/x\u{202e}txt.exe\x1b[2J".into());
        request.agent = Some("evil\x1b]0;title\x07".into());
        let f = frame(&ApprovalView::new(request, 0, true));
        assert!(!f.contains('\u{202e}') && !f.contains('\x1b'));
        assert!(f.contains("$ rm -rf /tmp/xtxt.exe"));
    }

    #[test]
    fn a_long_command_wraps_so_all_of_it_is_shown() {
        let mut request = command();
        request.preview.command = Some(format!("echo {}", "word ".repeat(40)));
        let view = ApprovalView::new(request, 0, true);
        let f = frame(&view);
        assert_eq!(f.matches("word").count(), 40);
    }

    // Ports tests/approval.test.tsx "Approval: an undo of many files" (Greptile on Marv #71).
    fn undo(files: usize) -> ApprovalRequest {
        use crate::view::{DiffShown, PreviewFile};
        let diff = |n: usize| DiffShown {
            lines: (1..=8)
                .map(|i| DiffLine { kind: DiffKind::Add, text: format!("file {n} line {i}"), old_line: None, new_line: Some(i) })
                .collect(),
            more: 0,
        };
        ApprovalRequest {
            reason: false,
            tool: "undo".into(),
            label: "turn 3".into(),
            once: true,
            must_ask: false,
            scope: Scope { key: "undo".into(), description: "undo".into() },
            preview: Preview {
                title: "Undo turn 3".into(),
                files: Some(
                    (1..=files)
                        .map(|i| PreviewFile { title: format!("restore src/file{i}.ts"), diff: Some(diff(i)), note: None })
                        .collect(),
                ),
                note: Some(format!("{files} files")),
                ..Default::default()
            },
            agent: None,
            network: false,
            door: None,
        }
    }

    #[test]
    fn the_file_list_fits_the_window_and_scrolls_yes_and_no_stay_visible() {
        let mut view = ApprovalView::new(undo(12), 0, true);
        view.set_window_rows(30);
        let first = frame(&view);
        assert!(first.split('\n').count() <= 30, "{first}");
        assert!(first.contains("restore src/file1.ts"));
        assert!(!first.contains("restore src/file12.ts"));
        assert!(first.contains("❯ Yes"));
        assert!(first.contains("No"));
        assert!(first.contains("PgUp/PgDn or the wheel scroll the files"));
        for _ in 0..20 {
            assert_eq!(view.on_key(key(KeyCode::PageDown)), None);
        }
        let last = frame(&view);
        assert!(last.split('\n').count() <= 30, "{last}");
        assert!(last.contains("restore src/file12.ts"), "{last}");
        assert!(last.contains("file 12 line 8"));
        assert!(last.contains("❯ Yes"));
        assert!(last.contains("No"));
        view.scroll_files(-3); // the wheel: three rows up
        assert!(!frame(&view).contains("file 12 line 8"));
        for _ in 0..20 {
            view.on_key(key(KeyCode::PageUp));
        }
        assert!(frame(&view).contains("restore src/file1.ts"));
    }

    #[test]
    fn a_few_files_are_all_shown_with_their_notes_and_no_scroll_hint() {
        let mut request = undo(2);
        if let Some(files) = request.preview.files.as_mut() {
            files[1].diff = None;
            files[1].note = Some("binary".into());
        }
        let mut view = ApprovalView::new(request, 0, true);
        view.set_window_rows(40);
        let f = frame(&view);
        assert!(f.contains("restore src/file1.ts") && f.contains("file 1 line 8"), "{f}");
        assert!(f.contains("restore src/file2.ts · binary"), "{f}");
        assert!(!f.contains("PgUp/PgDn"));
        assert!(!f.contains("don't ask again"));
    }
}
