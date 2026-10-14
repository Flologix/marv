//! The adapter for Ollama's native chat API (`POST /api/chat`, NDJSON). Ports `src/provider/ollama.ts`.
//!
//! Native, not Ollama's OpenAI-compatible endpoint, because only the native one sets the context
//! window (`num_ctx`): Ollama's default of 4096 tokens is too small for an agent that reads files.
//!
//! - **`num_ctx`** = min(configured `context_length`, the length the model was trained for).
//!   `/api/show` (`model_info.<arch>.context_length`) is asked **once, before the first request**,
//!   so `num_ctx` is right from the start and never changes (a different `num_ctx` reloads the
//!   model). A failed or empty answer changes nothing and is asked again on the next stream
//!   (Ollama may have been starting up). Concurrent streams (the session and its subagents share
//!   the provider) ask only once. The configured value stays the upper bound: Ollama can't tell
//!   what fits in VRAM. Past the trained length Ollama shifts the window and loses the prompt's start.
//! - **`num_predict`** ([`reply_cap`]): a quarter of the window, at most 8192 (half / 16384 with
//!   thinking on: the cap counts reasoning). Once a reply reported `prompt_eval_count`, also at
//!   most the window minus the next prompt's estimate (its JSON characters × the **highest**
//!   tokens-per-character measured, + 5%), never below 1024. A ratio, not the last count: the
//!   provider is shared by conversations of different sizes. The highest: with a cached prefix
//!   Ollama may count only the new tokens, which only ever lowers it. It's an option, not part of
//!   the prompt, so changing it per request keeps the cache.
//! - **`presence_penalty: 0`**: qwen3.5's Modelfile sets 1.5, which punishes every reused token;
//!   code reuses tokens constantly, so files came out garbled and cut off and tool calls unclosed.
//! - **No client timeout**: Ollama sends nothing while the model writes a tool call; Bun's 300 s
//!   default cut long calls off. `num_predict` bounds the wait and cancel still stops it.
//! - A 500 saying Ollama couldn't parse the tool call ([`UNPARSABLE_CALL`]) is retried **once**
//!   (a re-sample usually comes out whole); other 500s (out of memory) aren't. That retry doesn't
//!   count toward the connection retries and sends no `Retry` event.
//! - When the cap cuts a tool call off, Ollama answers `done_reason: "length"` and drops the call.
//! - Tool calls come whole (arguments as an object): stored as JSON text like every other
//!   provider. Ollama numbers each step's calls from `call_0`: the agent loop de-duplicates ids.
//! - Without its `done` line the reply is partial (a proxy closed the stream): retry.
//! - A base URL with a trailing `/v1` (older configs) is accepted.
//! - #57: Ollama has no reasoning round-trip to keep; #59: no `tool_choice` (the note alone).
//! - #64 (marv-rs, not in Marv yet): a drop after text arrived, before any tool call, continues
//!   from the partial text like [`crate::openai_compat`] does; jitter on the backoff waits too.
//!   Tool calls arrive whole but are only sent with the `done` line, so a drop after one starts over.
//! - #90 (marv-rs): the `done` line carries usage and `done_reason` together, so the Usage event
//!   sends both ([`marv_core::ReplyEnd`]); with `StreamOptions::debug_raw`, the NDJSON lines are
//!   collected and sent with them (the trajectory's `request` record).

use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use futures::StreamExt;
use marv_core::{ChatTurn, ProviderEvent, ReplyEnd, ToolCall, ToolSpec, Usage};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::degenerate::DegenerateGuard;
use crate::errors::{ErrorHints, http_error, unreachable};
use crate::openai_compat::{
    DropCause, Emit, Incomplete, Read, after_degenerate, backoff, channel_stream, is_network_failure, keep_or_restart,
    read_error, root_cause, status_wait, until_cancelled, with_continuation,
};
use crate::retry::{MAX_ATTEMPTS, RetryTiming, retriable_http_status, sleep_abortable};
use crate::{EventStream, Provider, StreamOptions};

pub const LABEL: &str = "Ollama";
/// The most a reply may be (`num_predict`), whatever the window; twice that with thinking on.
pub const MAX_REPLY_TOKENS: u64 = 8192;
/// The least a reply gets, however full the window: below this a model can't finish even a short answer.
pub const MIN_REPLY_TOKENS: u64 = 1024;
/// Headroom on the prompt estimate: tokens per character differ between what was measured and what was added.
pub const PROMPT_MARGIN: f64 = 1.05;
/// Shown when Ollama can't be reached.
pub const OFFLINE_HINT: &str = "Is Ollama running? Start it with `ollama serve`.";

/// How Ollama says it couldn't parse the tool call a model wrote ("qwen tool call parsing failed: XML syntax error…").
pub static UNPARSABLE_CALL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)tool call|pars(e|ing)|XML syntax|unexpected EOF").unwrap());

#[derive(Debug, Clone, Default)]
pub struct OllamaOptions {
    /// Where Ollama listens, e.g. `http://localhost:11434` (a trailing `/v1` is ignored).
    pub base_url: String,
    pub model: String,
    /// Window to load the model with; the upper bound (never past the trained length).
    pub context_length: u64,
    /// Let thinking models reason first. Off makes them answer directly.
    pub thinking: bool,
    pub hints: ErrorHints,
    pub timing: RetryTiming,
}

/// `http://host:11434/v1/` → `http://host:11434`.
pub fn normalize_base_url(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    let trimmed = trimmed.strip_suffix("/v1").unwrap_or(trimmed);
    trimmed.trim_end_matches('/').to_string()
}

/// One line of Ollama's stream (only the fields Marv reads).
#[derive(Debug, Default, Deserialize)]
pub struct Line {
    #[serde(default)]
    pub message: Option<LineMessage>,
    #[serde(default)]
    pub done: bool,
    #[serde(default)]
    pub done_reason: Option<String>,
    #[serde(default)]
    pub prompt_eval_count: Option<u64>,
    #[serde(default)]
    pub eval_count: Option<u64>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct LineMessage {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub thinking: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<LineToolCall>,
}

#[derive(Debug, Deserialize)]
pub struct LineToolCall {
    #[serde(default)]
    pub id: Option<String>,
    pub function: LineFunction,
}

#[derive(Debug, Deserialize)]
pub struct LineFunction {
    pub name: String,
    #[serde(default)]
    pub arguments: Option<Value>,
}

/// Ollama streams newline-delimited JSON: one complete object per line. Bytes are buffered until
/// a newline (a chunk can end mid-line, even mid-character); blank lines are skipped.
#[derive(Debug, Default)]
pub struct NdjsonDecoder {
    buffer: Vec<u8>,
}

impl NdjsonDecoder {
    /// Adds a chunk; returns every complete, non-blank line.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(newline) = self.buffer.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=newline).collect();
            let line = String::from_utf8_lossy(&line).trim().to_string();
            if !line.is_empty() {
                out.push(line);
            }
        }
        out
    }

    /// The stream ended: a last line without its newline.
    pub fn finish(self) -> Option<String> {
        let line = String::from_utf8_lossy(&self.buffer).trim().to_string();
        (!line.is_empty()).then_some(line)
    }
}

// ---- Request (wire) types. Field order is byte order. ----

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum WireMessage {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<WireToolCall>>,
    },
    /// Results are tagged by tool name, not call id.
    Tool {
        tool_name: String,
        content: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WireToolCall {
    pub function: WireFunction,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WireFunction {
    pub name: String,
    /// Ollama wants an object: the raw text is parsed (`{}` when it isn't valid JSON).
    pub arguments: Value,
}

/// Marv's conversation in Ollama's format (tool arguments as objects, results tagged by tool name).
pub fn to_messages(history: &[ChatTurn], system: Option<&str>) -> Vec<WireMessage> {
    let parse = |args: &str| serde_json::from_str::<Value>(args).unwrap_or_else(|_| Value::Object(Default::default()));
    system
        .map(|s| WireMessage::System { content: s.to_string() })
        .into_iter()
        .chain(history.iter().map(|turn| match turn {
            ChatTurn::User { text } => WireMessage::User { content: text.clone() },
            ChatTurn::Assistant { text, tool_calls, .. } => WireMessage::Assistant {
                content: text.clone(),
                tool_calls: (!tool_calls.is_empty()).then(|| {
                    tool_calls
                        .iter()
                        .map(|c| WireToolCall { function: WireFunction { name: c.name.clone(), arguments: parse(&c.arguments) } })
                        .collect()
                }),
            },
            ChatTurn::Tool { name, text, .. } => WireMessage::Tool { tool_name: name.clone(), content: text.clone() },
        }))
        .collect()
}

/// The prompt part of the request (`messages` + `tools`): also what [`reply_cap`] measures in characters.
#[derive(Debug, Clone, Serialize)]
pub struct Prompt<'a> {
    pub messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<crate::openai_compat::WireTool<'a>>,
}

impl<'a> Prompt<'a> {
    pub fn new(history: &[ChatTurn], system: Option<&str>, tools: &'a [ToolSpec]) -> Self {
        Self {
            messages: to_messages(history, system),
            tools: tools.iter().map(|t| crate::openai_compat::WireTool { kind: "function", function: t }).collect(),
        }
    }
}

/// The longest trained context length in `/api/show`'s `model_info` (keys ending in
/// `.context_length` with a positive number), or `None` when it says nothing usable.
pub fn trained_context(model_info: &serde_json::Map<String, Value>) -> Option<u64> {
    model_info
        .iter()
        .filter(|(key, _)| key.ends_with(".context_length"))
        .filter_map(|(_, value)| value.as_u64())
        .filter(|&n| n > 0)
        .max()
}

/// What the adapter has learned about the model, shared by every stream of this provider.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Measured {
    /// The most tokens per character of prompt JSON measured so far (`prompt_eval_count` / size).
    pub tokens_per_char: Option<f64>,
    /// The trained context length, once `/api/show` answered.
    pub trained: Option<u64>,
}

impl Measured {
    /// The window the model runs at: the configured one, never past the trained length.
    pub fn window(&self, configured: u64) -> u64 {
        self.trained.map_or(configured, |t| configured.min(t))
    }

    /// Records a finished reply's prompt count. Keeps the highest ratio; a reply without a count
    /// keeps the last measurement.
    pub fn record(&mut self, prompt_eval_count: Option<u64>, chars: usize) {
        if let Some(count) = prompt_eval_count.filter(|&c| c > 0) {
            let ratio = count as f64 / chars as f64;
            self.tokens_per_char = Some(self.tokens_per_char.map_or(ratio, |r| r.max(ratio)));
        }
    }
}

/// `num_predict` for a prompt of `chars` characters (see the module notes).
pub fn reply_cap(window: u64, thinking: bool, tokens_per_char: Option<f64>, chars: usize) -> u64 {
    let share = if thinking { (2 * MAX_REPLY_TOKENS).min(window / 2) } else { MAX_REPLY_TOKENS.min(window / 4) };
    let Some(ratio) = tokens_per_char.filter(|r| *r > 0.0) else { return share };
    let prompt = (chars as f64 * ratio * PROMPT_MARGIN).ceil() as i64;
    // A nearly full window still gets a short reply; the 85% warning and compaction make room before the next turn.
    let room = (window as i64 - prompt).max(0) as u64;
    share.min(MIN_REPLY_TOKENS).max(share.min(room))
}

/// The whole `/api/chat` body: the settings, then the prompt.
#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest<'a> {
    pub model: &'a str,
    pub stream: bool,
    pub think: bool,
    pub options: ChatOptions,
    #[serde(flatten)]
    pub prompt: &'a Prompt<'a>,
}

/// Ollama's `options`, in this order.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ChatOptions {
    /// Must stay the same for the whole session: a different num_ctx reloads the model.
    pub num_ctx: u64,
    pub num_predict: u64,
    pub presence_penalty: u32,
}

/// Turns Ollama's NDJSON lines into [`ProviderEvent`]s. Pure.
#[derive(Debug, Default)]
pub struct LineAssembler {
    calls: Vec<ToolCall>,
    reason: Option<String>,
    complete: bool,
    /// #90: the raw response lines, when debug recording is on (`with_debug`); sent on the
    /// Usage event at the `done` line.
    raw: Option<Vec<String>>,
    /// The prompt count of the `done` line, for [`Measured::record`].
    pub prompt_eval_count: Option<u64>,
}

impl LineAssembler {
    pub fn new() -> Self {
        Self::with_debug(false)
    }

    /// `debug_raw` (#90): also collect the raw response lines (sent in the Usage event).
    pub fn with_debug(debug_raw: bool) -> Self {
        Self { raw: debug_raw.then(Vec::new), ..Default::default() }
    }

    /// Handles one line. Returns `false` when the stream must stop here (an error event was
    /// emitted). A line that isn't valid JSON is `Err` (treated like a dropped connection: retry).
    pub fn on_line(&mut self, line: &str, out: &mut Vec<ProviderEvent>) -> Result<bool, serde_json::Error> {
        if let Some(raw) = &mut self.raw {
            raw.push(line.to_string());
        }
        let line: Line = serde_json::from_str(line)?;
        if let Some(error) = line.error {
            out.push(ProviderEvent::Error(format!("{LABEL}: {error}")));
            return Ok(false);
        }
        if let Some(message) = line.message {
            if let Some(t) = message.thinking.filter(|t| !t.is_empty()) {
                out.push(ProviderEvent::ThinkingDelta(t));
            }
            if let Some(t) = message.content.filter(|t| !t.is_empty()) {
                out.push(ProviderEvent::TextDelta(t));
            }
            for call in message.tool_calls {
                let id = call.id.unwrap_or_else(|| format!("call_{}", self.calls.len()));
                let arguments = call.function.arguments.unwrap_or_else(|| Value::Object(Default::default())).to_string();
                self.calls.push(ToolCall { id, name: call.function.name, arguments });
            }
        }
        if line.done {
            self.complete = true;
            self.reason = line.done_reason;
            self.prompt_eval_count = line.prompt_eval_count;
            out.push(ProviderEvent::Usage(
                Usage {
                    prompt_tokens: line.prompt_eval_count.unwrap_or(0),
                    completion_tokens: line.eval_count.unwrap_or(0),
                    cached_tokens: None,
                    cost: None,
                },
                // The done line carries usage and done_reason together (#90); the raw lines join them.
                ReplyEnd {
                    finish_reason: self.reason.clone(),
                    raw_response: self.raw.take().as_ref().map(|raw| raw.join("\n")),
                },
            ));
        }
        Ok(true)
    }

    /// #64: a tool call already arrived (they're sent only with `done`, so a drop now starts over).
    pub fn tool_call_in_progress(&self) -> bool {
        !self.calls.is_empty()
    }

    /// The body ended: the calls and `Done`, or [`Incomplete`] when the `done` line never came (retry).
    pub fn finish(self, out: &mut Vec<ProviderEvent>) -> Result<(), Incomplete> {
        if !self.complete {
            return Err(Incomplete);
        }
        out.extend(self.calls.into_iter().map(ProviderEvent::ToolCall));
        out.push(ProviderEvent::Done { reason: self.reason });
        Ok(())
    }
}

pub struct OllamaProvider {
    pub options: OllamaOptions,
    name: String,
    shared: Arc<Shared>,
    pub client: reqwest::Client,
}

/// What every stream of one provider shares (the session and its subagents use the same one).
#[derive(Default)]
struct Shared {
    measured: Mutex<Measured>,
    /// Held while `/api/show` is in flight, so concurrent streams ask only once.
    showing: tokio::sync::Mutex<()>,
}

impl Shared {
    fn measured(&self) -> Measured {
        *self.measured.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn update(&self, f: impl FnOnce(&mut Measured)) {
        f(&mut self.measured.lock().unwrap_or_else(|e| e.into_inner()));
    }
}

impl OllamaProvider {
    pub fn new(options: OllamaOptions) -> Self {
        let name = format!("ollama · {}", options.model);
        // No overall or read timeout (see the module notes): Ollama sends nothing while the model
        // writes a tool call. num_predict bounds the wait (minutes on a GPU, longer on a CPU), and
        // cancel still stops it. A connect timeout is fine.
        let client = reqwest::Client::builder().connect_timeout(Duration::from_secs(30)).build().unwrap_or_default();
        Self { options, name, shared: Arc::default(), client }
    }

    /// Asks `/api/show` once what the model was trained for (see the module notes). Never fails:
    /// a failure leaves the window as configured and is asked again next stream.
    pub async fn ask_trained(&self, base_url: &str, cancel: &CancellationToken) {
        ask_trained(&self.client, &self.shared, base_url, &self.options.model, cancel).await;
    }
}

#[derive(Serialize)]
struct ShowRequest<'a> {
    model: &'a str,
}

#[derive(Deserialize)]
struct ShowResponse {
    #[serde(default)]
    model_info: Option<serde_json::Map<String, Value>>,
}

/// Asked before the first request so num_ctx is right from the start and never changes (a
/// different num_ctx reloads the model). A successful answer is kept; a failed or empty one is
/// asked again on the next stream (Ollama may have been starting up). A server that never
/// answers changes nothing.
async fn ask_trained(client: &reqwest::Client, shared: &Shared, base_url: &str, model: &str, cancel: &CancellationToken) {
    let Some(_asking) = until_cancelled(cancel, shared.showing.lock()).await else { return };
    // Another stream may have asked while this one waited.
    if shared.measured().trained.is_some() {
        return;
    }
    let ask = async {
        let response = client.post(format!("{base_url}/api/show")).json(&ShowRequest { model }).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        // No /api/show (an old Ollama, a proxy that only forwards /api/chat): run at the configured window.
        trained_context(&response.json::<ShowResponse>().await.ok()?.model_info?)
    };
    // The stream's cancel stops this request too; that just leaves the window as configured.
    if let Some(Some(trained)) = until_cancelled(cancel, ask).await {
        shared.update(|m| m.trained = Some(trained));
    }
}

impl Provider for OllamaProvider {
    fn name(&self) -> &str {
        &self.name
    }

    /// What the model actually runs at, once `/api/show` answered (the usage bar, the 85% warning
    /// and compaction measure against it); the configured window until then.
    fn context_length(&self) -> Option<u64> {
        Some(self.shared.measured().window(self.options.context_length))
    }

    fn stream(&self, history: &[ChatTurn], options: StreamOptions) -> EventStream {
        // Copied now, so the caller can append to its history while the reply streams.
        let outgoing = Outgoing { history: history.to_vec(), system: options.system, tools: options.tools, note: None };
        let debug_raw = options.debug_raw;
        let (client, shared, adapter, cancel) = (self.client.clone(), self.shared.clone(), self.options.clone(), options.cancel);
        channel_stream(move |emit| run(client, shared, adapter, outgoing, cancel, debug_raw, emit))
    }
}

/// What one stream sends, owned so the returned stream is `'static`.
struct Outgoing {
    history: Vec<ChatTurn>,
    system: Option<String>,
    tools: Vec<ToolSpec>,
    /// #96: sent after the history once a reply degenerated.
    note: Option<&'static str>,
}

impl Outgoing {
    /// The body of the next attempt and its prompt's size in characters (what [`reply_cap`] and
    /// [`Measured::record`] measure). #64: with the kept partial reply and the note, if any.
    fn body(&mut self, o: &OllamaOptions, measured: Measured, kept: &str) -> (Vec<u8>, usize) {
        let Self { history, system, tools, note } = self;
        with_continuation(history, *note, kept, |history| {
            let prompt = Prompt::new(history, system.as_deref(), tools);
            // UTF-16 units, as Marv's `JSON.stringify(prompt).length` counts them.
            let chars = serde_json::to_string(&prompt).expect("a prompt is plain JSON").encode_utf16().count();
            let window = measured.window(o.context_length);
            let body = ChatRequest {
                model: &o.model,
                stream: true,
                think: o.thinking,
                options: ChatOptions {
                    num_ctx: window,
                    // A cap on each reply. Without one, a model that loses its way can generate for minutes (11k
                    // tokens were measured), and Ollama sends nothing while it writes a tool call. It changes per
                    // request, but it's an option, not part of the prompt: the cache holds.
                    num_predict: reply_cap(window, o.thinking, measured.tokens_per_char, chars),
                    presence_penalty: 0,
                },
                prompt: &prompt,
            };
            (serde_json::to_vec(&body).expect("a request body is plain JSON"), chars)
        })
    }
}

/// The retry loop: network errors, retriable statuses (502, 503, 504, 429) and drops, up to
/// [`MAX_ATTEMPTS`] in all; and, apart from those, an unparsable tool call once.
async fn run(
    client: reqwest::Client,
    shared: Arc<Shared>,
    o: OllamaOptions,
    mut outgoing: Outgoing,
    cancel: CancellationToken,
    debug_raw: bool,
    emit: Emit,
) {
    let base_url = normalize_base_url(&o.base_url);
    // The trained context length is fetched once, before the first chat request, so num_ctx is right from the start.
    ask_trained(&client, &shared, &base_url, &o.model, &cancel).await;
    let url = format!("{base_url}/api/chat");
    let mut kept = String::new();
    // Counts only network/HTTP retries, not the tool-call parse retry (nor #96's degenerate one).
    let mut conn_retries = 0;
    let mut degenerate_retries = 0;
    let mut announce = false;
    for attempt in 1.. {
        if cancel.is_cancelled() {
            return;
        }
        if announce {
            emit.retry(LABEL, conn_retries, !kept.is_empty(), Default::default());
            announce = false;
        }

        let (body, chars) = outgoing.body(&o, shared.measured(), &kept);
        let request = client.post(&url).header(reqwest::header::CONTENT_TYPE, "application/json").body(body);
        let Some(sent) = until_cancelled(&cancel, request.send()).await else { return };
        let response = match sent {
            Ok(response) => response,
            Err(err) => {
                if conn_retries < MAX_ATTEMPTS - 1 && is_network_failure(&err) {
                    conn_retries += 1;
                    announce = true;
                    if sleep_abortable(backoff(o.timing, conn_retries), &cancel).await {
                        return;
                    }
                    continue;
                }
                emit.send(ProviderEvent::Error(unreachable(LABEL, &base_url, &root_cause(&err), Some(OFFLINE_HINT))));
                return;
            }
        };

        let status = response.status().as_u16();
        if response.status().is_success() {
            // Stream the NDJSON body. If the connection drops mid-stream, retry (#64: or continue).
            match read_reply(response, &shared, chars, &cancel, debug_raw, &emit).await {
                Read::Finished => return,
                Read::Degenerate { discarded } => {
                    if !after_degenerate(LABEL, discarded, &mut degenerate_retries, &mut kept, &mut outgoing.note, &emit) {
                        return;
                    }
                    continue;
                }
                Read::Dropped { text, tool_call_in_progress, cause } => {
                    keep_or_restart(&mut kept, &text, tool_call_in_progress);
                    if conn_retries < MAX_ATTEMPTS - 1 {
                        conn_retries += 1;
                        announce = true;
                        if sleep_abortable(backoff(o.timing, conn_retries), &cancel).await {
                            return;
                        }
                        continue;
                    }
                    emit.send(ProviderEvent::Error(cause.final_message(LABEL)));
                    return;
                }
            }
        }

        // HTTP-level error.
        if retriable_http_status(status) && conn_retries < MAX_ATTEMPTS - 1 {
            conn_retries += 1;
            announce = true;
            if sleep_abortable(status_wait(&response, o.timing, conn_retries), &cancel).await {
                return;
            }
            continue;
        }
        let Some(detail) = read_error(response, &cancel).await else { return };
        // An unparsable tool call: a re-sample usually comes out whole. Only once, and only first.
        if status >= 500 && UNPARSABLE_CALL.is_match(&detail) && attempt < 2 {
            continue;
        }
        emit.send(ProviderEvent::Error(http_error(status, &detail, LABEL, &o.model, o.hints)));
        return;
    }
}

/// Parses the NDJSON body, sending events as they come.
async fn read_reply(
    response: reqwest::Response,
    shared: &Shared,
    chars: usize,
    cancel: &CancellationToken,
    debug_raw: bool,
    emit: &Emit,
) -> Read {
    let mut assembler = LineAssembler::with_debug(debug_raw);
    let mut guard = DegenerateGuard::default();
    let mut decoder = NdjsonDecoder::default();
    let mut text = String::new();
    let mut out = Vec::new();
    let mut body = Box::pin(response.bytes_stream());
    loop {
        let Some(next) = until_cancelled(cancel, body.next()).await else { return Read::Finished };
        let (lines, ended) = match next {
            Some(Ok(chunk)) => (decoder.push(&chunk), false),
            None => (std::mem::take(&mut decoder).finish().into_iter().collect(), true),
            // A connection drop mid-stream is retriable.
            Some(Err(_)) => {
                return Read::Dropped {
                    text,
                    tool_call_in_progress: assembler.tool_call_in_progress(),
                    cause: DropCause::Dropped,
                };
            }
        };
        for line in lines {
            let handled = assembler.on_line(&line, &mut out);
            // #96: a reply that turned into whitespace or one repeated piece is aborted.
            if out.iter().any(|event| guard.check(event)) {
                return Read::Degenerate { discarded: guard.streamed() };
            }
            emit.forward(&mut out, &mut text);
            match handled {
                Ok(true) => {}
                Ok(false) => return Read::Finished,
                // Invalid JSON: like a drop (Marv's JSON.parse throws into the same catch).
                Err(_) => {
                    return Read::Dropped {
                        text,
                        tool_call_in_progress: assembler.tool_call_in_progress(),
                        cause: DropCause::Dropped,
                    };
                }
            }
        }
        if ended {
            break;
        }
    }
    let tool_call_in_progress = assembler.tool_call_in_progress();
    let prompt_eval_count = assembler.prompt_eval_count;
    match assembler.finish(&mut out) {
        Ok(()) => {
            shared.update(|m| m.record(prompt_eval_count, chars));
            emit.forward(&mut out, &mut text);
            Read::Finished
        }
        // Without its done line the reply is partial (a proxy closed the stream): retry.
        Err(Incomplete) => Read::Dropped { text, tool_call_in_progress, cause: DropCause::Dropped },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ported from tests/ollama.test.ts (the pure parts; the HTTP loop is tested with a server later).

    #[test]
    fn caps_each_reply_at_a_quarter_of_a_small_window_and_twice_that_with_thinking() {
        assert_eq!(reply_cap(8192, false, None, 100), 2048);
        assert_eq!(reply_cap(32768, true, None, 100), 16384);
        assert_eq!(reply_cap(32768, false, None, 100), 8192);
    }

    const CHARS: usize = 4000;

    fn measured(tokens: u64) -> Measured {
        let mut m = Measured::default();
        m.record(Some(tokens), CHARS);
        m
    }

    #[test]
    fn then_at_most_the_window_minus_the_estimated_prompt_with_5_percent_headroom() {
        // Same prompt: 20000 tokens, plus 5% → 21000; 32768 − 21000 leaves 11768.
        assert_eq!(reply_cap(32768, true, measured(20000).tokens_per_char, CHARS), 11768);
    }

    #[test]
    fn a_prompt_that_grew_is_estimated_at_the_measured_ratio() {
        let longer = CHARS + 2000;
        let estimate = (longer as f64 * (20000.0 / CHARS as f64) * 1.05).ceil() as u64;
        assert_eq!(reply_cap(32768, true, measured(20000).tokens_per_char, longer), 32768 - estimate);
    }

    #[test]
    fn a_nearly_full_window_still_leaves_a_short_reply() {
        assert_eq!(reply_cap(32768, false, measured(32000).tokens_per_char, CHARS), 1024);
    }

    #[test]
    fn with_room_to_spare_the_share_still_caps_it() {
        assert_eq!(reply_cap(32768, false, measured(1000).tokens_per_char, CHARS), 8192);
    }

    #[test]
    fn a_cache_hit_or_a_missing_count_does_not_lower_the_estimate() {
        let mut m = measured(20000);
        m.record(Some(50), CHARS);
        m.record(None, CHARS);
        assert_eq!(reply_cap(32768, true, m.tokens_per_char, CHARS), 11768);
    }

    #[test]
    fn num_ctx_is_the_configured_or_trained_window_whichever_is_smaller() {
        let m = Measured { trained: Some(16384), ..Default::default() };
        assert_eq!(m.window(32768), 16384);
        assert_eq!(m.window(8192), 8192);
        assert_eq!(Measured::default().window(32768), 32768);
    }

    #[test]
    fn trained_context_reads_model_info() {
        let info: serde_json::Map<String, Value> =
            serde_json::from_str(r#"{"general.architecture":"qwen3","qwen3.context_length":40960,"x.context_length":0}"#)
                .unwrap();
        assert_eq!(trained_context(&info), Some(40960));
        assert_eq!(trained_context(&Default::default()), None);
    }

    #[test]
    fn accepts_a_base_url_with_a_trailing_v1() {
        assert_eq!(normalize_base_url("http://localhost:11434/v1"), "http://localhost:11434");
        assert_eq!(normalize_base_url("http://localhost:11434/v1/"), "http://localhost:11434");
        assert_eq!(normalize_base_url("http://localhost:11434/"), "http://localhost:11434");
    }

    #[test]
    fn sends_history_in_ollama_format() {
        let history = vec![
            ChatTurn::Assistant {
                text: "".into(),
                tool_calls: vec![ToolCall { id: "c1".into(), name: "read_file".into(), arguments: r#"{"path":"a"}"#.into() }],
                reasoning: None,
            },
            ChatTurn::Tool { call_id: "c1".into(), name: "read_file".into(), text: "x".into() },
        ];
        assert_eq!(
            serde_json::to_string(&to_messages(&history, Some("S"))).unwrap(),
            r#"[{"role":"system","content":"S"},{"role":"assistant","content":"","tool_calls":[{"function":{"name":"read_file","arguments":{"path":"a"}}}]},{"role":"tool","tool_name":"read_file","content":"x"}]"#
        );
        assert_eq!(
            serde_json::to_string(&ChatOptions { num_ctx: 8192, num_predict: 2048, presence_penalty: 0 }).unwrap(),
            r#"{"num_ctx":8192,"num_predict":2048,"presence_penalty":0}"#
        );
    }

    #[test]
    fn streams_thinking_text_tool_calls_usage_and_done() {
        let mut decoder = NdjsonDecoder::default();
        let mut lines = decoder.push(b"{\"message\":{\"thinking\":\"hm\"}}\n{\"message\":{\"content\":\"hi\",\"tool_calls\":[{\"function\":{\"name\":\"glob\",\"arguments\":{\"p\":1}}}]}}\n{\"done\":true,");
        lines.extend(decoder.push(b"\"done_reason\":\"stop\",\"prompt_eval_count\":10,\"eval_count\":2}"));
        lines.extend(decoder.finish());
        let mut assembler = LineAssembler::default();
        let mut out = Vec::new();
        for line in &lines {
            assert!(assembler.on_line(line, &mut out).unwrap());
        }
        assembler.finish(&mut out).unwrap();
        assert_eq!(
            out,
            vec![
                ProviderEvent::ThinkingDelta("hm".into()),
                ProviderEvent::TextDelta("hi".into()),
                ProviderEvent::Usage(
                    Usage { prompt_tokens: 10, completion_tokens: 2, cached_tokens: None, cost: None },
                    ReplyEnd { finish_reason: Some("stop".into()), raw_response: None }
                ),
                ProviderEvent::ToolCall(ToolCall { id: "call_0".into(), name: "glob".into(), arguments: r#"{"p":1}"#.into() }),
                ProviderEvent::Done { reason: Some("stop".into()) },
            ]
        );
    }

    // #90: with debug recording on, the raw NDJSON lines ride the Usage event; without, raw_response stays None.
    #[test]
    fn the_raw_lines_are_collected_only_behind_the_debug_flag() {
        let lines = [r#"{"message":{"content":"hi"}}"#, r#"{"done":true,"done_reason":"stop","prompt_eval_count":10,"eval_count":2}"#];
        let mut off = LineAssembler::new();
        let mut out = Vec::new();
        for line in &lines {
            assert!(off.on_line(line, &mut out).unwrap());
        }
        off.finish(&mut out).unwrap();
        assert!(out.iter().all(|e| !matches!(e, ProviderEvent::Usage(_, end) if end.raw_response.is_some())));

        let mut on = LineAssembler::with_debug(true);
        let mut out = Vec::new();
        for line in &lines {
            assert!(on.on_line(line, &mut out).unwrap());
        }
        on.finish(&mut out).unwrap();
        let end =
            out.iter().find_map(|e| if let ProviderEvent::Usage(_, end) = e { Some(end) } else { None }).expect("the Usage event");
        assert_eq!(
            end,
            &ReplyEnd {
                finish_reason: Some("stop".into()),
                raw_response: Some(format!("{}\n{}", lines[0], lines[1]))
            }
        );
    }

    #[test]
    fn a_stream_without_its_done_line_needs_a_retry_and_errors_stop_it() {
        let mut assembler = LineAssembler::default();
        let mut out = Vec::new();
        assembler.on_line(r#"{"message":{"content":"part"}}"#, &mut out).unwrap();
        assert!(assembler.finish(&mut out).is_err());
        let mut assembler = LineAssembler::default();
        assert!(!assembler.on_line(r#"{"error":"model 'x' not found"}"#, &mut out).unwrap());
        assert_eq!(out.last(), Some(&ProviderEvent::Error("Ollama: model 'x' not found".into())));
    }

    #[test]
    fn unparsable_call_matches_ollamas_wording() {
        assert!(UNPARSABLE_CALL.is_match("qwen tool call parsing failed: XML syntax error"));
        assert!(!UNPARSABLE_CALL.is_match("out of memory"));
    }
}
