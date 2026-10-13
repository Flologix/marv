// Guard suite (#52): bash with sandbox: false runs one command on the user's machine, so it's asked every time and
// only where someone approves each command at a terminal. It asks in yolo; "always" never covers a second call; it's
// refused, before any ask, in marv -p (even with --approve naming its exact scope key), in an SDK session without
// hostCommands, and for a subagent. Each effect is a marker file outside the project and outside the sandbox's own
// /tmp, so it shows up only if the command really ran on the host; the controls show it does when allowed. Each was
// checked to fail with its guard removed.
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { existsSync } from "node:fs";
import { mkdir, mkdtemp, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { buildApprover, type HeadlessResult } from "../../src/headless.ts";
import type { AgentEvent, ToolCall } from "../../src/provider/types.ts";
import { sandboxAvailable } from "../../src/sandbox.ts";
import { createSession, type ApprovalRequest, type SessionOptions } from "../../src/sdk.ts";
import { NO_HOST_DOOR } from "../../src/tools/bash.ts";
import { runTool } from "../../src/tools/index.ts";
import { ScriptedProvider } from "../fake-provider.ts";

let root: string;
let configDir: string;
let outside: string;
let server: ReturnType<typeof Bun.serve> | undefined;
beforeEach(async () => {
  root = await mkdtemp(join(tmpdir(), "marv-guard-host-"));
  configDir = await mkdtemp(join(tmpdir(), "marv-guard-host-config-"));
  outside = await mkdtemp(join(tmpdir(), "marv-guard-host-outside-"));
  await mkdir(join(root, ".git"));
});
afterEach(async () => {
  server?.stop(true);
  server = undefined;
  for (const d of [root, configDir, outside]) await rm(d, { recursive: true, force: true });
});

/** The command a scripted model runs on the host: it leaves a marker per run, outside the project. */
const hostCommand = () => `touch ${join(outside, "ran-$(date +%s%N)")}`;
const runs = async () => (await readdir(outside)).length;

const toolCall = (id: string, name: string, args: Record<string, unknown>): ToolCall => ({ id, name, arguments: JSON.stringify(args) });
const useTools = (...calls: ToolCall[]): AgentEvent[] => [...calls.map((call) => ({ type: "tool_call" as const, call })), { type: "done", reason: "tool_calls" }];
const say = (text: string): AgentEvent[] => [{ type: "text_delta", text }, { type: "done", reason: "stop" }];

/** One SDK turn with a scripted model; returns every approval request. */
async function sdkTurn(script: AgentEvent[][], options: Partial<SessionOptions>) {
  const session = await createSession({ cwd: root, provider: new ScriptedProvider(script), configDir, ...options });
  try {
    const events = await Array.fromAsync(session.send("do it"));
    return events.flatMap((e) => (e.type === "tool_end" ? [e.result] : []));
  } finally {
    await session.close();
  }
}

describe.if(sandboxAvailable())("guard: bash with sandbox: false", () => {
  test("asks in yolo, with the host door and once (yolo never runs it unasked)", async () => {
    const asked: ApprovalRequest[] = [];
    const call = toolCall("h1", "bash", { command: hostCommand(), sandbox: false });
    const result = await runTool(call, { root, yolo: true, doors: { host: true }, approve: async (r) => (asked.push(r), "no") });
    expect(asked).toHaveLength(1);
    expect(asked[0]).toMatchObject({ door: "host", once: true });
    expect(result.declined).toBe(true);
    expect(await runs()).toBe(0);
    // Control: a yes really runs it on the host.
    await runTool(call, { root, yolo: true, doors: { host: true }, approve: async () => "yes" });
    expect(await runs()).toBe(1);
  });

  test('never uses "always": two calls, two asks, even when the client answers "always"', async () => {
    const asked: ApprovalRequest[] = [];
    const command = hostCommand();
    const results = await sdkTurn([useTools(toolCall("h1", "bash", { command, sandbox: false })), useTools(toolCall("h2", "bash", { command, sandbox: false })), say("Done.")], {
      hostCommands: true,
      approve: async (r) => (asked.push(r), "always"),
    });
    expect(asked).toHaveLength(2);
    expect(asked.map((r) => r.scope.key)).toEqual([`bash:host:${command}`, `bash:host:${command}`]);
    expect(results.map((r) => r.approval)).toEqual(["yes", "yes"]);
    expect(await runs()).toBe(2);
  });

  test("an SDK session without hostCommands refuses it before its approver is asked", async () => {
    const asked: ApprovalRequest[] = [];
    const results = await sdkTurn([useTools(toolCall("h1", "bash", { command: hostCommand(), sandbox: false })), say("Done.")], {
      approve: async (r) => (asked.push(r), "yes"),
    });
    expect(results[0]).toMatchObject({ isError: true, output: NO_HOST_DOOR });
    expect(asked).toEqual([]);
    expect(await runs()).toBe(0);
  });

  test("marv -p's approver, even one naming the exact scope key, never gets the call: refused like in any session without hostCommands", async () => {
    const command = hostCommand();
    // buildApprover says yes to an exact key: that's why the door is the session's to open, not the approver's.
    const [approver] = buildApprover([`bash:host:${command}`]);
    expect(await approver!({ tool: "bash", label: command, preview: { title: "" }, scope: { key: `bash:host:${command}`, description: "" } })).toBe("yes");
    const asked: ApprovalRequest[] = [];
    // The options runHeadless gives createSession (no hostCommands), with that approver.
    const results = await sdkTurn([useTools(toolCall("h1", "bash", { command, sandbox: false })), say("Done.")], {
      sources: ["user", "project"],
      persist: true,
      approve: async (r) => (asked.push(r), approver!(r)),
    });
    expect(results[0]).toMatchObject({ isError: true, output: NO_HOST_DOOR });
    expect(asked).toEqual([]);
    expect(await runs()).toBe(0);
  });

  test("a subagent can't: refused, even though the session has the door and its approver says yes", async () => {
    const asked: ApprovalRequest[] = [];
    const results = await sdkTurn(
      [
        useTools(toolCall("a1", "agent", { description: "set up", prompt: "run it on the host" })),
        useTools(toolCall("s1", "bash", { command: hostCommand(), sandbox: false })),
        say("Tried."),
        say("Done."),
      ],
      { hostCommands: true, approve: async (r) => (asked.push(r), "yes") },
    );
    expect(results[0]?.output).toContain("Tried.");
    expect(asked.filter((r) => r.door === "host")).toEqual([]);
    expect(await runs()).toBe(0);
  });
});

/** A stand-in OpenRouter for the real CLI: the host call, then a plain answer. */
function serveHostCall(command: string): string {
  const sse = (...events: unknown[]) =>
    new Response([...events].map((e) => `data: ${JSON.stringify(e)}\n\n`).concat("data: [DONE]\n\n").join(""), { headers: { "content-type": "text/event-stream" } });
  const finish = (reason: string) => ({ choices: [{ delta: {}, finish_reason: reason }] });
  const replies = [
    [{ choices: [{ delta: { tool_calls: [{ index: 0, id: "h1", type: "function", function: { name: "bash", arguments: JSON.stringify({ command, sandbox: false }) } }] } }] }, finish("tool_calls")],
    [{ choices: [{ delta: { content: "Done." } }] }, finish("stop")],
  ];
  let n = 0;
  server = Bun.serve({
    port: 0,
    fetch: (req) => (new URL(req.url).pathname.endsWith("/chat/completions") ? sse(...replies[Math.min(n++, replies.length - 1)]!) : Response.json({ data: [] })),
  });
  return `http://localhost:${server.port}/v1`;
}

describe.if(sandboxAvailable())("guard: marv -p and sandbox: false", () => {
  test("refused (exit 3) with every --approve name there is, and nothing runs", async () => {
    const command = hostCommand();
    await writeFile(join(configDir, "config.json"), JSON.stringify({ provider: "openrouter", model: "test-model", apiKey: "sk-test", baseUrl: serveHostCall(command) }));
    const names = ["git_write", "network", "web", "mcp", "files", "memory", "agent_worktree"].flatMap((n) => ["--approve", n]);
    const proc = Bun.spawn([process.execPath, join(import.meta.dir, "..", "..", "src", "cli.tsx"), "-p", "do it", "--output", "json", ...names], {
      env: { ...process.env, MARV_CONFIG_DIR: configDir, NO_COLOR: "1" },
      cwd: root,
      stdout: "pipe",
      stderr: "pipe",
    });
    const [stdout, stderr] = await Promise.all([new Response(proc.stdout).text(), new Response(proc.stderr).text()]);
    const exitCode = await proc.exited;
    let result: HeadlessResult;
    try {
      result = JSON.parse(stdout) as HeadlessResult;
    } catch {
      throw new Error(`marv -p printed no JSON (exit ${exitCode}):\n${stdout}\n${stderr}`);
    }
    expect(exitCode).toBe(3);
    expect(result.refused?.map((r) => r.tool)).toEqual(["bash"]);
    expect(await runs()).toBe(0);
    expect(existsSync(join(root, "ran"))).toBe(false);
  }, 20_000);
});
