// The seam between Marv and any LLM vendor.
//
// Every provider (OpenRouter through the OpenAI-compatible adapter, Ollama
// through its native one) turns its own streaming format into these events.
// The UI and the agent loop only ever see these types, so swapping vendors
// never touches them.

/** A tool the model asked Marv to run. */
export interface ToolCall {
  /** Pairs the call with its result in the next request. */
  id: string;
  name: string;
  /** The arguments as the raw JSON text the model produced. Kept verbatim so
   *  every later request re-sends exactly the same bytes (prompt cache). */
  arguments: string;
}

/** One entry of the conversation the model sees. */
export type ChatTurn =
  | { role: "user"; text: string }
  | { role: "assistant"; text: string; toolCalls?: ToolCall[] }
  | { role: "tool"; callId: string; name: string; text: string };

/** How a tool is described to the model: what it does and the JSON schema of its input. */
export interface ToolSpec {
  name: string;
  description: string;
  parameters: Record<string, unknown>;
}

export interface Usage {
  /** Tokens in the prompt we sent (system + tools + conversation). */
  promptTokens: number;
  completionTokens: number;
  /** How many prompt tokens the provider served from its cache, when it reports that. */
  cachedTokens?: number;
  /** What the request cost in USD, when the provider reports it (OpenRouter does). */
  cost?: number;
}

export type AgentEvent =
  | { type: "text_delta"; text: string }
  /** The model's private reasoning before it answers (thinking models only). */
  | { type: "thinking_delta"; text: string }
  /**
   * A piece of a tool call as it streams (`text` is the next part of its JSON arguments), so the screen can show
   * a long write_file being written. For display only: the call itself still comes complete, as tool_call.
   * Providers that only send whole calls (Ollama) never emit it.
   */
  | { type: "tool_call_delta"; index: number; name: string; text: string }
  /** Emitted once the call is complete (arguments can stream in pieces). */
  | { type: "tool_call"; call: ToolCall }
  | { type: "usage"; usage: Usage }
  /** `reason` is the vendor's stop reason, e.g. "stop", "tool_calls", "length". */
  | { type: "done"; reason?: string }
  | { type: "error"; message: string }
  /** The provider is retrying after a dropped connection; `message` is shown in the status line. */
  | { type: "retry"; attempt: number; max: number; message: string };

export interface StreamOptions {
  /** Instructions sent ahead of the conversation ("You are Marv…"). */
  system?: string;
  /** Tools the model may call. */
  tools?: ToolSpec[];
  /** Aborted by ctrl+c to stop the reply (and the HTTP request behind it). */
  signal?: AbortSignal;
}

export interface Provider {
  /** Shown in the status bar, e.g. "openrouter · z-ai/glm-5.3". */
  readonly name: string;
  /** Context window in tokens, when known (Ollama: the num_ctx Marv asks for). */
  readonly contextLength?: number;
  stream(history: ChatTurn[], options?: StreamOptions): AsyncIterable<AgentEvent>;
}
