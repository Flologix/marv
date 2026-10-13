import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { buildApprover } from "../src/headless.ts";
import { ProcessManager } from "../src/processes.ts";
import type { AgentEvent, Provider, ToolCall } from "../src/provider/types.ts";
import type { ProviderFactory } from "../src/provider/factory.ts";
import { sandboxAvailable } from "../src/sandbox.ts";
import { createSession } from "../src/sdk.ts";
import { MarvSession, type SessionEvent, type SessionInit } from "../src/session.ts";
import type { ApprovalRequest, Decision, ToolResult } from "../src/tools/types.ts";
import { ScriptedProvider } from "./fake-provider.ts";

const say = (text: string): AgentEvent[] => [{ type: "text_delta", text }, { type: "done", reason: "stop" }];
const useTools = (...calls: ToolCall[]): AgentEvent[] => [...calls.map((c) => ({ type: "tool_call" as const, call: c })), { type: "done", reason: "tool_calls" }];
let ids = 0;
const call = (name: string, args: Record<string, unknown>): ToolCall => ({ id: `c${++ids}`, name, arguments: JSON.stringify(args) });
const fixed = (provider: Provider): ProviderFactory => ({ id: "test", model: "test-model", make: () => provider });
const results = (events: SessionEvent[]): ToolResult[] => events.flatMap((e) => (e.type === "tool_end" ? [e.result] : []));
const collect = async (events: AsyncIterable<SessionEvent>) => Array.fromAsync(events);

async function freePort(): Promise<number> {
  const server = createServer();
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const port = (server.address() as { port: number }).port;
  await new Promise((resolve) => server.close(resolve));
  return port;
}

const serve = (port: number, text = "served") =>
  `/run/marv-bun -e 'Bun.serve({ port: ${port}, hostname: "127.0.0.1", fetch: () => new Response(${JSON.stringify(text)}) })'`;

test("--approve process approves starting a process, not one on the user's network without network too", async () => {
  const request = (key: string) => ({ tool: "process", label: "x", preview: { title: "x" }, scope: { key, description: "" } }) as ApprovalRequest;
  const [only] = buildApprover(["process"]);
  expect(await only!(request("process:bun run dev"))).toBe("yes");
  expect(await only!(request("process:net:bun run dev"))).toBe("unavailable");
  const [both] = buildApprover(["process", "network"]);
  expect(await both!(request("process:net:bun run dev"))).toBe("yes");
  const [none] = buildApprover(["network"]);
  expect(await none!(request("process:bun run dev"))).toBe("unavailable");
});

describe.if(sandboxAvailable())("the process tool in a session (real sandbox)", () => {
  let root: string;
  let home: string;
  let runtime: string;
  let manager: ProcessManager;
  beforeEach(() => {
    root = mkdtempSync(join(tmpdir(), "marv-ptool-root-"));
    home = mkdtempSync(join(tmpdir(), "marv-ptool-home-"));
    runtime = mkdtempSync(join(tmpdir(), "mpt-"));
    manager = new ProcessManager({ root, home, runtimeDir: runtime, pollMs: 100, startWaitMs: 3000 });
  });
  afterEach(async () => {
    await manager.closeAll();
    for (const dir of [root, home, runtime]) rmSync(dir, { recursive: true, force: true });
  });

  const makeSession = (provider: Provider, init: Partial<SessionInit> = {}) =>
    new MarvSession({ root, provider: fixed(provider), processManager: manager, yolo: true, ...init });

  test("start asks in yolo (door process, scope per command, 'always' covers that command only); logs, list, stop never ask", async () => {
    const [a, b] = [await freePort(), await freePort()];
    const asked: ApprovalRequest[] = [];
    const provider = new ScriptedProvider([
      useTools(call("process", { action: "start", command: serve(a), name: "a" })),
      useTools(call("process", { action: "stop", name: "a" })),
      useTools(call("process", { action: "start", command: serve(a), name: "a" })),
      useTools(call("process", { action: "start", command: serve(b), name: "b" })),
      useTools(call("process", { action: "logs", name: "a" }), call("process", { action: "list" })),
      useTools(call("process", { action: "start", command: "sleep 100", network: true, name: "net" })),
      say("Done."),
    ]);
    const session = makeSession(provider, { approve: async (r) => (asked.push(r), "always" as Decision) });
    const [started, stopped, again, other, logs, list, net] = results(await collect(session.send("go")));
    expect(asked.map((r) => [r.door, r.scope.key, r.once, r.network])).toEqual([
      ["process", `process:${serve(a)}`, undefined, undefined],
      ["process", `process:${serve(b)}`, undefined, undefined],
      ["process", "process:net:sleep 100", undefined, true],
    ]);
    expect(started).toMatchObject({ door: "process", approval: "always" });
    expect(started!.output).toContain(`Forwarded to the user's machine: http://localhost:${a}`);
    expect(stopped!.output).toContain("Stopped a");
    expect(stopped!.approval).toBeUndefined();
    expect(again).toMatchObject({ approval: "yes", door: "process" }); // from "always"
    expect(other!.output).toContain(`http://localhost:${b}`);
    expect(logs!.approval).toBeUndefined();
    expect(logs!.output).toStartWith("Logs of a (running");
    expect(list!.output).toContain("a: running");
    expect(list!.output).toContain("b: running");
    expect(net!.output).toContain("net: running");
    expect(await (await fetch(`http://127.0.0.1:${b}/`)).text()).toBe("served");
    await session.close();
  });

  test("a sandboxed bash command reaches a started server on localhost, without network or an approval in yolo", async () => {
    const port = await freePort();
    const asked: ApprovalRequest[] = [];
    const provider = new ScriptedProvider([
      useTools(call("process", { action: "start", command: serve(port, "PAGE-FROM-PROCESS"), name: "web" })),
      useTools(call("bash", { command: `curl -s --max-time 5 localhost:${port}` })),
      say("Done."),
    ]);
    const session = makeSession(provider, { approve: async (r) => (asked.push(r), "yes" as Decision) });
    const [, curl] = results(await collect(session.send("go")));
    expect(asked.map((r) => r.tool)).toEqual(["process"]);
    expect(curl).toMatchObject({ approval: "auto" });
    expect(curl!.output).toContain("PAGE-FROM-PROCESS");
    await session.close();
  });

  test("processes survive clear() and resume; close() stops them", async () => {
    const port = await freePort();
    const provider = new ScriptedProvider([useTools(call("process", { action: "start", command: serve(port), name: "web" })), say("Up.")]);
    const session = makeSession(provider, { approve: async () => "yes" });
    await collect(session.send("go"));
    session.clear();
    expect(session.processes.map((p) => [p.name, p.state, p.ports])).toEqual([["web", "running", [port]]]);
    expect(await (await fetch(`http://127.0.0.1:${port}/`)).text()).toBe("served");
    await session.close();
    expect(session.processes).toEqual([]);
    await expect(fetch(`http://127.0.0.1:${port}/`)).rejects.toThrow();
  });

  test("a subagent has no process tool: naming it fails as unknown", async () => {
    const asked: ApprovalRequest[] = [];
    const provider = new ScriptedProvider([
      useTools(call("agent", { type: "general-purpose", description: "try", prompt: "start a server" })),
      useTools(call("process", { action: "start", command: "sleep 100" })),
      say("Couldn't."),
      say("Done."),
    ]);
    const session = makeSession(provider, { approve: async (r) => (asked.push(r), "yes" as Decision) });
    const events = await collect(session.send("go"));
    const sub = events.flatMap((e) => (e.type === "subagent" && e.event.type === "tool_end" ? [e.event.result] : []));
    expect(sub[0]!.output).toStartWith('Unknown tool "process"');
    expect(asked).toEqual([]);
    expect(manager.list()).toEqual([]);
    // Its tools never included it either.
    expect(provider.requests[1]!.options.tools!.map((t) => t.name)).not.toContain("process");
    await session.close();
  });

  test("processes: false, and no prompt paragraph or tool without the door", async () => {
    const provider = new ScriptedProvider([say("Hi.")]);
    const off = makeSession(provider, { processes: false });
    await collect(off.send("hi"));
    expect(provider.requests[0]!.options.tools!.map((t) => t.name)).not.toContain("process");
    expect(provider.requests[0]!.options.system).not.toContain("use process (start, logs, stop, list)");
    expect(provider.requests[0]!.options.system).toContain("# Sandbox limits");
    const on = new ScriptedProvider([say("Hi.")]);
    await collect(makeSession(on).send("hi"));
    expect(on.requests[0]!.options.tools!.map((t) => t.name)).toContain("process");
    expect(on.requests[0]!.options.system).toContain("use process (start, logs, stop, list)");
  });

  test("an SDK session (as marv -p runs) that started a server leaves nothing running after close()", async () => {
    const port = await freePort();
    const marker = `marv-sdk-proc-${Date.now()}`;
    const provider = new ScriptedProvider([
      useTools(call("process", { action: "start", command: `${serve(port)}; echo ${marker}`, name: "web" })),
      say("Started."),
    ]);
    const session = await createSession({ cwd: root, provider, approve: async () => "yes" });
    const events = await collect(session.send("start it"));
    expect(results(events)[0]!.output).toContain(`http://localhost:${port}`);
    expect(Bun.spawnSync(["pgrep", "-f", marker]).stdout.toString().trim()).not.toBe("");
    await session.close();
    expect(Bun.spawnSync(["pgrep", "-f", marker]).stdout.toString().trim()).toBe("");
    await expect(fetch(`http://127.0.0.1:${port}/`)).rejects.toThrow();
  });
});
