import { afterEach, describe, expect, test } from "bun:test";
import { OllamaProvider } from "../src/provider/ollama.ts";
import type { AgentEvent } from "../src/provider/types.ts";

// A local stand-in for Ollama's native /api/chat.
let server: ReturnType<typeof Bun.serve> | undefined;
let lastRequest: { path: string; body: any } | undefined;

function serve(respond: (req: Request) => Response | Promise<Response>) {
  server = Bun.serve({
    port: 0,
    async fetch(req) {
      lastRequest = { path: new URL(req.url).pathname, body: await req.clone().json() };
      return respond(req);
    },
  });
  return `http://localhost:${server.port}`;
}
afterEach(() => server?.stop(true));

/** Ollama streams one JSON object per line. */
const ndjson = (...lines: unknown[]) => new Response(lines.map((l) => JSON.stringify(l)).join("\n") + "\n");
const msg = (message: Record<string, unknown>) => ({ message: { role: "assistant", content: "", ...message }, done: false });
const end = { message: { role: "assistant", content: "" }, done: true, done_reason: "stop", prompt_eval_count: 298, eval_count: 26 };

function provider(baseUrl: string, thinking = false) {
  return new OllamaProvider({ baseUrl, model: "qwen3.5:9b", contextLength: 32768, thinking });
}

async function collect(stream: AsyncIterable<AgentEvent>) {
  const events: AgentEvent[] = [];
  for await (const event of stream) events.push(event);
  return events;
}

describe("OllamaProvider", () => {
  test("streams thinking and text, then usage and done", async () => {
    const url = serve(() => ndjson(msg({ thinking: "Hmm." }), msg({ content: "Hel" }), msg({ content: "lo" }), end));
    expect(await collect(provider(url).stream([{ role: "user", text: "hi" }]))).toEqual([
      { type: "thinking_delta", text: "Hmm." },
      { type: "text_delta", text: "Hel" },
      { type: "text_delta", text: "lo" },
      { type: "usage", usage: { promptTokens: 298, completionTokens: 26 } },
      { type: "done", reason: "stop" },
    ]);
  });

  test("turns tool calls (arguments as an object) into Marv's ToolCalls", async () => {
    const url = serve(() =>
      ndjson(msg({ tool_calls: [{ id: "call_x", function: { index: 0, name: "read_file", arguments: { path: "package.json" } } }] }), end),
    );
    const events = await collect(provider(url).stream([{ role: "user", text: "read it" }]));
    expect(events).toContainEqual({ type: "tool_call", call: { id: "call_x", name: "read_file", arguments: '{"path":"package.json"}' } });
  });

  test("asks for the context size and thinking setting, and sends history in Ollama's format", async () => {
    const url = serve(() => ndjson(end));
    const tools = [{ name: "read_file", description: "Read a file", parameters: { type: "object" } }];
    await collect(
      provider(url, true).stream(
        [
          { role: "user", text: "read it" },
          { role: "assistant", text: "", toolCalls: [{ id: "c1", name: "read_file", arguments: '{"path":"a.ts"}' }] },
          { role: "tool", callId: "c1", name: "read_file", text: "contents" },
        ],
        { system: "You are Marv.", tools },
      ),
    );
    expect(lastRequest!.path).toBe("/api/chat");
    expect(lastRequest!.body).toEqual({
      model: "qwen3.5:9b",
      stream: true,
      think: true,
      options: { num_ctx: 32768, num_predict: 16384, presence_penalty: 0 },
      messages: [
        { role: "system", content: "You are Marv." },
        { role: "user", content: "read it" },
        { role: "assistant", content: "", tool_calls: [{ function: { name: "read_file", arguments: { path: "a.ts" } } }] },
        { role: "tool", tool_name: "read_file", content: "contents" },
      ],
      tools: [{ type: "function", function: tools[0] }],
    });
  });

  test("caps each reply at a quarter of a small context window", async () => {
    const url = serve(() => ndjson(end));
    await collect(new OllamaProvider({ baseUrl: url, model: "m", contextLength: 8192, thinking: false }).stream([{ role: "user", text: "hi" }]));
    expect(lastRequest!.body.options).toEqual({ num_ctx: 8192, num_predict: 2048, presence_penalty: 0 });
  });

  test("with thinking on, a reply may be twice as long (the cap counts reasoning too)", async () => {
    const url = serve(() => ndjson(end));
    await collect(provider(url, true).stream([{ role: "user", text: "hi" }]));
    expect(lastRequest!.body.options).toEqual({ num_ctx: 32768, num_predict: 16384, presence_penalty: 0 });
  });

  describe("fits the reply cap to the room the prompt leaves", () => {
    const history = [{ role: "user" as const, text: "x".repeat(4000) }];
    /** The size the provider measures a prompt by: its messages (and tools) as JSON. */
    const chars = (h: typeof history) => JSON.stringify({ messages: h.map((t) => ({ role: t.role, content: t.text })) }).length;
    /** Serves replies that report a prompt of `tokens`. */
    const reporting = (tokens: number) => serve(() => ndjson({ ...end, prompt_eval_count: tokens }));

    test("before any usage is known, the share of the window alone", async () => {
      const url = reporting(20000);
      await collect(provider(url, true).stream(history));
      expect(lastRequest!.body.options.num_predict).toBe(16384);
    });

    test("then at most the window minus the estimated prompt (with 5% headroom)", async () => {
      const url = reporting(20000);
      const p = provider(url, true);
      await collect(p.stream(history));
      await collect(p.stream(history));
      // Same prompt: 20000 tokens, plus 5% → 21000; 32768 − 21000 leaves 11768 (the share alone would be 16384).
      expect(lastRequest!.body.options.num_predict).toBe(11768);
    });

    test("a prompt that grew since is estimated at the measured tokens per character", async () => {
      const url = reporting(20000);
      const p = provider(url, true);
      await collect(p.stream(history));
      const longer = [...history, { role: "user" as const, text: "y".repeat(2000) }];
      await collect(p.stream(longer));
      const estimate = Math.ceil((chars(longer) * 20000 * 1.05) / chars(history));
      expect(lastRequest!.body.options.num_predict).toBe(32768 - estimate);
    });

    test("a nearly full window still leaves a short reply, not zero or less", async () => {
      const url = reporting(32000);
      const p = provider(url);
      await collect(p.stream(history));
      await collect(p.stream(history));
      expect(lastRequest!.body.options.num_predict).toBe(1024);
    });

    test("with room to spare, the share still caps it", async () => {
      const url = reporting(1000);
      const p = provider(url);
      await collect(p.stream(history));
      await collect(p.stream(history));
      expect(lastRequest!.body.options.num_predict).toBe(8192);
    });

    test("a cache hit (Ollama counting only the new tokens) doesn't lower the estimate", async () => {
      let tokens = 20000;
      const url = serve(() => ndjson({ ...end, prompt_eval_count: tokens }));
      const p = provider(url, true);
      await collect(p.stream(history));
      tokens = 50;
      await collect(p.stream(history));
      await collect(p.stream(history));
      expect(lastRequest!.body.options.num_predict).toBe(11768);
    });

    test("a reply without a prompt count keeps the last measurement", async () => {
      let tokens: number | undefined = 20000;
      const url = serve(() => ndjson({ ...end, prompt_eval_count: tokens }));
      const p = provider(url, true);
      await collect(p.stream(history));
      tokens = undefined;
      await collect(p.stream(history));
      await collect(p.stream(history));
      expect(lastRequest!.body.options.num_predict).toBe(11768);
    });
  });

  test("other server errors (out of memory, a failed load) aren't retried: a second try would only repeat them", async () => {
    let requests = 0;
    const url = serve(() => (++requests, Response.json({ error: "model requires more system memory" }, { status: 500 })));
    const events = await collect(provider(url).stream([{ role: "user", text: "hi" }]));
    expect(requests).toBe(1);
    expect(events).toEqual([{ type: "error", message: expect.stringContaining("more system memory") }]);
  });

  test("a server error (an unparsable tool call) is retried once; a second one is reported", async () => {
    let requests = 0;
    let url = serve(() =>
      ++requests === 1 ? Response.json({ error: "XML syntax error on line 181: unexpected EOF" }, { status: 500 }) : ndjson(msg({ content: "ok" }), end),
    );
    expect(await collect(provider(url).stream([{ role: "user", text: "hi" }]))).toContainEqual({ type: "text_delta", text: "ok" });
    expect(requests).toBe(2);
    server!.stop(true);

    requests = 0;
    url = serve(() => (++requests, Response.json({ error: "XML syntax error on line 9: unexpected EOF" }, { status: 500 })));
    const events = await collect(provider(url).stream([{ role: "user", text: "hi" }]));
    expect(requests).toBe(2);
    expect(events).toEqual([{ type: "error", message: expect.stringContaining("XML syntax error") }]);
  });

  test("accepts a base URL with a trailing /v1 (older configs)", async () => {
    const url = serve(() => ndjson(end));
    await collect(provider(`${url}/v1`).stream([{ role: "user", text: "hi" }]));
    expect(lastRequest!.path).toBe("/api/chat");
  });

  test("reports a missing model and a mid-stream error", async () => {
    let url = serve(() => Response.json({ error: "model 'nope' not found" }, { status: 404 }));
    expect((await collect(provider(url).stream([{ role: "user", text: "hi" }])))[0]).toMatchObject({
      type: "error",
      message: expect.stringContaining('wasn\'t found on Ollama'),
    });
    server!.stop(true);

    url = serve(() => ndjson(msg({ content: "par" }), { error: "out of memory" }));
    expect(await collect(provider(url).stream([{ role: "user", text: "hi" }]))).toEqual([
      { type: "text_delta", text: "par" },
      { type: "error", message: "Ollama: out of memory" },
    ]);
  });

  test("explains when Ollama isn't running", async () => {
    const events = await collect(provider("http://localhost:1").stream([{ role: "user", text: "hi" }]));
    expect(events).toEqual([{ type: "error", message: expect.stringContaining("Is Ollama running?") }]);
  });

  test("exposes its context size", () => {
    expect(provider("http://localhost:11434").contextLength).toBe(32768);
  });
  test("a stream that ends without its done line is an error, not a finished reply", async () => {
    const url = serve(() => ndjson(msg({ content: "Writing the fi" })));
    const events = await collect(provider(url).stream([{ role: "user", text: "hi" }]));
    expect(events.map((e) => e.type)).toEqual(["text_delta", "error"]);
  });
});
