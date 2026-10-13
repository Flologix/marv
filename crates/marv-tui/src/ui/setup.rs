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
//!
//! What changes in Rust: the config file and the environment stay in the engine. Setup gets a
//! [`SetupContext`] (presets, the saved provider and model, *masked* keys) and answers with a
//! [`SetupAnswer`] whose `api_key: None` means "keep the saved key" (Marv's `finish({})` kept
//! `initial.apiKey`); the engine keeps the endpoint and drops it on a provider switch (Marv's
//! `tidy`/`sameProvider`).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Padding, Widget};

use super::model_picker::ModelPicker;
use super::select::{Select, SelectItem, wrap_all};
use super::theme;
use crate::view::{ModelInfo, ProviderId, ProviderPreset, SetupAnswer, SetupContext, SetupStart};

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

/// The hint beside each provider (Marv's `PROVIDER_ITEMS`).
fn provider_hint(id: ProviderId) -> &'static str {
    match id {
        ProviderId::OpenRouter => "hundreds of cloud models, one API key",
        ProviderId::Ollama => "models running on this machine, free",
    }
}

impl Setup {
    /// Starts at the provider step, or at the model step for `/model`. When it starts at the model
    /// step the caller asks for the list ([`Setup::needs_models`]).
    pub fn new(context: SetupContext) -> Self {
        let first = context.presets.first().map_or(ProviderId::OpenRouter, |p| p.id);
        let provider = context.current.as_ref().map_or(first, |(p, _)| *p);
        let items = context.presets.iter().map(|p| SelectItem::new(p.id, p.label.clone()).hint(provider_hint(p.id))).collect();
        let step = if context.start == SetupStart::Model { SetupStep::Model } else { SetupStep::Provider };
        let mut setup = Self {
            providers: Select::new(items, Some(&provider)),
            picker: ModelPicker::default(),
            context,
            step,
            provider,
            model: String::new(),
            key_input: String::new(),
            error: None,
        };
        setup.picker = ModelPicker::new(setup.initial_model());
        setup
    }

    /// The provider whose models the model step shows, if it's waiting for them.
    pub fn needs_models(&self) -> Option<ProviderId> {
        (self.step == SetupStep::Model && self.picker.models.is_none()).then_some(self.provider)
    }

    pub fn models_loaded(&mut self, models: Result<Vec<ModelInfo>, String>) {
        self.picker.loaded(models);
    }

    fn preset(&self) -> Option<&ProviderPreset> {
        self.context.presets.iter().find(|p| p.id == self.provider)
    }

    fn label(&self) -> String {
        self.preset().map_or_else(String::new, |p| p.label.clone())
    }

    /// The key found in the environment for this provider, masked.
    fn env_key(&self) -> Option<&str> {
        self.context.env_keys.iter().find(|(p, _)| *p == self.provider).map(|(_, k)| k.as_str())
    }

    fn needs_key_step(&self) -> bool {
        self.preset().is_some_and(|p| p.key_env.is_some()) && self.env_key().is_none()
    }

    fn initial_model(&self) -> Option<String> {
        match &self.context.current {
            Some((p, model)) if *p == self.provider && !model.is_empty() => Some(model.clone()),
            _ => self.preset().map(|p| p.default_model.clone()).filter(|m| !m.is_empty()),
        }
    }

    fn finish(&self, api_key: Option<String>) -> SetupOutcome {
        SetupOutcome::Complete(SetupAnswer { provider: self.provider, model: self.model.clone(), api_key })
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Option<SetupOutcome> {
        if key.code == KeyCode::Esc || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)) {
            return Some(SetupOutcome::Cancel);
        }
        match self.step {
            SetupStep::Provider => {
                let provider = self.providers.on_key(key)?;
                self.provider = provider;
                self.step = SetupStep::Model;
                self.picker = ModelPicker::new(self.initial_model());
                Some(SetupOutcome::LoadModels(provider))
            }
            SetupStep::Model => {
                let model = self.picker.on_key(key)?;
                self.model = model;
                let keep_saved = self.context.start == SetupStart::Model && self.context.saved_key.is_some();
                if self.needs_key_step() && !keep_saved {
                    self.step = SetupStep::ApiKey;
                    None
                } else {
                    Some(self.finish(None))
                }
            }
            SetupStep::ApiKey => match key.code {
                KeyCode::Enter => {
                    let value = self.key_input.trim().to_string();
                    if value.is_empty() && self.context.saved_key.is_some() {
                        return Some(self.finish(None)); // Enter on empty keeps the saved key
                    }
                    if value.is_empty() {
                        self.error = Some("Paste your API key, or press Esc to cancel.".into());
                    } else if value.chars().any(char::is_whitespace) {
                        self.error = Some("That doesn't look like a key (it contains spaces).".into());
                    } else {
                        return Some(self.finish(Some(value)));
                    }
                    None
                }
                KeyCode::Backspace => {
                    self.key_input.pop();
                    self.error = None;
                    None
                }
                KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                    self.key_input.push(c);
                    self.error = None;
                    None
                }
                _ => None,
            },
        }
    }

    fn steps(&self) -> Vec<SetupStep> {
        let mut steps = vec![SetupStep::Provider, SetupStep::Model];
        if self.needs_key_step() {
            steps.push(SetupStep::ApiKey);
        }
        steps
    }

    /// The box's content, unwrapped.
    pub fn lines(&self) -> Vec<Line<'static>> {
        let dim = Style::new().fg(theme::DIM);
        let accent = Style::new().fg(theme::ACCENT);
        let from_model = self.context.start == SetupStart::Model;
        let title = if from_model { format!("Switch {} model", self.label()) } else { "Marv setup".to_string() };
        let mut head = vec![Span::styled(title, accent.add_modifier(Modifier::BOLD))];
        if !from_model {
            let steps = self.steps();
            let at = steps.iter().position(|s| *s == self.step).map_or(1, |i| i + 1);
            // The total depends on the provider, so it's only known after step 1.
            let of = if self.step == SetupStep::Provider { String::new() } else { format!(" of {}", steps.len()) };
            head.push(Span::styled(format!("  step {at}{of}"), dim));
        }
        let mut lines = vec![Line::from(head), Line::default()];
        match self.step {
            SetupStep::Provider => {
                lines.push(Line::raw("Which AI provider should Marv use?"));
                lines.extend(self.providers.render());
            }
            SetupStep::Model => {
                lines.push(Line::raw("Which model?"));
                lines.extend(self.picker.lines());
                if let (Some(key), Some(var)) = (self.env_key(), self.preset().and_then(|p| p.key_env.clone())) {
                    lines.push(Line::styled(format!("Using {var} from your environment ({key})."), dim));
                }
            }
            SetupStep::ApiKey => {
                lines.push(Line::raw(format!("Paste your {} API key:", self.label())));
                let url = self.preset().and_then(|p| p.key_url.clone()).unwrap_or_default();
                lines.push(Line::styled(
                    format!("Get one at {url}. It's saved to ~/.marv/config.json, readable only by you."),
                    dim,
                ));
                if let Some(saved) = &self.context.saved_key {
                    lines.push(Line::styled(format!("Press Enter to keep the current key ({saved})."), dim));
                }
                lines.push(Line::from(vec![
                    Span::styled("> ", accent),
                    Span::raw("*".repeat(self.key_input.chars().count())),
                    Span::styled(" ", Style::new().add_modifier(Modifier::REVERSED)),
                ]));
                if let Some(error) = &self.error {
                    lines.push(Line::styled(error.clone(), Style::new().fg(theme::ERROR)));
                }
            }
        }
        lines.push(Line::default());
        let keys = if self.step == SetupStep::ApiKey { "Enter to save" } else { "↑/↓ to move · Enter to choose" };
        lines.push(Line::styled(format!("{keys} · Esc to cancel"), dim));
        lines
    }

    /// Rows it needs at `width` (the bottom panel sizes itself to this).
    pub fn height(&self, width: u16) -> u16 {
        wrap_all(&self.lines(), width.saturating_sub(4)).len() as u16 + 2
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(theme::ACCENT))
            .padding(Padding::horizontal(1));
        let inner = block.inner(area);
        block.render(area, buf);
        for (i, line) in wrap_all(&self.lines(), inner.width).iter().enumerate().take(inner.height as usize) {
            buf.set_line(inner.x, inner.y + i as u16, line, inner.width);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn presets() -> Vec<ProviderPreset> {
        vec![
            ProviderPreset {
                id: ProviderId::OpenRouter,
                label: "OpenRouter".into(),
                key_env: Some("OPENROUTER_API_KEY".into()),
                key_url: Some("openrouter.ai/settings/keys".into()),
                default_model: "anthropic/claude-sonnet-5.5".into(),
            },
            ProviderPreset {
                id: ProviderId::Ollama,
                label: "Ollama".into(),
                key_env: None,
                key_url: None,
                default_model: String::new(),
            },
        ]
    }

    fn models(provider: ProviderId) -> Vec<ModelInfo> {
        let m = |id: &str, tools: bool, price: Option<(f64, f64)>, local: bool| ModelInfo {
            id: id.into(),
            tools: Some(tools),
            price_in: price.map(|p| p.0),
            price_out: price.map(|p| p.1),
            local,
            ..Default::default()
        };
        match provider {
            ProviderId::OpenRouter => vec![
                m("anthropic/claude-sonnet-5.5", true, Some((2.0, 10.0)), false),
                m("deepseek/deepseek-v4.1-flash", true, Some((0.3, 1.2)), false),
                m("z-ai/glm-5.3", true, Some((0.22, 3.39)), false),
            ],
            ProviderId::Ollama => vec![m("qwen3.5:9b", true, None, true), m("tiny:1b", false, None, true)],
        }
    }

    fn context(start: SetupStart) -> SetupContext {
        SetupContext { start, presets: presets(), current: None, saved_key: None, env_keys: vec![] }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Presses keys, answering LoadModels like the engine would; returns the last outcome.
    fn press(setup: &mut Setup, keys: &[KeyCode]) -> Option<SetupOutcome> {
        let mut last = None;
        for &code in keys {
            last = setup.on_key(key(code));
            if let Some(SetupOutcome::LoadModels(p)) = last {
                setup.models_loaded(Ok(models(p)));
            }
        }
        last
    }

    fn typed(setup: &mut Setup, text: &str) {
        for c in text.chars() {
            setup.on_key(key(KeyCode::Char(c)));
        }
    }

    fn frame(setup: &Setup) -> String {
        setup.lines().iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n")
    }

    fn answer(provider: ProviderId, model: &str, api_key: Option<&str>) -> Option<SetupOutcome> {
        Some(SetupOutcome::Complete(SetupAnswer { provider, model: model.into(), api_key: api_key.map(Into::into) }))
    }

    #[test]
    fn openrouter_provider_then_filtered_model_then_key() {
        let mut setup = Setup::new(context(SetupStart::FirstRun));
        assert!(frame(&setup).contains("Which AI provider"));
        assert!(frame(&setup).contains("step 1"));
        assert!(!frame(&setup).contains("step 1 of"));
        assert_eq!(press(&mut setup, &[KeyCode::Enter]), Some(SetupOutcome::LoadModels(ProviderId::OpenRouter))); // OpenRouter (default)

        assert!(frame(&setup).contains("Which model?"));
        assert!(frame(&setup).contains("step 2 of 3"));
        assert!(frame(&setup).contains("$2 / $10"));
        typed(&mut setup, "deep");
        assert_eq!(press(&mut setup, &[KeyCode::Enter]), None);

        assert!(frame(&setup).contains("Paste your OpenRouter API key"));
        typed(&mut setup, "sk-or-secret");
        assert!(!frame(&setup).contains("sk-or-secret")); // input is masked
        assert!(frame(&setup).contains("> ************"));
        assert_eq!(
            press(&mut setup, &[KeyCode::Enter]),
            answer(ProviderId::OpenRouter, "deepseek/deepseek-v4.1-flash", Some("sk-or-secret"))
        );
    }

    #[test]
    fn skips_the_key_step_when_openrouter_api_key_is_set() {
        let mut setup = Setup::new(SetupContext {
            env_keys: vec![(ProviderId::OpenRouter, "sk-or-from…3456".into())],
            ..context(SetupStart::FirstRun)
        });
        press(&mut setup, &[KeyCode::Enter]);
        assert!(frame(&setup).contains("Using OPENROUTER_API_KEY from your environment (sk-or-from…3456)."));
        assert!(frame(&setup).contains("step 2 of 2"));
        // The default model is highlighted.
        assert_eq!(press(&mut setup, &[KeyCode::Enter]), answer(ProviderId::OpenRouter, "anthropic/claude-sonnet-5.5", None));
    }

    #[test]
    fn ollama_lists_local_models_flags_ones_without_tools_needs_no_key() {
        let mut setup = Setup::new(context(SetupStart::FirstRun));
        assert_eq!(press(&mut setup, &[KeyCode::Down, KeyCode::Enter]), Some(SetupOutcome::LoadModels(ProviderId::Ollama)));
        assert!(frame(&setup).contains("qwen3.5:9b"));
        assert!(frame(&setup).contains("no tool support"));
        assert_eq!(press(&mut setup, &[KeyCode::Enter]), answer(ProviderId::Ollama, "qwen3.5:9b", None));
    }

    #[test]
    fn enter_with_no_matches_uses_the_typed_text_as_a_model_id() {
        let mut setup =
            Setup::new(SetupContext { env_keys: vec![(ProviderId::OpenRouter, "****".into())], ..context(SetupStart::FirstRun) });
        press(&mut setup, &[KeyCode::Enter]);
        typed(&mut setup, "acme/brand-new-model");
        assert_eq!(press(&mut setup, &[KeyCode::Enter]), answer(ProviderId::OpenRouter, "acme/brand-new-model", None));
    }

    #[test]
    fn a_failed_model_list_still_lets_you_type_an_id() {
        let mut setup = Setup::new(context(SetupStart::FirstRun));
        assert_eq!(setup.on_key(key(KeyCode::Down)), None);
        assert_eq!(setup.on_key(key(KeyCode::Enter)), Some(SetupOutcome::LoadModels(ProviderId::Ollama)));
        setup.models_loaded(Err("connection refused".into()));
        assert!(frame(&setup).contains("Couldn't load models: connection refused"));
        typed(&mut setup, "llama3:8b");
        assert_eq!(press(&mut setup, &[KeyCode::Enter]), answer(ProviderId::Ollama, "llama3:8b", None));
    }

    #[test]
    fn only_real_providers_are_offered() {
        let f = frame(&Setup::new(context(SetupStart::FirstRun)));
        assert!(f.contains("OpenRouter · hundreds of cloud models, one API key"));
        assert!(f.contains("Ollama · models running on this machine, free"));
        assert!(!f.contains("Echo"));
    }

    #[test]
    fn enter_on_an_empty_key_keeps_the_saved_one() {
        let mut setup = Setup::new(SetupContext {
            current: Some((ProviderId::OpenRouter, "z-ai/glm-5.3".into())),
            saved_key: Some("sk-or-save…0000".into()),
            ..context(SetupStart::Reconfigure)
        });
        press(&mut setup, &[KeyCode::Enter, KeyCode::Enter]); // provider, model (current one highlighted)
        assert!(frame(&setup).contains("Press Enter to keep the current key (sk-or-save…0000)."));
        assert_eq!(press(&mut setup, &[KeyCode::Enter]), answer(ProviderId::OpenRouter, "z-ai/glm-5.3", None));
    }

    #[test]
    fn a_key_with_spaces_or_no_key_is_refused() {
        let mut setup = Setup::new(context(SetupStart::FirstRun));
        press(&mut setup, &[KeyCode::Enter, KeyCode::Enter]);
        assert_eq!(press(&mut setup, &[KeyCode::Enter]), None);
        assert!(frame(&setup).contains("Paste your API key, or press Esc to cancel."));
        typed(&mut setup, "sk or");
        assert!(!frame(&setup).contains("Paste your API key, or")); // typing clears the error
        assert_eq!(press(&mut setup, &[KeyCode::Enter]), None);
        assert!(frame(&setup).contains("That doesn't look like a key (it contains spaces)."));
    }

    #[test]
    fn model_mode_just_the_picker_keeps_the_key_and_endpoint() {
        let mut setup =
            Setup::new(SetupContext { current: Some((ProviderId::Ollama, "tiny:1b".into())), ..context(SetupStart::Model) });
        assert_eq!(setup.needs_models(), Some(ProviderId::Ollama));
        setup.models_loaded(Ok(models(ProviderId::Ollama)));
        assert_eq!(setup.needs_models(), None);
        assert!(frame(&setup).contains("Switch Ollama model"));
        assert!(!frame(&setup).contains("step"));
        assert!(frame(&setup).contains("❯ tiny:1b"));
        typed(&mut setup, "qwen");
        assert_eq!(press(&mut setup, &[KeyCode::Enter]), answer(ProviderId::Ollama, "qwen3.5:9b", None));
    }

    #[test]
    fn esc_or_ctrl_c_cancels() {
        let mut setup = Setup::new(context(SetupStart::FirstRun));
        assert_eq!(setup.on_key(key(KeyCode::Esc)), Some(SetupOutcome::Cancel));
        assert_eq!(setup.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)), Some(SetupOutcome::Cancel));
    }

    #[test]
    fn renders_in_a_box_sized_to_its_content() {
        let setup = Setup::new(context(SetupStart::FirstRun));
        let area = Rect::new(0, 0, 60, setup.height(60));
        let mut buf = Buffer::empty(area);
        setup.render(area, &mut buf);
        let last = (0..60).map(|x| buf[(x, area.height - 1)].symbol()).collect::<String>();
        assert!(last.starts_with('╰'));
        let footer = (0..60).map(|x| buf[(x, area.height - 2)].symbol()).collect::<String>();
        assert!(footer.contains("↑/↓ to move · Enter to choose · Esc to cancel"));
    }
}
