//! How one transcript entry looks. Ports `src/ui/MessageView.tsx` (tests: `tests/messageview.test.tsx`).
//!
//! - user: `> ` + the text in `theme::USER`; assistant: `● ` + Markdown; tool:
//!   `● name label` then `  ⎿ summary` (`running…` while it runs; error red, declined yellow), then
//!   (ctrl+o) its subagent steps `    · step`, then its diff collapsed to [`COLLAPSED_DIFF_LINES`]
//!   with an `(ctrl+o)` hint; system: a notice block, Markdown, or dim/error text.
//! - The 2-column marker ("● ", "> ") is a fixed-width gutter: Ink laid out "● " as 1 column but
//!   drew it as 2, so text beside it wrapped one column too wide. Here the gutter is a fixed span
//!   and the text wraps at `width - 2` with a hanging indent.
//! - A saved session may have been edited by hand: a malformed diff (or a line in it) is skipped
//!   rather than taking the App down on resume (serde `default`s; non-object lines dropped by the
//!   engine's loader).
//! - Memoised in Marv (`memo`): here the transcript caches each entry's lines per width and
//!   `show_steps`, so a transcript change only re-lays-out entries that changed.
//! - Every string goes through `printable()` here, at render time.

use marv_core::printable;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::theme;
use crate::view::{Message, Role};

/// A change's diff under its entry: enough to see what changed; ctrl+o shows the rest.
pub const COLLAPSED_DIFF_LINES: usize = 15;

/// One entry as lines. `streaming`: the assistant reply still coming in (parsed incrementally).
pub fn render(
    message: &Message,
    width: u16,
    show_steps: bool,
    streaming: Option<&mut super::markdown::StreamLexer>,
) -> Vec<Line<'static>> {
    match message.role {
        Role::User => {
            // TODO(port): wrap at width - 2 with a hanging indent; for now one logical line.
            let style = Style::new().fg(theme::USER);
            vec![Line::from(vec![Span::styled("> ", style), Span::styled(printable(&message.text), style)]), Line::default()]
        }
        Role::Assistant => {
            // The raw reply goes to markdown::render, which applies printable() itself.
            let body = super::markdown::render(&message.text, width.saturating_sub(2), streaming);
            let _ = body;
            todo!("port: the ● gutter (theme::ASSISTANT) before the reply's lines, hanging indent, blank line after")
        }
        Role::Tool | Role::System => {
            let _ = show_steps;
            todo!("port: tool and system entries (printable on label, summary, steps; DiffView; NoticeView)")
        }
    }
}
