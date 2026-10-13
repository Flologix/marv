//! `list_models` against a local server. Ported from Marv's `tests/models.test.ts` (there with a
//! fake `fetch`; `format_price` and the filters are unit-tested in `src/models.rs`).

mod support;

use marv_provider::ProviderKind;
use marv_provider::models::{ModelInfo, ReasoningSupport, list_models, list_models_with};
use serde_json::json;
use support::{FakeServer, Reply};

#[tokio::test]
async fn openrouter_tool_capable_chat_models_with_prices_per_million_tokens() {
    let server = FakeServer::start(|req| {
        assert_eq!((req.method.as_str(), req.path.as_str()), ("GET", "/api/v1/models"));
        Reply::json(
            200,
            json!({"data": [
                {"id": "anthropic/claude-sonnet-5.5", "context_length": 1000000, "pricing": {"prompt": "0.000002", "completion": "0.00001", "input_cache_read": "0.0000002"}, "supported_parameters": ["tools", "temperature"]},
                {"id": "some/image-model", "pricing": {"prompt": "0", "completion": "0"}, "supported_parameters": ["temperature"]},
                {"id": "anthropic/claude-sonnet-5.5:batch", "supported_parameters": ["tools"]},
            ]}),
        )
    })
    .await;
    let models = list_models(ProviderKind::OpenRouter, &format!("{}/api/v1", server.url()), None).await.unwrap();
    assert_eq!(
        models,
        vec![ModelInfo {
            id: "anthropic/claude-sonnet-5.5".into(),
            tools: Some(true),
            price_in: Some(2.0),
            price_out: Some(10.0),
            price_cache_read: Some(0.2),
            context: Some(1000000),
            ..Default::default()
        }]
    );
    // The list is public: no key is sent unless one is given.
    assert_eq!(server.last().header("authorization"), None);
}

#[tokio::test]
async fn openrouter_whether_a_models_reasoning_can_be_turned_off() {
    let server = FakeServer::start(|_| {
        Reply::json(
            200,
            json!({"data": [
                {"id": "a/optional", "supported_parameters": ["tools", "reasoning"], "reasoning": {"mandatory": false, "default_enabled": true}},
                {"id": "b/mandatory", "supported_parameters": ["tools", "reasoning"], "reasoning": {"mandatory": true, "default_enabled": true}},
                {"id": "c/none", "supported_parameters": ["tools"]},
            ]}),
        )
    })
    .await;
    let client = reqwest::Client::new();
    let models = list_models_with(&client, ProviderKind::OpenRouter, &server.url(), Some("sk-test")).await.unwrap();
    assert_eq!(
        models.iter().map(|m| (m.id.as_str(), m.reasoning)).collect::<Vec<_>>(),
        [("a/optional", Some(ReasoningSupport::Optional)), ("b/mandatory", Some(ReasoningSupport::Mandatory)), ("c/none", None)]
    );
    assert_eq!(server.last().header("authorization"), Some("Bearer sk-test"));
}

#[tokio::test]
async fn ollama_local_models_with_tool_support_read_from_api_show() {
    let server = FakeServer::start(|req| match (req.path.as_str(), req.json()["model"].as_str()) {
        ("/api/tags", _) => {
            Reply::json(200, json!({"models": [{"name": "qwen3.5:9b"}, {"name": "tiny:1b"}, {"name": "gone:1b"}]}))
        }
        ("/api/show", Some("qwen3.5:9b")) => Reply::json(200, json!({"capabilities": ["completion", "tools"]})),
        ("/api/show", Some("tiny:1b")) => Reply::json(200, json!({"capabilities": ["completion"]})),
        _ => Reply::text(404, "not found"),
    })
    .await;
    // The origin is used: a configured /v1 suffix doesn't matter.
    let models = list_models(ProviderKind::Ollama, &format!("{}/v1", server.url()), None).await.unwrap();
    let local = |id: &str, tools| ModelInfo { id: id.into(), tools, local: true, ..Default::default() };
    // A failed /api/show leaves tool support unknown rather than failing the list.
    assert_eq!(models, vec![local("qwen3.5:9b", Some(true)), local("tiny:1b", Some(false)), local("gone:1b", None)]);
}

#[tokio::test]
async fn a_failed_request_becomes_a_readable_error() {
    let server = FakeServer::start(|_| Reply::text(404, "not found")).await;
    let err = list_models(ProviderKind::Ollama, &server.url(), None).await.unwrap_err();
    assert_eq!(err, format!("{}/api/tags returned 404", server.url()));
}
