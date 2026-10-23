//! Mouse text selection. Ports `src/selection.ts` (tests: `tests/selection.test.ts`).
//!
//! In the alternate screen with mouse reporting on, the terminal no longer selects text itself, so
//! Marv does: drag highlights, release copies (`clipboard`), a click or any key clears it.
//!
//! - The selection lives in *transcript* coordinates (row 1250 of the conversation, not row 12 of
//!   the screen), so it stays on the same text as the transcript scrolls, and dragging past the top
//!   or bottom auto-scrolls to extend it (one row per [`AUTO_SCROLL_MS`]).
//! - Only visible rows are ever drawn (viewport culling), so the store remembers the text of each
//!   transcript row as it passes through the viewport; that's what gets copied, the way a
//!   terminal keeps its scrollback. A resize (new width reflows, new height moves rows) or `/clear`
//!   resets it.
//! - Wide characters (CJK, emoji) take two cells; a mouse edge can land on the second half, so
//!   columns are widened to whole characters ([`whole_cells`]) or the copy would lose them.
//!
//! What changes in Rust: Marv patched Ink with a `transformOutput(frame)` hook (the store saw each
//! frame as an ANSI string and re-sliced it with slice-ansi) and a `repaint()` that re-applied the
//! highlight without a React render (~3 ms instead of ~30 ms). With ratatui the store works on the
//! drawn `Buffer` cells instead ([`SelectionStore::transform_buffer`]): it reads each visible row's
//! symbols and sets `Modifier::REVERSED` on the selected cells. Moving the highlight is a redraw of
//! an already-culled frame plus ratatui's cell diff, so the "repaint" path is just a normal draw,
//! and Marv's "one repaint per burst of mouse events" falls out of the app loop draining all input
//! before it draws. Auto-scroll is a deadline the app's loop wakes for, not a `setInterval`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::style::Modifier;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Auto-scroll step while dragging past an edge (Marv's default `autoScrollMs`).
pub const AUTO_SCROLL_MS: Duration = Duration::from_millis(50);

/// A cell: `x` is the column; `y` is a screen row, or a transcript row inside a [`Selection`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub x: u16,
    pub y: usize,
}

/// Where the drag started (anchor) and where the pointer is now (focus), in transcript rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: Point,
    pub focus: Point,
}

/// Where the transcript sits on screen and how far it's scrolled; published by its scroll view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Viewport {
    /// Screen row of the transcript's first visible row.
    pub top: u16,
    /// Visible rows.
    pub height: u16,
    /// Transcript row shown at `top`.
    pub scroll_top: usize,
    pub content_height: usize,
    pub width: u16,
}

fn ordered(selection: &Selection) -> (Point, Point) {
    let Selection { anchor, focus } = *selection;
    let anchor_first = anchor.y < focus.y || (anchor.y == focus.y && anchor.x <= focus.x);
    if anchor_first { (anchor, focus) } else { (focus, anchor) }
}

/// The columns `[from, to)` selected on transcript row `y`, or None. Like a terminal: the first
/// row runs to the end of the line, middle rows are whole, and the last row stops at the cell
/// under the pointer (inclusive). "To the end of the line" is `u16::MAX` (Marv's `Infinity`).
pub fn columns_on_row(selection: &Selection, y: usize) -> Option<(u16, u16)> {
    let (start, end) = ordered(selection);
    if y < start.y || y > end.y {
        return None;
    }
    Some((if y == start.y { start.x } else { 0 }, if y == end.y { end.x.saturating_add(1) } else { u16::MAX }))
}

/// The columns `[from, to)` widened to whole characters, clamped to the line's width. A wide
/// character (CJK, emoji) takes two cells, and a mouse edge can land on its second half; slicing
/// there would drop it from both sides (the line shrinks and shifts, and the copy loses it).
pub fn whole_cells(line: &str, (from, to): (u16, u16)) -> (u16, u16) {
    let width = line.width().min(u16::MAX as usize) as u16;
    let (mut start, mut end) = (from.min(width), to.min(width));
    let mut col: u16 = 0;
    for grapheme in line.graphemes(true) {
        let next = col.saturating_add(grapheme.width() as u16);
        if from > col && from < next {
            start = col; // `from` inside this character: start at its first cell
        }
        if to > col && to < next {
            end = next; // `to` inside it: take all of it
        }
        col = next;
    }
    (start, end)
}

/// The plain text of buffer row `y`: each cell's symbol, skipping the cells a wide symbol covers.
fn row_text(buf: &Buffer, y: u16) -> String {
    let area = buf.area;
    let mut text = String::new();
    let mut x = area.left();
    while x < area.right() {
        let symbol = buf[(x, y)].symbol();
        let symbol = if symbol.is_empty() { " " } else { symbol };
        text.push_str(symbol);
        x += (symbol.width() as u16).max(1);
    }
    text
}

/// The text of cells `[from, to)` of a plain line.
fn slice_cells(line: &str, from: u16, to: u16) -> String {
    let mut out = String::new();
    let mut col: u16 = 0;
    for grapheme in line.graphemes(true) {
        if col >= to {
            break;
        }
        if col >= from {
            out.push_str(grapheme);
        }
        col = col.saturating_add(grapheme.width() as u16);
    }
    out
}

/// The selection state shared by the app (mouse), the transcript's scroll view and the draw.
#[derive(Debug, Default)]
pub struct SelectionStore {
    selection: Option<Selection>,
    viewport: Option<Viewport>,
    /// Plain text of each transcript row, as last seen on screen.
    rows: HashMap<usize, String>,
    /// Dragging past an edge: the direction and when the next step is due.
    auto_scroll: Option<(i8, Instant)>,
}

/// What the store wants the app to do after a mouse event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionEffect {
    None,
    /// The highlight moved: redraw.
    Redraw,
    /// Scroll the transcript by this many rows (auto-scroll), then redraw.
    ScrollBy(i32),
    /// Released over a selection: copy this text.
    Copy(String),
}

impl SelectionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Called by the transcript's scroll view on every draw. A new width or height resets the
    /// remembered rows (they no longer line up): a new width reflows the text and a new height
    /// moves rows around on screen.
    pub fn set_viewport(&mut self, viewport: Viewport) {
        if let Some(previous) = self.viewport
            && (previous.width != viewport.width || previous.height != viewport.height)
        {
            self.reset();
        }
        self.viewport = Some(viewport);
    }

    /// No transcript on screen (setup, the splash): clicks and drags select nothing.
    pub fn clear_viewport(&mut self) {
        self.viewport = None;
    }

    /// The current selection, if any.
    pub fn selection(&self) -> Option<Selection> {
        self.selection
    }

    /// After the widgets drew a frame: remember the visible rows' text and reverse the selected cells.
    pub fn transform_buffer(&mut self, buf: &mut Buffer) {
        let Some(vp) = self.viewport else { return };
        let area = buf.area;
        let bottom = vp.top.saturating_add(vp.height).min(area.bottom());
        for y in vp.top.max(area.top())..bottom {
            let row = (y - vp.top) as usize + vp.scroll_top;
            let text = row_text(buf, y);
            if let Some(selection) = self.selection
                && let Some(columns) = columns_on_row(&selection, row)
            {
                let (start, end) = whole_cells(&text, columns);
                for x in start..end {
                    let x = area.left().saturating_add(x);
                    if x < area.right() {
                        let cell = &mut buf[(x, y)];
                        cell.modifier.insert(Modifier::REVERSED);
                    }
                }
            }
            self.rows.insert(row, text);
        }
    }

    /// The transcript row under a screen point, or None outside the transcript (for clicks).
    pub fn row_at(&self, x: u16, y: u16) -> Option<usize> {
        self.to_transcript(x, y, false).map(|p| p.y)
    }

    /// Mouse down: start a selection if it's on the transcript.
    pub fn press(&mut self, x: u16, y: u16) -> SelectionEffect {
        self.auto_scroll = None;
        let at = self.to_transcript(x, y, false);
        self.set(at.map(|at| Selection { anchor: at, focus: at }))
    }

    /// Mouse moved with the button held: extend, and start auto-scroll while past an edge.
    pub fn drag(&mut self, x: u16, y: u16, now: Instant) -> SelectionEffect {
        let (Some(selection), Some(vp)) = (self.selection, self.viewport) else { return SelectionEffect::None };
        // A transcript at the top of the screen (the main view) has no row above it to drag into, so its top row is
        // the edge for a drag that started below it, while there's more above (#214: a pasted message taller than
        // the window couldn't be selected upwards past what was shown). A drag within the top row still selects.
        let at_top_edge = vp.top == 0 && y == 0 && vp.scroll_top > 0 && selection.anchor.y > vp.scroll_top;
        let direction: i8 = if y < vp.top || at_top_edge {
            -1
        } else if y >= vp.top.saturating_add(vp.height) {
            1
        } else {
            0
        };
        if direction == 0 {
            self.auto_scroll = None;
            let focus = self.to_transcript(x, y, true).unwrap_or(selection.focus);
            self.set(Some(Selection { anchor: selection.anchor, focus }))
        } else if self.auto_scroll.map(|(d, _)| d) != Some(direction) {
            self.auto_scroll = Some((direction, now + AUTO_SCROLL_MS));
            self.scroll_step(direction)
        } else {
            SelectionEffect::None
        }
    }

    /// Mouse up: the selected text to copy (nothing for a plain click). The highlight stays.
    pub fn release(&mut self) -> SelectionEffect {
        self.auto_scroll = None;
        let Some(selection) = self.selection.filter(|s| s.anchor != s.focus) else {
            return self.set(None);
        };
        let (start, end) = ordered(&selection);
        let mut text = Vec::new();
        for y in start.y..=end.y {
            let Some(row) = self.rows.get(&y) else { continue };
            let Some(columns) = columns_on_row(&selection, y) else { continue };
            let (from, to) = whole_cells(row, columns);
            text.push(slice_cells(row, from, to).trim_end().to_string());
        }
        SelectionEffect::Copy(text.join("\n"))
    }

    /// When the next auto-scroll step is due (the app's loop wakes for it).
    pub fn deadline(&self) -> Option<Instant> {
        self.auto_scroll.map(|(_, at)| at)
    }

    /// An auto-scroll step, if one is due: scrolls one row past the edge and extends onto it.
    pub fn tick(&mut self, now: Instant) -> SelectionEffect {
        match self.auto_scroll {
            Some((direction, at)) if now >= at => {
                // From now, not from `at`: a late wake shouldn't replay the missed steps.
                self.auto_scroll = Some((direction, now + AUTO_SCROLL_MS));
                self.scroll_step(direction)
            }
            _ => SelectionEffect::None,
        }
    }

    /// A key or a click elsewhere.
    pub fn clear(&mut self) -> SelectionEffect {
        self.auto_scroll = None;
        if self.selection.is_some() { self.set(None) } else { SelectionEffect::None }
    }

    /// Forgets the selection and the remembered rows (after `/clear` or a resize).
    pub fn reset(&mut self) {
        self.auto_scroll = None;
        self.rows.clear();
        self.selection = None;
    }

    /// Scrolls one row past the edge and extends the selection onto the row coming into view.
    fn scroll_step(&mut self, direction: i8) -> SelectionEffect {
        let (Some(vp), Some(selection)) = (self.viewport, self.selection) else {
            self.auto_scroll = None;
            return SelectionEffect::None;
        };
        // Nothing more past the edge: extend to the edge row and stop, or the loop would wake every step for nothing.
        let at_end = if direction < 0 { vp.scroll_top == 0 } else { vp.scroll_top + vp.height as usize >= vp.content_height };
        if at_end {
            self.auto_scroll = None;
            let focus = if direction < 0 {
                Point { x: 0, y: vp.scroll_top }
            } else {
                Point { x: vp.width.saturating_sub(1), y: vp.content_height.saturating_sub(1) }
            };
            return self.set(Some(Selection { anchor: selection.anchor, focus }));
        }
        let row = if direction < 0 {
            vp.scroll_top.saturating_sub(1)
        } else {
            (vp.scroll_top + vp.height as usize).min(vp.content_height.saturating_sub(1))
        };
        let x = if direction < 0 { 0 } else { vp.width.saturating_sub(1) };
        self.selection = Some(Selection { anchor: selection.anchor, focus: Point { x, y: row } });
        SelectionEffect::ScrollBy(direction as i32)
    }

    /// Screen point → transcript point. Outside the transcript: None, or clamped to its edge rows.
    fn to_transcript(&self, x: u16, y: u16, clamp: bool) -> Option<Point> {
        let vp = self.viewport?;
        if vp.height == 0 {
            return None;
        }
        let bottom = vp.top + vp.height;
        let y = if y < vp.top || y >= bottom {
            if !clamp {
                return None;
            }
            y.clamp(vp.top, bottom - 1)
        } else {
            y
        };
        Some(Point { x, y: (y - vp.top) as usize + vp.scroll_top })
    }

    fn set(&mut self, next: Option<Selection>) -> SelectionEffect {
        let changed = self.selection != next;
        self.selection = next;
        if changed { SelectionEffect::Redraw } else { SelectionEffect::None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Style};

    /// A transcript of numbered lines.
    fn transcript() -> Vec<String> {
        (0..30).map(|i| format!("line {i}")).collect()
    }

    /// The screen shows `height` transcript rows from `scroll_top` (from screen row `top`), then a prompt line.
    fn screen_for(store: &mut SelectionStore, scroll_top: usize, height: u16, top: u16) -> Buffer {
        store.set_viewport(Viewport { top, height, scroll_top, content_height: 30, width: 40 });
        let t = transcript();
        let mut lines: Vec<String> = (0..top).map(|_| String::new()).collect();
        lines.extend(t[scroll_top..scroll_top + height as usize].iter().cloned());
        lines.push("> prompt".into());
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, lines.len() as u16));
        for (y, line) in lines.iter().enumerate() {
            buf.set_string(0, y as u16, line, Style::new());
        }
        buf
    }

    fn draw(store: &mut SelectionStore, scroll_top: usize) -> Buffer {
        let mut buf = screen_for(store, scroll_top, 5, 0);
        store.transform_buffer(&mut buf);
        buf
    }

    fn reversed(buf: &Buffer, x: u16, y: u16) -> bool {
        buf[(x, y)].modifier.contains(Modifier::REVERSED)
    }

    /// The rows that have a highlighted cell, by their plain text.
    fn highlighted(buf: &Buffer) -> Vec<String> {
        (0..buf.area.height)
            .filter(|&y| (0..buf.area.width).any(|x| reversed(buf, x, y)))
            .map(|y| row_text(buf, y).trim_end().to_string())
            .collect()
    }

    fn one_row(line: &str, style: Style) -> (SelectionStore, Buffer) {
        let mut store = SelectionStore::new();
        store.set_viewport(Viewport { top: 0, height: 1, scroll_top: 0, content_height: 1, width: 40 });
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 2));
        buf.set_string(0, 0, line, style);
        buf.set_string(0, 1, "> prompt", Style::new());
        (store, buf)
    }

    /// The highlighted cells of row 0, as text.
    fn highlighted_text(buf: &Buffer) -> String {
        let text = row_text(buf, 0);
        let cells: Vec<u16> = (0..buf.area.width).filter(|&x| reversed(buf, x, 0)).collect();
        match (cells.first(), cells.last()) {
            (Some(&a), Some(&b)) => slice_cells(&text, a, b + 1),
            _ => String::new(),
        }
    }

    // --- highlightRow (Marv) → transform_buffer over cells ---

    #[test]
    fn inverts_only_the_given_columns_and_keeps_the_original_colors_around_them() {
        let red = Style::new().fg(Color::Red);
        let (mut store, mut buf) = one_row("abcdef", red);
        store.press(2, 0);
        store.drag(3, 0, Instant::now());
        store.transform_buffer(&mut buf);
        assert_eq!(row_text(&buf, 0).trim_end(), "abcdef");
        assert_eq!(highlighted_text(&buf), "cd");
        assert!(!reversed(&buf, 1, 0) && !reversed(&buf, 4, 0));
        assert_eq!(buf[(0, 0)].fg, Color::Red);
        assert_eq!(buf[(2, 0)].fg, Color::Red);
    }

    #[test]
    fn an_edge_inside_a_wide_character_takes_the_whole_character() {
        let line = "ab中文cd😀ef";
        for (from, to) in [(2, 3), (3, 5), (8, 9), (3, 4)] {
            let (s, e) = whole_cells(line, (from, to));
            assert!(s <= from && e >= to.min(line.width() as u16), "{from}..{to} → {s}..{e}");
        }
        assert_eq!(whole_cells(line, (3, 5)), (2, 6));
        let (mut store, mut buf) = one_row(line, Style::new());
        store.press(3, 0); // second half of 中
        store.drag(4, 0, Instant::now()); // first half of 文
        store.transform_buffer(&mut buf);
        assert_eq!(row_text(&buf, 0).trim_end(), line); // nothing lost, the rest of the line doesn't shift
        assert_eq!(highlighted_text(&buf), "中文");
    }

    #[test]
    fn counts_wide_characters_by_their_screen_width() {
        assert_eq!(whole_cells("● 日本 ok", (2, 6)), (2, 6));
        let (mut store, mut buf) = one_row("● 日本 ok", Style::new());
        store.press(2, 0);
        store.drag(5, 0, Instant::now());
        store.transform_buffer(&mut buf);
        assert_eq!(highlighted_text(&buf), "日本");
    }

    #[test]
    fn columns_past_the_end_are_clamped() {
        assert_eq!(whole_cells("abc", (1, u16::MAX)), (1, 3));
        assert_eq!(whole_cells("", (0, 4)), (0, 0));
    }

    // --- SelectionStore ---

    #[test]
    fn drag_selects_like_a_terminal_and_copies_the_text() {
        let mut store = SelectionStore::new();
        draw(&mut store, 0);
        store.press(5, 1); // "line 1", from the "1"
        store.drag(3, 3, Instant::now()); // to "line" of "line 3"
        assert_eq!(highlighted(&draw(&mut store, 0)), ["line 1", "line 2", "line 3"]);
        assert_eq!(store.release(), SelectionEffect::Copy("1\nline 2\nline".into()));
    }

    #[test]
    fn copying_across_wide_characters_copies_whole_characters() {
        let (mut store, mut buf) = one_row("ab中文cd😀ef", Style::new());
        store.transform_buffer(&mut buf);
        store.press(3, 0); // the second half of 中
        store.drag(4, 0, Instant::now()); // the first half of 文
        assert_eq!(store.release(), SelectionEffect::Copy("中文".into()));
    }

    #[test]
    fn the_highlight_stays_on_the_same_text_when_the_transcript_scrolls() {
        let mut store = SelectionStore::new();
        draw(&mut store, 10);
        store.press(0, 0);
        store.drag(5, 0, Instant::now()); // "line 10"
        let after = draw(&mut store, 8); // scrolled up 2 rows: "line 10" is now on screen row 2
        assert!(reversed(&after, 0, 2));
        assert_eq!(highlighted(&after), ["line 10"]);
    }

    #[test]
    fn copies_rows_that_have_scrolled_out_of_view_from_what_was_seen() {
        let mut store = SelectionStore::new();
        draw(&mut store, 0); // rows 0-4 seen
        store.press(0, 1);
        draw(&mut store, 5); // scrolled: rows 5-9 seen, row 1 now off screen
        store.drag(5, 2, Instant::now()); // to "line 7"
        assert_eq!(store.release(), SelectionEffect::Copy("line 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7".into()));
    }

    #[test]
    fn dragging_below_the_transcript_auto_scrolls_and_extends_the_selection() {
        let mut store = SelectionStore::new();
        let t0 = Instant::now();
        draw(&mut store, 0);
        store.press(0, 3);
        // The prompt row, just below the 5-row transcript: one step at once, then one per AUTO_SCROLL_MS.
        assert_eq!(store.drag(2, 5, t0), SelectionEffect::ScrollBy(1));
        assert_eq!(store.deadline(), Some(t0 + AUTO_SCROLL_MS));
        assert_eq!(store.tick(t0 + Duration::from_millis(10)), SelectionEffect::None); // not due yet
        assert_eq!(store.tick(t0 + AUTO_SCROLL_MS), SelectionEffect::ScrollBy(1));
        assert_eq!(store.selection().unwrap().focus, Point { x: 39, y: 5 });
        // A further drag in the same direction doesn't restart it.
        assert_eq!(store.drag(3, 6, t0 + AUTO_SCROLL_MS), SelectionEffect::None);

        // Back inside: auto-scroll stops.
        assert_eq!(store.drag(2, 4, t0 + AUTO_SCROLL_MS), SelectionEffect::Redraw);
        assert_eq!(store.deadline(), None);
        assert_eq!(store.tick(t0 + 10 * AUTO_SCROLL_MS), SelectionEffect::None);
        store.release();
    }

    #[test]
    fn dragging_above_the_transcript_scrolls_up() {
        let mut store = SelectionStore::new();
        let mut buf = screen_for(&mut store, 10, 5, 2);
        store.transform_buffer(&mut buf);
        store.press(0, 4);
        assert_eq!(store.drag(0, 0, Instant::now()), SelectionEffect::ScrollBy(-1)); // above the transcript's first row
        assert_eq!(store.selection().unwrap().focus, Point { x: 0, y: 9 });
        // Row 9 was never seen on screen, so the copy starts at row 10.
        assert_eq!(store.release(), SelectionEffect::Copy("line 10\nline 11\nl".into()));
    }

    // #214: the main transcript starts at the screen's first row, so nothing lies above it to drag into. Its top row
    // scrolls up while there's more above, or a message taller than the window can't be selected upwards.
    #[test]
    fn dragging_onto_the_top_row_of_a_transcript_at_the_screen_top_scrolls_up_while_theres_more() {
        let mut store = SelectionStore::new();
        let t0 = Instant::now();
        draw(&mut store, 10);
        store.press(0, 3);
        assert_eq!(store.drag(2, 0, t0), SelectionEffect::ScrollBy(-1));
        assert_eq!(store.selection().unwrap().focus, Point { x: 0, y: 9 });
        assert_eq!(store.tick(t0 + AUTO_SCROLL_MS), SelectionEffect::ScrollBy(-1));
        store.release();
        // A drag that starts on the top row selects on it.
        draw(&mut store, 10);
        store.press(0, 0);
        assert_eq!(store.drag(4, 0, t0), SelectionEffect::Redraw);
        store.release();
        // Scrolled to the very top: the top row is just a row.
        draw(&mut store, 0);
        store.press(0, 3);
        assert_eq!(store.drag(2, 0, t0), SelectionEffect::Redraw);
        assert_eq!(store.deadline(), None);
    }

    // #214 review: auto-scroll stops once there's nothing more past the edge, instead of waking every step.
    #[test]
    fn auto_scroll_stops_at_the_top_and_the_bottom() {
        let mut store = SelectionStore::new();
        let t0 = Instant::now();
        // Above the view (a subagent's header row above it): one row left above, then the top.
        let mut buf = screen_for(&mut store, 1, 5, 2);
        store.transform_buffer(&mut buf);
        store.press(0, 4);
        assert_eq!(store.drag(0, 0, t0), SelectionEffect::ScrollBy(-1));
        screen_for(&mut store, 0, 5, 2);
        store.tick(t0 + AUTO_SCROLL_MS); // already on row 0: nothing to redraw
        assert_eq!(store.selection().unwrap().focus, Point { x: 0, y: 0 });
        assert_eq!(store.deadline(), None);
        assert_eq!(store.tick(t0 + 5 * AUTO_SCROLL_MS), SelectionEffect::None);
        store.release();
        // The main view's top row, scrolled to the top while dragging.
        draw(&mut store, 1);
        store.press(0, 3);
        assert_eq!(store.drag(0, 0, t0), SelectionEffect::ScrollBy(-1));
        draw(&mut store, 0);
        store.tick(t0 + AUTO_SCROLL_MS);
        assert_eq!(store.deadline(), None);
        store.release();
        // Below, at the end of the transcript.
        draw(&mut store, 25);
        store.press(0, 1);
        assert_eq!(store.drag(0, 5, t0), SelectionEffect::Redraw);
        assert_eq!(store.selection().unwrap().focus, Point { x: 39, y: 29 });
        assert_eq!(store.deadline(), None);
    }

    #[test]
    fn a_click_without_a_drag_selects_nothing_and_a_click_outside_the_transcript_is_ignored() {
        let mut store = SelectionStore::new();
        draw(&mut store, 0);
        store.press(3, 1);
        assert!(!matches!(store.release(), SelectionEffect::Copy(_)));
        assert_eq!(store.selection(), None);
        assert_eq!(store.press(3, 5), SelectionEffect::None); // the prompt row
        assert_eq!(store.drag(8, 5, Instant::now()), SelectionEffect::None);
        assert!(!matches!(store.release(), SelectionEffect::Copy(_)));
        assert_eq!(store.row_at(3, 5), None);
        assert_eq!(store.row_at(3, 1), Some(1));
    }

    #[test]
    fn a_resize_forgets_the_remembered_rows_and_the_selection() {
        // (the text reflows)
        let mut store = SelectionStore::new();
        draw(&mut store, 0);
        store.press(0, 0);
        store.drag(4, 0, Instant::now());
        store.set_viewport(Viewport { top: 0, height: 5, scroll_top: 0, content_height: 30, width: 60 });
        assert!(!matches!(store.release(), SelectionEffect::Copy(_)));
    }

    #[test]
    fn reset_forgets_everything() {
        // (used by /clear)
        let mut store = SelectionStore::new();
        draw(&mut store, 0);
        store.press(0, 0);
        store.drag(4, 0, Instant::now());
        store.reset();
        assert!(!matches!(store.release(), SelectionEffect::Copy(_)));
    }

    #[test]
    fn clear_asks_for_a_redraw_only_when_something_was_selected() {
        let mut store = SelectionStore::new();
        draw(&mut store, 0);
        assert_eq!(store.clear(), SelectionEffect::None);
        store.press(0, 0);
        store.drag(4, 0, Instant::now());
        assert_eq!(store.clear(), SelectionEffect::Redraw);
        assert!(highlighted(&draw(&mut store, 0)).is_empty());
    }
}
