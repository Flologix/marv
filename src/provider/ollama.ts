// The adapter for Ollama's native chat API (POST /api/chat). We use it
// instead of Ollama's OpenAI-compatible endpoint because only the native one
// lets us set the context window (num_ctx): Ollama's default of 4096 tokens
// is too small for an agent that reads files.
import { errorMessage, httpError, unreachable, type ErrorHints } from "./errors.ts";
import type { AgentEvent, ChatTurn, Provider, StreamOptions, ToolCall } from "./types.ts";

const LABEL = "Ollama";
/** The most a reply may be (num_predict), whatever the context window; twice that with thinking on. */
const MAX_REPLY_TOKENS = 8192;
/** How Ollama says it couldn't parse the tool call a model wrote ("qwen tool call parsing failed: XML syntax error…"). */
const UNPARSABLE_CALL = /tool call|pars(e|ing)|XML syntax|unexpected EOF/i;

interface Options {
  /** Where Ollama listens, e.g. http://localhost:11434 (a trailing /v1 is ignored). */
  baseUrl: string;
  model: string;
  /** Context window to load the model with (num_ctx). */
  contextLength: number;
  /** Let thinking models reason first. Off makes them answer directly. */
  thinking: boolean;
  /** What an unknown model tells the user to change (default: the TUI's commands). */
  hints?: ErrorHints;
}

/** One line of Ollama's stream. */
interface Line {
  message?: {
    content?: string;
    thinking?: string;
    tool_calls?: { id?: string; function: { name: string; arguments: Record<string, unknown> } }[];
  };
  done?: boolean;
  done_reason?: string;
  prompt_eval_count?: number;
  eval_count?: number;
  error?: string;
}

/** Ollama streams newline-delimited JSON: one complete object per line. */
async function* parseNDJSON(body: ReadableStream<Uint8Array>): AsyncGenerator<Line> {
  const decoder = new TextDecoder();
  let buffer = "";
  for await (const chunk of body) {
    buffer += decoder.decode(chunk, { stream: true });
    let newline: number;
    while ((newline = buffer.indexOf("\n")) !== -1) {
      const line = buffer.slice(0, newline).trim();
      buffer = buffer.slice(newline + 1);
      if (line) yield JSON.parse(line) as Line;
    }
  }
  if (buffer.trim()) yield JSON.parse(buffer) as Line;
}

/** Marv's conversation in Ollama's message format (tool arguments as objects, results tagged by tool name). */
function toMessages(history: ChatTurn[], system?: string) {
  const parse = (args: string) => {
    try {
      return JSON.parse(args) as Record<string, unknown>;
    } catch {
      return {};
    }
  };
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
                tool_calls: turn.toolCalls.map((c) => ({ function: { name: c.name, arguments: parse(c.arguments) } })),
              }
            : { role: "assistant", content: turn.text };
        case "tool":
          return { role: "tool", tool_name: turn.name, content: turn.text };
      }
    }),
  ];
}

export class OllamaProvider implements Provider {
  constructor(private readonly options: Options) {}

  get name() {
    return `ollama · ${this.options.model}`;
  }

  get contextLength() {
    return this.options.contextLength;
  }

  async *stream(history: ChatTurn[], { system, tools, signal }: StreamOptions = {}): AsyncIterable<AgentEvent> {
    const { model, contextLength, thinking } = this.options;
    const baseUrl = this.options.baseUrl.replace(/\/v1\/?$/, "").replace(/\/+$/, "");

    const body = JSON.stringify({
      model,
      stream: true,
      think: thinking,
      options: {
        // Must stay the same for the whole session: a different num_ctx reloads the model.
        num_ctx: contextLength,
        // A cap on each reply. Without one, a model that loses its way can generate for minutes (11k tokens
        // were measured), and Ollama sends nothing while it writes a tool call. A quarter of the window leaves
        // room for the conversation and still fits a whole file (a complete boids page took ~3.5k tokens).
        // The cap counts reasoning too, so with thinking on a reply gets twice as much.
        num_predict: thinking ? Math.min(2 * MAX_REPLY_TOKENS, Math.floor(contextLength / 2)) : Math.min(MAX_REPLY_TOKENS, Math.floor(contextLength / 4)),
        // Some models ship a presence penalty in their Modelfile (qwen3.5: 1.5). It punishes every token already
        // used, and code reuses tokens all the time: files came out garbled and cut off, and tool calls were left
        // unclosed. With 0 the same request wrote a complete file every time.
        presence_penalty: 0,
      },
      messages: toMessages(history, system),
      ...(tools?.length
        ? { tools: tools.map((t) => ({ type: "function", function: { name: t.name, description: t.description, parameters: t.parameters } })) }
        : {}),
    });

    let response: Response;
    // Ollama answers 500 when it can't parse the tool call the model wrote (an unclosed one), if the reply was
    // only that call (with text or reasoning before it, the error comes in the stream instead). A second sample
    // usually comes out whole, and nothing was shown yet, so it's retried once. Other 500s (out of memory, a
    // model that won't load) would only fail again, more slowly.
    for (let attempt = 1; ; attempt++) {
      try {
        // timeout: false turns off Bun's own 300 s limit: Ollama sends nothing while the model writes a tool call,
        // so a long one looked like a dead connection. num_predict bounds the wait (minutes on a GPU, longer on a
        // CPU), and Esc still stops it.
        response = await fetch(`${baseUrl}/api/chat`, { method: "POST", headers: { "content-type": "application/json" }, body, signal, timeout: false });
      } catch (err) {
        if (signal?.aborted) return;
        yield { type: "error", message: unreachable(LABEL, baseUrl, err, "Is Ollama running? Start it with `ollama serve`.") };
        return;
      }
      if (response.ok) break;
      const detail = await errorMessage(response);
      if (response.status >= 500 && UNPARSABLE_CALL.test(detail) && attempt < 2 && !signal?.aborted) continue;
      yield { type: "error", message: httpError(response.status, detail, LABEL, model, this.options.hints) };
      return;
    }

    const calls: ToolCall[] = [];
    let reason: string | undefined;
    let complete = false;
    try {
      for await (const line of parseNDJSON(response.body!)) {
        if (line.error) {
          yield { type: "error", message: `${LABEL}: ${line.error}` };
          return;
        }
        const { thinking: thought, content, tool_calls } = line.message ?? {};
        if (thought) yield { type: "thinking_delta", text: thought };
        if (content) yield { type: "text_delta", text: content };
        for (const call of tool_calls ?? []) {
          calls.push({
            id: call.id ?? `call_${calls.length}`,
            name: call.function.name,
            // Ollama gives an object; store it as JSON text like every other provider.
            arguments: JSON.stringify(call.function.arguments ?? {}),
          });
        }
        if (line.done) {
          complete = true;
          reason = line.done_reason;
          yield { type: "usage", usage: { promptTokens: line.prompt_eval_count ?? 0, completionTokens: line.eval_count ?? 0 } };
        }
      }
    } catch (err) {
      if (signal?.aborted) return;
      yield { type: "error", message: `${LABEL}: the stream broke off (${(err as Error).message})` };
      return;
    }

    // Without its done line the reply is partial (a proxy closed the stream): don't pass it off as complete.
    if (!complete) {
      if (signal?.aborted) return;
      yield { type: "error", message: `${LABEL}: the stream ended early, before the reply was complete. Try again.` };
      return;
    }
    for (const call of calls) yield { type: "tool_call", call };
    yield { type: "done", reason };
  }
}
