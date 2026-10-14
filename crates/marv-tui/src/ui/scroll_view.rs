//! The transcript's own scrolling. Ports `src/ui/ScrollView.tsx` (tests: `tests/scrollview.test.tsx`,
//! `tests/render-performance.test.tsx`).
//!
//! The alternate screen has no terminal scrollback, so Marv scrolls its own transcript. The
//! viewport fills the height its parent leaves and clips the content.
//! - It follows the bottom (like a chat) until you press PgUp or scroll up; then it stays where you
//!   scrolled, even while a reply streams in, until you scroll back down or `follow()` (on submit).
//! - Clamped on every draw: the maximum changes as content grows or the terminal resizes. A page
//!   keeps one row of overlap.
//! - Wheel scrolling glides: notches add to a distance still to travel, and each [`GLIDE_MS`] step
//!   covers a third of what's left (rounded up, at least a row); one notch moves 1+1+1 rows instead
//!   of jumping 3; reversing direction drops the rest. 20 ms, just above the 60 fps frame interval,
//!   so every step gets its own frame (16 ms beat against Ink's limiter and stuttered). Here the
//!   glide is a deadline for the app's loop, not a `setTimeout` chain, and it stops at either end
//!   (nothing left to move).
//! - Hidden (a subagent's view is open), it keeps its position and stops any glide. It isn't
//!   measured while hidden, so it keeps its last measurements, as Marv's did (a hidden Ink box
//!   measured zero, and clamping to that drew it scrolled to the top).
//!
//! **Viewport culling** (the Rust form of Marv's Ink patch, which skipped drawing nodes outside the
//! clipped rows: ~270 ms → ~30 ms per keystroke on a 60-entry session): [`ScrollView::visible`]
//! takes each entry's height at the current width (cached by the transcript), finds the entries
//! intersecting `[scroll_top, scroll_top + height)`, and only those are drawn. The App publishes
//! its [`Viewport`] to the selection store each draw.

use std::ops::Range;
use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;

use crate::selection::Viewport;

/// One glide step every 20 ms (50/s). Just above the frame interval (60/s = 16.7 ms) so every step
/// gets its own frame: at 16 ms the two timers beat against each other and frames came 4 ms, then
/// 50 ms apart (visible stutter).
pub const GLIDE_MS: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, Default)]
pub struct ScrollView {
    /// None = following the bottom; otherwise the row pinned to the top.
    pub top: Option<usize>,
    /// Glide distance still to travel, and when the next step is due.
    pub glide_remaining: i32,
    pub glide_at: Option<Instant>,
    pub hidden: bool,
    /// Last measured content and viewport height (kept while hidden).
    pub content_height: usize,
    pub viewport_height: u16,
}

impl ScrollView {
    /// The row shown at the top, clamped.
    pub fn scroll_top(&self) -> usize {
        let max = self.max_top();
        self.top.map_or(max, |t| t.min(max))
    }

    fn max_top(&self) -> usize {
        self.content_height.saturating_sub(self.viewport_height as usize)
    }

    /// Back to following the bottom (Marv's `followKey` change).
    pub fn follow(&mut self) {
        self.top = None;
    }

    /// Takes the content's and the viewport's height for this draw.
    pub fn measure(&mut self, content_height: usize, viewport_height: u16) {
        self.content_height = content_height;
        self.viewport_height = viewport_height;
    }

    /// Scrolls by `rows` from where it is now; reaching the bottom resumes following.
    pub fn scroll_by(&mut self, rows: i32) {
        let max = self.max_top();
        let next = (self.scroll_top() as i64 + rows as i64).clamp(0, max as i64) as usize;
        self.top = if next >= max { None } else { Some(next) };
    }

    /// A page keeps one row of overlap so you don't lose your place.
    fn page(&self) -> i32 {
        (self.viewport_height as i32 - 1).max(1)
    }

    pub fn page_up(&mut self) {
        self.scroll_by(-self.page());
    }

    pub fn page_down(&mut self) {
        self.scroll_by(self.page());
    }

    /// A wheel notch, in rows (`mouse::WHEEL_ROWS` × step). Notches add up; reversing drops what's
    /// left; the first row moves at once.
    pub fn glide_by(&mut self, rows: i32, now: Instant) {
        if self.hidden {
            return;
        }
        if rows.signum() != self.glide_remaining.signum() {
            self.glide_remaining = 0;
        }
        self.glide_remaining += rows;
        if self.glide_at.is_none() {
            self.glide_at = Some(now);
            self.tick(now);
        }
    }

    /// Advances the glide if due (a third of what's left, rounded up); true if it scrolled.
    pub fn tick(&mut self, now: Instant) -> bool {
        let Some(at) = self.glide_at else { return false };
        if now < at {
            return false;
        }
        if self.glide_remaining == 0 {
            self.glide_at = None;
            return false;
        }
        let left = self.glide_remaining;
        let step = left.signum() * ((left.abs() + 2) / 3).max(1);
        self.glide_remaining -= step;
        let before = self.scroll_top();
        self.scroll_by(step);
        self.glide_at = (self.glide_remaining != 0).then_some(now + GLIDE_MS);
        if self.scroll_top() == before {
            // At an end: nothing more to glide.
            self.stop_glide();
            return false;
        }
        true
    }

    fn stop_glide(&mut self) {
        self.glide_remaining = 0;
        self.glide_at = None;
    }

    /// Hidden (a subagent's view opening) or shown again. Hiding stops a glide still on its way.
    pub fn set_hidden(&mut self, hidden: bool) {
        if hidden {
            self.stop_glide();
        }
        self.hidden = hidden;
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.glide_at
    }

    /// Measures the content from each entry's height and returns the entries (by index) that
    /// intersect the viewport: the only ones to draw.
    pub fn visible(&mut self, heights: &[usize], viewport_height: u16) -> Range<usize> {
        self.measure(heights.iter().sum(), viewport_height);
        let top = self.scroll_top();
        let bottom = top + viewport_height as usize;
        let mut row = 0;
        let mut first = heights.len();
        let mut last = heights.len();
        for (i, &height) in heights.iter().enumerate() {
            let end = row + height;
            if first == heights.len() && end > top {
                first = i;
            }
            if row >= bottom {
                last = i;
                break;
            }
            row = end;
        }
        first.min(last)..last
    }

    /// Draws already laid-out lines (the subagent's view) into `area`; returns the viewport.
    pub fn draw(&mut self, area: Rect, buf: &mut Buffer, lines: &[Line<'static>]) -> Viewport {
        self.measure(lines.len(), area.height);
        let top = self.scroll_top();
        for (i, line) in lines.iter().skip(top).take(area.height as usize).enumerate() {
            buf.set_line(area.x, area.y + i as u16, line, area.width);
        }
        self.viewport(area.y, area.width)
    }

    pub fn viewport(&self, top: u16, width: u16) -> Viewport {
        Viewport { top, height: self.viewport_height, scroll_top: self.scroll_top(), content_height: self.content_height, width }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::{Terminal, widgets::Widget};

    /// `count` one-row entries "line N" in a viewport `height` rows tall, drawn through ratatui's
    /// `TestBackend`, culled by [`ScrollView::visible`]; returns the visible rows.
    fn frame(scroll: &mut ScrollView, count: usize, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(20, height)).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                let heights = vec![1; count];
                let shown = scroll.visible(&heights, area.height);
                let top = scroll.scroll_top();
                for i in shown {
                    Line::raw(format!("line {}", i + 1)).render(Rect { y: (i - top) as u16, height: 1, ..area }, f.buffer_mut());
                }
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..20).map(|x| buf[(x, y)].symbol()).collect::<String>().trim_end().to_string())
            .filter(|l| !l.is_empty())
            .collect()
    }

    /// Runs the glide for `ms`, a tick every millisecond (the loop wakes at the deadline).
    fn glide_for(scroll: &mut ScrollView, from: Instant, ms: u64, mut each: impl FnMut(&ScrollView)) -> Instant {
        for t in 0..=ms {
            if scroll.tick(from + Duration::from_millis(t)) {
                each(scroll);
            }
        }
        from + Duration::from_millis(ms)
    }

    #[test]
    fn follows_the_bottom_until_pinned_and_clamps() {
        let mut s = ScrollView { content_height: 100, viewport_height: 20, ..Default::default() };
        assert_eq!(s.scroll_top(), 80);
        s.top = Some(500);
        assert_eq!(s.scroll_top(), 80);
        s.top = Some(10);
        assert_eq!(s.scroll_top(), 10);
        s.follow();
        assert_eq!(s.scroll_top(), 80);
    }

    #[test]
    fn starts_at_the_bottom_showing_the_newest_lines() {
        let mut s = ScrollView::default();
        assert_eq!(frame(&mut s, 10, 4), ["line 7", "line 8", "line 9", "line 10"]);
    }

    #[test]
    fn keeps_following_the_bottom_as_content_grows() {
        let mut s = ScrollView::default();
        frame(&mut s, 10, 4);
        assert_eq!(frame(&mut s, 12, 4).last().unwrap(), "line 12");
    }

    #[test]
    fn page_up_scrolls_back_page_down_returns_to_the_bottom() {
        let mut s = ScrollView::default();
        frame(&mut s, 10, 4);
        s.page_up();
        assert_eq!(frame(&mut s, 10, 4)[0], "line 4");
        s.page_down();
        assert_eq!(frame(&mut s, 10, 4).last().unwrap(), "line 10");
        assert_eq!(s.top, None, "back to following");
    }

    #[test]
    fn stays_put_while_scrolled_up_even_as_content_grows() {
        let mut s = ScrollView::default();
        frame(&mut s, 10, 4);
        s.page_up();
        frame(&mut s, 10, 4);
        assert_eq!(frame(&mut s, 12, 4)[0], "line 4");
    }

    #[test]
    fn the_mouse_wheel_scrolls_3_rows_per_notch_gliding_one_row_per_frame() {
        let t = Instant::now();
        let mut s = ScrollView::default();
        frame(&mut s, 10, 4);
        let mut tops = vec![];
        s.glide_by(-3, t);
        tops.push(s.scroll_top());
        assert_eq!(s.deadline(), Some(t + GLIDE_MS));
        let t = glide_for(&mut s, t, 120, |s| tops.push(s.scroll_top()));
        assert_eq!(frame(&mut s, 10, 4)[0], "line 4");
        // It passed through the rows in between instead of jumping.
        assert_eq!(tops, [5, 4, 3]);
        assert_eq!(s.deadline(), None, "nothing left: the loop sleeps");

        s.glide_by(3, t);
        glide_for(&mut s, t, 120, |_| {});
        assert_eq!(frame(&mut s, 10, 4).last().unwrap(), "line 10");
    }

    #[test]
    fn a_fast_flick_adds_up_and_the_glide_catches_up_quickly() {
        let t = Instant::now();
        let mut s = ScrollView::default();
        frame(&mut s, 60, 4);
        for _ in 0..6 {
            s.glide_by(-3, t); // 18 rows
        }
        glide_for(&mut s, t, 200, |_| {});
        assert_eq!(frame(&mut s, 60, 4)[0], "line 39"); // bottom was lines 57-60; 18 rows up
    }

    #[test]
    fn a_direction_change_cancels_the_rest_of_the_glide() {
        let t = Instant::now();
        let mut s = ScrollView::default();
        frame(&mut s, 60, 4);
        for _ in 0..6 {
            s.glide_by(-3, t);
        }
        s.glide_by(3, t); // changed my mind
        glide_for(&mut s, t, 200, |_| {});
        // Only the 3 rows of the last notch remain to travel, back toward the bottom: it ends at the bottom.
        assert_eq!(frame(&mut s, 60, 4).last().unwrap(), "line 60");
    }

    #[test]
    fn follow_jumps_back_to_the_bottom() {
        let mut s = ScrollView::default();
        frame(&mut s, 10, 4);
        s.page_up();
        s.follow();
        assert_eq!(frame(&mut s, 10, 4).last().unwrap(), "line 10");
    }

    #[test]
    fn hidden_and_shown_again_its_where_it_was_and_a_glide_stops() {
        let t = Instant::now();
        let mut s = ScrollView::default();
        frame(&mut s, 30, 4);
        for _ in 0..5 {
            s.page_up();
        }
        assert_eq!(frame(&mut s, 30, 4)[0], "line 12");
        s.glide_by(-3, t); // a wheel glide still going when the view opens
        s.set_hidden(true);
        assert_eq!(s.deadline(), None);
        glide_for(&mut s, t, 150, |_| panic!("a hidden view doesn't glide"));
        s.glide_by(-3, t);
        assert_eq!(s.deadline(), None, "nor scrolls by the wheel");
        s.set_hidden(false);
        // Never the top, not snapped to the bottom: the row it was on (the glide's first row had moved).
        assert_eq!(frame(&mut s, 30, 4)[0], "line 11");
    }

    #[test]
    fn only_entries_in_the_viewport_are_visible() {
        let mut s = ScrollView::default();
        // Entries of 5, 3, 4, 2 and 6 rows: 20 in all; a 6-row viewport at the bottom shows rows 14..20.
        let heights = [5, 3, 4, 2, 6];
        assert_eq!(s.visible(&heights, 6), 4..5);
        s.top = Some(4);
        // Rows 4..10: the first entry's last row, all of the second, the third's first two.
        assert_eq!(s.visible(&heights, 6), 0..3);
        s.top = Some(0);
        assert_eq!(s.visible(&[], 6), 0..0);
        assert_eq!(s.visible(&[0, 0, 2], 6), 2..3);
        assert_eq!(s.content_height, 2);
    }

    #[test]
    fn draws_laid_out_lines_clipped_and_reports_its_viewport() {
        let mut s = ScrollView::default();
        let lines: Vec<Line<'static>> = (1..=10).map(|i| Line::raw(format!("row {i}"))).collect();
        let area = Rect::new(0, 2, 10, 3);
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 6));
        let viewport = s.draw(area, &mut buf, &lines);
        assert_eq!(viewport, Viewport { top: 2, height: 3, scroll_top: 7, content_height: 10, width: 10 });
        assert_eq!((0..6).map(|x| buf[(x, 2)].symbol()).collect::<String>(), "row 8 ");
        assert_eq!(buf[(0, 1)].symbol(), " ");
    }
}
