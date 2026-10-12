import { afterEach, beforeEach, describe, expect, spyOn, test } from "bun:test";
import { realpathSync } from "node:fs";
import { mkdtemp, rm } from "node:fs/promises";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";
import { WebStandardStreamableHTTPServerTransport } from "@modelcontextprotocol/sdk/server/webStandardStreamableHttp.js";
import type { McpServerConfig } from "../src/mcp/config.ts";
import { deferredTools } from "../src/mcp/deferred.ts";
import { McpManager } from "../src/mcp/manager.ts";
import { formatMcpResult, makeMcpTool, mcpToolName } from "../src/mcp/tool.ts";
import { McpTrust } from "../src/mcp/trust.ts";
import type { ToolCall } from "../src/provider/types.ts";
import { runTool } from "../src/tools/index.ts";
import type { ApprovalRequest } from "../src/tools/types.ts";
import { makeServer } from "./fixtures/mcp-server.ts";

const FIXTURE = join(import.meta.dir, "fixtures", "mcp-server.ts");
const stdio = (name = "test", over: Partial<McpServerConfig> = {}): McpServerConfig => ({
  name,
  source: "personal",
  key: `k-${name}`,
  transport: { type: "stdio", command: process.execPath, args: [FIXTURE], env: { CUSTOM: "yes" } },
  ...over,
});

let root: string;
let managers: McpManager[] = [];
beforeEach(async () => {
  root = await mkdtemp(join(tmpdir(), "marv-mcp-"));
});
afterEach(async () => {
  await Promise.all(managers.map((m) => m.close()));
  managers = [];
  await rm(root, { recursive: true, force: true });
});

function manager(servers: McpServerConfig[], opts: Partial<ConstructorParameters<typeof McpManager>[1]> = {}) {
  const m = new McpManager(servers, { root, version: "9.9.9", ...opts });
  managers.push(m);
  return m;
}

let nextId = 0;
async function call(m: McpManager, name: string, args: unknown, extra: { yolo?: boolean; signal?: AbortSignal } = {}) {
  const asked: ApprovalRequest[] = [];
  const toolCall: ToolCall = { id: `m${nextId++}`, name, arguments: JSON.stringify(args) };
  const result = await runTool(toolCall, { root, approve: async (r) => (asked.push(r), "yes"), ...extra }, m.tools);
  return { result, asked };
}

describe("a local (stdio) server", () => {
  test("its tools become Marv tools named mcp__<server>__<tool>, with the server's schema", async () => {
    const m = manager([stdio()]);
    await m.start();
    expect(m.status()).toEqual([
      expect.objectContaining({ name: "test", state: "connected", tools: ["mcp__test__echo", "mcp__test__lookup", "mcp__test__fail", "mcp__test__picture", "mcp__test__slow", "mcp__test__env", "mcp__test__progress", "mcp__test__cwd"] }),
    ]);
    const echo = m.specs.find((s) => s.name === "mcp__test__echo")!;
    expect(echo.description).toBe('(MCP server "test") Echo the text back.');
    expect(echo.parameters).toMatchObject({ type: "object", properties: { text: { type: "string" } }, required: ["text"] });
  });

  test("a call asks first (it runs outside the sandbox), even in yolo mode; then returns the server's text", async () => {
    const m = manager([stdio()]);
    await m.start();
    const { result, asked } = await call(m, "mcp__test__echo", { text: "hi" }, { yolo: true });
    expect(result).toMatchObject({ output: "echo: hi", summary: "1 line", label: "hi", approval: "yes" });
    expect(asked[0]).toMatchObject({
      tool: "mcp__test__echo",
      preview: { title: "test: echo", text: '{\n  "text": "hi"\n}', note: 'MCP server "test" (yours) · runs outside the sandbox' },
      scope: { key: "mcp:test:echo", description: "test's echo" },
    });
  });

  test("deferred: tool_search and mcp_call reach the server's tools (#14)", async () => {
    const m = manager([stdio()]);
    await m.start();
    const offered = deferredTools({ tools: m.tools, specs: m.specs, loaded: new Set() });
    const asked: ApprovalRequest[] = [];
    const run = (name: string, args: unknown) => runTool({ id: `m${nextId++}`, name, arguments: JSON.stringify(args) }, { root, approve: async (r) => (asked.push(r), "yes") }, offered);
    expect((await run("tool_search", { query: "echo" })).output).toContain('"name":"mcp__test__echo"');
    expect((await run("mcp_call", { tool: "mcp__test__echo", arguments: { text: "hi" } })).output).toBe("echo: hi");
    expect(asked.map((r) => r.scope.key)).toEqual(["mcp:test:echo"]);
    expect((await run("mcp_call", { tool: "mcp__test__lookup", arguments: { word: "w" } })).output).toBe("w: a word");
    expect(asked).toHaveLength(1); // lookup is read-only
  });

  test("a tool the server marks read-only runs without asking", async () => {
    const m = manager([stdio()]);
    await m.start();
    const { result, asked } = await call(m, "mcp__test__lookup", { word: "mars" });
    expect(result.output).toBe("mars: a word");
    expect(asked).toHaveLength(0);
  });

  test("errors, images, and what the server's environment holds", async () => {
    const m = manager([stdio()]);
    await m.start();
    expect((await call(m, "mcp__test__fail", {})).result).toMatchObject({ output: "it broke", isError: true });
    expect((await call(m, "mcp__test__picture", {})).result.output).toBe("here it is\n[image (image/png), 0 KB, not shown: Marv passes text only]");
    process.env.OPENROUTER_API_KEY = "sk-or-secret";
    try {
      expect((await call(m, "mcp__test__env", {})).result.output).toBe("key= custom=yes"); // API keys don't reach it; its own env does
    } finally {
      delete process.env.OPENROUTER_API_KEY;
    }
  });

  test("a long call that reports progress isn't cut off by the timeout", async () => {
    const m = manager([stdio("test", { timeout: 1 })]); // 1 s, but the tool takes 1.5 s, reporting every 0.3 s
    await m.start();
    expect((await call(m, "mcp__test__progress", {})).result.output).toBe("made it");
  });

  test("Esc stops a call that's taking long", async () => {
    const m = manager([stdio()]);
    await m.start();
    const controller = new AbortController();
    setTimeout(() => controller.abort(), 200);
    const started = Date.now();
    const { result } = await call(m, "mcp__test__slow", {}, { signal: controller.signal });
    expect(Date.now() - started).toBeLessThan(2000);
    expect(result.isError).toBe(true);
  });

  test("your own servers start in your home folder, never the project (npx would run a package planted in its node_modules)", async () => {
    const mine = manager([stdio()]);
    const theirs = manager([stdio("proj", { source: "project" })]);
    await Promise.all([mine.start(), theirs.start()]);
    expect((await call(mine, "mcp__test__cwd", {})).result.output).toBe(realpathSync(homedir()));
    expect((await call(theirs, "mcp__proj__cwd", {})).result.output).toBe(realpathSync(root));
  });

  test("its stderr is kept off the screen, for /mcp", async () => {
    const m = manager([stdio()]);
    await m.start();
    await Bun.sleep(100);
    expect(m.status()[0]!.stderr).toContain("test-server starting");
  });

  test("a server that can't start, or never answers, fails without holding the others up", async () => {
    const m = manager(
      [
        stdio("missing", { transport: { type: "stdio", command: "/nonexistent/marv-mcp" } }),
        stdio("mute", { transport: { type: "stdio", command: "sleep", args: ["30"] } }),
        stdio(),
      ],
      { connectTimeoutMs: 1000 },
    );
    await m.start();
    const [missing, mute, ok] = m.status();
    expect(missing).toMatchObject({ state: "failed", error: expect.any(String) });
    expect(mute).toMatchObject({ state: "failed", error: "connecting timed out after 1s" });
    expect(ok!.state).toBe("connected");
    expect(m.tools.every((t) => t.name.startsWith("mcp__test__"))).toBe(true);
  });
});

describe("a remote (http) server", () => {
  test("connects over Streamable HTTP, sending the configured headers", async () => {
    const seen: (string | null)[] = [];
    const server = Bun.serve({
      port: 0,
      async fetch(request) {
        seen.push(request.headers.get("authorization"));
        // Stateless: a fresh server and transport per request.
        const transport = new WebStandardStreamableHTTPServerTransport({ sessionIdGenerator: undefined, enableJsonResponse: true });
        await makeServer().connect(transport);
        return transport.handleRequest(request);
      },
    });
    try {
      const m = manager([{ name: "remote", source: "personal", key: "k", transport: { type: "http", url: `http://localhost:${server.port}/mcp`, headers: { Authorization: "Bearer t0k" } } }]);
      await m.start();
      expect(m.status()[0]).toMatchObject({ state: "connected" });
      const { result } = await call(m, "mcp__remote__echo", { text: "over http" });
      expect(result.output).toBe("echo: over http");
      expect(seen.every((h) => h === "Bearer t0k")).toBe(true);
    } finally {
      server.stop(true);
    }
  });
});

test("a server whose tool list never ends (the same next-page cursor forever) doesn't hang startup", async () => {
  const m = manager([stdio("paging", { transport: { type: "stdio", command: process.execPath, args: [join(import.meta.dir, "fixtures", "mcp-paging-server.ts")] } })]);
  const started = Date.now();
  await m.start();
  expect(Date.now() - started).toBeLessThan(5000);
  expect(m.status()[0]).toMatchObject({ state: "connected", tools: ["mcp__paging__only"] });
});

test("if two servers' tools end up with the same name, the second is reported, not silently dropped", async () => {
  const m = manager([stdio("test"), stdio("test", { key: "other" })]);
  await m.start();
  const [first, second] = m.status();
  expect(first!.tools).toContain("mcp__test__echo");
  expect(second!.skipped).toContain("mcp__test__echo");
  expect(m.tools.filter((t) => t.name === "mcp__test__echo")).toHaveLength(1);
});

describe("trust", () => {
  test("/mcp trust while servers are still starting doesn't start yours a second time", async () => {
    const connects = spyOn(McpManager.prototype as unknown as { connect: () => Promise<void> }, "connect");
    try {
      const trust = new McpTrust(join(root, "trust.json"));
      const m = manager([stdio(), stdio("proj", { source: "project" })], { trust });
      void m.start();
      expect(m.untrusted()).toEqual([]); // still checking: nothing is "untrusted" yet
      const started = await m.trustAll();
      expect(started.map((s) => s.name)).toEqual(["proj"]);
      expect(connects).toHaveBeenCalledTimes(2); // each server once
    } finally {
      connects.mockRestore();
    }
  });

  test("a project's server waits until it's trusted, then starts", async () => {
    const trust = new McpTrust(join(root, "trust.json"));
    const m = manager([stdio("proj", { source: "project" })], { trust });
    await m.start();
    expect(m.status()[0]!.state).toBe("untrusted");
    expect(m.tools).toEqual([]);
    expect(m.untrusted().map((s) => s.name)).toEqual(["proj"]);

    const started = await m.trustAll();
    expect(started).toEqual([expect.objectContaining({ name: "proj", state: "connected" })]);
    expect(m.tools.length).toBeGreaterThan(0);
    expect(await trust.isTrusted(root, stdio("proj", { source: "project" }))).toBe(true);
  });
});

test("a broken trust file can't crash startup: the project's servers just wait to be trusted", async () => {
  const path = join(root, "trust.json");
  await Bun.write(path, "null");
  const m = manager([stdio("proj", { source: "project" }), stdio()], { trust: new McpTrust(path) });
  await m.start();
  expect(m.status().map((s) => [s.name, s.state])).toEqual([
    ["proj", "untrusted"],
    ["test", "connected"],
  ]);
});

describe("naming and output", () => {
  test("tool names stay within what providers accept", () => {
    expect(mcpToolName("gh", "create_issue")).toBe("mcp__gh__create_issue");
    const odd = mcpToolName("gh", "issues.create/v2");
    expect(odd).toMatch(/^mcp__gh__issues_create_v2_[0-9a-f]{8}$/);
    const long = mcpToolName("server-with-a-long-name", "a_really_long_tool_name_that_goes_on_and_on_forever");
    expect(long.length).toBeLessThanOrEqual(64);
    expect(long).toMatch(/^[A-Za-z0-9_-]+$/);
  });

  test("a schema providers would reject is made acceptable (one bad tool would fail every request)", () => {
    const config = stdio();
    const spec = (inputSchema: Record<string, unknown>) => makeMcpTool(config, { name: "t", inputSchema }, async () => ({})).spec.parameters;
    expect(spec({ $schema: "http://json-schema.org/draft-07/schema#", type: "object", properties: { a: { type: "string" } } })).toEqual({
      type: "object",
      properties: { a: { type: "string" } },
    });
    expect(spec({ anyOf: [{ type: "object" }, { type: "object" }] })).toEqual({ type: "object", properties: {} });
    expect(spec({ type: "string" })).toEqual({ type: "object", properties: {} });
    expect(spec({})).toEqual({ type: "object", properties: {} });
  });

  test("structured results and huge outputs", () => {
    expect(formatMcpResult({ content: [], structuredContent: { a: 1 } })).toBe('{\n  "a": 1\n}');
    const huge = formatMcpResult({ content: [{ type: "text", text: "x".repeat(100_000) }] });
    expect(huge.length).toBeLessThan(31_000);
    expect(huge).toContain("characters omitted");
  });
});

test("close() right after start() leaves no server connected or running", async () => {
  const marker = `close-race-${crypto.randomUUID()}`;
  const m = manager([stdio("racy", { transport: { type: "stdio", command: process.execPath, args: [FIXTURE, marker] } })]);
  const ready = m.start();
  await m.close();
  await ready;
  expect(m.status()[0]).toMatchObject({ state: "failed", error: "closed" });
  for (let i = 0; i < 50 && Bun.spawnSync(["pgrep", "-f", marker]).stdout.toString().trim(); i++) await Bun.sleep(100);
  expect(Bun.spawnSync(["pgrep", "-f", marker]).stdout.toString().trim()).toBe("");
});

describe("adding and removing servers while running (/mcp add, /mcp remove)", () => {
  test("a manager created empty starts an added server; a turn starting meanwhile waits for it", async () => {
    const m = manager([]);
    await m.start();
    expect(m.settled).toBe(true);
    const adding = m.add(stdio("added"));
    expect(m.settled).toBe(false);
    let waited = false;
    const ready = m.ready.then(() => (waited = true));
    const status = await adding;
    await ready;
    expect(waited).toBe(true);
    expect(m.settled).toBe(true);
    expect(status).toMatchObject({ name: "added", state: "connected" });
    expect(m.tools.map((t) => t.name)).toContain("mcp__added__echo");
    expect(m.specs.map((s) => s.name)).toContain("mcp__added__echo");
    const { result } = await call(m, "mcp__added__echo", { text: "hi" });
    expect(result.output).toBe("echo: hi");
  });

  test("it starts in the home folder, like your other servers", async () => {
    const m = manager([]);
    await m.add(stdio("home"));
    const { result } = await call(m, "mcp__home__cwd", {});
    expect(realpathSync(result.output)).toBe(realpathSync(homedir()));
  });

  test("a name that's taken is refused; a server that fails says so in its status", async () => {
    const m = manager([stdio("a")]);
    await m.start();
    await expect(m.add(stdio("a"))).rejects.toThrow(/already an MCP server named "a"/);
    const failed = await m.add(stdio("broken", { transport: { type: "stdio", command: "/nonexistent/server" } }));
    expect(failed.state).toBe("failed");
    expect(m.settled).toBe(true);
  });

  test("remove stops it; its tools stay listed and calls fail clearly; a new conversation forgets them", async () => {
    const m = manager([]);
    await m.add(stdio("gone"));
    const specs = m.specs.length;
    await m.remove("gone");
    expect(m.status()).toEqual([expect.objectContaining({ name: "gone", state: "removed" })]);
    expect(m.specs.length).toBe(specs);
    const { result } = await call(m, "mcp__gone__echo", { text: "hi" });
    expect(result.isError).toBe(true);
    expect(result.output).toContain("removed with /mcp remove");
    await expect(m.remove("gone")).rejects.toThrow(/no MCP server named "gone"/);
    m.forgetRemoved();
    expect(m.specs).toEqual([]);
    expect(m.status()).toEqual([]);
  });

  test("a server removed while it's still connecting stays removed, and isn't left running", async () => {
    const m = manager([]);
    const adding = m.add(stdio("quick"));
    // remove() while connect() is in flight: the race OCR found through trustAll's window.
    await Promise.resolve();
    const removing = m.remove("quick").catch(() => {});
    await Promise.all([adding, removing]);
    const state = m.status().find((s) => s.name === "quick")?.state;
    expect(state === "removed" || state === undefined).toBe(true);
    if (state === "removed") {
      const { result } = await call(m, "mcp__quick__echo", { text: "hi" });
      expect(result.isError).toBe(true);
    }
  });

  test("connect() never marks a server removed meanwhile as connected", async () => {
    const m = manager([]);
    // Drive the private path directly: a connection marked removed before its connect finishes.
    const c = { config: stdio("late"), state: "connecting", tools: [], stderr: [] } as never as { state: string };
    (m as never as { connections: unknown[] }).connections.push(c);
    const connecting = (m as never as { connect(c: unknown): Promise<void> }).connect(c);
    c.state = "removed";
    await connecting;
    expect(c.state).toBe("removed");
  });

  test("a server removed in this conversation can't be added again until the next one (its tools stay listed)", async () => {
    const m = manager([]);
    await m.add(stdio("again"));
    const specs = [...m.specs];
    await m.remove("again");
    await expect(m.add(stdio("again"))).rejects.toThrow(/removed in this conversation/);
    expect(m.specs).toEqual(specs);
    m.forgetRemoved();
    expect((await m.add(stdio("again"))).state).toBe("connected");
  });

  test("close() after add stops it", async () => {
    const m = manager([]);
    await m.add(stdio("x"));
    await m.close();
    await expect(m.add(stdio("y"))).rejects.toThrow(/closing/);
  });
});
