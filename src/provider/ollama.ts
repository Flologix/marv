// The adapter for Ollama's native chat API (POST /api/chat). We use it
// instead of Ollama's OpenAI-compatible endpoint because only the native one
// lets us set the context window (num_ctx): Ollama's default of 4096 tokens
// is too small for an agent that reads files.
import { errorMessage, httpError, unreachable } from "./errors.ts";
import type { AgentEvent, ChatTurn, Provider, StreamOptions, ToolCall } from "./types.ts";

const LABEL = "Ollama";
/** The most a reply may be (num_predict), whatever the context window; twice that with thinking on. */
const MAX_REPLY_TOKENS = 8192;
/** The least a reply gets, however full the window: below this a model can't finish even a short answer. */
const MIN_REPLY_TOKENS = 1024;
/** Headroom on the prompt estimate: tokens per character differ between what was measured and what was added. */
const PROMPT_MARGIN = 1.05;
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
  /** The most tokens per character of prompt JSON measured so far (Ollama's prompt_eval_count over the request's size). */
  private tokensPerChar: number | undefined;

  constructor(private readonly options: Options) {}

  /**
   * num_predict for a prompt of `chars` characters. A fixed share of the window (a quarter, half with thinking on:
   * the cap counts reasoning too), but never more than the room the prompt leaves, or prompt plus reply outgrow
   * num_ctx. The prompt's tokens aren't known until Ollama answers, so they're estimated from the last measured
   * request's tokens per character (the new prompt is the last one plus whatever was added since). This instance
   * is shared by the session and its subagents, so a ratio, unlike the last count itself, still fits whichever
   * conversation comes next, or one just compacted. The highest ratio is kept: with a cached prefix Ollama can count
   * only the newly evaluated tokens, which only ever lowers it. Before any measurement, the share alone.
   */
  private replyCap(chars: number): number {
    const { contextLength, thinking } = this.options;
    const share = thinking ? Math.min(2 * MAX_REPLY_TOKENS, Math.floor(contextLength / 2)) : Math.min(MAX_REPLY_TOKENS, Math.floor(contextLength / 4));
    if (!this.tokensPerChar) return share;
    const prompt = Math.ceil(chars * this.tokensPerChar * PROMPT_MARGIN);
    // A nearly full window still gets a short reply; the 85% warning and compaction before the next turn make room.
    return Math.max(Math.min(share, MIN_REPLY_TOKENS), Math.min(share, contextLength - prompt));
  }

  get name() {
    return `ollama · ${this.options.model}`;
  }

  get contextLength() {
    return this.options.contextLength;
  }

  async *stream(history: ChatTurn[], { system, tools, signal }: StreamOptions = {}): AsyncIterable<AgentEvent> {
    const { model, contextLength, thinking } = this.options;
    const baseUrl = this.options.baseUrl.replace(/\/v1\/?$/, "").replace(/\/+$/, "");

    const prompt = {
      messages: toMessages(history, system),
      ...(tools?.length
        ? { tools: tools.map((t) => ({ type: "function", function: { name: t.name, description: t.description, parameters: t.parameters } })) }
        : {}),
    };
    const chars = JSON.stringify(prompt).length;
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
        // The cap counts reasoning too, so with thinking on a reply gets twice as much. It shrinks to the room the
        // prompt leaves (replyCap). It changes per request, but it's an option, not part of the prompt: the cache holds.
        num_predict: this.replyCap(chars),
        // Some models ship a presence penalty in their Modelfile (qwen3.5: 1.5). It punishes every token already
        // used, and code reuses tokens all the time: files came out garbled and cut off, and tool calls were left
        // unclosed. With 0 the same request wrote a complete file every time.
        presence_penalty: 0,
      },
      ...prompt,
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
      yield { type: "error", message: httpError(response.status, detail, LABEL, model) };
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
          if (line.prompt_eval_count) this.tokensPerChar = Math.max(this.tokensPerChar ?? 0, line.prompt_eval_count / chars);
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
