//! The adapter for OpenAI's Responses API: `POST {baseUrl}/responses` with streaming (marv-rs #159, not in Marv).
//! Sign in with ChatGPT bills only through it ([`crate::chatgpt`]).
//!
//! Like [`crate::openai_compat`] it turns the vendor's stream into [`ProviderEvent`]s, so nothing above this module
//! knows which wire format is on the other end.
//!
//! - **Stateless:** `store: false` and the whole conversation in `input` on every request (no
//!   `previous_response_id`). [`to_input`] is deterministic (typed structs, fixed field order), so repeated requests
//!   share a cacheable prefix; `ToolCall::arguments` is sent as the model's raw text, never re-serialized.
//! - The system prompt goes in `instructions`: a `system` message item is refused on the ChatGPT route.
//! - Tool calls are `function_call` items, results `function_call_output` items, matched by `call_id`. Tools are
//!   sent flat (`{"type":"function","name",…}`), not nested under `function` as in chat completions; with a
//!   [`ToolNamespace`] (the ChatGPT route, whose preview takes function tools only grouped in a namespace) they go
//!   inside one `{"type":"namespace","name","description","tools":[…]}`, and the history's `function_call` items
//!   carry that `namespace`.
//! - **Reasoning round-trip:** with `include: ["reasoning.encrypted_content"]` the stream's `reasoning` output items
//!   carry their encrypted state. They're kept on the assistant turn ([`ProviderEvent::Reasoning`], an array of the
//!   items) and sent back before that turn's message and calls, without their `id` (with `store: false` the server
//!   has nothing stored under it and refuses the reference). Anything else in a turn's reasoning (OpenRouter's
//!   `reasoning_details`, from before a provider switch) is left out.
//! - **Events read:** `response.output_text.delta` (text), `response.reasoning_summary_text.delta` and
//!   `response.reasoning_text.delta` (thinking), `response.output_item.added` / `.done` and
//!   `response.function_call_arguments.delta` (calls; the `done` item's arguments are the whole text), then
//!   `response.completed` (usage; the reply is complete only then), `response.incomplete` (complete, finish reason
//!   `length` when it hit the output limit), `response.failed` and `error`. Anything else is ignored.
//! - Finish reasons map to chat completions' words, which the agent loop knows: `stop`, `tool_calls`, `length`.
//! - **Retries** as in [`crate::openai_compat`] (the loop helpers are shared): network errors, drops, stalls and
//!   429/502/503/504 up to [`MAX_ATTEMPTS`]; a drop after text, outside a tool call, continues (#64); one retry
//!   after a degenerate reply (#96). A stream that ends without `response.completed`/`incomplete` is a drop.
//! - **Credentials** come from a [`Credentials`]: an API key as it is, or a sign-in whose access token is refreshed
//!   when it's about to expire. A 401 asks it for a fresh token once, then ends with its message.
//! - `max_output_tokens` and sampling fields are sent only when set ([`ResponsesOptions`]): the ChatGPT route
//!   refuses them.
//! - **Tools are sent with `strict: false`** (#265). Without the field the Responses API treats a function tool as
//!   strict, which makes every property required: models then pad each optional field (mostly with `""`), and a tool
//!   that checks its fields refuses the call. Marv's schemas list only what's really required.
//! - **`prompt_cache_key`** (#266), when the options carry one: a value fixed for the provider's life (a session, until
//!   `/model` makes a new one), which OpenAI uses to route a session's requests to the same prompt cache. Without it
//!   cache hits were left to chance (36% of a session's input in one run), and on a plan uncached input runs the
//!   allowance down faster.
//! - **Stream errors** come as `{"type":"error","error":{"code","message"}}` (#264: a plan's usage limit is sent this
//!   way, after the 200); the older top-level `code`/`message` is still read.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use marv_core::{ChatTurn, ProviderEvent, ReplyEnd, RetryCause, ToolCall, ToolChoice, ToolSpec, Usage};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::degenerate::DegenerateGuard;
use crate::errors::{ErrorHints, ErrorObject, describe_error, http_error, plan_limit, plan_limit_of, unreachable};
use crate::openai_compat::{
    DropCause, Emit, Read, after_degenerate, backoff, channel_stream, is_network_failure, keep_or_restart, read_error_body,
    root_cause, status_wait, until_cancelled, with_continuation,
};
use crate::retry::{BodyError, MAX_ATTEMPTS, RetryTiming, idle_guard, retriable_http_status, sleep_abortable};
use crate::sse::parse_sse;
use crate::{EventStream, Provider, ReasoningEffort, StreamOptions};

/// Where a request's bearer token comes from.
#[async_trait]
pub trait Credentials: Send + Sync {
    /// The token for the next request (a sign-in refreshes it first when it's about to expire). `Err`: why there is
    /// none, as a sentence the user can act on.
    async fn bearer(&self) -> Result<String, String>;
    /// The server refused `token` (a 401): a fresh one, or why there won't be one. An API key has nothing fresher.
    async fn refused(&self, token: &str) -> Result<String, String> {
        let _ = token;
        Err(String::new())
    }
}

/// An API key, sent as it is.
pub struct ApiKey(pub String);

#[async_trait]
impl Credentials for ApiKey {
    async fn bearer(&self) -> Result<String, String> {
        Ok(self.0.clone())
    }
}

#[derive(Clone)]
pub struct ResponsesOptions {
    /// Shown in the status bar.
    pub name: String,
    /// Used in error messages, e.g. "ChatGPT".
    pub label: String,
    /// `https://api.openai.com/v1`.
    pub base_url: String,
    pub model: String,
    pub credentials: Arc<dyn Credentials>,
    /// Extra request headers, in order.
    pub headers: Vec<(String, String)>,
    /// Sent as `reasoning: {effort, summary: "auto"}` when set.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Ask for the encrypted reasoning state, to send it back (`include: ["reasoning.encrypted_content"]`).
    pub encrypted_reasoning: bool,
    /// Sent as `max_output_tokens` when set. `None` on the ChatGPT route, which refuses it.
    pub max_output_tokens: Option<u64>,
    /// The namespace the tools go in (`None`: flat). The ChatGPT route's preview takes them only grouped.
    pub tool_namespace: Option<ToolNamespace>,
    /// The model's context window, when known (the session compacts before it fills).
    pub context_length: Option<u64>,
    pub hints: ErrorHints,
    pub timing: RetryTiming,
    /// A stream silent for this long is retried (#88).
    pub idle_timeout: Option<Duration>,
    /// #266: sent as `prompt_cache_key`, so the session's requests reach the same prompt cache. Made once per provider.
    pub prompt_cache_key: Option<String>,
}

// ---- Request (wire) types. Field order here is the byte order on the wire: don't reorder. ----

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ContentPart {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub text: String,
}

/// One `input` item.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum InputItem {
    Message {
        #[serde(rename = "type")]
        kind: &'static str,
        role: &'static str,
        content: Vec<ContentPart>,
    },
    FunctionCall {
        #[serde(rename = "type")]
        kind: &'static str,
        call_id: String,
        name: String,
        /// The model's raw JSON text (prompt cache: never re-serialized).
        arguments: String,
        /// The tools' namespace, when they're sent in one.
        #[serde(skip_serializing_if = "Option::is_none")]
        namespace: Option<&'static str>,
    },
    FunctionCallOutput {
        #[serde(rename = "type")]
        kind: &'static str,
        call_id: String,
        output: String,
    },
    /// A `reasoning` item from an earlier reply, without its `id`, otherwise as it came.
    Reasoning(Value),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WireTool<'a> {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub name: &'a str,
    pub description: &'a str,
    pub parameters: &'a Value,
    /// Always `false` (#265): strict would make every property required.
    pub strict: bool,
}

/// One entry of `tools`: a function, or a namespace holding them.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ToolEntry<'a> {
    Function(WireTool<'a>),
    Namespace {
        #[serde(rename = "type")]
        kind: &'static str,
        name: &'static str,
        description: &'static str,
        tools: Vec<WireTool<'a>>,
    },
}

/// A namespace the tools are grouped in (the Responses API's `namespace` tool).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolNamespace {
    /// Used in tool calls (`function_call.namespace`).
    pub name: &'static str,
    /// Shown to the model.
    pub description: &'static str,
}

/// The namespace Marv's tools go in where a route wants one (ChatGPT).
pub const MARV_TOOLS: ToolNamespace = ToolNamespace { name: "marv", description: "Marv's tools for working on this project." };

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ReasoningParam {
    pub effort: ReasoningEffort,
    pub summary: &'static str,
}

/// The whole request body.
#[derive(Debug, Clone, Serialize)]
pub struct ResponsesRequest<'a> {
    pub model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<&'a str>,
    pub input: Vec<InputItem>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolEntry<'a>>,
    /// Only ever "none" (#59, the wrap-up reply); `Auto` sends nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningParam>,
    pub store: bool,
    pub stream: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
}

fn message(role: &'static str, kind: &'static str, text: &str) -> InputItem {
    InputItem::Message { kind: "message", role, content: vec![ContentPart { kind, text: text.to_string() }] }
}

/// The reasoning items of a turn's reasoning worth sending back: Responses `reasoning` items, without their `id`.
fn reasoning_items(reasoning: Option<&marv_core::Reasoning>) -> impl Iterator<Item = InputItem> + '_ {
    reasoning.and_then(|r| r.0.as_array()).into_iter().flatten().filter_map(|item| {
        let mut item = item.as_object().filter(|o| o.get("type").and_then(Value::as_str) == Some("reasoning"))?.clone();
        item.remove("id");
        Some(InputItem::Reasoning(Value::Object(item)))
    })
}

/// Marv's conversation as Responses `input` items. Deterministic, so repeated requests share a cacheable prefix.
/// `namespace`: the one the tools are sent in, named on every `function_call`.
pub fn to_input(history: &[ChatTurn], namespace: Option<&'static str>) -> Vec<InputItem> {
    let mut input = Vec::new();
    for turn in history {
        match turn {
            ChatTurn::User { text } => input.push(message("user", "input_text", text)),
            ChatTurn::Assistant { text, tool_calls, reasoning } => {
                input.extend(reasoning_items(reasoning.as_ref()));
                // An empty assistant message is noise (a turn of calls only).
                if !text.is_empty() {
                    input.push(message("assistant", "output_text", text));
                }
                input.extend(tool_calls.iter().map(|c| InputItem::FunctionCall {
                    kind: "function_call",
                    call_id: c.id.clone(),
                    name: c.name.clone(),
                    arguments: c.arguments.clone(),
                    namespace,
                }));
            }
            ChatTurn::Tool { call_id, text, .. } => input.push(InputItem::FunctionCallOutput {
                kind: "function_call_output",
                call_id: call_id.clone(),
                output: text.clone(),
            }),
        }
    }
    input
}

/// The tools as sent: flat, or (`namespace`) all inside one namespace. None, nothing (not an empty namespace).
fn wire_tools(tools: &[ToolSpec], namespace: Option<ToolNamespace>) -> Vec<ToolEntry<'_>> {
    let flat = tools.iter().map(|t| WireTool {
        kind: "function",
        name: &t.name,
        description: &t.description,
        parameters: &t.parameters,
        strict: false,
    });
    match namespace {
        Some(_) if tools.is_empty() => Vec::new(),
        Some(ns) => {
            vec![ToolEntry::Namespace { kind: "namespace", name: ns.name, description: ns.description, tools: flat.collect() }]
        }
        None => flat.map(ToolEntry::Function).collect(),
    }
}

/// Builds the request body for one stream.
pub fn request_body<'a>(
    options: &'a ResponsesOptions,
    history: &[ChatTurn],
    system: Option<&'a str>,
    tools: &'a [ToolSpec],
    tool_choice: ToolChoice,
) -> ResponsesRequest<'a> {
    ResponsesRequest {
        model: &options.model,
        instructions: system,
        input: to_input(history, options.tool_namespace.map(|n| n.name)),
        tools: wire_tools(tools, options.tool_namespace),
        tool_choice: (tool_choice == ToolChoice::None).then_some("none"),
        reasoning: options.reasoning_effort.map(|effort| ReasoningParam { effort, summary: "auto" }),
        store: false,
        stream: true,
        include: if options.encrypted_reasoning { vec!["reasoning.encrypted_content"] } else { Vec::new() },
        prompt_cache_key: options.prompt_cache_key.as_deref(),
        max_output_tokens: options.max_output_tokens,
    }
}

// ---- Response (stream) types: only the fields Marv reads. ----

#[derive(Debug, Default, Deserialize)]
pub struct StreamEvent {
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub delta: Option<String>,
    #[serde(default)]
    pub output_index: Option<usize>,
    #[serde(default)]
    pub item: Option<Value>,
    #[serde(default)]
    pub response: Option<ResponseObject>,
    /// An `error` event's details (#264): `{"type":"error","error":{"code","message"}}`.
    #[serde(default)]
    pub error: Option<ErrorObject>,
    /// An `error` event's own fields, in the older form with them at the top level.
    #[serde(default)]
    pub code: Option<Value>,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ResponseObject {
    #[serde(default)]
    pub usage: Option<ResponseUsage>,
    #[serde(default)]
    pub error: Option<ErrorObject>,
    #[serde(default)]
    pub incomplete_details: Option<IncompleteDetails>,
}

#[derive(Debug, Default, Deserialize)]
pub struct IncompleteDetails {
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ResponseUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub input_tokens_details: Option<InputTokensDetails>,
}

#[derive(Debug, Default, Deserialize)]
pub struct InputTokensDetails {
    #[serde(default)]
    pub cached_tokens: Option<u64>,
}

pub use crate::openai_compat::{Flow, Incomplete};

/// Turns one response's SSE `data` payloads into [`ProviderEvent`]s. Pure (no I/O), so the stream's edge cases are
/// tested without a server.
#[derive(Debug)]
pub struct ReplyAssembler {
    label: String,
    /// Calls by output index, in arrival order of their index; complete once their item is done.
    pending: BTreeMap<usize, ToolCall>,
    reason: Option<String>,
    complete: bool,
    /// `reasoning` output items, in order.
    reasoning: Vec<Value>,
    usage: Option<Usage>,
    raw: Option<Vec<String>>,
    /// #64: what a dropped stream had already delivered.
    pub text_received: bool,
    /// An error a fresh attempt may get past (`errors::transient`): retried like a drop.
    pub upstream: Option<String>,
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
        }
    }

    /// #64: a tool call is mid-stream (a drop now can't be continued).
    pub fn tool_call_in_progress(&self) -> bool {
        !self.pending.is_empty()
    }

    /// The display index of the call at `output_index`: its place among the calls.
    fn index_of(&self, output_index: usize) -> usize {
        self.pending.range(..output_index).count()
    }

    /// An error from the stream: retried when a fresh attempt may get past it, else the turn's error.
    fn error(&mut self, error: &ErrorObject, out: &mut Vec<ProviderEvent>) -> Flow {
        // #159: a plan's limit (`response.failed` after the stream opened): a hard stop, before the retry check (its
        // message may well say "try again later").
        if let Some(message) = plan_limit_of(&self.label, error.code.as_ref(), error.message.as_deref()) {
            out.push(ProviderEvent::Error(message));
            return Flow::Stop;
        }
        let message = format!("{}: {}", self.label, describe_error(error));
        if crate::errors::transient(error) {
            self.upstream = Some(message);
            return Flow::Broken;
        }
        out.push(ProviderEvent::Error(message));
        Flow::Stop
    }

    fn usage(&mut self, response: &ResponseObject) {
        if let Some(u) = &response.usage {
            self.usage = Some(Usage {
                prompt_tokens: u.input_tokens,
                completion_tokens: u.output_tokens,
                cached_tokens: u.input_tokens_details.as_ref().and_then(|d| d.cached_tokens),
                cost: None,
            });
        }
    }

    /// Handles one SSE event's data, pushing what it yields onto `out`.
    pub fn on_data(&mut self, data: &str, out: &mut Vec<ProviderEvent>) -> Flow {
        if let Some(raw) = &mut self.raw {
            raw.push(data.to_string());
        }
        if data == "[DONE]" {
            return if self.complete { Flow::Done } else { Flow::Continue };
        }
        let Ok(event) = serde_json::from_str::<StreamEvent>(data) else { return Flow::Broken };
        match event.kind.as_str() {
            "response.output_text.delta" => {
                if let Some(text) = event.delta.filter(|t| !t.is_empty()) {
                    self.text_received = true;
                    out.push(ProviderEvent::TextDelta(text));
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(text) = event.delta.filter(|t| !t.is_empty()) {
                    out.push(ProviderEvent::ThinkingDelta(text));
                }
            }
            "response.output_item.added" => {
                let (Some(item), Some(at)) = (event.item, event.output_index) else { return Flow::Continue };
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    let field = |name: &str| item.get(name).and_then(Value::as_str).unwrap_or_default().to_string();
                    let call = ToolCall { id: field("call_id"), name: field("name"), arguments: String::new() };
                    let name = call.name.clone();
                    self.pending.insert(at, call);
                    // The name alone is worth showing before any arguments.
                    out.push(ProviderEvent::ToolCallDelta { index: self.index_of(at), name, text: String::new() });
                }
            }
            "response.function_call_arguments.delta" => {
                let (Some(at), Some(text)) = (event.output_index, event.delta) else { return Flow::Continue };
                let index = self.index_of(at);
                if let Some(call) = self.pending.get_mut(&at) {
                    call.arguments.push_str(&text);
                    out.push(ProviderEvent::ToolCallDelta { index, name: call.name.clone(), text });
                }
            }
            "response.output_item.done" => {
                let (Some(item), Some(at)) = (event.item, event.output_index) else { return Flow::Continue };
                match item.get("type").and_then(Value::as_str) {
                    Some("function_call") => {
                        let call = self.pending.entry(at).or_insert_with(|| ToolCall {
                            id: String::new(),
                            name: String::new(),
                            arguments: String::new(),
                        });
                        // The done item is the whole call: its arguments are the model's exact text.
                        for (field, slot) in
                            [("call_id", &mut call.id), ("name", &mut call.name), ("arguments", &mut call.arguments)]
                        {
                            if let Some(value) = item.get(field).and_then(Value::as_str) {
                                *slot = value.to_string();
                            }
                        }
                    }
                    Some("reasoning") => self.reasoning.push(item),
                    _ => {}
                }
            }
            "response.completed" => {
                if let Some(response) = &event.response {
                    self.usage(response);
                }
                self.reason = Some(if self.pending.is_empty() { "stop" } else { "tool_calls" }.into());
                self.complete = true;
                return Flow::Done;
            }
            "response.incomplete" => {
                let response = event.response.unwrap_or_default();
                self.usage(&response);
                let why = response.incomplete_details.and_then(|d| d.reason);
                self.reason = Some(match why.as_deref() {
                    Some("max_output_tokens") => "length".into(),
                    Some(other) => other.to_string(),
                    None => "length".into(),
                });
                self.complete = true;
                return Flow::Done;
            }
            "response.failed" => {
                let error = event.response.and_then(|r| r.error).or(event.error).unwrap_or_default();
                return self.error(&error, out);
            }
            "error" => {
                let error = event.error.unwrap_or(ErrorObject { code: event.code, message: event.message, metadata: None });
                return self.error(&error, out);
            }
            _ => {}
        }
        Flow::Continue
    }

    /// The body ended. Complete: the calls (in output order), the reasoning items, the usage with how the reply
    /// ended (#90), and `Done`. Not complete: [`Incomplete`], retry.
    pub fn finish(self, out: &mut Vec<ProviderEvent>) -> Result<(), Incomplete> {
        if !self.complete {
            return Err(Incomplete);
        }
        for (i, (_, mut call)) in self.pending.into_iter().enumerate() {
            if call.id.is_empty() {
                call.id = format!("call_{i}");
            }
            out.push(ProviderEvent::ToolCall(call));
        }
        if !self.reasoning.is_empty() {
            out.push(ProviderEvent::Reasoning(marv_core::Reasoning(Value::Array(self.reasoning))));
        }
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

/// The Responses API adapter.
pub struct ResponsesProvider {
    pub options: ResponsesOptions,
    pub client: reqwest::Client,
}

impl ResponsesProvider {
    pub fn new(options: ResponsesOptions) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            // Per read, not for the whole reply.
            .read_timeout(Duration::from_secs(300))
            .build()
            .unwrap_or_default();
        Self { options, client }
    }
}

impl Provider for ResponsesProvider {
    fn name(&self) -> &str {
        &self.options.name
    }

    fn context_length(&self) -> Option<u64> {
        self.options.context_length
    }

    fn stream(&self, history: &[ChatTurn], options: StreamOptions) -> EventStream {
        let outgoing = Outgoing {
            history: history.to_vec(),
            system: options.system,
            tools: options.tools,
            tool_choice: options.tool_choice,
            note: None,
        };
        let (client, adapter, cancel, debug_raw) = (self.client.clone(), self.options.clone(), options.cancel, options.debug_raw);
        channel_stream(move |emit| run(client, adapter, outgoing, cancel, debug_raw, emit))
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
    fn body(&mut self, options: &ResponsesOptions, kept: &str) -> Vec<u8> {
        let Self { history, system, tools, tool_choice, note } = self;
        with_continuation(history, *note, kept, |history| {
            let request = request_body(options, history, system.as_deref(), tools, *tool_choice);
            serde_json::to_vec(&request).expect("a request body is plain JSON")
        })
    }
}

/// The retry loop: [`MAX_ATTEMPTS`] for network errors, drops, stalls and 429/502/503/504; once more after a degenerate
/// reply; once with a fresh token after a 401.
async fn run(
    client: reqwest::Client,
    o: ResponsesOptions,
    mut outgoing: Outgoing,
    cancel: CancellationToken,
    debug_raw: bool,
    emit: Emit,
) {
    let label = o.label.as_str();
    let url = format!("{}/responses", o.base_url);
    let mut kept = String::new();
    let mut cause = RetryCause::Dropped;
    let (mut attempt, mut announce, mut degenerate_retries) = (0, false, 0);
    let mut token = match until_cancelled(&cancel, o.credentials.bearer()).await {
        None => return,
        Some(Ok(token)) => token,
        Some(Err(why)) => return emit.send(ProviderEvent::Error(format!("{label}: {why}"))),
    };
    let mut refreshed = false;
    loop {
        if cancel.is_cancelled() {
            return;
        }
        attempt += 1;
        if announce {
            emit.retry(label, attempt - 1, !kept.is_empty(), std::mem::take(&mut cause));
        }
        announce = true;

        let mut request = client.post(&url).header(reqwest::header::CONTENT_TYPE, "application/json").bearer_auth(&token);
        for (name, value) in &o.headers {
            request = request.header(name, value);
        }
        let body = outgoing.body(&o, &kept);
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
                emit.send(ProviderEvent::Error(unreachable(label, &o.base_url, &root_cause(&err), None)));
                return;
            }
        };

        let status = response.status().as_u16();
        if response.status().is_success() {
            match read_reply(response, label, o.idle_timeout, &cancel, debug_raw, &emit).await {
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

        // A refused token: one fresh one (a sign-in refreshes), then the error.
        if status == 401 && !refreshed {
            refreshed = true;
            match until_cancelled(&cancel, o.credentials.refused(&token)).await {
                None => return,
                Some(Ok(fresh)) => {
                    token = fresh;
                    (attempt, announce) = (attempt - 1, false);
                    continue;
                }
                Some(Err(why)) if !why.is_empty() => return emit.send(ProviderEvent::Error(format!("{label}: {why}"))),
                Some(Err(_)) => {}
            }
        }
        // #159: the body first: a plan's limit (a 429) is a hard stop, never retried, never another key or provider.
        let wait = status_wait(&response, o.timing, attempt);
        let Some((detail, body)) = read_error_body(response, &cancel).await else { return };
        if let Some(message) = plan_limit(label, &body) {
            return emit.send(ProviderEvent::Error(message));
        }
        if retriable_http_status(status) && attempt < MAX_ATTEMPTS {
            if sleep_abortable(wait, &cancel).await {
                return;
            }
            continue;
        }
        emit.send(ProviderEvent::Error(http_error(status, &detail, label, &o.model, o.hints)));
        return;
    }
}

/// Parses the SSE body, sending events as they come.
async fn read_reply(
    response: reqwest::Response,
    label: &str,
    idle: Option<Duration>,
    cancel: &CancellationToken,
    debug_raw: bool,
    emit: &Emit,
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
            Some(Err(BodyError::Stalled(idle))) => {
                return Read::Dropped {
                    text,
                    tool_call_in_progress: assembler.tool_call_in_progress(),
                    cause: DropCause::Stalled(idle),
                };
            }
            Some(Err(BodyError::Transport(_))) => Flow::Broken,
            Some(Ok(data)) => assembler.on_data(&data, &mut out),
        };
        if out.iter().any(|event| guard.check(event)) {
            return Read::Degenerate { discarded: guard.streamed() };
        }
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
    let finished = assembler.finish(&mut out);
    emit.forward(&mut out, &mut text);
    match finished {
        Ok(()) => Read::Finished,
        Err(Incomplete) => Read::Dropped { text, tool_call_in_progress, cause: DropCause::Dropped },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> ResponsesOptions {
        ResponsesOptions {
            name: "t".into(),
            label: "TestAI".into(),
            base_url: "http://unused".into(),
            model: "gpt-x".into(),
            credentials: Arc::new(ApiKey("k".into())),
            headers: Vec::new(),
            reasoning_effort: None,
            encrypted_reasoning: false,
            max_output_tokens: None,
            tool_namespace: None,
            context_length: None,
            hints: ErrorHints::default(),
            timing: RetryTiming::default(),
            idle_timeout: None,
            prompt_cache_key: None,
        }
    }

    fn history() -> Vec<ChatTurn> {
        vec![
            ChatTurn::User { text: "hi".into() },
            ChatTurn::Assistant {
                text: "".into(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "read_file".into(),
                    arguments: "{\"path\": \"a.ts\"}".into(),
                }],
                reasoning: Some(marv_core::Reasoning(serde_json::json!([
                    {"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "enc"},
                    {"type": "reasoning.text", "text": "OpenRouter's, from before a switch"}
                ]))),
            },
            ChatTurn::Tool { call_id: "c1".into(), name: "read_file".into(), text: "contents".into() },
            ChatTurn::Assistant { text: "done".into(), tool_calls: vec![], reasoning: None },
        ]
    }

    #[test]
    fn history_becomes_input_items_with_raw_arguments_and_reasoning_without_ids() {
        assert_eq!(
            serde_json::to_string(&to_input(&history(), None)).unwrap(),
            concat!(
                r#"[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},"#,
                r#"{"encrypted_content":"enc","summary":[],"type":"reasoning"},"#,
                r#"{"type":"function_call","call_id":"c1","name":"read_file","arguments":"{\"path\": \"a.ts\"}"},"#,
                r#"{"type":"function_call_output","call_id":"c1","output":"contents"},"#,
                r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}]"#
            )
        );
    }

    /// #159 review: the ChatGPT route takes function tools only in a namespace; the history's calls name it.
    #[test]
    fn tools_can_go_in_a_namespace_and_calls_carry_it() {
        let tools = vec![ToolSpec { name: "t".into(), description: "d".into(), parameters: serde_json::json!({}) }];
        let o = ResponsesOptions { tool_namespace: Some(MARV_TOOLS), ..options() };
        let body = serde_json::to_value(request_body(&o, &history(), None, &tools, ToolChoice::Auto)).unwrap();
        assert_eq!(
            body["tools"],
            serde_json::json!([{"type": "namespace", "name": "marv", "description": MARV_TOOLS.description,
                "tools": [{"type": "function", "name": "t", "description": "d", "parameters": {}, "strict": false}]}])
        );
        let call = body["input"].as_array().unwrap().iter().find(|i| i["type"] == "function_call").unwrap();
        assert_eq!(call["namespace"], "marv");
        // No tools, no empty namespace; flat calls carry no namespace.
        let body = serde_json::to_value(request_body(&o, &[], None, &[], ToolChoice::Auto)).unwrap();
        assert_eq!(body.get("tools"), None);
        let flat = serde_json::to_string(&to_input(&history(), None)).unwrap();
        assert!(!flat.contains("namespace"));
    }

    #[test]
    fn the_body_has_instructions_flat_tools_store_false_and_no_unset_fields() {
        let tools = vec![ToolSpec { name: "t".into(), description: "d".into(), parameters: serde_json::json!({}) }];
        let o = options();
        let body = serde_json::to_string(&request_body(&o, &[], Some("SYS"), &tools, ToolChoice::Auto)).unwrap();
        assert_eq!(
            body,
            r#"{"model":"gpt-x","instructions":"SYS","input":[],"tools":[{"type":"function","name":"t","description":"d","parameters":{},"strict":false}],"store":false,"stream":true}"#
        );
        let o = ResponsesOptions {
            reasoning_effort: Some(ReasoningEffort::Medium),
            encrypted_reasoning: true,
            max_output_tokens: Some(100),
            prompt_cache_key: Some("marv-k".into()),
            ..options()
        };
        let body = serde_json::to_string(&request_body(&o, &[], None, &tools, ToolChoice::None)).unwrap();
        assert!(
            body.ends_with(r#""tool_choice":"none","reasoning":{"effort":"medium","summary":"auto"},"store":false,"stream":true,"include":["reasoning.encrypted_content"],"prompt_cache_key":"marv-k","max_output_tokens":100}"#),
            "{body}"
        );
        assert!(!body.contains("instructions") && !body.contains("temperature"));
    }

    fn run(chunks: &[&str]) -> (Vec<ProviderEvent>, Result<(), Incomplete>) {
        let mut assembler = ReplyAssembler::new("TestAI");
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

    const COMPLETED: &str = r#"{"type":"response.completed","response":{"usage":{"input_tokens":100,"output_tokens":5,"input_tokens_details":{"cached_tokens":80}}}}"#;

    #[test]
    fn streams_text_and_thinking_and_ends_with_usage_and_done() {
        let (events, result) = run(&[
            r#"{"type":"response.created","response":{}}"#,
            r#"{"type":"response.reasoning_summary_text.delta","delta":"hm"}"#,
            r#"{"type":"response.output_text.delta","delta":"Hel"}"#,
            r#"{"type":"response.output_text.delta","delta":"lo"}"#,
            COMPLETED,
        ]);
        assert_eq!(result, Ok(()));
        assert_eq!(
            events,
            vec![
                ProviderEvent::ThinkingDelta("hm".into()),
                ProviderEvent::TextDelta("Hel".into()),
                ProviderEvent::TextDelta("lo".into()),
                ProviderEvent::Usage(
                    Usage { prompt_tokens: 100, completion_tokens: 5, cached_tokens: Some(80), cost: None },
                    ReplyEnd { finish_reason: Some("stop".into()), raw_response: None }
                ),
                ProviderEvent::Done { reason: Some("stop".into()) },
            ]
        );
    }

    #[test]
    fn assembles_function_calls_and_keeps_reasoning_items() {
        let (events, result) = run(&[
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1"}}"#,
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"enc"}}"#,
            r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_a","name":"read_file","arguments":""}}"#,
            r#"{"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"path\":"}"#,
            r#"{"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","call_id":"call_b","name":"glob"}}"#,
            r#"{"type":"response.function_call_arguments.delta","output_index":1,"delta":"\"a.ts\"}"}"#,
            r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","call_id":"call_a","name":"read_file","arguments":"{\"path\":\"a.ts\"}"}}"#,
            r#"{"type":"response.output_item.done","output_index":2,"item":{"type":"function_call","call_id":"call_b","name":"glob","arguments":"{}"}}"#,
            COMPLETED,
        ]);
        assert_eq!(result, Ok(()));
        assert_eq!(events[0], ProviderEvent::ToolCallDelta { index: 0, name: "read_file".into(), text: "".into() });
        assert_eq!(events[2], ProviderEvent::ToolCallDelta { index: 1, name: "glob".into(), text: "".into() });
        let calls: Vec<_> =
            events.iter().filter_map(|e| if let ProviderEvent::ToolCall(c) = e { Some(c.clone()) } else { None }).collect();
        assert_eq!(
            calls,
            vec![
                ToolCall { id: "call_a".into(), name: "read_file".into(), arguments: r#"{"path":"a.ts"}"#.into() },
                ToolCall { id: "call_b".into(), name: "glob".into(), arguments: "{}".into() },
            ]
        );
        assert!(events.iter().any(|e| matches!(e, ProviderEvent::Reasoning(r) if r.0[0]["encrypted_content"] == "enc")));
        assert_eq!(events.last(), Some(&ProviderEvent::Done { reason: Some("tool_calls".into()) }));
    }

    #[test]
    fn an_incomplete_reply_at_the_output_limit_ends_with_length() {
        let (events, result) = run(&[
            r#"{"type":"response.output_text.delta","delta":"half"}"#,
            r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}"#,
        ]);
        assert_eq!(result, Ok(()));
        assert_eq!(events.last(), Some(&ProviderEvent::Done { reason: Some("length".into()) }));
    }

    #[test]
    fn a_stream_without_completed_needs_a_retry() {
        let (_, result) = run(&[r#"{"type":"response.output_text.delta","delta":"Writing the fi"}"#]);
        assert_eq!(result, Err(Incomplete));
        let (_, result) = run(&[r#"{"type":"response.output_text.delta","delta":"x"}"#, "not json"]);
        assert_eq!(result, Err(Incomplete));
    }

    #[test]
    fn a_failed_response_is_the_turns_error_unless_a_retry_may_get_past_it() {
        let (events, _) =
            run(&[r#"{"type":"response.failed","response":{"error":{"code":"invalid_prompt","message":"Bad input"}}}"#]);
        assert_eq!(events.last(), Some(&ProviderEvent::Error("TestAI: Bad input".into())));
        let (events, _) = run(&[r#"{"type":"error","code":"bad","message":"Nope"}"#]);
        assert_eq!(events.last(), Some(&ProviderEvent::Error("TestAI: Nope".into())));
        // #264: the current form nests the details under `error`.
        let (events, _) = run(&[r#"{"type":"error","error":{"type":"invalid_request_error","code":"bad","message":"Nope"}}"#]);
        assert_eq!(events.last(), Some(&ProviderEvent::Error("TestAI: Nope".into())));
        // #159: the plan's limit is a hard stop even though its message says to try again.
        let (events, result) = run(&[
            r#"{"type":"response.failed","response":{"error":{"code":"subscription_sharing_usage_limit_exceeded","message":"Usage limit reached, try again later"}}}"#,
        ]);
        assert_eq!(result, Ok(()));
        assert!(
            matches!(events.last(), Some(ProviderEvent::Error(m)) if m.starts_with("TestAI: the usage limit for Marv on your ChatGPT plan is reached"))
        );
        let (events, result) = run(&[
            r#"{"type":"response.failed","response":{"error":{"code":"server_error","message":"The server is overloaded"}}}"#,
        ]);
        assert_eq!(result, Err(Incomplete));
        assert!(!events.iter().any(|e| matches!(e, ProviderEvent::Error(_))));
    }

    /// #264: the plan's usage limit as the ChatGPT route really sends it (captured on 0.13.0): an `error` event, after
    /// the 200, with the details nested. It was "ChatGPT: unknown error"; now it's the plan-limit message, not retried.
    #[test]
    fn the_plans_limit_in_a_nested_error_event_is_the_plan_limit_message() {
        let (events, result) = run(&[
            r#"{"type":"error","error":{"type":"invalid_request_error","code":"subscription_sharing_usage_limit_exceeded","message":"The ChatGPT user has reached their Subscription Sharing usage limit. Ask the user to try again after their usage limit resets or use an API key instead.","param":null},"sequence_number":3}"#,
        ]);
        assert_eq!(result, Ok(()), "a hard stop, not a retry");
        assert!(
            matches!(events.last(), Some(ProviderEvent::Error(m)) if m.starts_with("TestAI: the usage limit for Marv on your ChatGPT plan is reached")),
            "{events:?}"
        );
        assert!(!events.iter().any(|e| matches!(e, ProviderEvent::Error(m) if m.contains("unknown error"))));
    }

    /// #265: every function tool says `strict: false`, flat or in a namespace, so optional fields stay optional.
    #[test]
    fn every_tool_is_sent_not_strict() {
        let tools = vec![
            ToolSpec { name: "a".into(), description: "d".into(), parameters: serde_json::json!({"type": "object"}) },
            ToolSpec { name: "b".into(), description: "d".into(), parameters: serde_json::json!({"type": "object"}) },
        ];
        for ns in [None, Some(MARV_TOOLS)] {
            let o = ResponsesOptions { tool_namespace: ns, ..options() };
            let body = serde_json::to_value(request_body(&o, &[], None, &tools, ToolChoice::Auto)).unwrap();
            let fns: Vec<&Value> = match ns {
                None => body["tools"].as_array().unwrap().iter().collect(),
                Some(_) => body["tools"][0]["tools"].as_array().unwrap().iter().collect(),
            };
            assert_eq!(fns.len(), 2);
            assert!(fns.iter().all(|f| f["strict"] == Value::Bool(false)), "{body}");
        }
    }
}
