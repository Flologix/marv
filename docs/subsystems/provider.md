# marv-provider

The seam between Marv and LLM vendors: the `Provider` trait, the OpenRouter (OpenAI-compatible) and Ollama adapters, SSE/NDJSON parsing, retries, error messages, model lists and provider factories.

Spec: Marv `CLAUDE.md` bullets **Provider seam** and **createProvider**, `src/provider/*.ts`, and `tests/{sse,retry,openai-compat,ollama,models,provider-index,provider-factory}.test.ts`.

## Modules

| Marv (TS) | Rust module | Status |
|---|---|---|
| `src/provider/types.ts` | `lib.rs` (`Provider`, `StreamOptions`, `EventStream`) + `marv-core` (events, turns) | ported |
| `src/provider/index.ts` | `lib.rs` (`reasoning_for`, `create_provider`, `ProviderConfig`, `ProviderKind`) | `reasoning_for` ported and tested; `create_provider` todo |
| `src/provider/sse.ts` | `sse.rs` (`SseDecoder`, `parse_sse`) | ported, tests ported |
| `src/provider/retry.ts` | `retry.rs` | ported, tests ported (`sleep_abortable` on a `CancellationToken`); `drop_recovery` (#64) added |
| `src/provider/errors.ts` | `errors.rs` | ported and tested (`error_message` is pure: the caller reads the body first); `is_context_overflow` (#63) todo |
| `src/provider/models.ts` | `models.rs` | `from_openrouter`, `per_million` and `format_price` ported and tested; `list_models` (HTTP) todo |
| `src/provider/openai-compat.ts` | `openai_compat.rs` | wire types, `to_messages`, `request_body` and the `ReplyAssembler` (chunk → events) ported and tested; the HTTP/retry loop in `stream` is todo |
| `src/provider/ollama.ts` | `ollama.rs` | `NdjsonDecoder`, `to_messages`, `reply_cap`, `Measured`, `trained_context`, `LineAssembler` ported and tested; `ask_trained` and `stream` are todo |
| `src/provider/factory.ts` | `factory.rs` | `ProviderFactory` trait, `ProviderOption`, `CustomFactory` ported; `ConfigFactory::{make, lookup}` todo; `providerFactory` moves to `marv-session` |

`grep -rn 'todo!("port' crates/marv-provider` lists what's left.

## Port notes

- **HTTP:** `reqwest` 0.12 with `rustls-tls`, `stream` and `json`, and no default features. That means no OpenSSL and a single static binary. `Response::bytes_stream()` feeds `sse::parse_sse` (OpenRouter) or `ollama::NdjsonDecoder` (Ollama). Build one `reqwest::Client` per provider and reuse it.
- **Timeouts:**
  - OpenRouter keeps a normal timeout. It sends keep-alives while it works, so the timeout only catches dead connections.
  - Ollama must not have a total timeout: it sends nothing while the model writes a tool call, and Bun's 300 s limit cut long calls off. A connect timeout is fine.
- **Cancel:** every `stream` races its I/O against `StreamOptions::cancel` with `tokio::select!`. A cancelled stream ends quietly, with no `Error` event. That includes a cancel during a backoff wait (`retry::sleep_abortable`).
- **`stream` returns a `'static` stream.** Serialize the request body before returning (`request_body` / `ollama::Prompt`) so the caller can keep appending to its history. Build the returned stream with `futures::stream::unfold` or a channel. Don't add a generator crate unless it pays for itself.
- **Prompt cache:**
  - Request bodies are typed `Serialize` structs, so field order is byte order. Don't route them through `serde_json::Value`: another crate could switch on serde_json's `preserve_order` feature through feature unification and silently change key order.
  - `ToolCall::arguments` is sent as the raw text (OpenAI). Ollama needs an object, so it's parsed there; Ollama's cache is local, and Marv does the same.
  - `extra` body fields (`reasoning`) are flattened last.
- **Retry semantics to keep:**
  - The `Retry` event's `attempt`/`max` are n/2, and it comes before each new attempt.
  - `Retry-After` is honoured only as whole seconds, at most 30.
  - Ollama's "unparsable tool call" 500 is retried once and doesn't count as a connection retry.
  - A stream that ends without `[DONE]`, a `finish_reason` or Ollama's `done` line counts as a drop.
  - Invalid JSON in a chunk also counts as a drop (Marv's `JSON.parse` throws into the same catch).
  - After 3 attempts: `"<label>: the stream ended early, before the reply was complete. Try again."`.
- **Network errors:** `retry::is_network_error` matches Marv's (Bun's) messages. With reqwest, check `err.is_connect() || err.is_timeout()` first, and walk `std::error::Error::source()` for hyper's wording.
- **Ollama `/api/show`:**
  - Concurrent streams ask once. They hold a `tokio::sync::Mutex` and re-check `Measured::trained` after taking it.
  - A failed or empty answer leaves the window as configured and is asked again on the next stream.
  - The configured `context_length` is always the upper bound.
- **`RetryTiming`** replaces Marv's global `retryTiming.scale`. Pass it in the adapter options; tests set `scale` to something like 0.001.
- **Tests that need a server** (the retry loop, headers, the `Authorization` header only when a key is set, `X-Title`, aborts) port `tests/openai-compat.test.ts` and `tests/ollama.test.ts` against a local `tokio::net::TcpListener`. Hand-roll a tiny HTTP/1.1 responder, or add `axum` as a dev-dependency.

## Planned improvements (Marv issues, not in Marv yet)

- **#56 cache_control breakpoints:**
  - `openai_compat::needs_cache_breakpoints` decides by model id: `anthropic/*` and `google/gemini-*`.
  - `to_messages(.., breakpoints: true)` marks the system prompt and the last message.
  - Every other message keeps exactly the bytes it has without breakpoints (tested).
  - Still to do: measure the cache-hit rate before and after, as the issue asks. Check that an assistant message with only `tool_calls` (empty content) is handled sensibly. Today it stays unmarked, because Anthropic rejects an empty text part.
- **#57 reasoning round-trip:**
  - `Delta::reasoning_details` is collected and emitted as `ProviderEvent::Reasoning` before `Done`.
  - `to_messages` re-sends it as `reasoning_details` on the assistant message.
  - Open question: OpenRouter may split one block over several chunks (same `index`). Check whether the blocks must be merged before they're sent back.
- **#59 `tool_choice: "none"`** is in `ChatRequest` and is sent only for `ToolChoice::None`. Auto requests keep their bytes. Ollama has no equivalent, so it relies on the note.
- **#63 overflow recovery:** `errors::is_context_overflow` is a stub. Collect real OpenRouter and Ollama overflow messages first.
- **#64 continue after a dropped stream:**
  - `retry::drop_recovery` decides between continue (text received and no tool call in flight) and restart.
  - `ReplyAssembler::{text_received, tool_call_in_progress}` provide its inputs.
  - Telling the loop needs `marv_core::ProviderEvent::Retry` to carry a `continued: bool`, or a new variant. **This is a marv-core change, still to request.**
  - Also planned: jitter, about 6 attempts for 429/overloaded, and longer `Retry-After`.
