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
//! - **#90 how a request ended**: the usage chunk is held and sent with the finish reason in
//!   [`ReplyAssembler::finish`] (the finish-reason chunk often arrives after the usage chunk),
//!   as [`marv_core::ReplyEnd`]; the trajectory's `request` record carries both. The raw
//!   response payloads are collected only when `StreamOptions::debug_raw` was on (`MARV_DEBUG_RAW`).

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use futures::{FutureExt, StreamExt};
use marv_core::{ChatTurn, ProviderEvent, ReplyEnd, RetryCause, ToolCall, ToolChoice, ToolSpec, Usage};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::degenerate::{DEGENERATE_NOTE, DegenerateGuard};
use crate::errors::{ErrorHints, ErrorObject, describe_error, error_message, http_error, plan_limit, plan_limit_of, unreachable};
use crate::retry::{
    BACKOFFS, BodyError, DropRecovery, FailureKind, MAX_ATTEMPTS, RetryTiming, drop_recovery, idle_guard, parse_retry_after,
    retriable_failure, retriable_http_status, sleep_abortable,
};
use crate::sse::parse_sse;
use crate::{EventStream, Provider, StreamOptions};

/// Marv #64: sent after the partial reply kept from a dropped stream. The same text as
/// `marv_agent::DROPPED_STREAM_NOTE` (marv-agent depends on this crate, so it can't be imported).
pub const DROPPED_STREAM_NOTE: &str = "(Automatic note from Marv, the program running you) The previous response was interrupted. Continue from where you left off.";

/// Marv #64: the most a backoff wait is lengthened at random (a share of it).
pub const JITTER: f64 = 0.25;

/// Marv #88: how long a stream may send nothing at all (not even a keep-alive) before it counts as stalled and is
/// retried like a dropped connection. OpenRouter sends keep-alive comments while the model works, so a provider that
/// is slow but alive isn't cut off; a stalled one used to hold the turn for the whole 300 s read timeout.
pub const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(75);

/// Marv #96: the `max_tokens` sent when the model list doesn't say what the model can do (and the most sent when it
/// does). Without one, a runaway reply only ends when the provider gives up; GLM streamed newlines for six minutes.
/// opencode sends the same cap.
pub const DEFAULT_MAX_TOKENS: u64 = 32_000;

/// #96: the `max_tokens` for a model whose list entry says it can write at most `max_output` tokens per reply.
pub fn max_tokens_for(max_output: Option<u64>) -> u64 {
    max_output.map_or(DEFAULT_MAX_TOKENS, |n| n.min(DEFAULT_MAX_TOKENS))
}

/// #96: how a provider says it won't take the `max_tokens` Marv sent: it names the field, or (OpenRouter) the prompt
/// plus the requested output don't fit the window ("…, 32000 in the output").
static MAX_TOKENS_REFUSED: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)max_tokens|max_completion_tokens|max output tokens|in the output").unwrap());

/// Names the field and says it isn't supported at all: later requests leave it out too. Any other refusal ("max_tokens
/// exceeds the remaining context") drops it for that request only.
static NAMES_MAX_TOKENS: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)max_tokens|max_completion_tokens").unwrap());
static UNSUPPORTED: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?i)unsupported|not supported|unknown (parameter|field|argument)|unrecognized|extra inputs are not permitted|not allowed")
        .unwrap()
});

/// #116: the least `max_tokens` a clamped request is sent with: below this a model can't finish even a short answer.
/// When the room left is smaller, the request goes without `max_tokens` (a cap that small would only be refused).
pub const MIN_MAX_TOKENS: u64 = 1024;
/// #116: tokens per byte of request body assumed until a reply reports its prompt count: 4 characters a token, the
/// estimate compaction uses. JSON escaping makes the body longer than the text, so it errs toward a smaller cap.
pub const ASSUMED_TOKENS_PER_BYTE: f64 = 1.0 / marv_core::CHARS_PER_TOKEN as f64;

/// #116: OpenRouter's refusal when the prompt plus `max_tokens` don't fit: "maximum context length is 131072 tokens.
/// However, you requested about 140000 tokens (108000 of text input, 32000 in the output)".
static WINDOW_REFUSAL: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?is)context length is (\d+) tokens.*?requested about (\d+) tokens.*?(\d+) in the output").unwrap()
});

/// #116: what a refusal like [`WINDOW_REFUSAL`] says: the window, and how many tokens the prompt was.
pub fn window_refusal(detail: &str) -> Option<(u64, u64)> {
    let caps = WINDOW_REFUSAL.captures(detail)?;
    let number = |i: usize| caps.get(i)?.as_str().parse::<u64>().ok();
    let (window, total, output) = (number(1)?, number(2)?, number(3)?);
    Some((window, total.checked_sub(output).filter(|&input| input > 0)?))
}

/// #116: what the adapter has learned about the room in the window, shared by every stream of this provider (the
/// session and its subagents use the same one), so one refusal lowers the cap for the rest of the session.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Room {
    /// The context window: from the model list, or from a refusal that said it.
    pub window: Option<u64>,
    /// The most prompt tokens per byte of request body measured so far (a reply's `prompt_tokens`, or a refusal's
    /// "N of text input", over the body's size). The highest: a cached prefix never lowers it.
    pub tokens_per_byte: Option<f64>,
}

impl Room {
    /// Records a prompt count for a body of `bytes` bytes.
    pub fn record(&mut self, prompt_tokens: u64, bytes: usize) {
        if prompt_tokens > 0 && bytes > 0 {
            let ratio = prompt_tokens as f64 / bytes as f64;
            self.tokens_per_byte = Some(self.tokens_per_byte.map_or(ratio, |r| r.max(ratio)));
        }
    }

    /// The `max_tokens` for a body of `bytes` bytes, given the model's limit `max`: at most the window minus the
    /// prompt's estimate (+5% headroom). `None` (send none) when that room is under [`MIN_MAX_TOKENS`] (or `max`, if
    /// smaller): such a cap wouldn't fit either, and a refusal costs a round trip. `max` when the window isn't known.
    pub fn cap(&self, max: u64, bytes: usize) -> Option<u64> {
        let Some(window) = self.window.filter(|&w| w > 0) else { return Some(max) };
        let ratio = self.tokens_per_byte.unwrap_or(ASSUMED_TOKENS_PER_BYTE);
        let prompt = (bytes as f64 * ratio * crate::ollama::PROMPT_MARGIN).ceil() as u64;
        let room = window.saturating_sub(prompt);
        (room >= MIN_MAX_TOKENS.min(max)).then(|| max.min(room))
    }
}

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
    /// #88: a stream silent for this long is retried ([`STREAM_IDLE_TIMEOUT`] for OpenRouter). `None`: only the
    /// 300 s read timeout applies.
    pub idle_timeout: Option<Duration>,
    /// #96: sent as `max_tokens`, so the provider ends a runaway reply ([`max_tokens_for`] for OpenRouter). `None`
    /// sends nothing. #116: clamped per request to the room the prompt leaves ([`Room::cap`]). A provider that refuses
    /// it gets the request again with what its refusal says is left ([`window_refusal`]), or without it.
    pub max_tokens: Option<u64>,
    /// #116: the model's context window (the model list's `context_length`), so `max_tokens` is clamped to the room
    /// the prompt leaves. `None`: learned from the first refusal that says it.
    pub context_length: Option<u64>,
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
    /// #96: the most the reply may be. Not part of the prompt, so it doesn't touch the cache.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
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
        max_tokens: options.max_tokens,
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
    /// The usage chunk, sent with the finish reason in [`Self::finish`] (#90): the usage chunk
    /// often arrives before the finish_reason chunk, and the record wants both together.
    usage: Option<Usage>,
    /// #90: every `data` payload of this attempt, when debug recording is on. The last
    /// attempt's payload list is "the raw last response" — a retried attempt starts fresh.
    raw: Option<Vec<String>>,
    /// #64: what a dropped stream had already delivered.
    pub text_received: bool,
    /// An error the provider sent mid-stream that a fresh attempt may get past (`errors::transient`): retried like a
    /// drop, and reported in full if every attempt ends this way.
    pub upstream: Option<String>,
    /// #96: readable reasoning that came only in `reasoning_details` blocks (no plain `reasoning` in the chunk), for
    /// the degenerate guard. The blocks themselves are kept as they are.
    pub detail_text: String,
}

impl ReplyAssembler {
    pub fn new(label: impl Into<String>) -> Self {
        Self::with_debug(label, false)
    }

    /// `debug_raw` (#90): also collect the raw response payloads (sent in the Usage event).
    pub fn with_debug(label: impl Into<String>, debug_raw: bool) -> Self {
        Self {
            label: label.into(),
            pending: BTreeMap::new(),
            reason: None,
            complete: false,
            reasoning: Vec::new(),
            usage: None,
            raw: debug_raw.then(Vec::new),
            text_received: false,
            upstream: None,
            detail_text: String::new(),
        }
    }

    /// #64: a tool call is mid-stream (a drop now can't be continued).
    pub fn tool_call_in_progress(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Handles one SSE event's data, pushing what it yields onto `out`.
    pub fn on_data(&mut self, data: &str, out: &mut Vec<ProviderEvent>) -> Flow {
        // #90: the payload itself is the raw response when debug recording is on (even an
        // unparsable one: that's exactly what the recording is for).
        if let Some(raw) = &mut self.raw {
            raw.push(data.to_string());
        }
        if data == "[DONE]" {
            self.complete = true;
            return Flow::Done;
        }
        let Ok(chunk) = serde_json::from_str::<Chunk>(data) else { return Flow::Broken };
        // Errors can also arrive mid-stream, after the 200 status was already sent. One the provider behind the router
        // may get past on a fresh attempt (an upstream abort, timeout or overload) is retried like a dropped connection.
        if let Some(error) = &chunk.error {
            // #159: a plan's limit, mid-stream: a hard stop with its own message.
            if let Some(message) = plan_limit_of(&self.label, error.code.as_ref(), error.message.as_deref()) {
                out.push(ProviderEvent::Error(message));
                return Flow::Stop;
            }
            let message = format!("{}: {}", self.label, describe_error(error));
            if crate::errors::transient(error) {
                // The reply already finished (a finish_reason came): nothing is lost, so don't resend it.
                if self.complete {
                    return Flow::Done;
                }
                self.upstream = Some(message);
                return Flow::Broken;
            }
            out.push(ProviderEvent::Error(message));
            return Flow::Stop;
        }
        if let Some(u) = chunk.usage {
            // Held for finish(): the finish reason may still arrive in a later chunk.
            self.usage = Some(Usage {
                prompt_tokens: u.prompt_tokens,
                completion_tokens: u.completion_tokens,
                cached_tokens: u.prompt_tokens_details.and_then(|d| d.cached_tokens),
                cost: u.cost,
            });
        }
        let Some(choice) = chunk.choices.into_iter().next() else { return Flow::Continue };
        if let Some(reason) = choice.finish_reason {
            self.reason = Some(reason);
            self.complete = true;
        }
        let Some(delta) = choice.delta else { return Flow::Continue };
        let thinking = delta.reasoning.or(delta.reasoning_content).filter(|t| !t.is_empty());
        if thinking.is_none() {
            // #96: a model that reasons only in blocks would otherwise get past the degenerate guard.
            for block in delta.reasoning_details.iter().flatten() {
                for field in ["text", "summary"] {
                    if let Some(t) = block.get(field).and_then(Value::as_str) {
                        self.detail_text.push_str(t);
                    }
                }
            }
        }
        if let Some(thinking) = thinking {
            out.push(ProviderEvent::ThinkingDelta(thinking));
        }
        // OpenRouter streams one block over many chunks (same `index` and `type`, often a token each):
        // merged here, so the session keeps and sends back one block, not one per chunk.
        for block in delta.reasoning_details.unwrap_or_default() {
            merge_reasoning_block(&mut self.reasoning, block);
        }
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

    /// The body ended. Complete: the assembled calls (in index order), the reasoning (#57),
    /// the usage with how the reply ended (#90), and `Done`. Not complete (no `[DONE]` nor
    /// `finish_reason`): [`Incomplete`], retry.
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
        // Only with a usage chunk (no phantom requests for servers that don't report usage:
        // the loop counts steps by this event). Raw recording rides on it; a server that
        // sends no usage gives the run no request record, as before.
        if let Some(usage) = self.usage {
            out.push(ProviderEvent::Usage(
                usage,
                ReplyEnd { finish_reason: self.reason.clone(), raw_response: self.raw.as_ref().map(|raw| raw.join("\n")) },
            ));
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
    /// #96: the provider said it doesn't take `max_tokens`: later streams leave it out.
    max_tokens_refused: Arc<AtomicBool>,
    /// #116: the room in the window, learned from replies and refusals.
    room: Arc<Mutex<Room>>,
}

impl OpenAiCompatProvider {
    pub fn new(options: OpenAiCompatOptions) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            // Per read, not for the whole reply: keep-alives arrive while the model works.
            .read_timeout(Duration::from_secs(300))
            .build()
            .unwrap_or_default();
        let room = Room { window: options.context_length, tokens_per_byte: None };
        Self { options, client, max_tokens_refused: Arc::default(), room: Arc::new(Mutex::new(room)) }
    }

    /// #116: what the provider has learned about the room in the window.
    pub fn room(&self) -> Room {
        *self.room.lock().unwrap_or_else(|e| e.into_inner())
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
            note: None,
        };
        let debug_raw = options.debug_raw;
        let (client, mut adapter, cancel) = (self.client.clone(), self.options.clone(), options.cancel);
        if self.max_tokens_refused.load(Ordering::Relaxed) {
            adapter.max_tokens = None;
        }
        let shared = Shared { refused: self.max_tokens_refused.clone(), room: self.room.clone() };
        channel_stream(move |emit| run(client, adapter, outgoing, shared, cancel, debug_raw, emit))
    }
}

/// What one stream sends, owned so the returned stream is `'static`.
struct Outgoing {
    history: Vec<ChatTurn>,
    system: Option<String>,
    tools: Vec<ToolSpec>,
    tool_choice: ToolChoice,
    /// #96: sent after the history once a reply degenerated.
    note: Option<&'static str>,
}

impl Outgoing {
    /// The body of the next attempt (#64: with the kept partial reply and the note, if any). #116: `max_tokens` is
    /// clamped to the room `room` says the prompt leaves (the body is built again only when that changes it). Also
    /// returns the `max_tokens` sent.
    fn body(&mut self, options: &OpenAiCompatOptions, room: Room, kept: &str) -> (Vec<u8>, Option<u64>) {
        let Self { history, system, tools, tool_choice, note } = self;
        with_continuation(history, *note, kept, |history| {
            let mut request = request_body(options, history, system.as_deref(), tools, *tool_choice);
            let body = serde_json::to_vec(&request).expect("a request body is plain JSON");
            let Some(max) = options.max_tokens else { return (body, None) };
            let cap = room.cap(max, body.len());
            if cap == Some(max) {
                return (body, cap);
            }
            request.max_tokens = cap;
            (serde_json::to_vec(&request).expect("a request body is plain JSON"), cap)
        })
    }
}

/// What every stream of one provider shares.
struct Shared {
    /// #96: the provider doesn't take `max_tokens` at all.
    refused: Arc<AtomicBool>,
    /// #116: the room in the window.
    room: Arc<Mutex<Room>>,
}

impl Shared {
    fn room(&self) -> Room {
        *self.room.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn update(&self, f: impl FnOnce(&mut Room)) {
        f(&mut self.room.lock().unwrap_or_else(|e| e.into_inner()));
    }
}

/// Marv #64: runs `f` on the history with, when `kept` isn't empty, the partial reply as an
/// assistant turn and [`DROPPED_STREAM_NOTE`] after it. The history is left as it was.
/// #96: `note` (after a degenerate reply) goes right after the history, as a user turn.
pub(crate) fn with_continuation<R>(
    history: &mut Vec<ChatTurn>,
    note: Option<&str>,
    kept: &str,
    f: impl FnOnce(&[ChatTurn]) -> R,
) -> R {
    let len = history.len();
    if let Some(note) = note {
        history.push(ChatTurn::User { text: note.to_string() });
    }
    if !kept.is_empty() {
        history.push(ChatTurn::Assistant { text: kept.to_string(), tool_calls: Vec::new(), reasoning: None });
        history.push(ChatTurn::User { text: DROPPED_STREAM_NOTE.to_string() });
    }
    let result = f(history);
    history.truncate(len);
    result
}

/// #96: after a reply degenerated, retry once (`true`: the caller sends the next attempt at once) or end the stream.
/// Either way what the attempt streamed is thrown away: the retry starts over, with [`DEGENERATE_NOTE`]; the second
/// time a `Discard` goes before the error.
pub(crate) fn after_degenerate(
    label: &str,
    discarded: u64,
    degenerate_retries: &mut u32,
    kept: &mut String,
    note: &mut Option<&'static str>,
    emit: &Emit,
) -> bool {
    if *degenerate_retries >= 1 {
        // Thrown away like the first one, so none of it ends up in the history or the trajectory.
        kept.clear();
        emit.send(ProviderEvent::Discard { cause: RetryCause::Degenerate { discarded } });
        emit.send(ProviderEvent::Error(format!(
            "{label}: the model's reply degenerated again (whitespace, or the same short piece over and over), so Marv \
             stopped it. Try again, or switch to another model."
        )));
        return false;
    }
    *degenerate_retries += 1;
    kept.clear();
    *note = Some(DEGENERATE_NOTE);
    let kb = discarded.div_ceil(1024);
    emit.send(ProviderEvent::Retry {
        attempt: 1,
        max: 1,
        message: format!("{label}: the reply degenerated into repeated output ({kb} KB thrown away), asking again…"),
        continued: false,
        cause: RetryCause::Degenerate { discarded },
    });
    true
}

/// The retry loop: up to [`MAX_ATTEMPTS`] for network errors, drops, stalls and 429/502/503/504; once more after a
/// degenerate reply (#96); once with a fitted `max_tokens` when the provider says the cap doesn't fit (#116), and once
/// without it when the provider refuses it.
async fn run(
    client: reqwest::Client,
    mut o: OpenAiCompatOptions,
    mut outgoing: Outgoing,
    shared: Shared,
    cancel: CancellationToken,
    debug_raw: bool,
    emit: Emit,
) {
    let label = o.label.clone();
    let label = label.as_str();
    let url = format!("{}/chat/completions", o.base_url);
    // #64: the partial reply kept from dropped attempts; the next request continues it.
    let mut kept = String::new();
    // Why the next attempt is a retry (#88: a stall is recorded as one).
    let mut cause = RetryCause::Dropped;
    // The attempt number for connection retries; a degenerate retry or one without max_tokens doesn't count.
    let mut attempt = 0;
    let mut announce = false;
    let mut degenerate_retries = 0;
    // #116: a refusal that said how big the prompt was is resent once with a cap that fits; after that, without one.
    let mut refit = false;
    loop {
        if cancel.is_cancelled() {
            return;
        }
        attempt += 1;
        if announce {
            emit.retry(label, attempt - 1, !kept.is_empty(), std::mem::take(&mut cause));
        }
        announce = true;

        let mut request = client.post(&url).header(reqwest::header::CONTENT_TYPE, "application/json");
        if let Some(key) = &o.api_key {
            request = request.bearer_auth(key);
        }
        for (name, value) in &o.headers {
            request = request.header(name, value);
        }
        let (body, sent_cap) = outgoing.body(&o, shared.room(), &kept);
        let bytes = body.len();
        let Some(sent) = until_cancelled(&cancel, request.body(body).send()).await else { return };
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
            let mut prompt_tokens = None;
            let read = read_reply(response, label, o.idle_timeout, &cancel, debug_raw, &emit, &mut prompt_tokens).await;
            // #116: the measured prompt size, so the next request's cap fits beside it.
            if let Some(tokens) = prompt_tokens {
                shared.update(|room| room.record(tokens, bytes));
            }
            match read {
                Read::Finished => return,
                Read::Degenerate { discarded } => {
                    if !after_degenerate(label, discarded, &mut degenerate_retries, &mut kept, &mut outgoing.note, &emit) {
                        return;
                    }
                    (attempt, announce) = (attempt - 1, false);
                    continue;
                }
                Read::Dropped { text, tool_call_in_progress, cause: dropped } => {
                    keep_or_restart(&mut kept, &text, tool_call_in_progress);
                    if matches!(dropped, DropCause::Stalled(_)) {
                        cause = RetryCause::Stalled;
                    }
                    if attempt < MAX_ATTEMPTS {
                        if sleep_abortable(backoff(o.timing, attempt), &cancel).await {
                            return;
                        }
                        continue;
                    }
                    emit.send(ProviderEvent::Error(dropped.final_message(label)));
                    return;
                }
            }
        }

        // HTTP-level error. #159: the body is read first: a plan's limit (a 429) is a hard stop, never retried, and
        // never answered with another key or provider.
        let wait = status_wait(&response, o.timing, attempt);
        let Some((detail, body)) = read_error_body(response, &cancel).await else { return };
        if let Some(message) = plan_limit(label, &body) {
            emit.send(ProviderEvent::Error(message));
            return;
        }
        if retriable_http_status(status) && attempt < MAX_ATTEMPTS {
            if sleep_abortable(wait, &cancel).await {
                return;
            }
            continue;
        }
        // #96: a provider that won't take max_tokens (or can't fit it beside this prompt) gets the request without it.
        if (400..500).contains(&status) && sent_cap.is_some() && MAX_TOKENS_REFUSED.is_match(&detail) {
            if NAMES_MAX_TOKENS.is_match(&detail) && UNSUPPORTED.is_match(&detail) {
                shared.refused.store(true, Ordering::Relaxed);
            }
            // #116: the refusal said the window and the prompt's size: remember both (for every later request too)
            // and resend once with what's left, rather than without a cap.
            if let Some((window, input)) = window_refusal(&detail).filter(|_| !refit) {
                shared.update(|room| {
                    room.window = Some(window);
                    room.record(input, bytes);
                });
                refit = true;
                // Never the cap that was just refused again (a cap of None leaves it out at once).
                if o.max_tokens.and_then(|max| shared.room().cap(max, bytes)) == sent_cap {
                    o.max_tokens = None;
                }
            } else {
                o.max_tokens = None;
            }
            (attempt, announce) = (attempt - 1, false);
            continue;
        }
        emit.send(ProviderEvent::Error(http_error(status, &detail, label, &o.model, o.hints)));
        return;
    }
}

/// How one response's body went.
pub(crate) enum Read {
    /// Complete, or an error event was sent, or cancelled: the stream is over.
    Finished,
    /// It broke off before the reply was complete: retry. `text` is what this attempt streamed.
    Dropped { text: String, tool_call_in_progress: bool, cause: DropCause },
    /// #96: the reply degenerated and the request was aborted; `discarded` bytes of text and reasoning are thrown away.
    Degenerate { discarded: u64 },
}

/// Why a stream broke off: what the error says when no attempt gets past it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DropCause {
    /// The connection dropped, or the stream ended (or broke) before the reply was complete.
    Dropped,
    /// The provider sent an error mid-stream that a fresh attempt may get past (the full message).
    Upstream(String),
    /// #88: nothing at all arrived for this long.
    Stalled(Duration),
}

impl DropCause {
    /// The error after the last attempt broke off too.
    pub(crate) fn final_message(&self, label: &str) -> String {
        match self {
            Self::Dropped => ended_early(label),
            Self::Upstream(message) => message.clone(),
            Self::Stalled(idle) => {
                let idle = if idle.as_secs() > 0 { format!("{} s", idle.as_secs()) } else { format!("{} ms", idle.as_millis()) };
                format!("{label}: the stream stalled (nothing arrived for {idle}), on every attempt. Try again.")
            }
        }
    }
}

/// Parses the SSE body, sending events as they come. `idle` (#88): the longest silence allowed between chunks.
async fn read_reply(
    response: reqwest::Response,
    label: &str,
    idle: Option<Duration>,
    cancel: &CancellationToken,
    debug_raw: bool,
    emit: &Emit,
    prompt_tokens: &mut Option<u64>,
) -> Read {
    let mut assembler = ReplyAssembler::with_debug(label, debug_raw);
    let mut guard = DegenerateGuard::default();
    let mut text = String::new();
    let mut out = Vec::new();
    let mut data = Box::pin(parse_sse(idle_guard(response.bytes_stream(), idle)));
    loop {
        let Some(next) = until_cancelled(cancel, data.next()).await else { return Read::Finished };
        let flow = match next {
            None => break,
            // #88: silent for too long. Retried like a drop: the partial reply is kept or continued (#64).
            Some(Err(BodyError::Stalled(idle))) => {
                return Read::Dropped {
                    text,
                    tool_call_in_progress: assembler.tool_call_in_progress(),
                    cause: DropCause::Stalled(idle),
                };
            }
            // A connection drop mid-stream is retriable: typically a socket closed or reset.
            Some(Err(BodyError::Transport(_))) => Flow::Broken,
            Some(Ok(data)) => assembler.on_data(&data, &mut out),
        };
        // #96: a reply that turned into whitespace or one repeated piece is aborted (dropping the response closes it).
        let details = std::mem::take(&mut assembler.detail_text);
        if out.iter().any(|event| guard.check(event)) || (!details.is_empty() && guard.check_reasoning(&details)) {
            return Read::Degenerate { discarded: guard.streamed() };
        }
        note_prompt_tokens(&out, prompt_tokens);
        emit.forward(&mut out, &mut text);
        match flow {
            Flow::Continue => {}
            Flow::Done => break,
            Flow::Stop => return Read::Finished,
            Flow::Broken => {
                let cause = assembler.upstream.take().map_or(DropCause::Dropped, DropCause::Upstream);
                return Read::Dropped { text, tool_call_in_progress: assembler.tool_call_in_progress(), cause };
            }
        }
    }
    let tool_call_in_progress = assembler.tool_call_in_progress();
    // The stream ended cleanly but early (no [DONE] or finish_reason): a proxy closed it, or the
    // server lost the connection. A retry gets a fresh response.
    let finished = assembler.finish(&mut out);
    note_prompt_tokens(&out, prompt_tokens);
    emit.forward(&mut out, &mut text);
    match finished {
        Ok(()) => Read::Finished,
        Err(Incomplete) => Read::Dropped { text, tool_call_in_progress, cause: DropCause::Dropped },
    }
}

/// #116: the prompt count of a `Usage` event among `out`, if any.
fn note_prompt_tokens(out: &[ProviderEvent], prompt_tokens: &mut Option<u64>) {
    for event in out {
        if let ProviderEvent::Usage(usage, _) = event {
            *prompt_tokens = Some(usage.prompt_tokens);
        }
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
    read_error_body(response, cancel).await.map(|(message, _)| message)
}

/// The message in an error response's body, and the body itself (#159: a plan's limit is told by its code).
pub(crate) async fn read_error_body(response: reqwest::Response, cancel: &CancellationToken) -> Option<(String, String)> {
    let content_type = response.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(str::to_string);
    let status_text = response.status().canonical_reason().unwrap_or_default();
    let body = until_cancelled(cancel, response.text()).await?.unwrap_or_default();
    Some((error_message(content_type.as_deref(), status_text, &body), body))
}

/// Whether a failed request is worth retrying ([`retriable_failure`], #107): reqwest says it failed connecting,
/// sending, timing out or reading the body, or the wording says so (hyper's and rustls' differ from Bun's, so the
/// whole chain is checked). A certificate error never is.
pub(crate) fn is_network_failure(err: &reqwest::Error) -> bool {
    let kind =
        FailureKind { connect: err.is_connect(), timeout: err.is_timeout(), body: err.is_body(), request: err.is_request() };
    retriable_failure(kind, &error_chain(err))
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

    /// The `Retry` event before a new attempt (`retry`/`MAX_ATTEMPTS - 1`), saying why.
    pub(crate) fn retry(&self, label: &str, retry: u32, continued: bool, cause: RetryCause) {
        let max = MAX_ATTEMPTS - 1;
        let what = match cause {
            RetryCause::Stalled => "the stream stalled",
            _ => "connection lost",
        };
        self.send(ProviderEvent::Retry {
            attempt: retry,
            max,
            message: format!("{label}: {what}, retrying ({retry}/{max})…"),
            continued,
            cause,
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

/// Adds a streamed `reasoning_details` block to `blocks`. A piece of the block before it (the same `index` and `type`)
/// is appended to it: its strings (`text`, `summary`, `data`) are concatenated, and any other field it carries
/// (`signature`, `id`, `format`) fills in one the block didn't have yet. A block without an `index` is kept as is, and
/// so is one that can't be a piece of the one before: both carry a different `id`, or both already carry a `signature`
/// (two whole blocks under one index; merging would drop the second signature, or corrupt both).
fn merge_reasoning_block(blocks: &mut Vec<Value>, block: Value) {
    let filled = |v: &Value, key: &str| v.get(key).filter(|x| !x.is_null() && *x != &Value::String(String::new())).cloned();
    let same = |a: &Value, b: &Value| {
        a.get("index").is_some_and(|i| !i.is_null())
            && a.get("index") == b.get("index")
            && a.get("type") == b.get("type")
            && !matches!((filled(a, "id"), filled(b, "id")), (Some(x), Some(y)) if x != y)
            && !(filled(a, "signature").is_some() && filled(b, "signature").is_some())
    };
    let (Some(last), Value::Object(piece)) = (blocks.last_mut(), &block) else {
        blocks.push(block);
        return;
    };
    if !same(last, &block) {
        blocks.push(block);
        return;
    }
    let Value::Object(into) = last else { unreachable!("same() saw an index on it") };
    for (key, value) in piece {
        match (key.as_str(), into.get_mut(key), value) {
            ("text" | "summary" | "data", Some(Value::String(have)), Value::String(more)) => have.push_str(more),
            (_, Some(have), _) if !have.is_null() && have != &Value::String(String::new()) => {}
            _ => {
                into.insert(key.clone(), value.clone());
            }
        }
    }
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
    fn max_tokens_goes_after_the_tools_before_extra_fields_and_is_capped() {
        // #96
        let mut options = OpenAiCompatOptions { model: "acme/m".into(), max_tokens: Some(8192), ..Default::default() };
        options.body.insert("reasoning".into(), serde_json::json!({"enabled": true}));
        let body = serde_json::to_string(&request_body(&options, &[], None, &[], ToolChoice::None)).unwrap();
        assert!(body.ends_with(r#""tool_choice":"none","max_tokens":8192,"reasoning":{"enabled":true}}"#), "{body}");
        assert_eq!(max_tokens_for(None), DEFAULT_MAX_TOKENS);
        assert_eq!(max_tokens_for(Some(4096)), 4096);
        assert_eq!(max_tokens_for(Some(1_000_000)), DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn a_window_refusal_says_the_window_and_the_prompt_size() {
        let openrouter = "This endpoint's maximum context length is 131072 tokens. However, you requested about 140000 tokens (108000 of text input, 32000 in the output). Please reduce the length.";
        assert_eq!(window_refusal(openrouter), Some((131_072, 108_000)));
        let with_tools = "maximum context length is 1000 tokens. However, you requested about 1200 tokens (700 of text input, 100 of tool input, 400 in the output).";
        assert_eq!(window_refusal(with_tools), Some((1000, 800)));
        assert_eq!(window_refusal("max_tokens exceeds the remaining context"), None);
    }

    #[test]
    fn the_cap_is_the_room_left_in_the_window_with_a_floor() {
        // No window known: the model's limit.
        assert_eq!(Room::default().cap(32_000, 1_000_000), Some(32_000));
        // Before any measurement, 4 bytes a token (+5%): 400 KB is 105000 tokens.
        let room = Room { window: Some(131_072), tokens_per_byte: None };
        assert_eq!(room.cap(32_000, 400_000), Some(26_072));
        assert_eq!(room.cap(32_000, 1_000), Some(32_000));
        // Measured: the highest ratio wins, so a cached prefix doesn't lower it.
        let mut room = Room { window: Some(50_000), tokens_per_byte: None };
        room.record(30_000, 100_000);
        room.record(10_000, 100_000);
        assert_eq!(room.cap(32_000, 100_000), Some(18_500));
        // Under the floor the request goes without a cap: a cap that small would be refused too.
        assert_eq!(room.cap(32_000, 150_000), Some(2_750));
        assert_eq!(room.cap(32_000, 200_000), None);
        assert_eq!(room.cap(32_000, 157_000), None, "545 tokens left: under the floor");
        // A model whose own limit is under the floor keeps it while that fits.
        assert_eq!(room.cap(512, 157_000), Some(512));
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
        run_with(ReplyAssembler::new("TestRouter"), chunks)
    }

    fn run_with(mut assembler: ReplyAssembler, chunks: &[&str]) -> (Vec<ProviderEvent>, Result<(), Incomplete>) {
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
            ProviderEvent::Usage(
                Usage { prompt_tokens: 100, completion_tokens: 5, cached_tokens: Some(80), cost: Some(0.002) },
                ReplyEnd::default()
            )
        );
    }

    // #90: the finish reason rides the Usage event, even when the usage chunk arrives before
    // the finish_reason chunk (OpenRouter's order).
    #[test]
    fn usage_carries_the_finish_reason() {
        let (events, _) = run(&[
            r#"{"choices":[{"delta":{"content":"Now checking issue #7 before commenting:"}}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":5}}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ]);
        assert_eq!(
            events[1],
            ProviderEvent::Usage(
                Usage { prompt_tokens: 100, completion_tokens: 5, cached_tokens: None, cost: None },
                ReplyEnd { finish_reason: Some("stop".into()), raw_response: None }
            )
        );
        assert_eq!(events[2], ProviderEvent::Done { reason: Some("stop".into()) });
    }

    // #90: the raw response payloads are collected only when the stream was asked to, and are
    // what the debug flag's request record gets ("the raw last response" = the last attempt's).
    #[test]
    fn the_raw_response_is_collected_only_behind_the_debug_flag() {
        let chunks = [
            r#"{"choices":[{"delta":{"content":"hi"}}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":5}}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ];
        let (off, _) = run_with(ReplyAssembler::new("T"), &chunks);
        assert_eq!(
            off,
            [
                ProviderEvent::TextDelta("hi".into()),
                ProviderEvent::Usage(
                    Usage { prompt_tokens: 100, completion_tokens: 5, cached_tokens: None, cost: None },
                    ReplyEnd { finish_reason: Some("stop".into()), raw_response: None }
                ),
                ProviderEvent::Done { reason: Some("stop".into()) }
            ]
        );
        let (on, _) = run_with(ReplyAssembler::with_debug("T", true), &chunks);
        let last = on.iter().find_map(|e| if let ProviderEvent::Usage(_, end) = e { Some(end) } else { None }).unwrap();
        assert_eq!(
            last,
            &ReplyEnd {
                finish_reason: Some("stop".into()),
                raw_response: Some(format!("{}\n{}\n{}\n{}", chunks[0], chunks[1], chunks[2], chunks[3]))
            }
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
    fn an_upstream_abort_in_the_stream_is_retried_like_a_dropped_connection() {
        // What OpenRouter sent when Gemini's upstream gave up mid-reply.
        for error in [
            r#"{"error":{"code":502,"message":"The operation was aborted"}}"#,
            r#"{"error":{"message":"The operation was aborted"}}"#,
            r#"{"error":{"code":"503","message":"Provider is overloaded"}}"#,
        ] {
            let (events, result) = run(&[r#"{"choices":[{"delta":{"content":"partial"}}]}"#, error]);
            assert_eq!(result, Err(Incomplete), "{error}");
            assert!(!events.iter().any(|e| matches!(e, ProviderEvent::Error(_))), "{error}: {events:?}");
        }
        // A request the provider refuses isn't retried: same code family, different meaning.
        let (_, result) = run(&[r#"{"error":{"code":400,"message":"Provider returned error"}}"#]);
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn a_transient_error_after_the_reply_finished_doesnt_resend_it() {
        // #112 review: finish_reason came, then an upstream error before [DONE]: the reply stands.
        let (events, result) = run(&[
            r#"{"choices":[{"delta":{"content":"All done."},"finish_reason":"stop"}]}"#,
            r#"{"error":{"code":502,"message":"The operation was aborted"}}"#,
        ]);
        assert_eq!(result, Ok(()));
        assert!(!events.iter().any(|e| matches!(e, ProviderEvent::Error(_))), "{events:?}");
        assert!(events.iter().any(|e| matches!(e, ProviderEvent::TextDelta(t) if t == "All done.")), "{events:?}");
    }

    #[test]
    fn a_stream_that_ends_before_the_reply_is_complete_needs_a_retry() {
        let (_, result) = run(&[r#"{"choices":[{"delta":{"content":"Writing the fi"}}]}"#]);
        assert_eq!(result, Err(Incomplete));
    }

    /// One block streamed a token per chunk (as OpenRouter sends DeepSeek's) is kept as one block, its signature
    /// included; a block with another index or type stays separate.
    #[test]
    fn a_reasoning_block_streamed_in_pieces_is_kept_as_one() {
        let piece = |json: &str| format!(r#"{{"choices":[{{"delta":{{"reasoning_details":[{json}]}}}}]}}"#);
        let (events, _) = run(&[
            &piece(r#"{"type":"reasoning.text","index":0,"format":"unknown","text":"The"}"#),
            &piece(r#"{"type":"reasoning.text","index":0,"format":"unknown","text":" user"}"#),
            &piece(r#"{"type":"reasoning.text","index":0,"text":" asks","signature":"sig"}"#),
            &piece(r#"{"type":"reasoning.encrypted","index":0,"data":"ab"}"#),
            &piece(r#"{"type":"reasoning.encrypted","index":0,"data":"cd"}"#),
            &piece(r#"{"type":"reasoning.text","index":1,"text":"next"}"#),
            "[DONE]",
        ]);
        let Some(ProviderEvent::Reasoning(r)) = events.iter().find(|e| matches!(e, ProviderEvent::Reasoning(_))) else {
            panic!("no reasoning: {events:?}")
        };
        assert_eq!(
            r.0,
            serde_json::json!([
                {"type": "reasoning.text", "index": 0, "format": "unknown", "text": "The user asks", "signature": "sig"},
                {"type": "reasoning.encrypted", "index": 0, "data": "abcd"},
                {"type": "reasoning.text", "index": 1, "text": "next"},
            ])
        );
    }

    /// Blocks without an index are never merged: nothing says they are one.
    #[test]
    fn reasoning_blocks_without_an_index_stay_apart() {
        let mut blocks = Vec::new();
        merge_reasoning_block(&mut blocks, serde_json::json!({"type": "reasoning.text", "text": "a"}));
        merge_reasoning_block(&mut blocks, serde_json::json!({"type": "reasoning.text", "text": "b"}));
        assert_eq!(blocks.len(), 2);
    }

    /// Two whole blocks under one index (a different id each, or a signature each) stay apart, signatures intact; a
    /// piece with the same id, or with none, still joins.
    #[test]
    fn reasoning_blocks_with_their_own_id_or_signature_stay_apart() {
        use serde_json::json;
        let mut blocks = Vec::new();
        merge_reasoning_block(&mut blocks, json!({"type": "reasoning.encrypted", "index": 0, "id": "a", "data": "x"}));
        merge_reasoning_block(&mut blocks, json!({"type": "reasoning.encrypted", "index": 0, "id": "a", "data": "y"}));
        merge_reasoning_block(&mut blocks, json!({"type": "reasoning.encrypted", "index": 0, "data": "z"}));
        merge_reasoning_block(&mut blocks, json!({"type": "reasoning.encrypted", "index": 0, "id": "b", "data": "w"}));
        assert_eq!(
            blocks,
            vec![
                json!({"type": "reasoning.encrypted", "index": 0, "id": "a", "data": "xyz"}),
                json!({"type": "reasoning.encrypted", "index": 0, "id": "b", "data": "w"}),
            ]
        );
        let mut blocks = Vec::new();
        merge_reasoning_block(&mut blocks, json!({"type": "reasoning.text", "index": 0, "text": "a", "signature": "s1"}));
        merge_reasoning_block(&mut blocks, json!({"type": "reasoning.text", "index": 0, "text": "b", "signature": "s2"}));
        merge_reasoning_block(&mut blocks, json!({"type": "reasoning.text", "index": 0, "id": "", "text": "c"}));
        assert_eq!(
            blocks,
            vec![
                json!({"type": "reasoning.text", "index": 0, "text": "a", "signature": "s1"}),
                json!({"type": "reasoning.text", "index": 0, "text": "bc", "signature": "s2", "id": ""}),
            ]
        );
    }

    #[test]
    fn reasoning_details_are_kept_for_the_assistant_turn() {
        let (events, _) =
            run(&[r#"{"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.text","text":"hm"}]}}]}"#, "[DONE]"]);
        assert!(matches!(&events[0], ProviderEvent::Reasoning(r) if r.0[0]["text"] == "hm"));
    }
}
