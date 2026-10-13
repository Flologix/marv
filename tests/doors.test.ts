// The shared rules of the doors out of the sandbox (#47, #51, #52): a door's approval names it and is asked every
// time when it's `once` (the session never answers it from "don't ask again", nor remembers it, whatever the client
// says), its result and trajectory record name it, and the doors are the main agent's alone.
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { z } from "zod";
import type { AgentEvent, Provider, ToolCall } from "../src/provider/types.ts";
import type { ProviderFactory } from "../src/provider/factory.ts";
import { MarvSession, type SessionEvent, type SessionInit } from "../src/session.ts";
import { subagentContext } from "../src/subagent.ts";
import { runTool } from "../src/tools/index.ts";
import type { ApprovalRequest, Decision, Door, Tool } from "../src/tools/types.ts";
import { TrajectoryStore } from "../src/trajectory.ts";
import { ScriptedProvider } from "./fake-provider.ts";

let project: string;
let dir: string;
beforeEach(async () => {
  project = await mkdtemp(join(tmpdir(), "marv-doors-project-"));
  dir = await mkdtemp(join(tmpdir(), "marv-doors-"));
});
afterEach(async () => {
  await rm(project, { recursive: true, force: true });
  await rm(dir, { recursive: true, force: true });
});

const say = (text: string): AgentEvent[] => [{ type: "text_delta", text }, { type: "done", reason: "stop" }];
const useTools = (...calls: ToolCall[]): AgentEvent[] => [...calls.map((c) => ({ type: "tool_call" as const, call: c })), { type: "done", reason: "tool_calls" }];
const call = (id: string, name: string, args: Record<string, unknown> = {}): ToolCall => ({ id, name, arguments: JSON.stringify(args) });
const fixed = (provider: Provider): ProviderFactory => ({ id: "test", model: "test-model", make: () => provider });
const collect = async (events: AsyncIterable<SessionEvent>) => Array.fromAsync(events);

/** A door of any name, as #47 and #51 will add theirs: it asks, and says which door and whether it's `once`. */
function doorTool(name: string, door: Door, once: boolean, ran: string[]): Tool {
  const input = z.object({ what: z.string().optional() });
  return {
    name,
    description: "A door for tests.",
    input,
    kind: "execute",
    label: () => name,
    door: () => door,
    once: () => once,
    scope: () => ({ key: `${name}:same`, description: "the same thing" }),
    preview: async () => ({ title: name }),
    run: async () => (ran.push(name), { output: "ran", summary: "ran" }),
  } as Tool;
}

const makeSession = (provider: Provider, init: Partial<SessionInit>) => new MarvSession({ root: project, provider: fixed(provider), yolo: false, ...init });

describe("a once request (a door that asks every time)", () => {
  test('is asked again for the same scope even when the client answered "always"; its key is never remembered', async () => {
    const ran: string[] = [];
    const asked: ApprovalRequest[] = [];
    const provider = new ScriptedProvider([
      useTools(call("c1", "host_like")),
      useTools(call("c2", "host_like")),
      say("One."),
      useTools(call("c3", "host_like")),
      say("Two."),
    ]);
    const session = makeSession(provider, {
      tools: [doorTool("host_like", "process", true, ran)],
      approve: async (r) => (asked.push(r), "always"),
    });
    const events = [...(await collect(session.send("one"))), ...(await collect(session.send("two")))];
    expect(asked).toHaveLength(3);
    expect(asked[0]).toMatchObject({ once: true, door: "process" });
    expect(ran).toEqual(["host_like", "host_like", "host_like"]);
    // An "always" for a once request counts as one yes, and that's what the result records.
    const approvals = events.flatMap((e) => (e.type === "tool_end" ? [e.result.approval] : []));
    expect(approvals).toEqual(["yes", "yes", "yes"]);
  });

  test("a scope remembered from an ordinary call doesn't answer a once request with the same key", async () => {
    const ran: string[] = [];
    const asked: string[] = [];
    const provider = new ScriptedProvider([useTools(call("c1", "plain")), useTools(call("c2", "door")), say("Done.")]);
    const plain = { ...doorTool("plain", "process", false, ran), door: undefined, scope: () => ({ key: "shared", description: "x" }) } as Tool;
    const door = { ...doorTool("door", "host", true, ran), scope: () => ({ key: "shared", description: "x" }) } as Tool;
    const session = makeSession(provider, { tools: [plain, door], approve: async (r) => (asked.push(r.tool), "always") });
    await collect(session.send("go"));
    expect(asked).toEqual(["plain", "door"]);
  });

  test("an ordinary request's \"always\" still covers its scope (control)", async () => {
    const ran: string[] = [];
    const asked: string[] = [];
    const provider = new ScriptedProvider([useTools(call("c1", "plain")), useTools(call("c2", "plain")), say("Done.")]);
    const session = makeSession(provider, { tools: [doorTool("plain", "github", false, ran)], approve: async (r) => (asked.push(r.tool), "always") });
    await collect(session.send("go"));
    expect(asked).toEqual(["plain"]);
    expect(ran).toEqual(["plain", "plain"]);
  });
});

describe("runTool and a door", () => {
  test("copies the tool's door onto the request and the result, and once onto the request", async () => {
    const ran: string[] = [];
    const asked: ApprovalRequest[] = [];
    const result = await runTool(call("c1", "x"), { root: project, approve: async (r) => (asked.push(r), "yes") }, [doorTool("x", "github", true, ran)]);
    expect(asked[0]).toMatchObject({ door: "github", once: true });
    expect(result).toMatchObject({ door: "github", approval: "yes" });
    const no = await runTool(call("c2", "x"), { root: project, approve: async () => "no" as Decision }, [doorTool("x", "github", true, ran)]);
    expect(no).toMatchObject({ door: "github", declined: true });
  });

  test("an ordinary call has no door and no once", async () => {
    const asked: ApprovalRequest[] = [];
    await runTool(call("c1", "write_file", { path: "a.txt", content: "a" }), { root: project, approve: async (r) => (asked.push(r), "yes") });
    expect(asked[0]!.door).toBeUndefined();
    expect(asked[0]!.once).toBeUndefined();
  });

  test("the trajectory's tool record names the door", async () => {
    const store = new TrajectoryStore(dir);
    const session = makeSession(new ScriptedProvider([useTools(call("c1", "d"), call("c2", "read_file", { path: "nope" })), say("Done.")]), {
      tools: [doorTool("d", "host", true, [])],
      approve: async () => "yes",
      trajectories: store,
    });
    await collect(session.send("go"));
    await session.flush();
    const log = (await readFile(store.open(project, session.id).path, "utf8")).trim().split("\n").map((l) => JSON.parse(l));
    const tools = log.filter((r) => r.type === "tool");
    expect(tools[0]).toMatchObject({ door: "host", approval: "yes" });
    expect("door" in tools[1]).toBe(false);
  });
});

describe("doors are the main agent's", () => {
  test("subagentContext drops doors, whatever the parent has", () => {
    const sub = subagentContext({ root: project, doors: { host: true, process: true } }, "general-purpose · x");
    expect(sub.doors).toBeUndefined();
  });

  test("the session gives its main agent the host door only with hostCommands", async () => {
    const seen: unknown[] = [];
    const probe = (name: string): Tool =>
      ({ name, description: "probe", input: z.object({}), label: () => name, run: async (_i, ctx) => (seen.push(ctx.doors), { output: "", summary: "" }) }) as Tool;
    for (const hostCommands of [undefined, true]) {
      const session = makeSession(new ScriptedProvider([useTools(call("p", "probe")), say("Done.")]), { tools: [probe("probe")], hostCommands });
      await collect(session.send("go"));
    }
    expect(seen).toEqual([{ host: false }, { host: true }]);
  });
});
