//! The setup wizard: provider → model → API key. Ports `src/ui/Setup.tsx` (tests: `tests/setup.test.tsx`).
//!
//! Runs on first run, or when OpenRouter has no key, and again via `/setup` (and `/model`, starting
//! at the model step). It renders *in place of the prompt* (pinned at the bottom, below the
//! transcript), not as a separate screen.
//! - Step count "step 2 of 3": the total depends on the provider, so it's only known after step 1.
//! - A key in the environment (`OPENROUTER_API_KEY`) skips the key step and is shown masked.
//! - The key step masks input with `*`; Enter on empty keeps the saved key; a key with whitespace is
//!   refused ("That doesn't look like a key (it contains spaces).").
//! - A custom endpoint belongs to the provider it was set for (the engine drops it on a switch).
//! - Esc or ctrl+c cancels.

use crossterm::event::KeyEvent;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::model_picker::ModelPicker;
use super::select::Select;
use crate::view::{ModelInfo, ProviderId, SetupAnswer, SetupContext};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupStep {
    Provider,
    Model,
    ApiKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupOutcome {
    /// Needs the provider's models: call `SessionPort::load_models`.
    LoadModels(ProviderId),
    Complete(SetupAnswer),
    Cancel,
}

pub struct Setup {
    pub context: SetupContext,
    pub step: SetupStep,
    pub provider: ProviderId,
    pub providers: Select<ProviderId>,
    pub picker: ModelPicker,
    pub model: String,
    pub key_input: String,
    pub error: Option<String>,
}

impl Setup {
    pub fn new(context: SetupContext) -> Self {
        let _ = context;
        todo!("port: Setup initial state (start step, provider)")
    }

    pub fn models_loaded(&mut self, models: Result<Vec<ModelInfo>, String>) {
        self.picker.loaded(models);
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Option<SetupOutcome> {
        let _ = key;
        todo!("port: Setup steps (chooseProvider, chooseModel, submitKey)")
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let _ = (area, buf);
        todo!("port: Setup box")
    }
}
