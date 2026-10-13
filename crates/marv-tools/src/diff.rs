//! A change as diff lines, for the approval prompt and the transcript.
//!
//! Ports Marv's `src/tools/diff.ts` (jsdiff's `structuredPatch` → the `similar` crate's grouped ops).
//!
//! - Lines carry numbers: new for added and unchanged lines, old for removed ones; a `gap` marks skipped unchanged
//!   lines between hunks.
//! - A result keeps at most [`MAX_SHOWN_DIFF`] = 400 lines of at most [`MAX_SHOWN_LINE_CHARS`] = 500 chars (an edit to
//!   minified code would otherwise store a huge line, twice, in every save of the session); `more` counts the rest.
//! - [`preview_lines`]: a change (removals then additions) that doesn't fit keeps only its first few removals (a fifth
//!   of `max`) and a gap, so a full rewrite shows the new file's start, not only old lines.

use similar::{ChangeTag, TextDiff};

use crate::types::{DiffKind, DiffLine, DiffShown};

/// The most lines of one change a tool result keeps for the transcript (saved sessions hold them too).
pub const MAX_SHOWN_DIFF: usize = 400;
/// The longest line it keeps: the screen truncates at the terminal's width anyway.
pub const MAX_SHOWN_LINE_CHARS: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diff {
    pub lines: Vec<DiffLine>,
    pub added: usize,
    pub removed: usize,
}

/// Changed lines with `context` lines around them; a gap between hunks.
pub fn diff_text(before: &str, after: &str, context: usize) -> Diff {
    let diff = TextDiff::from_lines(before, after);
    let mut lines = Vec::new();
    let (mut added, mut removed) = (0, 0);
    for (i, group) in diff.grouped_ops(context).iter().enumerate() {
        if i > 0 {
            lines.push(DiffLine::gap());
        }
        for op in group {
            for change in diff.iter_changes(op) {
                let text = change.value().strip_suffix('\n').unwrap_or(change.value()).to_string();
                let old_line = change.old_index().map(|n| n + 1);
                let new_line = change.new_index().map(|n| n + 1);
                let kind = match change.tag() {
                    ChangeTag::Insert => {
                        added += 1;
                        DiffKind::Add
                    }
                    ChangeTag::Delete => {
                        removed += 1;
                        DiffKind::Del
                    }
                    ChangeTag::Equal => DiffKind::Ctx,
                };
                lines.push(DiffLine { kind, text, old_line, new_line });
            }
        }
    }
    Diff { lines, added, removed }
}

/// "+3 −1" (a real minus sign, which lines up with the plus).
pub fn change_summary(diff: &Diff) -> String {
    format!("+{} −{}", diff.added, diff.removed)
}

/// A new file as a diff: every line added, numbered from 1.
pub fn added_lines(content: &str) -> Vec<DiffLine> {
    let text = content.strip_suffix('\n').unwrap_or(content);
    if text.is_empty() {
        return vec![];
    }
    text.split('\n')
        .enumerate()
        .map(|(i, line)| DiffLine { kind: DiffKind::Add, text: line.into(), old_line: None, new_line: Some(i + 1) })
        .collect()
}

/// At most `max` lines of a diff, for a collapsed view (see the module doc).
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
            out.push(DiffLine::gap());
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

/// What a result keeps: `preview_lines` to [`MAX_SHOWN_DIFF`], each line cut to [`MAX_SHOWN_LINE_CHARS`].
pub fn shown_diff(lines: &[DiffLine]) -> DiffShown {
    let kept = preview_lines(lines, MAX_SHOWN_DIFF);
    let more = file_lines(lines) - file_lines(&kept);
    let lines = kept
        .into_iter()
        .map(|mut l| {
            if l.text.chars().count() > MAX_SHOWN_LINE_CHARS {
                l.text = l.text.chars().take(MAX_SHOWN_LINE_CHARS).collect();
            }
            l
        })
        .collect();
    DiffShown { lines, more }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(kind: DiffKind, text: &str, old: Option<usize>, new: Option<usize>) -> DiffLine {
        DiffLine { kind, text: text.into(), old_line: old, new_line: new }
    }

    #[test]
    fn diff_lines_carry_line_numbers_across_hunks() {
        let before: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        let after = before.replace("line 2\n", "line two\n").replace("line 18\n", "line 18\nline 18b\n");
        let diff = diff_text(&before, &after, 1);
        assert_eq!(
            diff.lines,
            vec![
                line(DiffKind::Ctx, "line 1", Some(1), Some(1)),
                line(DiffKind::Del, "line 2", Some(2), None),
                line(DiffKind::Add, "line two", None, Some(2)),
                line(DiffKind::Ctx, "line 3", Some(3), Some(3)),
                DiffLine::gap(),
                line(DiffKind::Ctx, "line 18", Some(18), Some(18)),
                line(DiffKind::Add, "line 18b", None, Some(19)),
                line(DiffKind::Ctx, "line 19", Some(19), Some(20)),
            ]
        );
        assert_eq!(change_summary(&diff), "+2 −1");
    }

    #[test]
    fn a_new_file_is_all_additions() {
        assert_eq!(added_lines("a\nb\n"), vec![line(DiffKind::Add, "a", None, Some(1)), line(DiffKind::Add, "b", None, Some(2))]);
        assert!(added_lines("").is_empty());
    }

    #[test]
    fn what_a_result_keeps_is_capped_and_lines_are_cut() {
        let many = added_lines(&(0..MAX_SHOWN_DIFF + 7).map(|i| format!("l{i}")).collect::<Vec<_>>().join("\n"));
        assert_eq!(shown_diff(&many), DiffShown { lines: many[..MAX_SHOWN_DIFF].to_vec(), more: 7 });
        let long = "x".repeat(MAX_SHOWN_LINE_CHARS * 10);
        assert_eq!(shown_diff(&[line(DiffKind::Add, &long, None, Some(1))]).lines[0].text.len(), MAX_SHOWN_LINE_CHARS);
    }

    #[test]
    fn a_collapsed_preview_keeps_a_changes_additions_in_view() {
        let lines = diff_text("a\nb\nc\nd\ne\nf\n", "x\ny\n", 3).lines;
        let shown: Vec<String> = preview_lines(&lines, 5)
            .iter()
            .map(|l| if l.kind == DiffKind::Gap { "…".into() } else { format!("{:?} {}", l.kind, l.text) })
            .collect();
        assert_eq!(shown, ["Del a", "…", "Add x", "Add y"]);
        assert_eq!(preview_lines(&lines, 8), lines);
        assert_eq!(preview_lines(&lines, 2).len(), 2);
    }

    #[test]
    fn a_big_rewrite_keeps_the_new_files_start() {
        let file = |p: &str| (1..=500).map(|i| format!("{p} {i}\n")).collect::<String>();
        let all = diff_text(&file("old"), &file("new"), 3).lines;
        let shown = shown_diff(&all);
        assert_eq!(shown.lines.len(), MAX_SHOWN_DIFF);
        assert_eq!(shown.lines[MAX_SHOWN_DIFF / 5], DiffLine::gap());
        assert_eq!(shown.lines[MAX_SHOWN_DIFF / 5 + 1], line(DiffKind::Add, "new 1", None, Some(1)));
        assert_eq!(shown.more, 1000 - (MAX_SHOWN_DIFF - 1));
    }
}
