import { afterAll, afterEach, beforeAll, describe, expect, test } from "bun:test";
import { OpenAICompatProvider } from "../src/provider/openai-compat.ts";
import type { AgentEvent } from "../src/provider/types.ts";
import { retryTiming } from "../src/provider/retry.ts";

// Retry waits (1 s, 4 s, 10 s, Retry-After) shrunk to milliseconds, so retry tests don't wait in real time.
beforeAll(() => void (retryTiming.scale = 0.001));
afterAll(() => void (retryTiming.scale = 1));

// A local stand-in for OpenRouter/Ollama: each test decides how it responds.
let server: ReturnType<typeof Bun.serve> | undefined;
let lastRequest: { headers: Headers; body: any } | undefined;

function serve(respond: (req: Request) => Response | Promise<Response>) {
  server = Bun.serve({
    port: 0,
    async fetch(req) {
      lastRequest = { headers: req.headers, body: await req.clone().json() };
      return respond(req);
    },
  });
  return `http://localhost:${server.port}/v1`;
}
afterEach(() => server?.stop(true));

const sse = (...events: unknown[]) =>
  new Response(events.map((e) => `data: ${typeof e === "string" ? e : JSON.stringify(e)}\n\n`).join(""), {
    headers: { "content-type": "text/event-stream" },
  });
const delta = (content: string) => ({ choices: [{ delta: { content } }] });

function provider(baseUrl: string, apiKey?: string, body?: Record<string, unknown>) {
  return new OpenAICompatProvider({ name: "test", label: "TestRouter", baseUrl, model: "acme/model-1", apiKey, body });
}

async function collect(stream: AsyncIterable<AgentEvent>) {
  const events: AgentEvent[] = [];
  for await (const event of stream) events.push(event);
  return events;
}

describe("OpenAICompatProvider", () => {
  test("streams text deltas and ends with done", async () => {
    const url = serve(() => sse(delta("Hel"), delta("lo"), { choices: [{ delta: {}, finish_reason: "stop" }] }, "[DONE]"));
    expect(await collect(provider(url).stream([{ role: "user", text: "hi" }]))).toEqual([
      { type: "text_delta", text: "Hel" },
      { type: "text_delta", text: "lo" },
      { type: "done", reason: "stop" },
    ]);
  });

  test("sends the model, system prompt, history, and key", async () => {
    const url = serve(() => sse("[DONE]"));
    await collect(
      provider(url, "sk-test").stream(
        [
          { role: "user", text: "hi" },
          { role: "assistant", text: "hello" },
          { role: "user", text: "again" },
        ],
        { system: "You are Marv." },
      ),
    );
    expect(lastRequest!.headers.get("authorization")).toBe("Bearer sk-test");
    expect(lastRequest!.body).toEqual({
      model: "acme/model-1",
      stream: true,
      stream_options: { include_usage: true },
      messages: [
        { role: "system", content: "You are Marv." },
        { role: "user", content: "hi" },
        { role: "assistant", content: "hello" },
        { role: "user", content: "again" },
      ],
    });
  });

  test("streams reasoning as thinking deltas, from either field name", async () => {
    const url = serve(() =>
      sse({ choices: [{ delta: { reasoning: "Hmm, " } }] }, { choices: [{ delta: { reasoning_content: "ok." } }] }, delta("Hi!"), "[DONE]"),
    );
    expect(await collect(provider(url).stream([{ role: "user", text: "hi" }]))).toEqual([
      { type: "thinking_delta", text: "Hmm, " },
      { type: "thinking_delta", text: "ok." },
      { type: "text_delta", text: "Hi!" },
      { type: "done" },
    ]);
  });

  test("adds extra body fields to the request", async () => {
    const url = serve(() => sse("[DONE]"));
    await collect(provider(url, undefined, { reasoning_effort: "none" }).stream([{ role: "user", text: "hi" }]));
    expect(lastRequest!.body.reasoning_effort).toBe("none");
  });

  test("sends tools, and tool calls and results in OpenAI's format", async () => {
    const url = serve(() => sse("[DONE]"));
    const tools = [{ name: "read_file", description: "Read a file", parameters: { type: "object" } }];
    await collect(
      provider(url).stream(
        [
          { role: "user", text: "read it" },
          { role: "assistant", text: "", toolCalls: [{ id: "c1", name: "read_file", arguments: '{"path":"a.ts"}' }] },
          { role: "tool", callId: "c1", name: "read_file", text: "file contents" },
        ],
        { tools },
      ),
    );
    expect(lastRequest!.body.tools).toEqual([{ type: "function", function: tools[0] }]);
    expect(lastRequest!.body.messages).toEqual([
      { role: "user", content: "read it" },
      {
        role: "assistant",
        content: "",
        tool_calls: [{ id: "c1", type: "function", function: { name: "read_file", arguments: '{"path":"a.ts"}' } }],
      },
      { role: "tool", tool_call_id: "c1", content: "file contents" },
    ]);
  });

  test("reassembles tool calls streamed in fragments", async () => {
    const tc = (tool_calls: unknown) => ({ choices: [{ delta: { tool_calls } }] });
    const url = serve(() =>
      sse(
        tc([{ index: 0, id: "c1", type: "function", function: { name: "read_file", arguments: "" } }]),
        tc([{ index: 0, function: { arguments: '{"pa' } }]),
        tc([{ index: 1, id: "c2", type: "function", function: { name: "glob", arguments: '{"pattern":"*"}' } }]),
        tc([{ index: 0, function: { arguments: 'th":"a.ts"}' } }]),
        { choices: [{ delta: {}, finish_reason: "tool_calls" }] },
        { choices: [], usage: { prompt_tokens: 1200, completion_tokens: 30, prompt_tokens_details: { cached_tokens: 1000 }, cost: 0.00042 } },
        "[DONE]",
      ),
    );
    const events = await collect(provider(url).stream([{ role: "user", text: "go" }]));
    expect(events.filter((e) => e.type !== "tool_call_delta")).toEqual([
      { type: "usage", usage: { promptTokens: 1200, completionTokens: 30, cachedTokens: 1000, cost: 0.00042 } },
      { type: "tool_call", call: { id: "c1", name: "read_file", arguments: '{"path":"a.ts"}' } },
      { type: "tool_call", call: { id: "c2", name: "glob", arguments: '{"pattern":"*"}' } },
      { type: "done", reason: "tool_calls" },
    ]);
    // Each fragment is also reported as it arrives: a big write_file can take minutes to stream, and showing
    // nothing until it's complete looked like a hang.
    expect(events.filter((e) => e.type === "tool_call_delta")).toEqual([
      { type: "tool_call_delta", index: 0, name: "read_file", text: "" },
      { type: "tool_call_delta", index: 0, name: "read_file", text: '{"pa' },
      { type: "tool_call_delta", index: 1, name: "glob", text: '{"pattern":"*"}' },
      { type: "tool_call_delta", index: 0, name: "read_file", text: 'th":"a.ts"}' },
    ]);
  });

  test("sends no Authorization header without a key (Ollama)", async () => {
    const url = serve(() => sse("[DONE]"));
    await collect(provider(url).stream([{ role: "user", text: "hi" }]));
    expect(lastRequest!.headers.get("authorization")).toBeNull();
  });

  test.each([
    [401, "TestRouter rejected the API key"],
    [402, "out of credits"],
    [404, 'Model "acme/model-1" wasn\'t found'],
  ])("turns HTTP %d into a helpful error", async (status, message) => {
    const url = serve(() => Response.json({ error: { message: "upstream says no" } }, { status }));
    const events = await collect(provider(url).stream([{ role: "user", text: "hi" }]));
    expect(events).toHaveLength(1);
    expect(events[0]).toMatchObject({ type: "error" });
    expect((events[0] as { message: string }).message).toContain(message);
    expect((events[0] as { message: string }).message).toContain("upstream says no");
  });

  test("turns HTTP 429 into a helpful error after retries are exhausted", async () => {
    let attempts = 0;
    const url = serve(() => {
      attempts++;
      return Response.json({ error: { message: "too fast" } }, { status: 429 });
    });
    const events = await collect(provider(url).stream([{ role: "user", text: "hi" }]));
    expect(attempts).toBe(3);
    expect(events.filter((e) => e.type === "retry")).toHaveLength(2);
    expect(events.some((e) => e.type === "error")).toBe(true);
    expect((events.find((e) => e.type === "error") as { message: string }).message).toContain("Rate limited");
  }, { timeout: 20000 });

  test("reports an error sent in the middle of the stream", async () => {
    const url = serve(() => sse(delta("partial"), { error: { message: "provider crashed" } }));
    expect(await collect(provider(url).stream([{ role: "user", text: "hi" }]))).toEqual([
      { type: "text_delta", text: "partial" },
      { type: "error", message: "TestRouter: provider crashed" },
    ]);
  });

  test("an upstream failure wrapped by OpenRouter shows the provider's own message", async () => {
    const wrapped = { error: { message: "Provider returned error", code: 400, metadata: { provider_name: "Anthropic", raw: '{"type":"error","error":{"message":"tool_use ids must be unique"}}' } } };
    const http = serve(() => Response.json(wrapped, { status: 400 }));
    const [event] = await collect(provider(http).stream([{ role: "user", text: "hi" }]));
    const message = (event as { type: string; message: string }).message;
    expect(event?.type).toBe("error");
    expect(message).toContain("Anthropic");
    expect(message).toContain("tool_use ids must be unique");
    server!.stop(true);
    const midStream = serve(() => sse(delta("partial"), wrapped));
    const events = await collect(provider(midStream).stream([{ role: "user", text: "hi" }]));
    expect(events.at(-1)).toMatchObject({ type: "error", message: expect.stringContaining("tool_use ids must be unique") });
  });

  test("a gateway's 502 retries first, then reports a clean error without HTML in the transcript", async () => {
    const page = `<!DOCTYPE html><html><head><title>502 Bad Gateway</title></head><body>${"x".repeat(8000)}</body></html>`;
    let attempts = 0;
    const url = serve(() => {
      attempts++;
      return new Response(page, { status: 502, statusText: "Bad Gateway", headers: { "content-type": "text/html" } });
    });
    const events = await collect(provider(url).stream([{ role: "user", text: "hi" }]));
    expect(attempts).toBe(3);
    const errorEvent = events.find((e) => e.type === "error") as { message: string };
    expect(errorEvent.message).toStartWith("TestRouter is having trouble (502).");
    expect(errorEvent.message).not.toContain("<");
    expect(errorEvent.message.length).toBeLessThan(200);
  }, { timeout: 20000 });

  test("a stream that ends before the reply is complete retries, then reports the error", async () => {
    const toolFragment = { choices: [{ delta: { tool_calls: [{ index: 0, id: "c1", function: { name: "write_file", arguments: '{"path":"a.ts","con' } }] } }] };
    let attempts = 0;
    const url = serve(() => {
      attempts++;
      return sse(delta("Writing the fi"), toolFragment); // no finish_reason, no [DONE]
    });
    const events = await collect(provider(url).stream([{ role: "user", text: "hi" }]));
    expect(attempts).toBe(3);
    // The fragment was shown each time it streamed, and retries happened.
    expect(events.filter((e) => e.type === "text_delta").length).toBe(3);
    expect(events.filter((e) => e.type === "tool_call_delta").length).toBe(3);
    expect(events.filter((e) => e.type === "retry")).toHaveLength(2);
    expect(events.some((e) => e.type === "error")).toBe(true);
  }, { timeout: 20000 });

  test("explains when the server can't be reached, after retries", async () => {
    const events = await collect(provider("http://localhost:1/v1").stream([{ role: "user", text: "hi" }]));
    // Unable to connect is a network error, retried 3 times, then reported.
    expect(events.filter((e) => e.type === "retry")).toHaveLength(2);
    expect(events.some((e) => e.type === "error")).toBe(true);
    const errorEvent = events.find((e) => e.type === "error") as { message: string };
    expect(errorEvent.message).toContain("Can't reach TestRouter at http://localhost:1");
  }, { timeout: 20000 });

  describe("retries on network errors and retriable HTTP statuses", () => {
    test("resends the request after an HTTP 503, succeeding on retry", async () => {
      let attempts = 0;
      const url = serve(() => {
        if (++attempts === 1) return new Response("Service Unavailable", { status: 503 });
        return sse(delta("ok"), { choices: [{ delta: {}, finish_reason: "stop" }] }, "[DONE]");
      });
      const events = await collect(provider(url).stream([{ role: "user", text: "hi" }]));
      expect(attempts).toBe(2);
      expect(events).toContainEqual({ type: "text_delta", text: "ok" });
      expect(events.filter((e) => e.type === "error")).toHaveLength(0);
    });

    test("exhausts retries on repeated HTTP 503 and reports the error", async () => {
      let attempts = 0;
      const url = serve(() => {
        attempts++;
        return new Response("Service Unavailable", { status: 503 });
      });
      const events = await collect(provider(url).stream([{ role: "user", text: "hi" }]));
      expect(attempts).toBe(3);
      expect(events.filter((e) => e.type === "retry")).toHaveLength(2);
      expect(events.some((e) => e.type === "done")).toBe(false);
      expect(events.some((e) => e.type === "error")).toBe(true);
      const errorEvent = events.find((e) => e.type === "error") as { message: string };
      expect(errorEvent.message).toContain("is having trouble (503)");
    }, { timeout: 20000 });

    test("retries on 429 when Retry-After is present", async () => {
      let attempts = 0;
      const url = serve(() => {
        if (++attempts === 1) return new Response("Rate limited", { status: 429, headers: { "Retry-After": "1" } });
        return sse(delta("ok"), { choices: [{ delta: {}, finish_reason: "stop" }] }, "[DONE]");
      });
      const events = await collect(provider(url).stream([{ role: "user", text: "hi" }]));
      expect(attempts).toBe(2);
      expect(events).toContainEqual({ type: "text_delta", text: "ok" });
    });

    test("ignores non-integer Retry-After values and uses the normal backoff", async () => {
      const start = Date.now();
      let attempts = 0;
      const url = serve(() => {
        if (++attempts === 1) return new Response("Rate limited", { status: 429, headers: { "Retry-After": "Thu, 1 Jan 2026 00:00:00 GMT" } });
        return sse(delta("retried"), { choices: [{ delta: {}, finish_reason: "stop" }] }, "[DONE]");
      });
      const events = await collect(provider(url).stream([{ role: "user", text: "hi" }]));
      expect(attempts).toBe(2);
      // An HTTP date Retry-After should fall back to the 1s backoff.
      // An HTTP date falls back to the first backoff (1 s, scaled for tests); the 30 s cap is unit-tested in retry.test.ts.
      expect(Date.now() - start).toBeGreaterThanOrEqual(1000 * retryTiming.scale * 0.9);
      expect(events).toContainEqual({ type: "text_delta", text: "retried" });
    });

    test("caps Retry-After at 30 seconds", async () => {
      let attempts = 0;
      const url = serve(() => {
        if (++attempts === 1) return new Response("Rate limited", { status: 429, headers: { "Retry-After": "3600" } });
        return sse(delta("retried"), { choices: [{ delta: {}, finish_reason: "stop" }] }, "[DONE]");
      });
      const events = await collect(provider(url).stream([{ role: "user", text: "hi" }]));
      expect(attempts).toBe(2);
      // With cap at 30s, the test should finish well under 31s, not wait 3600s.
      expect(events).toContainEqual({ type: "text_delta", text: "retried" });
    });

    test("abort stops the provider promptly, even during a backoff wait", async () => {
      // Real waits here: the point is that a real backoff is cut short.
      retryTiming.scale = 1;
      const controller = new AbortController();
      // A server that always returns 503 (retriable), so the provider waits on the backoff.
      const url = serve(() => new Response("Service Unavailable", { status: 503 }));
      const start = Date.now();
      // Start the stream, then abort immediately.
      const iterator = provider(url).stream([{ role: "user", text: "hi" }], { signal: controller.signal })[Symbol.asyncIterator]();
      // Read the first event (should be a retry) then abort.
      const first = await iterator.next();
      controller.abort();
      // Collect the rest (the abort should cut the *second* backoff short).
      const rest: AgentEvent[] = [];
      for (let next = await iterator.next(); !next.done; next = await iterator.next()) rest.push(next.value);
      const elapsed = Date.now() - start;
      // The first backoff (1s) ran before the retry event was yielded, so the
      // total includes it. Without an abortable sleep it would take 1+4=5s.
      expect(elapsed).toBeLessThan(3000);
      expect(first.value).toMatchObject({ type: "retry" });
      retryTiming.scale = 0.001;
    }, { timeout: 10000 });

    test("retries on a stream that ends without DONE (a proxy that closed it), then gives up", async () => {
      let attempts = 0;
      const url = serve(() => {
        attempts++;
        return sse(delta("partial"));
      });
      const events = await collect(provider(url).stream([{ role: "user", text: "hi" }]));
      expect(attempts).toBe(3);
      expect(events.filter((e) => e.type === "retry")).toHaveLength(2);
      expect(events.some((e) => e.type === "error")).toBe(true);
    }, { timeout: 20000 });
  });

  test("stops quietly when aborted mid-stream", async () => {
    const encoder = new TextEncoder();
    const url = serve(
      () =>
        new Response(
          new ReadableStream({
            async start(controller) {
              controller.enqueue(encoder.encode(`data: ${JSON.stringify(delta("first"))}\n\n`));
              await Bun.sleep(5000); // never finishes on its own
            },
          }),
          { headers: { "content-type": "text/event-stream" } },
        ),
    );
    const controller = new AbortController();
    const events: AgentEvent[] = [];
    for await (const event of provider(url).stream([{ role: "user", text: "hi" }], { signal: controller.signal })) {
      events.push(event);
      controller.abort();
    }
    expect(events).toEqual([{ type: "text_delta", text: "first" }]);
  });
});
