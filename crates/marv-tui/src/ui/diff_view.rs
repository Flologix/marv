//! A change, drawn: numbered lines, green additions, red removals. Ports `src/ui/DiffView.tsx`
//! (tests: `tests/diffview.test.tsx`). Used by the approval prompt (before a change) and the
//! transcript (after one), so both look the same.
//!
//! - Removed lines show their old number, the others their new one, right-aligned in one column
//!   wide enough for *all* lines, shown or not, so the text doesn't shift when ctrl+o shows the rest.
//!   (A loop, not `max(...lines)`: a new file's preview holds all its lines.)
//! - Marks: `+ `, `- `, `  `; a gap is `…`. Lines are truncated, not wrapped.
//! - At most `max` lines (`previewLines`, which keeps hunks whole where it can); the rest are
//!   counted: `… 12 more lines (ctrl+o)`, or `… not kept` when the result itself dropped them and
//!   nothing can show them.
//! - Line text goes through `printable()`.

use ratatui::text::Line;

use crate::view::{DiffKind, DiffLine};

/// The mark before a line of each kind.
pub fn mark(kind: DiffKind) -> &'static str {
    match kind {
        DiffKind::Add => "+ ",
        DiffKind::Del => "- ",
        DiffKind::Ctx | DiffKind::Gap => "  ",
    }
}

/// How a diff is shown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiffOptions<'a> {
    /// Show at most this many lines (the rest are counted).
    pub max: Option<usize>,
    /// Lines left out before they got here (a result keeps at most MAX_SHOWN_DIFF).
    pub more: usize,
    /// How to see the rest, e.g. "ctrl+o".
    pub hint: Option<&'a str>,
    pub indent: u16,
}

/// The diff as lines (raw line text; cleaned here).
pub fn render(lines: &[DiffLine], width: u16, options: DiffOptions<'_>) -> Vec<Line<'static>> {
    let _ = (lines, width, options);
    todo!("port: DiffView (previewLines/fileLines from marv-tools' diff helpers, number column, printable)")
}
