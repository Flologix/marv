import { describe, expect, test } from "bun:test";
import { announcement, chooseMode, DEFERRED_SPECS, deferredTools, MAX_LINE, oneLine, searchTools } from "../src/mcp/deferred.ts";
import type { ToolSpec } from "../src/provider/types.ts";
import { runTool } from "../src/tools/index.ts";
import type { ApprovalRequest } from "../src/tools/types.ts";
import { fakeTool } from "./mcp-fakes.ts";

const spec = (name: string, description: string, extra = 0): ToolSpec => ({
  name,
  description: `(MCP server "x") ${description}`,
  parameters: { type: "object", properties: { q: { type: "string", description: "p".repeat(extra) } } },
});

describe("oneLine", () => {
  test("drops the server prefix (the name says it) and keeps the first sentence", () => {
    expect(oneLine('(MCP server "github") Create an issue. Needs a token.')).toBe("Create an issue.");
  });
  test("one line, at most MAX_LINE characters, escape sequences stripped", () => {
    const line = oneLine(`(MCP server "x") ${"word ".repeat(60)}\n\u001b]52;c;aGk=\u0007more`);
    expect(line).not.toContain("\n");
    expect(line).not.toContain("\u001b");
    expect(line.length).toBeLessThanOrEqual(MAX_LINE);
    expect(line.endsWith("…")).toBe(true);
  });
  test("a version number's dot doesn't end the sentence", () => {
    expect(oneLine('(MCP server "x") Talks to API v2.1 servers. More.')).toBe("Talks to API v2.1 servers.");
  });
});

describe("announcement", () => {
  test("one line per new tool, in the order given, after the instruction", () => {
    expect(announcement([spec("mcp__x__a", "Does a."), spec("mcp__x__b", "")], [])).toBe(
      "Marv: MCP tools you can use (not loaded: call tool_search with their names to see their parameters, then run one with mcp_call):\n- mcp__x__a: Does a.\n- mcp__x__b",
    );
  });
  test("tools that are gone get one line; nothing new and nothing gone is null", () => {
    expect(announcement([], ["mcp__x__old", "mcp__y__older"])).toBe("Marv: these MCP tools are no longer available: mcp__x__old, mcp__y__older.");
    expect(announcement([], [])).toBeNull();
  });
});

describe("chooseMode", () => {
  const small = [spec("mcp__x__a", "Does a."), spec("mcp__x__b", "Does b.")];
  const big = Array.from({ length: 30 }, (_, i) => spec(`mcp__x__t${i}`, `Tool ${i}.`, 600));

  test("auto: a few small tools are sent as today", () => {
    expect(chooseMode("auto", small, 32_768).mode).toBe("eager");
  });
  test("auto: specs over min(4000, 10% of the window) are deferred", () => {
    const choice = chooseMode("auto", big, 200_000);
    expect(choice.specTokens).toBeGreaterThan(4000);
    expect(choice).toMatchObject({ mode: "deferred", count: 30 });
    expect(choice.deferredTokens * 2).toBeLessThanOrEqual(choice.specTokens);
  });
  test("auto: a small window lowers the limit; an unknown one uses 4000", () => {
    const medium = Array.from({ length: 6 }, (_, i) => spec(`mcp__x__m${i}`, `Tool ${i}.`, 600)); // ~1k tokens
    expect(chooseMode("auto", medium, 8192).mode).toBe("deferred");
    expect(chooseMode("auto", medium, undefined).mode).toBe("eager");
  });
  test("deferring must save at least half, or it isn't worth the extra step", () => {
    // 60 tools whose whole description is one ~90-char sentence and whose schemas are nearly empty: the note would
    // repeat most of what the specs say.
    const wordy = Array.from({ length: 60 }, (_, i) => spec(`mcp__x__w${i}`, `${"Lists things ".repeat(7)}${i}.`));
    const choice = chooseMode("auto", wordy, 8192);
    expect(choice.specTokens).toBeGreaterThan(819);
    expect(choice.mode).toBe("eager");
  });
  test("the overrides win", () => {
    expect(chooseMode("deferred", small, 32_768).mode).toBe("deferred");
    expect(chooseMode("eager", big, 32_768).mode).toBe("eager");
  });
});

describe("searchTools", () => {
  const specs = [spec("mcp__gh__create_issue", "Create an issue."), spec("mcp__gh__search_code", "Search code."), spec("mcp__gh__get_issue", "Get one issue.")];

  test("by exact names, unknown names reported", () => {
    expect(searchTools(specs, { names: ["mcp__gh__get_issue", "mcp__gh__nope"] })).toEqual({ found: [specs[2]!], unknown: ["mcp__gh__nope"] });
  });
  test("by words in names and descriptions, best match first, ties in list order", () => {
    expect(searchTools(specs, { query: "create issue" }).found.map((s) => s.name)).toEqual(["mcp__gh__create_issue", "mcp__gh__get_issue"]);
  });
  test("at most 5", () => {
    const many = Array.from({ length: 9 }, (_, i) => spec(`mcp__x__issue${i}`, "Issue tool."));
    expect(searchTools(many, { query: "issue" }).found).toHaveLength(5);
  });
});

test("the two deferred specs are constants: the same bytes every time (prompt cache)", () => {
  expect(DEFERRED_SPECS.map((s) => s.name)).toEqual(["tool_search", "mcp_call"]);
  expect(JSON.stringify(DEFERRED_SPECS)).toBe(JSON.stringify(DEFERRED_SPECS));
});

describe("the deferred tools", () => {
  const tools = [fakeTool("mcp__x__write"), fakeTool("mcp__x__read", { kind: "read" }), fakeTool("mcp__x__broken", { fail: "q is required" })];
  const specs: ToolSpec[] = tools.map((t) => ({ name: t.name, description: t.description, parameters: { type: "object", properties: { q: { type: "string" } }, required: ["q"] } }));
  let id = 0;
  const call = (name: string, args: unknown, extra: { approve?: (r: ApprovalRequest) => Promise<"yes" | "no">; yolo?: boolean } = {}) => {
    const loaded = new Set<string>();
    const offered = deferredTools({ tools, specs, loaded });
    return { loaded, result: runTool({ id: `c${id++}`, name, arguments: typeof args === "string" ? args : JSON.stringify(args) }, { root: "/tmp", ...extra }, offered) };
  };

  test("tool_search returns the specs as compact JSON, read-only marked, and remembers them", async () => {
    const { loaded, result } = call("tool_search", { names: ["mcp__x__read", "mcp__x__nope"] });
    const { output } = await result;
    expect(output.split("\n")[0]).toBe(JSON.stringify({ name: "mcp__x__read", description: specs[1]!.description, parameters: specs[1]!.parameters, readOnly: true }));
    expect(output).toContain("Marv: no MCP tool is named mcp__x__nope.");
    expect([...loaded]).toEqual(["mcp__x__read"]);
  });

  test("mcp_call asks with the real tool's prompt and scope, and runs it", async () => {
    const asked: ApprovalRequest[] = [];
    const { result } = call("mcp_call", { tool: "mcp__x__write", arguments: { q: "hi" } }, { approve: async (r) => (asked.push(r), "yes") });
    const done = await result;
    expect(asked.map((r) => [r.tool, r.scope.key])).toEqual([["mcp__x__write", "mcp:x:mcp__x__write"]]);
    expect(done.output).toBe('mcp__x__write got {"q":"hi"}');
    expect(done.label).toBe("mcp__x__write");
    expect(done.approval).toBe("yes");
  });

  test("yolo doesn't skip the asking; a read-only tool runs unasked", async () => {
    const noOne = await call("mcp_call", { tool: "mcp__x__write", arguments: {} }, { yolo: true }).result;
    expect(noOne.output).toContain("needs the user's approval");
    expect((await call("mcp_call", { tool: "mcp__x__read", arguments: { q: "a" } }).result).isError).toBeFalsy();
  });

  test("a no stops the run like any decline", async () => {
    expect(await call("mcp_call", { tool: "mcp__x__write", arguments: {} }, { approve: async () => "no" }).result).toMatchObject({ declined: true, approval: "no" });
  });

  test("an unknown tool's error names the closest ones", async () => {
    const { output, isError } = await call("mcp_call", { tool: "mcp__x__wrte" }).result;
    expect(isError).toBe(true);
    expect(output).toContain('No MCP tool is named "mcp__x__wrte"');
  });

  test("a rejected call to a tool not yet loaded gets its schema, once", async () => {
    const loaded = new Set<string>();
    const offered = deferredTools({ tools, specs, loaded });
    const run = () => runTool({ id: `c${id++}`, name: "mcp_call", arguments: '{"tool":"mcp__x__broken","arguments":{}}' }, { root: "/tmp", approve: async () => "yes" }, offered);
    expect((await run()).output).toContain(`Marv: mcp__x__broken takes: ${JSON.stringify(specs[2]!.parameters)}`);
    expect((await run()).output).not.toContain("takes:");
  });

  test("arguments sent as a JSON string are read as the object (#10), and the result says so", async () => {
    const { output } = await call("mcp_call", '{"tool":"mcp__x__read","arguments":"{\\"q\\":\\"a\\"}"}').result;
    expect(output).toContain('mcp__x__read got {"q":"a"}');
    expect(output).toContain("Marv: mcp_call's arguments came as a JSON string");
  });
});
