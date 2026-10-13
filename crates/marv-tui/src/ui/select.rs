//! A vertical list: ↑/↓ to move (wrapping), Enter to choose. Ports `src/ui/Select.tsx`.
//! The active row is `❯ ` in the accent; a hint follows ` · ` dimmed. Labels and hints go through
//! `printable()` (approval scopes can quote model text).

use crossterm::event::{KeyCode, KeyEvent};
use marv_core::printable;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::theme;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectItem<T> {
    pub value: T,
    pub label: String,
    pub hint: Option<String>,
}

impl<T> SelectItem<T> {
    pub fn new(value: T, label: impl Into<String>) -> Self {
        Self { value, label: label.into(), hint: None }
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

#[derive(Debug, Clone)]
pub struct Select<T> {
    pub items: Vec<SelectItem<T>>,
    pub index: usize,
}

impl<T: Clone + PartialEq> Select<T> {
    /// Starts on `initial` if it's there, else the first item.
    pub fn new(items: Vec<SelectItem<T>>, initial: Option<&T>) -> Self {
        let index = initial.and_then(|v| items.iter().position(|i| &i.value == v)).unwrap_or(0);
        Self { items, index }
    }

    /// ↑/↓ move; Enter returns the chosen value.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<T> {
        let n = self.items.len();
        if n == 0 {
            return None;
        }
        match key.code {
            KeyCode::Up => self.index = (self.index + n - 1) % n,
            KeyCode::Down => self.index = (self.index + 1) % n,
            KeyCode::Enter => return Some(self.items[self.index].value.clone()),
            _ => {}
        }
        None
    }

    /// The highlighted value.
    pub fn value(&self) -> Option<&T> {
        self.items.get(self.index).map(|i| &i.value)
    }

    pub fn render(&self) -> Vec<Line<'static>> {
        self.items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let active = i == self.index;
                let style = if active { Style::new().fg(theme::ACCENT) } else { Style::new() };
                let mut spans =
                    vec![Span::styled(if active { "❯ " } else { "  " }, style), Span::styled(printable(&item.label), style)];
                if let Some(hint) = &item.hint {
                    spans.push(Span::styled(format!(" · {}", printable(hint)), Style::new().fg(theme::DIM)));
                }
                Line::from(spans)
            })
            .collect()
    }
}

/// Word-wraps a styled line to `width` columns, like Ink's `wrap` (wrap-ansi): breaks at the last
/// space that fits (the space goes), and hard-breaks a word longer than the line. Shared by the
/// bottom panels (approval, setup, pickers), which size themselves to their wrapped height.
pub fn wrap(line: &Line<'_>, width: u16) -> Vec<Line<'static>> {
    let width = width.max(1) as usize;
    let cells: Vec<(String, Style, usize)> = line
        .spans
        .iter()
        .flat_map(|span| {
            let style = line.style.patch(span.style);
            span.content.graphemes(true).map(move |g| (g.to_string(), style, g.width()))
        })
        .collect();
    let mut rows: Vec<Vec<(String, Style, usize)>> = vec![Vec::new()];
    let mut used = 0;
    for cell in cells {
        let row = rows.last_mut().expect("never empty");
        if used + cell.2 > width && !row.is_empty() {
            // Break after the last space on this row, if there is one; the space itself goes.
            let next = match row.iter().rposition(|(g, _, _)| g == " ") {
                Some(at) => {
                    let rest = row.split_off(at + 1);
                    row.pop();
                    rest
                }
                None => Vec::new(),
            };
            used = next.iter().map(|c| c.2).sum();
            rows.push(next);
            if cell.0 == " " && rows.last().is_some_and(|r| r.is_empty()) {
                continue;
            }
        }
        used += cell.2;
        rows.last_mut().expect("never empty").push(cell);
    }
    rows.into_iter()
        .map(|row| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for (g, style, _) in row {
                match spans.last_mut() {
                    Some(last) if last.style == style => last.content.to_mut().push_str(&g),
                    _ => spans.push(Span::styled(g, style)),
                }
            }
            Line::from(spans)
        })
        .collect()
}

/// [`wrap`] over several lines.
pub fn wrap_all(lines: &[Line<'_>], width: u16) -> Vec<Line<'static>> {
    lines.iter().flat_map(|l| wrap(l, width)).collect()
}

/// A line cut to `width` columns with a trailing "…" (Ink's `truncate-end`).
pub fn truncate_end(line: Line<'static>, width: u16) -> Line<'static> {
    let width = width as usize;
    if line.width() <= width {
        return line;
    }
    let mut spans = Vec::new();
    let mut used = 0;
    'outer: for span in line.spans {
        let mut text = String::new();
        for g in span.content.graphemes(true) {
            if used + g.width() + 1 > width {
                spans.push(Span::styled(text, span.style));
                break 'outer;
            }
            used += g.width();
            text.push_str(g);
        }
        spans.push(Span::styled(text, span.style));
    }
    if width > 0 {
        spans.push(Span::raw("…"));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn select() -> Select<&'static str> {
        Select::new(vec![SelectItem::new("a", "A"), SelectItem::new("b", "B").hint("bee"), SelectItem::new("c", "C")], Some(&"b"))
    }

    #[test]
    fn starts_on_the_initial_value_and_wraps_both_ways() {
        let mut s = select();
        assert_eq!(s.value(), Some(&"b"));
        s.on_key(key(KeyCode::Down));
        s.on_key(key(KeyCode::Down));
        assert_eq!(s.on_key(key(KeyCode::Enter)), Some("a"));
        s.on_key(key(KeyCode::Up));
        assert_eq!(s.on_key(key(KeyCode::Enter)), Some("c"));
        assert_eq!(Select::new(vec![SelectItem::new(1, "x")], Some(&9)).index, 0);
    }

    #[test]
    fn marks_the_active_row_and_cleans_labels_and_hints() {
        let s = Select::new(vec![SelectItem::new(1, "ok\x1b[2J").hint("h\u{202e}int"), SelectItem::new(2, "two")], None);
        let rows: Vec<String> = s.render().iter().map(|l| l.to_string()).collect();
        assert_eq!(rows, ["❯ ok · hint", "  two"]);
    }

    #[test]
    fn wrap_breaks_at_spaces_and_hard_breaks_long_words() {
        let text = |lines: Vec<Line>| lines.iter().map(|l| l.to_string()).collect::<Vec<_>>();
        assert_eq!(text(wrap(&Line::raw("the quick brown fox"), 10)), ["the quick", "brown fox"]);
        assert_eq!(text(wrap(&Line::raw("abcdefghij"), 4)), ["abcd", "efgh", "ij"]);
        assert_eq!(text(wrap(&Line::raw(""), 4)), [""]);
        assert_eq!(text(wrap(&Line::raw("中文中文"), 5)), ["中文", "中文"]);
        assert_eq!(truncate_end(Line::raw("abcdef"), 4).to_string(), "abc…");
    }
}
