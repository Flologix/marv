//! The adapter for "OpenAI-compatible" chat APIs: OpenRouter, and anything else that speaks
//! `POST {baseUrl}/chat/completions` with streaming. Ports `src/provider/openai-compat.ts`.
//!
//! It translates their stream into [`ProviderEvent`]s, so nothing above this module knows which
//! vendor (or which wire format) is on the other end.
//!
//! - **Stateless API:** every request carries the whole conversation. [`to_messages`] is
//!   deterministic (typed structs, fixed field order), so repeated requests share a cacheable
//!   prefix. `ToolCall::arguments` is sent as the model's raw text, never re-serialized.
//! - `stream_options.include_usage` gets token counts, including cached tokens, in the final
//!   chunk. OpenRouter adds `cost` (USD actually charged) to every final usage chunk.
//! - Reasoning arrives as `reasoning` (OpenRouter) or `reasoning_content` (DeepSeek-style servers).
//! - **Tool calls arrive in fragments keyed by `index`** (id and name first, arguments in pieces)
//!   and are only complete at the end. Each fragment is also passed on as `ToolCallDelta` (display
//!   only: a long `write_file` being written); the first time a call's name is known it's worth
//!   telling the screen even before any arguments. A call without an id gets `call_<index>`.
//! - Errors can arrive **mid-stream**, after the 200 status was already sent.
//! - **A stream that ends without `[DONE]` or a `finish_reason` is partial** (a proxy closed it
//!   cleanly halfway: the text is partial and a tool call's arguments are cut off): retried, like
//!   a connection drop. After 3 attempts: "the stream ended early, before the reply was complete".
//! - Retry loop: up to [`crate::retry::MAX_ATTEMPTS`] for network errors and 429/502/503/504
//!   (Retry-After honoured); a `Retry` event (`attempt`/`max` = n/2) precedes each new attempt;
//!   cancel stops the retries and ends quietly.
//! - OpenRouter keeps a timeout, but per read (300 s) rather than for the whole reply: it sends
//!   keep-alives while it works, so the limit only catches dead connections.
//! - Cancel ends the stream quietly (no `Error` event), also during a backoff wait.
//!
//! marv-rs changes (not in Marv yet):
//! - **#56 cache_control breakpoints**: for `anthropic/*` and `google/gemini-*` (decided by model
//!   id in [`needs_cache_breakpoints`]), mark the system prompt and the history's last message
//!   with `cache_control: {type: "ephemeral"}` (at most 4). Earlier messages keep identical bytes;
//!   other models' requests stay byte-identical to today's.
//! - **#57 reasoning round-trip**: collect `reasoning_details` from the stream, emit them as
//!   [`ProviderEvent::Reasoning`], and send them back on the assistant message in later requests.
//! - **#59 `tool_choice: "none"`** for the wrap-up reply at the step limit (tools still sent).
//! - **#64 continue after a dropped stream**: a drop after text arrived, with no tool call
//!   mid-stream ([`crate::retry::drop_recovery`]), sends the next attempt with the partial text as
//!   an assistant turn plus [`DROPPED_STREAM_NOTE`], and its `Retry` event says `continued: true`
//!   (the loop keeps the text and appends the rest). A drop inside a tool call starts over
//!   (`continued: false`, the loop discards the attempt). Status and connection retries repeat the
//!   last request, so they say `continued` when partial text is being kept.
//! - **#64 jitter**: backoff waits get up to [`JITTER`] (25%) added at random, so clients dropped by
//!   one outage don't all come back at the same moment. `Retry-After` waits are taken as given.

use std::collections::BTreeMap;
use std::future::Future;
use std::time::Duration;

use futures::{FutureExt, StreamExt};
use marv_core::{ChatTurn, ProviderEvent, ToolCall, ToolChoice, ToolSpec, Usage};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::errors::{ErrorHints, ErrorObject, describe_error, error_message, http_error, unreachable};
use crate::retry::{
    BACKOFFS, DropRecovery, MAX_ATTEMPTS, RetryTiming, drop_recovery, is_network_error, parse_retry_after, retriable_http_status,
    sleep_abortable,
};
use crate::sse::parse_sse;
use crate::{EventStream, Provider, StreamOptions};

/// Marv #64: sent after the partial reply kept from a dropped stream. The same text as
/// `marv_agent::DROPPED_STREAM_NOTE` (marv-agent depends on this crate, so it can't be imported).
pub const DROPPED_STREAM_NOTE: &str = "(Automatic note from Marv, the program running you) The previous response was interrupted. Continue from where you left off.";

/// Marv #64: the most a backoff wait is lengthened at random (a share of it).
pub const JITTER: f64 = 0.25;

#[derive(Debug, Clone, Default)]
pub struct OpenAiCompatOptions {
    /// Shown in the status bar.
    pub name: String,
    /// Used in error messages, e.g. "OpenRouter".
    pub label: String,
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    /// Extra request headers (OpenRouter's `X-Title: Marv` attribution). Ordered, so the request is stable.
    pub headers: Vec<(String, String)>,
    /// Extra request body fields (`reasoning: {enabled}`), appended after Marv's own.
    pub body: serde_json::Map<String, Value>,
    /// Appended when the server can't be reached at all.
    pub offline_hint: Option<String>,
    /// What a bad key or an unknown model tells the user to change (default: the TUI's commands).
    pub hints: ErrorHints,
    pub timing: RetryTiming,
}

/// Marv #56: models that cache only (or more reliably) with explicit breakpoints. One small table.
pub fn needs_cache_breakpoints(model: &str) -> bool {
    model.starts_with("anthropic/") || model.starts_with("google/gemini-")
}

// ---- Request (wire) types. Field order here is the byte order on the wire: don't reorder. ----

/// Message content: a plain string, or parts when a cache breakpoint is attached (#56).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum WireContent {
    Text(String),
    Parts(Vec<WirePart>),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WirePart {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct CacheControl {
    #[serde(rename = "type")]
    pub kind: &'static str,
}

const EPHEMERAL: CacheControl = CacheControl { kind: "ephemeral" };

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum WireMessage {
    System {
        content: WireContent,
    },
    User {
        content: WireContent,
    },
    Assistant {
        content: WireContent,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<WireToolCall>>,
        /// #57: the opaque reasoning payload, re-sent byte for byte.
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_details: Option<Value>,
    },
    Tool {
        tool_call_id: String,
        content: WireContent,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WireToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: WireFunction,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WireFunction {
    pub name: String,
    /// The model's raw JSON text (prompt cache: never re-serialized).
    pub arguments: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WireTool<'a> {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: &'a ToolSpec,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StreamFlags {
    pub include_usage: bool,
}

/// The whole request body.
#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest<'a> {
    pub model: &'a str,
    pub stream: bool,
    pub stream_options: StreamFlags,
    pub messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<WireTool<'a>>,
    /// Only ever "none" (#59); `Auto` sends nothing, so today's requests keep their bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<&'static str>,
    #[serde(flatten)]
    pub extra: &'a serde_json::Map<String, Value>,
}

/// Marv's conversation in OpenAI's message format. Deterministic, so repeated requests share a
/// cacheable prefix. With `breakpoints` (#56) the system prompt and the last message carry
/// `cache_control`; every other message keeps exactly the bytes it has without them.
pub fn to_messages(history: &[ChatTurn], system: Option<&str>, breakpoints: bool) -> Vec<WireMessage> {
    let text = |s: &str| WireContent::Text(s.to_string());
    let mut messages: Vec<WireMessage> = system.map(|s| WireMessage::System { content: text(s) }).into_iter().collect();
    messages.extend(history.iter().map(|turn| match turn {
        ChatTurn::User { text: t } => WireMessage::User { content: text(t) },
        ChatTurn::Assistant { text: t, tool_calls, reasoning } => WireMessage::Assistant {
            content: text(t),
            tool_calls: (!tool_calls.is_empty()).then(|| {
                tool_calls
                    .iter()
                    .map(|c| WireToolCall {
                        id: c.id.clone(),
                        kind: "function",
                        function: WireFunction { name: c.name.clone(), arguments: c.arguments.clone() },
                    })
                    .collect()
            }),
            reasoning_details: reasoning.as_ref().map(|r| r.0.clone()),
        },
        ChatTurn::Tool { call_id, text: t, .. } => WireMessage::Tool { tool_call_id: call_id.clone(), content: text(t) },
    }));
    if breakpoints {
        let last = messages.len().saturating_sub(1);
        if system.is_some() {
            mark(&mut messages[0]);
        }
        if (last > 0 || system.is_none())
            && let Some(message) = messages.get_mut(last)
        {
            mark(message);
        }
    }
    messages
}

/// Turns a message's text content into one part with an ephemeral cache breakpoint (#56).
fn mark(message: &mut WireMessage) {
    let content = match message {
        WireMessage::System { content } | WireMessage::User { content } | WireMessage::Tool { content, .. } => content,
        WireMessage::Assistant { content, .. } => content,
    };
    if let WireContent::Text(t) = content {
        // An empty text part is rejected by Anthropic: leave such a message unmarked.
        if !t.is_empty() {
            *content =
                WireContent::Parts(vec![WirePart { kind: "text", text: std::mem::take(t), cache_control: Some(EPHEMERAL) }]);
        }
    }
}

/// Builds the request body for one stream.
pub fn request_body<'a>(
    options: &'a OpenAiCompatOptions,
    history: &[ChatTurn],
    system: Option<&str>,
    tools: &'a [ToolSpec],
    tool_choice: ToolChoice,
) -> ChatRequest<'a> {
    ChatRequest {
        model: &options.model,
        stream: true,
        stream_options: StreamFlags { include_usage: true },
        messages: to_messages(history, system, needs_cache_breakpoints(&options.model)),
        tools: tools.iter().map(|t| WireTool { kind: "function", function: t }).collect(),
        tool_choice: (tool_choice == ToolChoice::None).then_some("none"),
        extra: &options.body,
    }
}

// ---- Response (stream) types: only the fields Marv reads. ----

#[derive(Debug, Default, Deserialize)]
pub struct Chunk {
    #[serde(default)]
    pub choices: Vec<Choice>,
    #[serde(default)]
    pub usage: Option<ChunkUsage>,
    #[serde(default)]
    pub error: Option<ErrorObject>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Choice {
    #[serde(default)]
    pub delta: Option<Delta>,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Delta {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub reasoning_content: Option<String>,
    /// #57: OpenRouter's structured reasoning blocks (some encrypted or redacted).
    #[serde(default)]
    pub reasoning_details: Option<Vec<Value>>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallFragment>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ToolCallFragment {
    #[serde(default)]
    pub index: Option<usize>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<FunctionFragment>,
}

#[derive(Debug, Default, Deserialize)]
pub struct FunctionFragment {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ChunkUsage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
    #[serde(default)]
    pub cost: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
pub struct PromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: Option<u64>,
}

/// What the stream loop does after one SSE event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    /// `[DONE]`: stop reading, then [`ReplyAssembler::finish`].
    Done,
    /// An error event was emitted: stop, emit nothing more.
    Stop,
    /// The data wasn't valid JSON: treated like a connection drop (retry from the top).
    Broken,
}

/// The stream wasn't complete: retry the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Incomplete;

/// Turns one response's SSE `data` payloads into [`ProviderEvent`]s. Pure (no I/O), so the
/// stream's edge cases are tested without a server.
#[derive(Debug)]
pub struct ReplyAssembler {
    label: String,
    /// Tool calls arrive in fragments keyed by index; they're only complete at the end.
    pending: BTreeMap<usize, ToolCall>,
    reason: Option<String>,
    /// Whether the server said the reply was complete (`[DONE]` or a `finish_reason`).
    complete: bool,
    /// #57: reasoning blocks in arrival order.
    reasoning: Vec<Value>,
    /// #64: what a dropped stream had already delivered.
    pub text_received: bool,
}

impl ReplyAssembler {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            pending: BTreeMap::new(),
            reason: None,
            complete: false,
            reasoning: Vec::new(),
            text_received: false,
        }
    }

    /// #64: a tool call is mid-stream (a drop now can't be continued).
    pub fn tool_call_in_progress(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Handles one SSE event's data, pushing what it yields onto `out`.
    pub fn on_data(&mut self, data: &str, out: &mut Vec<ProviderEvent>) -> Flow {
        if data == "[DONE]" {
            self.complete = true;
            return Flow::Done;
        }
        let Ok(chunk) = serde_json::from_str::<Chunk>(data) else { return Flow::Broken };
        // Errors can also arrive mid-stream, after the 200 status was already sent.
        if let Some(error) = &chunk.error {
            out.push(ProviderEvent::Error(format!("{}: {}", self.label, describe_error(error))));
            return Flow::Stop;
        }
        if let Some(u) = chunk.usage {
            out.push(ProviderEvent::Usage(Usage {
                prompt_tokens: u.prompt_tokens,
                completion_tokens: u.completion_tokens,
                cached_tokens: u.prompt_tokens_details.and_then(|d| d.cached_tokens),
                cost: u.cost,
            }));
        }
        let Some(choice) = chunk.choices.into_iter().next() else { return Flow::Continue };
        if let Some(reason) = choice.finish_reason {
            self.reason = Some(reason);
            self.complete = true;
        }
        let Some(delta) = choice.delta else { return Flow::Continue };
        if let Some(thinking) = delta.reasoning.or(delta.reasoning_content).filter(|t| !t.is_empty()) {
            out.push(ProviderEvent::ThinkingDelta(thinking));
        }
        // Port note (#57): OpenRouter may split one block over several chunks (same `index`);
        // check whether the blocks must be merged before they're sent back.
        self.reasoning.extend(delta.reasoning_details.unwrap_or_default());
        if let Some(text) = delta.content.filter(|t| !t.is_empty()) {
            self.text_received = true;
            out.push(ProviderEvent::TextDelta(text));
        }
        for fragment in delta.tool_calls {
            let index = fragment.index.unwrap_or(self.pending.len());
            let call = self.pending.entry(index).or_insert_with(|| ToolCall {
                id: String::new(),
                name: String::new(),
                arguments: String::new(),
            });
            let function = fragment.function.unwrap_or_default();
            // The first time a call's name is known, it's worth telling the screen even before any arguments.
            let named = call.name.is_empty() && function.name.as_deref().is_some_and(|n| !n.is_empty());
            if let Some(id) = fragment.id.filter(|id| !id.is_empty()) {
                call.id = id;
            }
            if let Some(name) = function.name.filter(|n| !n.is_empty()) {
                call.name = name;
            }
            let text = function.arguments.unwrap_or_default();
            call.arguments.push_str(&text);
            if !text.is_empty() || named {
                out.push(ProviderEvent::ToolCallDelta { index, name: call.name.clone(), text });
            }
        }
        Flow::Continue
    }

    /// The body ended. Complete: the assembled calls (in index order), the reasoning (#57) and
    /// `Done`. Not complete (no `[DONE]` nor `finish_reason`): [`Incomplete`], retry.
    pub fn finish(self, out: &mut Vec<ProviderEvent>) -> Result<(), Incomplete> {
        if !self.complete {
            return Err(Incomplete);
        }
        for (index, mut call) in self.pending {
            if call.id.is_empty() {
                call.id = format!("call_{index}");
            }
            out.push(ProviderEvent::ToolCall(call));
        }
        if !self.reasoning.is_empty() {
            out.push(ProviderEvent::Reasoning(marv_core::Reasoning(Value::Array(self.reasoning))));
        }
        out.push(ProviderEvent::Done { reason: self.reason });
        Ok(())
    }
}

/// The OpenAI-compatible adapter.
pub struct OpenAiCompatProvider {
    pub options: OpenAiCompatOptions,
    /// Shared connection pool; built with rustls and the timeouts in the module notes.
    pub client: reqwest::Client,
}

impl OpenAiCompatProvider {
    pub fn new(options: OpenAiCompatOptions) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            // Per read, not for the whole reply: keep-alives arrive while the model works.
            .read_timeout(Duration::from_secs(300))
            .build()
            .unwrap_or_default();
        Self { options, client }
    }
}

impl Provider for OpenAiCompatProvider {
    fn name(&self) -> &str {
        &self.options.name
    }

    /// Not known from the stream: the session reads it from the model list (`lookup()`).
    fn context_length(&self) -> Option<u64> {
        None
    }

    fn stream(&self, history: &[ChatTurn], options: StreamOptions) -> EventStream {
        // The API is stateless: every request carries the whole conversation. It's copied now, so
        // the caller can append to its history while the reply streams (and #64 can extend it).
        let outgoing = Outgoing {
            history: history.to_vec(),
            system: options.system,
            tools: options.tools,
            tool_choice: options.tool_choice,
        };
        let (client, adapter, cancel) = (self.client.clone(), self.options.clone(), options.cancel);
        channel_stream(move |emit| run(client, adapter, outgoing, cancel, emit))
    }
}

/// What one stream sends, owned so the returned stream is `'static`.
struct Outgoing {
    history: Vec<ChatTurn>,
    system: Option<String>,
    tools: Vec<ToolSpec>,
    tool_choice: ToolChoice,
}

impl Outgoing {
    /// The body of the next attempt (#64: with the kept partial reply and the note, if any).
    fn body(&mut self, options: &OpenAiCompatOptions, kept: &str) -> Vec<u8> {
        let Self { history, system, tools, tool_choice } = self;
        with_continuation(history, kept, |history| {
            let body = request_body(options, history, system.as_deref(), tools, *tool_choice);
            serde_json::to_vec(&body).expect("a request body is plain JSON")
        })
    }
}

/// Marv #64: runs `f` on the history with, when `kept` isn't empty, the partial reply as an
/// assistant turn and [`DROPPED_STREAM_NOTE`] after it. The history is left as it was.
pub(crate) fn with_continuation<R>(history: &mut Vec<ChatTurn>, kept: &str, f: impl FnOnce(&[ChatTurn]) -> R) -> R {
    if kept.is_empty() {
        return f(history);
    }
    history.push(ChatTurn::Assistant { text: kept.to_string(), tool_calls: Vec::new(), reasoning: None });
    history.push(ChatTurn::User { text: DROPPED_STREAM_NOTE.to_string() });
    let result = f(history);
    history.truncate(history.len() - 2);
    result
}

/// The retry loop: up to [`MAX_ATTEMPTS`] for network errors, drops and 429/502/503/504.
async fn run(client: reqwest::Client, o: OpenAiCompatOptions, mut outgoing: Outgoing, cancel: CancellationToken, emit: Emit) {
    let label = o.label.as_str();
    let url = format!("{}/chat/completions", o.base_url);
    // #64: the partial reply kept from dropped attempts; the next request continues it.
    let mut kept = String::new();
    for attempt in 1.. {
        if cancel.is_cancelled() {
            return;
        }
        if attempt > 1 {
            emit.retry(label, attempt - 1, !kept.is_empty());
        }

        let mut request = client.post(&url).header(reqwest::header::CONTENT_TYPE, "application/json");
        if let Some(key) = &o.api_key {
            request = request.bearer_auth(key);
        }
        for (name, value) in &o.headers {
            request = request.header(name, value);
        }
        let Some(sent) = until_cancelled(&cancel, request.body(outgoing.body(&o, &kept)).send()).await else { return };
        let response = match sent {
            Ok(response) => response,
            Err(err) => {
                if attempt < MAX_ATTEMPTS && is_network_failure(&err) {
                    if sleep_abortable(backoff(o.timing, attempt), &cancel).await {
                        return;
                    }
                    continue;
                }
                emit.send(ProviderEvent::Error(unreachable(label, &o.base_url, &root_cause(&err), o.offline_hint.as_deref())));
                return;
            }
        };

        let status = response.status().as_u16();
        if response.status().is_success() {
            // Stream the SSE body. If the connection drops mid-stream, retry (#64: or continue).
            match read_reply(response, label, &cancel, &emit).await {
                Read::Finished => return,
                Read::Dropped { text, tool_call_in_progress } => {
                    keep_or_restart(&mut kept, &text, tool_call_in_progress);
                    if attempt < MAX_ATTEMPTS {
                        if sleep_abortable(backoff(o.timing, attempt), &cancel).await {
                            return;
                        }
                        continue;
                    }
                    emit.send(ProviderEvent::Error(ended_early(label)));
                    return;
                }
            }
        }

        // HTTP-level error.
        if retriable_http_status(status) && attempt < MAX_ATTEMPTS {
            if sleep_abortable(status_wait(&response, o.timing, attempt), &cancel).await {
                return;
            }
            continue;
        }
        let Some(detail) = read_error(response, &cancel).await else { return };
        emit.send(ProviderEvent::Error(http_error(status, &detail, label, &o.model, o.hints)));
        return;
    }
}

/// How one response's body went.
pub(crate) enum Read {
    /// Complete, or an error event was sent, or cancelled: the stream is over.
    Finished,
    /// It broke off before the reply was complete: retry. `text` is what this attempt streamed.
    Dropped { text: String, tool_call_in_progress: bool },
}

/// Parses the SSE body, sending events as they come.
async fn read_reply(response: reqwest::Response, label: &str, cancel: &CancellationToken, emit: &Emit) -> Read {
    let mut assembler = ReplyAssembler::new(label);
    let mut text = String::new();
    let mut out = Vec::new();
    let mut data = Box::pin(parse_sse(Box::pin(response.bytes_stream())));
    loop {
        let Some(next) = until_cancelled(cancel, data.next()).await else { return Read::Finished };
        let flow = match next {
            None => break,
            // A connection drop mid-stream is retriable: typically a socket closed or reset.
            Some(Err(_)) => Flow::Broken,
            Some(Ok(data)) => assembler.on_data(&data, &mut out),
        };
        emit.forward(&mut out, &mut text);
        match flow {
            Flow::Continue => {}
            Flow::Done => break,
            Flow::Stop => return Read::Finished,
            Flow::Broken => return Read::Dropped { text, tool_call_in_progress: assembler.tool_call_in_progress() },
        }
    }
    let tool_call_in_progress = assembler.tool_call_in_progress();
    // The stream ended cleanly but early (no [DONE] or finish_reason): a proxy closed it, or the
    // server lost the connection. A retry gets a fresh response.
    let finished = assembler.finish(&mut out);
    emit.forward(&mut out, &mut text);
    match finished {
        Ok(()) => Read::Finished,
        Err(Incomplete) => Read::Dropped { text, tool_call_in_progress },
    }
}

// ---- Shared by both adapters' retry loops. ----

/// Marv #64: after a drop, keep the attempt's text for a continue request, or forget everything
/// kept and start over.
pub(crate) fn keep_or_restart(kept: &mut String, text: &str, tool_call_in_progress: bool) {
    match drop_recovery(!kept.is_empty() || !text.is_empty(), tool_call_in_progress) {
        DropRecovery::Continue => kept.push_str(text),
        DropRecovery::Restart => kept.clear(),
    }
}

/// After the last attempt broke off too.
pub(crate) fn ended_early(label: &str) -> String {
    format!("{label}: the stream ended early, before the reply was complete. Try again.")
}

/// The wait before retry number `retry` (1-based): its backoff, with jitter (#64).
pub(crate) fn backoff(timing: RetryTiming, retry: u32) -> Duration {
    let seconds = BACKOFFS[(retry as usize - 1).min(BACKOFFS.len() - 1)];
    jittered(timing.wait(seconds), random_fraction())
}

/// `wait` lengthened by `fraction` (0 to 1) of [`JITTER`].
pub fn jittered(wait: Duration, fraction: f64) -> Duration {
    wait.mul_f64(1.0 + JITTER * fraction.clamp(0.0, 1.0))
}

/// A number in [0, 1) from std's randomly keyed hasher (no RNG crate for one number per retry).
fn random_fraction() -> f64 {
    use std::hash::BuildHasher;
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let bits = std::collections::hash_map::RandomState::new().hash_one(nanos);
    (bits >> 11) as f64 / (1u64 << 53) as f64
}

/// The wait before retrying a retriable status: `Retry-After` when it's whole seconds, else the backoff.
pub(crate) fn status_wait(response: &reqwest::Response, timing: RetryTiming, retry: u32) -> Duration {
    let retry_after = response.headers().get(reqwest::header::RETRY_AFTER).and_then(|v| v.to_str().ok());
    parse_retry_after(retry_after).map_or_else(|| backoff(timing, retry), |seconds| timing.wait(seconds))
}

/// The message in an error response's body. `None`: cancelled while reading it.
pub(crate) async fn read_error(response: reqwest::Response, cancel: &CancellationToken) -> Option<String> {
    let content_type = response.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(str::to_string);
    let status_text = response.status().canonical_reason().unwrap_or_default();
    let body = until_cancelled(cancel, response.text()).await?.unwrap_or_default();
    Some(error_message(content_type.as_deref(), status_text, &body))
}

/// Whether a failed request is worth retrying: a connection or timeout error, or one whose
/// wording says so (hyper's differs from Bun's, so the whole chain is checked).
pub(crate) fn is_network_failure(err: &reqwest::Error) -> bool {
    err.is_connect() || err.is_timeout() || is_network_error(&error_chain(err))
}

/// Every message in an error's chain, joined with ": ".
fn error_chain(err: &dyn std::error::Error) -> String {
    let mut messages = vec![err.to_string()];
    let mut source = err.source();
    while let Some(inner) = source {
        messages.push(inner.to_string());
        source = inner.source();
    }
    messages.join(": ")
}

/// The innermost message ("Connection refused (os error 111)"): reqwest's own repeats the URL.
pub(crate) fn root_cause(err: &dyn std::error::Error) -> String {
    let mut cause = err;
    while let Some(inner) = cause.source() {
        cause = inner;
    }
    cause.to_string()
}

/// Runs `future` unless `cancel` fires first (`None`).
pub(crate) async fn until_cancelled<T>(cancel: &CancellationToken, future: impl Future<Output = T>) -> Option<T> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => None,
        value = future => Some(value),
    }
}

/// Where a running stream sends its events.
pub(crate) struct Emit(futures::channel::mpsc::UnboundedSender<ProviderEvent>);

impl Emit {
    pub(crate) fn send(&self, event: ProviderEvent) {
        // The receiver is gone only when the stream was dropped, and then this future is too.
        let _ = self.0.unbounded_send(event);
    }

    /// Sends the events in `out`, adding their text to `text` (#64: what a drop would keep).
    pub(crate) fn forward(&self, out: &mut Vec<ProviderEvent>, text: &mut String) {
        for event in out.drain(..) {
            if let ProviderEvent::TextDelta(t) = &event {
                text.push_str(t);
            }
            self.send(event);
        }
    }

    /// The `Retry` event before a new attempt (`retry`/`MAX_ATTEMPTS - 1`).
    pub(crate) fn retry(&self, label: &str, retry: u32, continued: bool) {
        let max = MAX_ATTEMPTS - 1;
        self.send(ProviderEvent::Retry {
            attempt: retry,
            max,
            message: format!("{label}: connection lost, retrying ({retry}/{max})…"),
            continued,
        });
    }
}

/// A `'static` event stream fed by `produce`. The producer runs inside the stream (no spawned
/// task): dropping the stream drops the request with it.
pub(crate) fn channel_stream<F, Fut>(produce: F) -> EventStream
where
    F: FnOnce(Emit) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (tx, rx) = futures::channel::mpsc::unbounded();
    let producer = produce(Emit(tx)).into_stream().filter_map(|()| futures::future::ready(None));
    Box::pin(futures::stream::select(rx, producer))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str, args: &str) -> ToolCall {
        ToolCall { id: id.into(), name: "read_file".into(), arguments: args.into() }
    }

    fn history() -> Vec<ChatTurn> {
        vec![
            ChatTurn::User { text: "hi".into() },
            ChatTurn::Assistant { text: "".into(), tool_calls: vec![call("c1", "{\"path\": \"a.ts\"}")], reasoning: None },
            ChatTurn::Tool { call_id: "c1".into(), name: "read_file".into(), text: "contents".into() },
        ]
    }

    fn json(messages: &[WireMessage]) -> String {
        serde_json::to_string(messages).unwrap()
    }

    // From tests/openai-compat.test.ts: "sends tools, and tool calls and results in OpenAI's format".
    #[test]
    fn sends_history_in_openai_format_with_raw_arguments() {
        assert_eq!(
            json(&to_messages(&history(), Some("SYS"), false)),
            r#"[{"role":"system","content":"SYS"},{"role":"user","content":"hi"},{"role":"assistant","content":"","tool_calls":[{"id":"c1","type":"function","function":{"name":"read_file","arguments":"{\"path\": \"a.ts\"}"}}]},{"role":"tool","tool_call_id":"c1","content":"contents"}]"#
        );
    }

    #[test]
    fn request_body_has_usage_tools_and_extra_fields_and_tool_choice_only_when_none() {
        let mut options = OpenAiCompatOptions { model: "acme/m".into(), ..Default::default() };
        options.body.insert("reasoning".into(), serde_json::json!({"enabled": true}));
        let tools = vec![ToolSpec { name: "t".into(), description: "d".into(), parameters: serde_json::json!({}) }];
        let auto = serde_json::to_string(&request_body(&options, &[], None, &tools, ToolChoice::Auto)).unwrap();
        assert_eq!(
            auto,
            r#"{"model":"acme/m","stream":true,"stream_options":{"include_usage":true},"messages":[],"tools":[{"type":"function","function":{"name":"t","description":"d","parameters":{}}}],"reasoning":{"enabled":true}}"#
        );
        let none = serde_json::to_string(&request_body(&options, &[], None, &tools, ToolChoice::None)).unwrap();
        assert!(none.contains(r#""tool_choice":"none""#));
        let no_tools = serde_json::to_string(&request_body(&options, &[], None, &[], ToolChoice::Auto)).unwrap();
        assert!(!no_tools.contains("tools"));
    }

    #[test]
    fn cache_breakpoints_only_for_models_that_need_them() {
        assert!(needs_cache_breakpoints("anthropic/claude-sonnet-5.5"));
        assert!(needs_cache_breakpoints("google/gemini-3-pro"));
        assert!(!needs_cache_breakpoints("z-ai/glm-5.3"));
    }

    #[test]
    fn breakpoints_mark_system_and_tail_and_keep_earlier_bytes() {
        let marked = to_messages(&history(), Some("SYS"), true);
        let s = json(&marked);
        assert!(
            s.starts_with(r#"[{"role":"system","content":[{"type":"text","text":"SYS","cache_control":{"type":"ephemeral"}}]}"#)
        );
        assert!(s.ends_with(r#"{"role":"tool","tool_call_id":"c1","content":[{"type":"text","text":"contents","cache_control":{"type":"ephemeral"}}]}]"#));
        // Everything between the breakpoints is byte-identical to the unmarked request.
        let plain = to_messages(&history(), Some("SYS"), false);
        assert_eq!(json(&marked[1..3]), json(&plain[1..3]));
    }

    #[test]
    fn reasoning_is_sent_back_on_the_assistant_message() {
        let turn = ChatTurn::Assistant {
            text: "ok".into(),
            tool_calls: vec![],
            reasoning: Some(marv_core::Reasoning(serde_json::json!([{"data": "xyz"}]))),
        };
        assert_eq!(
            json(&to_messages(&[turn], None, false)),
            r#"[{"role":"assistant","content":"ok","reasoning_details":[{"data":"xyz"}]}]"#
        );
    }

    fn run(chunks: &[&str]) -> (Vec<ProviderEvent>, Result<(), Incomplete>) {
        let mut assembler = ReplyAssembler::new("TestRouter");
        let mut out = Vec::new();
        for data in chunks {
            match assembler.on_data(data, &mut out) {
                Flow::Continue => {}
                Flow::Done => break,
                Flow::Stop => return (out, Ok(())),
                Flow::Broken => return (out, Err(Incomplete)),
            }
        }
        let result = assembler.finish(&mut out);
        (out, result)
    }

    #[test]
    fn streams_text_deltas_and_ends_with_done() {
        let (events, result) = run(&[
            r#"{"choices":[{"delta":{"content":"Hel"}}]}"#,
            r#"{"choices":[{"delta":{"content":"lo"},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ]);
        assert_eq!(result, Ok(()));
        assert_eq!(
            events,
            vec![
                ProviderEvent::TextDelta("Hel".into()),
                ProviderEvent::TextDelta("lo".into()),
                ProviderEvent::Done { reason: Some("stop".into()) }
            ]
        );
    }

    #[test]
    fn streams_reasoning_as_thinking_deltas_from_either_field_name() {
        let (events, _) = run(&[
            r#"{"choices":[{"delta":{"reasoning":"a"}}]}"#,
            r#"{"choices":[{"delta":{"reasoning_content":"b"}}]}"#,
            "[DONE]",
        ]);
        assert_eq!(events[..2], [ProviderEvent::ThinkingDelta("a".into()), ProviderEvent::ThinkingDelta("b".into())]);
    }

    #[test]
    fn reassembles_tool_calls_streamed_in_fragments() {
        let (events, _) = run(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"read_file","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"name":"glob","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.ts\"}"}}]},"finish_reason":"tool_calls"}]}"#,
        ]);
        let calls: Vec<_> =
            events.iter().filter_map(|e| if let ProviderEvent::ToolCall(c) = e { Some(c.clone()) } else { None }).collect();
        assert_eq!(
            calls,
            vec![call("c1", r#"{"path":"a.ts"}"#), ToolCall { id: "call_1".into(), name: "glob".into(), arguments: "{}".into() }]
        );
        // The name alone is shown at once; then each argument fragment.
        assert_eq!(events[0], ProviderEvent::ToolCallDelta { index: 0, name: "read_file".into(), text: "".into() });
        assert_eq!(events.last(), Some(&ProviderEvent::Done { reason: Some("tool_calls".into()) }));
    }

    #[test]
    fn usage_with_cached_tokens_and_cost() {
        let (events, _) = run(&[
            r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":80},"cost":0.002}}"#,
            "[DONE]",
        ]);
        assert_eq!(
            events[0],
            ProviderEvent::Usage(Usage { prompt_tokens: 100, completion_tokens: 5, cached_tokens: Some(80), cost: Some(0.002) })
        );
    }

    #[test]
    fn reports_an_error_sent_in_the_middle_of_the_stream() {
        let (events, _) = run(&[
            r#"{"choices":[{"delta":{"content":"partial"}}]}"#,
            r#"{"error":{"message":"Provider returned error","metadata":{"provider_name":"Anthropic","raw":"tool_use ids must be unique"}}}"#,
        ]);
        assert_eq!(
            events.last(),
            Some(&ProviderEvent::Error("TestRouter: Provider returned error: Anthropic: tool_use ids must be unique".into()))
        );
    }

    #[test]
    fn a_stream_that_ends_before_the_reply_is_complete_needs_a_retry() {
        let (_, result) = run(&[r#"{"choices":[{"delta":{"content":"Writing the fi"}}]}"#]);
        assert_eq!(result, Err(Incomplete));
    }

    #[test]
    fn reasoning_details_are_kept_for_the_assistant_turn() {
        let (events, _) =
            run(&[r#"{"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.text","text":"hm"}]}}]}"#, "[DONE]"]);
        assert!(matches!(&events[0], ProviderEvent::Reasoning(r) if r.0[0]["text"] == "hm"));
    }
}
