//! How a session gets its providers. Ports `src/provider/factory.ts`.
//!
//! A session needs more than one [`Provider`] object:
//! - a subagent type can name another model;
//! - on OpenRouter the provider is remade once the model list says whether the model's reasoning
//!   can be turned off (`/think`, see [`crate::reasoning_for`]).
//!
//! A factory makes them, and says what to record in saved sessions and trajectories. Thinking is
//! part of the factory: the TUI makes a new factory when `/think` changes.
//!
//! Port notes:
//! - Marv's `configFactory(config, make, load)` takes a resolved `Config`; here it takes the
//!   [`ProviderConfig`] part, and `make`/`load` stay injectable (tests swap both).
//! - Marv's `providerFactory(option, thinking)` (the SDK's way) needs `resolveConfig` and
//!   `PRESETS`, which live in `marv-session`; it moves there and builds a [`ConfigFactory`].
//!   Environment variables (`OPENROUTER_API_KEY`, `MARV_MODEL`…) are ignored on that path: a
//!   library uses what it's given, and errors name the option to change ([`crate::errors::SDK_HINTS`]).
//! - A typo'd provider kind from plain JS ("OpenRouter") must not quietly become Ollama on
//!   localhost: Rust's [`ProviderOption`] enum makes that unrepresentable.

use std::sync::Arc;

use futures::future::BoxFuture;

use crate::models::ModelInfo;
use crate::{Provider, ProviderConfig, ProviderKind};

pub trait ProviderFactory: Send + Sync {
    /// Recorded with sessions and trajectories: "openrouter", "ollama", or "custom".
    fn id(&self) -> &str;
    fn model(&self) -> &str;
    /// Runs on this machine (Ollama): no cost.
    fn local(&self) -> bool {
        false
    }
    /// The session's provider; `model` for a subagent type that names another one; `info` once
    /// the model list was read (its reasoning support and, #96, its output limit).
    fn make(&self, model: Option<&str>, info: Option<&ModelInfo>) -> Arc<dyn Provider>;
    /// What the provider's model list says about the model (context window, prices, reasoning),
    /// when it has a list. OpenRouter's list has all of that; Ollama's knows none of it (`None`).
    fn lookup(&self) -> Option<BoxFuture<'static, Option<ModelInfo>>> {
        None
    }
}

/// What the SDK takes: a built-in provider by name, or any [`Provider`] of your own. A provider of
/// your own is used for everything, subagents included (a subagent type naming another model
/// gets the same provider), and what's recorded as the model is its `name()`.
#[derive(Clone)]
pub enum ProviderOption {
    OpenRouter {
        api_key: String,
        model: Option<String>,
        base_url: Option<String>,
    },
    Ollama {
        model: String,
        host: Option<String>,
        context_length: Option<u64>,
    },
    /// #159: Z.ai's GLM Coding Plan, with the plan's own key (never another provider's).
    Zai {
        api_key: String,
        model: Option<String>,
        base_url: Option<String>,
    },
    Custom(Arc<dyn Provider>),
}

/// Makes a provider from a config and what the model list says about it.
pub type MakeProvider = Arc<dyn Fn(&ProviderConfig, Option<&ModelInfo>) -> Arc<dyn Provider> + Send + Sync>;
/// Reads a provider's model list.
pub type LoadModels = Arc<dyn Fn(ProviderKind, String) -> BoxFuture<'static, Result<Vec<ModelInfo>, String>> + Send + Sync>;

/// The TUI's way: from its resolved config, with the provider maker and model list it was given.
pub struct ConfigFactory {
    pub config: ProviderConfig,
    pub make: MakeProvider,
    pub load: LoadModels,
}

impl ProviderFactory for ConfigFactory {
    fn id(&self) -> &str {
        self.config.provider.id()
    }

    fn model(&self) -> &str {
        &self.config.model
    }

    fn local(&self) -> bool {
        self.config.provider == ProviderKind::Ollama
    }

    fn make(&self, model: Option<&str>, info: Option<&ModelInfo>) -> Arc<dyn Provider> {
        match model {
            Some(model) => (self.make)(&ProviderConfig { model: model.to_string(), ..self.config.clone() }, info),
            None => (self.make)(&self.config, info),
        }
    }

    /// OpenRouter's list has every model's context window, prices and reasoning; Ollama's knows
    /// none of that. A list that fails to load says nothing about the model (`None`). Z.ai's plan
    /// models come from Marv's own table ([`crate::models::zai_models`]), so no request is made.
    fn lookup(&self) -> Option<BoxFuture<'static, Option<ModelInfo>>> {
        if self.config.provider == ProviderKind::Zai {
            let found = crate::models::zai_models().into_iter().find(|m| m.id == self.config.model);
            return Some(Box::pin(async move { found }));
        }
        if self.config.provider != ProviderKind::OpenRouter {
            return None;
        }
        let models = (self.load)(self.config.provider, self.config.base_url.clone());
        let model = self.config.model.clone();
        Some(Box::pin(async move { models.await.ok()?.into_iter().find(|m| m.id == model) }))
    }
}

/// A [`Provider`] of your own, wrapped as a factory: id "custom", model = its name, and `make`
/// always returns the same provider.
pub struct CustomFactory {
    provider: Arc<dyn Provider>,
    model: String,
}

impl CustomFactory {
    pub fn new(provider: Arc<dyn Provider>) -> Self {
        let model = provider.name().to_string();
        Self { provider, model }
    }
}

impl ProviderFactory for CustomFactory {
    fn id(&self) -> &str {
        "custom"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn make(&self, _model: Option<&str>, _info: Option<&ModelInfo>) -> Arc<dyn Provider> {
        self.provider.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EventStream, StreamOptions};
    use marv_core::ChatTurn;

    struct Named;
    impl Provider for Named {
        fn name(&self) -> &str {
            "mine"
        }
        fn context_length(&self) -> Option<u64> {
            None
        }
        fn stream(&self, _history: &[ChatTurn], _options: StreamOptions) -> EventStream {
            Box::pin(futures::stream::empty())
        }
    }

    // From tests/provider-factory.test.ts: a Provider of your own is used for everything.
    #[test]
    fn a_custom_provider_is_used_for_every_model() {
        let factory = CustomFactory::new(Arc::new(Named));
        assert_eq!(factory.id(), "custom");
        assert_eq!(factory.model(), "mine");
        assert_eq!(factory.make(Some("other/model"), None).name(), "mine");
        assert!(factory.lookup().is_none());
    }
}
