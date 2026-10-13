//! The transcript: the welcome banner, then every message. Ports `src/ui/Transcript.tsx` (tests:
//! `tests/render-performance.test.tsx`).
//!
//! In the alternate screen every frame is redrawn, so the whole transcript stays in memory (never
//! printed once like Ink's `<Static>`, which would scroll off and be lost). Each entry's lines are
//! cached per (width, show_steps) like Marv's `memo` — only entries with steps or a diff depend on
//! ctrl+o, so toggling re-lays-out just those — and [`super::scroll_view`] culls to the viewport.
//! A subagent's entry records which transcript rows it covers ([`agent_entry_at`]) so a click can
//! open its view. Text is raw here; `message_view` cleans it.
//!
//! The welcome banner isn't cached: the martian in it animates, so it is drawn each frame it's in
//! the viewport ([`welcome_lines`]), and its height is fixed.

use std::ops::Range;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

use super::martian::FrameName;
use super::message_view;
use super::welcome::{self, Welcome};
use crate::view::{Message, Role, WelcomeInfo};

/// The welcome banner's rows, and the blank line after it.
pub const WELCOME_HEIGHT: usize = welcome::HEIGHT as usize + 1;

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

#[derive(Debug, Clone)]
pub struct Transcript {
    pub items: Vec<TranscriptItem>,
    pub cache: Vec<Option<EntryCache>>,
}

impl Default for Transcript {
    fn default() -> Self {
        Self::new()
    }
}

impl Transcript {
    /// Just the welcome banner.
    pub fn new() -> Self {
        Self { items: vec![TranscriptItem::Welcome], cache: vec![None] }
    }

    /// Back to just the welcome banner (`/clear`).
    pub fn clear(&mut self) {
        *self = Self::new();
    }

    /// The welcome banner, then `messages` (a resumed session).
    pub fn replace(&mut self, messages: Vec<Message>) {
        self.clear();
        for message in messages {
            self.push(message);
        }
    }

    pub fn push(&mut self, message: Message) {
        self.items.push(TranscriptItem::Message(Box::new(message)));
        self.cache.push(None);
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The message with `id`, to change: its cached lines go.
    pub fn message_mut(&mut self, id: u64) -> Option<&mut Message> {
        // Usually one of the last few entries: search from the end.
        let index = self.items.iter().rposition(|item| matches!(item, TranscriptItem::Message(m) if m.id == id))?;
        self.cache[index] = None;
        match &mut self.items[index] {
            TranscriptItem::Message(m) => Some(m),
            TranscriptItem::Welcome => None,
        }
    }

    /// The messages, in order.
    pub fn messages(&self) -> impl Iterator<Item = &Message> {
        self.items.iter().filter_map(|item| match item {
            TranscriptItem::Message(m) => Some(&**m),
            TranscriptItem::Welcome => None,
        })
    }

    /// Each entry's height at `width` (filling the cache): the scroll view's input. An entry is laid
    /// out again only when it changed, the width changed, or ctrl+o changed and it has details.
    pub fn heights(&mut self, width: u16, show_steps: bool) -> Vec<usize> {
        self.items
            .iter()
            .zip(&mut self.cache)
            .map(|(item, cache)| match item {
                TranscriptItem::Welcome => WELCOME_HEIGHT,
                TranscriptItem::Message(message) => {
                    // Only entries with steps or a diff get the flag, so ctrl+o lays out just those.
                    let steps = show_steps && message_view::has_details(message);
                    let fresh = cache.as_ref().is_some_and(|c| c.width == width && c.show_steps == steps);
                    if !fresh {
                        let lines = message_view::render(message, width, steps, None);
                        *cache = Some(EntryCache { width, show_steps: steps, lines });
                    }
                    cache.as_ref().map_or(0, |c| c.lines.len())
                }
            })
            .collect()
    }

    /// The cached lines of entry `index` (empty for the welcome banner, or before [`heights`](Self::heights)).
    pub fn lines(&self, index: usize) -> &[Line<'static>] {
        self.cache.get(index).and_then(Option::as_ref).map_or(&[], |c| c.lines.as_slice())
    }

    /// The transcript rows each subagent entry covers, from `heights` (Marv's `onAgentRef`).
    pub fn agent_entries(&self, heights: &[usize]) -> Vec<(u64, Range<usize>)> {
        let mut row = 0;
        let mut entries = Vec::new();
        for (item, height) in self.items.iter().zip(heights) {
            if let TranscriptItem::Message(m) = item
                && m.role == Role::Tool
                && m.text == "agent"
            {
                entries.push((m.id, row..row + height));
            }
            row += height;
        }
        entries
    }
}

/// The id of the subagent entry covering a transcript row, from the last layout.
pub fn agent_entry_at(row: usize, entries: &[(u64, Range<usize>)]) -> Option<u64> {
    entries.iter().find(|(_, rows)| rows.contains(&row)).map(|(id, _)| *id)
}

/// The welcome banner as lines (its widget drawn into a scratch buffer), a blank line after.
pub fn welcome_lines(info: &WelcomeInfo, frame: FrameName, width: u16) -> Vec<Line<'static>> {
    let area = Rect::new(0, 0, width, welcome::HEIGHT);
    let mut buf = Buffer::empty(area);
    Welcome { info, frame }.render(area, &mut buf);
    let mut lines: Vec<Line<'static>> = (0..area.height)
        .map(|y| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            let mut x = 0;
            while x < width {
                let cell = &buf[(x, y)];
                let style = cell.style();
                let symbol = cell.symbol();
                x += (symbol.width() as u16).max(1);
                match spans.last_mut() {
                    Some(last) if last.style == style => last.content.to_mut().push_str(symbol),
                    _ => spans.push(Span::styled(symbol.to_string(), style)),
                }
            }
            Line::from(spans)
        })
        .collect();
    lines.push(Line::default());
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::scroll_view::ScrollView;
    use crate::view::{DiffKind, DiffLine, DiffShown, ToolState, ToolStatus};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::time::{Duration, Instant};

    const REPLY: &str =
        "Each file's line count:\n\n- `src/agent.ts`: 124 lines\n- `src/app.tsx`: 433 lines\n- `src/cli.tsx`: 74 lines";

    fn message(id: u64, role: Role, text: &str) -> Message {
        Message { id, role, text: text.into(), is_error: false, markdown: false, notice: None, tool: None }
    }

    fn tool(id: u64, name: &str, label: &str) -> Message {
        let status = ToolStatus {
            label: label.into(),
            status: ToolState::Done,
            summary: Some("124 lines".into()),
            steps: None,
            diff: None,
        };
        Message { tool: Some(status), ..message(id, Role::Tool, name) }
    }

    /// A long agent session: mostly tool calls, with a Markdown reply every so often.
    fn session(length: u64) -> Transcript {
        let mut t = Transcript::new();
        for i in 0..length {
            t.push(if i % 6 == 0 {
                message(i, Role::Assistant, REPLY)
            } else {
                tool(i, "read_file", &format!("src/file{i}.ts"))
            });
        }
        t
    }

    /// Draws the transcript as the App does: heights (cached), cull, draw the visible entries.
    fn draw(t: &mut Transcript, scroll: &mut ScrollView, terminal: &mut Terminal<TestBackend>) {
        terminal
            .draw(|f| {
                let area = f.area();
                let heights = t.heights(area.width, false);
                let shown = scroll.visible(&heights, area.height);
                let top = scroll.scroll_top();
                let mut row: usize = heights[..shown.start].iter().sum();
                for i in shown {
                    let welcome;
                    let lines = match &t.items[i] {
                        TranscriptItem::Welcome => {
                            welcome = welcome_lines(&WelcomeInfo::default(), FrameName::Idle, area.width);
                            &welcome[..]
                        }
                        TranscriptItem::Message(_) => t.lines(i),
                    };
                    for (j, line) in lines.iter().enumerate() {
                        if let Some(y) = (row + j).checked_sub(top).filter(|y| *y < area.height as usize) {
                            f.buffer_mut().set_line(0, y as u16, line, area.width);
                        }
                    }
                    row += heights[i];
                }
            })
            .unwrap();
    }

    fn screen(terminal: &Terminal<TestBackend>) -> String {
        let buf = terminal.backend().buffer();
        (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>().trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_keystroke_stays_fast_however_long_the_transcript_gets() {
        // Before off-screen messages were skipped, 600 entries took ~850-2400 ms per keystroke in Marv. Here each
        // entry is laid out once, and a frame draws only the entries in the viewport.
        let mut t = session(600);
        let mut scroll = ScrollView::default();
        let mut terminal = Terminal::new(TestBackend::new(120, 45)).unwrap();
        draw(&mut t, &mut scroll, &mut terminal); // the first frame lays everything out
        let mut times = Vec::new();
        for _ in 0..7 {
            let t0 = Instant::now();
            draw(&mut t, &mut scroll, &mut terminal);
            times.push(t0.elapsed());
        }
        times.sort();
        assert!(times[3] < Duration::from_millis(50), "median frame {:?}", times[3]);
        assert!(screen(&terminal).contains("read_file src/file599.ts"));
    }

    #[test]
    fn only_changed_entries_are_laid_out_again() {
        let mut t = session(10);
        t.heights(80, false);
        let before = t.cache.clone();
        t.message_mut(3).unwrap().tool.as_mut().unwrap().summary = Some("done".into());
        assert!(t.cache[4].is_none(), "the changed entry (index 4: the welcome banner is first) is dropped");
        t.heights(80, false);
        for (i, (a, b)) in before.iter().zip(&t.cache).enumerate() {
            let same = a.as_ref().map(|c| &c.lines) == b.as_ref().map(|c| &c.lines);
            assert_eq!(same, i != 4, "entry {i}");
        }
        assert!(t.message_mut(99).is_none());
    }

    #[test]
    fn ctrl_o_lays_out_again_only_the_entries_with_details() {
        let mut t = session(3);
        let diff = DiffShown {
            lines: vec![DiffLine { kind: DiffKind::Add, text: "x".into(), old_line: None, new_line: Some(1) }],
            more: 0,
        };
        let mut edit = tool(7, "edit_file", "a.ts");
        edit.tool.as_mut().unwrap().diff = Some(diff);
        t.push(edit);
        t.heights(80, false);
        t.heights(80, true);
        let flags: Vec<bool> = t.cache.iter().map(|c| c.as_ref().is_some_and(|c| c.show_steps)).collect();
        assert_eq!(flags, [false, false, false, false, true]);
    }

    #[test]
    fn subagent_entries_know_their_rows() {
        let mut t = Transcript::new();
        t.push(message(1, Role::User, "go"));
        t.push(tool(2, "agent", "Explore"));
        let heights = t.heights(80, false);
        let entries = t.agent_entries(&heights);
        let start = WELCOME_HEIGHT + 2;
        assert_eq!(entries, [(2, start..start + 3)]);
        assert_eq!(agent_entry_at(start + 1, &entries), Some(2));
        assert_eq!(agent_entry_at(start - 1, &entries), None);
    }

    #[test]
    fn clear_and_replace_keep_the_welcome_banner_first() {
        let mut t = session(4);
        t.clear();
        assert_eq!(t.items, [TranscriptItem::Welcome]);
        t.replace(vec![message(5, Role::User, "hi")]);
        assert_eq!(t.len(), 2);
        assert_eq!(t.messages().map(|m| m.id).collect::<Vec<_>>(), [5]);
    }
}
