//! How `edit_file` finds `old_string` in a file.
//!
//! Ports Marv's `src/tools/edit-match.ts` (fully, with its tests).
//!
//! Exact first; when that finds nothing, two forgiving matches for the way models most often copy text wrong, each on
//! whole lines and only when it's unique:
//! - trailing whitespace dropped (editors and models both strip it);
//! - indentation changed: spaces for the file's tabs, or the block copied without its outer indentation. The
//!   replacement is re-indented by the one column shift (tabs 4, 2 or 8 wide) that explains every matched line, in
//!   the file's tabs or spaces, so the file keeps its own style. An unindented first line is `old_string` starting at
//!   the code: it isn't counted, and `new_string`'s first line gets the file's indentation there.
//! - marv-rs (Marv #60): before those two, a tier for typographic look-alikes. Models rewrite `"` as `“ ”`, `'` as
//!   `’`, `-` as `–`/`—`, `...` as `…`, or type a non-breaking space, and then `old_string` misses a file that has
//!   the ASCII forms (or the other way round). Both sides are folded to ASCII ([`fold_unicode`]) and compared; the
//!   match may be part of a line, like an exact one. The **original** span of the file is replaced and `new_string`
//!   is written as given: a file that really uses curly quotes keeps them outside the edited span. Unique only, like
//!   the other tiers. Order: exact, Unicode, trailing whitespace, indentation.
//!
//! In a file whose line breaks are all CRLF, `old_string` and `new_string` get CRLF too: a model writes `\n`, and an
//! edit that inserts bare `\n` leaves mixed line endings (an eval run spent 6 of 11 steps repairing that).
//!
//! When nothing matches, [`closest_lines`] finds the part of the file most like `old_string`, so the error can show
//! the model the current text instead of sending it back to `read_file`.
//!
//! Port note: offsets are byte offsets and similarity bigrams are `char` pairs (Marv: UTF-16 units); results are the
//! same for any text the tests cover.

/// Which forgiving match applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fuzzy {
    /// marv-rs (Marv #60): typographic quotes, dashes, the ellipsis and Unicode spaces read as ASCII.
    Unicode,
    TrailingWhitespace,
    Indentation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditResult {
    Applied { text: String, count: usize, fuzzy: Option<Fuzzy> },
    NotFound,
    Ambiguous { count: usize },
}

/// `"\r\n"` when every line break in the text is one; otherwise `"\n"` (a mixed file is left as it is).
pub fn line_ending(text: &str) -> &'static str {
    let crlf = text.matches("\r\n").count();
    if crlf > 0 && crlf == text.matches('\n').count() { "\r\n" } else { "\n" }
}

/// The text with its bare `\n` turned into `eol` (only ever CRLF; LF text is returned as it is).
pub fn with_line_ending(text: &str, eol: &str) -> String {
    if eol == "\n" {
        return text.to_string();
    }
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

fn leading(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

/// How many columns an indentation takes, with tabs `width` wide.
fn columns(indent: &str, width: usize) -> usize {
    indent.chars().fold(0, |col, ch| if ch == '\t' { col + width - (col % width) } else { col + 1 })
}

type Reindent = Box<dyn Fn(&str, usize) -> String>;

/// A function that re-indents a line of `new_string` the way the file's lines differ from `old_string`'s, or None
/// when no single rule explains every line.
fn reindenter(file_lines: &[&str], old_lines: &[&str]) -> Option<Reindent> {
    let from_code = leading(old_lines[0]).is_empty() && !leading(file_lines[0]).is_empty();
    let pairs: Vec<(&str, &str)> = file_lines
        .iter()
        .zip(old_lines)
        .enumerate()
        .filter(|(i, (line, _))| !line.trim().is_empty() && !(from_code && *i == 0))
        .map(|(_, (l, o))| (*l, *o))
        .collect();
    let tabs = file_lines.iter().any(|l| leading(l).contains('\t'));
    for width in [4usize, 2, 8] {
        let mut shifts: Vec<isize> =
            pairs.iter().map(|(l, o)| columns(leading(l), width) as isize - columns(leading(o), width) as isize).collect();
        shifts.sort_unstable();
        shifts.dedup();
        if shifts.len() > 1 {
            continue;
        }
        let shift = shifts.first().copied().unwrap_or(0);
        let first_indent = leading(file_lines[0]).to_string();
        return Some(Box::new(move |line: &str, index: usize| {
            if line.trim().is_empty() {
                return String::new();
            }
            if from_code && index == 0 {
                return format!("{first_indent}{}", line.trim_start());
            }
            let col = (columns(leading(line), width) as isize + shift).max(0) as usize;
            let indent = if tabs { format!("{}{}", "\t".repeat(col / width), " ".repeat(col % width)) } else { " ".repeat(col) };
            format!("{indent}{}", line.trim_start())
        }));
    }
    None
}

/// How two lines compare in a forgiving tier.
type LineEq = fn(&str, &str) -> bool;

/// Where whole lines of the file match `old_string`'s lines under `same`: the first line of each, without overlaps.
fn line_matches(file_lines: &[&str], old_lines: &[&str], same: LineEq) -> Vec<usize> {
    let mut found = Vec::new();
    let mut i = 0;
    while i + old_lines.len() <= file_lines.len() {
        if old_lines.iter().enumerate().all(|(j, old)| same(file_lines[i + j], old)) {
            found.push(i);
            i += old_lines.len();
        } else {
            i += 1;
        }
    }
    found
}

/// `old_string` replaced by `new_string` in `text` (exactly, then forgivingly), or why it couldn't be.
pub fn apply_edit(text: &str, old_string: &str, new_string: &str, replace_all: bool) -> EditResult {
    let eol = line_ending(text);
    let old = with_line_ending(old_string, eol);
    let replacement = with_line_ending(new_string, eol);
    if old.is_empty() {
        return EditResult::NotFound;
    }

    let exact = text.matches(old.as_str()).count();
    if exact > 1 && !replace_all {
        return EditResult::Ambiguous { count: exact };
    }
    if exact > 0 {
        // `$` in new_string is literal (Rust's replace has no patterns; Marv needed a function replacer for this).
        let text = if replace_all { text.replace(&old, &replacement) } else { text.replacen(&old, &replacement, 1) };
        return EditResult::Applied { text, count: exact, fuzzy: None };
    }

    if let Some(result) = unicode_match(text, &old, &replacement, replace_all) {
        return result;
    }

    // Whole lines from here on. A trailing line break in old_string stays part of what's replaced, as it would be in
    // an exact match.
    let trailing = old.ends_with(eol);
    let old_body = if trailing { &old[..old.len() - eol.len()] } else { old.as_str() };
    let old_lines: Vec<&str> = old_body.split(eol).collect();
    let file_lines: Vec<&str> = text.split(eol).collect();
    let stages: [(Fuzzy, LineEq); 2] =
        [(Fuzzy::TrailingWhitespace, |a, b| a.trim_end() == b.trim_end()), (Fuzzy::Indentation, |a, b| a.trim() == b.trim())];
    for (fuzzy, same) in stages {
        let starts = line_matches(&file_lines, &old_lines, same);
        if starts.is_empty() {
            continue;
        }
        if starts.len() > 1 && !replace_all {
            return EditResult::Ambiguous { count: starts.len() };
        }
        let mut new_lines: Vec<String> = replacement.split(eol).map(str::to_string).collect();
        if fuzzy == Fuzzy::Indentation {
            // Every match must re-indent the same way; one rule from the first is applied to all.
            let block = |s: usize| &file_lines[s..s + old_lines.len()];
            let Some(reindent) = reindenter(block(starts[0]), &old_lines) else { return EditResult::NotFound };
            if starts.iter().any(|&s| reindenter(block(s), &old_lines).is_none()) {
                return EditResult::NotFound;
            }
            new_lines = new_lines.iter().enumerate().map(|(i, l)| reindent(l, i)).collect();
        }
        // Splice by offsets: each matched block runs from its first line's start to its last line's end, plus that
        // line's break when old_string ended with one.
        let mut line_start = vec![0usize];
        for line in &file_lines {
            line_start.push(line_start.last().unwrap() + line.len() + eol.len());
        }
        let inserted = new_lines.join(eol);
        let mut out = String::with_capacity(text.len());
        let mut at = 0;
        for &start in &starts {
            let last = start + old_lines.len() - 1;
            let mut end = line_start[last] + file_lines[last].len();
            if trailing && last < file_lines.len() - 1 {
                end += eol.len();
            }
            out.push_str(&text[at..line_start[start]]);
            out.push_str(&inserted);
            at = end;
        }
        out.push_str(&text[at..]);
        return EditResult::Applied { text: out, count: starts.len(), fuzzy: Some(fuzzy) };
    }
    EditResult::NotFound
}

/// The ASCII a typographic character stands for, or None for any other character.
fn ascii_for(ch: char) -> Option<&'static str> {
    Some(match ch {
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' | '\u{2032}' => "'",
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' | '\u{2033}' => "\"",
        '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}' | '\u{2212}' => "-",
        '\u{2026}' => "...",
        '\u{00A0}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => " ",
        _ => return None,
    })
}

/// The text with its typographic quotes, dashes, ellipses and Unicode spaces as ASCII.
pub fn fold_unicode(text: &str) -> String {
    folded_with_offsets(text).0
}

/// The folded text, and for each of its byte offsets (plus its end) the original offset it came from, or None
/// inside an expansion (the 2nd and 3rd dot of a folded `…`), where a match can't start or end.
fn folded_with_offsets(text: &str) -> (String, Vec<Option<usize>>) {
    let mut out = String::with_capacity(text.len());
    let mut offsets = Vec::with_capacity(text.len() + 1);
    for (at, ch) in text.char_indices() {
        let start = out.len();
        match ascii_for(ch) {
            Some(ascii) => out.push_str(ascii),
            None => out.push(ch),
        }
        offsets.push(Some(at));
        offsets.extend(std::iter::repeat_n(None, out.len() - start - 1));
    }
    offsets.push(Some(text.len()));
    (out, offsets)
}

/// Whether two strings are the same once Unicode look-alikes are folded to ASCII (Marv #60).
pub fn unicode_equivalent(a: &str, b: &str) -> bool {
    fold_unicode(a) == fold_unicode(b)
}

/// The Unicode tier: `old` found in `text` once both are folded, the file's original spans replaced. None when it
/// doesn't match (the next tier is tried).
fn unicode_match(text: &str, old: &str, replacement: &str, replace_all: bool) -> Option<EditResult> {
    let (folded, offsets) = folded_with_offsets(text);
    let needle = fold_unicode(old);
    // Spans of the original text; a match that starts or ends inside a folded `…` isn't one.
    let spans: Vec<(usize, usize)> =
        folded.match_indices(needle.as_str()).filter_map(|(at, m)| Some((offsets[at]?, offsets[at + m.len()]?))).collect();
    if spans.is_empty() {
        return None;
    }
    if spans.len() > 1 && !replace_all {
        return Some(EditResult::Ambiguous { count: spans.len() });
    }
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for &(start, end) in &spans {
        out.push_str(&text[at..start]);
        out.push_str(replacement);
        at = end;
    }
    out.push_str(&text[at..]);
    Some(EditResult::Applied { text: out, count: spans.len(), fuzzy: Some(Fuzzy::Unicode) })
}

fn bigrams(line: &str) -> std::collections::HashSet<(char, char)> {
    let chars: Vec<char> = line.chars().collect();
    chars.windows(2).map(|w| (w[0], w[1])).collect()
}

/// 0 (nothing in common) to 1 (the same, ignoring surrounding whitespace).
fn similarity(
    a: &str,
    b: &str,
    set_a: &std::collections::HashSet<(char, char)>,
    set_b: &std::collections::HashSet<(char, char)>,
) -> f64 {
    if a == b {
        return 1.0;
    }
    if set_a.is_empty() || set_b.is_empty() {
        return 0.0;
    }
    let shared = set_a.iter().filter(|p| set_b.contains(p)).count();
    (2 * shared) as f64 / (set_a.len() + set_b.len()) as f64
}

const MAX_SCAN_LINES: usize = 20_000;
const MAX_OLD_LINES: usize = 200;
/// Below this, the "closest" text is probably unrelated and showing it would only mislead.
const MIN_SIMILARITY: f64 = 0.5;

fn split_lines(text: &str) -> impl Iterator<Item = &str> {
    text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l))
}

/// The 1-based, inclusive line range of `text` most like `old_string`, or None if nothing is close.
pub fn closest_lines(text: &str, old_string: &str) -> Option<(usize, usize)> {
    let file_lines: Vec<&str> = split_lines(text).map(str::trim).collect();
    let old_trimmed = old_string.strip_suffix('\n').map(|s| s.strip_suffix('\r').unwrap_or(s)).unwrap_or(old_string);
    let old_lines: Vec<&str> = split_lines(old_trimmed).map(str::trim).collect();
    if text.is_empty() || old_string.trim().is_empty() || file_lines.len() > MAX_SCAN_LINES || old_lines.len() > MAX_OLD_LINES {
        return None;
    }
    let k = old_lines.len().min(file_lines.len());
    let file_sets: Vec<_> = file_lines.iter().map(|l| bigrams(l)).collect();
    let old_sets: Vec<_> = old_lines.iter().map(|l| bigrams(l)).collect();
    let (mut best_score, mut best_start) = (0.0f64, 0usize);
    for i in 0..=(file_lines.len() - k) {
        let total: f64 = (0..k).map(|j| similarity(file_lines[i + j], old_lines[j], &file_sets[i + j], &old_sets[j])).sum();
        let score = total / old_lines.len() as f64;
        if score > best_score {
            best_score = score;
            best_start = i;
        }
    }
    (best_score >= MIN_SIMILARITY).then_some((best_start + 1, best_start + k))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(text: &str, old: &str, new: &str) -> EditResult {
        apply_edit(text, old, new, false)
    }
    fn applied(result: EditResult) -> (String, usize, Option<Fuzzy>) {
        match result {
            EditResult::Applied { text, count, fuzzy } => (text, count, fuzzy),
            other => panic!("expected an applied edit, got {other:?}"),
        }
    }

    #[test]
    fn a_file_is_crlf_only_when_every_line_break_is() {
        assert_eq!(line_ending("a\r\nb\r\n"), "\r\n");
        assert_eq!(line_ending("a\nb\n"), "\n");
        assert_eq!(line_ending("a\r\nb\n"), "\n"); // mixed: left as it is
        assert_eq!(line_ending("one line"), "\n");
    }

    #[test]
    fn with_line_ending_converts_lf_without_doubling_crlf() {
        assert_eq!(with_line_ending("a\nb\r\nc", "\r\n"), "a\r\nb\r\nc");
        assert_eq!(with_line_ending("a\r\nb", "\n"), "a\r\nb"); // LF files are never touched
    }

    #[test]
    fn a_crlf_file_keeps_crlf_in_what_an_edit_inserts() {
        // A model's edit put bare \n into a CRLF file; it then spent 6 steps repairing it (eval, 2026-10-06).
        let result = edit("a\r\nreturn x;\r\nb\r\n", "return x;", "if (!x) return 0;\nreturn x;");
        assert_eq!(
            result,
            EditResult::Applied { text: "a\r\nif (!x) return 0;\r\nreturn x;\r\nb\r\n".into(), count: 1, fuzzy: None }
        );
    }

    #[test]
    fn an_lf_old_string_matches_across_a_crlf_files_lines() {
        assert_eq!(applied(edit("a\r\nb\r\nc\r\n", "a\nb", "x\ny")).0, "x\r\ny\r\nc\r\n");
    }

    #[test]
    fn exact_matches_come_first_and_replace_all_replaces_every_one() {
        assert_eq!(applied(apply_edit("foo bar foo", "foo", "baz", true)), ("baz bar baz".into(), 2, None));
        assert_eq!(edit("a  \nb", "a  \nb", "c"), EditResult::Applied { text: "c".into(), count: 1, fuzzy: None });
    }

    #[test]
    fn more_than_one_without_replace_all_is_ambiguous() {
        assert_eq!(edit("x\nx\n", "x", "y"), EditResult::Ambiguous { count: 2 });
    }

    #[test]
    fn dollar_in_new_string_is_literal() {
        assert_eq!(applied(edit("a", "a", "$&$1")).0, "$&$1");
    }

    #[test]
    fn trailing_whitespace_is_ignored_when_nothing_matches_exactly() {
        let file = "const a = {   \n  timeout: 30,    \n};\n";
        let (text, count, fuzzy) = applied(edit(file, "const a = {\n  timeout: 30,\n};", "const a = {\n  timeout: 60,\n};"));
        assert_eq!((text.as_str(), count, fuzzy), ("const a = {\n  timeout: 60,\n};\n", 1, Some(Fuzzy::TrailingWhitespace)));
    }

    #[test]
    fn a_trailing_newline_in_old_string_takes_the_line_break_with_it() {
        assert_eq!(applied(edit("a  \nb\nc\n", "a\nb\n", "x\n")).0, "x\nc\n");
    }

    const TABBED: &str = "function f() {\n\tif (x) {\n\t\treturn 1;\n\t}\n}\n";

    #[test]
    fn spaces_for_tabs_are_matched_and_reindented_with_tabs() {
        let (text, _, fuzzy) = applied(edit(
            TABBED,
            "    if (x) {\n        return 1;\n    }",
            "    if (x) {\n        return 2;\n    }\n    return 0;",
        ));
        assert_eq!(text, "function f() {\n\tif (x) {\n\t\treturn 2;\n\t}\n\treturn 0;\n}\n");
        assert_eq!(fuzzy, Some(Fuzzy::Indentation));
    }

    #[test]
    fn two_space_indentation_for_a_tab_file_works_too() {
        assert_eq!(
            applied(edit(TABBED, "  if (x) {\n    return 1;\n  }", "  if (x) {\n    return 3;\n  }")).0,
            "function f() {\n\tif (x) {\n\t\treturn 3;\n\t}\n}\n"
        );
    }

    #[test]
    fn a_block_shifted_by_a_constant_amount() {
        let file = "class A {\n    m() {\n        return 1;\n    }\n}\n";
        let (text, _, fuzzy) = applied(edit(file, "m() {\n    return 1;\n}", "m() {\n    return 2;\n}"));
        assert_eq!(text, "class A {\n    m() {\n        return 2;\n    }\n}\n");
        assert_eq!(fuzzy, Some(Fuzzy::Indentation));
    }

    #[test]
    fn blank_lines_in_new_string_stay_empty() {
        assert_eq!(
            applied(edit(TABBED, "  if (x) {\n    return 1;\n  }", "  if (x) {\n\n    return 1;\n  }")).0,
            "function f() {\n\tif (x) {\n\n\t\treturn 1;\n\t}\n}\n"
        );
    }

    #[test]
    fn first_line_from_its_code_the_rest_one_tab_too_deep() {
        // Every model that failed the eval's tab task wrote this: read_file's old tab separator taken for indentation
        // on every line but the first, where copying started at the code (eval, 2026-10-06).
        let file = "function f() {\n\tconst a = 1;\n\treturn a;\n}\n";
        let (text, _, fuzzy) = applied(edit(file, "const a = 1;\n\t\treturn a;", "const a = 2;\n\t\treturn a;"));
        assert_eq!(text, "function f() {\n\tconst a = 2;\n\treturn a;\n}\n");
        assert_eq!(fuzzy, Some(Fuzzy::Indentation));
    }

    #[test]
    fn inconsistent_indentation_isnt_guessed_at() {
        let file = "a {\n    b\n    c\n    d\n}\n";
        assert_eq!(edit(file, "    b\n        c\n    d", "B\n        C\n    D"), EditResult::NotFound);
    }

    #[test]
    fn a_fuzzy_match_that_isnt_unique_is_ambiguous() {
        assert_eq!(edit("\tx();\n\ty();\n\tx();\n", "  x();", "  z();"), EditResult::Ambiguous { count: 2 });
    }

    #[test]
    fn part_of_a_line_only_matches_exactly() {
        assert_eq!(edit("\tconst  x = 1;\n", "const x", "const y"), EditResult::NotFound);
    }

    #[test]
    fn curly_quotes_in_old_string_match_the_files_ascii_quotes() {
        let file = "const greeting = \"Hello\";\nconst name = 'Marv';\n";
        let (text, count, fuzzy) = applied(edit(file, "const greeting = \u{201C}Hello\u{201D};", "const greeting = \"Hi\";"));
        assert_eq!((text.as_str(), count, fuzzy), ("const greeting = \"Hi\";\nconst name = 'Marv';\n", 1, Some(Fuzzy::Unicode)));
        assert_eq!(
            applied(edit(file, "name = \u{2018}Marv\u{2019}", "name = 'M'")).0,
            "const greeting = \"Hello\";\nconst name = 'M';\n"
        );
    }

    #[test]
    fn ascii_in_old_string_matches_the_files_typographic_characters_which_stay_outside_the_span() {
        // A file that really uses curly quotes keeps them; only the matched span is replaced, new_string as given.
        let file = "title: \u{201C}Intro\u{201D}\nnote: \u{201C}keep\u{201D}\n";
        assert_eq!(
            applied(edit(file, "title: \"Intro\"", "title: \"Start\"")).0,
            "title: \"Start\"\nnote: \u{201C}keep\u{201D}\n"
        );
    }

    #[test]
    fn dashes_the_ellipsis_and_unicode_spaces_fold_to_ascii() {
        assert_eq!(applied(edit("a - b\n", "a \u{2013} b", "x")).0, "x\n"); // en dash
        assert_eq!(applied(edit("a -- b\n", "a \u{2014}\u{2014} b", "x")).0, "x\n"); // em dashes
        assert_eq!(applied(edit("wait...\n", "wait\u{2026}", "x")).0, "x\n");
        assert_eq!(applied(edit("wait\u{2026} ok\n", "wait...", "x")).0, "x ok\n");
        assert_eq!(applied(edit("let a = 1;\n", "let\u{00A0}a = 1;", "x")).0, "x\n"); // NBSP
        assert_eq!(applied(edit("let\u{202F}a = 1;\n", "let a = 1;", "x")).0, "x\n"); // narrow NBSP in the file
        assert!(unicode_equivalent("\u{2018}a\u{2019} \u{2014} b\u{2026}", "'a' - b..."));
    }

    #[test]
    fn a_match_inside_a_folded_ellipsis_isnt_one() {
        // "wait.." would end between the dots of the file's "…".
        assert_eq!(edit("wait\u{2026}\n", "wait..", "x"), EditResult::NotFound);
    }

    #[test]
    fn a_unicode_match_that_isnt_unique_is_ambiguous_and_replace_all_takes_both() {
        let file = "say(\"hi\");\nsay(\"hi\");\n";
        assert_eq!(edit(file, "say(\u{201C}hi\u{201D});", "x"), EditResult::Ambiguous { count: 2 });
        assert_eq!(applied(apply_edit(file, "say(\u{201C}hi\u{201D});", "x", true)), ("x\nx\n".into(), 2, Some(Fuzzy::Unicode)));
    }

    #[test]
    fn the_unicode_tier_comes_before_whitespace_and_indentation() {
        // A match the Unicode tier finds is reported as Unicode, never as a whitespace tier.
        assert_eq!(applied(edit("\tx = \"a\";\n", "x = \u{201C}a\u{201D};", "y")).2, Some(Fuzzy::Unicode));
        // Unicode and indentation both wrong: no tier explains it.
        assert_eq!(edit("\tx = \"a\";\n\ty();\n", "    x = \u{201C}a\u{201D};\n    y();", "z"), EditResult::NotFound);
    }

    const FILE: &str = "function a() {\n  return 1;\n}\n\nfunction greet(name) {\n  return `Hello, ${name}!`;\n}";

    #[test]
    fn closest_lines_finds_the_most_similar_window() {
        assert_eq!(closest_lines(FILE, "function greet(person) {\n  return `Hi, ${person}!`;\n}"), Some((5, 7)));
    }

    #[test]
    fn closest_lines_nothing_close_or_empty() {
        assert_eq!(closest_lines(FILE, "SELECT * FROM users WHERE id = 1;"), None);
        assert_eq!(closest_lines("", "x"), None);
    }
}
