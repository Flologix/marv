import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { AgentType } from "../src/agents.ts";
import type { McpManager } from "../src/mcp/manager.ts";
import { addMemory, loadMemory, memoryPaths } from "../src/memory.ts";
import type { ProviderFactory, ProviderOption } from "../src/provider/factory.ts";
import type { ModelInfo } from "../src/provider/models.ts";
import type { AgentEvent, ChatTurn, Provider, StreamOptions, ToolCall, ToolSpec } from "../src/provider/types.ts";
import { Diagnostics, type RunCheck } from "../src/diagnostics/turn.ts";
import type { CommandResult } from "../src/tools/bash.ts";
import { MarvSession, type SessionEvent, type SessionInit } from "../src/session.ts";
import { SessionStore, type SavedSession } from "../src/sessions.ts";
import { TrajectoryStore } from "../src/trajectory.ts";
import type { ApprovalRequest, Decision, Tool } from "../src/tools/types.ts";
import { toolSpecsFor } from "../src/tools/index.ts";
import { MID_TURN_NOTE } from "../src/compact.ts";
import { readInstructions } from "../src/prompt.ts";
import { fakeTool } from "./mcp-fakes.ts";
import { answerProblems } from "./answered.ts";
import { RoutedProvider, ScriptedProvider } from "./fake-provider.ts";
import { sandboxAvailable } from "../src/sandbox.ts";
import { z } from "zod";

const say = (text: string): AgentEvent[] => [
  { type: "text_delta", text },
  { type: "done", reason: "stop" },
];
const useTools = (...calls: ToolCall[]): AgentEvent[] => [...calls.map((c) => ({ type: "tool_call" as const, call: c })), { type: "done", reason: "tool_calls" }];
const call = (id: string, name: string, args: Record<string, unknown>): ToolCall => ({ id, name, arguments: JSON.stringify(args) });
const read = (id: string) => call(id, "read_file", { path: id });
const fixed = (provider: Provider, extra: Partial<ProviderFactory> = {}): ProviderFactory => ({ id: "test", model: "test-model", make: () => provider, ...extra });

async function collect(events: AsyncIterable<SessionEvent>): Promise<SessionEvent[]> {
  const out: SessionEvent[] = [];
  for await (const event of events) out.push(event);
  return out;
}
const types = (events: SessionEvent[]) => events.map((e) => e.type);

/** Tool calls in a history without a result: a request with one is rejected by the API. */
function unanswered(history: ChatTurn[]): string[] {
  const calls = history.flatMap((t) => (t.role === "assistant" ? (t.toolCalls ?? []).map((c) => c.id) : []));
  const answered = new Set(history.flatMap((t) => (t.role === "tool" ? [t.callId] : [])));
  return calls.filter((id) => !answered.has(id));
}

/** A model that says something, then works until it's stopped. */
class Hanging implements Provider {
  readonly name = "hanging";
  requests = 0;
  async *stream(_history: ChatTurn[], { signal }: StreamOptions = {}): AsyncIterable<AgentEvent> {
    this.requests++;
    yield { type: "text_delta", text: "Working" };
    await new Promise<void>((resolve) => (signal?.aborted ? resolve() : signal?.addEventListener("abort", () => resolve(), { once: true })));
    yield { type: "done" };
  }
}

let project: string;
let dir: string;
beforeEach(async () => {
  project = await mkdtemp(join(tmpdir(), "marv-session-project-"));
  dir = await mkdtemp(join(tmpdir(), "marv-session-"));
  await writeFile(join(project, "notes.txt"), "remember the milk\n");
});
afterEach(async () => {
  await rm(project, { recursive: true, force: true });
  await rm(dir, { recursive: true, force: true });
});

function makeSession(provider: Provider | ProviderFactory, init: Partial<SessionInit> = {}): MarvSession {
  return new MarvSession({ root: project, provider: "make" in provider ? provider : fixed(provider), ...init });
}

describe("a turn", () => {
  test("starts with turn_start, ends with turn_end, and streams the reply in between", async () => {
    const events = await collect(makeSession(new ScriptedProvider([say("Hello.")])).send("hi"));
    expect(types(events)).toEqual(["turn_start", "status", "text_delta", "assistant", "done", "turn_end"]);
    const start = events[0] as Extract<SessionEvent, { type: "turn_start" }>;
    expect(events.at(-1)).toEqual({ type: "turn_end", turn: start.turn, reason: "end" });
  });

  test("one at a time: send() during a turn throws; the next can start once it ended", async () => {
    const provider = new ScriptedProvider([say("One."), say("Two.")]);
    const session = makeSession(provider);
    const first = session.send("one");
    expect(session.busy).toBe(true);
    expect(() => session.send("two")).toThrow(/working/);
    await collect(first);
    expect(session.busy).toBe(false);
    await collect(session.send("two"));
    expect(provider.requests.map((r) => r.history.at(-1)?.text)).toEqual(["one", "two"]);
  });

  test("interrupt() stops the running turn", async () => {
    const session = makeSession(new Hanging());
    const events: SessionEvent[] = [];
    for await (const event of session.send("go")) {
      events.push(event);
      if (event.type === "text_delta") session.interrupt();
    }
    expect(events).toContainEqual({ type: "done", reason: "aborted" });
    expect(events.at(-1)).toMatchObject({ type: "turn_end", reason: "aborted" });
  });

  test("so does the signal passed to send()", async () => {
    const stop = new AbortController();
    const events: SessionEvent[] = [];
    for await (const event of makeSession(new Hanging()).send("go", { signal: stop.signal })) {
      events.push(event);
      if (event.type === "text_delta") stop.abort();
    }
    expect(events.at(-1)).toMatchObject({ type: "turn_end", reason: "aborted" });
  });

  test("leaving the loop early interrupts the turn, and every tool call still gets a result", async () => {
    const provider = new ScriptedProvider([useTools(read("c1"), read("c2")), say("Next.")]);
    const session = makeSession(provider);
    for await (const event of session.send("read it twice")) if (event.type === "tool_start") break;
    expect(session.busy).toBe(false);
    await collect(session.send("next"));
    expect(unanswered(provider.requests[1]!.history)).toEqual([]);
  });

  test("usage adds up, and the last request's size is kept", async () => {
    const provider = new ScriptedProvider([[{ type: "usage", usage: { promptTokens: 100, completionTokens: 20 } }, ...say("Hi.")]]);
    const session = makeSession(fixed(provider, { local: true }));
    await collect(session.send("hi"));
    expect(session.usage()).toMatchObject({
      last: { promptTokens: 100, completionTokens: 20 },
      totals: { requests: 1, promptTokens: 100, completionTokens: 20, local: true },
    });
  });

  test("a provider error ends the turn with reason error", async () => {
    const events = await collect(makeSession(new ScriptedProvider([[{ type: "error", message: "bad key" }]])).send("hi"));
    expect(events).toContainEqual({ type: "error", message: "bad key" });
    expect(events.at(-1)).toMatchObject({ type: "turn_end", reason: "error" });
  });

  test("a provider that throws ends the turn, not the session", async () => {
    const broken: Provider = {
      name: "broken",
      stream: () => {
        throw new Error("kaput");
      },
    };
    const session = makeSession(broken);
    const events = await collect(session.send("hi"));
    expect(events).toContainEqual({ type: "error", message: "Error: kaput" });
    expect(events.at(-1)).toMatchObject({ type: "turn_end", reason: "error" });
    expect(session.busy).toBe(false);
  });

  test("systemPrompt replaces Marv's, or adds to it", async () => {
    const replaced = new ScriptedProvider([say("ok")]);
    await collect(makeSession(replaced, { systemPrompt: "You are a pirate." }).send("hi"));
    expect(replaced.requests[0]!.options.system).toBe("You are a pirate.");
    const appended = new ScriptedProvider([say("ok")]);
    await collect(makeSession(appended, { systemPrompt: { append: "Answer briefly." } }).send("hi"));
    expect(appended.requests[0]!.options.system).toStartWith("You are Marv");
    expect(appended.requests[0]!.options.system).toEndWith("Answer briefly.");
  });
});

describe("approvals", () => {
  test("without an approver, a yolo-safe edit runs, anything else is refused, and the turn goes on", async () => {
    const provider = new ScriptedProvider([
      useTools(call("c1", "write_file", { path: "a.txt", content: "hi\n" }), call("c2", "bash", { command: "curl example.com", network: true })),
      say("Done."),
    ]);
    const events = await collect(makeSession(provider).send("go"));
    expect(await readFile(join(project, "a.txt"), "utf8")).toBe("hi\n");
    const refused = events.find((e) => e.type === "tool_end" && e.call.id === "c2");
    expect(refused).toMatchObject({ result: { isError: true, output: expect.stringContaining("no one to ask") } });
    expect(events).toContainEqual({ type: "done", reason: "end" });
  });

  test("'always' covers that scope for the rest of the session, later turns included", async () => {
    const asked: string[] = [];
    const provider = new ScriptedProvider([
      useTools(call("c1", "write_file", { path: "a.txt", content: "1" })),
      say("One."),
      useTools(call("c2", "write_file", { path: "b.txt", content: "2" })),
      say("Two."),
    ]);
    const session = makeSession(provider, { yolo: false, approve: async (r) => (asked.push(r.label), "always") });
    await collect(session.send("one"));
    await collect(session.send("two"));
    expect(asked).toEqual(["a.txt"]);
    expect(existsSync(join(project, "b.txt"))).toBe(true);
  });

  test("stopping the turn answers a pending approval with no", async () => {
    const provider = new ScriptedProvider([useTools(call("c1", "write_file", { path: "a.txt", content: "1" }))]);
    let session: MarvSession | undefined;
    session = makeSession(provider, {
      yolo: false,
      approve: () => {
        session!.interrupt();
        return new Promise<Decision>(() => {}); // never answers: only the stop can
      },
    });
    const events = await collect(session.send("go"));
    expect(events).toContainEqual(expect.objectContaining({ type: "tool_end", result: expect.objectContaining({ declined: true, summary: "interrupted" }) }));
    expect(existsSync(join(project, "a.txt"))).toBe(false);
    expect(events.at(-1)).toMatchObject({ type: "turn_end", reason: "aborted" });
  });

  test("without an approver it stops at the step limit", async () => {
    const provider = new ScriptedProvider(Array.from({ length: 30 }, (_, i) => useTools(read(`c${i}`))));
    const events = await collect(makeSession(provider).send("loop"));
    expect(provider.requests).toHaveLength(25);
    expect(events).toContainEqual({ type: "done", reason: "max_steps" });
  });

  test("maxSteps moves the step limit", async () => {
    const provider = new ScriptedProvider(Array.from({ length: 40 }, (_, i) => useTools(read(`c${i}`))));
    const events = await collect(makeSession(provider, { maxSteps: 30 }).send("loop"));
    expect(provider.requests).toHaveLength(30);
    expect(events).toContainEqual({ type: "done", reason: "max_steps" });
  });

  test("the step-limit question says when it asks again", async () => {
    const provider = new ScriptedProvider(Array.from({ length: 10 }, (_, i) => useTools(read(`c${i}`))));
    const asked: ApprovalRequest[] = [];
    await collect(makeSession(provider, { maxSteps: 4, approve: async (r) => (asked.push(r), "no") }).send("loop"));
    expect(asked[0]?.preview.note).toContain("after another 4");
  });

  test("a maxSteps that isn't a positive whole number throws", () => {
    for (const maxSteps of [-1, 2.5, Number.NaN]) expect(() => makeSession(new ScriptedProvider([]), { maxSteps })).toThrow(/maxSteps/);
  });

  test("maxSteps 0 turns the asking off (the subagents' hard limit stays)", () => {
    expect(() => makeSession(new ScriptedProvider([]), { maxSteps: 0 })).not.toThrow();
  });

  test("maxSteps 0 (off) runs past the default 25 without asking, even with an approver", async () => {
    const provider = new ScriptedProvider([...Array.from({ length: 30 }, (_, i) => useTools(read(`c${i}`))), say("Finally done.")]);
    const asked: ApprovalRequest[] = [];
    const events = await collect(
      makeSession(provider, { maxSteps: 0, approve: async (r) => (asked.push(r), "no") }).send("loop"),
    );
    expect(provider.requests).toHaveLength(31); // 30 tool steps, then the plain answer
    expect(asked).toEqual([]);
    expect(events.filter((e) => e.type === "step_limit")).toEqual([]);
    expect(events).toContainEqual({ type: "done", reason: "end" });
  });

  test("configure({ maxSteps }) moves the limit from the next turn on", async () => {
    const provider = new ScriptedProvider(Array.from({ length: 30 }, (_, i) => useTools(read(`c${i}`))));
    const session = makeSession(provider);
    await collect(session.send("first")); // stops at the default 25
    session.configure({ maxSteps: 0 });
    const events = await collect(session.send("loop"));
    expect(events).toContainEqual({ type: "done", reason: "end" });
    await session.close();
  });

  test("with an approver it asks at the step limit, in the continue scope", async () => {
    const provider = new ScriptedProvider(Array.from({ length: 30 }, (_, i) => useTools(read(`c${i}`))));
    const asked: ApprovalRequest[] = [];
    const events = await collect(makeSession(provider, { approve: async (r) => (asked.push(r), "no") }).send("loop"));
    expect(asked.map((r) => r.scope.key)).toEqual(["continue"]);
    expect(events).toContainEqual({ type: "step_limit", steps: 25, continued: false });
  });
});

/** Stands in for McpManager: the parts a session uses. */
function fakeMcp(ready: Promise<void>, specs: ToolSpec[] = [], tools: Tool[] = []): McpManager {
  const mcp = { settled: false, ready, specs, tools, status: () => [] };
  void ready.then(() => (mcp.settled = true));
  return mcp as unknown as McpManager;
}

describe("MCP servers", () => {
  test("the first request waits for them, and offers their tools", async () => {
    let start!: () => void;
    const spec: ToolSpec = { name: "mcp__x__ping", description: "[x] ping", parameters: { type: "object" } };
    const provider = new ScriptedProvider([say("Pong.")]);
    const session = makeSession(provider, { mcp: fakeMcp(new Promise<void>((resolve) => (start = resolve)), [spec]) });
    for await (const event of session.send("ping")) {
      if (event.type === "status" && event.status === "waiting_for_mcp") {
        expect(provider.requests).toHaveLength(0);
        start();
      }
    }
    expect(provider.requests[0]!.options.tools).toContainEqual(spec);
  });

  test("interrupting the wait ends the turn before anything is sent", async () => {
    const provider = new ScriptedProvider([say("never")]);
    const session = makeSession(provider, { mcp: fakeMcp(new Promise<void>(() => {})) });
    const events: SessionEvent[] = [];
    for await (const event of session.send("ping")) {
      events.push(event);
      if (event.type === "status") session.interrupt();
    }
    expect(types(events)).toEqual(["turn_start", "status", "turn_end"]);
    expect(events.at(-1)).toMatchObject({ reason: "interrupted" });
    expect(provider.requests).toHaveLength(0);
  });
});

describe("compaction", () => {
  const used = (promptTokens: number): AgentEvent => ({ type: "usage", usage: { promptTokens, completionTokens: 0 } });

  test("a nearly full context is summarized before the next message", async () => {
    const provider = new ScriptedProvider([[used(900), ...say("First answer.")], say("SUMMARY of it all"), say("Second answer.")], 1000);
    const session = makeSession(provider);
    await collect(session.send("first"));
    const events = await collect(session.send("second"));
    expect(events).toContainEqual({ type: "status", status: "compacting" });
    expect(events).toContainEqual({ type: "compaction", result: { compacted: true, summary: "SUMMARY of it all", tokensBefore: 900 } });
    const after = provider.requests[2]!.history;
    expect(after[0]!.text).toContain("SUMMARY of it all");
    expect(after.at(-1)).toEqual({ role: "user", text: "second" });
    // Before a message, the message itself says what to do next: no mid-turn "continue" note in between.
    expect(after.map((turn) => turn.role)).toEqual(["user", "assistant", "user"]);
  });

  test("stopping the compaction stops the turn, and the message isn't sent", async () => {
    const first = new ScriptedProvider([[used(900), ...say("First answer.")]]);
    const hanging = new Hanging();
    let requests = 0;
    const provider: Provider = {
      name: "first, then hanging",
      contextLength: 1000,
      stream: (history, options) => (++requests === 1 ? first.stream(history, options) : hanging.stream(history, options)),
    };
    const session = makeSession(provider);
    await collect(session.send("first"));
    const events: SessionEvent[] = [];
    for await (const event of session.send("second")) {
      events.push(event);
      if (event.type === "status" && event.status === "compacting") session.interrupt();
    }
    expect(events).toContainEqual({ type: "compaction", result: { compacted: false, reason: "stopped", error: "Stopped." } });
    expect(events.at(-1)).toMatchObject({ type: "turn_end", reason: "interrupted" });
    expect(requests).toBe(2); // the first answer and the summary: "second" was never sent
  });

  test("compact(): nothing to do on an empty conversation; afterwards the next request starts from the summary", async () => {
    const provider = new ScriptedProvider([say("Answer."), say("THE SUMMARY"), say("Next.")]);
    const session = makeSession(provider);
    expect(await session.compact()).toEqual({ compacted: false, reason: "empty", error: "Nothing to compact yet." });
    await collect(session.send("first"));
    expect(await session.compact("the plan")).toMatchObject({ compacted: true, summary: "THE SUMMARY" });
    expect(provider.requests[1]!.history.at(-1)!.text).toContain("Focus especially on: the plan");
    await collect(session.send("next"));
    expect(provider.requests[2]!.history[0]!.text).toContain("THE SUMMARY");
  });

  test("a long turn compacts between steps, and continues from the summary", async () => {
    const provider = new ScriptedProvider([[used(900), ...useTools(read("c1"))], say("SUMMARY of it all"), say("Finished.")], 1000);
    const session = makeSession(provider);
    const events = await collect(session.send("first"));
    expect(events.filter((e) => e.type === "status" && e.status === "compacting")).toHaveLength(1);
    expect(events).toContainEqual({ type: "compacted" });
    expect(events).toContainEqual({ type: "compaction", result: { compacted: true, summary: "SUMMARY of it all", tokensBefore: 900 } });
    expect(events.at(-1)).toMatchObject({ type: "turn_end", reason: "end" });
    // The summary request carried the whole conversation, tool result included.
    const summaryRequest = provider.requests[1]!.history;
    expect(summaryRequest.at(-1)!.text).toContain("Summarize our conversation");
    expect(summaryRequest.at(-2)!.role).toBe("tool");
    // The next request starts from the summary: the conversation was swapped under the loop.
    const after = provider.requests[2]!.history;
    expect(after.map((t) => t.role)).toEqual(["user", "assistant", "user"]);
    expect(after[0]!.text).toContain("SUMMARY of it all");
    // The compacted history ends with the model's own "Got it.", so Marv's note to go on comes last:
    // the next request must not end with an assistant turn.
    expect(after.at(-1)!.text).toContain("Continue where you left off");
    expect(answerProblems(after)).toEqual([]);
  });

  test("a failed mid-turn compaction leaves the history as it was, and the turn goes on", async () => {
    const provider = new ScriptedProvider([[used(900), ...useTools(read("c1"))], [{ type: "error", message: "summary failed" }], [used(900), ...useTools(read("c2"))], say("Finished.")], 1000);
    const session = makeSession(provider);
    const events = await collect(session.send("first"));
    expect(events).toContainEqual({ type: "compaction", result: { compacted: false, reason: "failed", error: "summary failed" } });
    expect(events).toContainEqual({ type: "assistant", text: "Finished." });
    expect(events.at(-1)).toMatchObject({ type: "turn_end", reason: "end" });
    expect(provider.requests[2]!.history.at(-1)!.role).toBe("tool"); // not replaced by the summary
    expect(answerProblems(provider.requests[2]!.history)).toEqual([]);
    // Still nearly full after the next step, but the failed summary isn't tried again within the turn:
    // every retry would be another paid request that likely fails the same way.
    expect(events.filter((e) => e.type === "status" && e.status === "compacting")).toHaveLength(1);
    const summaryRequests = provider.requests.filter((r) => (r.history.at(-1)?.text ?? "").startsWith("Summarize our conversation"));
    expect(summaryRequests).toHaveLength(1);
  });

  test("stopping a mid-turn compaction stops the turn", async () => {
    const first = new ScriptedProvider([[used(900), ...useTools(read("c1"))]]);
    const hanging = new Hanging();
    let requests = 0;
    const provider: Provider = {
      name: "first, then hanging",
      contextLength: 1000,
      stream: (history, options) => (++requests === 1 ? first.stream(history, options) : hanging.stream(history, options)),
    };
    const session = makeSession(provider);
    const events: SessionEvent[] = [];
    for await (const event of session.send("first")) {
      events.push(event);
      if (event.type === "status" && event.status === "compacting") session.interrupt();
    }
    expect(events).toContainEqual({ type: "compaction", result: { compacted: false, reason: "stopped", error: "Stopped." } });
    // The turn was already running, so this is an abort, not the pre-message "interrupted".
    expect(events.at(-1)).toMatchObject({ type: "turn_end", reason: "aborted" });
    expect(requests).toBe(2); // the first answer and the summary: the loop never went on
  });
});

/** A parent that starts one subagent ("look around"), and the subagent, which finds the notes. */
const withSubagent = () =>
  new RoutedProvider({
    "parent-task": new ScriptedProvider([useTools(call("a1", "agent", { description: "look around", prompt: "sub-task: find the notes" })), say("Parent done.")]),
    "sub-task": new ScriptedProvider([say("Found them.")]),
  });

describe("subagents", () => {
  test("their events arrive in the same stream, tagged with the call that started them", async () => {
    const events = await collect(makeSession(withSubagent()).send("parent-task: go"));
    const start = events.findIndex((e) => e.type === "tool_start" && e.call.id === "a1");
    const end = events.findIndex((e) => e.type === "tool_end" && e.call.id === "a1");
    const sub = events.flatMap((e, i) => (e.type === "subagent" ? [{ i, e }] : []));
    expect(sub.length).toBeGreaterThan(0);
    expect(sub.every(({ e }) => e.callId === "a1")).toBe(true);
    expect(sub.some(({ e }) => e.event.type === "assistant" && e.event.text === "Found them.")).toBe(true);
    expect(sub.every(({ i }) => i > start && i < end)).toBe(true);
    expect(events).toContainEqual(expect.objectContaining({ type: "subagent_progress", callId: "a1" }));
    expect(events.at(-1)).toMatchObject({ type: "turn_end", reason: "end" });
  });
});

// Issue #5: every way a turn ends leaves each tool call answered once, in call order, with real subagents too.
describe("every tool call gets a result, with parallel subagents", () => {
  /** Each parent step: a read, two subagents (in parallel), a read. */
  const parallel = (sub: Provider, steps = 1) => {
    const parent = new ScriptedProvider([
      ...Array.from({ length: steps }, (_, i) =>
        useTools(read(`r${i}`), call(`a${i}`, "agent", { description: "one", prompt: "sub-task: one" }), call(`b${i}`, "agent", { description: "two", prompt: "sub-task: two" }), read(`z${i}`)),
      ),
      say("Next."),
    ]);
    return { parent, provider: new RoutedProvider({ "parent-task": parent, "sub-task": sub }) };
  };

  for (const how of ["break", "throw"] as const) {
    test(`a client that ${how}s while subagents run`, async () => {
      const { parent, provider } = parallel(new Hanging());
      const session = makeSession(provider);
      const failed = new Error("the client failed");
      let stoppedAt: SessionEvent | undefined;
      try {
        for await (const event of session.send("parent-task: go")) {
          if (event.type !== "subagent") continue;
          stoppedAt = event;
          if (how === "break") break;
          throw failed;
        }
      } catch (err) {
        if (err !== failed) throw err;
      }
      expect(stoppedAt).toMatchObject({ type: "subagent", callId: expect.stringMatching(/^[ab]0$/) });
      expect(session.busy).toBe(false);
      await collect(session.send("next"));
      const next = parent.requests.at(-1)!.history;
      expect(next.some((t) => t.role === "tool" && t.callId === "b0")).toBe(true);
      expect(answerProblems(next)).toEqual([]);
    });
  }

  test("an interrupt while subagents run", async () => {
    const { parent, provider } = parallel(new Hanging());
    const session = makeSession(provider);
    const events: SessionEvent[] = [];
    for await (const event of session.send("parent-task: go")) {
      events.push(event);
      if (event.type === "subagent") session.interrupt();
    }
    expect(events.at(-1)).toMatchObject({ type: "turn_end", reason: "aborted" });
    await collect(session.send("next"));
    expect(answerProblems(parent.requests.at(-1)!.history)).toEqual([]);
  });

  test("the step limit, with and without an approver", async () => {
    for (const approve of [undefined, async () => "no" as const]) {
      const { parent, provider } = parallel(new ScriptedProvider(Array.from({ length: 10 }, () => say("Done."))), 2);
      const session = makeSession(provider, { maxSteps: 2, approve });
      const events = await collect(session.send("parent-task: go"));
      expect(events).toContainEqual({ type: "done", reason: "max_steps" });
      await collect(session.send("next"));
      expect(parent.requests).toHaveLength(3);
      const next = parent.requests[2]!.history;
      expect(next.filter((t) => t.role === "tool")).toHaveLength(8);
      expect(answerProblems(next)).toEqual([]);
    }
  });
});

// Issue #5: with no approver, a worktree dispatch needs a yes no one can give, unless yolo vouches for it (the
// sandbox works); then inside the worktree there's no auto-yes either, only what yolo vouches for runs.
describe("a worktree subagent with no approver", () => {
  const git = (...args: string[]) => Bun.spawnSync(["git", ...args], { cwd: project }).stdout.toString().trim();
  let trees: string;
  beforeEach(() => {
    git("init", "-q", "-b", "main");
    git("config", "user.email", "test@example.com");
    git("config", "user.name", "Test");
    git("add", "-A");
    git("commit", "-q", "-m", "first");
    trees = join(dir, "trees");
  });
  const dispatch = () => {
    const sub = new ScriptedProvider([
      useTools(call("w1", "write_file", { path: "new.txt", content: "new\n" }), call("b1", "bash", { command: "curl example.com", network: true })),
      say("Added new.txt."),
    ]);
    const parent = new ScriptedProvider([useTools(call("a1", "agent", { description: "add file", prompt: "sub-task: add it", isolation: "worktree" })), say("Parent done.")]);
    return { sub, parent, provider: new RoutedProvider({ "parent-task": parent, "sub-task": sub }) };
  };
  const agentResult = (events: SessionEvent[]) => events.find((e) => e.type === "tool_end" && e.call.id === "a1") as Extract<SessionEvent, { type: "tool_end" }>;

  for (const [name, init] of [
    ["yolo off", { yolo: false }],
    ["yolo on, sandbox off", { yolo: true, sandbox: false }],
  ] as const) {
    test(`${name}: the dispatch is refused at once, nothing is created, and the turn goes on`, async () => {
      const { sub, parent, provider } = dispatch();
      const events = await collect(makeSession(provider, { worktreesDir: trees, ...init }).send("parent-task: go"));
      expect(agentResult(events).result).toMatchObject({ isError: true, output: "agent needs the user's approval, and there's no one to ask." });
      expect(sub.requests).toHaveLength(0);
      expect(parent.requests).toHaveLength(2);
      expect(existsSync(trees)).toBe(false);
      expect(git("branch", "--list", "marv/*")).toBe("");
      expect(events.at(-1)).toMatchObject({ type: "turn_end", reason: "end" });
    }, 10_000);
  }

  test.if(sandboxAvailable())("yolo on with the sandbox: it runs; yolo-safe edits land on its branch, a network command is refused", async () => {
    const { sub, provider } = dispatch();
    const events = await collect(makeSession(provider, { worktreesDir: trees, yolo: true }).send("parent-task: go"));
    const { result } = agentResult(events);
    expect(result.isError).toBeFalsy();
    expect(result.output).toStartWith("Added new.txt.");
    const branch = /Marv: Branch (marv\/add-file-[0-9a-f]{4}): 1 commit/.exec(result.output)?.[1];
    expect(branch).toBeDefined();
    expect(git("show", `${branch}:new.txt`)).toBe("new");
    expect(existsSync(join(project, "new.txt"))).toBe(false);
    // Inside the worktree the curl had no one to ask: refused, as a result the subagent read.
    expect(sub.requests[1]!.history.find((t) => t.role === "tool" && t.callId === "b1")).toMatchObject({ text: "bash needs the user's approval, and there's no one to ask." });
    expect(events.at(-1)).toMatchObject({ type: "turn_end", reason: "end" });
  }, 20_000);
});

describe("trajectories", () => {
  async function records(store: TrajectoryStore, session: MarvSession) {
    await session.flush();
    const text = await readFile(store.open(project, session.id).path, "utf8");
    return text
      .trim()
      .split("\n")
      .map((line) => JSON.parse(line) as { type: string; [key: string]: unknown });
  }

  test("a turn is recorded, and the next message's tone rates it", async () => {
    const store = new TrajectoryStore(dir);
    const session = makeSession(new ScriptedProvider([say("Done."), say("Glad to help.")]), { trajectories: store, version: "9.9.9" });
    await collect(session.send("fix it"));
    await collect(session.send("thanks, perfect"));
    const log = await records(store, session);
    expect(log.map((r) => r.type)).toEqual(["session", "turn_start", "assistant", "agent_end", "feedback", "turn_start", "assistant", "agent_end"]);
    expect(log[0]).toMatchObject({ marv: "9.9.9", provider: "test", model: "test-model" });
    expect(log[4]).toMatchObject({ source: "implicit", score: 1, turn: log[1]!.turn });
  });

  test("rate() rates the last turn, and says why when it can't", async () => {
    const store = new TrajectoryStore(dir);
    const session = makeSession(new ScriptedProvider([say("Done.")]), { trajectories: store });
    expect(session.rate({ score: 1 })).toBe("nothing");
    await collect(session.send("fix it"));
    expect(session.rate({ score: -1, note: "wrong file" })).toBe("rated");
    expect((await records(store, session)).at(-1)).toMatchObject({ type: "feedback", source: "explicit", score: -1, note: "wrong file" });
    session.configure({ trajectories: false });
    expect(session.rate({ score: 1 })).toBe("off");
    expect(makeSession(new ScriptedProvider([])).rate({ score: 1 })).toBe("off");
  });

  test("a subagent's steps are recorded under its own agent id", async () => {
    const store = new TrajectoryStore(dir);
    const session = makeSession(withSubagent(), { trajectories: store });
    await collect(session.send("parent-task: go"));
    const log = await records(store, session);
    const start = log.find((r) => r.type === "subagent_start");
    expect(start).toMatchObject({ agent: "main", call: "a1", description: "look around" });
    expect(log).toContainEqual(expect.objectContaining({ type: "assistant", agent: start!.subagent, text: "Found them." }));
    expect(log).toContainEqual(expect.objectContaining({ type: "agent_end", agent: start!.subagent }));
  });
});

describe("saving and resuming", () => {
  test("a turn is saved, and another session object can resume it", async () => {
    const sessions = new SessionStore(dir);
    const a = makeSession(new ScriptedProvider([say("Noted.")]), { sessions });
    await collect(a.send("remember 42"));
    await a.flush();
    const provider = new ScriptedProvider([say("42.")]);
    const b = makeSession(provider, { sessions });
    const resumed = await b.resume("latest");
    expect(resumed).toMatchObject({
      id: a.id,
      model: "test-model",
      transcript: [
        { role: "user", text: "remember 42" },
        { role: "assistant", text: "Noted." },
      ],
    });
    expect(b.id).toBe(a.id);
    await collect(b.send("what was it?"));
    expect(provider.requests[0]!.history.map((t) => t.text)).toEqual(["remember 42", "Noted.", "what was it?"]);
  });

  test("the client's transcript is what's saved", async () => {
    const sessions = new SessionStore(dir);
    const transcript = [
      { id: 7, role: "user" as const, text: "hi" },
      { id: 8, role: "system" as const, text: "a notice" },
    ];
    const session = makeSession(new ScriptedProvider([say("Hello.")]), { sessions, transcript: () => transcript });
    await collect(session.send("hi"));
    await session.flush();
    expect((await sessions.load(project, session.id))?.transcript).toEqual(transcript);
  });

  test("a session file saved by the current version loads", async () => {
    const sessions = new SessionStore(dir);
    const saved: SavedSession = {
      version: 1,
      id: "2026-10-05T10-00-00-000Z-abc123",
      root: project,
      createdAt: 1,
      updatedAt: 2,
      provider: "ollama",
      model: "qwen3.5:9b",
      conversation: [
        { role: "user", text: "hi" },
        { role: "assistant", text: "Hello." },
      ],
      transcript: [
        { id: 1, role: "user", text: "hi" },
        { id: 2, role: "tool", text: "read_file", tool: { label: "a.ts", status: "done" } },
        { id: 3, role: "assistant", text: "Hello." },
      ],
      totals: { requests: 1, promptTokens: 10, cachedTokens: 0, completionTokens: 2 },
    };
    await sessions.save(saved);
    const resumed = await makeSession(new ScriptedProvider([]), { sessions }).resume(saved.id);
    expect(resumed).toMatchObject({ id: saved.id, transcript: saved.transcript, totals: saved.totals });
  });

  test("without a store, or for an unknown id, there's nothing to resume", async () => {
    expect(await makeSession(new ScriptedProvider([])).resume("latest")).toBeNull();
    expect(await makeSession(new ScriptedProvider([]), { sessions: new SessionStore(dir) }).resume("nope")).toBeNull();
  });
});

describe("between turns", () => {
  test("clear(), resume() and compact() refuse during a turn; configure() applies from the next one", async () => {
    const hanging = new Hanging();
    const other = new ScriptedProvider([say("From the other model.")]);
    const session = makeSession(hanging, { sessions: new SessionStore(dir) });
    for await (const event of session.send("go")) {
      if (event.type !== "text_delta") continue;
      expect(() => session.clear()).toThrow(/working/);
      await expect(session.resume("latest")).rejects.toThrow(/working/);
      await expect(session.compact()).rejects.toThrow(/working/);
      session.configure({ provider: fixed(other) });
      session.interrupt();
    }
    expect(hanging.requests).toBe(1);
    await collect(session.send("again"));
    expect(other.requests).toHaveLength(1);
  });

  test("clear() starts a new conversation, with a new id and the memories saved meanwhile", async () => {
    const paths = memoryPaths(dir, project);
    const provider = new ScriptedProvider([say("One."), say("Two.")]);
    const session = makeSession(provider, { memory: { paths, initial: await loadMemory(paths) } });
    await collect(session.send("one"));
    const before = session.id;
    await addMemory(paths.personal, "The user likes tabs");
    await session.clear();
    expect(session.id).not.toBe(before);
    await collect(session.send("two"));
    expect(provider.requests[1]!.history).toEqual([{ role: "user", text: "two" }]);
    expect(provider.requests[0]!.options.system).not.toContain("The user likes tabs");
    expect(provider.requests[1]!.options.system).toContain("The user likes tabs");
  });

  test("the model's info arrives in the background: its context window, and a provider remade for its reasoning", async () => {
    const provider = new ScriptedProvider([]);
    const made: unknown[] = [];
    let changed = 0;
    const factory: ProviderFactory = {
      id: "openrouter",
      model: "x/y",
      make: (_model, info) => (made.push(info), provider),
      lookup: async () => ({ id: "x/y", context: 200_000, reasoning: "optional" }),
    };
    const session = makeSession(factory, { onChange: () => changed++ });
    await Bun.sleep(0);
    expect(session.contextLength).toBe(200_000);
    expect(session.modelInfo?.reasoning).toBe("optional");
    expect(made).toEqual([undefined, { reasoning: "optional" }]);
    expect(changed).toBe(1);
  });

  test("every request extends the previous one exactly, across turns and settings changes (prompt cache)", async () => {
    const provider = new ScriptedProvider([useTools(read("c1")), say("One."), useTools(read("c2")), say("Two."), say("Three.")]);
    const session = makeSession(provider);
    await collect(session.send("one"));
    session.configure({ yolo: false, sandbox: false });
    await collect(session.send("two"));
    await collect(session.send("three"));
    expect(provider.requests).toHaveLength(5);
    for (let i = 1; i < provider.requests.length; i++) {
      const before = provider.requests[i - 1]!;
      const after = provider.requests[i]!;
      expect(after.history.slice(0, before.history.length)).toEqual(before.history); // only appended to
      expect(after.history.length).toBeGreaterThan(before.history.length);
      expect(after.options.system).toBe(before.options.system); // the same system prompt
      expect(after.options.tools).toBe(before.options.tools); // the very same tool definitions
    }
  });
});

/** Waits (at most ~1 s) for a condition that becomes true in the background. */
async function until(condition: () => boolean): Promise<boolean> {
  for (let i = 0; i < 100 && !condition(); i++) await Bun.sleep(10);
  return condition();
}

describe("review fixes", () => {
  test("a turn nobody reads still runs to the end and frees the session", async () => {
    const provider = new ScriptedProvider([say("One."), say("Two.")]);
    const session = makeSession(provider);
    session.send("one"); // never iterated
    expect(await until(() => !session.busy)).toBe(true);
    const events = await collect(session.send("two"));
    expect(events.at(-1)).toMatchObject({ type: "turn_end", reason: "end" });
    expect(provider.requests.map((r) => r.history.at(-1)?.text)).toEqual(["one", "two"]);
  });

  test("a turn whose reader is dropped part-way still frees the session", async () => {
    const session = makeSession(new ScriptedProvider([say("One."), say("Two.")]));
    const reader = session.send("one")[Symbol.asyncIterator]();
    await reader.next(); // turn_start, then the reader is simply forgotten (no return())
    expect(await until(() => !session.busy)).toBe(true);
    expect((await collect(session.send("two"))).at(-1)).toMatchObject({ type: "turn_end", reason: "end" });
  });

  test("the session is free at turn_end: a client can send the next message from inside its loop", async () => {
    const provider = new ScriptedProvider([say("One."), say("Two.")]);
    const session = makeSession(provider);
    let next: AsyncIterable<SessionEvent> | undefined;
    for await (const event of session.send("one")) {
      if (event.type !== "turn_end") continue;
      expect(session.busy).toBe(false);
      next = session.send("two");
    }
    expect((await collect(next!)).at(-1)).toMatchObject({ type: "turn_end", reason: "end" });
    expect(provider.requests.map((r) => r.history.at(-1)?.text)).toEqual(["one", "two"]);
  });

  test("clear() right after a turn still saves that turn's conversation", async () => {
    const sessions = new SessionStore(dir);
    const session = makeSession(new ScriptedProvider([say("Noted.")]), { sessions });
    await collect(session.send("remember 42"));
    const first = session.id;
    await session.clear();
    await session.flush();
    expect((await sessions.load(project, first))?.conversation[0]).toEqual({ role: "user", text: "remember 42" });
  });

  test("so does resume() into another session", async () => {
    const sessions = new SessionStore(dir);
    const other = makeSession(new ScriptedProvider([say("Old.")]), { sessions });
    await collect(other.send("an old conversation"));
    await other.flush();
    const session = makeSession(new ScriptedProvider([say("Noted.")]), { sessions });
    await collect(session.send("remember 42"));
    const first = session.id;
    await session.resume(other.id);
    await session.flush();
    expect((await sessions.load(project, first))?.conversation[0]).toEqual({ role: "user", text: "remember 42" });
    expect((await sessions.load(project, other.id))?.conversation[0]).toEqual({ role: "user", text: "an old conversation" });
  });

  test("a turn keeps the provider it started with: a subagent's model comes from the same factory", async () => {
    const helper: AgentType = { name: "helper", description: "Helps.", body: "", tools: ["read_file"], model: "small", source: "project" };
    const parent = new ScriptedProvider([
      useTools(call("a1", "agent", { type: "helper", description: "help", prompt: "find the notes" })),
      [{ type: "usage", usage: { promptTokens: 10, completionTokens: 1 } }, ...say("Parent done.")],
    ]);
    const sub = new ScriptedProvider([say("Found them.")]);
    const made: string[] = [];
    let session!: MarvSession;
    const switched: ProviderFactory = {
      id: "b",
      model: "main",
      make: (model) => (made.push(`b:${model ?? "main"}`), new ScriptedProvider([])),
    };
    const original: ProviderFactory = {
      id: "a",
      model: "main",
      local: true,
      make: (model) => {
        if (model) return made.push(`a:${model}`), sub;
        return {
          name: "a",
          stream: (history, options) => {
            // The user switches models while the turn runs, before the subagent starts.
            if (parent.requests.length === 0) session.configure({ provider: switched });
            return parent.stream(history, options);
          },
        };
      },
    };
    session = makeSession(original, { agents: [helper] });
    const events = await collect(session.send("go"));
    expect(events.at(-1)).toMatchObject({ type: "turn_end", reason: "end" });
    expect(made).toContain("a:small");
    expect(made).not.toContain("b:small");
    expect(sub.requests).toHaveLength(1);
    // Counted as the turn's (local) model, not the one switched to meanwhile.
    expect(session.usage().totals.local).toBe(true);
  });

  test("a turn prices its requests with the model it started with", async () => {
    let session!: MarvSession;
    const switched = fixed(new ScriptedProvider([]), { id: "other", model: "other-model" }); // no prices known
    const priced: ProviderFactory = {
      id: "openrouter",
      model: "x/y",
      lookup: async () => ({ id: "x/y", priceIn: 2, priceOut: 4 }),
      make: () => ({
        name: "x/y",
        async *stream() {
          // The user switches models while the turn runs, before its request is counted.
          session.configure({ provider: switched });
          yield { type: "usage", usage: { promptTokens: 1_000_000, completionTokens: 500_000 } };
          yield* say("Hi.");
        },
      }),
    };
    session = makeSession(priced);
    await Bun.sleep(0); // the prices arrive
    await collect(session.send("hi"));
    expect(session.usage().totals).toMatchObject({ requests: 1, cost: 4, estimated: true }); // $2 in + $2 out
  });

  test("a turn that started without prices isn't priced with the next model's", async () => {
    let session!: MarvSession;
    const next: ProviderFactory = {
      id: "openrouter",
      model: "b/priced",
      lookup: async () => ({ id: "b/priced", priceIn: 2, priceOut: 4 }),
      make: () => new ScriptedProvider([]),
    };
    const unpriced = fixed({
      name: "unpriced",
      async *stream() {
        session.configure({ provider: next }); // switched mid-turn...
        await Bun.sleep(0); // ...and the new model's prices arrive before this request is counted
        yield { type: "usage", usage: { promptTokens: 1_000_000, completionTokens: 0 } };
        yield* say("Hi.");
      },
    });
    session = makeSession(unpriced);
    await collect(session.send("hi"));
    expect(session.modelInfo?.priceIn).toBe(2);
    expect(session.usage().totals).toMatchObject({ requests: 1, cost: undefined });
  });

  test("a turn stopped before its message is sent leaves nothing in the trajectory", async () => {
    const store = new TrajectoryStore(dir);
    const session = makeSession(new ScriptedProvider([say("Glad to help.")]), { trajectories: store });
    await collect(session.send("hi", { signal: AbortSignal.abort() }));
    await collect(session.send("thanks, perfect"));
    await session.flush();
    const log = (await readFile(store.open(project, session.id).path, "utf8"))
      .trim()
      .split("\n")
      .map((line) => JSON.parse(line) as { type: string; text?: string });
    expect(log.filter((r) => r.type === "turn_start").map((r) => r.text)).toEqual(["thanks, perfect"]);
    expect(log.some((r) => r.type === "feedback")).toBe(false);
  });

  test("close() waits for the running turn, and nothing is saved after it returns", async () => {
    const sessions = new SessionStore(dir);
    let saves = 0;
    const save = sessions.save.bind(sessions);
    sessions.save = (saved) => (saves++, save(saved));
    const session = makeSession(new Hanging(), { sessions });
    const reader = session.send("go")[Symbol.asyncIterator]();
    while (!(await reader.next()).value?.type.startsWith("text")); // the model is working
    await session.close();
    expect(session.busy).toBe(false);
    const after = saves;
    await Bun.sleep(300); // past the save delay: a timer armed after close() would have fired by now
    expect(saves).toBe(after);
    expect((await sessions.load(project, session.id))?.conversation[0]).toEqual({ role: "user", text: "go" });
  });

  test("an onChange that throws doesn't break the session", async () => {
    const provider = new ScriptedProvider([say("Hi.")]);
    const factory = fixed(provider, { lookup: async () => ({ id: "test-model", context: 1000 }) });
    const session = makeSession(factory, {
      onChange: () => {
        throw new Error("the UI broke");
      },
    });
    await Bun.sleep(0); // the lookup's onChange throws here
    await session.clear().catch(() => {});
    const events = await collect(session.send("hi"));
    expect(events.at(-1)).toMatchObject({ type: "turn_end", reason: "end" });
  });

  test("a signal that's already aborted ends the turn before anything is sent", async () => {
    const provider = new ScriptedProvider([say("Next.")]);
    const session = makeSession(provider);
    const events = await collect(session.send("hi", { signal: AbortSignal.abort() }));
    expect(events.at(-1)).toMatchObject({ type: "turn_end", reason: "interrupted" });
    expect(provider.requests).toHaveLength(0);
    await collect(session.send("next"));
    expect(provider.requests[0]!.history).toEqual([{ role: "user", text: "next" }]);
  });

  test("configure() with the same model keeps what the model list said", async () => {
    const made: unknown[] = [];
    const factory: ProviderFactory = {
      id: "openrouter",
      model: "x/y",
      make: (_model, info) => (made.push(info), new ScriptedProvider([])),
      lookup: async () => ({ id: "x/y", context: 200_000, reasoning: "optional" }),
    };
    const session = makeSession(factory);
    await Bun.sleep(0);
    session.configure({ provider: { ...factory } });
    expect(session.modelInfo?.reasoning).toBe("optional");
    expect(made.at(-1)).toEqual({ reasoning: "optional" });
  });

  test("a lookup for a provider that was replaced meanwhile is ignored", async () => {
    let answer!: (info: ModelInfo) => void;
    let changed = 0;
    const stale = fixed(new ScriptedProvider([]), { lookup: () => new Promise<ModelInfo>((resolve) => (answer = resolve)) });
    const session = makeSession(stale, { onChange: () => changed++ });
    session.configure({ provider: fixed(new ScriptedProvider([]), { id: "other", model: "other-model" }) });
    answer({ id: "test-model", context: 123 });
    await Bun.sleep(0);
    expect(session.modelInfo).toBeUndefined();
    expect(session.contextLength).toBeUndefined();
    expect(changed).toBe(0);
  });

  test("after resume(), a compaction doesn't point at a turn in the other session's log", async () => {
    const sessions = new SessionStore(dir);
    const other = makeSession(new ScriptedProvider([say("Old.")]), { sessions });
    await collect(other.send("an old conversation"));
    await other.flush();
    const store = new TrajectoryStore(dir);
    const session = makeSession(new ScriptedProvider([say("New."), say("THE SUMMARY")]), { sessions, trajectories: store });
    await collect(session.send("a new one"));
    await session.resume(other.id);
    expect(await session.compact()).toMatchObject({ compacted: true });
    await session.flush();
    const text = await readFile(store.open(project, other.id).path, "utf8");
    const compact = text
      .trim()
      .split("\n")
      .map((line) => JSON.parse(line))
      .find((r) => r.type === "compact");
    expect(compact).toMatchObject({ summary: "THE SUMMARY" });
    expect(compact.turn).toBeUndefined();
  });

  test("a failed automatic compaction lets the turn go on", async () => {
    const used: AgentEvent = { type: "usage", usage: { promptTokens: 900, completionTokens: 0 } };
    const provider = new ScriptedProvider([[used, ...say("First.")], [{ type: "error", message: "summary failed" }], say("Second.")], 1000);
    const session = makeSession(provider);
    await collect(session.send("first"));
    const events = await collect(session.send("second"));
    expect(events).toContainEqual({ type: "compaction", result: { compacted: false, reason: "failed", error: "summary failed" } });
    expect(events).toContainEqual({ type: "assistant", text: "Second." });
    expect(events.at(-1)).toMatchObject({ type: "turn_end", reason: "end" });
    expect(provider.requests[2]!.history.map((t) => t.text)).toEqual(["first", "First.", "second"]);
  });
});

describe("final review fixes", () => {
  const custom = (name: string) => ({
    name,
    description: "mine",
    input: z.object({}),
    label: () => name,
    run: async () => ({ output: "ran mine", summary: "mine" }),
  });

  test("a tool of your own can't take a built-in tool's name (the built-in would run instead)", () => {
    expect(() => makeSession(new ScriptedProvider([]), { tools: [custom("bash")] })).toThrow('A tool named "bash" is already built in: give yours another name.');
    // Built in even when it isn't offered (skill, with no skills): the name is still taken.
    expect(() => makeSession(new ScriptedProvider([]), { tools: [custom("skill")] })).toThrow(/"skill" is already built in/);
  });

  test("two tools of your own can't share a name", () => {
    expect(() => makeSession(new ScriptedProvider([]), { tools: [custom("x"), custom("x")] })).toThrow('Two tools are named "x".');
  });

  test("mcp__ names are for MCP servers' tools", () => {
    expect(() => makeSession(new ScriptedProvider([]), { tools: [custom("mcp__a__b")] })).toThrow('Tool names starting with "mcp__" are for MCP servers\' tools.');
  });

  test("configure() with an unknown provider kind throws and changes nothing", async () => {
    const provider = new ScriptedProvider([useTools(call("w1", "write_file", { path: "out.txt", content: "hi\n" })), say("Still me.")]);
    const session = makeSession(provider, { yolo: true });
    const bad = { kind: "nope" } as unknown as ProviderOption;
    expect(() => session.configure({ provider: bad, yolo: false })).toThrow(/Unknown provider kind "nope"/);
    // The bad provider wasn't kept: changing something else afterwards works.
    session.configure({ thinking: true });
    const events = await collect(session.send("hi"));
    expect(events).toContainEqual({ type: "assistant", text: "Still me." });
    expect(provider.requests).toHaveLength(2);
    // Nor was the yolo: false that came with it: with no approver, the edit still ran on its own.
    expect(existsSync(join(project, "out.txt"))).toBe(true);
  });

  test("where yolo isn't allowed (noYolo), a session with yolo on can't be made", () => {
    expect(() => makeSession(new ScriptedProvider([]), { noYolo: "Not here." })).toThrow("Not here.");
    expect(() => makeSession(new ScriptedProvider([]), { noYolo: "Not here.", yolo: true })).toThrow("Not here.");
  });

  test("nor can configure() turn it on: it throws and changes nothing", async () => {
    const provider = new ScriptedProvider([useTools(call("w1", "write_file", { path: "out.txt", content: "hi\n" })), say("Refused.")]);
    const session = makeSession(provider, { noYolo: "Not here.", yolo: false });
    expect(() => session.configure({ yolo: true, provider: fixed(new ScriptedProvider([say("Other.")])) })).toThrow("Not here.");
    session.configure({ yolo: false }); // allowed
    const events = await collect(session.send("write it"));
    // Still yolo off, and still the first provider: with no approver the edit is refused.
    expect(events).toContainEqual({ type: "assistant", text: "Refused." });
    expect(existsSync(join(project, "out.txt"))).toBe(false);
  });

  test("after close(), nothing starts work or swaps the conversation; close() twice is fine", async () => {
    const session = makeSession(new ScriptedProvider([say("Hi.")]), { sessions: new SessionStore(dir) });
    await session.close();
    await session.close();
    expect(() => session.send("hi")).toThrow("This session is closed.");
    await expect(session.compact()).rejects.toThrow("This session is closed.");
    expect(() => session.clear()).toThrow("This session is closed.");
    await expect(session.resume("latest")).rejects.toThrow("This session is closed.");
    // Settings and saving still work.
    session.configure({ yolo: false });
    await session.flush();
  });
});

describe("diagnostics", () => {
  const ok: CommandResult = { output: "", exitCode: 0, timedOut: false, aborted: false };
  const broken: CommandResult = { output: "src/b.ts(1,1): error TS2304: Cannot find name 'x'.\n", exitCode: 2, timedOut: false, aborted: false };
  const write = (id: string) => call(id, "write_file", { path: "a.ts", content: "export const a = 1;\n" });
  /** The project as a TypeScript one, and a stand-in tsc that answers in turn and notes whether a.ts existed each time. */
  function typescriptProject(...answers: CommandResult[]) {
    mkdirSync(join(project, "node_modules", ".bin"), { recursive: true });
    writeFileSync(join(project, "node_modules", ".bin", "tsc"), "");
    const sawFile: boolean[] = [];
    const run: RunCheck = async () => (sawFile.push(existsSync(join(project, "a.ts"))), answers.shift() ?? ok);
    return { checks: new Diagnostics({ run, sandboxWorks: () => true }), sawFile };
  }
  beforeEach(async () => writeFile(join(project, "tsconfig.json"), "{}"));

  test("new errors after a step that changed a file reach the model, after the tool results", async () => {
    const { checks } = typescriptProject(ok, broken);
    const provider = new ScriptedProvider([useTools(write("c1")), say("Done.")]);
    const events = await collect(makeSession(provider, { typecheck: checks }).send("go"));
    const second = provider.requests[1]!.history;
    expect(second.at(-2)?.role).toBe("tool");
    expect(second.at(-1)).toEqual({ role: "user", text: expect.stringContaining("src/b.ts:1:1 TS2304 Cannot find name 'x'.") });
    expect(events).toContainEqual({ type: "status", status: "checking" });
    expect(events).toContainEqual({ type: "check", result: expect.objectContaining({ status: "done", errors: 1 }) });
    // In this order, each after the one before.
    const steps: ((e: SessionEvent) => boolean)[] = [
      (e) => e.type === "tool_end",
      (e) => e.type === "status" && e.status === "checking",
      (e) => e.type === "check",
      (e) => e.type === "status" && e.status === "running",
      (e) => e.type === "assistant" && e.text === "Done.",
    ];
    let from = 0;
    for (const match of steps) {
      const i = events.findIndex((e, n) => n >= from && match(e));
      expect(i).toBeGreaterThanOrEqual(from);
      from = i + 1;
    }
  });

  test("the baseline is taken before the file is written, also in yolo mode", async () => {
    const { checks, sawFile } = typescriptProject(ok, ok);
    await collect(makeSession(new ScriptedProvider([useTools(write("c1")), say("Done.")]), { typecheck: checks }).send("go"));
    expect(sawFile).toEqual([false, true]);
  });

  test("diagnostics: false runs nothing, and configure() turns it on from the next turn", async () => {
    const { checks, sawFile } = typescriptProject(ok, ok);
    const session = makeSession(new ScriptedProvider([useTools(write("c1")), say("Done."), useTools(write("c2")), say("Done.")]), { typecheck: checks, diagnostics: false });
    await collect(session.send("go"));
    expect(sawFile).toEqual([]);
    session.configure({ diagnostics: true });
    await collect(session.send("again"));
    expect(sawFile).toHaveLength(2);
  });

  test("the check is in the trajectory", async () => {
    const { checks } = typescriptProject(ok, broken);
    const trajectories = new TrajectoryStore(dir);
    const session = makeSession(new ScriptedProvider([useTools(write("c1")), say("Done.")]), { typecheck: checks, trajectories });
    await collect(session.send("go"));
    await session.flush();
    const files = await Array.fromAsync(new Bun.Glob("**/*.jsonl").scan(dir));
    const records = (await readFile(join(dir, files[0]!), "utf8")).trim().split("\n").map((line) => JSON.parse(line));
    expect(files).toHaveLength(1);
    expect(records).toContainEqual(expect.objectContaining({ type: "check", agent: "main", status: "done", before: 0, errors: 1, added: 1, note: expect.stringContaining("Cannot find name 'x'") }));
    expect(records).toContainEqual(expect.objectContaining({ type: "turn_start", diagnostics: true }));
  });

  test("the step's check isn't counted in the next request's time", async () => {
    mkdirSync(join(project, "node_modules", ".bin"), { recursive: true });
    writeFileSync(join(project, "node_modules", ".bin", "tsc"), "");
    const usage: AgentEvent = { type: "usage", usage: { promptTokens: 10, completionTokens: 1 } };
    let runs = 0;
    const run: RunCheck = async () => (runs++ && (await Bun.sleep(500)), ok);
    const checks = new Diagnostics({ run, sandboxWorks: () => true });
    const trajectories = new TrajectoryStore(dir);
    const session = makeSession(new ScriptedProvider([[usage, ...useTools(write("c1"))], [usage, ...say("Done.")]]), { typecheck: checks, trajectories });
    await collect(session.send("go"));
    await session.flush();
    const files = await Array.fromAsync(new Bun.Glob("**/*.jsonl").scan(dir));
    const records = (await readFile(join(dir, files[0]!), "utf8")).trim().split("\n").map((line) => JSON.parse(line));
    const requests = records.filter((r) => r.type === "request");
    expect(requests).toHaveLength(2);
    expect(requests[1].ms).toBeLessThan(250);
  });

  test("turning diagnostics back on resets what a session gave up on", async () => {
    let resets = 0;
    class Spy extends Diagnostics {
      override reset() {
        resets++;
        super.reset();
      }
    }
    const session = makeSession(new ScriptedProvider([]), { typecheck: new Spy() });
    session.configure({ diagnostics: true });
    expect(resets).toBe(0);
    session.configure({ diagnostics: false });
    expect(resets).toBe(0);
    session.configure({ diagnostics: true });
    expect(resets).toBe(1);
  });

  test("a baseline whose call was declined is stopped when the turn ends, and the next turn works", async () => {
    mkdirSync(join(project, "node_modules", ".bin"), { recursive: true });
    writeFileSync(join(project, "node_modules", ".bin", "tsc"), "");
    let captured: AbortSignal | undefined;
    const run: RunCheck = async (_command, { signal }) => {
      captured = signal;
      await new Promise<void>((resolve) => (signal.aborted ? resolve() : signal.addEventListener("abort", () => resolve(), { once: true })));
      return { output: "", exitCode: null, timedOut: false, aborted: true };
    };
    const checks = new Diagnostics({ run, sandboxWorks: () => true });
    const session = makeSession(new ScriptedProvider([useTools(write("c1")), say("Fine.")]), { typecheck: checks, yolo: false, approve: async () => "no" });
    await collect(session.send("go"));
    expect(captured).toBeDefined();
    expect(captured!.aborted).toBe(true);
    const events = await collect(session.send("again"));
    expect(events.at(-1)).toMatchObject({ type: "turn_end" });
    expect(events.some((e) => e.type === "assistant")).toBe(true);
  });
});

describe("deferred MCP tools (#14)", () => {
  const used = (promptTokens: number): AgentEvent => ({ type: "usage", usage: { promptTokens, completionTokens: 0 } });
  const tools = [fakeTool("mcp__x__a", { kind: "read" }), fakeTool("mcp__x__b", { kind: "read" })];
  const specsOf = (ts: Tool[]): ToolSpec[] => ts.map((t) => ({ name: t.name, description: t.description, parameters: { type: "object", properties: {} } }));
  const builtinNames = toolSpecsFor({ hasSkills: false }).map((s) => s.name);
  const NOTE = "Marv: MCP tools you can use (not loaded: call tool_search with their names to see their parameters, then run one with mcp_call):";
  const lastUser = (p: ScriptedProvider, i: number) => p.requests[i]!.history.findLast((t) => t.role === "user")!.text;
  const ready = (ts: Tool[] = tools) => fakeMcp(Promise.resolve(), specsOf(ts), [...ts]);

  test("the tool list and system prompt never change; only appended messages carry the list", async () => {
    const mcp = ready(); // its own arrays: this test adds a tool to them, as /mcp trust would
    const provider = new ScriptedProvider([
      useTools(call("c1", "tool_search", { names: ["mcp__x__a"] })),
      useTools(call("c2", "mcp_call", { tool: "mcp__x__a", arguments: {} })),
      say("Done."),
      say("Again."),
    ]);
    const session = makeSession(provider, { mcp, mcpTools: "deferred" });
    await collect(session.send("use a"));
    const late = fakeTool("mcp__y__c", { kind: "read" });
    mcp.specs.push(...specsOf([late]));
    mcp.tools.push(late);
    await collect(session.send("and now?"));

    const [first, ...rest] = provider.requests;
    expect(first!.options.tools!.map((t) => t.name)).toEqual([...builtinNames, "tool_search", "mcp_call"]);
    for (const [i, r] of rest.entries()) {
      expect(r.options.tools).toEqual(first!.options.tools);
      expect(r.options.system).toBe(first!.options.system);
      expect(r.history.slice(0, provider.requests[i]!.history.length)).toEqual(provider.requests[i]!.history); // only appended
    }
    expect(lastUser(provider, 0)).toBe(`${NOTE}\n- mcp__x__a: Does mcp__x__a.\n- mcp__x__b: Does mcp__x__b.\n\nuse a`);
    expect(provider.requests[2]!.history.findLast((t) => t.role === "tool")!.text).toBe("mcp__x__a got {}");
    expect(lastUser(provider, 3)).toBe(`${NOTE}\n- mcp__y__c: Does mcp__y__c.\n\nand now?`);
  });

  test("/mcp add mid-conversation: an eager conversation offers the new specs from the next turn and stays eager", async () => {
    const mcp = ready();
    const provider = new ScriptedProvider([say("One."), say("Two.")]);
    const session = makeSession(provider, { mcp, mcpTools: "eager" });
    await collect(session.send("one"));
    const added = fakeTool("mcp__y__c", { kind: "read" });
    mcp.specs.push(...specsOf([added])); // what McpManager.add's rebuild does
    mcp.tools.push(added);
    await collect(session.send("two"));
    const [first, second] = provider.requests;
    expect(second!.options.tools!.map((t) => t.name)).toEqual([...first!.options.tools!.map((t) => t.name), "mcp__y__c"]);
    expect(second!.options.tools!.map((t) => t.name)).not.toContain("tool_search");
    expect(second!.options.system).toBe(first!.options.system);
    expect(second!.history.slice(0, first!.history.length)).toEqual(first!.history);
  });

  test("/mcp add into a conversation with no MCP tools yet: the next turn decides, with the new server counted", async () => {
    const mcp = fakeMcp(Promise.resolve(), [], []);
    const provider = new ScriptedProvider([say("One."), say("Two.")]);
    const session = makeSession(provider, { mcp, mcpTools: "deferred" });
    await collect(session.send("one"));
    mcp.specs.push(...specsOf(tools));
    mcp.tools.push(...tools);
    await collect(session.send("two"));
    expect(provider.requests[0]!.options.tools!.map((t) => t.name)).toEqual(builtinNames);
    expect(provider.requests[1]!.options.tools!.map((t) => t.name)).toEqual([...builtinNames, "tool_search", "mcp_call"]);
    expect(lastUser(provider, 1)).toBe(`${NOTE}\n- mcp__x__a: Does mcp__x__a.\n- mcp__x__b: Does mcp__x__b.\n\ntwo`);
    // The mcp__ paragraph was there from the start (a manager, even an empty one), so the system prompt didn't change.
    expect(provider.requests[0]!.options.system).toContain("Tools named mcp__<server>__<tool>");
    expect(provider.requests[1]!.options.system).toBe(provider.requests[0]!.options.system);
  });

  test("/clear forgets servers removed with /mcp remove", async () => {
    let forgot = 0;
    const mcp = Object.assign(ready(), { forgetRemoved: () => void forgot++ });
    const session = makeSession(new ScriptedProvider([say("Hi.")]), { mcp });
    await session.clear();
    expect(forgot).toBe(1);
  });

  test("a turn with nothing new adds nothing, and the transcript never shows the note", async () => {
    const provider = new ScriptedProvider([say("One."), say("Two.")]);
    const store = new SessionStore(join(dir, "sessions"));
    const session = makeSession(provider, { mcp: ready(), mcpTools: "deferred", sessions: store });
    await collect(session.send("one"));
    await collect(session.send("two"));
    expect(lastUser(provider, 1)).toBe("two");
    await session.flush();
    const saved = (await store.latest(project))!;
    expect(saved.transcript.filter((m) => m.role === "user").map((m) => m.text)).toEqual(["one", "two"]);
  });

  test("the system prompt is the same whatever the mode", async () => {
    const systemOf = async (mcpTools: "deferred" | "eager") => {
      const provider = new ScriptedProvider([say("Hi.")]);
      await collect(makeSession(provider, { mcp: ready(), mcpTools }).send("hi"));
      return provider.requests[0]!.options.system;
    };
    expect(await systemOf("deferred")).toBe(await systemOf("eager"));
  });

  test("a server that settles late is still in the first note", async () => {
    let start!: () => void;
    const specs: ToolSpec[] = [];
    const late: Tool[] = [];
    const mcp = fakeMcp(new Promise<void>((resolve) => (start = resolve)), specs, late);
    setTimeout(() => {
      late.push(...tools);
      specs.push(...specsOf(tools));
      start();
    }, 50);
    const provider = new ScriptedProvider([say("Hi.")]);
    await collect(makeSession(provider, { mcp, mcpTools: "deferred" }).send("hi"));
    expect(lastUser(provider, 0)).toContain("- mcp__x__b");
  });

  test("a direct call to a listed tool runs as mcp_call, and the history says mcp_call", async () => {
    const provider = new ScriptedProvider([useTools(call("c1", "mcp__x__a", { q: 1 })), say("Done.")]);
    await collect(makeSession(provider, { mcp: ready(), mcpTools: "deferred" }).send("go"));
    const history = provider.requests[1]!.history;
    expect(history.find((t) => t.role === "assistant")).toMatchObject({ toolCalls: [{ name: "mcp_call" }] });
    const result = history.find((t) => t.role === "tool")!.text;
    expect(result).toContain('mcp__x__a got {"q":1}');
    expect(result).toContain("Marv: mcp__x__a isn't offered as a tool of its own");
  });

  test("auto mode sends a few small tools as today", async () => {
    const provider = new ScriptedProvider([say("Hi.")]);
    await collect(makeSession(provider, { mcp: ready() }).send("hi"));
    expect(provider.requests[0]!.options.tools!.map((t) => t.name)).toContain("mcp__x__a");
    expect(lastUser(provider, 0)).toBe("hi");
  });

  test("compaction before a turn re-announces everything with that message", async () => {
    const provider = new ScriptedProvider([[used(900), ...say("Big.")], say("Summary."), say("After.")], 1000);
    const session = makeSession(provider, { mcp: ready(), mcpTools: "deferred" });
    await collect(session.send("one"));
    await collect(session.send("two"));
    expect(lastUser(provider, 2)).toBe(`${NOTE}\n- mcp__x__a: Does mcp__x__a.\n- mcp__x__b: Does mcp__x__b.\n\ntwo`);
  });

  test("a mid-turn compaction re-announces after MID_TURN_NOTE", async () => {
    const provider = new ScriptedProvider([[used(900), ...useTools(read("c1"))], say("SUMMARY"), say("Finished.")], 1000);
    await collect(makeSession(provider, { mcp: ready(), mcpTools: "deferred" }).send("first"));
    expect(lastUser(provider, 2)).toBe(`${MID_TURN_NOTE}\n\n${NOTE}\n- mcp__x__a: Does mcp__x__a.\n- mcp__x__b: Does mcp__x__b.`);
  });

  test("clear() starts over: the next message announces everything again", async () => {
    const provider = new ScriptedProvider([say("One."), say("Two.")]);
    const session = makeSession(provider, { mcp: ready(), mcpTools: "deferred" });
    await collect(session.send("one"));
    await session.clear();
    await collect(session.send("two"));
    expect(lastUser(provider, 1)).toStartWith(NOTE);
  });

  test("resume() keeps the mode and what was announced, and says what's gone", async () => {
    const store = new SessionStore(join(dir, "sessions"));
    const first = makeSession(new ScriptedProvider([say("Hi.")]), { mcp: ready(), mcpTools: "deferred", sessions: store });
    await collect(first.send("hi"));
    await first.flush();
    const provider = new ScriptedProvider([say("Back.")]);
    const second = makeSession(provider, { mcp: ready([tools[0]!]), mcpTools: "eager", sessions: store });
    await second.resume("latest");
    await collect(second.send("back"));
    expect(provider.requests[0]!.options.tools!.map((t) => t.name)).toContain("mcp_call"); // the saved mode wins
    expect(lastUser(provider, 0)).toBe("Marv: these MCP tools are no longer available: mcp__x__b.\n\nback");
  });

  test("the trajectory records the mode and the note", async () => {
    const trajectories = new TrajectoryStore(join(dir, "trajectories"));
    const session = makeSession(new ScriptedProvider([say("Hi.")]), { mcp: ready(), mcpTools: "deferred", trajectories });
    await collect(session.send("hi"));
    await session.flush();
    const records = (await readFile(trajectories.open(project, session.id).path, "utf8")).trim().split("\n").map((l) => JSON.parse(l));
    expect(records.find((r) => r.type === "turn_start").mcpTools).toMatchObject({ mode: "deferred", count: 2 });
    expect(records.find((r) => r.type === "mcp_note").text).toStartWith(NOTE);
  });

  test("a caller's tool can't be named tool_search or mcp_call", () => {
    expect(() => makeSession(new ScriptedProvider([]), { tools: [fakeTool("mcp_call")] })).toThrow(/mcp_call/);
  });
});

describe("a long AGENTS.md (#40)", () => {
  test("the model is told what was cut, reads it with read_file, and the system prompt stays the same every request", async () => {
    const body = (title: string) => `## ${title}\n\n${"- a rule that matters here\n".repeat(400)}`;
    await writeFile(join(project, "AGENTS.md"), `# Project\n\n${body("Commands")}\n${body("Architecture")}\n${body("Conventions")}`);
    const { text, cut } = (await readInstructions(project))!;
    const conventions = cut!.sections.find((s) => s.heading === "## Conventions")!;
    const provider = new ScriptedProvider([
      useTools(call("c1", "read_file", { path: "AGENTS.md", offset: conventions.line, limit: 1 })),
      say("Read it."),
      say("Second turn."),
    ]);
    const session = makeSession(provider, { instructions: text });
    await collect(session.send("what are the conventions?"));
    await collect(session.send("and again"));

    expect(provider.requests).toHaveLength(3);
    const system = provider.requests[0]!.options.system!;
    expect(system).toContain(`- ## Conventions (line ${conventions.line})`);
    for (const r of provider.requests) expect(r.options.system).toBe(system);
    const result = provider.requests[1]!.history.find((t) => t.role === "tool");
    expect(result).toMatchObject({ role: "tool", text: expect.stringContaining(`${conventions.line}→## Conventions`) });
  });
});

describe("the plan (#15)", () => {
  const items = [
    { text: "Read the report command", status: "done" },
    { text: "Add the --json flag", status: "in_progress" },
    { text: "Update README and CHANGELOG", status: "pending" },
  ] as const;
  const planCall = (id: string, list: readonly unknown[] = items) => call(id, "plan", { items: list });

  test("a plan call sets session.plan; the system prompt and specs stay byte-identical", async () => {
    const provider = new ScriptedProvider([useTools(planCall("p1")), say("Working."), say("More.")]);
    const session = makeSession(provider);
    expect(session.plan).toBeNull();
    const events = await collect(session.send("do three things"));
    expect(session.plan).toEqual([...items]);
    const end = events.find((e) => e.type === "tool_end");
    expect(end).toMatchObject({ result: { output: "Plan saved: 1 of 3 done; now: Add the --json flag.", plan: [...items] } });
    // The model got only the line; the list is in its own call.
    expect(provider.requests[1]!.history.at(-1)).toEqual({ role: "tool", callId: "p1", name: "plan", text: "Plan saved: 1 of 3 done; now: Add the --json flag." });
    await collect(session.send("and then?"));
    const [a, b, c] = provider.requests;
    expect(a!.options.tools!.map((t) => t.name)).toContain("plan");
    for (const r of [b!, c!]) {
      expect(r.options.system).toBe(a!.options.system);
      expect(r.options.tools).toBe(a!.options.tools);
    }
    expect(a!.options.system).toContain("keep a plan with the plan tool");
  });

  test("an empty list clears it; clear() drops it", async () => {
    const session = makeSession(new ScriptedProvider([useTools(planCall("p1")), say("Ok."), useTools(planCall("p2", [])), say("Cleared."), useTools(planCall("p3")), say("Again.")]));
    await collect(session.send("plan"));
    expect(session.plan).not.toBeNull();
    await collect(session.send("drop it"));
    expect(session.plan).toBeNull();
    await collect(session.send("plan again"));
    expect(session.plan).not.toBeNull();
    await session.clear();
    expect(session.plan).toBeNull();
  });

  test("saved with the session, and restored by resume()", async () => {
    const sessions = new SessionStore(dir);
    const a = makeSession(new ScriptedProvider([useTools(planCall("p1")), say("Planned.")]), { sessions });
    await collect(a.send("plan it"));
    await a.flush();
    expect((await sessions.load(project, a.id))?.plan).toEqual([...items]);
    const b = makeSession(new ScriptedProvider([]), { sessions });
    const resumed = await b.resume(a.id);
    expect(resumed?.plan).toEqual([...items]);
    expect(b.plan).toEqual([...items]);
  });

  test("a malformed saved plan is dropped; the session still loads", async () => {
    const sessions = new SessionStore(dir);
    const a = makeSession(new ScriptedProvider([say("Hi.")]), { sessions });
    await collect(a.send("hi"));
    await a.flush();
    const file = sessions.file(project, a.id);
    const saved = JSON.parse(await readFile(file, "utf8"));
    await writeFile(file, JSON.stringify({ ...saved, plan: [{ text: "x", status: "started" }] }));
    const b = makeSession(new ScriptedProvider([]), { sessions });
    const resumed = await b.resume(a.id);
    expect(resumed?.id).toBe(a.id);
    expect(resumed?.plan).toBeNull();
    expect(b.plan).toBeNull();
  });

  test("plan: false leaves it out of every request and the system prompt; a call to it is unknown", async () => {
    const provider = new ScriptedProvider([useTools(planCall("p1")), say("Ok.")]);
    const session = makeSession(provider, { plan: false });
    const events = await collect(session.send("plan"));
    for (const r of provider.requests) {
      expect(r.options.tools!.map((t) => t.name)).not.toContain("plan");
      expect(r.options.system).not.toContain("plan tool");
    }
    const end = events.find((e) => e.type === "tool_end");
    expect(end?.type === "tool_end" && end.result.isError).toBe(true);
    expect(end?.type === "tool_end" ? end.result.output : "").toStartWith('Unknown tool "plan"');
    expect(session.plan).toBeNull();
  });

  test("a subagent is offered no plan, and its call to one fails", async () => {
    const sub = new ScriptedProvider([useTools(planCall("s1")), say("Found them.")]);
    const provider = new RoutedProvider({
      "parent-task": new ScriptedProvider([useTools(call("a1", "agent", { description: "look around", prompt: "sub-task: find the notes" })), say("Parent done.")]),
      "sub-task": sub,
    });
    const session = makeSession(provider);
    await collect(session.send("parent-task: go"));
    expect(sub.requests[0]!.options.tools!.map((t) => t.name)).not.toContain("plan");
    expect(sub.requests[1]!.history.at(-1)!.text).toStartWith('Unknown tool "plan"');
    expect(session.plan).toBeNull();
  });

  describe("compaction keeps it", () => {
    const used = (promptTokens: number): AgentEvent => ({ type: "usage", usage: { promptTokens, completionTokens: 0 } });

    test("mid-turn: the request after the summary has the plan, then MID_TURN_NOTE", async () => {
      const provider = new ScriptedProvider([useTools(planCall("p1")), [used(900), ...useTools(read("c1"))], say("SUMMARY"), say("Finished.")], 1000);
      const session = makeSession(provider);
      await collect(session.send("long task"));
      const after = provider.requests[3]!.history;
      expect(after[0]!.text).toContain("SUMMARY\n\nYour plan (from the plan tool), as it stood:\n- [done] Read the report command\n- [in progress] Add the --json flag\n- [pending] Update README and CHANGELOG\nKeep updating it with the plan tool.");
      expect(after.at(-1)!.text).toBe(MID_TURN_NOTE);
    });

    test("before a user message: the plan is in the first message, and the message follows", async () => {
      const provider = new ScriptedProvider([useTools(planCall("p1")), [used(900), ...say("Paused.")], say("SUMMARY"), say("Going on.")], 1000);
      const session = makeSession(provider);
      await collect(session.send("long task"));
      await collect(session.send("go on"));
      const after = provider.requests[3]!.history;
      expect(after[0]!.text).toContain("- [in progress] Add the --json flag");
      expect(after.at(-1)).toEqual({ role: "user", text: "go on" });
    });
  });
});
