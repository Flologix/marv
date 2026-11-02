//! Custom providers (#275): an endpoint the user names in `~/.marv/config.json` (a company gateway, LiteLLM, vLLM, a
//! proxy), spoken to through one of the adapters Marv already has.
//!
//! - [`Wire`] says which API shape it speaks: Chat Completions ([`crate::openai_compat`]) or the Responses API
//!   ([`crate::responses`]).
//! - The key, headers and extra body fields go only to the endpoint's own `baseUrl`, on every request; the model list
//!   is read from there too (`GET /models`) unless the config lists the models.
//! - `marv-session` reads the config and resolves the key (`env:`, `command:` or the key itself); this crate gets the
//!   resolved [`Endpoint`] in [`crate::ProviderConfig::endpoint`].

use std::sync::Arc;

use futures::StreamExt;
use futures::future::BoxFuture;
use marv_core::{ChatTurn, ProviderEvent};
use serde::Deserialize;
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::models::ModelInfo;
use crate::{EventStream, Provider, StreamOptions};

/// The API shape an endpoint speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    /// `POST /chat/completions` (OpenAI's Chat Completions; LiteLLM, vLLM, llama.cpp, most gateways).
    OpenAiChat,
    /// `POST /responses` (OpenAI's Responses API).
    OpenAiResponses,
}

impl Wire {
    /// As written in the config: "openai-chat", "openai-responses".
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "openai-chat" => Some(Self::OpenAiChat),
            "openai-responses" => Some(Self::OpenAiResponses),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiChat => "openai-chat",
            Self::OpenAiResponses => "openai-responses",
        }
    }
}

/// A custom provider, resolved: everything a request to it needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Endpoint {
    /// The config's name for it ("acme-gateway"): shown as `acme-gateway · <model>` and recorded with sessions.
    pub name: String,
    pub wire: Wire,
    /// Without a trailing slash, e.g. `https://llm.acme.internal/v1`.
    pub base_url: String,
    /// Sent as `Authorization: Bearer <key>`, to `base_url` only. `None`: no header (a local server).
    pub api_key: Option<String>,
    /// Why it can't be used (its key couldn't be read: `$ACME_LLM_KEY` unset, a failed `command:`; #278: no
    /// `llama-server`): every request ends with it, rather than going out without the key.
    pub problem: Option<String>,
    /// Merged into every request, in the config's order.
    pub headers: Vec<(String, String)>,
    /// Merged into every request body, after Marv's own fields.
    pub extra_body: Map<String, Value>,
    /// The config's model list; empty: read `GET /models`, else any id typed into `/model`.
    pub models: Vec<String>,
    /// The context window, when Marv knows it (#278: the `-c` it started `llama-server` with); else the model list's.
    pub context_length: Option<u64>,
    /// #278: what each request waits for first (a local server loading its model).
    pub ready: Option<Ready>,
}

/// #278: something a request waits for before it's sent: a server Marv started, until it answers. `Err`: why it
/// never will (it exited, with its last lines), as a sentence.
pub trait Readiness: Send + Sync {
    fn wait(&self, cancel: CancellationToken) -> BoxFuture<'static, Result<(), String>>;
}

/// A shared [`Readiness`]; equal only to itself (the same server).
#[derive(Clone)]
pub struct Ready(pub Arc<dyn Readiness>);

impl PartialEq for Ready {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl std::fmt::Debug for Ready {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Ready(..)")
    }
}

/// A provider whose requests wait for [`Ready`] first; a failed wait is the request's error.
pub struct ReadyFirst {
    pub ready: Ready,
    pub inner: Arc<dyn Provider>,
}

impl Provider for ReadyFirst {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn context_length(&self) -> Option<u64> {
        self.inner.context_length()
    }

    fn stream(&self, history: &[ChatTurn], options: StreamOptions) -> EventStream {
        let wait = self.ready.0.wait(options.cancel.clone());
        let inner = self.inner.clone();
        // The history is serialized now, as the trait asks; the request goes once the wait is over.
        let history = history.to_vec();
        Box::pin(
            futures::stream::once(async move {
                match wait.await {
                    Ok(()) => inner.stream(&history, options),
                    Err(why) => Box::pin(futures::stream::iter([ProviderEvent::Error(why)])) as EventStream,
                }
            })
            .flatten(),
        )
    }
}

#[derive(Deserialize)]
struct ModelList {
    data: Vec<ListedModel>,
}

#[derive(Deserialize)]
struct ListedModel {
    id: String,
    /// OpenRouter and LiteLLM say it; vLLM says `max_model_len`.
    #[serde(default, alias = "max_model_len")]
    context_length: Option<u64>,
}

/// The endpoint's models: the config's list, else its `GET /models` (OpenAI's shape), with its key and headers. An
/// endpoint without a list is an `Err`, and `/model` still takes any id typed.
pub async fn list_models(client: &reqwest::Client, endpoint: &Endpoint) -> Result<Vec<ModelInfo>, String> {
    if !endpoint.models.is_empty() {
        return Ok(endpoint.models.iter().map(|id| ModelInfo { id: id.clone(), ..Default::default() }).collect());
    }
    let url = format!("{}/models", endpoint.base_url);
    let mut request = client.get(&url);
    for (name, value) in &endpoint.headers {
        request = request.header(name, value);
    }
    if let Some(key) = &endpoint.api_key {
        request = request.bearer_auth(key);
    }
    let response = request.send().await.map_err(|e| format!("{url}: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("{url} returned {}", response.status().as_u16()));
    }
    let list: ModelList = response.json().await.map_err(|e| format!("{url}: {e}"))?;
    Ok(list.data.into_iter().map(|m| ModelInfo { id: m.id, context: m.context_length, ..Default::default() }).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wires_read_as_written() {
        for wire in [Wire::OpenAiChat, Wire::OpenAiResponses] {
            assert_eq!(Wire::parse(wire.as_str()), Some(wire));
        }
        assert_eq!(Wire::parse("anthropic"), None);
    }
}
