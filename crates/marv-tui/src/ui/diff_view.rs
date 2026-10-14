//! A change, drawn: numbered lines, green additions, red removals. Ports `src/ui/DiffView.tsx`
//! (tests: `tests/diffview.test.tsx`). Used by the approval prompt (before a change) and the
//! transcript (after one), so both look the same.
//!
//! - Removed lines show their old number, the others their new one, right-aligned in one column
//!   wide enough for *all* lines, shown or not, so the text doesn't shift when ctrl+o shows the rest.
//!   (A loop, not `max(...lines)`: a new file's preview holds all its lines.)
//! - Marks: `+ `, `- `, `  `; a gap is `…`. Lines are truncated, not wrapped. The whole line takes
//!   its kind's colour (context dim).
//! - At most `max` lines ([`preview_lines`], which keeps hunks whole where it can); the rest are
//!   counted: `… 12 more lines (ctrl+o)`, or `… not kept` when the result itself dropped them and
//!   nothing can show them.
//! - Line text goes through `printable()`; a tab is drawn as two spaces (a cell holds one column).
//!
//! [`preview_lines`] and [`file_lines`] port `src/tools/diff.ts`'s helpers over the view's
//! [`DiffLine`]: marv-tui doesn't depend on marv-tools (the view boundary is plain data), so the
//! two copies are kept in step (marv-tools' `diff.rs` has the same functions and tests).

use marv_core::printable;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::select::truncate_end;
use super::theme;
use crate::view::{DiffKind, DiffLine};

/// The mark before a line of each kind.
pub fn mark(kind: DiffKind) -> &'static str {
    match kind {
        DiffKind::Add => "+ ",
        DiffKind::Del => "- ",
        DiffKind::Ctx | DiffKind::Gap => "  ",
    }
}

/// The colour of a line of each kind.
pub fn color(kind: DiffKind) -> ratatui::style::Color {
    match kind {
        DiffKind::Add => theme::DIFF_ADD,
        DiffKind::Del => theme::DIFF_DEL,
        DiffKind::Ctx | DiffKind::Gap => theme::DIM,
    }
}

/// How a diff is shown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiffOptions<'a> {
    /// Show at most this many lines (the rest are counted).
    pub max: Option<usize>,
    /// Lines left out before they got here (a result keeps at most MAX_SHOWN_DIFF).
    pub more: usize,
    /// How to see the rest, e.g. "ctrl+o". Without one, `more` lines are said to be "not kept":
    /// nothing can show them.
    pub hint: Option<&'a str>,
    pub indent: u16,
}

fn gap() -> DiffLine {
    DiffLine { kind: DiffKind::Gap, text: String::new(), old_line: None, new_line: None }
}

/// At most `max` lines of a diff for a preview. A run of removals followed by additions (a rewrite)
/// would otherwise fill the preview with removed lines only: when it doesn't fit, it keeps the first
/// fifth of the removals, a gap, then the additions.
pub fn preview_lines(lines: &[DiffLine], max: usize) -> Vec<DiffLine> {
    let keep = (max / 5).max(1);
    let mut out: Vec<DiffLine> = Vec::new();
    let mut i = 0;
    while i < lines.len() && out.len() < max {
        if lines[i].kind != DiffKind::Del {
            out.push(lines[i].clone());
            i += 1;
            continue;
        }
        let mut end = i;
        while end < lines.len() && lines[end].kind == DiffKind::Del {
            end += 1;
        }
        // A gap right after removals is one of ours (a hunk ends in context), from a kept diff that was already cut.
        let adds = if lines.get(end).is_some_and(|l| l.kind == DiffKind::Gap) { end + 1 } else { end };
        let mut next = adds;
        while next < lines.len() && lines[next].kind == DiffKind::Add {
            next += 1;
        }
        if end - i > keep && next > adds && next - i > max - out.len() {
            out.extend_from_slice(&lines[i..i + keep]);
            out.push(gap());
            i = adds;
        } else {
            let stop = end.min(i + (max - out.len()));
            out.extend_from_slice(&lines[i..stop]);
            i = end;
        }
    }
    out.truncate(max);
    out
}

/// Lines of the file, not counting gap markers.
pub fn file_lines(lines: &[DiffLine]) -> usize {
    lines.iter().filter(|l| l.kind != DiffKind::Gap).count()
}

/// The diff as lines `width` columns wide (raw line text; cleaned here).
pub fn render(lines: &[DiffLine], width: u16, options: DiffOptions<'_>) -> Vec<Line<'static>> {
    let shown: std::borrow::Cow<[DiffLine]> = match options.max {
        Some(max) => preview_lines(lines, max).into(),
        None => lines.into(),
    };
    let hidden_here = file_lines(lines) - file_lines(&shown);
    let hidden = hidden_here + options.more;
    // Removed lines show their old number, the others their new one; one column wide enough for all of them, shown
    // or not, so the text doesn't shift when ctrl+o shows the rest.
    let number = |l: &DiffLine| if l.kind == DiffKind::Del { l.old_line } else { l.new_line };
    // A loop, not a max over a spread: a new file's preview holds all its lines.
    let mut number_width = 1;
    for line in lines {
        if let Some(n) = number(line) {
            number_width = number_width.max(n.to_string().len());
        }
    }
    let pad = " ".repeat(number_width);
    let indent = " ".repeat(options.indent as usize);
    let inner = width.saturating_sub(options.indent);
    let mut out: Vec<Line<'static>> = shown
        .iter()
        .map(|line| {
            let text = if line.kind == DiffKind::Gap {
                format!("{pad} {}…", mark(DiffKind::Gap))
            } else {
                let n = number(line).map_or_else(String::new, |n| n.to_string());
                format!("{n:>number_width$} {}{}", mark(line.kind), printable(&line.text).replace('\t', "  "))
            };
            let row = truncate_end(Line::from(Span::styled(text, Style::new().fg(color(line.kind)))), inner);
            with_indent(&indent, row)
        })
        .collect();
    if hidden > 0 {
        let plural = if hidden == 1 { "" } else { "s" };
        let how = match options.hint {
            Some(hint) => format!(" ({hint})"),
            None if hidden_here == 0 => " not kept".into(),
            None => String::new(),
        };
        let text = format!("{pad} {}… {hidden} more line{plural}{how}", mark(DiffKind::Gap));
        out.push(with_indent(&indent, truncate_end(Line::from(Span::styled(text, Style::new().fg(theme::DIM))), inner)));
    }
    out
}

fn with_indent(indent: &str, line: Line<'static>) -> Line<'static> {
    if indent.is_empty() {
        return line;
    }
    let mut spans = vec![Span::raw(indent.to_string())];
    spans.extend(line.spans);
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(kind: DiffKind, text: &str, old: Option<usize>, new: Option<usize>) -> DiffLine {
        DiffLine { kind, text: text.into(), old_line: old, new_line: new }
    }

    /// What `diffText(file("old", 30), file("new", 30))` gives: every old line removed, then every new one added.
    fn rewrite(n: usize) -> Vec<DiffLine> {
        let mut lines: Vec<DiffLine> = (1..=n).map(|i| line(DiffKind::Del, &format!("old {i}"), Some(i), None)).collect();
        lines.extend((1..=n).map(|i| line(DiffKind::Add, &format!("new {i}"), None, Some(i))));
        lines
    }

    fn frame(lines: &[DiffLine], options: DiffOptions<'_>) -> String {
        render(lines, 80, options).iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn a_collapsed_full_rewrite_shows_the_start_of_the_new_file_not_only_removed_lines() {
        let f = frame(&rewrite(30), DiffOptions { max: Some(15), hint: Some("ctrl+o"), ..Default::default() });
        assert!(f.contains("- old 1"));
        assert!(!f.contains("old 4"));
        assert!(f.contains("+ new 1"));
        assert!(f.contains("+ new 11"));
        assert!(!f.contains("new 12"));
        // 60 lines, 3 removed and 11 added shown.
        assert!(f.contains("… 46 more lines (ctrl+o)"), "{f}");
    }

    #[test]
    fn expanded_a_full_rewrite_shows_every_line_in_diff_order() {
        let f = frame(&rewrite(30), DiffOptions::default());
        assert!(f.contains("old 30"));
        assert!(f.find("old 30") < f.find("new 1"));
        assert!(!f.contains("more line"));
    }

    #[test]
    fn a_small_change_fits_as_it_is() {
        // diffText of ten lines with "line 5" → "line five" (3 lines of context).
        let mut lines: Vec<DiffLine> = (2..=4).map(|i| line(DiffKind::Ctx, &format!("line {i}"), Some(i), Some(i))).collect();
        lines.push(line(DiffKind::Del, "line 5", Some(5), None));
        lines.push(line(DiffKind::Add, "line five", None, Some(5)));
        lines.extend((6..=8).map(|i| line(DiffKind::Ctx, &format!("line {i}"), Some(i), Some(i))));
        let f = frame(&lines, DiffOptions { max: Some(15), ..Default::default() });
        assert!(f.contains("- line 5"));
        assert!(f.contains("+ line five"));
        assert!(!f.contains("more line"));
    }

    #[test]
    fn numbers_colours_gaps_and_what_isnt_kept() {
        let lines = [line(DiffKind::Ctx, "a", Some(9), Some(9)), gap(), line(DiffKind::Del, "b\tc", Some(120), None)];
        let rows = render(&lines, 80, DiffOptions { more: 1, indent: 2, ..Default::default() });
        let text: Vec<String> = rows.iter().map(|l| l.to_string()).collect();
        assert_eq!(text, ["    9   a", "        …", "  120 - b  c", "        … 1 more line not kept"]);
        assert_eq!(rows[2].spans[1].style.fg, Some(theme::DIFF_DEL));
        assert_eq!(rows[0].spans[1].style.fg, Some(theme::DIM));
    }

    #[test]
    fn lines_are_cut_not_wrapped_and_cleaned() {
        let lines = [line(DiffKind::Add, &format!("evil\x1b]52;c;aGk=\x07{}", "x".repeat(100)), None, Some(1))];
        let rows = render(&lines, 20, DiffOptions::default());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].width(), 20);
        assert!(!rows[0].to_string().contains('\x1b'));
    }
}
