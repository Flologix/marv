//! `create_provider` and `ConfigFactory` against a local server. Ported from Marv's
//! `tests/provider-index.test.ts` and `tests/provider-factory.test.ts` (the SDK's
//! `providerFactory` moves to marv-session; its hint test is here through `create_provider`).

mod support;

use std::sync::{Arc, Mutex};

use futures::StreamExt;
use marv_core::{ChatTurn, ProviderEvent};
use marv_provider::errors::SDK_HINTS;
use marv_provider::factory::{ConfigFactory, ProviderFactory};
use marv_provider::models::ModelInfo;
use marv_provider::{ErrorHints, ProviderConfig, ProviderKind, ReasoningSupport, StreamOptions, create_provider};
use serde_json::{Value, json};
use support::{FakeServer, Reply};

fn openrouter(base_url: &str, thinking: bool) -> ProviderConfig {
    ProviderConfig {
        provider: ProviderKind::OpenRouter,
        model: "acme/m".into(),
        base_url: base_url.into(),
        api_key: Some("k".into()),
        context_length: 32768,
        thinking,
    }
}

fn hi() -> Vec<ChatTurn> {
    vec![ChatTurn::User { text: "hi".into() }]
}

async fn body_sent(thinking: bool, reasoning: Option<ReasoningSupport>) -> Value {
    let server =
        FakeServer::start(|_| Reply::sse(&[json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}), json!("[DONE]")])).await;
    let provider = create_provider(&openrouter(&format!("{}/v1", server.url()), thinking), reasoning, None);
    provider.stream(&hi(), StreamOptions::default()).collect::<Vec<_>>().await;
    server.last().json()
}

// OpenRouter's reasoning switch: "off" must only be sent where the model's reasoning is optional
// (for a model that always reasons, OpenRouter rejects the request).

#[tokio::test]
async fn think_on_turns_reasoning_on_for_models_that_can_reason() {
    assert_eq!(body_sent(true, Some(ReasoningSupport::Optional)).await["reasoning"], json!({"enabled": true}));
    assert_eq!(body_sent(true, Some(ReasoningSupport::Mandatory)).await["reasoning"], json!({"enabled": true}));
}

#[tokio::test]
async fn think_off_turns_it_off_only_where_its_optional() {
    assert_eq!(body_sent(false, Some(ReasoningSupport::Optional)).await["reasoning"], json!({"enabled": false}));
    assert_eq!(body_sent(false, Some(ReasoningSupport::Mandatory)).await.get("reasoning"), None);
}

#[tokio::test]
async fn for_a_model_marv_knows_nothing_about_yet_nothing_is_sent_either_way() {
    assert_eq!(body_sent(true, None).await.get("reasoning"), None);
    assert_eq!(body_sent(false, None).await.get("reasoning"), None);
}

#[tokio::test]
async fn openrouter_is_named_after_its_model_and_sends_the_attribution_header_and_key() {
    let server = FakeServer::start(|_| Reply::sse(&[json!("[DONE]")])).await;
    let provider = create_provider(&openrouter(&format!("{}/v1", server.url()), false), None, None);
    assert_eq!(provider.name(), "openrouter · acme/m");
    provider.stream(&hi(), StreamOptions::default()).collect::<Vec<_>>().await;
    let request = server.last();
    assert_eq!(request.header("x-title"), Some("Marv"));
    assert_eq!(request.header("authorization"), Some("Bearer k"));
}

#[test]
fn ollama_has_its_context_window() {
    let config = ProviderConfig {
        provider: ProviderKind::Ollama,
        model: "qwen3.5:9b".into(),
        base_url: "http://gpu-box:11434".into(),
        api_key: None,
        context_length: 65536,
        thinking: false,
    };
    let provider = create_provider(&config, None, None);
    assert_eq!(provider.name(), "ollama · qwen3.5:9b");
    assert_eq!(provider.context_length(), Some(65536));
}

#[tokio::test]
async fn config_factory_uses_the_maker_and_model_list_it_was_given() {
    let config = ProviderConfig { model: "a/b".into(), ..openrouter("http://unused", true) };
    let made = Arc::new(Mutex::new(Vec::new()));
    let seen = made.clone();
    let factory = ConfigFactory {
        config: config.clone(),
        make: Arc::new(move |c: &ProviderConfig, info| {
            seen.lock().unwrap().push((c.model.clone(), c.thinking, info));
            create_provider(c, info, None)
        }),
        load: Arc::new(|_, _| {
            Box::pin(async {
                Ok(vec![
                    ModelInfo { id: "a/b".into(), context: Some(1000), ..Default::default() },
                    ModelInfo { id: "c/d".into(), ..Default::default() },
                ])
            })
        }),
    };
    assert_eq!((factory.id(), factory.model(), factory.local()), ("openrouter", "a/b", false));
    factory.make(None, None);
    factory.make(Some("c/d"), None);
    factory.make(None, Some(ReasoningSupport::Optional));
    assert_eq!(
        *made.lock().unwrap(),
        [("a/b".into(), true, None), ("c/d".into(), true, None), ("a/b".into(), true, Some(ReasoningSupport::Optional))]
    );
    assert_eq!(factory.lookup().unwrap().await, Some(ModelInfo { id: "a/b".into(), context: Some(1000), ..Default::default() }));

    // Ollama is local, and its list knows nothing worth looking up.
    let ollama = ConfigFactory { config: ProviderConfig { provider: ProviderKind::Ollama, ..config }, ..factory };
    assert!(ollama.local());
    assert!(ollama.lookup().is_none());
}

#[tokio::test]
async fn a_model_list_that_fails_to_load_says_nothing_about_the_model() {
    let factory = ConfigFactory {
        config: openrouter("http://unused", false),
        make: Arc::new(|c: &ProviderConfig, info| create_provider(c, info, None)),
        load: Arc::new(|_, _| Box::pin(async { Err("offline".to_string()) })),
    };
    assert_eq!(factory.lookup().unwrap().await, None);
}

// Error hints name what to change: the TUI's commands, the SDK's options.

async fn error_of(status: u16, config: ProviderConfig, hints: Option<ErrorHints>) -> String {
    let server = FakeServer::start(move |_| Reply::json(status, json!({"error": {"message": "nope"}}))).await;
    let base_url = match config.provider {
        ProviderKind::OpenRouter => format!("{}/v1", server.url()),
        ProviderKind::Ollama => server.url(),
    };
    let provider = create_provider(&ProviderConfig { base_url, ..config }, None, hints);
    let events: Vec<_> = provider.stream(&hi(), StreamOptions::default()).collect().await;
    events.into_iter().find_map(|e| if let ProviderEvent::Error(m) = e { Some(m) } else { None }).expect("an error")
}

#[tokio::test]
async fn a_rejected_key() {
    let config = || ProviderConfig { model: "x/y".into(), ..openrouter("", false) };
    assert!(error_of(401, config(), None).await.starts_with("OpenRouter rejected the API key. Run /setup to enter a new one."));
    let message = error_of(401, config(), Some(SDK_HINTS)).await;
    assert!(
        message.starts_with("OpenRouter rejected the API key. Pass another provider.apiKey to createSession() or configure().")
    );
    assert!(!message.contains("/setup"));
}

#[tokio::test]
async fn an_unknown_model_on_openrouter_and_on_ollama() {
    let config = || ProviderConfig { model: "x/y".into(), ..openrouter("", false) };
    assert!(
        error_of(404, config(), None).await.starts_with("Model \"x/y\" wasn't found on OpenRouter. Pick another with /model.")
    );
    assert!(
        error_of(404, config(), Some(SDK_HINTS)).await.starts_with(
            "Model \"x/y\" wasn't found on OpenRouter. Pass another provider.model to createSession() or configure()."
        )
    );
    let ollama =
        ProviderConfig { provider: ProviderKind::Ollama, model: "nope:1b".into(), api_key: None, ..openrouter("", false) };
    assert!(
        error_of(404, ollama, Some(SDK_HINTS)).await.starts_with(
            "Model \"nope:1b\" wasn't found on Ollama. Pass another provider.model to createSession() or configure()."
        )
    );
}
