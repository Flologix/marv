// The evals' tracker MCP server (evals/mcp/tracker.ts, #14): big enough that the MCP tasks really measure deferral,
// and add_comment records what the task's check reads.
import { afterEach, beforeEach, expect, test } from "bun:test";
import { existsSync, readFileSync } from "node:fs";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { chooseMode, DEFER_TOKENS } from "../src/mcp/deferred.ts";
import { McpManager } from "../src/mcp/manager.ts";
import { runTool } from "../src/tools/index.ts";

const TRACKER = join(import.meta.dir, "..", "evals", "mcp", "tracker.ts");
const DATA = join(import.meta.dir, "..", "evals", "mcp", "data", "tracker.json");

let dir: string;
let manager: McpManager;
beforeEach(async () => {
  dir = await mkdtemp(join(tmpdir(), "marv-eval-mcp-"));
});
afterEach(async () => {
  await manager?.close();
  await rm(dir, { recursive: true, force: true });
});

async function start(out: string): Promise<McpManager> {
  manager = new McpManager([{ name: "tracker", source: "personal", key: "k", transport: { type: "stdio", command: process.execPath, args: [TRACKER, DATA, out] } }], { root: dir, version: "0" });
  await manager.start();
  return manager;
}

test("25 tools whose specs are well over the threshold: deferred with Ollama's window, and with an unknown one", async () => {
  const m = await start(join(dir, "out.json"));
  expect(m.specs).toHaveLength(25);
  expect(chooseMode("auto", m.specs, 32_768).mode).toBe("deferred");
  const unknown = chooseMode("auto", m.specs, undefined);
  expect(unknown.specTokens).toBeGreaterThan(DEFER_TOKENS);
  expect(unknown.mode).toBe("deferred");
});

test("get_issue reads the data; add_comment runs unasked and appends to the out file", async () => {
  const out = join(dir, "out.json");
  const m = await start(out);
  const run = (name: string, args: unknown) => runTool({ id: name, name, arguments: JSON.stringify(args) }, { root: dir }, m.tools);
  expect((await run("mcp__tracker__get_issue", { number: 12 })).output).toContain("negative step");
  expect(JSON.parse((await run("mcp__tracker__get_issue", { number: 12, fields: ["number", "state"] })).output)).toEqual({ number: 12, state: "open" });
  expect(existsSync(out)).toBe(false);
  expect((await run("mcp__tracker__add_comment", { number: 7, body: "Added --json." })).isError).toBeFalsy();
  expect((await run("mcp__tracker__add_comment", { number: 99, body: "x" })).isError).toBe(true);
  expect(JSON.parse(readFileSync(out, "utf8"))).toEqual([{ issue: 7, body: "Added --json." }]);
});
