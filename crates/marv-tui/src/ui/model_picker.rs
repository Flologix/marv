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

use crossterm::event::KeyEvent;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::view::ModelInfo;

pub const VISIBLE: usize = 8;

/// Every space-separated term must appear in the id (case-insensitive).
pub fn filter_models<'a>(models: &'a [ModelInfo], query: &str) -> Vec<&'a ModelInfo> {
    let _ = (models, query);
    todo!("port: filterModels")
}

/// "$2 / $10" (in / out per million tokens), "free", or "local" (Marv `formatPrice`).
pub fn format_price(model: &ModelInfo) -> String {
    let _ = model;
    todo!("port: formatPrice (toPrecision 2 below $1, else 3)")
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
        let _ = models;
        todo!("port: ModelPicker load effect")
    }

    /// Returns the chosen model id on Enter.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<String> {
        let _ = key;
        todo!("port: ModelPicker input")
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let _ = (area, buf);
        todo!("port: ModelPicker rows")
    }
}
