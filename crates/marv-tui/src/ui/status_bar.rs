//! The bottom line. Ports `src/ui/StatusBar.tsx` (tests in `tests/app.test.tsx`, `tests/usage.test.ts`).
//!
//! Left (never cut off): `yolo · ` when yolo is on (so the user always knows what runs without
//! asking; hidden while a confirm or a flash shows), then the hint: "Press ctrl+c again to exit"
//! (warning) / a flash ("Copied 3 lines") / busy: "esc to interrupt · alt+enter send now" or, in a subagent's
//! view, "esc to go back", plus "· 2 agents running" and "· 1 for the next step" (send now, waiting) / idle:
//! "/help · PgUp/PgDn · ctrl+c exit".
//! Right (cut from the *start* on a narrow terminal): usage · cost · model · cwd.
//!
//! Usage is the *latest request's* size: `12.3k/32.8k ctx` against the window when known, else
//! `12.3k tokens`, plus `· 87% cached` when the provider reports cache hits (OpenRouter): the
//! prompt-cache hit rate, the number that says whether the append-only history is working. Cost is
//! the session total: `$0.042`, `~$0.042` (estimated from prices), `<$0.001`, or `local` (Ollama).
//! Model and cwd go through `printable()`.
//!
//! Rounding: JS's `toFixed` and `Math.round` round halves up, Rust's float formatting rounds them
//! to even (1250 tokens is "1.3k" in Marv, would be "1.2k"); the token counts use integer
//! arithmetic so they match Marv exactly.

use marv_core::{Usage, printable};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::theme;
use crate::view::{StatusInfo, Totals};

/// "12.3k" style token counts (Marv `tokens` in usage.ts).
pub fn tokens(n: u64) -> String {
    /// `tenths` as "12.3", or "12" when the tenth is 0 (Marv strips ".0").
    fn tenths(t: u64) -> String {
        if t.is_multiple_of(10) { format!("{}", t / 10) } else { format!("{}.{}", t / 10, t % 10) }
    }
    if n < 1000 {
        n.to_string()
    } else if n < 100_000 {
        format!("{}k", tenths((n + 50) / 100))
    } else if n < 1_000_000 {
        format!("{}k", (n + 500) / 1000)
    } else {
        format!("{}M", tenths((n + 50_000) / 100_000))
    }
}

/// The latest request's size, and the cache hit rate when reported (Marv `formatUsage`): against
/// the context window when we know it (Ollama), plus how much the provider served from its prompt
/// cache when it reports that (OpenRouter).
pub fn format_usage(usage: &Usage, context_length: Option<u64>) -> String {
    let used = usage.prompt_tokens + usage.completion_tokens;
    let size = match context_length {
        Some(ctx) if ctx > 0 => format!("{}/{} ctx", tokens(used), tokens(ctx)),
        _ => format!("{} tokens", tokens(usage.prompt_tokens)),
    };
    match usage.cached_tokens {
        Some(cached) if usage.prompt_tokens > 0 => {
            let p = usage.prompt_tokens;
            format!("{size} · {}% cached", (200 * cached + p) / (2 * p))
        }
        _ => size,
    }
}

/// "$0.042", "~$0.042", "<$0.001", "local", or "" before any request (Marv `costText`).
pub fn cost_text(totals: &Totals) -> String {
    if totals.requests == 0 {
        return String::new();
    }
    if totals.local {
        return "local".into();
    }
    let Some(cost) = totals.cost else { return String::new() };
    let amount = if cost == 0.0 {
        "$0.00".to_string()
    } else if cost < 0.001 {
        "<$0.001".to_string()
    } else if cost < 1.0 {
        format!("${cost:.3}")
    } else {
        format!("${cost:.2}")
    };
    if totals.estimated && cost >= 0.001 { format!("~{amount}") } else { amount }
}

/// The status bar's usage text: the latest request's size and the session's cost, or None before
/// any request.
pub fn usage_text(last: Option<&Usage>, totals: &Totals, context_length: Option<u64>) -> Option<String> {
    let parts: Vec<String> = [last.map(|u| format_usage(u, context_length)), Some(cost_text(totals))]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// `text` cut to `width` columns from the start, with a leading "…" (Ink's `truncate-start`).
pub fn truncate_start(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut tail: Vec<&str> = Vec::new();
    let mut used = 1; // the "…"
    for g in text.graphemes(true).rev() {
        let w = g.width();
        if used + w > width {
            break;
        }
        used += w;
        tail.push(g);
    }
    tail.reverse();
    format!("…{}", tail.concat())
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
    /// "Send now" messages waiting for the turn's next step.
    pub sending: usize,
}

impl StatusBar<'_> {
    /// The left side: yolo and the hint (never cut off).
    pub fn left(&self) -> Line<'static> {
        let hint = if self.confirm_exit {
            "Press ctrl+c again to exit".to_string()
        } else if let Some(flash) = self.flash {
            printable(flash)
        } else if self.busy {
            let esc = if self.viewing { "esc to go back" } else { "esc to interrupt · alt+enter send now" };
            let mut hint = match self.agents {
                0 => esc.to_string(),
                1 => format!("{esc} · 1 agent running"),
                n => format!("{esc} · {n} agents running"),
            };
            if self.sending > 0 {
                hint.push_str(&format!(" · {} for the next step", self.sending));
            }
            hint
        } else {
            "/help · PgUp/PgDn · ctrl+c exit".to_string()
        };
        // On the left, which is never cut off, and whenever the hint is (busy is when it matters most).
        let show_yolo = self.info.is_some_and(|i| i.yolo) && !self.confirm_exit && self.flash.is_none();
        let color = if self.confirm_exit { theme::WARNING } else { theme::DIM };
        let mut spans = Vec::new();
        if show_yolo {
            spans.push(Span::styled("yolo", Style::new().fg(theme::ACCENT)));
            spans.push(Span::styled(" · ", Style::new().fg(color)));
        }
        let running = self.info.map_or(0, |i| i.processes);
        if running > 0 && !self.confirm_exit && self.flash.is_none() {
            spans.push(Span::styled(format!("{running} running"), Style::new().fg(theme::ACCENT)));
            spans.push(Span::styled(" · ", Style::new().fg(color)));
        }
        spans.push(Span::styled(hint, Style::new().fg(color)));
        Line::from(spans)
    }

    /// The right side, before truncation: usage · model · cwd.
    pub fn right(&self) -> String {
        let (model, cwd) = self.info.map_or((String::new(), String::new()), |i| (printable(&i.model), printable(&i.cwd)));
        match &self.usage {
            Some(usage) => format!("{usage} · {model} · {cwd}"),
            None => format!("{model} · {cwd}"),
        }
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width < 2 {
            return;
        }
        // paddingX 1.
        let inner = Rect { x: area.x + 1, width: area.width - 2, height: 1, ..area };
        let left = self.left();
        let left_width = (left.width() as u16).min(inner.width);
        buf.set_line(inner.x, inner.y, &left, inner.width);
        // marginRight 2, then the rest right-aligned, cut from the start: on a narrow terminal the
        // model/cwd side gets cut off, not the hint.
        let room = inner.width.saturating_sub(left_width + 2) as usize;
        let right = truncate_start(&self.right(), room);
        let w = right.width() as u16;
        if w > 0 {
            buf.set_string(inner.right() - w, inner.y, right, Style::new().fg(theme::DIM));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(prompt: u64, completion: u64, cached: Option<u64>) -> Usage {
        Usage { prompt_tokens: prompt, completion_tokens: completion, cached_tokens: cached, cost: None }
    }

    fn line(bar: &StatusBar, width: u16) -> String {
        let mut buf = Buffer::empty(Rect::new(0, 0, width, 1));
        bar.render(buf.area, &mut buf);
        (0..width).map(|x| buf[(x, 0)].symbol().to_string()).collect::<String>()
    }

    #[test]
    fn tokens_like_marv() {
        assert_eq!(tokens(999), "999");
        assert_eq!(tokens(1000), "1k");
        assert_eq!(tokens(1200), "1.2k");
        assert_eq!(tokens(1250), "1.3k"); // JS toFixed rounds the half up
        assert_eq!(tokens(12_345), "12.3k");
        assert_eq!(tokens(99_960), "100k");
        assert_eq!(tokens(131_072), "131k");
        assert_eq!(tokens(1_000_000), "1M");
        assert_eq!(tokens(1_250_000), "1.3M");
    }

    #[test]
    fn shows_token_usage_and_cache_hits() {
        // app.test.tsx "shows token usage and cache hits in the status bar"
        assert_eq!(format_usage(&usage(1200, 30, Some(1000)), None), "1.2k tokens · 83% cached");
        assert_eq!(format_usage(&usage(1200, 30, None), None), "1.2k tokens");
        assert_eq!(format_usage(&usage(0, 0, Some(0)), None), "0 tokens");
        assert_eq!(format_usage(&usage(12_000, 300, None), Some(32_768)), "12.3k/32.8k ctx");
        assert_eq!(format_usage(&usage(200, 0, Some(1)), None), "200 tokens · 1% cached"); // 0.5% rounds up
    }

    #[test]
    fn cost_text_like_marv() {
        let t = |requests, cost, estimated, local| Totals { requests, cost, estimated, local, ..Totals::default() };
        assert_eq!(cost_text(&t(0, Some(1.0), false, false)), "");
        assert_eq!(cost_text(&t(1, Some(1.0), false, true)), "local");
        assert_eq!(cost_text(&t(1, None, false, false)), "");
        assert_eq!(cost_text(&t(1, Some(0.0), false, false)), "$0.00");
        assert_eq!(cost_text(&t(1, Some(0.0004), true, false)), "<$0.001");
        assert_eq!(cost_text(&t(1, Some(0.042), false, false)), "$0.042");
        assert_eq!(cost_text(&t(1, Some(0.042), true, false)), "~$0.042");
        assert_eq!(cost_text(&t(1, Some(3.5), false, false)), "$3.50");
        assert_eq!(usage_text(None, &Totals::default(), None), None);
        assert_eq!(
            usage_text(Some(&usage(1200, 30, Some(1000))), &t(1, Some(0.042), false, false), None).unwrap(),
            "1.2k tokens · 83% cached · $0.042"
        );
    }

    #[test]
    fn the_hint_follows_the_state() {
        let info = StatusInfo { model: "ollama/qwen".into(), cwd: "~/x".into(), yolo: true, context_length: None, processes: 0 };
        let idle = StatusBar { info: Some(&info), ..Default::default() };
        assert_eq!(idle.left().to_string(), "yolo · /help · PgUp/PgDn · ctrl+c exit");
        let busy = StatusBar { busy: true, agents: 2, ..idle.clone() };
        assert_eq!(busy.left().to_string(), "yolo · esc to interrupt · alt+enter send now · 2 agents running");
        let sending = StatusBar { sending: 1, ..busy.clone() };
        assert_eq!(
            sending.left().to_string(),
            "yolo · esc to interrupt · alt+enter send now · 2 agents running · 1 for the next step"
        );
        let viewing = StatusBar { busy: true, agents: 1, viewing: true, ..idle.clone() };
        assert_eq!(viewing.left().to_string(), "yolo · esc to go back · 1 agent running");
        let confirm = StatusBar { confirm_exit: true, ..busy.clone() };
        assert_eq!(confirm.left().to_string(), "Press ctrl+c again to exit");
        let flash = StatusBar { flash: Some("Copied 7 chars"), ..idle.clone() };
        assert_eq!(flash.left().to_string(), "Copied 7 chars");
    }

    #[test]
    fn running_background_processes_are_counted_whenever_there_are_any() {
        // Marv's StatusBar (#51): they keep running between turns, so idle or busy, after yolo and before the hint.
        let info = StatusInfo { yolo: true, processes: 1, ..Default::default() };
        let idle = StatusBar { info: Some(&info), ..Default::default() };
        assert_eq!(idle.left().to_string(), "yolo · 1 running · /help · PgUp/PgDn · ctrl+c exit");
        assert_eq!(idle.left().spans[2].style.fg, Some(theme::ACCENT));
        let two = StatusInfo { processes: 2, ..Default::default() };
        let busy = StatusBar { info: Some(&two), busy: true, ..Default::default() };
        assert_eq!(busy.left().to_string(), "2 running · esc to interrupt · alt+enter send now");
        // Not over the exit confirmation or a flash, and not at all with none.
        assert_eq!(StatusBar { confirm_exit: true, ..busy.clone() }.left().to_string(), "Press ctrl+c again to exit");
        assert_eq!(StatusBar { flash: Some("Copied"), ..busy.clone() }.left().to_string(), "Copied");
        let none = StatusInfo::default();
        assert_eq!(StatusBar { info: Some(&none), ..Default::default() }.left().to_string(), "/help · PgUp/PgDn · ctrl+c exit");
    }

    #[test]
    fn a_narrow_terminal_cuts_the_right_side_from_the_start_never_the_hint() {
        let info =
            StatusInfo { model: "openrouter/some-model\x1b[31m".into(), cwd: "~/Projects/app".into(), ..Default::default() };
        let bar = StatusBar { info: Some(&info), usage: Some("1.2k tokens".into()), ..Default::default() };
        let wide = line(&bar, 100);
        assert!(wide.starts_with(" /help · PgUp/PgDn · ctrl+c exit"));
        assert!(wide.trim_end().ends_with("1.2k tokens · openrouter/some-model · ~/Projects/app"), "{wide}");
        let narrow = line(&bar, 50);
        assert!(narrow.starts_with(" /help · PgUp/PgDn · ctrl+c exit"));
        assert!(narrow.trim_end().ends_with(" …~/Projects/app"), "{narrow}");
        assert_eq!(truncate_start("abcdef", 4), "…def");
        assert_eq!(truncate_start("abc", 4), "abc");
    }
}
