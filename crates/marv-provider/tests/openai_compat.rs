//! The OpenAI-compatible adapter against a local server. Ported from Marv's
//! `tests/openai-compat.test.ts`, plus #64 (continue after a drop, jitter).

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures::StreamExt;
use marv_core::{ChatTurn, ProviderEvent, RetryCause, ToolCall, ToolSpec, Usage};
use marv_provider::degenerate::DEGENERATE_NOTE;
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
            call("c1", "read_file", r#"{"path":"a.ts"}"#),
            call("c2", "glob", r#"{"pattern":"*"}"#),
            // The usage (with #90's ReplyEnd) is sent at the end of the reply, after the calls.
            ProviderEvent::Usage(
                Usage { prompt_tokens: 1200, completion_tokens: 30, cached_tokens: Some(1000), cost: Some(0.00042) },
                marv_core::ReplyEnd { finish_reason: Some("tool_calls".into()), raw_response: None }
            ),
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
async fn a_connection_reset_mid_stream_is_retried_and_continued() {
    // #107: a broken TLS session ends the connection with a reset, not a clean close.
    let server = FakeServer::start(counting(|n| {
        if n == 1 { Reply::sse(&[delta("Half ")]).ending(End::Reset) } else { Reply::sse(&[delta("done."), stop(), done()]) }
    }))
    .await;
    let events = collect(provider(&v1(&server), None).stream(&hi(), StreamOptions::default())).await;
    assert_eq!(retries(&events), [true], "{events:?}");
    assert!(events.contains(&text("done.")));
    assert!(!events.iter().any(|e| matches!(e, ProviderEvent::Error(_))), "{events:?}");
}

#[tokio::test]
async fn a_request_whose_connection_is_reset_before_any_reply_is_retried() {
    // #107: the send itself fails (a pooled connection whose TLS session broke fails the same way).
    let server = FakeServer::start(counting(|n| {
        if n == 1 { Reply::text(200, "").ending(End::ResetBeforeReply) } else { Reply::sse(&[delta("ok"), stop(), done()]) }
    }))
    .await;
    let events = collect(provider(&v1(&server), None).stream(&hi(), StreamOptions::default())).await;
    assert_eq!(retries(&events), [false], "{events:?}");
    assert!(events.contains(&text("ok")));
}

#[tokio::test]
async fn an_upstream_abort_inside_the_stream_is_retried_and_reported_in_full_if_it_persists() {
    // OpenRouter sends the upstream's failure as an error object after the 200.
    let abort = json!({"error": {"code": 502, "message": "The operation was aborted"}});
    let server = FakeServer::start(counting(move |n| {
        if n == 1 { Reply::sse(&[delta("Hal"), abort.clone()]) } else { Reply::sse(&[delta("f."), stop(), done()]) }
    }))
    .await;
    let events = collect(provider(&v1(&server), None).stream(&hi(), StreamOptions::default())).await;
    assert_eq!(retries(&events), [true]);
    assert!(events.contains(&text("f.")));
    assert!(!events.iter().any(|e| matches!(e, ProviderEvent::Error(_))));

    let abort = json!({"error": {"code": 502, "message": "The operation was aborted"}});
    let server = FakeServer::start(move |_| Reply::sse(std::slice::from_ref(&abort))).await;
    let events = collect(provider(&v1(&server), None).stream(&hi(), StreamOptions::default())).await;
    assert_eq!(server.requests().len(), 3);
    assert_eq!(error_of(&events), "TestRouter: The operation was aborted");
}

fn idle(base_url: &str, idle: Duration) -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(OpenAiCompatOptions {
        label: "TestRouter".into(),
        base_url: base_url.into(),
        model: "acme/model-1".into(),
        timing: FAST,
        idle_timeout: Some(idle),
        ..Default::default()
    })
}

fn causes(events: &[ProviderEvent]) -> Vec<RetryCause> {
    events.iter().filter_map(|e| if let ProviderEvent::Retry { cause, .. } = e { Some(*cause) } else { None }).collect()
}

#[tokio::test]
async fn a_stream_that_goes_silent_is_retried_after_the_idle_timeout_and_continued() {
    // #88: the server sends part of the reply, then nothing for 3 s (it would end the body after that).
    let server = FakeServer::start(counting(|n| {
        if n == 1 {
            Reply::sse(&[delta("Half ")]).ending(End::StallThenComplete(Duration::from_secs(3)))
        } else {
            Reply::sse(&[delta("done."), stop(), done()])
        }
    }))
    .await;
    let start = Instant::now();
    let events = collect(idle(&v1(&server), Duration::from_millis(150)).stream(&hi(), StreamOptions::default())).await;
    assert!(start.elapsed() < Duration::from_secs(2), "waited out the stall: {:?}", start.elapsed());
    assert_eq!(retries(&events), [true], "the partial reply is continued (#64)");
    assert_eq!(causes(&events), [RetryCause::Stalled]);
    let message =
        events.iter().find_map(|e| if let ProviderEvent::Retry { message, .. } = e { Some(message.clone()) } else { None });
    assert_eq!(message.as_deref(), Some("TestRouter: the stream stalled, retrying (1/2)…"));
    assert!(events.contains(&text("done.")));
    assert!(server.last().text().contains(DROPPED_STREAM_NOTE));
}

#[tokio::test]
async fn keep_alives_are_not_a_stall_and_a_stream_that_always_stalls_ends_with_a_clear_error() {
    // Keep-alive comments every 50 ms for 400 ms, with a 150 ms idle timeout: alive, so not cut off.
    let mut alive = Reply::sse(&[]).paced(Duration::from_millis(50));
    alive.chunks = (0..8).map(|_| b": OPENROUTER PROCESSING\n\n".to_vec()).collect();
    alive.chunks.extend(Reply::sse(&[delta("ok"), stop(), done()]).chunks);
    let server = FakeServer::start(move |_| alive.clone()).await;
    let paced = OpenAiCompatProvider::new(OpenAiCompatOptions {
        label: "TestRouter".into(),
        base_url: v1(&server),
        timing: FAST,
        idle_timeout: Some(Duration::from_millis(150)),
        ..Default::default()
    });
    let events = collect(paced.stream(&hi(), StreamOptions::default())).await;
    assert!(retries(&events).is_empty(), "{events:?}");
    assert!(events.contains(&text("ok")));

    let server = FakeServer::start(|_| Reply::sse(&[]).ending(End::StallThenComplete(Duration::from_secs(3)))).await;
    let events = collect(idle(&v1(&server), Duration::from_millis(100)).stream(&hi(), StreamOptions::default())).await;
    assert_eq!(server.requests().len(), 3);
    assert_eq!(causes(&events), [RetryCause::Stalled, RetryCause::Stalled]);
    assert_eq!(error_of(&events), "TestRouter: the stream stalled (nothing arrived for 100 ms), on every attempt. Try again.");
}

/// #96: what GLM streamed for six minutes: reasoning made of nothing but newlines (here 64 KB of it).
fn newline_flood() -> Reply {
    let mut events: Vec<Value> = (0..128).map(|_| json!({"choices": [{"delta": {"reasoning": "\n".repeat(512)}}]})).collect();
    events.extend([stop(), done()]);
    Reply::sse(&events)
}

#[tokio::test]
async fn a_degenerate_reply_is_aborted_and_asked_again_once_with_a_note() {
    let server = FakeServer::start(counting(|n| {
        if n == 1 {
            let mut flood = newline_flood();
            flood.chunks.insert(0, Reply::sse(&[delta("Let me check")]).chunks.remove(0));
            flood
        } else {
            Reply::sse(&[delta("All done."), stop(), done()])
        }
    }))
    .await;
    let events = collect(provider(&v1(&server), None).stream(&hi(), StreamOptions::default())).await;
    assert_eq!(retries(&events), [false], "the attempt is thrown away, not continued");
    let Some(RetryCause::Degenerate { discarded }) = causes(&events).first().copied() else { panic!("{events:?}") };
    // Aborted within the guard's tail (16 KB, checked every KB), long before the 64 KB the server would have sent.
    let thinking: usize = events.iter().map(|e| if let ProviderEvent::ThinkingDelta(t) = e { t.len() } else { 0 }).sum();
    assert!(thinking < 20 * 1024 && discarded >= thinking as u64, "{thinking} {discarded}");
    assert!(events.contains(&text("All done.")));
    assert!(!events.iter().any(|e| matches!(e, ProviderEvent::Error(_))));
    // The retry starts over (no partial reply kept), with the note after the history.
    let retried = server.last().json();
    let messages = retried["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1], json!({"role": "user", "content": DEGENERATE_NOTE}));
}

#[tokio::test]
async fn a_newline_flood_in_reasoning_details_alone_is_aborted_too() {
    // #96: a model that reasons only in `reasoning_details` blocks, with no plain `reasoning` field.
    let server = FakeServer::start(counting(|n| {
        if n == 1 {
            let block = json!({"choices": [{"delta": {"reasoning_details": [{"type": "reasoning.text", "text": "\n".repeat(512), "index": 0}]}}]});
            let mut events: Vec<Value> = (0..128).map(|_| block.clone()).collect();
            events.extend([stop(), done()]);
            Reply::sse(&events)
        } else {
            Reply::sse(&[delta("ok"), stop(), done()])
        }
    }))
    .await;
    let events = collect(provider(&v1(&server), None).stream(&hi(), StreamOptions::default())).await;
    assert!(matches!(causes(&events)[..], [RetryCause::Degenerate { discarded }] if discarded >= 16 * 1024), "{events:?}");
    assert!(events.contains(&text("ok")));
}

#[tokio::test]
async fn a_reply_that_degenerates_again_ends_the_turn_with_a_clear_error() {
    let server = FakeServer::start(|_| newline_flood()).await;
    let events = collect(provider(&v1(&server), None).stream(&hi(), StreamOptions::default())).await;
    assert_eq!(server.requests().len(), 2, "one retry only");
    assert_eq!(causes(&events).len(), 1);
    assert!(error_of(&events).starts_with("TestRouter: the model's reply degenerated again"), "{}", error_of(&events));
    // The second attempt is thrown away too (nothing of it reaches the history), with its bytes, right before the error.
    let n = events.len();
    assert!(
        matches!(&events[n - 2], ProviderEvent::Discard { cause: RetryCause::Degenerate { discarded } } if *discarded > 0),
        "{:?}",
        &events[n - 2..]
    );
    assert!(!events.iter().any(|e| matches!(e, ProviderEvent::Done { .. })));
}

fn capped(base_url: &str, max_tokens: Option<u64>) -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(OpenAiCompatOptions {
        label: "TestRouter".into(),
        base_url: base_url.into(),
        model: "acme/model-1".into(),
        timing: FAST,
        max_tokens,
        ..Default::default()
    })
}

#[tokio::test]
async fn max_tokens_is_sent_and_left_out_for_good_when_the_provider_refuses_the_field() {
    let server = FakeServer::start(|r| {
        if r.json().get("max_tokens").is_some() {
            Reply::json(400, json!({"error": {"message": "Unsupported parameter: 'max_tokens'"}}))
        } else {
            Reply::sse(&[delta("ok"), stop(), done()])
        }
    })
    .await;
    let provider = capped(&v1(&server), Some(32_000));
    let events = collect(provider.stream(&hi(), StreamOptions::default())).await;
    assert!(events.contains(&text("ok")), "{events:?}");
    assert!(retries(&events).is_empty(), "not a connection retry");
    let sent: Vec<_> = server.requests().iter().map(|r| r.json().get("max_tokens").cloned()).collect();
    assert_eq!(sent, [Some(json!(32_000)), None]);
    // The next stream doesn't try again.
    collect(provider.stream(&hi(), StreamOptions::default())).await;
    assert_eq!(server.requests().len(), 3);
    assert_eq!(server.last().json().get("max_tokens"), None);
}

/// OpenRouter's wording when the prompt plus the requested output don't fit the window.
const WINDOW_REFUSAL: &str = "This endpoint's maximum context length is 131072 tokens. However, you requested about 140000 tokens (108000 of text input, 32000 in the output).";

/// #116: what's left of OpenRouter's 131072-token window beside a 108000-token prompt, less the 5% headroom.
const LEFT: u64 = 131_072 - 113_400;

#[tokio::test]
async fn a_prompt_too_big_to_fit_beside_max_tokens_is_resent_with_what_is_left_and_later_requests_keep_that_cap() {
    let first = Arc::new(AtomicUsize::new(0));
    let seen = first.clone();
    let server = FakeServer::start(move |r| {
        if r.json().get("max_tokens").is_some() && seen.fetch_add(1, Ordering::SeqCst) == 0 {
            Reply::json(400, json!({"error": {"message": WINDOW_REFUSAL}}))
        } else {
            Reply::sse(&[delta("ok"), stop(), done()])
        }
    })
    .await;
    let provider = capped(&v1(&server), Some(32_000));
    let events = collect(provider.stream(&hi(), StreamOptions::default())).await;
    assert!(events.contains(&text("ok")));
    assert!(retries(&events).is_empty(), "not a connection retry");
    assert_eq!(provider.room().window, Some(131_072), "the window the refusal named is kept");
    // The next step fits its cap beside the prompt at once: no refusal, and never uncapped.
    collect(provider.stream(&hi(), StreamOptions::default())).await;
    let sent: Vec<_> = server.requests().iter().map(|r| r.json().get("max_tokens").cloned()).collect();
    assert_eq!(sent, [Some(json!(32_000)), Some(json!(LEFT)), Some(json!(LEFT))]);
}

#[tokio::test]
async fn when_the_fitted_cap_is_refused_too_the_request_goes_without_one() {
    let server = FakeServer::start(|r| {
        if r.json().get("max_tokens").is_some() {
            Reply::json(400, json!({"error": {"message": WINDOW_REFUSAL}}))
        } else {
            Reply::sse(&[delta("ok"), stop(), done()])
        }
    })
    .await;
    let events = collect(capped(&v1(&server), Some(32_000)).stream(&hi(), StreamOptions::default())).await;
    assert!(events.contains(&text("ok")));
    let sent: Vec<_> = server.requests().iter().map(|r| r.json().get("max_tokens").cloned()).collect();
    assert_eq!(sent, [Some(json!(32_000)), Some(json!(LEFT)), None]);
}

/// #116: a provider that knows the window (from the model list).
fn windowed(base_url: &str, window: u64) -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(OpenAiCompatOptions { context_length: Some(window), ..capped(base_url, Some(32_000)).options })
}

#[tokio::test]
async fn near_a_full_window_a_step_sends_one_request_with_a_smaller_cap() {
    // Like OpenRouter: the prompt is 30000 tokens and the window 50000, so more than 20000 in the output is refused.
    const PROMPT: u64 = 30_000;
    const WINDOW: u64 = 50_000;
    let server = FakeServer::start(|r| {
        let max = r.json().get("max_tokens").and_then(Value::as_u64);
        if max.is_some_and(|max| PROMPT + max > WINDOW) {
            let message = format!(
                "This endpoint's maximum context length is {WINDOW} tokens. However, you requested about {} tokens ({PROMPT} of text input, {} in the output).",
                PROMPT + max.unwrap(),
                max.unwrap()
            );
            return Reply::json(400, json!({"error": {"message": message}}));
        }
        Reply::sse(&[
            delta("ok"),
            stop(),
            json!({"choices": [], "usage": {"prompt_tokens": PROMPT, "completion_tokens": 2}}),
            done(),
        ])
    })
    .await;
    let provider = windowed(&v1(&server), WINDOW);
    // The first step's prompt is estimated (4 characters a token): a short body leaves room for the whole 32000…
    // but the server counts 30000 prompt tokens, so this one is refused and fitted once.
    collect(provider.stream(&hi(), StreamOptions::default())).await;
    let before = server.requests().len();
    // From then on the measured prompt size clamps the cap: one request per step, capped.
    for _ in 0..2 {
        let events = collect(provider.stream(&hi(), StreamOptions::default())).await;
        assert!(events.contains(&text("ok")), "{events:?}");
    }
    let later: Vec<_> = server.requests()[before..].iter().map(|r| r.json().get("max_tokens").and_then(Value::as_u64)).collect();
    assert_eq!(later.len(), 2, "one request per step: {later:?}");
    for max in later {
        let max = max.expect("still capped");
        assert!(max < WINDOW - PROMPT && max > 18_000, "{max}");
    }
}

#[tokio::test]
async fn a_reply_measures_the_prompt_so_the_next_cap_fits_before_any_refusal() {
    // The reply says the prompt was 30000 tokens, in a 50000-token window: the next request asks for at most what's left.
    let server = FakeServer::start(|_| {
        Reply::sse(&[delta("ok"), stop(), json!({"choices": [], "usage": {"prompt_tokens": 30_000}}), done()])
    })
    .await;
    let provider = windowed(&v1(&server), 50_000);
    collect(provider.stream(&hi(), StreamOptions::default())).await;
    collect(provider.stream(&hi(), StreamOptions::default())).await;
    let sent: Vec<_> = server.requests().iter().map(|r| r.json().get("max_tokens").and_then(Value::as_u64)).collect();
    assert_eq!(sent, [Some(32_000), Some(50_000 - 31_500)]);
}

#[tokio::test]
async fn a_prompt_that_fills_the_window_still_gets_the_floor() {
    let server = FakeServer::start(|_| {
        Reply::sse(&[delta("ok"), stop(), json!({"choices": [], "usage": {"prompt_tokens": 49_900}}), done()])
    })
    .await;
    let provider = windowed(&v1(&server), 50_000);
    collect(provider.stream(&hi(), StreamOptions::default())).await;
    collect(provider.stream(&hi(), StreamOptions::default())).await;
    assert_eq!(server.last().json().get("max_tokens"), Some(&json!(marv_provider::openai_compat::MIN_MAX_TOKENS)));
}

#[tokio::test]
async fn a_refusal_that_names_max_tokens_drops_it_for_that_request_only() {
    // A refusal that names the field but doesn't say it's unsupported drops it for that request only.
    let first = Arc::new(AtomicUsize::new(0));
    let seen = first.clone();
    let server = FakeServer::start(move |r| {
        if r.json().get("max_tokens").is_some() && seen.fetch_add(1, Ordering::SeqCst) == 0 {
            Reply::json(400, json!({"error": {"message": "max_tokens exceeds the remaining context"}}))
        } else {
            Reply::sse(&[delta("ok"), stop(), done()])
        }
    })
    .await;
    let provider = capped(&v1(&server), Some(32_000));
    collect(provider.stream(&hi(), StreamOptions::default())).await;
    collect(provider.stream(&hi(), StreamOptions::default())).await;
    let sent: Vec<_> = server.requests().iter().map(|r| r.json().get("max_tokens").cloned()).collect();
    assert_eq!(sent, [Some(json!(32_000)), None, Some(json!(32_000))]);
    // Any other refusal is reported as before.
    let server = FakeServer::start(|_| Reply::json(400, json!({"error": {"message": "tools are not supported"}}))).await;
    let events = collect(capped(&v1(&server), Some(32_000)).stream(&hi(), StreamOptions::default())).await;
    assert_eq!(server.requests().len(), 1);
    assert!(error_of(&events).contains("tools are not supported"));
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
            continued: false,
            cause: Default::default(),
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
                continued: true,
                cause: RetryCause::Dropped,
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
