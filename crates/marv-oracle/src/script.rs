//! `script.jsonl`: what the fake provider replies, one line per `POST /chat/completions`.
//!
//! No Marv file ports to this: Marv's evals (`evals/run.ts`) call real models. The wire format it
//! produces is the one Marv's adapter reads (`src/provider/openai-compat.ts`, `src/provider/sse.ts`):
//! OpenAI-style `chat.completion.chunk` events over SSE, tool calls in fragments keyed by `index`,
//! a `finish_reason`, a usage chunk (`stream_options.include_usage`), then `data: [DONE]`.
//!
//! A line is a [`Reply`]. Everything is optional; `{}` is an empty reply that finishes with `stop`:
//!
//! ```json
//! {"text": "Done.", "reasoning": "…", "tool_calls": [{"id": "call_0", "name": "read_file", "arguments": {"path": "a.js"}}],
//!  "finish_reason": "length", "usage": {"prompt_tokens": 10, "completion_tokens": 5, "cached_tokens": 0, "cost": 0.001},
//!  "chunk": 40}
//! ```
//!
//! Faults, for the loop's edge cases:
//! - `"status": 429` (with `"retry_after": "1"`, `"body": "…"`): an HTTP error instead of a stream;
//! - `"drop": true`: the stream closes cleanly after the content but before any `finish_reason` or
//!   `[DONE]`, the way a proxy cuts a reply off (Marv retries it from scratch);
//! - `"stream_error": {"message": "…"}`: an `{"error": …}` chunk after the 200, as OpenRouter sends
//!   mid-stream.

use serde::Deserialize;
use serde_json::{Value, json};

/// Text is streamed in pieces of this many characters unless a reply says otherwise. Small enough
/// that Marv's text-loop check (every 400 characters while streaming) runs mid-reply.
pub const DEFAULT_CHUNK: usize = 50;

/// One scripted reply.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    #[serde(default)]
    pub text: String,
    /// Sent as `delta.reasoning` (OpenRouter's name for it).
    #[serde(default)]
    pub reasoning: String,
    #[serde(default)]
    pub tool_calls: Vec<ScriptCall>,
    /// Default: `tool_calls` with calls, else `stop`. `"length"` is a cut-off.
    pub finish_reason: Option<String>,
    /// Default: [`Usage::default`]. Every reply reports usage, so the result's totals are checked too.
    pub usage: Option<Usage>,
    /// Characters per text chunk (default [`DEFAULT_CHUNK`]).
    pub chunk: Option<usize>,
    /// Answer with this HTTP status and no stream.
    pub status: Option<u16>,
    /// The `Retry-After` header for a `status` reply.
    pub retry_after: Option<String>,
    /// The body of a `status` reply (default: an OpenAI-style error object).
    pub body: Option<String>,
    /// Close the stream after the content, with no `finish_reason` and no `[DONE]`.
    #[serde(default)]
    pub drop: bool,
    /// An error chunk sent after the content.
    pub stream_error: Option<Value>,
}

/// A scripted tool call. `id` may be left out (Marv then names it `call_<index>`), or repeated on
/// purpose: Ollama numbers each step's calls from `call_0`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptCall {
    pub id: Option<String>,
    pub name: String,
    /// A JSON string, sent as is, or an object, serialized compactly.
    pub arguments: Value,
}

impl ScriptCall {
    /// The arguments as the wire carries them: a string of JSON.
    pub fn arguments_text(&self) -> String {
        match &self.arguments {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    #[serde(default)]
    pub cached_tokens: u64,
    pub cost: Option<f64>,
}

impl Default for Usage {
    fn default() -> Self {
        Usage { prompt_tokens: 1000, completion_tokens: 50, cached_tokens: 0, cost: Some(0.001) }
    }
}

/// Parses a `script.jsonl`. Blank lines and lines starting with `//` are skipped, so a script can
/// say what each reply is for.
pub fn parse_script(text: &str) -> anyhow::Result<Vec<Reply>> {
    let mut replies = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        let reply: Reply = serde_json::from_str(line).map_err(|err| anyhow::anyhow!("script.jsonl line {}: {err}", n + 1))?;
        replies.push(reply);
    }
    Ok(replies)
}

/// What the server sends for a reply.
#[derive(Debug, Clone, PartialEq)]
pub enum Response {
    /// A non-200 answer.
    Status { status: u16, retry_after: Option<String>, body: String },
    /// A 200 with these SSE `data:` payloads, in order.
    Stream(Vec<String>),
}

impl Reply {
    /// The response for this reply. `model` is echoed in each chunk, as real servers do.
    pub fn response(&self, model: &str) -> Response {
        if let Some(status) = self.status {
            let body = self.body.clone().unwrap_or_else(|| {
                json!({"error": {"message": format!("oracle: scripted HTTP {status}"), "code": status}}).to_string()
            });
            return Response::Status { status, retry_after: self.retry_after.clone(), body };
        }
        let chunk = |delta: Value, finish: Option<&str>| {
            json!({
                "id": "chatcmpl-oracle",
                "object": "chat.completion.chunk",
                "model": model,
                "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
            })
            .to_string()
        };
        let mut events = Vec::new();
        let size = self.chunk.unwrap_or(DEFAULT_CHUNK).max(1);
        for piece in pieces(&self.reasoning, size) {
            events.push(chunk(json!({"role": "assistant", "reasoning": piece}), None));
        }
        for piece in pieces(&self.text, size) {
            events.push(chunk(json!({"role": "assistant", "content": piece}), None));
        }
        for (index, call) in self.tool_calls.iter().enumerate() {
            // The first fragment names the call, the next carries its arguments: the shape that makes
            // Marv's adapter tell the screen about a call before its arguments arrive.
            let mut head = json!({"index": index, "type": "function", "function": {"name": call.name, "arguments": ""}});
            if let Some(id) = &call.id {
                head["id"] = json!(id);
            }
            events.push(chunk(json!({"tool_calls": [head]}), None));
            events.push(chunk(json!({"tool_calls": [{"index": index, "function": {"arguments": call.arguments_text()}}]}), None));
        }
        if let Some(error) = &self.stream_error {
            events.push(json!({"error": error}).to_string());
            return Response::Stream(events);
        }
        if self.drop {
            return Response::Stream(events);
        }
        let finish =
            self.finish_reason.clone().unwrap_or_else(|| if self.tool_calls.is_empty() { "stop" } else { "tool_calls" }.into());
        events.push(chunk(json!({}), Some(&finish)));
        let usage = self.usage.clone().unwrap_or_default();
        let mut usage_json = json!({
            "prompt_tokens": usage.prompt_tokens,
            "completion_tokens": usage.completion_tokens,
            "total_tokens": usage.prompt_tokens + usage.completion_tokens,
            "prompt_tokens_details": {"cached_tokens": usage.cached_tokens},
        });
        if let Some(cost) = usage.cost {
            usage_json["cost"] = json!(cost);
        }
        events.push(json!({"id": "chatcmpl-oracle", "object": "chat.completion.chunk", "model": model, "choices": [], "usage": usage_json}).to_string());
        events.push("[DONE]".into());
        Response::Stream(events)
    }
}

/// `text` in pieces of at most `size` characters (never splitting a character).
fn pieces(text: &str, size: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    chars.chunks(size).map(|c| c.iter().collect()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(reply: &Reply) -> Vec<Value> {
        match reply.response("m") {
            Response::Stream(events) => events.iter().map(|e| serde_json::from_str(e).unwrap_or(json!(e))).collect(),
            other => panic!("expected a stream, got {other:?}"),
        }
    }

    #[test]
    fn parses_lines_and_skips_comments() {
        let script = "// the first step\n{\"text\": \"hi\"}\n\n{\"tool_calls\": [{\"name\": \"glob\", \"arguments\": {\"pattern\": \"*\"}}]}\n";
        let replies = parse_script(script).unwrap();
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[1].tool_calls[0].arguments_text(), r#"{"pattern":"*"}"#);
    }

    #[test]
    fn rejects_unknown_fields() {
        let err = parse_script("{\"txt\": \"typo\"}").unwrap_err().to_string();
        assert!(err.contains("line 1"), "{err}");
    }

    #[test]
    fn text_streams_in_chunks_then_finish_usage_and_done() {
        let reply = Reply { text: "abcdef".into(), chunk: Some(4), ..Reply::default() };
        let events = stream(&reply);
        assert_eq!(events[0]["choices"][0]["delta"]["content"], "abcd");
        assert_eq!(events[1]["choices"][0]["delta"]["content"], "ef");
        assert_eq!(events[2]["choices"][0]["finish_reason"], "stop");
        assert_eq!(events[3]["usage"]["prompt_tokens"], 1000);
        assert_eq!(events[4], json!("[DONE]"));
    }

    #[test]
    fn chunks_never_split_a_character() {
        assert_eq!(pieces("ééé", 2), vec!["éé".to_string(), "é".to_string()]);
    }

    #[test]
    fn tool_calls_stream_name_first_then_arguments() {
        let reply = parse_script(r#"{"tool_calls": [{"id": "call_0", "name": "read_file", "arguments": "{\"path\":\"a\"}"}]}"#)
            .unwrap()
            .remove(0);
        let events = stream(&reply);
        let head = &events[0]["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(head["id"], "call_0");
        assert_eq!(head["function"]["name"], "read_file");
        assert_eq!(events[1]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"], r#"{"path":"a"}"#);
        assert_eq!(events[2]["choices"][0]["finish_reason"], "tool_calls");
    }

    #[test]
    fn a_call_without_an_id_sends_none() {
        let reply = parse_script(r#"{"tool_calls": [{"name": "glob", "arguments": {}}]}"#).unwrap().remove(0);
        assert!(stream(&reply)[0]["choices"][0]["delta"]["tool_calls"][0].get("id").is_none());
    }

    #[test]
    fn a_dropped_stream_has_no_finish_and_no_done() {
        let reply = Reply { text: "partial".into(), drop: true, ..Reply::default() };
        let events = stream(&reply);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn a_status_reply_has_no_stream() {
        let reply = parse_script(r#"{"status": 429, "retry_after": "1"}"#).unwrap().remove(0);
        let Response::Status { status, retry_after, .. } = reply.response("m") else { panic!() };
        assert_eq!((status, retry_after.as_deref()), (429, Some("1")));
    }
}
