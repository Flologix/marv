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
//!
//! The command and text wrap (the full payload is shown); diff lines are cut, not wrapped, as in
//! `DiffView`. Each queued request gets a fresh `ApprovalView` (Marv keyed it by id), so the cursor
//! starts on Yes for every one.

use crossterm::event::{KeyCode, KeyEvent};
use marv_core::printable;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Padding, Widget};

use super::diff_view::mark;
use super::select::{Select, SelectItem, truncate_end, wrap_all};
use super::theme;
use crate::view::{ApprovalRequest, Decision, DiffKind, DiffLine};

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

/// The preview's diff, at most `max` lines, the rest counted: numbered lines right-aligned in a
/// column wide enough for *all* of them (a loop over the lines, not a max over a spread: a new
/// file's preview holds all its lines), `+ `/`- ` marks, text cut rather than wrapped.
///
/// TODO(#9): a stand-in until `diff_view::render` is ported; then call
/// `diff_view::render(lines, width, DiffOptions { max: Some(MAX_DIFF_LINES), ..Default::default() })`.
fn diff_lines(lines: &[DiffLine], max: usize, width: u16) -> Vec<Line<'static>> {
    let mut widest = 0;
    for line in lines {
        if let Some(n) = line.old_line.max(line.new_line) {
            widest = widest.max(n);
        }
    }
    let number_width = if widest > 0 { widest.to_string().len() } else { 0 };
    let mut out: Vec<Line<'static>> = lines
        .iter()
        .take(max)
        .map(|line| {
            let style = match line.kind {
                DiffKind::Add => Style::new().fg(theme::DIFF_ADD),
                DiffKind::Del => Style::new().fg(theme::DIFF_DEL),
                DiffKind::Ctx | DiffKind::Gap => Style::new(),
            };
            let mut spans = Vec::new();
            if number_width > 0 {
                let n = if line.kind == DiffKind::Del { line.old_line } else { line.new_line };
                let n = n.map_or_else(String::new, |n| n.to_string());
                spans.push(Span::styled(format!("{n:>number_width$} "), Style::new().fg(theme::DIM)));
            }
            let text = if line.kind == DiffKind::Gap { "…".to_string() } else { printable(&line.text).replace('\t', "  ") };
            spans.push(Span::styled(format!("{}{text}", mark(line.kind)), style));
            truncate_end(Line::from(spans), width)
        })
        .collect();
    if lines.len() > max {
        out.push(Line::styled(format!("… {} more lines", lines.len() - max), Style::new().fg(theme::DIM)));
    }
    out
}

impl ApprovalView {
    pub fn new(request: ApprovalRequest, waiting: usize, escape_cancels: bool) -> Self {
        let mut items = vec![SelectItem::new(Decision::Yes, "Yes")];
        if !request.once {
            items.push(SelectItem::new(
                Decision::Always,
                format!("Yes, and don't ask again for {} this session", request.scope.description),
            ));
        }
        let no = if request.once { "No" } else { "No, and tell Marv what to do instead" };
        items.push(SelectItem::new(Decision::No, no).hint("esc"));
        Self { request, waiting, escape_cancels, select: Select::new(items, None) }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Option<ApprovalOutcome> {
        if key.code == KeyCode::Esc {
            return self.escape_cancels.then_some(ApprovalOutcome::CancelAll);
        }
        self.select.on_key(key).map(ApprovalOutcome::Decide)
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
                lines.extend(diff_lines(diff, MAX_DIFF_LINES, width));
            }
        }
        if let Some(note) = &preview.note {
            lines.push(Line::styled(printable(note), dim));
        }
        if let Some(warning) = &preview.warning {
            lines.push(Line::styled(format!("⚠ {}", printable(warning)), Style::new().fg(theme::WARNING)));
        }
        if self.waiting > 0 {
            lines.push(Line::styled(format!("{} more waiting", self.waiting), dim));
        }
        lines.push(Line::default());
        lines.push(Line::raw("Do you want to proceed?"));
        lines.extend(self.select.render());
        wrap_all(&lines, width)
    }

    /// Rows it needs at `width` (the bottom panel sizes itself to this).
    pub fn height(&self, width: u16) -> u16 {
        self.lines(width.saturating_sub(4)).len() as u16 + 2
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let border = if self.request.preview.warning.is_some() { theme::WARNING } else { theme::ACCENT };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(border))
            .padding(Padding::horizontal(1));
        let inner = block.inner(area);
        block.render(area, buf);
        let lines = self.lines(inner.width);
        // Taller than the screen: keep the choice in view and cut the payload's end instead.
        let skip = lines.len().saturating_sub(inner.height as usize);
        let keep_head = lines.len().saturating_sub(self.select.items.len() + 2 + skip);
        let shown = lines[..keep_head].iter().chain(&lines[keep_head + skip..]);
        for (i, line) in shown.enumerate().take(inner.height as usize) {
            buf.set_line(inner.x, inner.y + i as u16, line, inner.width);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::{Preview, Scope};
    use crossterm::event::KeyModifiers;

    fn edit() -> ApprovalRequest {
        let line = |kind, text: &str| DiffLine { kind, text: text.into(), old_line: None, new_line: None };
        ApprovalRequest {
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
        }
    }

    fn command() -> ApprovalRequest {
        ApprovalRequest {
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
}
