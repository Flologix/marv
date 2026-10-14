//! The OpenAI-compatible adapter against a local server. Ported from Marv's
//! `tests/openai-compat.test.ts`, plus #64 (continue after a drop, jitter).

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures::StreamExt;
use marv_core::{ChatTurn, ProviderEvent, ToolCall, ToolSpec, Usage};
use marv_provider::openai_compat::{DROPPED_STREAM_NOTE, JITTER, OpenAiCompatOptions, OpenAiCompatProvider, jittered};
use marv_provider::retry::RetryTiming;
use marv_provider::{EventStream, Provider, StreamOptions};
use serde_json::{Value, json};
use support::{End, FakeServer, Reply};
use tokio_util::sync::CancellationToken;

/// Retry waits (1 s, 4 s, 10 s, Retry-After) shrunk to milliseconds, so retry tests don't wait in real time.
const FAST: RetryTiming = RetryTiming { scale: 0.001 };

fn provider(base_url: &str, api_key: Option<&str>) -> OpenAiCompatProvider {
    with_timing(base_url, api_key, FAST)
}

fn with_timing(base_url: &str, api_key: Option<&str>, timing: RetryTiming) -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(OpenAiCompatOptions {
        name: "test".into(),
        label: "TestRouter".into(),
        base_url: base_url.into(),
        model: "acme/model-1".into(),
        api_key: api_key.map(Into::into),
        timing,
        ..Default::default()
    })
}

fn v1(server: &FakeServer) -> String {
    format!("{}/v1", server.url())
}

fn hi() -> Vec<ChatTurn> {
    vec![ChatTurn::User { text: "hi".into() }]
}

async fn collect(stream: EventStream) -> Vec<ProviderEvent> {
    stream.collect().await
}

fn delta(content: &str) -> Value {
    json!({"choices": [{"delta": {"content": content}}]})
}

fn stop() -> Value {
    json!({"choices": [{"delta": {}, "finish_reason": "stop"}]})
}

fn done() -> Value {
    json!("[DONE]")
}

fn text(t: &str) -> ProviderEvent {
    ProviderEvent::TextDelta(t.into())
}

fn error_of(events: &[ProviderEvent]) -> &str {
    events.iter().find_map(|e| if let ProviderEvent::Error(m) = e { Some(m.as_str()) } else { None }).expect("an error event")
}

fn retries(events: &[ProviderEvent]) -> Vec<bool> {
    events.iter().filter_map(|e| if let ProviderEvent::Retry { continued, .. } = e { Some(*continued) } else { None }).collect()
}

/// A handler that answers the n-th request (1-based) with `reply(n)`.
fn counting(
    reply: impl Fn(usize) -> Reply + Send + Sync + 'static,
) -> impl Fn(&support::Request) -> Reply + Send + Sync + 'static {
    let n = AtomicUsize::new(0);
    move |_| reply(n.fetch_add(1, Ordering::SeqCst) + 1)
}

#[tokio::test]
async fn streams_text_deltas_and_ends_with_done() {
    let server = FakeServer::start(|_| Reply::sse(&[delta("Hel"), delta("lo"), stop(), done()])).await;
    assert_eq!(
        collect(provider(&v1(&server), None).stream(&hi(), StreamOptions::default())).await,
        vec![text("Hel"), text("lo"), ProviderEvent::Done { reason: Some("stop".into()) }]
    );
}

#[tokio::test]
async fn sends_the_model_system_prompt_history_and_key() {
    let server = FakeServer::start(|_| Reply::sse(&[done()])).await;
    let history = vec![
        ChatTurn::User { text: "hi".into() },
        ChatTurn::Assistant { text: "hello".into(), tool_calls: vec![], reasoning: None },
        ChatTurn::User { text: "again".into() },
    ];
    let options = StreamOptions { system: Some("You are Marv.".into()), ..Default::default() };
    collect(provider(&v1(&server), Some("sk-test")).stream(&history, options)).await;
    let request = server.last();
    assert_eq!(request.path, "/v1/chat/completions");
    assert_eq!(request.header("authorization"), Some("Bearer sk-test"));
    assert_eq!(request.header("content-type"), Some("application/json"));
    // Byte for byte: field order is the prompt cache's prefix.
    assert_eq!(
        request.text(),
        r#"{"model":"acme/model-1","stream":true,"stream_options":{"include_usage":true},"messages":[{"role":"system","content":"You are Marv."},{"role":"user","content":"hi"},{"role":"assistant","content":"hello"},{"role":"user","content":"again"}]}"#
    );
}

#[tokio::test]
async fn streams_reasoning_as_thinking_deltas_from_either_field_name() {
    let server = FakeServer::start(|_| {
        Reply::sse(&[
            json!({"choices": [{"delta": {"reasoning": "Hmm, "}}]}),
            json!({"choices": [{"delta": {"reasoning_content": "ok."}}]}),
            delta("Hi!"),
            done(),
        ])
    })
    .await;
    assert_eq!(
        collect(provider(&v1(&server), None).stream(&hi(), StreamOptions::default())).await,
        vec![
            ProviderEvent::ThinkingDelta("Hmm, ".into()),
            ProviderEvent::ThinkingDelta("ok.".into()),
            text("Hi!"),
            ProviderEvent::Done { reason: None }
        ]
    );
}

#[tokio::test]
async fn adds_extra_body_fields_and_headers_to_the_request() {
    let server = FakeServer::start(|_| Reply::sse(&[done()])).await;
    let mut options = OpenAiCompatOptions {
        base_url: v1(&server),
        model: "m".into(),
        headers: vec![("X-Title".into(), "Marv".into())],
        ..Default::default()
    };
    options.body.insert("reasoning_effort".into(), json!("none"));
    collect(OpenAiCompatProvider::new(options).stream(&hi(), StreamOptions::default())).await;
    let request = server.last();
    assert_eq!(request.json()["reasoning_effort"], "none");
    assert_eq!(request.header("x-title"), Some("Marv"));
}

#[tokio::test]
async fn sends_tools_and_tool_calls_and_results_in_openai_format() {
    let server = FakeServer::start(|_| Reply::sse(&[done()])).await;
    let tools =
        vec![ToolSpec { name: "read_file".into(), description: "Read a file".into(), parameters: json!({"type": "object"}) }];
    let history = vec![
        ChatTurn::User { text: "read it".into() },
        ChatTurn::Assistant {
            text: "".into(),
            tool_calls: vec![ToolCall { id: "c1".into(), name: "read_file".into(), arguments: r#"{"path":"a.ts"}"#.into() }],
            reasoning: None,
        },
        ChatTurn::Tool { call_id: "c1".into(), name: "read_file".into(), text: "file contents".into() },
    ];
    collect(provider(&v1(&server), None).stream(&history, StreamOptions { tools, ..Default::default() })).await;
    let body = server.last().json();
    assert_eq!(
        body["tools"],
        json!([{"type": "function", "function": {"name": "read_file", "description": "Read a file", "parameters": {"type": "object"}}}])
    );
    assert_eq!(
        body["messages"],
        json!([
            {"role": "user", "content": "read it"},
            {"role": "assistant", "content": "", "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\":\"a.ts\"}"}}]},
            {"role": "tool", "tool_call_id": "c1", "content": "file contents"},
        ])
    );
}

#[tokio::test]
async fn reassembles_tool_calls_streamed_in_fragments() {
    let tc = |calls: Value| json!({"choices": [{"delta": {"tool_calls": calls}}]});
    let server = FakeServer::start(move |_| {
        Reply::sse(&[
            tc(json!([{"index": 0, "id": "c1", "type": "function", "function": {"name": "read_file", "arguments": ""}}])),
            tc(json!([{"index": 0, "function": {"arguments": "{\"pa"}}])),
            tc(json!([{"index": 1, "id": "c2", "type": "function", "function": {"name": "glob", "arguments": "{\"pattern\":\"*\"}"}}])),
            tc(json!([{"index": 0, "function": {"arguments": "th\":\"a.ts\"}"}}])),
            json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 1200, "completion_tokens": 30, "prompt_tokens_details": {"cached_tokens": 1000}, "cost": 0.00042}}),
            done(),
        ])
    })
    .await;
    let events = collect(provider(&v1(&server), None).stream(&hi(), StreamOptions::default())).await;
    let (deltas, rest): (Vec<_>, Vec<_>) = events.into_iter().partition(|e| matches!(e, ProviderEvent::ToolCallDelta { .. }));
    let call = |id: &str, name: &str, arguments: &str| {
        ProviderEvent::ToolCall(ToolCall { id: id.into(), name: name.into(), arguments: arguments.into() })
    };
    assert_eq!(
        rest,
        vec![
            ProviderEvent::Usage(Usage {
                prompt_tokens: 1200,
                completion_tokens: 30,
                cached_tokens: Some(1000),
                cost: Some(0.00042)
            }),
            call("c1", "read_file", r#"{"path":"a.ts"}"#),
            call("c2", "glob", r#"{"pattern":"*"}"#),
            ProviderEvent::Done { reason: Some("tool_calls".into()) },
        ]
    );
    // Each fragment is also reported as it arrives: a big write_file can take minutes to stream,
    // and showing nothing until it's complete looked like a hang.
    let fragment = |index, name: &str, text: &str| ProviderEvent::ToolCallDelta { index, name: name.into(), text: text.into() };
    assert_eq!(
        deltas,
        vec![
            fragment(0, "read_file", ""),
            fragment(0, "read_file", "{\"pa"),
            fragment(1, "glob", "{\"pattern\":\"*\"}"),
            fragment(0, "read_file", "th\":\"a.ts\"}"),
        ]
    );
}

#[tokio::test]
async fn sends_no_authorization_header_without_a_key() {
    let server = FakeServer::start(|_| Reply::sse(&[done()])).await;
    collect(provider(&v1(&server), None).stream(&hi(), StreamOptions::default())).await;
    assert_eq!(server.last().header("authorization"), None);
}

#[tokio::test]
async fn turns_http_statuses_into_helpful_errors() {
    for (status, message) in
        [(401, "TestRouter rejected the API key"), (402, "out of credits"), (404, "Model \"acme/model-1\" wasn't found")]
    {
        let server = FakeServer::start(move |_| Reply::json(status, json!({"error": {"message": "upstream says no"}}))).await;
        let events = collect(provider(&v1(&server), None).stream(&hi(), StreamOptions::default())).await;
        assert_eq!(events.len(), 1, "{status}: {events:?}");
        let error = error_of(&events);
        assert!(error.contains(message) && error.contains("upstream says no"), "{status}: {error}");
    }
}

#[tokio::test]
async fn turns_http_429_into_a_helpful_error_after_retries_are_exhausted() {
    let server = FakeServer::start(|_| Reply::json(429, json!({"error": {"message": "too fast"}}))).await;
    let events = collect(provider(&v1(&server), None).stream(&hi(), StreamOptions::default())).await;
    assert_eq!(server.requests().len(), 3);
    assert_eq!(retries(&events).len(), 2);
    assert!(error_of(&events).contains("Rate limited"));
}

#[tokio::test]
async fn reports_an_error_sent_in_the_middle_of_the_stream() {
    let server = FakeServer::start(|_| Reply::sse(&[delta("partial"), json!({"error": {"message": "provider crashed"}})])).await;
    assert_eq!(
        collect(provider(&v1(&server), None).stream(&hi(), StreamOptions::default())).await,
        vec![text("partial"), ProviderEvent::Error("TestRouter: provider crashed".into())]
    );
}

#[tokio::test]
async fn an_upstream_failure_wrapped_by_openrouter_shows_the_providers_own_message() {
    let wrapped = json!({"error": {"message": "Provider returned error", "code": 400, "metadata": {"provider_name": "Anthropic", "raw": "{\"type\":\"error\",\"error\":{\"message\":\"tool_use ids must be unique\"}}"}}});
    let body = wrapped.clone();
    let http = FakeServer::start(move |_| Reply::json(400, body.clone())).await;
    let events = collect(provider(&v1(&http), None).stream(&hi(), StreamOptions::default())).await;
    let message = error_of(&events);
    assert!(message.contains("Anthropic") && message.contains("tool_use ids must be unique"), "{message}");

    let mid_stream = FakeServer::start(move |_| Reply::sse(&[delta("partial"), wrapped.clone()])).await;
    let events = collect(provider(&v1(&mid_stream), None).stream(&hi(), StreamOptions::default())).await;
    assert!(matches!(events.last(), Some(ProviderEvent::Error(m)) if m.contains("tool_use ids must be unique")));
}

#[tokio::test]
async fn a_gateways_502_retries_first_then_reports_a_clean_error_without_html() {
    let page =
        format!("<!DOCTYPE html><html><head><title>502 Bad Gateway</title></head><body>{}</body></html>", "x".repeat(8000));
    let server = FakeServer::start(move |_| Reply::new(502, "text/html", page.clone())).await;
    let events = collect(provider(&v1(&server), None).stream(&hi(), StreamOptions::default())).await;
    assert_eq!(server.requests().len(), 3);
    let error = error_of(&events);
    assert!(error.starts_with("TestRouter is having trouble (502)."), "{error}");
    assert!(!error.contains('<') && error.len() < 200);
}

#[tokio::test]
async fn a_stream_that_ends_inside_a_tool_call_retries_from_the_top_then_reports_the_error() {
    let fragment = json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1", "function": {"name": "write_file", "arguments": "{\"path\":\"a.ts\",\"con"}}]}}]});
    // No finish_reason, no [DONE].
    let server = FakeServer::start(move |_| Reply::sse(&[delta("Writing the fi"), fragment.clone()])).await;
    let events = collect(provider(&v1(&server), None).stream(&hi(), StreamOptions::default())).await;
    assert_eq!(server.requests().len(), 3);
    // The fragment was shown each time it streamed, and retries happened.
    assert_eq!(events.iter().filter(|e| matches!(e, ProviderEvent::TextDelta(_))).count(), 3);
    assert_eq!(events.iter().filter(|e| matches!(e, ProviderEvent::ToolCallDelta { .. })).count(), 3);
    // #64: a half tool call can't be continued: every retry starts over, with the same request.
    assert_eq!(retries(&events), [false, false]);
    let bodies: Vec<_> = server.requests().iter().map(|r| r.text()).collect();
    assert!(bodies.iter().all(|b| *b == bodies[0]));
    assert_eq!(error_of(&events), "TestRouter: the stream ended early, before the reply was complete. Try again.");
}

#[tokio::test]
async fn explains_when_the_server_cant_be_reached_after_retries() {
    let events = collect(provider("http://localhost:1/v1", None).stream(&hi(), StreamOptions::default())).await;
    // Unable to connect is a network error, retried 3 times, then reported.
    assert_eq!(retries(&events).len(), 2);
    let error = error_of(&events);
    assert!(error.contains("Can't reach TestRouter at http://localhost:1"), "{error}");
}

#[tokio::test]
async fn resends_the_request_after_an_http_503_succeeding_on_retry() {
    let server = FakeServer::start(counting(|n| {
        if n == 1 { Reply::text(503, "Service Unavailable") } else { Reply::sse(&[delta("ok"), stop(), done()]) }
    }))
    .await;
    let events = collect(provider(&v1(&server), None).stream(&hi(), StreamOptions::default())).await;
    assert_eq!(server.requests().len(), 2);
    assert!(events.contains(&text("ok")));
    assert!(!events.iter().any(|e| matches!(e, ProviderEvent::Error(_))));
    assert_eq!(
        events[0],
        ProviderEvent::Retry {
            attempt: 1,
            max: 2,
            message: "TestRouter: connection lost, retrying (1/2)…".into(),
            continued: false
        }
    );
}

#[tokio::test]
async fn exhausts_retries_on_repeated_http_503_and_reports_the_error() {
    let server = FakeServer::start(|_| Reply::text(503, "Service Unavailable")).await;
    let events = collect(provider(&v1(&server), None).stream(&hi(), StreamOptions::default())).await;
    assert_eq!(server.requests().len(), 3);
    assert_eq!(retries(&events).len(), 2);
    assert!(!events.iter().any(|e| matches!(e, ProviderEvent::Done { .. })));
    assert!(error_of(&events).contains("is having trouble (503)"));
}

#[tokio::test]
async fn retries_on_429_and_waits_what_retry_after_says() {
    // Scaled by 0.01: Retry-After 2 s is 20 ms; the first backoff (1 s, plus jitter) at most 12.5 ms.
    let server = FakeServer::start(counting(|n| {
        if n == 1 {
            Reply::text(429, "Rate limited").header("Retry-After", "2")
        } else {
            Reply::sse(&[delta("ok"), stop(), done()])
        }
    }))
    .await;
    let start = Instant::now();
    let events =
        collect(with_timing(&v1(&server), None, RetryTiming { scale: 0.01 }).stream(&hi(), StreamOptions::default())).await;
    assert!(start.elapsed() >= Duration::from_millis(20));
    assert_eq!(server.requests().len(), 2);
    assert!(events.contains(&text("ok")));
}

#[tokio::test]
async fn ignores_non_integer_retry_after_values_and_uses_the_backoff() {
    let server = FakeServer::start(counting(|n| {
        if n == 1 {
            Reply::text(429, "Rate limited").header("Retry-After", "Thu, 1 Jan 2026 00:00:00 GMT")
        } else {
            Reply::sse(&[delta("retried"), stop(), done()])
        }
    }))
    .await;
    let start = Instant::now();
    let events =
        collect(with_timing(&v1(&server), None, RetryTiming { scale: 0.01 }).stream(&hi(), StreamOptions::default())).await;
    // An HTTP date falls back to the first backoff (1 s, scaled); the 30 s cap is unit-tested in retry.rs.
    assert!(start.elapsed() >= Duration::from_millis(10));
    assert_eq!(server.requests().len(), 2);
    assert!(events.contains(&text("retried")));
}

#[tokio::test]
async fn caps_retry_after_at_30_seconds() {
    let server = FakeServer::start(counting(|n| {
        if n == 1 {
            Reply::text(429, "Rate limited").header("Retry-After", "3600")
        } else {
            Reply::sse(&[delta("retried"), stop(), done()])
        }
    }))
    .await;
    // Scaled by 0.001: 30 s is 30 ms; 3600 s would be 3.6 s.
    let start = Instant::now();
    let events = collect(provider(&v1(&server), None).stream(&hi(), StreamOptions::default())).await;
    assert!(start.elapsed() >= Duration::from_millis(30) && start.elapsed() < Duration::from_secs(2));
    assert!(events.contains(&text("retried")));
}

#[test]
fn jitter_lengthens_a_backoff_by_at_most_a_quarter() {
    let wait = Duration::from_secs(4);
    assert_eq!(jittered(wait, 0.0), wait);
    assert_eq!(jittered(wait, 1.0), wait.mul_f64(1.0 + JITTER));
    assert_eq!(jittered(wait, 7.0), Duration::from_secs(5), "clamped");
}

#[tokio::test]
async fn backoff_waits_are_jittered_but_never_shorter_than_the_backoff() {
    // Three 503s: 1 s + 4 s of backoff, scaled by 0.01 → at least 50 ms, at most 62.5 ms plus overhead.
    let server = FakeServer::start(|_| Reply::text(503, "Service Unavailable")).await;
    let start = Instant::now();
    collect(with_timing(&v1(&server), None, RetryTiming { scale: 0.01 }).stream(&hi(), StreamOptions::default())).await;
    let elapsed = start.elapsed();
    assert!(elapsed >= Duration::from_millis(50), "{elapsed:?}");
    assert!(elapsed < Duration::from_millis(1000), "{elapsed:?}");
}

#[tokio::test]
async fn cancel_stops_the_provider_promptly_even_during_a_backoff_wait() {
    // Real waits here: the point is that a real backoff is cut short.
    let server = FakeServer::start(|_| Reply::text(503, "Service Unavailable")).await;
    let cancel = CancellationToken::new();
    let start = Instant::now();
    let mut stream = with_timing(&v1(&server), None, RetryTiming::default())
        .stream(&hi(), StreamOptions { cancel: cancel.clone(), ..Default::default() });
    // The first event is the retry, after the first backoff (1 s plus jitter).
    let first = stream.next().await;
    cancel.cancel();
    let rest: Vec<_> = stream.collect().await;
    // Without an abortable wait it would take 1 + 4 = 5 s.
    assert!(start.elapsed() < Duration::from_secs(3));
    assert!(matches!(first, Some(ProviderEvent::Retry { .. })));
    assert_eq!(rest, vec![], "a cancelled stream ends quietly");
}

#[tokio::test]
async fn stops_quietly_when_cancelled_mid_stream() {
    let server = FakeServer::start(|_| Reply::sse(&[delta("first")]).ending(End::Hang)).await;
    let cancel = CancellationToken::new();
    let mut stream = provider(&v1(&server), None).stream(&hi(), StreamOptions { cancel: cancel.clone(), ..Default::default() });
    let mut events = Vec::new();
    while let Some(event) = tokio::time::timeout(Duration::from_secs(5), stream.next()).await.expect("ends promptly") {
        events.push(event);
        cancel.cancel();
    }
    assert_eq!(events, vec![text("first")]);
}

#[tokio::test]
async fn the_stream_is_static_and_dropping_it_drops_the_request() {
    let server = FakeServer::start(|_| Reply::sse(&[delta("first")]).ending(End::Hang)).await;
    let provider = Arc::new(provider(&v1(&server), None));
    let mut history = hi();
    let mut stream = provider.stream(&history, StreamOptions::default());
    // The caller may append to its history while the reply streams.
    history.push(ChatTurn::User { text: "more".into() });
    assert_eq!(stream.next().await, Some(text("first")));
    drop(stream);
    assert_eq!(server.last().json()["messages"].as_array().unwrap().len(), 1);
}

// ---- #64: continue after a dropped stream. ----

#[tokio::test]
async fn a_drop_after_text_continues_from_the_partial_reply() {
    // The first attempt's connection is cut mid-body (a transport error, not a clean end).
    let server = FakeServer::start(counting(|n| match n {
        1 => Reply::sse(&[delta("Hello, wor")]).ending(End::Drop),
        _ => Reply::sse(&[delta("ld!"), stop(), done()]),
    }))
    .await;
    let events = collect(provider(&v1(&server), None).stream(&hi(), StreamOptions::default())).await;
    assert_eq!(
        events,
        vec![
            text("Hello, wor"),
            ProviderEvent::Retry {
                attempt: 1,
                max: 2,
                message: "TestRouter: connection lost, retrying (1/2)…".into(),
                continued: true
            },
            text("ld!"),
            ProviderEvent::Done { reason: Some("stop".into()) },
        ]
    );
    // The second request is the first one plus the partial reply and the note: the prefix is kept.
    let requests = server.requests();
    assert_eq!(
        requests[1].json()["messages"],
        json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "Hello, wor"},
            {"role": "user", "content": DROPPED_STREAM_NOTE},
        ])
    );
    let first = requests[0].text();
    let second = requests[1].text();
    let cut = first.find(r#"{"role":"user","content":"hi"}"#).unwrap() + r#"{"role":"user","content":"hi"}"#.len();
    assert_eq!(first[..cut], second[..cut]);
}

#[tokio::test]
async fn a_clean_early_end_after_text_continues_too_and_keeps_all_partial_text() {
    // Each attempt ends cleanly without [DONE]; the kept text grows across attempts.
    let server = FakeServer::start(counting(|n| match n {
        1 => Reply::sse(&[delta("One ")]),
        2 => Reply::sse(&[delta("two ")]),
        _ => Reply::sse(&[delta("three."), stop(), done()]),
    }))
    .await;
    let events = collect(provider(&v1(&server), None).stream(&hi(), StreamOptions::default())).await;
    assert_eq!(retries(&events), [true, true]);
    assert_eq!(server.requests()[2].json()["messages"][1], json!({"role": "assistant", "content": "One two "}));
    assert_eq!(events.last(), Some(&ProviderEvent::Done { reason: Some("stop".into()) }));
}

#[tokio::test]
async fn a_retry_with_kept_text_repeats_the_continue_request() {
    // A drop with text, then a 503: the retry after the 503 still continues (the loop keeps the text).
    let server = FakeServer::start(counting(|n| match n {
        1 => Reply::sse(&[delta("Part")]).ending(End::Drop),
        2 => Reply::text(503, "Service Unavailable"),
        _ => Reply::sse(&[delta("ial."), stop(), done()]),
    }))
    .await;
    let events = collect(provider(&v1(&server), None).stream(&hi(), StreamOptions::default())).await;
    assert_eq!(retries(&events), [true, true]);
    let requests = server.requests();
    assert_eq!(requests[1].text(), requests[2].text());
}

#[tokio::test]
async fn a_drop_before_any_text_starts_over() {
    let server = FakeServer::start(counting(|n| match n {
        1 => Reply::sse(&[json!({"choices": [{"delta": {"reasoning": "hm"}}]})]).ending(End::Drop),
        _ => Reply::sse(&[delta("ok"), stop(), done()]),
    }))
    .await;
    let events = collect(provider(&v1(&server), None).stream(&hi(), StreamOptions::default())).await;
    assert_eq!(retries(&events), [false]);
    let requests = server.requests();
    assert_eq!(requests[0].text(), requests[1].text());
}

#[tokio::test]
async fn a_drop_inside_a_tool_call_after_a_continue_starts_over_from_the_original_request() {
    let fragment = json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1", "function": {"name": "glob", "arguments": "{\"pat"}}]}}]});
    let server = FakeServer::start(counting(move |n| match n {
        1 => Reply::sse(&[delta("Let me look.")]).ending(End::Drop),
        2 => Reply::sse(std::slice::from_ref(&fragment)).ending(End::Drop),
        _ => Reply::sse(&[delta("Done."), stop(), done()]),
    }))
    .await;
    let events = collect(provider(&v1(&server), None).stream(&hi(), StreamOptions::default())).await;
    assert_eq!(retries(&events), [true, false]);
    let requests = server.requests();
    assert_eq!(requests[2].text(), requests[0].text());
}

#[tokio::test]
async fn invalid_json_in_a_chunk_counts_as_a_drop() {
    let server = FakeServer::start(counting(|n| match n {
        1 => Reply::sse(&[json!("{not json")]),
        _ => Reply::sse(&[delta("ok"), stop(), done()]),
    }))
    .await;
    let events = collect(provider(&v1(&server), None).stream(&hi(), StreamOptions::default())).await;
    assert_eq!(retries(&events), [false]);
    assert!(events.contains(&text("ok")));
}
