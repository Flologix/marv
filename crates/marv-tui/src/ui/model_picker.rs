//! Type to filter, ↑/↓ to move, Enter to choose. Ports `src/ui/ModelPicker.tsx` (tests in
//! `tests/setup.test.tsx`, `tests/models.test.ts`).
//!
//! - The list is loaded by the engine ([`crate::view::SessionPort::load_models`]; OpenRouter has
//!   hundreds, Ollama what you've pulled) and arrives as `UiEvent::Models`; "Loading models…"
//!   until then, and on failure "Couldn't load models: … · type a model ID and press Enter".
//! - Every space-separated term must appear in the id ([`filter_models`]: "claude sonnet" matches
//!   "anthropic/claude-sonnet-5.5"). If nothing matches (or the list couldn't load), Enter uses
//!   what was typed as the model ID.
//! - [`VISIBLE`] rows, scrolled so the highlighted row stays in view; ids padded to the longest
//!   shown (max 44); price `$2 / $10`, `free` or `local` dimmed; `no tool support` in warning.
//! - Footer: `12 of 340 models · prices per million tokens, in / out`.
//! - Model ids come from a provider's list: they go through `printable()`.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use marv_core::printable;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::select::truncate_end;
use super::theme;
use crate::view::ModelInfo;

pub const VISIBLE: usize = 8;

/// Every space-separated term must appear in the id (case-insensitive).
pub fn filter_models<'a>(models: &'a [ModelInfo], query: &str) -> Vec<&'a ModelInfo> {
    let query = query.to_lowercase();
    let terms: Vec<&str> = query.split_whitespace().collect();
    models
        .iter()
        .filter(|m| {
            let id = m.id.to_lowercase();
            terms.iter().all(|t| id.contains(t))
        })
        .collect()
}

/// JS `Number(n.toPrecision(p))`: `p` significant digits, trailing zeros dropped.
fn precision(n: f64, p: i32) -> String {
    if n == 0.0 {
        return "0".into();
    }
    let magnitude = n.abs().log10().floor() as i32;
    let decimals = (p - 1 - magnitude).max(0) as usize;
    let s = format!("{n:.decimals$}");
    if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s }
}

/// "$2 / $10" (in / out per million tokens), "free", or "local" (Marv `formatPrice`).
pub fn format_price(model: &ModelInfo) -> String {
    if model.local {
        return "local".into();
    }
    let (Some(i), Some(o)) = (model.price_in, model.price_out) else { return String::new() };
    if i == 0.0 && o == 0.0 {
        return "free".into();
    }
    let usd = |n: f64| format!("${}", precision(n, if n < 1.0 { 2 } else { 3 }));
    format!("{} / {}", usd(i), usd(o))
}

#[derive(Debug, Clone, Default)]
pub struct ModelPicker {
    /// None while loading.
    pub models: Option<Result<Vec<ModelInfo>, String>>,
    pub initial: Option<String>,
    pub query: String,
    pub index: usize,
}

impl ModelPicker {
    pub fn new(initial: Option<String>) -> Self {
        Self { initial, ..Self::default() }
    }

    /// The list arrived: start on the initial model.
    pub fn loaded(&mut self, models: Result<Vec<ModelInfo>, String>) {
        if let Ok(list) = &models {
            self.index = self.initial.as_ref().and_then(|id| list.iter().position(|m| &m.id == id)).unwrap_or(0);
        }
        self.models = Some(models);
    }

    fn list(&self) -> &[ModelInfo] {
        match &self.models {
            Some(Ok(list)) => list,
            _ => &[],
        }
    }

    fn matches(&self) -> Vec<&ModelInfo> {
        filter_models(self.list(), &self.query)
    }

    /// Returns the chosen model id on Enter.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<String> {
        let n = self.matches().len();
        match key.code {
            KeyCode::Up if n > 0 => self.index = (self.index + n - 1) % n,
            KeyCode::Down if n > 0 => self.index = (self.index + 1) % n,
            KeyCode::Enter => {
                let custom = self.query.trim();
                return match self.matches().get(self.index) {
                    Some(chosen) => Some(chosen.id.clone()),
                    None if !custom.is_empty() => Some(custom.to_string()),
                    None => None,
                };
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.index = 0;
            }
            KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                self.query.push(c);
                self.index = 0;
            }
            _ => {}
        }
        None
    }

    pub fn lines(&self) -> Vec<Line<'static>> {
        let dim = Style::new().fg(theme::DIM);
        let accent = Style::new().fg(theme::ACCENT);
        let mut lines = Vec::new();
        let input = if self.query.is_empty() {
            vec![
                Span::styled("t", dim.add_modifier(Modifier::REVERSED)),
                Span::styled("ype to filter, e.g. sonnet, deepseek, qwen", dim),
            ]
        } else {
            vec![Span::raw(printable(&self.query)), Span::styled(" ", Style::new().add_modifier(Modifier::REVERSED))]
        };
        lines.push(Line::from([vec![Span::styled("> ", accent)], input].concat()));
        lines.push(Line::default());

        match &self.models {
            None => lines.push(Line::styled("Loading models…", dim)),
            Some(Err(error)) => lines.push(Line::from(vec![
                Span::styled(format!("Couldn't load models: {}", printable(error)), Style::new().fg(theme::ERROR)),
                Span::styled(" · type a model ID and press Enter", dim),
            ])),
            Some(Ok(_)) => {}
        }
        let matches = self.matches();
        // Scroll the visible window so the highlighted row stays in view.
        let start = self.index.saturating_sub(VISIBLE / 2).min(matches.len().saturating_sub(VISIBLE));
        let shown = &matches[start..(start + VISIBLE).min(matches.len())];
        let id_width = shown.iter().map(|m| printable(&m.id).width()).max().unwrap_or(0).min(44);
        for (i, model) in shown.iter().enumerate() {
            let active = start + i == self.index;
            let style = if active { accent } else { Style::new() };
            let id = printable(&model.id);
            let pad = id_width.saturating_sub(id.width());
            let mut spans = vec![
                Span::styled(if active { "❯ " } else { "  " }, style),
                Span::styled(format!("{id}{}", " ".repeat(pad)), style),
                Span::styled(format!("  {}", format_price(model)), dim),
            ];
            if let Some(name) = model.name.as_deref().filter(|n| *n != model.id) {
                spans.push(Span::styled(format!("  {}", printable(name)), dim));
            }
            if model.tools == Some(false) {
                spans.push(Span::styled("  no tool support", Style::new().fg(theme::WARNING)));
            }
            lines.push(Line::from(spans));
        }
        let custom = self.query.trim();
        if matches!(self.models, Some(Ok(_))) && matches.is_empty() && !custom.is_empty() {
            lines.push(Line::styled(format!("No matches. Press Enter to use \"{}\" as the model ID.", printable(custom)), dim));
        }
        let all = self.list();
        if !all.is_empty() {
            let prices = if all.iter().any(|m| m.price_in.is_some()) { " · prices per million tokens, in / out" } else { "" };
            lines.push(Line::styled(format!("{} of {} models{prices}", matches.len(), all.len()), dim));
        }
        lines
    }

    pub fn height(&self) -> u16 {
        self.lines().len() as u16
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        for (i, line) in self.lines().into_iter().enumerate().take(area.height as usize) {
            buf.set_line(area.x, area.y + i as u16, &truncate_end(line, area.width), area.width);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, price_in: Option<f64>, price_out: Option<f64>) -> ModelInfo {
        ModelInfo { id: id.into(), tools: Some(true), price_in, price_out, ..Default::default() }
    }

    fn openrouter() -> Vec<ModelInfo> {
        vec![
            model("anthropic/claude-sonnet-5.5", Some(2.0), Some(10.0)),
            model("deepseek/deepseek-v4.1-flash", Some(0.3), Some(1.2)),
            model("z-ai/glm-5.3", Some(0.22), Some(3.39)),
        ]
    }

    fn text(picker: &ModelPicker) -> String {
        picker.lines().iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n")
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn typed(picker: &mut ModelPicker, text: &str) {
        for c in text.chars() {
            picker.on_key(key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn filter_models_matches_every_term_in_any_order() {
        let models = openrouter();
        let ids: Vec<_> = filter_models(&models, "SONNET claude").iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["anthropic/claude-sonnet-5.5"]);
    }

    #[test]
    fn format_price_like_marv() {
        assert_eq!(format_price(&model("a", Some(2.0), Some(10.0))), "$2 / $10");
        assert_eq!(format_price(&model("a", Some(0.22), Some(3.39))), "$0.22 / $3.39");
        assert_eq!(format_price(&model("a", Some(0.075), Some(1.234))), "$0.075 / $1.23");
        assert_eq!(format_price(&model("a", Some(0.0), Some(0.0))), "free");
        assert_eq!(format_price(&model("a", None, Some(1.0))), "");
        assert_eq!(format_price(&ModelInfo { local: true, ..Default::default() }), "local");
    }

    #[test]
    fn loading_then_the_list_starting_on_the_initial_model() {
        let mut picker = ModelPicker::new(Some("z-ai/glm-5.3".into()));
        assert!(text(&picker).contains("Loading models…"));
        picker.loaded(Ok(openrouter()));
        let frame = text(&picker);
        assert!(frame.contains("$2 / $10"));
        assert!(frame.contains("❯ z-ai/glm-5.3"));
        assert!(frame.contains("3 of 3 models · prices per million tokens, in / out"));
        assert_eq!(picker.on_key(key(KeyCode::Enter)), Some("z-ai/glm-5.3".into()));
    }

    #[test]
    fn typing_filters_and_enter_takes_the_highlighted_match() {
        let mut picker = ModelPicker::new(None);
        picker.loaded(Ok(openrouter()));
        typed(&mut picker, "deep");
        assert!(text(&picker).contains("1 of 3 models"));
        assert_eq!(picker.on_key(key(KeyCode::Enter)), Some("deepseek/deepseek-v4.1-flash".into()));
    }

    #[test]
    fn enter_with_no_matches_uses_the_typed_text_as_a_model_id() {
        let mut picker = ModelPicker::new(None);
        picker.loaded(Ok(openrouter()));
        typed(&mut picker, "acme/brand-new-model");
        assert!(text(&picker).contains("No matches. Press Enter to use \"acme/brand-new-model\" as the model ID."));
        assert_eq!(picker.on_key(key(KeyCode::Enter)), Some("acme/brand-new-model".into()));
    }

    #[test]
    fn a_failed_model_list_still_lets_you_type_an_id() {
        let mut picker = ModelPicker::new(None);
        picker.loaded(Err("connection refused".into()));
        assert!(text(&picker).contains("Couldn't load models: connection refused · type a model ID and press Enter"));
        assert_eq!(picker.on_key(key(KeyCode::Enter)), None);
        typed(&mut picker, "llama3:8b");
        assert_eq!(picker.on_key(key(KeyCode::Enter)), Some("llama3:8b".into()));
    }

    #[test]
    fn flags_models_without_tools_and_scrolls_to_keep_the_highlight_in_view() {
        let mut models: Vec<_> = (0..20).map(|i| model(&format!("m{i:02}"), None, None)).collect();
        models[0].tools = Some(false);
        let mut picker = ModelPicker::new(Some("m15".into()));
        picker.loaded(Ok(models));
        let frame = text(&picker);
        assert!(frame.contains("❯ m15"));
        assert!(!frame.contains("m00"));
        assert_eq!(frame.matches("\n  m").count() + 1, VISIBLE);
        picker.index = 0;
        assert!(text(&picker).contains("m00    no tool support"));
    }
}
