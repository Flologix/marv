//! Turning HTTP and connection failures into messages the user can act on. Shared by every
//! adapter. Ports `src/provider/errors.ts`.
//!
//! - Bad key (401/403) → "Run /setup…", unknown model (404) → "Pick another with /model.",
//!   unreachable → "Is it running?". SDK sessions name the option instead ([`SDK_HINTS`]):
//!   `providerFactory` passes them through `create_provider` to the adapters, which default to
//!   [`TUI_HINTS`].
//! - OpenRouter wraps an upstream failure as "Provider returned error", with the provider's own
//!   message in `metadata.raw`: that's the part that says what went wrong, so it's kept (at most
//!   [`MAX_RAW`] characters: enough to act on, not a dump).
//! - A gateway's or proxy's HTML error page (a 502 from Cloudflare) says nothing useful and must
//!   never reach the transcript: it's dropped.
//!
//! Planned (Marv #63): recognize context-length overflow errors (OpenRouter and Ollama wording),
//! so the loop can compact and retry the step ([`is_context_overflow`]). Planned (Marv #64):
//! classify OpenRouter's `overloaded` / provider errors as retriable where they are.

use serde::Deserialize;

/// Upstream detail kept from a wrapped error: enough to act on, not a dump.
pub const MAX_RAW: usize = 400;

/// An error object as providers send it: OpenAI-style, with OpenRouter's wrapper for upstream failures.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ErrorObject {
    /// OpenRouter's numeric error code (an HTTP status), when it sends one. A number or a string.
    #[serde(default)]
    pub code: Option<serde_json::Value>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub metadata: Option<ErrorMetadata>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ErrorMetadata {
    #[serde(default)]
    pub provider_name: Option<String>,
    #[serde(default)]
    pub raw: Option<serde_json::Value>,
}

/// Cuts `text` to `max` characters, with "…" when something was cut.
fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_string(),
    }
}

/// The message of an error object, with OpenRouter's upstream detail (`metadata.raw`) appended.
pub fn describe_error(error: &ErrorObject) -> String {
    let message = error.message.as_deref().unwrap_or("unknown error");
    let Some(meta) = &error.metadata else { return message.to_string() };
    let detail = match &meta.raw {
        None | Some(serde_json::Value::Null) => return message.to_string(),
        Some(serde_json::Value::String(s)) if s.is_empty() => return message.to_string(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    };
    let who = meta.provider_name.as_deref().map(|p| format!("{p}: ")).unwrap_or_default();
    format!("{message}: {who}{}", clip(&detail, MAX_RAW))
}

/// Whether an error sent inside a stream (after the 200) is one a fresh attempt may get past: the provider behind
/// the router timed out, was overloaded or unavailable, or aborted the reply. OpenRouter sends these mid-stream as
/// `{"error":{"code":502,"message":"The operation was aborted"}}`; Gemini 3.8 Flash through OpenRouter did it again and
/// again in a benchmark run, where the same model straight from Google didn't. These are retried like a
/// dropped connection (#64: the partial reply is kept or continued). Anything else (a bad request, a refused tool
/// call, an auth error) still ends the turn with its message.
pub fn transient(error: &ErrorObject) -> bool {
    let code = match &error.code {
        Some(serde_json::Value::Number(n)) => n.as_u64(),
        Some(serde_json::Value::String(s)) => s.parse().ok(),
        _ => None,
    };
    if let Some(code) = code {
        return matches!(code, 408 | 429 | 500 | 502 | 503 | 504 | 520..=529);
    }
    let message = error.message.as_deref().unwrap_or_default().to_lowercase();
    ["aborted", "timed out", "timeout", "overloaded", "temporarily unavailable", "service unavailable", "try again"]
        .iter()
        .any(|m| message.contains(m))
}

/// Pulls the message out of an error body (`{"error":{"message":…}}` or Ollama's `{"error":"…"}`).
/// Pure: the adapter reads the response's content type, status text and body first.
pub fn error_message(content_type: Option<&str>, status_text: &str, body: &str) -> String {
    // A gateway's or proxy's HTML error page says nothing useful: drop it.
    if content_type.is_some_and(|ct| ct.to_ascii_lowercase().contains("text/html")) || body.trim_start().starts_with('<') {
        return String::new();
    }
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(value) => match value.get("error") {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(err) if !err.is_null() && err != &serde_json::Value::Bool(false) => {
                describe_error(&serde_json::from_value(err.clone()).unwrap_or_default())
            }
            _ => body.to_string(),
        },
        Err(_) => {
            let text = clip(body, MAX_RAW);
            if text.is_empty() { status_text.to_string() } else { text }
        }
    }
}

/// What a bad key or an unknown model tells the user to change: the TUI's commands, or the SDK's options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorHints {
    pub key: &'static str,
    pub model: &'static str,
}

pub const TUI_HINTS: ErrorHints = ErrorHints { key: "Run /setup to enter a new one.", model: "Pick another with /model." };
pub const SDK_HINTS: ErrorHints = ErrorHints {
    key: "Pass another provider.apiKey to createSession() or configure().",
    model: "Pass another provider.model to createSession() or configure().",
};

impl Default for ErrorHints {
    fn default() -> Self {
        TUI_HINTS
    }
}

/// Turns a status code into something the user can act on.
pub fn http_error(status: u16, detail: &str, label: &str, model: &str, hints: ErrorHints) -> String {
    let reason = match status {
        401 | 403 => format!("{label} rejected the API key. {}", hints.key),
        402 => format!("Your {label} account is out of credits."),
        404 => format!("Model \"{model}\" wasn't found on {label}. {}", hints.model),
        429 => format!("Rate limited by {label}. Wait a moment and try again."),
        s if s >= 500 => format!("{label} is having trouble ({s}). Try again shortly."),
        s => format!("{label} returned {s}."),
    };
    if detail.is_empty() { reason } else { format!("{reason}\n{detail}") }
}

/// The server couldn't be reached at all. `hint`: e.g. "Is Ollama running? Start it with `ollama serve`."
pub fn unreachable(label: &str, base_url: &str, err: &str, hint: Option<&str>) -> String {
    let origin = reqwest::Url::parse(base_url).map(|u| u.origin().ascii_serialization()).unwrap_or_else(|_| base_url.to_string());
    let hint = hint.map(|h| format!(" {h}")).unwrap_or_default();
    format!("Can't reach {label} at {origin}.{hint} ({err})")
}

/// Marv #63 (planned): whether an error says the request was longer than the context window
/// (OpenRouter: "maximum context length…"/"prompt is too long"; Ollama's wording), so the loop
/// can compact and retry the step once, then shrink the target (70%, 50%, 35%).
pub fn is_context_overflow(_message: &str) -> bool {
    todo!("port: #63 context-overflow detection (collect real OpenRouter/Ollama messages first)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_stream_errors_are_upstream_timeouts_aborts_and_overloads() {
        let e =
            |code: Option<serde_json::Value>, message: &str| ErrorObject { code, message: Some(message.into()), metadata: None };
        assert!(transient(&e(Some(502.into()), "The operation was aborted")));
        assert!(transient(&e(None, "The operation was aborted")));
        assert!(transient(&e(Some(429.into()), "Rate limited")));
        assert!(transient(&e(Some("529".into()), "Overloaded")));
        assert!(!transient(&e(Some(400.into()), "Request timed out?")), "the code decides when there is one");
        assert!(!transient(&e(None, "tool_use ids must be unique")));
        assert!(!transient(&e(Some(401.into()), "No auth credentials found")));
    }

    // From tests/openai-compat.test.ts and tests/provider-factory.test.ts (there through a server).

    #[test]
    fn turns_http_statuses_into_helpful_errors() {
        let e = |status| http_error(status, "upstream says no", "TestRouter", "acme/model-1", TUI_HINTS);
        assert!(e(401).contains("TestRouter rejected the API key"));
        assert!(e(402).contains("out of credits"));
        assert!(e(404).contains("Model \"acme/model-1\" wasn't found"));
        assert!(e(404).ends_with("\nupstream says no"));
        assert!(e(502).starts_with("TestRouter is having trouble (502)."));
    }

    #[test]
    fn sdk_hints_name_the_option_to_change() {
        assert!(
            http_error(401, "", "OpenRouter", "m", TUI_HINTS)
                .starts_with("OpenRouter rejected the API key. Run /setup to enter a new one.")
        );
        assert!(
            http_error(401, "", "OpenRouter", "m", SDK_HINTS)
                .starts_with("OpenRouter rejected the API key. Pass another provider.apiKey to createSession() or configure().")
        );
    }

    #[test]
    fn a_gateway_html_page_is_dropped() {
        let page = format!("<!DOCTYPE html><html><body>{}</body></html>", "x".repeat(8000));
        assert_eq!(error_message(Some("text/html"), "Bad Gateway", &page), "");
        assert_eq!(error_message(None, "Bad Gateway", &page), "");
        let message = http_error(502, &error_message(Some("text/html"), "Bad Gateway", &page), "TestRouter", "m", TUI_HINTS);
        assert!(!message.contains('<') && message.len() < 200);
    }

    #[test]
    fn error_bodies_openai_ollama_and_plain_text() {
        assert_eq!(error_message(None, "", r#"{"error":{"message":"upstream says no"}}"#), "upstream says no");
        assert_eq!(error_message(None, "", r#"{"error":"model not found"}"#), "model not found");
        assert_eq!(error_message(None, "Not Found", ""), "Not Found");
        assert_eq!(error_message(None, "", &"y".repeat(500)).chars().count(), MAX_RAW + 1);
    }

    #[test]
    fn openrouter_wrapped_errors_keep_the_upstream_detail() {
        let err = ErrorObject {
            code: None,
            message: Some("Provider returned error".into()),
            metadata: Some(ErrorMetadata {
                provider_name: Some("Anthropic".into()),
                raw: Some("tool_use ids must be unique".into()),
            }),
        };
        assert_eq!(describe_error(&err), "Provider returned error: Anthropic: tool_use ids must be unique");
        assert_eq!(describe_error(&ErrorObject::default()), "unknown error");
    }

    #[test]
    fn unreachable_names_the_origin_and_hint() {
        assert_eq!(
            unreachable("Ollama", "http://localhost:11434/v1", "connection refused", Some("Is Ollama running?")),
            "Can't reach Ollama at http://localhost:11434. Is Ollama running? (connection refused)"
        );
    }
}
