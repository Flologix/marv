//! The bottom line. Ports `src/ui/StatusBar.tsx` (tests in `tests/app.test.tsx`, `tests/usage.test.ts`).
//!
//! Left (never cut off): `yolo · ` when yolo is on (so the user always knows what runs without
//! asking; hidden while a confirm or a flash shows), then the hint: "Press ctrl+c again to exit"
//! (warning) / a flash ("Copied 3 lines") / busy: "esc to interrupt" or, in a subagent's view,
//! "esc to go back", plus "· 2 agents running" / idle: "/help · PgUp/PgDn · ctrl+c exit".
//! Right (cut from the *start* on a narrow terminal): usage · cost · model · cwd.
//!
//! Usage is the *latest request's* size: `12.3k/32.8k ctx` against the window when known, else
//! `12.3k tokens`, plus `· 87% cached` when the provider reports cache hits (OpenRouter): the
//! prompt-cache hit rate, the number that says whether the append-only history is working. Cost is
//! the session total: `$0.042`, `~$0.042` (estimated from prices), `<$0.001`, or `local` (Ollama).
//! Model and cwd go through `printable()`.

use marv_core::Usage;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::view::{StatusInfo, Totals};

/// "12.3k" style token counts (Marv `tokens` in usage.ts).
pub fn tokens(n: u64) -> String {
    let _ = n;
    todo!("port: tokens()")
}

/// The latest request's size, and the cache hit rate when reported (Marv `formatUsage`).
pub fn format_usage(usage: &Usage, context_length: Option<u64>) -> String {
    let _ = (usage, context_length);
    todo!("port: formatUsage (round(100 * cached / prompt)% cached)")
}

/// "$0.042", "~$0.042", "<$0.001", "local", or "" before any request (Marv `costText`).
pub fn cost_text(totals: &Totals) -> String {
    let _ = totals;
    todo!("port: costText")
}

/// Everything the bar shows.
#[derive(Debug, Clone, Default)]
pub struct StatusBar<'a> {
    pub info: Option<&'a StatusInfo>,
    /// Set after one ctrl+c on an empty prompt.
    pub confirm_exit: bool,
    /// A short-lived message, e.g. after copying a selection.
    pub flash: Option<&'a str>,
    /// From format_usage + cost_text, once a request has been made.
    pub usage: Option<String>,
    pub busy: bool,
    /// Subagents running right now.
    pub agents: usize,
    /// A subagent's view is open: Esc closes it instead of interrupting.
    pub viewing: bool,
}

impl StatusBar<'_> {
    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let _ = (area, buf);
        todo!("port: StatusBar (left hint fixed, right truncate-start)")
    }
}
