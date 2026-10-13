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
//!   covers a third of what's left (at least a row); one notch moves 1+1+1 rows instead of jumping
//!   3; reversing direction drops the rest. 20 ms, just above the 60 fps frame interval, so every
//!   step gets its own frame (16 ms beat against Ink's limiter and stuttered). Here the glide is a
//!   deadline for the app's loop, not a `setTimeout` chain.
//! - Hidden (a subagent's view is open), it keeps its position and stops any glide.
//!
//! **Viewport culling** (the Rust form of Marv's Ink patch, which skipped drawing nodes outside the
//! clipped rows: ~270 ms → ~30 ms per keystroke on a 60-entry session): [`ScrollView`] keeps each
//! entry's height at the current width, finds the entries intersecting
//! `[scroll_top, scroll_top + height)` by prefix sums, and only those are rendered. It publishes
//! its [`Viewport`] to the selection store each draw.

use std::time::{Duration, Instant};

use crate::selection::Viewport;

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
        let max = self.content_height.saturating_sub(self.viewport_height as usize);
        self.top.map_or(max, |t| t.min(max))
    }

    /// Back to following the bottom (Marv's `followKey` change).
    pub fn follow(&mut self) {
        self.top = None;
    }

    pub fn scroll_by(&mut self, rows: i32) {
        let _ = rows;
        todo!("port: scrollBy (reaching the bottom resumes following)")
    }

    pub fn page_up(&mut self) {
        todo!("port: PgUp (page = height - 1)")
    }

    pub fn page_down(&mut self) {
        todo!("port: PgDn")
    }

    /// A wheel notch, in rows (`mouse::WHEEL_ROWS` × step).
    pub fn glide_by(&mut self, rows: i32, now: Instant) {
        let _ = (rows, now);
        todo!("port: glideBy (first row moves at once)")
    }

    /// Advances the glide if due; true if it scrolled.
    pub fn tick(&mut self, now: Instant) -> bool {
        let _ = now;
        todo!("port: glideStep")
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.glide_at
    }

    /// Which entries (by index) and which row offset to draw, given each entry's height.
    pub fn visible(&mut self, heights: &[usize], viewport_height: u16) -> std::ops::Range<usize> {
        let _ = (heights, viewport_height);
        todo!("port: viewport culling by prefix sums")
    }

    pub fn viewport(&self, top: u16, width: u16) -> Viewport {
        Viewport { top, height: self.viewport_height, scroll_top: self.scroll_top(), content_height: self.content_height, width }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
