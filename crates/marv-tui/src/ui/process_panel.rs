//! The processes view (new in marv-rs): what the status bar's "N running" is, opened with ctrl+p or a click on it.
//! The background processes the `process` tool started (Marv #51), each with its command, state, address and uptime;
//! under them, the last lines of the selected one's output, refreshed every second while the view is open. ↑/↓
//! picks one, `s` twice stops it, Esc closes the view (the processes keep running).
//!
//! Commands and output are the project's and the model's words: drawn through `printable()`, shown here only.

use crossterm::event::{KeyCode, KeyEvent};
use marv_core::printable;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Padding, Paragraph, Widget};

use super::select::truncate_end;
use super::theme;
use crate::view::ProcessesView;

/// Output lines shown under the list, at most.
pub const OUTPUT_ROWS: usize = 12;

/// What a key did in the view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessKey {
    /// Nothing for the App to do (redraw only).
    None,
    Close,
    /// Another process is selected: ask for its output.
    Selected(String),
    /// `s` pressed again on the same process: stop it.
    Stop(String),
}

#[derive(Debug, Default)]
pub struct ProcessPanel {
    pub view: ProcessesView,
    selected: usize,
    /// `s` was pressed once on this process: the next `s` stops it.
    confirm_stop: Option<String>,
}

impl ProcessPanel {
    /// The process whose output is shown.
    pub fn selected(&self) -> Option<String> {
        self.view.rows.get(self.selected).map(|r| r.name.clone())
    }

    /// A fresh list: the selection stays on the same process when it's still there.
    pub fn update(&mut self, view: ProcessesView) {
        let name = self.selected();
        self.view = view;
        self.selected = name.and_then(|n| self.view.rows.iter().position(|r| r.name == n)).unwrap_or(0);
        if self.confirm_stop.as_ref().is_some_and(|n| !self.view.rows.iter().any(|r| &r.name == n)) {
            self.confirm_stop = None;
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> ProcessKey {
        let count = self.view.rows.len();
        let moved = |to: usize, me: &mut Self| {
            me.confirm_stop = None;
            if to == me.selected || count == 0 {
                return ProcessKey::None;
            }
            me.selected = to;
            me.selected().map_or(ProcessKey::None, ProcessKey::Selected)
        };
        match key.code {
            KeyCode::Esc => ProcessKey::Close,
            KeyCode::Up | KeyCode::Char('k') => moved(self.selected.saturating_sub(1), self),
            KeyCode::Down | KeyCode::Char('j') => moved((self.selected + 1).min(count.saturating_sub(1)), self),
            KeyCode::Char('s') => {
                let Some(name) = self.selected() else { return ProcessKey::None };
                if self.confirm_stop.as_ref() == Some(&name) {
                    self.confirm_stop = None;
                    ProcessKey::Stop(name)
                } else {
                    self.confirm_stop = Some(name);
                    ProcessKey::None
                }
            }
            _ => ProcessKey::None,
        }
    }

    fn lines(&self) -> Vec<Line<'static>> {
        let running = self.view.rows.iter().filter(|r| r.running).count();
        let mut lines = vec![
            Line::from(vec![
                Span::styled("Background processes", Style::new().add_modifier(Modifier::BOLD)),
                Span::styled(format!("  {running} running"), Style::new().fg(theme::DIM)),
            ]),
            Line::default(),
        ];
        if self.view.rows.is_empty() {
            lines.push(Line::styled("  None: the process tool hasn't started any.", Style::new().fg(theme::DIM)));
        }
        let name_width = self.view.rows.iter().map(|r| printable(&r.name).chars().count()).max().unwrap_or(0);
        for (i, row) in self.view.rows.iter().enumerate() {
            let chosen = i == self.selected;
            let marker = if chosen { "› " } else { "  " };
            let name_style = if chosen { Style::new().fg(theme::ACCENT).add_modifier(Modifier::BOLD) } else { Style::new() };
            let status_style = Style::new().fg(if row.running { theme::ACCENT } else { theme::DIM });
            lines.push(Line::from(vec![
                Span::styled(marker, Style::new().fg(theme::ACCENT)),
                Span::styled(format!("{:name_width$}  ", printable(&row.name)), name_style),
                Span::styled(printable(&row.status), status_style),
            ]));
            lines.push(Line::from(vec![
                Span::raw(format!("  {:name_width$}  ", "")),
                Span::styled(format!("$ {}", printable(&row.command)), Style::new().fg(theme::DIM)),
            ]));
        }
        if let Some(name) = self.selected() {
            lines.push(Line::default());
            lines.push(Line::styled(format!("OUTPUT · {}", printable(&name)), Style::new().fg(theme::DIM)));
            let output = self.view.output.as_ref().filter(|(of, _)| *of == name).map(|(_, text)| text.as_str());
            let shown: Vec<&str> = output.map(|t| t.lines().collect()).unwrap_or_default();
            if shown.is_empty() {
                let text = if output.is_some() { "  No output yet." } else { "  Loading…" };
                lines.push(Line::styled(text, Style::new().fg(theme::DIM)));
            }
            let skip = shown.len().saturating_sub(OUTPUT_ROWS);
            lines.extend(shown[skip..].iter().map(|l| Line::raw(format!("  {}", printable(l)))));
        }
        lines.push(Line::default());
        let help = match &self.confirm_stop {
            Some(name) => Line::styled(
                format!("Press s again to stop {} · any other key keeps it", printable(name)),
                Style::new().fg(theme::WARNING),
            ),
            None => Line::styled("↑↓ pick · s stop · Esc to close (they keep running)", Style::new().fg(theme::DIM)),
        };
        lines.push(help);
        lines
    }

    /// Rows it needs: one per line plus the border, the output counted at its full height so the view doesn't jump.
    pub fn height(&self) -> u16 {
        let lines = self.lines().len();
        let output = self.view.output.as_ref().map_or(0, |(_, t)| t.lines().count().clamp(1, OUTPUT_ROWS));
        let room = if self.view.rows.is_empty() { 0 } else { OUTPUT_ROWS.saturating_sub(output.max(1)) };
        (lines + room) as u16 + 2
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let inner = area.width.saturating_sub(4);
        let lines: Vec<Line> = self.lines().into_iter().map(|l| truncate_end(l, inner)).collect();
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
    use crate::view::ProcessRow;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn row(name: &str, running: bool) -> ProcessRow {
        ProcessRow {
            name: name.into(),
            command: format!("serve {name}"),
            status: if running { "running · http://localhost:4173 · up 2m".into() } else { "exited (code 1) · up 5m".into() },
            running,
        }
    }

    fn text(panel: &ProcessPanel) -> String {
        let area = Rect::new(0, 0, 70, panel.height());
        let mut buf = Buffer::empty(area);
        panel.render(area, &mut buf);
        (0..area.height)
            .map(|y| (0..area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn it_shows_each_process_its_command_and_the_selected_ones_output() {
        let mut panel = ProcessPanel::default();
        panel.update(ProcessesView {
            rows: vec![row("serve-site", true), row("old", false)],
            output: Some(("serve-site".into(), "Serving site on http://localhost:4173\nGET / 200".into())),
        });
        let shown = text(&panel);
        assert!(shown.contains("› serve-site  running · http://localhost:4173 · up 2m"), "{shown}");
        assert!(shown.contains("$ serve serve-site"), "{shown}");
        assert!(shown.contains("old         exited (code 1)"), "{shown}");
        assert!(shown.contains("OUTPUT · serve-site"), "{shown}");
        assert!(shown.contains("GET / 200"), "{shown}");
        assert!(shown.contains("1 running"), "{shown}");
    }

    #[test]
    fn arrows_pick_another_process_and_s_twice_stops_it() {
        let mut panel = ProcessPanel::default();
        panel.update(ProcessesView { rows: vec![row("a", true), row("b", true)], output: None });
        assert_eq!(panel.on_key(key(KeyCode::Up)), ProcessKey::None, "already at the top");
        assert_eq!(panel.on_key(key(KeyCode::Down)), ProcessKey::Selected("b".into()));
        assert_eq!(panel.on_key(key(KeyCode::Down)), ProcessKey::None, "already at the bottom");
        assert!(text(&panel).contains("Loading…"), "no output for b yet");
        assert_eq!(panel.on_key(key(KeyCode::Char('s'))), ProcessKey::None);
        assert!(text(&panel).contains("Press s again to stop b"));
        assert_eq!(panel.on_key(key(KeyCode::Char('s'))), ProcessKey::Stop("b".into()));
        // Moving away forgets a half-made stop.
        panel.on_key(key(KeyCode::Char('s')));
        panel.on_key(key(KeyCode::Up));
        assert_eq!(panel.on_key(key(KeyCode::Char('s'))), ProcessKey::None);
        assert_eq!(panel.on_key(key(KeyCode::Esc)), ProcessKey::Close);
    }

    #[test]
    fn a_new_list_keeps_the_selection_on_the_same_process() {
        let mut panel = ProcessPanel::default();
        panel.update(ProcessesView { rows: vec![row("a", true), row("b", true)], output: None });
        panel.on_key(key(KeyCode::Down));
        panel.update(ProcessesView { rows: vec![row("new", true), row("a", true), row("b", true)], output: None });
        assert_eq!(panel.selected().as_deref(), Some("b"));
        panel.update(ProcessesView { rows: vec![row("a", true)], output: None });
        assert_eq!(panel.selected().as_deref(), Some("a"), "b went away");
    }

    #[test]
    fn with_none_it_says_so() {
        let panel = ProcessPanel::default();
        assert!(text(&panel).contains("None: the process tool hasn't started any."));
    }
}
