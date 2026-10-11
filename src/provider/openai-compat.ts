// The adapter for "OpenAI-compatible" chat APIs (OpenRouter, and anything
// else that speaks POST /chat/completions with streaming). It translates
// their stream into Marv's AgentEvents, so nothing above this file knows
// which vendor (or which wire format) is on the other end.
import { describeError, errorMessage, httpError, unreachable, type ErrorHints, type ErrorObject } from "./errors.ts";
import { isNetworkError, retriableHttpStatus, sleepAbortable, parseRetryAfter, BACKOFFS } from "./retry.ts";
import { parseSSE } from "./sse.ts";
import type { AgentEvent, ChatTurn, Provider, StreamOptions, ToolCall } from "./types.ts";

interface Options {
  /** Shown in the status bar. */
  name: string;
  /** Used in error messages, e.g. "OpenRouter". */
  label: string;
  baseUrl: string;
  model: string;
  apiKey?: string;
  /** Extra request headers (e.g. OpenRouter's app attribution). */
  headers?: Record<string, string>;
  /** Extra request body fields. */
  body?: Record<string, unknown>;
  /** Appended when the server can't be reached at all. */
  offlineHint?: string;
  /** What a bad key or an unknown model tells the user to change (default: the TUI's commands). */
  hints?: ErrorHints;
}

/** A tool call as it streams in: id and name first, arguments in pieces. */
interface ToolCallDelta {
  index?: number;
  id?: string;
  function?: { name?: string; arguments?: string };
}

/** The shape of one streamed chunk; we only read the fields we use. */
interface Chunk {
  choices?: {
    delta?: {
      content?: string | null;
      // Reasoning arrives as `reasoning` (OpenRouter) or `reasoning_content` (DeepSeek-style servers).
      reasoning?: string | null;
      reasoning_content?: string | null;
      tool_calls?: ToolCallDelta[];
    };
    finish_reason?: string | null;
  }[];
  // OpenRouter adds `cost` (USD, what the request actually cost) to every final usage chunk.
  usage?: { prompt_tokens?: number; completion_tokens?: number; prompt_tokens_details?: { cached_tokens?: number }; cost?: number };
  error?: ErrorObject;
}

/** Marv's conversation in OpenAI's message format. Deterministic, so repeated requests share a cacheable prefix. */
function toMessages(history: ChatTurn[], system?: string) {
  return [
    ...(system ? [{ role: "system", content: system }] : []),
    ...history.map((turn) => {
      switch (turn.role) {
        case "user":
          return { role: "user", content: turn.text };
        case "assistant":
          return turn.toolCalls
            ? {
                role: "assistant",
                content: turn.text,
                tool_calls: turn.toolCalls.map((c) => ({
                  id: c.id,
                  type: "function",
                  function: { name: c.name, arguments: c.arguments },
                })),
              }
            : { role: "assistant", content: turn.text };
        case "tool":
          return { role: "tool", tool_call_id: turn.callId, content: turn.text };
      }
    }),
  ];
}

export class OpenAICompatProvider implements Provider {
  constructor(private readonly options: Options) {}

  get name() {
    return this.options.name;
  }

  async *stream(history: ChatTurn[], { system, tools, signal }: StreamOptions = {}): AsyncIterable<AgentEvent> {
    const { baseUrl, model, apiKey, label } = this.options;

    // The API is stateless: every request carries the whole conversation.
    const body = {
      model,
      stream: true,
      // Ask for token counts (incl. cached tokens) in the final chunk.
      stream_options: { include_usage: true },
      messages: toMessages(history, system),
      ...(tools?.length
        ? { tools: tools.map((t) => ({ type: "function", function: { name: t.name, description: t.description, parameters: t.parameters } })) }
        : {}),
      ...this.options.body,
    };

    // Retry loop: up to 3 attempts (original + 2 retries) with backoff, for network errors and
    // retriable HTTP statuses (502, 503, 504, 429 with Retry-After).
    // Esc (abort) stops the retries.
    for (let attempt = 1; ; attempt++) {
      if (signal?.aborted) return;

      if (attempt > 1) {
        yield { type: "retry", attempt: attempt - 1, max: 2, message: `${label}: connection lost, retrying (${attempt - 1}/2)…` };
      }

      let response: Response;
      try {
        response = await fetch(`${baseUrl}/chat/completions`, {
          method: "POST",
          headers: {
            "content-type": "application/json",
            ...(apiKey ? { authorization: `Bearer ${apiKey}` } : {}),
            ...this.options.headers,
          },
          body: JSON.stringify(body),
          signal,
        });
      } catch (err) {
        if (signal?.aborted) return;
        if (attempt < 3 && isNetworkError(err)) {
          if (await sleepAbortable(BACKOFFS[attempt - 1]! * 1000, signal)) return;
          continue;
        }
        yield { type: "error", message: unreachable(label, baseUrl, err, this.options.offlineHint) };
        return;
      }

      if (response.ok) {
        // Stream the SSE body. If the connection drops mid-stream, retry from the top.
        const result = yield* this.parseStream(response, label, signal);
        if (result === "retry") {
          if (attempt < 3) {
            if (await sleepAbortable(BACKOFFS[attempt - 1]! * 1000, signal)) return;
            continue;
          }
          yield { type: "error", message: `${label}: the stream ended early, before the reply was complete. Try again.` };
          return;
        }
        return;
      }

      // HTTP-level error
      if (retriableHttpStatus(response.status) && attempt < 3) {
        const delay = parseRetryAfter(response.headers.get("Retry-After")) ?? BACKOFFS[attempt - 1]!;
        if (await sleepAbortable(delay * 1000, signal)) return;
        continue;
      }

      yield { type: "error", message: httpError(response.status, await errorMessage(response), label, model, this.options.hints) };
      return;
    }
  }

  /**
   * Parses the SSE response body. Returns "retry" if the stream broke off mid-way (a retriable
   * connection drop), or undefined on success. Yields events as it goes.
   */
  private async *parseStream(response: Response, label: string, signal?: AbortSignal): AsyncGenerator<AgentEvent, "retry" | undefined, undefined> {
    // Tool calls arrive in fragments keyed by index; they're only complete at the end.
    const pending = new Map<number, ToolCall>();
    let reason: string | undefined;
    // Whether the server said the reply was complete ([DONE] or a finish_reason). A proxy can close the stream
    // cleanly halfway: then the text is partial and a tool call's arguments are cut off.
    let complete = false;
    try {
      for await (const data of parseSSE(response.body!)) {
        if (data === "[DONE]") {
          complete = true;
          break;
        }
        const chunk = JSON.parse(data) as Chunk;
        // Errors can also arrive mid-stream, after the 200 status was already sent.
        if (chunk.error) {
          yield { type: "error", message: `${label}: ${describeError(chunk.error)}` };
          return undefined;
        }
        if (chunk.usage) {
          const { prompt_tokens = 0, completion_tokens = 0, prompt_tokens_details, cost } = chunk.usage;
          yield {
            type: "usage",
            usage: {
              promptTokens: prompt_tokens,
              completionTokens: completion_tokens,
              cachedTokens: prompt_tokens_details?.cached_tokens,
              ...(typeof cost === "number" ? { cost } : {}),
            },
          };
        }
        const choice = chunk.choices?.[0];
        if (choice?.finish_reason) {
          reason = choice.finish_reason;
          complete = true;
        }
        const delta = choice?.delta;
        const thinking = delta?.reasoning ?? delta?.reasoning_content;
        if (thinking) yield { type: "thinking_delta", text: thinking };
        if (delta?.content) yield { type: "text_delta", text: delta.content };
        for (const fragment of delta?.tool_calls ?? []) {
          const index = fragment.index ?? pending.size;
          const call = pending.get(index) ?? { id: "", name: "", arguments: "" };
          // The first time a call's name is known, it's worth telling the screen even before any arguments.
          const named = !call.name && Boolean(fragment.function?.name);
          if (fragment.id) call.id = fragment.id;
          if (fragment.function?.name) call.name = fragment.function.name;
          const text = fragment.function?.arguments ?? "";
          call.arguments += text;
          pending.set(index, call);
          if (text || named) yield { type: "tool_call_delta", index, name: call.name, text };
        }
      }
    } catch (err) {
      if (signal?.aborted) return undefined;
      // A connection drop mid-stream is retriable: the error is typically a socket closed or
      // connection reset. Return "retry" so the caller can redo the entire request.
      return "retry";
    }

    if (!complete) {
      if (signal?.aborted) return undefined;
      // The stream ended cleanly but early (no [DONE] or finish_reason): a proxy closed it,
      // or the server lost the connection. A retry would get a fresh response.
      return "retry";
    }
    for (const [index, call] of [...pending].sort(([a], [b]) => a - b)) {
      yield { type: "tool_call", call: { ...call, id: call.id || `call_${index}` } };
    }
    yield { type: "done", reason };
    return undefined;
  }
}