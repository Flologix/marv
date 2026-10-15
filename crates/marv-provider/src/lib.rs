//! The seam between Marv and any LLM vendor. Ports `src/provider/types.ts` (the trait) and
//! `src/provider/index.ts` (`createProvider`, `reasoningFor`).
//!
//! Every adapter turns its own streaming format into [`marv_core::ProviderEvent`]s; the agent
//! loop and the UI only ever see those, so adding a vendor never touches them.
//!
//! - [`create_provider`] is the **only** place that maps a config to a concrete [`Provider`]
//!   (Marv `CLAUDE.md`, "createProvider"). OpenRouter goes through [`openai_compat`], Ollama
//!   through its native API ([`ollama`]), because only the native API can set `num_ctx`.
//! - Errors never surface as `Err`: an adapter yields [`ProviderEvent::Error`] with a message the
//!   user can act on ([`errors`]), and SDK sessions name the option to change ([`errors::SDK_HINTS`]).
//! - `/think` on OpenRouter is its unified `reasoning: { enabled }` switch, decided by
//!   [`reasoning_for`] from what the model list says ([`models::ReasoningSupport`]).
//! - Marv's `Config`/`PRESETS`/`resolveConfig` live in `marv-session`; this crate only takes the
//!   resolved, provider-relevant part of it ([`ProviderConfig`]), so the dependency arrow keeps
//!   pointing from session to provider.

use std::pin::Pin;
use std::sync::Arc;

use futures::Stream;
use marv_core::{ChatTurn, ProviderEvent, ToolChoice, ToolSpec};
use tokio_util::sync::CancellationToken;

pub mod degenerate;
pub mod errors;
pub mod factory;
pub mod models;
pub mod ollama;
pub mod openai_compat;
pub mod retry;
pub mod sse;

pub use errors::ErrorHints;
pub use factory::{ProviderFactory, ProviderOption};
pub use models::{ModelInfo, ReasoningSupport};

/// A stream of provider events for one request.
pub type EventStream = Pin<Box<dyn Stream<Item = ProviderEvent> + Send>>;

/// Per-request options.
#[derive(Debug, Clone, Default)]
pub struct StreamOptions {
    /// Instructions sent ahead of the conversation. Built once per session and byte-stable.
    pub system: Option<String>,
    /// Tools the model may call. Same list and order on every request (prompt cache).
    pub tools: Vec<ToolSpec>,
    /// `None` for the wrap-up reply at the step limit; the tools are still sent so the cached
    /// prefix holds (Marv #59). Ollama has no `tool_choice`: it relies on the note, and the loop
    /// answers any call it makes anyway with "Not run".
    pub tool_choice: ToolChoice,
    /// Cancelled by ctrl+c to stop the reply and the HTTP request behind it.
    pub cancel: CancellationToken,
    /// #90: when on, the adapter collects the raw response payloads and sends them back on the
    /// [`marv_core::ProviderEvent::Usage`] event, for the trajectory's `request` record. Off by
    /// default; the session turns it on from `MARV_DEBUG_RAW`.
    pub debug_raw: bool,
}

pub trait Provider: Send + Sync {
    /// Shown in the status bar, e.g. "openrouter · z-ai/glm-5.3".
    fn name(&self) -> &str;
    /// Context window in tokens, when known (Ollama: the `num_ctx` Marv asks for).
    fn context_length(&self) -> Option<u64>;
    /// Streams one reply. Never fails up front: errors arrive as [`ProviderEvent::Error`].
    ///
    /// The returned stream is `'static`: an adapter serializes `history` into its request body
    /// before returning, so the caller may append to its history while the reply streams.
    fn stream(&self, history: &[ChatTurn], options: StreamOptions) -> EventStream;
}

/// Which built-in provider a config names. A saved provider Marv no longer knows (the removed
/// `echo`) is dropped by `marv-session` on load, which reopens setup; it never reaches here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    OpenRouter,
    Ollama,
}

impl ProviderKind {
    /// The id recorded with sessions and trajectories ("openrouter", "ollama").
    pub fn id(self) -> &'static str {
        match self {
            Self::OpenRouter => "openrouter",
            Self::Ollama => "ollama",
        }
    }

    /// Used in error messages ("OpenRouter rejected the API key…"). Marv: `PRESETS[provider].label`.
    pub fn label(self) -> &'static str {
        match self {
            Self::OpenRouter => "OpenRouter",
            Self::Ollama => "Ollama",
        }
    }
}

/// The provider-relevant part of Marv's resolved `Config` (built by `marv-session`).
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderConfig {
    pub provider: ProviderKind,
    pub model: String,
    /// OpenRouter: `https://openrouter.ai/api/v1`; Ollama: `http://localhost:11434` (a trailing
    /// `/v1` is ignored by the Ollama adapter).
    pub base_url: String,
    /// OpenRouter only.
    pub api_key: Option<String>,
    /// Ollama's configured window (the upper bound for `num_ctx`; default 32768 in config).
    pub context_length: u64,
    /// `/think`: off by default (local thinking models can otherwise reason for minutes).
    pub thinking: bool,
}

/// OpenRouter's unified reasoning switch, as sent in the request body (`{"enabled": bool}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct ReasoningSwitch {
    pub enabled: bool,
}

/// `/think` on OpenRouter. "Off" goes only to models whose reasoning is optional: for one that
/// always reasons, OpenRouter rejects the request (and thinking is off by default, so that would
/// be every request). For a model Marv knows nothing about yet (the model list hasn't loaded),
/// nothing is sent.
pub fn reasoning_for(thinking: bool, reasoning: Option<ReasoningSupport>) -> Option<ReasoningSwitch> {
    let reasoning = reasoning?;
    if thinking {
        return Some(ReasoningSwitch { enabled: true });
    }
    (reasoning == ReasoningSupport::Optional).then_some(ReasoningSwitch { enabled: false })
}

/// The one place that maps a config to a concrete [`Provider`]. `model`: what the model list says
/// about it (its reasoning support, and #96 its output limit). `hints`: what a bad key or an unknown model
/// tells the user to change (the TUI's commands by default; the SDK passes [`errors::SDK_HINTS`]).
///
/// - OpenRouter: [`openai_compat::OpenAiCompatProvider`] named `openrouter · <model>`, with the
///   `X-Title: Marv` attribution header and, when [`reasoning_for`] says so, `body.reasoning`;
///   `max_tokens` from [`openai_compat::max_tokens_for`] (#96) and the idle timeout (#88).
/// - Ollama: [`ollama::OllamaProvider`] with `context_length` and `thinking` from the config.
pub fn create_provider(config: &ProviderConfig, model: Option<&ModelInfo>, hints: Option<ErrorHints>) -> Arc<dyn Provider> {
    let hints = hints.unwrap_or_default();
    match config.provider {
        ProviderKind::OpenRouter => {
            let mut body = serde_json::Map::new();
            if let Some(reasoning) = reasoning_for(config.thinking, model.and_then(|m| m.reasoning)) {
                body.insert("reasoning".into(), serde_json::json!(reasoning));
            }
            Arc::new(openai_compat::OpenAiCompatProvider::new(openai_compat::OpenAiCompatOptions {
                name: format!("openrouter · {}", config.model),
                label: ProviderKind::OpenRouter.label().into(),
                base_url: config.base_url.clone(),
                model: config.model.clone(),
                api_key: config.api_key.clone(),
                // Optional attribution: shows "Marv" in your OpenRouter activity log.
                headers: vec![("X-Title".into(), "Marv".into())],
                body,
                offline_hint: None,
                hints,
                timing: Default::default(),
                idle_timeout: Some(openai_compat::STREAM_IDLE_TIMEOUT),
                // #96: so the provider ends a runaway reply. Ollama has its own cap (`num_predict`).
                max_tokens: Some(openai_compat::max_tokens_for(model.and_then(|m| m.max_output))),
                // #116: so max_tokens is clamped to the room the prompt leaves in the window.
                context_length: model.and_then(|m| m.context),
            }))
        }
        // Ollama's native API, so the context window can be set (its default is 4096 tokens).
        ProviderKind::Ollama => Arc::new(ollama::OllamaProvider::new(ollama::OllamaOptions {
            base_url: config.base_url.clone(),
            model: config.model.clone(),
            context_length: config.context_length,
            thinking: config.thinking,
            hints,
            timing: Default::default(),
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ported from tests/provider-index.test.ts (there checked through the request body).

    #[test]
    fn think_on_turns_reasoning_on_for_models_that_can_reason() {
        let on = Some(ReasoningSwitch { enabled: true });
        assert_eq!(reasoning_for(true, Some(ReasoningSupport::Optional)), on);
        assert_eq!(reasoning_for(true, Some(ReasoningSupport::Mandatory)), on);
    }

    #[test]
    fn think_off_turns_it_off_only_where_it_is_optional() {
        assert_eq!(reasoning_for(false, Some(ReasoningSupport::Optional)), Some(ReasoningSwitch { enabled: false }));
        assert_eq!(reasoning_for(false, Some(ReasoningSupport::Mandatory)), None);
    }

    #[test]
    fn for_an_unknown_model_nothing_is_sent_either_way() {
        assert_eq!(reasoning_for(true, None), None);
        assert_eq!(reasoning_for(false, None), None);
    }

    #[test]
    fn reasoning_switch_serializes_like_openrouter_expects() {
        assert_eq!(serde_json::to_string(&ReasoningSwitch { enabled: false }).unwrap(), r#"{"enabled":false}"#);
    }
}
