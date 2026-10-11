import { describe, expect, test } from "bun:test";
import { answerAllCalls, CUT_OFF_NOTE, CUT_OFF_REASONING_NOTE, EMPTY_REPLY_NOTE, NOT_RUN, runAgent, type LoopEvent } from "../src/agent.ts";
import type { AgentEvent, ChatTurn, Provider, ToolCall, ToolSpec } from "../src/provider/types.ts";
import type { ToolResult } from "../src/tools/types.ts";
import { answerProblems } from "./answered.ts";
import { ScriptedProvider } from "./fake-provider.ts";

const SPECS: ToolSpec[] = [{ name: "read_file", description: "Read a file", parameters: { type: "object" } }];
const call = (id: string, path: string): ToolCall => ({ id, name: "read_file", arguments: JSON.stringify({ path }) });
const say = (text: string): AgentEvent[] => [{ type: "text_delta", text }, { type: "done", reason: "stop" }];
const useTools = (...calls: ToolCall[]): AgentEvent[] => [
  ...calls.map((c) => ({ type: "tool_call" as const, call: c })),
  { type: "done", reason: "tool_calls" },
];

const fakeTool = async (c: ToolCall): Promise<ToolResult & { label: string }> => ({
  output: `contents of ${JSON.parse(c.arguments).path}`,
  summary: "1 line",
  label: JSON.parse(c.arguments).path,
});

async function run(provider: Provider, history: ChatTurn[], opts: Partial<Parameters<typeof runAgent>[0]> = {}) {
  const events: LoopEvent[] = [];
  for await (const event of runAgent({
    provider,
    history,
    system: "SYSTEM",
    tools: SPECS,
    runTool: fakeTool,
    signal: new AbortController().signal,
    ...opts,
  }))
    events.push(event);
  return events;
}

test("answerAllCalls: a history saved mid-run gets 'not run' results for calls still waiting", () => {
  const history: ChatTurn[] = [
    { role: "user", text: "go" },
    { role: "assistant", text: "", toolCalls: [call("c1", "a.ts"), call("c2", "b.ts")] },
    { role: "tool", callId: "c1", name: "read_file", text: "a" },
  ];
  expect(answerAllCalls(history)).toEqual([...history, { role: "tool", callId: "c2", name: "read_file", text: expect.stringContaining("Interrupted") }]);
  expect(history).toHaveLength(3); // the original is untouched
  const done: ChatTurn[] = [{ role: "user", text: "hi" }, { role: "assistant", text: "hello" }];
  expect(answerAllCalls(done)).toEqual(done);
});

describe("runAgent", () => {
  test("a plain answer is one step", async () => {
    const provider = new ScriptedProvider([say("Hi!")]);
    const history: ChatTurn[] = [{ role: "user", text: "hello" }];
    const events = await run(provider, history);

    expect(history).toEqual([
      { role: "user", text: "hello" },
      { role: "assistant", text: "Hi!" },
    ]);
    expect(events.map((e) => e.type)).toEqual(["text_delta", "assistant", "done"]);
    expect(events.at(-1)).toEqual({ type: "done", reason: "end" });
  });

  test("runs the tools the model asks for, then sends the results back", async () => {
    const provider = new ScriptedProvider([useTools(call("c1", "a.ts"), call("c2", "b.ts")), say("Both read.")]);
    const history: ChatTurn[] = [{ role: "user", text: "read a and b" }];
    const events = await run(provider, history);

    expect(history).toEqual([
      { role: "user", text: "read a and b" },
      { role: "assistant", text: "", toolCalls: [call("c1", "a.ts"), call("c2", "b.ts")] },
      { role: "tool", callId: "c1", name: "read_file", text: "contents of a.ts" },
      { role: "tool", callId: "c2", name: "read_file", text: "contents of b.ts" },
      { role: "assistant", text: "Both read." },
    ]);
    // The second request carried the tool results.
    expect(provider.requests[1]!.history.at(-1)).toEqual({ role: "tool", callId: "c2", name: "read_file", text: "contents of b.ts" });
    expect(events.filter((e) => e.type === "tool_start").map((e) => e.type === "tool_start" && e.label)).toEqual(["a.ts", "b.ts"]);
    expect(events.filter((e) => e.type === "tool_end")).toHaveLength(2);
  });

  test("every request extends the previous one exactly (so the prompt cache keeps hitting)", async () => {
    const provider = new ScriptedProvider([useTools(call("c1", "a.ts")), useTools(call("c2", "b.ts")), say("Done."), useTools(call("c3", "c.ts")), say("Again.")]);
    const history: ChatTurn[] = [{ role: "user", text: "first" }];
    await run(provider, history);
    history.push({ role: "user", text: "second" });
    await run(provider, history);

    expect(provider.requests).toHaveLength(5);
    for (let i = 1; i < provider.requests.length; i++) {
      const before = provider.requests[i - 1]!;
      const after = provider.requests[i]!;
      expect(after.history.slice(0, before.history.length)).toEqual(before.history); // only appended to
      expect(after.history.length).toBeGreaterThan(before.history.length);
      expect(after.options.system).toBe(before.options.system); // same system prompt
      expect(after.options.tools).toBe(before.options.tools); // the very same tool definitions
    }
  });

  test("stops after maxSteps if the model never finishes", async () => {
    const loop = Array.from({ length: 10 }, (_, i) => useTools(call(`c${i}`, "a.ts")));
    const provider = new ScriptedProvider(loop);
    const events = await run(provider, [{ role: "user", text: "go" }], { maxSteps: 3 });

    expect(provider.requests).toHaveLength(3);
    expect(events).toContainEqual({ type: "error", message: expect.stringContaining("3 steps") });
    expect(events.at(-1)).toEqual({ type: "done", reason: "max_steps" });
  });

  test("at the limit it asks whether to keep going: yes buys another maxSteps, no stops", async () => {
    const loop = Array.from({ length: 10 }, (_, i) => useTools(call(`c${i}`, "a.ts")));
    const provider = new ScriptedProvider(loop);
    const asked: number[] = [];
    const events = await run(provider, [{ role: "user", text: "go" }], {
      maxSteps: 2,
      onLimit: async (steps) => (asked.push(steps), steps < 4),
    });
    expect(asked).toEqual([2, 4]);
    expect(provider.requests).toHaveLength(4);
    expect(events.filter((e) => e.type === "step_limit")).toEqual([
      { type: "step_limit", steps: 2, continued: true },
      { type: "step_limit", steps: 4, continued: false },
    ]);
    expect(events).toContainEqual({ type: "error", message: 'Stopped at 4 steps, as you asked. Say "continue" to pick up where it left off.' });
    expect(events.at(-1)).toEqual({ type: "done", reason: "max_steps" });
  });

  test("it asks only after a step's tool results are in, so the history stays valid either way", async () => {
    const provider = new ScriptedProvider([useTools(call("c1", "a.ts")), say("done")]);
    const history: ChatTurn[] = [{ role: "user", text: "go" }];
    await run(provider, history, {
      maxSteps: 1,
      onLimit: async () => {
        expect(history.at(-1)).toMatchObject({ role: "tool", callId: "c1" });
        return true;
      },
    });
    expect(history.at(-1)).toEqual({ role: "assistant", text: "done" });
  });

  test("Esc while it asks is an interrupt", async () => {
    const controller = new AbortController();
    const provider = new ScriptedProvider([useTools(call("c1", "a.ts")), say("never")]);
    const events = await run(provider, [{ role: "user", text: "go" }], {
      maxSteps: 1,
      signal: controller.signal,
      onLimit: async () => (controller.abort(), false),
    });
    expect(provider.requests).toHaveLength(1);
    expect(events.at(-1)).toEqual({ type: "done", reason: "aborted" });
    expect(events.some((e) => e.type === "error")).toBe(false);
  });

  test("tool-call ids are unique in the history: duplicates in a reply, ids reused across steps, and empty ids are renamed", async () => {
    const provider = new ScriptedProvider([
      useTools(call("call_0", "a.ts"), call("call_0", "b.ts"), call("", "c.ts")),
      useTools(call("call_0", "d.ts")), // Ollama numbers each step's calls from call_0
      say("done"),
    ]);
    const history: ChatTurn[] = [
      { role: "user", text: "earlier" },
      { role: "assistant", text: "", toolCalls: [{ id: "call_0_1", name: "read_file", arguments: "{}" }] }, // a resumed session
      { role: "tool", callId: "call_0_1", name: "read_file", text: "x" },
      { role: "user", text: "go" },
    ];
    const events = await run(provider, history);
    const ids = history.flatMap((t) => (t.role === "assistant" ? (t.toolCalls ?? []).map((c) => c.id) : []));
    expect(new Set(ids).size).toBe(ids.length);
    expect(ids.every(Boolean)).toBe(true);
    // Each result answers its own call, and the events carry the same ids.
    const results = history.flatMap((t) => (t.role === "tool" ? [t.callId] : []));
    expect(results).toEqual(ids);
    expect(events.flatMap((e) => (e.type === "tool_start" ? [e.call.id] : []))).toEqual(ids.slice(1));
  });

  test("if its consumer stops mid-tools (it threw), every call still gets a result, so the next request is valid", async () => {
    const provider = new ScriptedProvider([useTools(call("c1", "a.ts"), call("c2", "b.ts"))]);
    const history: ChatTurn[] = [{ role: "user", text: "go" }];
    const loop = runAgent({ provider, history, system: "S", tools: SPECS, runTool: fakeTool, signal: new AbortController().signal });
    for await (const event of loop) if (event.type === "tool_start") break; // like the App throwing while handling it
    expect(history.slice(-2)).toEqual([
      { role: "tool", callId: "c1", name: "read_file", text: expect.stringContaining("Interrupted") },
      { role: "tool", callId: "c2", name: "read_file", text: expect.stringContaining("Interrupted") },
    ]);
  });

  test("calls answered without running (after a no or an interrupt) are reported too, so a trajectory has every result", async () => {
    const provider = new ScriptedProvider([useTools(call("c1", "a.ts"), call("c2", "b.ts"), call("c3", "c.ts"))]);
    const history: ChatTurn[] = [{ role: "user", text: "go" }];
    const declineFirst = async (c: ToolCall) => ({ ...(await fakeTool(c)), declined: c.id === "c1" });
    const events = await run(provider, history, { runTool: declineFirst });
    expect(events.filter((e) => e.type === "tool_skipped")).toEqual([
      { type: "tool_skipped", call: call("c2", "b.ts"), output: expect.stringContaining("Not run") },
      { type: "tool_skipped", call: call("c3", "c.ts"), output: expect.stringContaining("Not run") },
    ]);
  });

  test("an interrupt mid-tools still answers every call, so the next request is valid", async () => {
    const controller = new AbortController();
    const provider = new ScriptedProvider([useTools(call("c1", "a.ts"), call("c2", "b.ts"))]);
    const history: ChatTurn[] = [{ role: "user", text: "go" }];
    const events = await run(provider, history, {
      signal: controller.signal,
      runTool: async (c) => {
        controller.abort(); // the user presses ctrl+c while the first tool runs
        return fakeTool(c);
      },
    });

    expect(history.filter((t) => t.role === "tool")).toEqual([
      { role: "tool", callId: "c1", name: "read_file", text: "contents of a.ts" },
      { role: "tool", callId: "c2", name: "read_file", text: "Interrupted by the user before this tool ran." },
    ]);
    expect(events.at(-1)).toEqual({ type: "done", reason: "aborted" });
  });

  test("a declined action stops the run, and later calls in the same step are answered too", async () => {
    const provider = new ScriptedProvider([useTools(call("c1", "a.ts"), call("c2", "b.ts")), say("never asked")]);
    const history: ChatTurn[] = [{ role: "user", text: "change things" }];
    const events = await run(provider, history, {
      runTool: async (c) => ({ output: "The user declined this.", summary: "declined", declined: true, label: JSON.parse(c.arguments).path }),
    });

    expect(provider.requests).toHaveLength(1); // no further request: the user decides what's next
    expect(history.filter((t) => t.role === "tool")).toEqual([
      { role: "tool", callId: "c1", name: "read_file", text: "The user declined this." },
      { role: "tool", callId: "c2", name: "read_file", text: "Not run: the user declined an earlier action." },
    ]);
    expect(events.at(-1)).toEqual({ type: "done", reason: "declined" });
  });

  test("a provider error ends the run and is reported", async () => {
    const provider = new ScriptedProvider([[{ type: "error", message: "Rate limited" }]]);
    const events = await run(provider, [{ role: "user", text: "go" }]);
    expect(events).toEqual([
      { type: "error", message: "Rate limited" },
      { type: "done", reason: "error" },
    ]);
  });

  test("tool-call fragments are passed on as they stream, for the screen", async () => {
    const provider = new ScriptedProvider([
      [{ type: "tool_call_delta", index: 0, name: "read_file", text: '{"path":' }, { type: "tool_call", call: call("c1", "a.ts") }, { type: "done" }],
      say("Done."),
    ]);
    const events = await run(provider, [{ role: "user", text: "go" }]);
    expect(events).toContainEqual({ type: "tool_call_delta", index: 0, name: "read_file", text: '{"path":' });
  });

  describe("an empty reply (no text, no tool calls)", () => {
    // Upstream hosts sometimes bill for a reply and send back nothing (OpenRouter's Novita and SiliconFlow for
    // qwen3-coder, gpt-oss-120b): the run ended as if the model had finished. A third of the failed eval runs.
    const empty: AgentEvent[] = [{ type: "done", reason: "stop" }];
    const go = (): ChatTurn[] => [{ role: "user", text: "go" }];

    test("is asked for again, unchanged (the prompt cache still hits), and the run goes on", async () => {
      const provider = new ScriptedProvider([empty, useTools(call("c1", "a.ts")), say("Done.")]);
      const events = await run(provider, go());
      expect(provider.requests).toHaveLength(3);
      expect(provider.requests[1]!.history).toEqual(provider.requests[0]!.history);
      expect(events).toContainEqual({ type: "empty_reply", next: "retry" });
      expect(events.at(-1)).toEqual({ type: "done", reason: "end" });
    });

    test("empty again: the model is told, once; empty a third time: the run ends", async () => {
      const provider = new ScriptedProvider([empty, empty, empty, say("never asked")]);
      const events = await run(provider, go());
      expect(provider.requests).toHaveLength(3);
      expect(provider.requests[2]!.history.at(-1)).toEqual({ role: "user", text: EMPTY_REPLY_NOTE });
      expect(events.filter((e) => e.type === "empty_reply")).toEqual([
        { type: "empty_reply", next: "retry" },
        { type: "empty_reply", next: "nudge" },
        { type: "empty_reply", next: "stop" },
      ]);
      expect(events.at(-1)).toEqual({ type: "done", reason: "end" });
    });

    test("whitespace only, or reasoning only, is empty too, and isn't put in the history", async () => {
      const provider = new ScriptedProvider([
        [{ type: "text_delta", text: "\n\n" }, { type: "done", reason: "stop" }],
        [{ type: "thinking_delta", text: "Hmm." }, { type: "done", reason: "stop" }],
        say("Done."),
      ]);
      const history = go();
      await run(provider, history);
      expect(history).toEqual([{ role: "user", text: "go" }, { role: "user", text: EMPTY_REPLY_NOTE }, { role: "assistant", text: "Done." }]);
    });

    test("each step gets its own retries: one that answered resets them", async () => {
      const provider = new ScriptedProvider([empty, useTools(call("c1", "a.ts")), empty, say("Done.")]);
      const events = await run(provider, go());
      expect(events.filter((e) => e.type === "empty_reply")).toEqual([
        { type: "empty_reply", next: "retry" },
        { type: "empty_reply", next: "retry" },
      ]);
      expect(events.at(-1)).toEqual({ type: "done", reason: "end" });
    });

    test("not after an interrupt, or an error", async () => {
      const stop = new AbortController();
      const aborted: Provider = {
        name: "p",
        async *stream() {
          stop.abort();
          yield { type: "done", reason: "stop" };
        },
      };
      const events = await run(aborted, go(), { signal: stop.signal });
      expect(events).toEqual([{ type: "done", reason: "aborted" }]);
      const failing = new ScriptedProvider([[{ type: "error", message: "Rate limited" }], say("never asked")]);
      expect((await run(failing, go())).some((e) => e.type === "empty_reply")).toBe(false);
      expect(failing.requests).toHaveLength(1);
    });
  });

  describe("a reply cut off at the output limit", () => {
    const cutOff = (...events: AgentEvent[]): AgentEvent[] => [...events, { type: "done", reason: "length" }];

    test("its tool calls aren't run (they may be incomplete); each gets a result saying so, and the run goes on", async () => {
      const provider = new ScriptedProvider([cutOff({ type: "tool_call", call: call("c1", "a.ts") }), say("Smaller steps, then.")]);
      const ran: string[] = [];
      const events = await run(provider, [{ role: "user", text: "go" }], { runTool: async (c) => (ran.push(c.id), fakeTool(c)) });
      expect(ran).toEqual([]);
      expect(events).toContainEqual({ type: "cut_off", continued: true });
      expect(events).toContainEqual({ type: "tool_skipped", call: call("c1", "a.ts"), output: NOT_RUN.cutOff });
      expect(provider.requests[1]!.history.at(-1)).toEqual({ role: "tool", callId: "c1", name: "read_file", text: NOT_RUN.cutOff });
      expect(events.at(-1)).toEqual({ type: "done", reason: "end" });
    });

    test("with nothing to run, the model is told once and goes on; a second cut-off ends the run", async () => {
      const provider = new ScriptedProvider([cutOff({ type: "text_delta", text: "Here is the fi" }), cutOff()]);
      const events = await run(provider, [{ role: "user", text: "go" }]);
      expect(provider.requests).toHaveLength(2);
      expect(provider.requests[1]!.history.slice(-2)).toEqual([
        { role: "assistant", text: "Here is the fi" },
        { role: "user", text: CUT_OFF_NOTE },
      ]);
      expect(events.filter((e) => e.type === "cut_off")).toEqual([
        { type: "cut_off", continued: true },
        { type: "cut_off", continued: false },
      ]);
      expect(events.at(-1)).toEqual({ type: "done", reason: "length" });
    });

    test("only two in a row end the run: after a step that finished, a cut-off is forgiven again", async () => {
      const provider = new ScriptedProvider([cutOff(), useTools(call("c1", "a.ts")), cutOff(), say("Done.")]);
      const events = await run(provider, [{ role: "user", text: "go" }]);
      expect(events.filter((e) => e.type === "cut_off")).toEqual([
        { type: "cut_off", continued: true },
        { type: "cut_off", continued: true },
      ]);
      expect(events.at(-1)).toEqual({ type: "done", reason: "end" });
    });

    test("a reply that was all reasoning is told to reason briefly (it never got to act)", async () => {
      const provider = new ScriptedProvider([cutOff({ type: "thinking_delta", text: "Let me think about boids at length…" }), say("Done.")]);
      await run(provider, [{ role: "user", text: "go" }]);
      expect(provider.requests[1]!.history.at(-1)).toEqual({ role: "user", text: CUT_OFF_REASONING_NOTE });
    });

    test("a consumer that stops at the cut_off event still leaves every call answered", async () => {
      const provider = new ScriptedProvider([cutOff({ type: "tool_call", call: call("c1", "a.ts") })]);
      const history: ChatTurn[] = [{ role: "user", text: "go" }];
      for await (const event of runAgent({ provider, history, system: "S", tools: SPECS, runTool: fakeTool, signal: new AbortController().signal })) {
        if (event.type === "cut_off") break;
      }
      expect(answerAllCalls(history)).toEqual(history);
      expect(history.at(-1)).toEqual({ role: "tool", callId: "c1", name: "read_file", text: NOT_RUN.cutOff });
    });

    test("cut off and interrupted: the calls are reported as interrupted, not cut off", async () => {
      const stop = new AbortController();
      const provider: Provider = {
        name: "p",
        async *stream() {
          stop.abort();
          yield { type: "tool_call", call: call("c1", "a.ts") };
          yield { type: "done", reason: "length" };
        },
      };
      const history: ChatTurn[] = [{ role: "user", text: "go" }];
      const events = await run(provider, history, { signal: stop.signal });
      expect(events.some((e) => e.type === "cut_off")).toBe(false);
      expect(history.at(-1)).toEqual({ role: "tool", callId: "c1", name: "read_file", text: NOT_RUN.aborted });
      expect(events.at(-1)).toEqual({ type: "done", reason: "aborted" });
    });
  });

  describe("parallel calls", () => {
    const sub = (id: string, ms: number): ToolCall => ({ id, name: "agent", arguments: JSON.stringify({ description: id, ms }) });
    const isParallel = (c: ToolCall) => c.name === "agent";

    /** A runTool that sleeps `ms` for agent calls and records how many run at once. */
    function tracked(declineId?: string) {
      const state = { active: 0, max: 0, started: [] as string[] };
      const runTool = async (c: ToolCall) => {
        state.started.push(c.id);
        state.active++;
        state.max = Math.max(state.max, state.active);
        const { ms = 0 } = JSON.parse(c.arguments) as { ms?: number };
        await Bun.sleep(ms);
        state.active--;
        return { output: `out ${c.id}`, summary: "ok", label: c.id, ...(c.id === declineId ? { declined: true } : {}) };
      };
      return { state, runTool };
    }

    test("consecutive parallel calls run at once; results go into history in call order", async () => {
      const provider = new ScriptedProvider([useTools(sub("a", 60), sub("b", 10)), say("Both done.")]);
      const history: ChatTurn[] = [{ role: "user", text: "go" }];
      const { state, runTool } = tracked();
      const events = await run(provider, history, { runTool, isParallel });

      expect(state.max).toBe(2);
      // b finished first…
      expect(events.filter((e) => e.type === "tool_end").map((e) => e.type === "tool_end" && e.call.id)).toEqual(["b", "a"]);
      // …but the history is in call order, so every request is deterministic (prompt cache).
      expect(history.filter((t) => t.role === "tool").map((t) => t.role === "tool" && t.callId)).toEqual(["a", "b"]);
      expect(events.at(-1)).toEqual({ type: "done", reason: "end" });
    });

    test("at most 4 run at once", async () => {
      const calls = Array.from({ length: 6 }, (_, i) => sub(`s${i}`, 20));
      const provider = new ScriptedProvider([useTools(...calls), say("ok")]);
      const { state, runTool } = tracked();
      await run(provider, [{ role: "user", text: "go" }], { runTool, isParallel });
      expect(state.max).toBe(4);
      expect(state.started).toHaveLength(6);
    });

    test("a non-parallel call between them splits the group", async () => {
      const provider = new ScriptedProvider([useTools(sub("a", 20), call("r", "x.ts"), sub("b", 20)), say("ok")]);
      const { state, runTool } = tracked();
      await run(provider, [{ role: "user", text: "go" }], { runTool, isParallel });
      expect(state.max).toBe(1);
      expect(state.started).toEqual(["a", "r", "b"]);
    });

    test("an interrupt answers queued calls without running them", async () => {
      const controller = new AbortController();
      const provider = new ScriptedProvider([useTools(sub("a", 10), sub("b", 10), sub("c", 10))]);
      const history: ChatTurn[] = [{ role: "user", text: "go" }];
      const events = await run(provider, history, {
        signal: controller.signal,
        isParallel,
        maxParallel: 1,
        runTool: async (c) => {
          controller.abort();
          return { output: `out ${c.id}`, summary: "ok", label: c.id };
        },
      });
      expect(history.filter((t) => t.role === "tool").map((t) => t.role === "tool" && t.text)).toEqual([
        "out a",
        "Interrupted by the user before this tool ran.",
        "Interrupted by the user before this tool ran.",
      ]);
      expect(events.at(-1)).toEqual({ type: "done", reason: "aborted" });
    });

    test("a no in a parallel group lets the running ones finish, then stops", async () => {
      const provider = new ScriptedProvider([useTools(sub("a", 5), sub("b", 30)), say("never")]);
      const history: ChatTurn[] = [{ role: "user", text: "go" }];
      const { runTool } = tracked("a");
      const events = await run(provider, history, { runTool, isParallel });
      expect(history.filter((t) => t.role === "tool").map((t) => t.role === "tool" && t.text)).toEqual(["out a", "out b"]);
      expect(provider.requests).toHaveLength(1);
      expect(events.at(-1)).toEqual({ type: "done", reason: "declined" });
    });

    /** Iterates the loop until the first tool_start, then stops (as a consumer whose handler throws would). */
    async function stopAtFirstToolStart(provider: Provider, opts: Partial<Parameters<typeof runAgent>[0]>) {
      for await (const event of runAgent({
        provider,
        history: [{ role: "user", text: "go" }],
        system: "SYSTEM",
        tools: SPECS,
        runTool: fakeTool,
        signal: new AbortController().signal,
        ...opts,
      }))
        if (event.type === "tool_start") break;
      await Bun.sleep(20); // anything already launched would have started by now
    }

    test("a tool doesn't start until its tool_start was delivered (single call)", async () => {
      const { state, runTool } = tracked();
      await stopAtFirstToolStart(new ScriptedProvider([useTools(call("r", "x.ts")), say("ok")]), { runTool, isParallel });
      expect(state.started).toEqual([]);
    });

    test("a tool doesn't start until its tool_start was delivered (parallel group)", async () => {
      const { state, runTool } = tracked();
      await stopAtFirstToolStart(new ScriptedProvider([useTools(sub("a", 5), sub("b", 5)), say("ok")]), { runTool, isParallel });
      expect(state.started).toEqual([]);
    });

    test("a no in a parallel group answers its queued calls without running them", async () => {
      const provider = new ScriptedProvider([useTools(sub("a", 5), sub("b", 5), sub("c", 5)), say("never")]);
      const history: ChatTurn[] = [{ role: "user", text: "go" }];
      const { state, runTool } = tracked("a");
      const events = await run(provider, history, { runTool, isParallel, maxParallel: 1 });
      expect(state.started).toEqual(["a"]);
      expect(history.filter((t) => t.role === "tool").map((t) => t.role === "tool" && t.text)).toEqual([
        "out a",
        "Not run: the user declined an earlier action.",
        "Not run: the user declined an earlier action.",
      ]);
      expect(events.at(-1)).toEqual({ type: "done", reason: "declined" });
    });

    test("a tool that throws or rejects in a group becomes an error result, and the run goes on", async () => {
      const provider = new ScriptedProvider([useTools(sub("a", 0), sub("b", 0), sub("c", 0)), say("Recovered.")]);
      const history: ChatTurn[] = [{ role: "user", text: "go" }];
      const runTool = (c: ToolCall): Promise<ToolResult & { label: string }> => {
        if (c.id === "a") throw new Error("boom");
        if (c.id === "b") return Promise.reject("not an Error");
        return Promise.resolve({ output: "out c", summary: "ok", label: "c" });
      };
      const events = await run(provider, history, { runTool, isParallel });
      expect(history.filter((t) => t.role === "tool").map((t) => t.role === "tool" && t.text)).toEqual([
        "agent failed: boom",
        "agent failed: not an Error",
        "out c",
      ]);
      expect(provider.requests).toHaveLength(2);
      expect(events.at(-1)).toEqual({ type: "done", reason: "end" });
    });

    test("after an interrupt, reserved and queued calls are answered without a tool_start", async () => {
      const controller = new AbortController();
      const provider = new ScriptedProvider([useTools(sub("a", 10), sub("b", 10), sub("c", 10))]);
      const history: ChatTurn[] = [{ role: "user", text: "go" }];
      const started: string[] = [];
      const events = await run(provider, history, {
        signal: controller.signal,
        isParallel,
        maxParallel: 2,
        runTool: async (c) => {
          started.push(c.id);
          controller.abort();
          await Bun.sleep(10);
          return { output: `out ${c.id}`, summary: "ok", label: c.id };
        },
      });
      // b had a slot too, but the interrupt came before its tool_start was delivered.
      expect(started).toEqual(["a"]);
      expect(events.filter((e) => e.type === "tool_start").map((e) => e.type === "tool_start" && e.call.id)).toEqual(["a"]);
      expect(history.filter((t) => t.role === "tool").map((t) => t.role === "tool" && t.text)).toEqual([
        "out a",
        "Interrupted by the user before this tool ran.",
        "Interrupted by the user before this tool ran.",
      ]);
      expect(events.at(-1)).toEqual({ type: "done", reason: "aborted" });
    });

    test("a call after a declined parallel group isn't run", async () => {
      const provider = new ScriptedProvider([useTools(sub("a", 5), sub("b", 5), call("r", "x.ts")), say("never")]);
      const history: ChatTurn[] = [{ role: "user", text: "go" }];
      const { state, runTool } = tracked("a");
      const events = await run(provider, history, { runTool, isParallel });
      expect(state.started).toEqual(["a", "b"]);
      expect(history.filter((t) => t.role === "tool").map((t) => t.role === "tool" && t.text)).toEqual([
        "out a",
        "out b",
        "Not run: the user declined an earlier action.",
      ]);
      expect(events.at(-1)).toEqual({ type: "done", reason: "declined" });
    });

    // Esc at an approval declines what's waiting and aborts the run: the model must read that as an
    // interrupt, not as the user saying no to those calls.
    const escOn = (controller: AbortController, id: string) => async (c: ToolCall) => {
      if (c.id === id) controller.abort();
      await Bun.sleep(5);
      return { output: `out ${c.id}`, summary: "ok", label: c.id, ...(c.id === id ? { declined: true } : {}) };
    };

    test("after Esc (a decline and an abort), a group's queued calls read as interrupted", async () => {
      const controller = new AbortController();
      const provider = new ScriptedProvider([useTools(sub("a", 5), sub("b", 5), sub("c", 5)), say("never")]);
      const history: ChatTurn[] = [{ role: "user", text: "go" }];
      const events = await run(provider, history, { signal: controller.signal, runTool: escOn(controller, "a"), isParallel, maxParallel: 1 });
      expect(history.filter((t) => t.role === "tool").map((t) => t.role === "tool" && t.text)).toEqual([
        "out a",
        "Interrupted by the user before this tool ran.",
        "Interrupted by the user before this tool ran.",
      ]);
      expect(events.at(-1)).toEqual({ type: "done", reason: "aborted" });
    });

    test("after Esc, the calls after the group read as interrupted too", async () => {
      const controller = new AbortController();
      const provider = new ScriptedProvider([useTools(sub("a", 5), call("r", "x.ts")), say("never")]);
      const history: ChatTurn[] = [{ role: "user", text: "go" }];
      await run(provider, history, { signal: controller.signal, runTool: escOn(controller, "a"), isParallel });
      expect(history.filter((t) => t.role === "tool").map((t) => t.role === "tool" && t.text)).toEqual([
        "out a",
        "Interrupted by the user before this tool ran.",
      ]);
    });

    test("an agent call is labeled with its type and description while it runs", async () => {
      const typed: ToolCall = { id: "t", name: "agent", arguments: JSON.stringify({ type: "explore", description: "Find the config" }) };
      const provider = new ScriptedProvider([useTools(sub("Read notes", 0), typed), say("ok")]);
      const { runTool } = tracked();
      const events = await run(provider, [{ role: "user", text: "go" }], { runTool, isParallel });
      expect(events.filter((e) => e.type === "tool_start").map((e) => e.type === "tool_start" && e.label)).toEqual([
        "general-purpose · Read notes",
        "explore · Find the config",
      ]);
    });
  });
  test("a tool's diff is for the transcript: the model gets only the output", async () => {
    const provider = new ScriptedProvider([useTools(call("c1", "a.ts")), say("Done.")]);
    const withDiff = async (c: ToolCall) => ({ ...(await fakeTool(c)), diff: { lines: [{ kind: "add" as const, text: "SECRET", newLine: 1 }], more: 0 } });
    await run(provider, [{ role: "user", text: "go" }], { runTool: withDiff });
    expect(JSON.stringify(provider.requests[1]!.history)).not.toContain("SECRET");
  });
});

describe("afterStep", () => {
  test("its note goes in after all of the step's tool results, as a message from Marv, before the next request", async () => {
    const provider = new ScriptedProvider([useTools(call("c1", "a.ts"), call("c2", "b.ts")), say("Fixed.")]);
    const history: ChatTurn[] = [{ role: "user", text: "go" }];
    const seen: string[][] = [];
    await run(provider, history, { afterStep: async (results) => (seen.push(results.map((r) => r.output)), "Marv: 1 new error") });
    expect(seen).toEqual([["contents of a.ts", "contents of b.ts"]]);
    const second = provider.requests[1]!.history;
    expect(second.slice(-3).map((t) => t.role)).toEqual(["tool", "tool", "user"]);
    expect(second.at(-1)).toEqual({ role: "user", text: "Marv: 1 new error" });
    // Appended, not inserted: the second request still starts with the first one (prompt cache).
    expect(second.slice(0, provider.requests[0]!.history.length)).toEqual(provider.requests[0]!.history);
  });

  test("null adds nothing", async () => {
    const provider = new ScriptedProvider([useTools(call("c1", "a.ts")), say("Done.")]);
    let calls = 0;
    await run(provider, [{ role: "user", text: "go" }], { afterStep: async () => (calls++, null) });
    expect(calls).toBe(1);
    expect(provider.requests[1]!.history.at(-1)?.role).toBe("tool");
  });

  test("not called after a plain answer", async () => {
    let calls = 0;
    await run(new ScriptedProvider([say("Hi.")]), [{ role: "user", text: "hi" }], { afterStep: async () => (calls++, null) });
    expect(calls).toBe(0);
  });

  test("stopped during it: the run ends there, every call answered, and the note isn't added", async () => {
    const stop = new AbortController();
    const provider = new ScriptedProvider([useTools(call("c1", "a.ts")), say("never asked")]);
    const history: ChatTurn[] = [{ role: "user", text: "go" }];
    const events = await run(provider, history, {
      signal: stop.signal,
      afterStep: async () => {
        stop.abort();
        return "Marv: too late";
      },
    });
    expect(events.at(-1)).toEqual({ type: "done", reason: "aborted" });
    expect(provider.requests).toHaveLength(1);
    expect(history.at(-1)?.role).toBe("tool");
  });

  test("not called after a step whose call was declined", async () => {
    let calls = 0;
    const provider = new ScriptedProvider([useTools(call("c1", "a.ts")), say("never asked")]);
    const events = await run(provider, [{ role: "user", text: "go" }], {
      runTool: async (c) => ({ ...(await fakeTool(c)), declined: true }),
      afterStep: async () => (calls++, null),
    });
    expect(events.at(-1)).toEqual({ type: "done", reason: "declined" });
    expect(calls).toBe(0);
  });

  test("not called after a cut-off reply", async () => {
    let calls = 0;
    const provider = new ScriptedProvider([
      [{ type: "tool_call", call: call("c1", "a.ts") }, { type: "done", reason: "length" }],
      say("Smaller steps, then."),
    ]);
    await run(provider, [{ role: "user", text: "go" }], { afterStep: async () => (calls++, null) });
    expect(calls).toBe(0);
  });

  test("a parallel group's results arrive in call order", async () => {
    const provider = new ScriptedProvider([useTools(call("a", "a.ts"), call("b", "b.ts")), say("Done.")]);
    const seen: string[][] = [];
    await run(provider, [{ role: "user", text: "go" }], {
      isParallel: () => true,
      runTool: async (c) => {
        if (c.id === "a") await Bun.sleep(40);
        return fakeTool(c);
      },
      afterStep: async (results) => (seen.push(results.map((r) => r.output)), null),
    });
    expect(seen).toEqual([["contents of a.ts", "contents of b.ts"]]);
  });

  test("at the step limit, the note is already in the history when onLimit is asked", async () => {
    const provider = new ScriptedProvider([useTools(call("c1", "a.ts")), say("never")]);
    const history: ChatTurn[] = [{ role: "user", text: "go" }];
    let last: ChatTurn | undefined;
    await run(provider, history, {
      maxSteps: 1,
      afterStep: async () => "Marv: note",
      onLimit: async () => ((last = history.at(-1)), false),
    });
    expect(last).toEqual({ role: "user", text: "Marv: note" });
  });
});

// Issue #5: whatever ends a run, the history it leaves must be one the API accepts. Each step mixes plain
// calls with a parallel group of agent calls (a slow one in the middle), so the groups are covered too.
describe("every tool call gets exactly one result, in call order, however the run ends", () => {
  const sub = (id: string, ms = 5): ToolCall => ({ id, name: "agent", arguments: JSON.stringify({ description: id, ms }) });
  const isParallel = (c: ToolCall) => c.name === "agent";
  const mixed = (n: string): ToolCall[] => [call(`${n}r1`, "a.ts"), sub(`${n}a`), sub(`${n}b`, 30), sub(`${n}c`), call(`${n}r2`, "b.ts")];
  const cutOff = (calls: ToolCall[]): AgentEvent[] => [...calls.map((c) => ({ type: "tool_call" as const, call: c })), { type: "done", reason: "length" }];
  const steps = (n: number) => Array.from({ length: n }, (_, i) => useTools(...mixed(`s${i + 1}`)));

  interface Drive {
    /** Calls to abort on (ctrl+c while it runs), and to answer "no" (both: Esc at an approval). */
    abortOn?: string;
    declineOn?: string;
    /** Where the consumer stops: it breaks out of its loop, or throws while handling the event. */
    stopAt?: (e: LoopEvent) => "break" | "throw" | undefined;
  }

  async function drive(script: AgentEvent[][], { abortOn, declineOn, stopAt }: Drive = {}, opts: Partial<Parameters<typeof runAgent>[0]> = {}) {
    const controller = new AbortController();
    const history: ChatTurn[] = [{ role: "user", text: "go" }];
    const events: LoopEvent[] = [];
    const runTool = async (c: ToolCall) => {
      if (c.id === abortOn) controller.abort();
      const { ms = 0 } = JSON.parse(c.arguments) as { ms?: number };
      await Bun.sleep(ms);
      return { output: `out ${c.id}`, summary: "ok", label: c.id, ...(c.id === declineOn ? { declined: true } : {}) };
    };
    const loop = runAgent({
      provider: new ScriptedProvider(script),
      history,
      system: "S",
      tools: SPECS,
      runTool,
      signal: controller.signal,
      isParallel,
      maxParallel: 2,
      ...opts,
    });
    let thrown: unknown;
    try {
      for await (const event of loop) {
        events.push(event);
        const stop = stopAt?.(event);
        if (stop === "break") break;
        if (stop === "throw") throw new Error("the consumer failed");
      }
    } catch (err) {
      thrown = err;
    }
    await Bun.sleep(50); // tools still running after the consumer left finish now: they must not add anything
    const calls = history.flatMap((t) => (t.role === "assistant" ? (t.toolCalls ?? []) : []));
    return { history, events, thrown, calls, end: events.at(-1) };
  }

  test("a run that answers (the baseline)", async () => {
    const { history, end, calls } = await drive([...steps(2), say("Done.")]);
    expect(calls).toHaveLength(10);
    expect(answerProblems(history)).toEqual([]);
    expect(end).toEqual({ type: "done", reason: "end" });
  });

  describe("ctrl+c mid-tools", () => {
    for (const id of ["s2r1", "s2a", "s2b", "s2r2"]) {
      test(`while ${id} runs`, async () => {
        const { history, end } = await drive([...steps(2), say("never")], { abortOn: id });
        expect(answerProblems(history)).toEqual([]);
        expect(end).toEqual({ type: "done", reason: "aborted" });
      });
    }
  });

  describe("a no from the approver", () => {
    for (const id of ["s2r1", "s2a", "s2b", "s2r2"]) {
      test(`to ${id}`, async () => {
        const { history, end } = await drive([...steps(2), say("never")], { declineOn: id });
        expect(answerProblems(history)).toEqual([]);
        expect(end).toEqual({ type: "done", reason: "declined" });
      });
    }

    test("Esc at an approval in the group (a no and an abort)", async () => {
      const { history, end } = await drive([...steps(2), say("never")], { declineOn: "s2b", abortOn: "s2b" });
      expect(answerProblems(history)).toEqual([]);
      expect(end).toEqual({ type: "done", reason: "aborted" });
    });
  });

  describe("a consumer that stops", () => {
    const at = (type: LoopEvent["type"], id: string) => (e: LoopEvent) =>
      e.type === type && "call" in e && e.call.id === id;
    const cases: [string, (e: LoopEvent) => boolean][] = [
      ["at a plain call's tool_start", at("tool_start", "s2r1")],
      ["at a group's first tool_start (the other has a slot)", at("tool_start", "s2a")],
      ["at a group's tool_end, with a call still running", at("tool_end", "s2a")],
      ["at the last call's tool_end", at("tool_end", "s2r2")],
    ];
    for (const [name, match] of cases) {
      for (const how of ["break", "throw"] as const) {
        test(`${how}s ${name}`, async () => {
          const { history, thrown } = await drive([...steps(2), say("never")], { stopAt: (e) => (match(e) ? how : undefined) });
          expect(thrown !== undefined).toBe(how === "throw");
          expect(answerProblems(history)).toEqual([]);
        });
      }
    }

    test("at a tool_skipped after a no", async () => {
      const { history, events } = await drive([...steps(2), say("never")], { declineOn: "s2r1", stopAt: (e) => (e.type === "tool_skipped" ? "break" : undefined) });
      expect(events.at(-1)?.type).toBe("tool_skipped");
      expect(answerProblems(history)).toEqual([]);
    });
  });

  describe("a reply cut off at the output limit", () => {
    test("once: its calls are answered, and the run goes on", async () => {
      const { history, end } = await drive([...steps(1), cutOff(mixed("s2")), say("Smaller steps.")]);
      expect(answerProblems(history)).toEqual([]);
      expect(end).toEqual({ type: "done", reason: "end" });
    });

    test("twice in a row: the run ends there", async () => {
      const { history, end, calls } = await drive([...steps(1), cutOff(mixed("s2")), cutOff(mixed("s3"))]);
      expect(calls).toHaveLength(15);
      expect(answerProblems(history)).toEqual([]);
      expect(end).toEqual({ type: "done", reason: "length" });
    });

    test("a consumer that stops at a cut-off call's tool_skipped", async () => {
      const { history } = await drive([cutOff(mixed("s1"))], { stopAt: (e) => (e.type === "tool_skipped" ? "throw" : undefined) });
      expect(answerProblems(history)).toEqual([]);
    });
  });

  describe("the step limit", () => {
    test("without onLimit: a hard stop", async () => {
      const { history, end, calls } = await drive(steps(5), {}, { maxSteps: 2 });
      expect(calls).toHaveLength(10);
      expect(answerProblems(history)).toEqual([]);
      expect(end).toEqual({ type: "done", reason: "max_steps" });
    });

    test("with onLimit: a no stops it", async () => {
      const { history, end } = await drive(steps(5), {}, { maxSteps: 2, onLimit: async () => false });
      expect(answerProblems(history)).toEqual([]);
      expect(end).toEqual({ type: "done", reason: "max_steps" });
    });

    test("with onLimit: a yes goes on to the next limit", async () => {
      const { history, end, calls } = await drive(steps(5), {}, { maxSteps: 2, onLimit: async (n) => n < 4 });
      expect(calls).toHaveLength(20);
      expect(answerProblems(history)).toEqual([]);
      expect(end).toEqual({ type: "done", reason: "max_steps" });
    });

    test("with onLimit: Esc while it asks", async () => {
      const stop = new AbortController();
      const { history, end } = await drive(steps(5), {}, { maxSteps: 2, signal: stop.signal, onLimit: async () => (stop.abort(), false) });
      expect(answerProblems(history)).toEqual([]);
      expect(end).toEqual({ type: "done", reason: "aborted" });
    });

    test("a consumer that stops at the step_limit event", async () => {
      const { history, events } = await drive(steps(5), { stopAt: (e) => (e.type === "step_limit" ? "throw" : undefined) }, { maxSteps: 2, onLimit: async () => true });
      expect(events.at(-1)?.type).toBe("step_limit");
      expect(answerProblems(history)).toEqual([]);
    });
  });
});
