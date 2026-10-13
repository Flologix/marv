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

use std::sync::LazyLock;
use std::sync::Mutex;

use marv_core::{ChatTurn, ProviderEvent, ToolCall, ToolSpec, Usage};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::errors::ErrorHints;
use crate::openai_compat::Incomplete;
use crate::retry::RetryTiming;
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
    /// The prompt count of the `done` line, for [`Measured::record`].
    pub prompt_eval_count: Option<u64>,
}

impl LineAssembler {
    /// Handles one line. Returns `false` when the stream must stop here (an error event was
    /// emitted). A line that isn't valid JSON is `Err` (treated like a dropped connection: retry).
    pub fn on_line(&mut self, line: &str, out: &mut Vec<ProviderEvent>) -> Result<bool, serde_json::Error> {
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
            out.push(ProviderEvent::Usage(Usage {
                prompt_tokens: line.prompt_eval_count.unwrap_or(0),
                completion_tokens: line.eval_count.unwrap_or(0),
                cached_tokens: None,
                cost: None,
            }));
        }
        Ok(true)
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
    measured: Mutex<Measured>,
    /// Held while `/api/show` is in flight, so concurrent streams ask only once.
    showing: tokio::sync::Mutex<()>,
    pub client: reqwest::Client,
}

impl OllamaProvider {
    pub fn new(options: OllamaOptions) -> Self {
        let name = format!("ollama · {}", options.model);
        // Port note: no overall timeout (see the module notes); a connect timeout is fine.
        Self {
            options,
            name,
            measured: Mutex::new(Measured::default()),
            showing: tokio::sync::Mutex::new(()),
            client: reqwest::Client::new(),
        }
    }

    fn measured(&self) -> Measured {
        *self.measured.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Asks `/api/show` once what the model was trained for (see the module notes). Never fails:
    /// a failure leaves the window as configured and is asked again next stream.
    pub async fn ask_trained(&self, _base_url: &str, _cancel: &tokio_util::sync::CancellationToken) {
        let _guard = self.showing.lock().await;
        todo!("port: askTrained (POST /api/show {{model}}, trained_context, store in measured.trained)")
    }
}

impl Provider for OllamaProvider {
    fn name(&self) -> &str {
        &self.name
    }

    /// What the model actually runs at, once `/api/show` answered (the usage bar, the 85% warning
    /// and compaction measure against it); the configured window until then.
    fn context_length(&self) -> Option<u64> {
        Some(self.measured().window(self.options.context_length))
    }

    fn stream(&self, _history: &[ChatTurn], _options: StreamOptions) -> EventStream {
        todo!(
            "port: OllamaProvider.stream — ask_trained first; body {{model, stream, think, options: ChatOptions, ..Prompt}}; \
             retry loop (conn retries n/2, unparsable-call 500 once); NdjsonDecoder + LineAssembler; Measured::record"
        )
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
                ProviderEvent::Usage(Usage { prompt_tokens: 10, completion_tokens: 2, cached_tokens: None, cost: None }),
                ProviderEvent::ToolCall(ToolCall { id: "call_0".into(), name: "glob".into(), arguments: r#"{"p":1}"#.into() }),
                ProviderEvent::Done { reason: Some("stop".into()) },
            ]
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
