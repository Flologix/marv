//! The Ollama adapter against a local stand-in for its native API. Ported from Marv's
//! `tests/ollama.test.ts`, plus #64 (continue after a drop).

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::StreamExt;
use marv_core::{ChatTurn, ProviderEvent, RetryCause, ToolCall, ToolSpec, Usage};
use marv_provider::degenerate::DEGENERATE_NOTE;
use marv_provider::ollama::{OllamaOptions, OllamaProvider};
use marv_provider::openai_compat::DROPPED_STREAM_NOTE;
use marv_provider::retry::RetryTiming;
use marv_provider::{EventStream, Provider, StreamOptions};
use serde_json::{Value, json};
use support::{End, FakeServer, Reply, Request};
use tokio_util::sync::CancellationToken;

fn options(base_url: &str, context_length: u64, thinking: bool) -> OllamaOptions {
    OllamaOptions {
        base_url: base_url.into(),
        model: "qwen3.5:9b".into(),
        context_length,
        thinking,
        timing: RetryTiming { scale: 0.001 },
        ..Default::default()
    }
}

fn provider(base_url: &str, thinking: bool) -> OllamaProvider {
    OllamaProvider::new(options(base_url, 32768, thinking))
}

fn hi() -> Vec<ChatTurn> {
    vec![ChatTurn::User { text: "hi".into() }]
}

async fn collect(stream: EventStream) -> Vec<ProviderEvent> {
    stream.collect().await
}

async fn say_hi(p: &OllamaProvider) -> Vec<ProviderEvent> {
    collect(p.stream(&hi(), StreamOptions::default())).await
}

fn msg(message: Value) -> Value {
    let mut m = json!({"role": "assistant", "content": ""});
    m.as_object_mut().unwrap().extend(message.as_object().unwrap().clone());
    json!({"message": m, "done": false})
}

fn end() -> Value {
    json!({"message": {"role": "assistant", "content": ""}, "done": true, "done_reason": "stop", "prompt_eval_count": 298, "eval_count": 26})
}

fn end_reporting(tokens: Option<u64>) -> Value {
    let mut e = end();
    match tokens {
        Some(t) => e["prompt_eval_count"] = json!(t),
        None => drop(e.as_object_mut().unwrap().remove("prompt_eval_count")),
    }
    e
}

/// `/api/show` gets `{}` (it may fail; it changes nothing); `/api/chat` gets `chat(n)` for its n-th request.
fn chat(reply: impl Fn(usize) -> Reply + Send + Sync + 'static) -> impl Fn(&Request) -> Reply + Send + Sync + 'static {
    let n = AtomicUsize::new(0);
    move |req| {
        if req.path != "/api/chat" {
            return Reply::json(200, json!({}));
        }
        reply(n.fetch_add(1, Ordering::SeqCst) + 1)
    }
}

/// The last `/api/chat` request's body.
fn last_chat(server: &FakeServer) -> Value {
    server.requests().into_iter().rev().find(|r| r.path == "/api/chat").expect("a chat request").json()
}

fn error_of(events: &[ProviderEvent]) -> &str {
    events.iter().find_map(|e| if let ProviderEvent::Error(m) = e { Some(m.as_str()) } else { None }).expect("an error event")
}

fn retries(events: &[ProviderEvent]) -> Vec<bool> {
    events.iter().filter_map(|e| if let ProviderEvent::Retry { continued, .. } = e { Some(*continued) } else { None }).collect()
}

#[tokio::test]
async fn streams_thinking_and_text_then_usage_and_done() {
    let server = FakeServer::start(chat(|_| {
        Reply::ndjson(&[msg(json!({"thinking": "Hmm."})), msg(json!({"content": "Hel"})), msg(json!({"content": "lo"})), end()])
    }))
    .await;
    assert_eq!(
        say_hi(&provider(&server.url(), false)).await,
        vec![
            ProviderEvent::ThinkingDelta("Hmm.".into()),
            ProviderEvent::TextDelta("Hel".into()),
            ProviderEvent::TextDelta("lo".into()),
            ProviderEvent::Usage(Usage { prompt_tokens: 298, completion_tokens: 26, cached_tokens: None, cost: None }),
            ProviderEvent::Done { reason: Some("stop".into()) },
        ]
    );
}

#[tokio::test]
async fn turns_tool_calls_with_object_arguments_into_marvs_tool_calls() {
    let server = FakeServer::start(chat(|_| {
        Reply::ndjson(&[
            msg(json!({"tool_calls": [{"id": "call_x", "function": {"index": 0, "name": "read_file", "arguments": {"path": "package.json"}}}]})),
            end(),
        ])
    }))
    .await;
    let events =
        collect(provider(&server.url(), false).stream(&[ChatTurn::User { text: "read it".into() }], StreamOptions::default()))
            .await;
    assert!(events.contains(&ProviderEvent::ToolCall(ToolCall {
        id: "call_x".into(),
        name: "read_file".into(),
        arguments: r#"{"path":"package.json"}"#.into()
    })));
}

#[tokio::test]
async fn asks_for_the_context_size_and_thinking_setting_and_sends_history_in_ollamas_format() {
    let server = FakeServer::start(chat(|_| Reply::ndjson(&[end()]))).await;
    let tools =
        vec![ToolSpec { name: "read_file".into(), description: "Read a file".into(), parameters: json!({"type": "object"}) }];
    let history = vec![
        ChatTurn::User { text: "read it".into() },
        ChatTurn::Assistant {
            text: "".into(),
            tool_calls: vec![ToolCall { id: "c1".into(), name: "read_file".into(), arguments: r#"{"path":"a.ts"}"#.into() }],
            reasoning: None,
        },
        ChatTurn::Tool { call_id: "c1".into(), name: "read_file".into(), text: "contents".into() },
    ];
    let options = StreamOptions { system: Some("You are Marv.".into()), tools, ..Default::default() };
    collect(provider(&server.url(), true).stream(&history, options)).await;
    let request = server.last();
    assert_eq!(request.path, "/api/chat");
    // Byte for byte: settings first, then the prompt (the cacheable part).
    assert_eq!(
        request.text(),
        r#"{"model":"qwen3.5:9b","stream":true,"think":true,"options":{"num_ctx":32768,"num_predict":16384,"presence_penalty":0},"messages":[{"role":"system","content":"You are Marv."},{"role":"user","content":"read it"},{"role":"assistant","content":"","tool_calls":[{"function":{"name":"read_file","arguments":{"path":"a.ts"}}}]},{"role":"tool","tool_name":"read_file","content":"contents"}],"tools":[{"type":"function","function":{"name":"read_file","description":"Read a file","parameters":{"type":"object"}}}]}"#
    );
}

#[tokio::test]
async fn caps_each_reply_at_a_quarter_of_a_small_context_window() {
    let server = FakeServer::start(chat(|_| Reply::ndjson(&[end()]))).await;
    say_hi(&OllamaProvider::new(OllamaOptions { model: "m".into(), ..options(&server.url(), 8192, false) })).await;
    assert_eq!(last_chat(&server)["options"], json!({"num_ctx": 8192, "num_predict": 2048, "presence_penalty": 0}));
}

#[tokio::test]
async fn with_thinking_on_a_reply_may_be_twice_as_long() {
    let server = FakeServer::start(chat(|_| Reply::ndjson(&[end()]))).await;
    say_hi(&provider(&server.url(), true)).await;
    assert_eq!(last_chat(&server)["options"], json!({"num_ctx": 32768, "num_predict": 16384, "presence_penalty": 0}));
}

mod fits_the_reply_cap_to_the_room_the_prompt_leaves {
    use super::*;

    fn history() -> Vec<ChatTurn> {
        vec![ChatTurn::User { text: "x".repeat(4000) }]
    }

    /// The size the provider measures a prompt by: its messages (and tools) as JSON.
    fn chars(h: &[ChatTurn]) -> usize {
        let messages: Vec<Value> = h
            .iter()
            .map(|t| if let ChatTurn::User { text } = t { json!({"role": "user", "content": text}) } else { unreachable!() })
            .collect();
        json!({ "messages": messages }).to_string().len()
    }

    async fn reporting(tokens: u64) -> FakeServer {
        FakeServer::start(chat(move |_| Reply::ndjson(&[end_reporting(Some(tokens))]))).await
    }

    async fn send(p: &OllamaProvider, h: &[ChatTurn]) {
        collect(p.stream(h, StreamOptions::default())).await;
    }

    fn num_predict(server: &FakeServer) -> u64 {
        last_chat(server)["options"]["num_predict"].as_u64().unwrap()
    }

    #[tokio::test]
    async fn before_any_usage_is_known_the_share_of_the_window_alone() {
        let server = reporting(20000).await;
        send(&provider(&server.url(), true), &history()).await;
        assert_eq!(num_predict(&server), 16384);
    }

    #[tokio::test]
    async fn then_at_most_the_window_minus_the_estimated_prompt_with_5_percent_headroom() {
        let server = reporting(20000).await;
        let p = provider(&server.url(), true);
        send(&p, &history()).await;
        send(&p, &history()).await;
        // Same prompt: 20000 tokens, plus 5% → 21000; 32768 − 21000 leaves 11768 (the share alone would be 16384).
        assert_eq!(num_predict(&server), 11768);
    }

    #[tokio::test]
    async fn a_prompt_that_grew_since_is_estimated_at_the_measured_tokens_per_character() {
        let server = reporting(20000).await;
        let p = provider(&server.url(), true);
        send(&p, &history()).await;
        let mut longer = history();
        longer.push(ChatTurn::User { text: "y".repeat(2000) });
        send(&p, &longer).await;
        let estimate = (chars(&longer) as f64 * 20000.0 * 1.05 / chars(&history()) as f64).ceil() as u64;
        assert_eq!(num_predict(&server), 32768 - estimate);
    }

    #[tokio::test]
    async fn a_nearly_full_window_still_leaves_a_short_reply() {
        let server = reporting(32000).await;
        let p = provider(&server.url(), false);
        send(&p, &history()).await;
        send(&p, &history()).await;
        assert_eq!(num_predict(&server), 1024);
    }

    #[tokio::test]
    async fn with_room_to_spare_the_share_still_caps_it() {
        let server = reporting(1000).await;
        let p = provider(&server.url(), false);
        send(&p, &history()).await;
        send(&p, &history()).await;
        assert_eq!(num_predict(&server), 8192);
    }

    #[tokio::test]
    async fn a_cache_hit_or_a_reply_without_a_count_does_not_lower_the_estimate() {
        for later in [Some(50), None] {
            let server =
                FakeServer::start(chat(move |n| Reply::ndjson(&[end_reporting(if n == 1 { Some(20000) } else { later })]))).await;
            let p = provider(&server.url(), true);
            send(&p, &history()).await;
            send(&p, &history()).await;
            send(&p, &history()).await;
            assert_eq!(num_predict(&server), 11768, "{later:?}");
        }
    }
}

#[tokio::test]
async fn other_server_errors_are_not_retried() {
    // Out of memory, a failed load: a second try would only repeat them.
    let server = FakeServer::start(chat(|_| Reply::json(500, json!({"error": "model requires more system memory"})))).await;
    let events = say_hi(&provider(&server.url(), false)).await;
    assert_eq!(server.count("/api/chat"), 1);
    assert_eq!(events.len(), 1);
    assert!(error_of(&events).contains("more system memory"));
}

#[tokio::test]
async fn an_unparsable_tool_call_is_retried_once_and_a_second_one_is_reported() {
    let retried = FakeServer::start(chat(|n| {
        if n == 1 {
            Reply::json(500, json!({"error": "XML syntax error on line 181: unexpected EOF"}))
        } else {
            Reply::ndjson(&[msg(json!({"content": "ok"})), end()])
        }
    }))
    .await;
    let events = say_hi(&provider(&retried.url(), false)).await;
    assert!(events.contains(&ProviderEvent::TextDelta("ok".into())));
    assert_eq!(retried.count("/api/chat"), 2);
    // Not a connection retry: no Retry event.
    assert_eq!(retries(&events), Vec::<bool>::new());

    let twice =
        FakeServer::start(chat(|_| Reply::json(500, json!({"error": "XML syntax error on line 9: unexpected EOF"})))).await;
    let events = say_hi(&provider(&twice.url(), false)).await;
    assert_eq!(twice.count("/api/chat"), 2);
    assert_eq!(events.len(), 1);
    assert!(error_of(&events).contains("XML syntax error"));
}

#[tokio::test]
async fn accepts_a_base_url_with_a_trailing_v1() {
    let server = FakeServer::start(chat(|_| Reply::ndjson(&[end()]))).await;
    say_hi(&provider(&format!("{}/v1", server.url()), false)).await;
    assert_eq!(server.last().path, "/api/chat");
}

#[tokio::test]
async fn reports_a_missing_model_and_a_mid_stream_error() {
    let server = FakeServer::start(|_| Reply::json(404, json!({"error": "model 'nope' not found"}))).await;
    let events = say_hi(&provider(&server.url(), false)).await;
    assert!(matches!(&events[0], ProviderEvent::Error(m) if m.contains("wasn't found on Ollama")));

    let server =
        FakeServer::start(chat(|_| Reply::ndjson(&[msg(json!({"content": "par"})), json!({"error": "out of memory"})]))).await;
    assert_eq!(
        say_hi(&provider(&server.url(), false)).await,
        vec![ProviderEvent::TextDelta("par".into()), ProviderEvent::Error("Ollama: out of memory".into())]
    );
}

#[tokio::test]
async fn explains_when_ollama_isnt_running_after_retries() {
    let events = say_hi(&provider("http://localhost:1", false)).await;
    // Unable to connect is a network error, retried 3 times, then reported.
    assert_eq!(retries(&events).len(), 2);
    let error = error_of(&events);
    assert!(error.contains("Can't reach Ollama at http://localhost:1") && error.contains("Is Ollama running?"), "{error}");
}

#[tokio::test]
async fn resends_the_request_after_an_http_503_succeeding_on_retry() {
    let server =
        FakeServer::start(chat(|n| if n == 1 { Reply::text(503, "Service Unavailable") } else { Reply::ndjson(&[end()]) })).await;
    let events = say_hi(&provider(&server.url(), false)).await;
    assert_eq!(server.count("/api/chat"), 2);
    assert!(!events.iter().any(|e| matches!(e, ProviderEvent::Error(_))));
    assert_eq!(
        events[0],
        ProviderEvent::Retry {
            attempt: 1,
            max: 2,
            message: "Ollama: connection lost, retrying (1/2)…".into(),
            continued: false,
            cause: Default::default(),
        }
    );
}

#[tokio::test]
async fn exhausts_retries_on_repeated_http_503_and_reports_the_error() {
    let server = FakeServer::start(chat(|_| Reply::text(503, "Service Unavailable"))).await;
    let events = say_hi(&provider(&server.url(), false)).await;
    assert_eq!(server.count("/api/chat"), 3);
    assert_eq!(retries(&events).len(), 2);
    assert!(error_of(&events).contains("having trouble"));
}

#[test]
fn exposes_its_context_size() {
    assert_eq!(provider("http://localhost:11434", false).context_length(), Some(32768));
}

mod never_runs_the_model_past_its_trained_context_length {
    use super::*;

    /// Serves `/api/show` with `show` and `/api/chat` with a complete reply.
    async fn serving(show: impl Fn() -> Reply + Send + Sync + 'static) -> FakeServer {
        FakeServer::start(move |req| if req.path == "/api/show" { show() } else { Reply::ndjson(&[end()]) }).await
    }

    fn trained(n: u64) -> Reply {
        Reply::json(200, json!({"model_info": {"qwen3.context_length": n}}))
    }

    fn num_ctx(server: &FakeServer) -> u64 {
        last_chat(server)["options"]["num_ctx"].as_u64().unwrap()
    }

    #[tokio::test]
    async fn num_ctx_is_the_configured_window_or_the_trained_one_whichever_is_smaller() {
        let server = serving(|| trained(8192)).await;
        say_hi(&provider(&server.url(), false)).await;
        assert_eq!(num_ctx(&server), 8192);
        // The reply cap shrinks with the window (a quarter of it, thinking off).
        assert_eq!(last_chat(&server)["options"]["num_predict"], 2048);
    }

    #[tokio::test]
    async fn the_configured_window_stays_the_upper_bound() {
        // Ollama can't tell what fits in VRAM.
        let server = serving(|| trained(131072)).await;
        say_hi(&provider(&server.url(), false)).await;
        assert_eq!(num_ctx(&server), 32768);
    }

    #[tokio::test]
    async fn an_api_show_that_fails_or_says_nothing_changes_nothing() {
        let server = serving(|| Reply::text(500, "boom")).await;
        say_hi(&provider(&server.url(), false)).await;
        assert_eq!(num_ctx(&server), 32768);

        let server = serving(|| Reply::json(200, json!({"model_info": {}}))).await;
        say_hi(&provider(&server.url(), false)).await;
        assert_eq!(num_ctx(&server), 32768);
    }

    #[tokio::test]
    async fn a_failed_api_show_is_asked_again_on_the_next_stream() {
        // Ollama may have been starting up.
        let shows = AtomicUsize::new(0);
        let server =
            serving(move || if shows.fetch_add(1, Ordering::SeqCst) == 0 { Reply::text(500, "boom") } else { trained(8192) })
                .await;
        let p = provider(&server.url(), false);
        say_hi(&p).await;
        assert_eq!(num_ctx(&server), 32768);
        say_hi(&p).await;
        assert_eq!(num_ctx(&server), 8192);
    }

    #[tokio::test]
    async fn asks_api_show_once_for_the_model_then_keeps_the_window() {
        // A different num_ctx reloads the model.
        let server = serving(|| trained(8192)).await;
        let p = provider(&server.url(), false);
        say_hi(&p).await;
        say_hi(&p).await;
        let paths: Vec<_> = server.requests().into_iter().map(|r| r.path).collect();
        assert_eq!(paths, ["/api/show", "/api/chat", "/api/chat"]);
        assert_eq!(num_ctx(&server), 8192);
        assert_eq!(server.requests()[0].json(), json!({"model": "qwen3.5:9b"}));
    }

    #[tokio::test]
    async fn concurrent_streams_ask_only_once() {
        // The session and its subagents share one provider.
        let server = serving(|| trained(8192)).await;
        let p = Arc::new(provider(&server.url(), false));
        let streams: Vec<_> = (0..4).map(|_| collect(p.stream(&hi(), StreamOptions::default()))).collect();
        futures::future::join_all(streams).await;
        assert_eq!(server.count("/api/show"), 1);
        assert_eq!(server.count("/api/chat"), 4);
    }

    #[tokio::test]
    async fn reports_the_window_it_runs_at_once_known() {
        let server = serving(|| trained(8192)).await;
        let p = provider(&server.url(), false);
        assert_eq!(p.context_length(), Some(32768));
        say_hi(&p).await;
        assert_eq!(p.context_length(), Some(8192));
    }
}

#[tokio::test]
async fn a_stream_that_ends_without_its_done_line_retries_then_reports_the_error() {
    let server = FakeServer::start(chat(|_| Reply::ndjson(&[msg(json!({"content": "Writing the fi"}))]))).await;
    let events = say_hi(&provider(&server.url(), false)).await;
    assert_eq!(server.count("/api/chat"), 3);
    // The text was shown each time it streamed, and retries happened.
    assert_eq!(events.iter().filter(|e| matches!(e, ProviderEvent::TextDelta(_))).count(), 3);
    assert_eq!(retries(&events).len(), 2);
    assert_eq!(error_of(&events), "Ollama: the stream ended early, before the reply was complete. Try again.");
}

#[tokio::test]
async fn stops_quietly_when_cancelled_mid_stream() {
    let server = FakeServer::start(chat(|_| Reply::ndjson(&[msg(json!({"content": "first"}))]).ending(End::Hang))).await;
    let cancel = CancellationToken::new();
    let mut stream = provider(&server.url(), false).stream(&hi(), StreamOptions { cancel: cancel.clone(), ..Default::default() });
    let mut events = Vec::new();
    while let Some(event) = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next()).await.expect("ends promptly") {
        events.push(event);
        cancel.cancel();
    }
    assert_eq!(events, vec![ProviderEvent::TextDelta("first".into())]);
}

// ---- #64: continue after a dropped stream. ----

#[tokio::test]
async fn a_drop_after_text_continues_from_the_partial_reply() {
    let server = FakeServer::start(chat(|n| match n {
        1 => Reply::ndjson(&[msg(json!({"content": "Hello, wor"}))]).ending(End::Drop),
        _ => Reply::ndjson(&[msg(json!({"content": "ld!"})), end()]),
    }))
    .await;
    let events = say_hi(&provider(&server.url(), false)).await;
    assert_eq!(retries(&events), [true]);
    assert_eq!(
        last_chat(&server)["messages"],
        json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "Hello, wor"},
            {"role": "user", "content": DROPPED_STREAM_NOTE},
        ])
    );
}

#[tokio::test]
async fn a_drop_after_a_tool_call_arrived_starts_over() {
    let call = msg(json!({"content": "Looking.", "tool_calls": [{"function": {"name": "glob", "arguments": {}}}]}));
    let server = FakeServer::start(chat(move |n| match n {
        1 => Reply::ndjson(std::slice::from_ref(&call)).ending(End::Drop),
        _ => Reply::ndjson(&[end()]),
    }))
    .await;
    let events = say_hi(&provider(&server.url(), false)).await;
    assert_eq!(retries(&events), [false]);
    assert_eq!(last_chat(&server)["messages"], json!([{"role": "user", "content": "hi"}]));
}

#[tokio::test]
async fn a_degenerate_reply_is_aborted_and_asked_again_once_then_ends_the_turn() {
    // #96: a local model can loop too; Ollama's num_predict caps it, but only after thousands of tokens.
    let flood = || {
        let mut lines: Vec<Value> = (0..128).map(|_| msg(json!({"thinking": "\n".repeat(512)}))).collect();
        lines.push(end());
        Reply::ndjson(&lines)
    };
    let server =
        FakeServer::start(chat(
            move |n| if n == 1 { flood() } else { Reply::ndjson(&[msg(json!({"content": "Done."})), end()]) },
        ))
        .await;
    let events = say_hi(&provider(&server.url(), true)).await;
    assert!(
        events.iter().any(|e| matches!(e, ProviderEvent::Retry { continued: false, cause: RetryCause::Degenerate { .. }, .. })),
        "{events:?}"
    );
    assert!(events.contains(&ProviderEvent::TextDelta("Done.".into())));
    let messages = last_chat(&server)["messages"].as_array().unwrap().clone();
    assert_eq!(messages.last().unwrap(), &json!({"role": "user", "content": DEGENERATE_NOTE}));

    let server = FakeServer::start(chat(move |_| flood())).await;
    let events = say_hi(&provider(&server.url(), true)).await;
    assert_eq!(server.count("/api/chat"), 2);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ProviderEvent::Error(m) if m.starts_with("Ollama: the model's reply degenerated again")))
    );
}
