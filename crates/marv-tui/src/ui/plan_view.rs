//! The plan tool's checklist (#15), pinned between the transcript and the prompt while it has something left to do:
//! the transcript scrolls away within a few tool calls, and "where is Marv in this task" is asked while it works.
//! Ports `src/ui/PlanView.tsx` (tests: `tests/planview.test.tsx`).
//!
//! Item text goes through `printable()`. Marv memoised the component so it re-rendered only when the plan changed;
//! here it's a handful of short lines built at draw time, which costs nothing next to the transcript's cache.

use marv_core::printable;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::select::truncate_end;
use super::theme;
use crate::view::{PlanItem, PlanStatus};

/// At most this many rows, the header included: the transcript needs the rest.
pub const PLAN_ROWS: usize = 6;
/// Below this many rows the window has no room for it.
pub const PLAN_MIN_WINDOW: u16 = 20;

fn mark(status: PlanStatus) -> &'static str {
    match status {
        PlanStatus::Done => "✓",
        PlanStatus::InProgress => "▸",
        PlanStatus::Pending => "○",
    }
}

/// A plan with something left to do (an all-done or empty plan has nothing to show). Marv's `planOpen`.
pub fn plan_open(items: Option<&[PlanItem]>) -> bool {
    items.is_some_and(|items| items.iter().any(|i| i.status != PlanStatus::Done))
}

/// The items to draw under the header, and how many more there are. A plan that fits is shown whole; a longer one
/// drops its done items (the header counts them) and shows the current item and the ones after it, then `+N more`.
pub fn plan_rows(items: &[PlanItem], max: usize) -> (Vec<PlanItem>, usize) {
    if items.len() <= max {
        return (items.to_vec(), 0);
    }
    let current = items.iter().position(|i| i.status == PlanStatus::InProgress);
    let start = current.unwrap_or_else(|| items.iter().position(|i| i.status != PlanStatus::Done).unwrap_or(0));
    let left: Vec<PlanItem> = items[start..]
        .iter()
        .enumerate()
        .filter(|(n, i)| *n == 0 || i.status != PlanStatus::Done)
        .map(|(_, i)| i.clone())
        .collect();
    if left.len() <= max {
        return (left, 0);
    }
    let more = left.len() - (max - 1);
    (left[..max - 1].to_vec(), more)
}

/// The panel's lines (none when there's no open plan): a header with the progress, then the rows.
pub fn render(items: Option<&[PlanItem]>, width: u16) -> Vec<Line<'static>> {
    let Some(list) = items.filter(|items| plan_open(Some(items))) else { return Vec::new() };
    let (shown, more) = plan_rows(list, PLAN_ROWS - 1);
    let done = list.iter().filter(|i| i.status == PlanStatus::Done).count();
    let title = format!(" Plan {done}/{} ", list.len());
    let rule = "─".repeat((width as usize).saturating_sub(title.chars().count() + 1));
    let header = Line::from(vec![
        Span::styled(title, Style::new().fg(theme::ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(rule, Style::new().fg(theme::DIM)),
    ]);
    let mut lines = vec![truncate_end(header, width)];
    for item in shown {
        let style = if item.status == PlanStatus::InProgress {
            Style::new().fg(theme::ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(theme::DIM)
        };
        let text = format!("  {} {}", mark(item.status), printable(&item.text));
        lines.push(truncate_end(Line::from(Span::styled(text, style)), width));
    }
    if more > 0 {
        lines.push(Line::from(Span::styled(format!("  +{more} more"), Style::new().fg(theme::DIM))));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(statuses: &[PlanStatus]) -> Vec<PlanItem> {
        statuses.iter().enumerate().map(|(i, s)| PlanItem { text: format!("step {}", i + 1), status: *s }).collect()
    }
    fn plain(items: Option<&[PlanItem]>, width: u16) -> Vec<String> {
        render(items, width).iter().map(|l| l.to_string()).collect()
    }
    use PlanStatus::{Done, InProgress, Pending};

    #[test]
    fn a_short_plan_is_shown_whole_the_current_item_marked() {
        let lines = plain(Some(&items(&[Done, Done, InProgress, Pending, Pending])), 60);
        assert!(lines[0].starts_with(" Plan 2/5 ─"), "{}", lines[0]);
        assert_eq!(lines[0].chars().count(), 59);
        assert_eq!(lines[1..], ["  ✓ step 1", "  ✓ step 2", "  ▸ step 3", "  ○ step 4", "  ○ step 5"]);
        let current = &render(Some(&items(&[Done, InProgress])), 60)[2];
        assert_eq!(current.spans[0].style.fg, Some(theme::ACCENT));
    }

    #[test]
    fn twelve_items_four_done_at_most_6_rows_the_current_item_the_next_ones_then_more() {
        let mut statuses = vec![Done, Done, Done, Done, InProgress];
        statuses.extend([Pending; 7]);
        let lines = plain(Some(&items(&statuses)), 60);
        assert_eq!(lines.len(), PLAN_ROWS);
        assert!(lines[0].starts_with(" Plan 4/12 "));
        assert_eq!(lines[1..], ["  ▸ step 5", "  ○ step 6", "  ○ step 7", "  ○ step 8", "  +4 more"]);
    }

    #[test]
    fn plan_rows_a_long_plan_without_a_current_item_starts_at_the_first_one_not_done() {
        let list = items(&[Done, Done, Pending, Pending, Pending, Pending]);
        assert_eq!(plan_rows(&list, PLAN_ROWS - 1), (list[2..].to_vec(), 0));
    }

    #[test]
    fn nothing_when_every_item_is_done_or_theres_no_plan() {
        assert!(render(Some(&items(&[Done, Done])), 60).is_empty());
        assert!(render(Some(&[]), 60).is_empty());
        assert!(render(None, 60).is_empty());
    }

    #[test]
    fn item_text_is_drawn_through_printable_no_escape_sequences_reach_the_terminal() {
        let evil = [PlanItem { text: "evil \x1b]52;c;aGk=\x07 text".into(), status: InProgress }];
        let lines = plain(Some(&evil), 60).join("\n");
        assert!(!lines.contains("\x1b]52"));
        assert!(!lines.contains('\x1b'));
        assert!(lines.contains("▸ evil"));
    }

    #[test]
    fn long_items_are_cut_to_the_width() {
        let long = [PlanItem { text: "x".repeat(100), status: InProgress }];
        assert!(render(Some(&long), 30).iter().all(|l| l.width() <= 30));
    }
}
