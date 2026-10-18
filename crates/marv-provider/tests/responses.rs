//! The Responses API adapter against a local server (#159): the request it sends, a refused token refreshed once,
//! retries after a dropped stream, and HTTP errors.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use futures::StreamExt;
use marv_core::{ChatTurn, ProviderEvent};
use marv_provider::errors::ErrorHints;
use marv_provider::responses::{ApiKey, Credentials, ResponsesOptions, ResponsesProvider};
use marv_provider::retry::RetryTiming;
use marv_provider::{Provider, StreamOptions};
use serde_json::{Value, json};
use support::{End, FakeServer, Reply};

fn options(base_url: &str, credentials: Arc<dyn Credentials>) -> ResponsesOptions {
    ResponsesOptions {
        name: "test".into(),
        label: "TestAI".into(),
        base_url: base_url.into(),
        model: "gpt-x".into(),
        credentials,
        headers: Vec::new(),
        reasoning_effort: None,
        encrypted_reasoning: true,
        max_output_tokens: None,
        tool_namespace: None,
        hints: ErrorHints::default(),
        timing: RetryTiming { scale: 0.001 },
        idle_timeout: None,
    }
}

fn hi() -> Vec<ChatTurn> {
    vec![ChatTurn::User { text: "hi".into() }]
}

fn reply(text: &str) -> Reply {
    Reply::sse(&[
        json!({"type": "response.output_text.delta", "delta": text}),
        json!({"type": "response.completed", "response": {"usage": {"input_tokens": 3, "output_tokens": 1}}}),
    ])
}

async fn events(provider: &ResponsesProvider) -> Vec<ProviderEvent> {
    provider.stream(&hi(), StreamOptions { system: Some("SYS".into()), ..Default::default() }).collect().await
}

fn text(events: &[ProviderEvent]) -> String {
    events.iter().filter_map(|e| if let ProviderEvent::TextDelta(t) = e { Some(t.as_str()) } else { None }).collect()
}

#[tokio::test]
async fn posts_the_conversation_to_responses_with_the_bearer_token() {
    let server = FakeServer::start(|_| reply("Hello")).await;
    let provider = ResponsesProvider::new(options(&format!("{}/v1", server.url()), Arc::new(ApiKey("sk-test".into()))));
    let events = events(&provider).await;
    assert_eq!(text(&events), "Hello");
    assert_eq!(events.last(), Some(&ProviderEvent::Done { reason: Some("stop".into()) }));
    let request = server.last();
    assert_eq!(request.path, "/v1/responses");
    assert_eq!(request.header("authorization"), Some("Bearer sk-test"));
    let body: Value = request.json();
    assert_eq!(body["instructions"], json!("SYS"));
    assert_eq!((body["store"].clone(), body["stream"].clone()), (json!(false), json!(true)));
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(body.get("max_output_tokens"), None);
    assert_eq!(body.get("temperature"), None);
}

/// A sign-in whose first token the server refuses: it's asked once for a fresh one.
struct Rotating {
    refused: AtomicUsize,
}

#[async_trait]
impl Credentials for Rotating {
    async fn bearer(&self) -> Result<String, String> {
        Ok("old".into())
    }
    async fn refused(&self, token: &str) -> Result<String, String> {
        assert_eq!(token, "old");
        self.refused.fetch_add(1, Ordering::SeqCst);
        Ok("fresh".into())
    }
}

#[tokio::test]
async fn a_refused_token_is_refreshed_once() {
    let server = FakeServer::start(|r| match r.header("authorization") {
        Some("Bearer fresh") => reply("ok"),
        _ => Reply::json(401, json!({"detail": "expired"})),
    })
    .await;
    let credentials = Arc::new(Rotating { refused: AtomicUsize::new(0) });
    let provider = ResponsesProvider::new(options(&server.url(), credentials.clone()));
    assert_eq!(text(&events(&provider).await), "ok");
    assert_eq!(credentials.refused.load(Ordering::SeqCst), 1);

    // Refused again with the fresh one: the error, not a loop.
    let server = FakeServer::start(|_| Reply::json(401, json!({"detail": "nope"}))).await;
    let credentials = Arc::new(Rotating { refused: AtomicUsize::new(0) });
    let provider = ResponsesProvider::new(options(&server.url(), credentials.clone()));
    let events = events(&provider).await;
    assert!(matches!(events.last(), Some(ProviderEvent::Error(m)) if m.starts_with("TestAI rejected the API key")), "{events:?}");
    assert_eq!((server.count("/responses"), credentials.refused.load(Ordering::SeqCst)), (2, 1));
}

/// Credentials that can't give a token end the stream with their sentence, before any request.
struct SignedOut;

#[async_trait]
impl Credentials for SignedOut {
    async fn bearer(&self) -> Result<String, String> {
        Err("not signed in: run /setup to sign in with ChatGPT.".into())
    }
}

#[tokio::test]
async fn no_token_no_request() {
    let server = FakeServer::start(|_| reply("never")).await;
    let provider = ResponsesProvider::new(options(&server.url(), Arc::new(SignedOut)));
    assert_eq!(
        events(&provider).await,
        vec![ProviderEvent::Error("TestAI: not signed in: run /setup to sign in with ChatGPT.".into())]
    );
    assert_eq!(server.count("/responses"), 0);
}

#[tokio::test]
async fn a_dropped_stream_is_retried() {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let server = FakeServer::start(move |_| {
        if seen.fetch_add(1, Ordering::SeqCst) == 0 {
            Reply::sse(&[json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "function_call", "call_id": "c", "name": "glob"}})])
                .ending(End::Drop)
        } else {
            reply("again")
        }
    })
    .await;
    let provider = ResponsesProvider::new(options(&server.url(), Arc::new(ApiKey("k".into()))));
    let events = events(&provider).await;
    assert!(events.iter().any(|e| matches!(e, ProviderEvent::Retry { continued: false, .. })), "{events:?}");
    assert_eq!(text(&events), "again");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn an_unknown_model_is_a_readable_error() {
    let server =
        FakeServer::start(|_| Reply::json(404, json!({"error": {"code": "model_not_found", "message": "no such model"}}))).await;
    let provider = ResponsesProvider::new(options(&server.url(), Arc::new(ApiKey("k".into()))));
    let events = events(&provider).await;
    assert_eq!(
        events,
        vec![ProviderEvent::Error("Model \"gpt-x\" wasn't found on TestAI. Pick another with /model.\nno such model".into())]
    );
}

/// #159: the ChatGPT plan's limit is a hard stop: one request, the plan's message, no retry.
#[tokio::test]
async fn the_plans_limit_is_not_retried() {
    let server = FakeServer::start(|_| {
        Reply::json(
            429,
            json!({"error": {"code": "subscription_sharing_usage_limit_exceeded", "message": "Usage limit reached"}}),
        )
    })
    .await;
    let provider = ResponsesProvider::new(options(&server.url(), Arc::new(ApiKey("k".into()))));
    let events = events(&provider).await;
    assert_eq!(server.count("/responses"), 1);
    assert!(
        matches!(events.last(), Some(ProviderEvent::Error(m)) if m.starts_with("TestAI: your ChatGPT plan's usage limit is reached")),
        "{events:?}"
    );
}
