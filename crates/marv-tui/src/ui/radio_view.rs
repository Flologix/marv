//! The mesh radio (marv-rs #124, phase 1: presence; new in marv-rs). Two pieces:
//!
//! - [`strip`]: one line above the prompt while the radio is on, never more:
//!   ` ◉ radio · 58a7 working "fix the login test" · docs-11c2 idle · api-3f9a waiting · +1`. A name is shown short
//!   (`58a7` for `flologix-58a7`) when it shares this Marv's project prefix and the rest is unique; `silent` is
//!   greyed; a Marv that left shows `left` (or `left (lost)`) for a moment. Names that don't fit become `+N`.
//! - [`RadioPanel`]: `/radio`'s view, the roster with full names, panes, states, titles and how long each state has
//!   lasted. Esc closes it.
//!
//! Titles and projects are other Marvs' users' words: drawn through `printable()`, shown here only.

use crossterm::event::{KeyCode, KeyEvent};
use marv_core::printable;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Padding, Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

use super::select::truncate_end;
use super::theme;
use crate::view::{RadioPeer, RadioState, RadioView};

/// The strip's marker.
pub const MARK: &str = "◉";

/// `name` as the strip shows it: without this Marv's own `<project>-` prefix when what's left is unique.
pub fn short_name(view: &RadioView, name: &str) -> String {
    let prefix = view.me.rsplit_once('-').map(|(base, _)| format!("{base}-"));
    let short = |n: &str| prefix.as_deref().and_then(|p| n.strip_prefix(p)).unwrap_or(n).to_string();
    let mine = short(name);
    let unique = view.peers.iter().filter(|p| short(&p.name) == mine).count() == 1
        && !view.peers.iter().any(|p| p.name == mine && p.name != name);
    if unique && !mine.is_empty() { mine } else { name.to_string() }
}

fn state_style(state: RadioState) -> Style {
    match state {
        RadioState::Working => Style::new().fg(theme::ACCENT),
        RadioState::Waiting => Style::new().fg(theme::WARNING),
        RadioState::Idle => Style::new(),
        RadioState::Silent | RadioState::Left | RadioState::Lost => Style::new().fg(theme::DIM),
    }
}

fn peer_spans(view: &RadioView, peer: &RadioPeer) -> Vec<Span<'static>> {
    let dim = matches!(peer.state, RadioState::Silent | RadioState::Left | RadioState::Lost);
    let name_style = if dim { Style::new().fg(theme::DIM) } else { Style::new().add_modifier(Modifier::BOLD) };
    let mut spans = vec![
        Span::styled(printable(&short_name(view, &peer.name)), name_style),
        Span::raw(" "),
        Span::styled(peer.state.word(), state_style(peer.state)),
    ];
    let title = printable(&peer.title);
    if peer.state == RadioState::Working && !title.trim().is_empty() {
        spans.push(Span::styled(format!(" \"{}\"", title.trim()), Style::new().fg(theme::DIM)));
    }
    spans
}

fn width(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.content.width()).sum()
}

/// The one line above the prompt, `width` columns at most: as many Marvs as fit, then `+N`.
pub fn strip(view: &RadioView, width_cols: u16) -> Line<'static> {
    let budget = width_cols as usize;
    let sep = || Span::styled(" · ", Style::new().fg(theme::DIM));
    let mut spans = vec![Span::styled(format!(" {MARK} radio"), Style::new().fg(theme::ACCENT))];
    if view.peers.is_empty() {
        spans.push(sep());
        spans.push(Span::styled("no other Marv on it yet", Style::new().fg(theme::DIM)));
        return truncate_end(Line::from(spans), width_cols);
    }
    for (i, peer) in view.peers.iter().enumerate() {
        let left = view.peers.len() - i;
        let item = peer_spans(view, peer);
        // Room for this one, and for a "+N" after it unless it's the last.
        let more = if left > 1 { format!(" · +{}", left - 1).width() } else { 0 };
        if width(&spans) + 3 + width(&item) + more > budget && i > 0 {
            spans.push(sep());
            spans.push(Span::styled(format!("+{left}"), Style::new().fg(theme::DIM)));
            return truncate_end(Line::from(spans), width_cols);
        }
        spans.push(sep());
        spans.extend(item);
    }
    truncate_end(Line::from(spans), width_cols)
}

/// "12m", "3h", "2d": how long a state has lasted.
pub fn age(since_ms: u64, now_ms: u64) -> String {
    let secs = now_ms.saturating_sub(since_ms) / 1000;
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// `/radio`: the roster. Esc closes it.
pub struct RadioPanel;

impl RadioPanel {
    pub fn on_key(&self, key: KeyEvent) -> bool {
        key.code == KeyCode::Esc
    }

    fn lines(view: &RadioView, now_ms: u64) -> Vec<Line<'static>> {
        let count = view.peers.iter().filter(|p| !matches!(p.state, RadioState::Left | RadioState::Lost)).count() + 1;
        let mut lines = vec![
            Line::from(vec![
                Span::styled("Radio", Style::new().add_modifier(Modifier::BOLD)),
                Span::styled(format!("  on · {count} Marv{}", if count == 1 { "" } else { "s" }), Style::new().fg(theme::DIM)),
            ]),
            Line::default(),
            Line::styled("ON THE RADIO", Style::new().fg(theme::DIM)),
        ];
        let name_width = view.peers.iter().map(|p| p.name.width()).chain([view.me.width()]).max().unwrap_or(0);
        let pane_width = view.peers.iter().filter_map(|p| p.pane.as_ref().map(|s| s.width())).max().unwrap_or(1).max(3);
        let row = |name: &str, pane: &str, state: &str, state_style: Style, title: &str, age: String| {
            Line::from(vec![
                Span::raw(format!("  {:name_width$}  ", printable(name))),
                Span::styled(format!("{:pane_width$}  ", printable(pane)), Style::new().fg(theme::DIM)),
                Span::styled(format!("{state:11}"), state_style),
                Span::raw(if title.is_empty() { String::new() } else { format!("\"{}\"", printable(title)) }),
                Span::styled(if age.is_empty() { age } else { format!("  {age}") }, Style::new().fg(theme::DIM)),
            ])
        };
        lines.push(row(&view.me, "you", "", Style::new(), &view.my_title, String::new()));
        for p in &view.peers {
            let pane = p.pane.clone().unwrap_or_else(|| "—".into());
            lines.push(row(&p.name, &pane, p.state.word(), state_style(p.state), &p.title, age(p.since_ms, now_ms)));
        }
        if view.peers.is_empty() {
            lines.push(Line::styled("  No other Marv is on the radio yet.", Style::new().fg(theme::DIM)));
        }
        lines.push(Line::default());
        lines.push(Line::styled("/radio off to leave · Esc to close", Style::new().fg(theme::DIM)));
        lines
    }

    /// Rows it needs: one per line plus the border.
    pub fn height(view: &RadioView) -> u16 {
        Self::lines(view, 0).len() as u16 + 2
    }

    pub fn render(view: &RadioView, now_ms: u64, area: Rect, buf: &mut Buffer) {
        let inner = area.width.saturating_sub(4);
        let lines: Vec<Line> = Self::lines(view, now_ms).into_iter().map(|l| truncate_end(l, inner)).collect();
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(theme::ACCENT))
            .padding(Padding::horizontal(1));
        Paragraph::new(lines).block(block).render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(name: &str, state: RadioState, title: &str) -> RadioPeer {
        RadioPeer { name: name.into(), state, title: title.into(), project: "p".into(), pane: None, since_ms: 0 }
    }

    fn view(peers: Vec<RadioPeer>) -> RadioView {
        RadioView { on: true, me: "flologix-c792".into(), my_title: "mine".into(), peers }
    }

    #[test]
    fn short_names_drop_this_marvs_prefix_when_unique() {
        let v = view(vec![peer("flologix-58a7", RadioState::Idle, ""), peer("docs-11c2", RadioState::Idle, "")]);
        assert_eq!(short_name(&v, "flologix-58a7"), "58a7");
        assert_eq!(short_name(&v, "docs-11c2"), "docs-11c2");
        let clash = view(vec![peer("flologix-58a7", RadioState::Idle, ""), peer("58a7", RadioState::Idle, "")]);
        assert_eq!(short_name(&clash, "flologix-58a7"), "flologix-58a7");
    }

    #[test]
    fn the_strip_is_one_line_with_states_titles_for_work_and_a_count_for_the_rest() {
        let v = view(vec![
            peer("flologix-58a7", RadioState::Working, "fix the login test\x1b[2J"),
            peer("docs-11c2", RadioState::Idle, "update the README"),
            peer("api-3f9a", RadioState::Waiting, "x"),
            peer("old-0001", RadioState::Lost, ""),
        ]);
        let wide = strip(&v, 200).to_string();
        assert_eq!(
            wide,
            " ◉ radio · 58a7 working \"fix the login test\" · docs-11c2 idle · api-3f9a waiting · old-0001 left (lost)"
        );
        let narrow = strip(&v, 72).to_string();
        assert!(narrow.ends_with("· +2") && narrow.width() <= 72, "{narrow}");
        assert_eq!(strip(&view(vec![]), 80).to_string(), " ◉ radio · no other Marv on it yet");
    }

    #[test]
    fn the_panel_lists_full_names_and_says_how_to_leave() {
        let mut p = peer("flologix-58a7", RadioState::Working, "fix the login test");
        p.pane = Some("w3_y-p1".into());
        p.since_ms = 0;
        let v = view(vec![p]);
        let area = Rect::new(0, 0, 90, RadioPanel::height(&v));
        let mut buf = Buffer::empty(area);
        RadioPanel::render(&v, 12 * 60_000, area, &mut buf);
        let screen: String =
            (0..area.height).map(|y| (0..area.width).map(|x| buf[(x, y)].symbol()).collect::<String>() + "\n").collect();
        assert!(screen.contains("on · 2 Marvs"), "{screen}");
        assert!(screen.contains("flologix-c792  you"), "{screen}");
        assert!(screen.contains("flologix-58a7  w3_y-p1  working    \"fix the login test\"  12m"), "{screen}");
        assert!(screen.contains("/radio off to leave · Esc to close"), "{screen}");
    }
}
