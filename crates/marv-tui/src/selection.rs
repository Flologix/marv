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
//! an already-culled frame plus ratatui's cell diff, so the "repaint" path is just a normal draw.
//! Auto-scroll is a deadline the app's loop wakes for, not a `setInterval`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;

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

/// The columns `[from, to)` selected on transcript row `y`, or None. Like a terminal: the first
/// row runs to the end of the line, middle rows are whole, and the last row stops at the cell
/// under the pointer (inclusive).
pub fn columns_on_row(selection: &Selection, y: usize) -> Option<(u16, u16)> {
    let _ = (selection, y);
    todo!("port: columnsOnRow")
}

/// The columns `[from, to)` widened to whole characters, clamped to the line's width.
pub fn whole_cells(line: &str, columns: (u16, u16)) -> (u16, u16) {
    let _ = (line, columns);
    todo!("port: wholeCells (unicode-segmentation graphemes + unicode-width)")
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
    /// Called by the transcript's scroll view on every draw. A new width or height resets the
    /// remembered rows (they no longer line up).
    pub fn set_viewport(&mut self, viewport: Viewport) {
        let _ = viewport;
        todo!("port: setViewport")
    }

    /// After the widgets drew a frame: remember the visible rows' text and reverse the selected cells.
    pub fn transform_buffer(&mut self, buf: &mut Buffer) {
        let _ = buf;
        todo!("port: transformOutput over Buffer cells (Modifier::REVERSED)")
    }

    /// The transcript row under a screen point, or None outside the transcript (for clicks).
    pub fn row_at(&self, x: u16, y: u16) -> Option<usize> {
        let _ = (x, y);
        todo!("port: rowAt")
    }

    /// Mouse down: start a selection if it's on the transcript.
    pub fn press(&mut self, x: u16, y: u16) -> SelectionEffect {
        let _ = (x, y);
        todo!("port: press")
    }

    /// Mouse moved with the button held: extend, and start auto-scroll while past an edge.
    pub fn drag(&mut self, x: u16, y: u16, now: Instant) -> SelectionEffect {
        let _ = (x, y, now);
        todo!("port: drag")
    }

    /// Mouse up: the selected text to copy (nothing for a plain click). The highlight stays.
    pub fn release(&mut self) -> SelectionEffect {
        todo!("port: release")
    }

    /// When the next auto-scroll step is due (the app's loop wakes for it).
    pub fn deadline(&self) -> Option<Instant> {
        self.auto_scroll.map(|(_, at)| at)
    }

    /// An auto-scroll step, if one is due: scrolls one row past the edge and extends onto it.
    pub fn tick(&mut self, now: Instant) -> SelectionEffect {
        let _ = now;
        todo!("port: scrollStep")
    }

    /// A key or a click elsewhere.
    pub fn clear(&mut self) -> SelectionEffect {
        todo!("port: clear")
    }

    /// Forgets the selection and the remembered rows (after `/clear` or a resize).
    pub fn reset(&mut self) {
        self.auto_scroll = None;
        self.rows.clear();
        self.selection = None;
        let _ = self.viewport;
    }
}
