# Deferred MCP Tools Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Issue #14. When the MCP servers' tool specs would cost more than min(4000 tokens, 10% of the context window) per request, Marv offers two fixed tools instead (`tool_search`, `mcp_call`) and tells the model the MCP tools' names in a note from Marv, appended to the conversation. The tool list and the system prompt never change, so the prompt cache holds.

**Architecture:** One new module, `src/mcp/deferred.ts`, holds everything specific to deferral. Its pure parts are the one-line listing, the note, the threshold and the search. It also builds the two tools, which close over the turn's MCP tools. `mcp_call` runs the real MCP tool through `runTool`, so that tool's approval, scope and yolo rules apply unchanged. `runAgent` gets one generic hook, `rewriteCall`, applied before a reply enters the history; the session uses it to turn a direct `mcp__x__y` call into an `mcp_call`. The session owns the per-conversation state: the mode, the names already announced, and the schemas already shown. It also decides the mode, announces new names at the start of a turn's message, and resets on compaction, `clear()` and `resume()`. Config, the SDK, trajectories and the evals each get a small, separate change.

**Tech Stack:** Bun, TypeScript, zod, `@modelcontextprotocol/sdk` (test fixtures), `bun:test`.

Spec: `docs/superpowers/specs/2026-10-12-deferred-mcp-tools.md` (approved 2026-10-12: token threshold, list in appended notes). Branch: `s2/14-deferred-mcp-tools`.

**One deliberate change from the spec:** the trajectory records the mode in the `turn_start` of the turn that decided it, not in the `session` record. The `session` record is written once per file per process, often before any MCP tool exists. The note itself gets its own record (`mcp_note`), since it's written after `turn_start`, once a pre-turn compaction has had its say.

**Background for the engineer:**
- Read `CLAUDE.md` first, especially **MCP servers**, **Session**, **Agent loop**, **Tools** (incl. malformed-call repair) and **Prompt cache (don't break it)**.
- **Cache rule:** never change `system`, the `tools` list, or any message already in `conversation`. Only append. A deferred conversation's tool list is `[...builtin specs, ...DEFERRED_SPECS]`, constant whatever the servers do.
- **Where the note goes:** at the start of the user message the model gets (`note + "\n\n" + forModel`), never as a separate user message (two user messages in a row), and never in the transcript (`this.note("user", text)` keeps the user's own text).
- **Conventions:** `.ts` extensions in imports, `import type` for types, comments say *why*. `ScriptedProvider` (`tests/fake-provider.ts`) records every request (`requests[i].history`, `requests[i].options.tools`, `.system`). `tests/session.test.ts` has `fakeMcp()` standing in for `McpManager`.
- **Commands:** `bun test <path substring>`, `bun test -t "<name>"`, `bun run typecheck`, `bun test` (whole suite, ~20 s).

---

### Task 1: The pure parts (`src/mcp/deferred.ts`)

**Files:**
- Create: `src/mcp/deferred.ts`
- Test: `tests/mcp-deferred.test.ts`

- [ ] **Step 1: Write the failing tests**

`tests/mcp-deferred.test.ts`:

```ts
import { describe, expect, test } from "bun:test";
import { announcement, chooseMode, DEFERRED_SPECS, MAX_LINE, oneLine, searchTools } from "../src/mcp/deferred.ts";
import type { ToolSpec } from "../src/provider/types.ts";

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
    // repeat most of what the specs say (measured: ~3.6k tokens of specs vs ~2k deferred).
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
```

The numbers were checked against this logic (4 chars a token): small ≈ 75 tokens; big ≈ 5.6k vs ≈ 0.5k deferred; medium ≈ 1.1k, over 10% of 8192 (819) but under 4000; wordy ≈ 3.6k vs ≈ 2k. If the final descriptions shift them, adjust the counts in the tests, not the thresholds.

- [ ] **Step 2: Run them to make sure they fail**

Run: `bun test tests/mcp-deferred`
Expected: FAIL, `Cannot find module '../src/mcp/deferred.ts'`.

- [ ] **Step 3: Write the pure parts**

`src/mcp/deferred.ts`:

```ts
// Deferred MCP tools (#14). Sending every MCP tool's full spec with every request costs thousands of tokens once a
// few servers are connected, and gives small models dozens of tools to choose from. Past a threshold, the model is
// offered two fixed tools instead, tool_search (a tool's full spec, as a tool result) and mcp_call (runs one), and
// learns the MCP tools' names from a note from Marv appended to the conversation. The tool list and the system
// prompt never change, so the prompt cache holds (CLAUDE.md, "Prompt cache").
import { z } from "zod";
import { printable } from "../printable.ts";
import type { ToolCall, ToolSpec } from "../provider/types.ts";
import { estimateTokens } from "../usage.ts";

export type McpToolsSetting = "auto" | "deferred" | "eager";
export type McpMode = "deferred" | "eager";

/** A listed tool's description, at most. */
export const MAX_LINE = 100;
/** Defer when the MCP specs cost more than this many tokens per request… */
export const DEFER_TOKENS = 4000;
/** …or more than this share of the context window, whichever is less. */
export const DEFER_SHARE = 0.1;
/** Specs tool_search returns at most. */
export const MAX_RESULTS = 5;

/** The description makeMcpTool wrote, without its `(MCP server "x")` prefix (the name says the server), as its first sentence on one line. */
export function oneLine(description: string): string {
  const about = printable(description)
    .replace(/^\(MCP server "[^"]*"\)\s*/, "")
    .replace(/\s+/g, " ")
    .trim();
  const sentence = /^.*?[.!?](?=\s|$)/.exec(about)?.[0] ?? about;
  return sentence.length > MAX_LINE ? `${sentence.slice(0, MAX_LINE - 1)}…` : sentence;
}

/** What Marv tells the model about MCP tools it hasn't heard of, and ones that are gone; null when there's nothing to say. */
export function announcement(added: ToolSpec[], gone: string[]): string | null {
  const parts: string[] = [];
  if (added.length) {
    const lines = added.map((s) => {
      const about = oneLine(s.description);
      return about ? `- ${s.name}: ${about}` : `- ${s.name}`;
    });
    parts.push(`Marv: MCP tools you can use (not loaded: call tool_search with their names to see their parameters, then run one with mcp_call):\n${lines.join("\n")}`);
  }
  if (gone.length) parts.push(`Marv: these MCP tools are no longer available: ${gone.join(", ")}.`);
  return parts.length ? parts.join("\n\n") : null;
}

export interface ModeChoice {
  mode: McpMode;
  /** What the MCP specs would add to every request. */
  specTokens: number;
  /** What deferral costs instead: the two specs plus the note. */
  deferredTokens: number;
  count: number;
}

/**
 * Whether a conversation defers its MCP tools. Decided once, when a turn first has MCP tools, and kept for the
 * conversation: switching would change the tool list. Tokens, not a tool count: schemas vary 20× in size, and tokens
 * are what costs money and context.
 */
export function chooseMode(setting: McpToolsSetting, specs: ToolSpec[], contextLength: number | undefined): ModeChoice {
  const specTokens = estimateTokens(JSON.stringify(specs));
  const deferredTokens = estimateTokens(JSON.stringify(DEFERRED_SPECS)) + estimateTokens(announcement(specs, []) ?? "");
  const limit = Math.min(DEFER_TOKENS, contextLength ? contextLength * DEFER_SHARE : DEFER_TOKENS);
  // Deferral costs a tool_search step per tool first used, and nested arguments: only worth it when it saves half.
  const defer = setting === "deferred" || (setting === "auto" && specTokens > limit && deferredTokens * 2 <= specTokens);
  return { mode: defer ? "deferred" : "eager", specTokens, deferredTokens, count: specs.length };
}

/** tool_search: exact names first, then the best word matches (no embeddings: names and descriptions are short). */
export function searchTools(specs: ToolSpec[], { names = [], query }: { names?: string[]; query?: string }): { found: ToolSpec[]; unknown: string[] } {
  const found: ToolSpec[] = [];
  const unknown: string[] = [];
  for (const name of names) {
    const match = specs.find((s) => s.name === name);
    if (!match) unknown.push(name);
    else if (!found.includes(match)) found.push(match);
  }
  const words = (query ?? "").toLowerCase().split(/[^a-z0-9]+/).filter(Boolean);
  if (words.length) {
    const scored = specs
      .map((s, i) => ({ s, i, score: words.filter((w) => `${s.name} ${s.description}`.toLowerCase().includes(w)).length }))
      .filter((x) => x.score > 0 && !found.includes(x.s))
      .sort((a, b) => b.score - a.score || a.i - b.i);
    found.push(...scored.map((x) => x.s));
  }
  return { found: found.slice(0, MAX_RESULTS), unknown };
}

export const searchInput = z.object({
  names: z.array(z.string()).optional().describe("Exact tool names (mcp__<server>__<tool>) to load."),
  query: z.string().optional().describe("Words to look for in the tools' names and descriptions."),
});
export const callInput = z.object({
  tool: z.string().describe("The MCP tool's name, mcp__<server>__<tool>."),
  arguments: z.record(z.string(), z.unknown()).optional().describe("Its arguments, as an object matching its parameters (see tool_search)."),
});
export const SEARCH_DESCRIPTION =
  "Loads MCP tools' parameters: by exact names (from Marv's list of MCP tools in the conversation), or by words. Returns each tool's name, description and JSON schema (at most 5). Then run it with mcp_call.";
export const CALL_DESCRIPTION =
  "Runs an MCP tool: tool is its name (mcp__<server>__<tool>), arguments an object matching its parameters. Load them with tool_search first. MCP tools run outside the sandbox; the user may be asked first.";

const specFrom = (name: string, description: string, input: z.ZodType): ToolSpec => {
  const { $schema: _, ...parameters } = z.toJSONSchema(input) as Record<string, unknown>;
  return { name, description, parameters };
};
/** What a deferred conversation is offered in place of the MCP specs: built once, the same bytes all session. */
export const DEFERRED_SPECS: ToolSpec[] = [specFrom("tool_search", SEARCH_DESCRIPTION, searchInput), specFrom("mcp_call", CALL_DESCRIPTION, callInput)];
export const DEFERRED_NAMES = DEFERRED_SPECS.map((s) => s.name);
```

`specFrom` matches `specOf` in `src/tools/index.ts`; it isn't imported from there, so this file stays pure. Task 2 adds the tools, which import `runTool`.

- [ ] **Step 4: Run the tests**

Run: `bun test tests/mcp-deferred && bun run typecheck`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/mcp/deferred.ts tests/mcp-deferred.test.ts
git commit -m "Deferred MCP tools: listing, note, threshold, search (#14)"
```

---

### Task 2: `tool_search` and `mcp_call`

**Files:**
- Modify: `src/mcp/deferred.ts`
- Test: `tests/mcp-deferred.test.ts`, `tests/mcp.test.ts`

- [ ] **Step 1: Write the failing tests**

Append to `tests/mcp-deferred.test.ts` (and add `deferredTools` to the import from `../src/mcp/deferred.ts`):

```ts
import { z } from "zod";
import { runTool } from "../src/tools/index.ts";
import { ToolError, type ApprovalRequest, type Tool } from "../src/tools/types.ts";

function fakeTool(name: string, opts: { kind?: "read" | "execute"; fail?: string } = {}): Tool {
  return {
    name,
    description: `(MCP server "x") Does ${name}.`,
    input: z.record(z.string(), z.unknown()),
    label: () => name,
    kind: opts.kind ?? "execute",
    scope: () => ({ key: `mcp:x:${name}`, description: name }),
    async preview(args) {
      return { title: name, text: JSON.stringify(args) };
    },
    async run(args) {
      if (opts.fail) throw new ToolError(opts.fail);
      return { output: `${name} got ${JSON.stringify(args)}`, summary: "ok" };
    },
  } as Tool;
}

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
```

In `tests/mcp.test.ts`, add one end-to-end test against the real fixture server, inside `describe("a local (stdio) server", …)`:

```ts
test("deferred: tool_search and mcp_call reach the server's tools (#14)", async () => {
  const m = await manager([stdio()]);
  const offered = deferredTools({ tools: m.tools, specs: m.specs, loaded: new Set() });
  const asked: ApprovalRequest[] = [];
  const run = (name: string, args: unknown) => runTool({ id: `m${nextId++}`, name, arguments: JSON.stringify(args) }, { root, approve: async (r) => (asked.push(r), "yes") }, offered);
  expect((await run("tool_search", { query: "echo" })).output).toContain('"name":"mcp__test__echo"');
  expect((await run("mcp_call", { tool: "mcp__test__echo", arguments: { text: "hi" } })).output).toBe("echo: hi");
  expect(asked.map((r) => r.scope.key)).toEqual(["mcp:test:echo"]);
  expect((await run("mcp_call", { tool: "mcp__test__lookup", arguments: { word: "w" } })).output).toBe("w: a word");
  expect(asked).toHaveLength(1); // lookup is read-only
});
```

Use the file's own `manager()` helper as the other tests do. If it doesn't return a started manager, call `await m.start()` first, as they do.

- [ ] **Step 2: Run them to make sure they fail**

Run: `bun test tests/mcp-deferred tests/mcp.test`
Expected: FAIL, `deferredTools` is not exported.

- [ ] **Step 3: Write the tools**

Add to `src/mcp/deferred.ts`:

```ts
import { runTool } from "../tools/index.ts";
import { ToolError, type Tool } from "../tools/types.ts";

/** A turn's MCP tools and specs, and the names whose schema this conversation has seen (shared across turns). */
export interface DeferredContext {
  tools: Tool[];
  specs: ToolSpec[];
  loaded: Set<string>;
}

/**
 * tool_search and mcp_call, over a turn's MCP tools. mcp_call itself never asks: it hands the call to runTool with
 * the real tool, whose preview, approval (kind, scope, no autoSafe: yolo never skips it), label and result apply
 * exactly as when the tool is offered directly.
 */
export function deferredTools({ tools, specs, loaded }: DeferredContext): Tool[] {
  const readOnly = (name: string) => tools.find((t) => t.name === name)?.kind === "read";
  const toolSearch: Tool<typeof searchInput> = {
    name: "tool_search",
    description: SEARCH_DESCRIPTION,
    input: searchInput,
    label: (input) => input.names?.join(", ") || input.query || "",
    async run(input) {
      const { found, unknown } = searchTools(specs, input);
      for (const s of found) loaded.add(s.name);
      // Compact JSON: a loaded schema costs what its spec would have, once instead of in every request.
      const lines = found.map((s) => JSON.stringify({ name: s.name, description: s.description, parameters: s.parameters, ...(readOnly(s.name) ? { readOnly: true } : {}) }));
      if (unknown.length) lines.push(`Marv: no MCP tool is named ${unknown.join(", ")}.`);
      if (!found.length && !unknown.length) lines.push("No MCP tool matches. Marv's note in this conversation lists them all.");
      return { output: lines.join("\n"), summary: `${found.length} tool${found.length === 1 ? "" : "s"}` };
    },
  };
  const mcpCall: Tool<typeof callInput> = {
    name: "mcp_call",
    description: CALL_DESCRIPTION,
    input: callInput,
    label: (input) => input.tool,
    repair: (raw) => argumentsAsObject(raw),
    async run(input, ctx) {
      const tool = tools.find((t) => t.name === input.tool);
      if (!tool) {
        const close = searchTools(specs, { query: input.tool.replace(/^mcp__/, "") }).found.map((s) => s.name);
        throw new ToolError(`No MCP tool is named "${input.tool}".${close.length ? ` Closest: ${close.join(", ")}.` : ""}`);
      }
      const result = await runTool({ id: ctx.callId ?? "mcp_call", name: tool.name, arguments: JSON.stringify(input.arguments ?? {}) }, ctx, [tool]);
      // A guess at the arguments that the server rejected: show the schema once, so a guess costs one step, not two.
      if (result.isError && !result.declined && !loaded.has(tool.name)) {
        loaded.add(tool.name);
        const spec = specs.find((s) => s.name === tool.name);
        if (spec) return { ...result, output: `${result.output}\n\nMarv: ${tool.name} takes: ${JSON.stringify(spec.parameters)}` };
      }
      return result;
    },
  };
  return [toolSearch, mcpCall] as Tool[];
}

/** #10: `arguments` sent as a JSON string of an object. Certain, so it's read as the object (with a note). */
export function argumentsAsObject(raw: unknown): { input: unknown; note: string } | null {
  if (!raw || typeof raw !== "object" || typeof (raw as { arguments?: unknown }).arguments !== "string") return null;
  try {
    const parsed: unknown = JSON.parse((raw as { arguments: string }).arguments);
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return null;
    return { input: { ...raw, arguments: parsed }, note: "Marv: mcp_call's arguments came as a JSON string; it was read as the object inside. Pass arguments as an object." };
  } catch {
    return null;
  }
}
```

The outer `runTool` spreads the inner result and puts its own `label` on top (`mcp_call`'s label is the tool's name), so `approval`, `declined` and `diff` come through. Move the header comment's imports together, and remove the comment from Task 1 that says the file stays pure.

- [ ] **Step 4: Run the tests**

Run: `bun test tests/mcp-deferred tests/mcp.test tests/tool-call-repair && bun run typecheck`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/mcp/deferred.ts tests/mcp-deferred.test.ts tests/mcp.test.ts
git commit -m "Deferred MCP tools: tool_search and mcp_call (#14)"
```

---

### Task 3: `runAgent`'s `rewriteCall` hook and the direct-call repair

**Files:**
- Modify: `src/agent.ts`, `src/mcp/deferred.ts`
- Test: `tests/agent.test.ts`, `tests/tool-call-repair.test.ts`

A model that read `mcp__x__y` in the note may call it as a tool. It must not enter the history under that name: a provider may reject a conversation whose history calls a tool that isn't in `tools`, and every later request would then fail. So the call is rewritten before the reply enters the history, in the loop where ids are made unique.

- [ ] **Step 1: Write the failing tests**

`tests/agent.test.ts`:

```ts
test("rewriteCall changes a call before it enters the history, and the call runs as rewritten", async () => {
  const provider = new ScriptedProvider([
    [{ type: "tool_call", call: { id: "c1", name: "mcp__x__ping", arguments: '{"a":1}' } }, { type: "done", reason: "tool_calls" }],
    [{ type: "text_delta", text: "Done." }, { type: "done", reason: "stop" }],
  ]);
  const history: ChatTurn[] = [{ role: "user", text: "ping" }];
  const ran: ToolCall[] = [];
  for await (const _ of runAgent({
    provider,
    history,
    system: "s",
    tools: [],
    signal: new AbortController().signal,
    runTool: async (call) => (ran.push(call), { output: "ok", summary: "ok", label: call.name }),
    rewriteCall: (call) => (call.name === "mcp__x__ping" ? { ...call, name: "mcp_call", arguments: `{"tool":"mcp__x__ping","arguments":${call.arguments}}` } : null),
  }));
  expect(ran.map((c) => c.name)).toEqual(["mcp_call"]);
  expect(provider.requests[1]!.history[1]).toMatchObject({ role: "assistant", toolCalls: [{ id: "c1", name: "mcp_call", arguments: '{"tool":"mcp__x__ping","arguments":{"a":1}}' }] });
});
```

`tests/tool-call-repair.test.ts`:

```ts
describe("a direct call to a deferred MCP tool (#14)", () => {
  const names = new Set(["mcp__x__ping"]);
  test("becomes mcp_call, its raw arguments kept byte for byte", () => {
    expect(asMcpCall({ id: "c", name: "mcp__x__ping", arguments: '{ "a": 1 }' }, names)).toEqual({
      call: { id: "c", name: "mcp_call", arguments: '{"tool":"mcp__x__ping","arguments":{ "a": 1 }}' },
      note: 'Marv: mcp__x__ping isn\'t offered as a tool of its own: it was run as mcp_call with tool "mcp__x__ping". Call MCP tools that way.',
    });
    expect(asMcpCall({ id: "c", name: "mcp__x__ping", arguments: "" }, names)?.call.arguments).toBe('{"tool":"mcp__x__ping","arguments":{}}');
  });
  test("a name that isn't a configured MCP tool is left alone (an unknown-tool error)", () => {
    expect(asMcpCall({ id: "c", name: "mcp__x__nope", arguments: "{}" }, names)).toBeNull();
    expect(asMcpCall({ id: "c", name: "read_file", arguments: "{}" }, names)).toBeNull();
  });
});
```

(import `asMcpCall` from `../src/mcp/deferred.ts`).

- [ ] **Step 2: Run them to make sure they fail**

Run: `bun test tests/agent.test tests/tool-call-repair`
Expected: FAIL. TypeScript ignores the unknown `rewriteCall` option at runtime, so the call runs as `mcp__x__ping`, and `asMcpCall` isn't exported.

- [ ] **Step 3: Implement**

`src/agent.ts`, in `interface Options`:

```ts
  /**
   * Applied to each call of a reply before it enters the history (after ids are made unique): a call to run and
   * record instead, or null to keep it. The session turns a direct call to a deferred MCP tool into mcp_call (#14),
   * so the history never names a tool that isn't offered.
   */
  rewriteCall?: (call: ToolCall) => ToolCall | null;
```

Destructure it in `runAgent`, and right after the id loop (`for (const call of calls) { … usedIds.add(call.id); }`):

```ts
    if (rewriteCall) for (let i = 0; i < calls.length; i++) calls[i] = rewriteCall(calls[i]!) ?? calls[i]!;
```

`src/mcp/deferred.ts`:

```ts
/**
 * #10/#14: a direct call to an MCP tool the conversation defers, as the mcp_call it meant. The raw arguments are
 * spliced in unparsed: malformed JSON stays malformed, for runTool's own JSON repair or its error.
 */
export function asMcpCall(call: ToolCall, mcpNames: ReadonlySet<string>): { call: ToolCall; note: string } | null {
  if (!mcpNames.has(call.name)) return null;
  return {
    call: { ...call, name: "mcp_call", arguments: `{"tool":${JSON.stringify(call.name)},"arguments":${call.arguments.trim() || "{}"}}` },
    note: `Marv: ${call.name} isn't offered as a tool of its own: it was run as mcp_call with tool "${call.name}". Call MCP tools that way.`,
  };
}
```

- [ ] **Step 4: Run the tests**

Run: `bun test tests/agent.test tests/tool-call-repair && bun run typecheck`
Expected: PASS, including the existing "every request extends the previous one" tests.

- [ ] **Step 5: Commit**

```bash
git add src/agent.ts src/mcp/deferred.ts tests/agent.test.ts tests/tool-call-repair.test.ts
git commit -m "Agent loop: rewriteCall; a direct call to a deferred MCP tool runs as mcp_call (#14)"
```

---

### Task 4: The session defers, announces and keeps the cache

**Files:**
- Modify: `src/session.ts`, `src/sessions.ts`, `src/trajectory.ts`
- Test: `tests/session.test.ts`

- [ ] **Step 1: Write the failing tests**

In `tests/session.test.ts`, give `fakeMcp` tools (the existing callers still compile):

```ts
function fakeMcp(ready: Promise<void>, specs: ToolSpec[] = [], tools: Tool[] = []): McpManager {
  const mcp = { settled: false, ready, specs, tools, status: () => [] };
  void ready.then(() => (mcp.settled = true));
  return mcp as unknown as McpManager;
}
```

Then a `describe("deferred MCP tools (#14)", …)` block. Reuse the `fakeTool` shape from Task 2: copy it into a small `tests/mcp-fakes.ts` and import it from both test files. `call()` and `say()` are the file's scripted-step helpers; add `call(name, args)` if there isn't one (one `tool_call` event plus `done` `tool_calls`).

```ts
describe("deferred MCP tools (#14)", () => {
  const tools = [fakeTool("mcp__x__a", { kind: "read" }), fakeTool("mcp__x__b", { kind: "read" })];
  const specsOf = (ts: Tool[]): ToolSpec[] => ts.map((t) => ({ name: t.name, description: t.description, parameters: { type: "object", properties: {} } }));
  const NOTE = "Marv: MCP tools you can use";
  const lastUser = (p: ScriptedProvider, i: number) => p.requests[i]!.history.findLast((t) => t.role === "user")!.text;

  test("the tool list and system prompt never change; only appended messages carry the list", async () => {
    const mcp = fakeMcp(Promise.resolve(), specsOf(tools), [...tools]); // a copy: this test adds a tool to it
    const provider = new ScriptedProvider([
      call("tool_search", { names: ["mcp__x__a"] }),
      call("mcp_call", { tool: "mcp__x__a", arguments: {} }),
      say("Done."),
      say("Again."),
    ]);
    const session = makeSession(provider, { mcp, mcpTools: "deferred" });
    await collect(session.send("use a"));
    // /mcp trust between turns: a new server's tool appears
    const late = fakeTool("mcp__y__c", { kind: "read" });
    (mcp as unknown as { specs: ToolSpec[]; tools: Tool[] }).specs.push(...specsOf([late]));
    (mcp as unknown as { tools: Tool[] }).tools.push(late);
    await collect(session.send("and now?"));

    const [first, ...rest] = provider.requests;
    expect(first!.options.tools!.map((t) => t.name)).toEqual([...builtinNames, "tool_search", "mcp_call"]);
    for (const [i, r] of rest.entries()) {
      expect(r.options.tools).toEqual(first!.options.tools);
      expect(r.options.system).toBe(first!.options.system);
      expect(r.history.slice(0, provider.requests[i]!.history.length)).toEqual(provider.requests[i]!.history); // only appended
    }
    expect(lastUser(provider, 0)).toBe(`${NOTE} (not loaded: call tool_search with their names to see their parameters, then run one with mcp_call):\n- mcp__x__a: Does mcp__x__a.\n- mcp__x__b: Does mcp__x__b.\n\nuse a`);
    expect(lastUser(provider, 3)).toBe(`${NOTE} (not loaded: call tool_search with their names to see their parameters, then run one with mcp_call):\n- mcp__y__c: Does mcp__y__c.\n\nand now?`);
  });

  test("a turn with nothing new adds nothing", async () => {
    const provider = new ScriptedProvider([say("One."), say("Two.")]);
    const session = makeSession(provider, { mcp: fakeMcp(Promise.resolve(), specsOf(tools), tools), mcpTools: "deferred" });
    await collect(session.send("one"));
    await collect(session.send("two"));
    expect(lastUser(provider, 1)).toBe("two");
  });

  test("the system prompt is the same whatever the mode", async () => {
    const systemOf = async (mcpTools: "deferred" | "eager") => {
      const provider = new ScriptedProvider([say("Hi.")]);
      await collect(makeSession(provider, { mcp: fakeMcp(Promise.resolve(), specsOf(tools), tools), mcpTools }).send("hi"));
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
    const provider = new ScriptedProvider([call("mcp__x__a", { q: 1 }), say("Done.")]);
    await collect(makeSession(provider, { mcp: fakeMcp(Promise.resolve(), specsOf(tools), tools), mcpTools: "deferred" }).send("go"));
    const history = provider.requests[1]!.history;
    expect(history.find((t) => t.role === "assistant")).toMatchObject({ toolCalls: [{ name: "mcp_call" }] });
    expect(history.find((t) => t.role === "tool")!.text).toContain("Marv: mcp__x__a isn't offered as a tool of its own");
  });

  test("auto mode sends a few small tools as today", async () => {
    const provider = new ScriptedProvider([say("Hi.")]);
    await collect(makeSession(provider, { mcp: fakeMcp(Promise.resolve(), specsOf(tools), tools) }).send("hi"));
    expect(provider.requests[0]!.options.tools!.map((t) => t.name)).toContain("mcp__x__a");
    expect(lastUser(provider, 0)).toBe("hi");
  });

  test("compaction re-announces everything with the next message; clear() starts over", async () => {
    // contextLength 1000, a first reply reporting 900 tokens: the next turn compacts first (COMPACT_AT 0.85)
    const provider = new ScriptedProvider(
      [[{ type: "text_delta", text: "Big." }, { type: "usage", usage: { promptTokens: 880, completionTokens: 20 } }, { type: "done", reason: "stop" }], say("Summary."), say("After.")],
      1000,
    );
    const session = makeSession(provider, { mcp: fakeMcp(Promise.resolve(), specsOf(tools), tools), mcpTools: "deferred" });
    await collect(session.send("one"));
    await collect(session.send("two"));
    expect(lastUser(provider, 2)).toStartWith(NOTE);
    expect(lastUser(provider, 2)).toEndWith("two");
  });

  test("resume() keeps the mode and what was announced, and says what's gone", async () => {
    const store = new SessionStore(join(project, ".sessions"));
    const first = makeSession(new ScriptedProvider([say("Hi.")]), { mcp: fakeMcp(Promise.resolve(), specsOf(tools), tools), mcpTools: "deferred", sessions: store });
    await collect(first.send("hi"));
    await first.flush();
    const onlyA = [tools[0]!];
    const provider = new ScriptedProvider([say("Back.")]);
    const second = makeSession(provider, { mcp: fakeMcp(Promise.resolve(), specsOf(onlyA), onlyA), mcpTools: "eager", sessions: store });
    await second.resume("latest");
    await collect(second.send("back"));
    expect(provider.requests[0]!.options.tools!.map((t) => t.name)).toContain("mcp_call"); // the saved mode wins
    expect(lastUser(provider, 0)).toBe("Marv: these MCP tools are no longer available: mcp__x__b.\n\nback");
  });

  test("a caller's tool can't be named tool_search or mcp_call", () => {
    expect(() => makeSession(new ScriptedProvider([]), { tools: [fakeTool("mcp_call")] })).toThrow(/mcp_call/);
  });
});
```

`builtinNames` is `toolSpecsFor({ hasSkills: false }).map((s) => s.name)`. Adjust the compaction test's numbers to how the existing compaction tests in this file trigger it, reusing their pattern, not these exact values.

- [ ] **Step 2: Run them to make sure they fail**

Run: `bun test tests/session.test -t "deferred MCP"`
Expected: FAIL. `mcpTools` is unknown, and the MCP specs are offered directly.

- [ ] **Step 3: Implement**

`src/sessions.ts`: in `SavedSession`, add:

```ts
  /** The conversation's MCP mode and the tools it has been told about (#14); absent before any had MCP tools. */
  mcp?: { mode: McpMode; announced: string[] };
```

`SessionShape` is a `looseObject`, so old files still load. The session checks the field itself (below), so a hand-edited one is treated as absent rather than failing the whole session.

`src/trajectory.ts`: `turn_start` gains `mcpTools?: ModeChoice` (the turn that decided), and a new record:

```ts
  /** Deferred MCP tools: the note at the start of the turn's message (what the model was told). */
  | { type: "mcp_note"; turn: string; text: string }
```

`src/session.ts`:

1. `SessionInit` gains `mcpTools?: McpToolsSetting` ("auto" by default; read when a conversation decides its mode). `configure()` accepts `mcpTools` too, validated with the other checks before anything changes; it applies from the next conversation that decides (after `clear()`).
2. `checkToolNames` refuses `DEFERRED_NAMES` like the built-in names.
3. Fields:

```ts
  /** This conversation's MCP mode and the tools it has been told about (#14); null until a turn has MCP tools to decide on. */
  private listing: { mode: McpMode; announced: string[] } | null = null;
  /** MCP tools whose schema this conversation has seen (tool_search, or a rejected call): their errors don't repeat it. */
  private readonly loaded = new Set<string>();
  /** The running turn's MCP specs: what a mid-turn compaction re-announces. */
  private turnMcpSpecs: ToolSpec[] = [];
  private mcpTools: McpToolsSetting;
```

4. `offered()`:

```ts
  private offered(): { specs: ToolSpec[]; tools: Tool[] } {
    const mcp = this.init.mcp;
    const tools = [...builtinTools, ...(this.init.tools ?? [])];
    if (!mcp) return { specs: this.specs, tools };
    // Deferred: two constant specs instead of the servers', whatever they do (prompt cache).
    if (this.listing?.mode === "deferred") {
      return { specs: [...this.specs, ...DEFERRED_SPECS], tools: [...tools, ...deferredTools({ tools: mcp.tools, specs: mcp.specs, loaded: this.loaded })] };
    }
    return { specs: [...this.specs, ...mcp.specs], tools: [...tools, ...mcp.tools] };
  }
```

5. Deciding and announcing:

```ts
  /** The first turn with MCP tools decides the conversation's mode; it's kept until clear() (switching would change the tool list). */
  private decideMcp(): ModeChoice | null {
    const mcp = this.init.mcp;
    if (this.listing || !mcp?.specs.length) return null;
    const choice = chooseMode(this.mcpTools, mcp.specs, this.contextLength);
    this.listing = { mode: choice.mode, announced: [] };
    return choice;
  }

  /** What the model hasn't been told about the MCP tools (new ones, gone ones), now marked as told; null when deferred is off or nothing changed. */
  private announce(specs: ToolSpec[]): string | null {
    const listing = this.listing;
    if (listing?.mode !== "deferred") return null;
    const names = new Set(specs.map((s) => s.name));
    const note = announcement(specs.filter((s) => !listing.announced.includes(s.name)), listing.announced.filter((n) => !names.has(n)));
    listing.announced = [...names];
    return note;
  }
```

6. In `runTurn`, after the MCP wait and the aborted check, before `const { specs, tools } = this.offered();`:

```ts
      const mcpChoice = this.decideMcp();
      this.turnMcpSpecs = this.init.mcp?.specs ?? [];
```

Pass `mcpChoice` to `startTurnLog` (it adds `mcpTools` to `turn_start` when set). Replace `this.conversation.push({ role: "user", text: forModel });` with:

```ts
      // After a compaction above (which forgets what was announced), so the note lists everything again.
      const mcpNote = this.announce(this.turnMcpSpecs);
      if (mcpNote) record({ type: "mcp_note", turn, text: mcpNote });
      this.conversation.push({ role: "user", text: mcpNote ? `${mcpNote}\n\n${forModel}` : forModel });
```

In the `runAgent({ … })` call, when deferred:

```ts
      const mcpNames = new Set(this.listing?.mode === "deferred" ? this.turnMcpSpecs.map((s) => s.name) : []);
      const rewritten = new Map<string, string>(); // call id → the repair's note
      …
        rewriteCall: mcpNames.size
          ? (call) => {
              const fixed = asMcpCall(call, mcpNames);
              if (fixed) rewritten.set(call.id, fixed.note);
              return fixed?.call ?? null;
            }
          : undefined,
        runTool: async (call) => {
          const result = await runTool(call, { … as before … }, tools);
          const note = rewritten.get(call.id);
          return note ? { ...result, output: withNote(result.output, note) } : result;
        },
```

7. Compaction: in `summarizeInto`, right after the `splice`:

```ts
    // A summary may drop the list: announce everything again with the next message (the cache starts over here anyway).
    if (this.listing) this.listing.announced = [];
    this.loaded.clear();
```

In `compactIfFull`'s mid-turn branch:

```ts
      const note = this.announce(this.turnMcpSpecs);
      this.conversation.push({ role: "user", text: note ? `${MID_TURN_NOTE}\n\n${note}` : MID_TURN_NOTE });
```

8. `clear()`: `this.listing = null; this.loaded.clear();`. `resume()`: `this.listing = listingOf(saved.mcp); this.loaded.clear();`, where

```ts
const ListingShape = z.object({ mode: z.enum(["deferred", "eager"]), announced: z.array(z.string()) });
/** A saved listing, or null for an old file or a hand edit (then nothing counts as announced). */
const listingOf = (value: unknown) => {
  const parsed = ListingShape.safeParse(value);
  return parsed.success ? { mode: parsed.data.mode, announced: [...parsed.data.announced] } : null;
};
```

`save()`: add `...(this.listing ? { mcp: { mode: this.listing.mode, announced: [...this.listing.announced] } } : {})` to `this.saved`. A `clear()` gets a fresh `newSession`, so nothing carries over.

- [ ] **Step 4: Run the tests**

Run: `bun test tests/session.test tests/agent.test tests/sessions.test tests/mcp && bun run typecheck`
Expected: PASS, the existing MCP session tests too.

- [ ] **Step 5: Commit**

```bash
git add src/session.ts src/sessions.ts src/trajectory.ts tests/session.test.ts tests/mcp-fakes.ts tests/mcp-deferred.test.ts
git commit -m "Session: defer MCP tools past the threshold, announce them in appended notes (#14)"
```

---

### Task 5: Config, SDK and TUI

**Files:**
- Modify: `src/config/config.ts`, `src/sdk.ts`, `src/app.tsx`
- Test: `tests/config.test.ts` (or wherever `diagnostics` is tested), `tests/sdk.test.ts`

- [ ] **Step 1: Write the failing tests**

- Config: `mcpTools` is read from `config.json` (`"deferred"`), defaults to `"auto"`, and a bad value (`"sometimes"`) fails validation like any other bad field. Copy the existing `diagnostics` config test.
- SDK: `createSession({ …, mcpServers: { fx: fixture }, mcpTools: "deferred" })` with a `ScriptedProvider`: the first request's tools include `mcp_call` and no `mcp__fx__…`, and its last user message starts with `Marv: MCP tools you can use`. Use `FIXTURE` and the `marker` pattern the file already has.

- [ ] **Step 2: Run them to make sure they fail**

Run: `bun test tests/config tests/sdk`
Expected: FAIL.

- [ ] **Step 3: Implement**

- `FileConfig`: `mcpTools: z.enum(["auto", "deferred", "eager"]).optional()`. `Config`: `mcpTools: McpToolsSetting`, resolved as `file?.mcpTools ?? "auto"`.
- `src/sdk.ts`: the option, documented like `diagnostics`:

```ts
  /**
   * MCP tools' specs in every request ("eager"), or only their names, in a note, with tool_search and mcp_call to
   * load and run them ("deferred"). Default "auto": deferred when the specs would cost more than min(4000 tokens,
   * 10% of the context window). Decided once per conversation.
   */
  mcpTools?: "auto" | "deferred" | "eager";
```

  It's passed through to `SessionInit.mcpTools`.
- `src/app.tsx`: pass `mcpTools: config.mcpTools` where `diagnostics: config.diagnostics` is passed (the constructor and `configure`). No slash command: it's a setting for people measuring, and `config.json` holds it.

- [ ] **Step 4: Run the tests**

Run: `bun test tests/config tests/sdk tests/app && bun run typecheck`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/config/config.ts src/sdk.ts src/app.tsx tests/
git commit -m "Config and SDK: mcpTools auto|deferred|eager (#14)"
```

---

### Task 6: An MCP fixture and tasks for the evals

**Files:**
- Create: `evals/mcp/tracker.ts`, `evals/mcp/data/*.json`, `evals/tasks/mcp-fix-from-issue/`, `evals/tasks/mcp-comment/`, `evals/tasks/mcp-unused/`
- Modify: `evals/run.ts`, `evals/summary.ts`
- Test: `tests/eval-mcp.test.ts`

The 13 current tasks connect no server, so they can only show that nothing regressed. Deferral's effect needs a server with many tools, of which a task uses one or two.

- [ ] **Step 1: The fixture server**

`evals/mcp/tracker.ts`: an issue tracker over stdio, built like `tests/fixtures/mcp-server.ts`. `bun evals/mcp/tracker.ts <data.json> <out.json>`. It reads issues from `data.json`, which lives outside the workspace so the model can't read it with `read_file`, and appends every comment to `out.json`. 25 tools with realistic descriptions (2 to 3 sentences) and 3 to 4 described parameters each, so the set clearly crosses the threshold:

- Used by the tasks: `get_issue`, `list_issues`, `add_comment`.
- Distractors, answering plausible data: `search_issues`, `create_issue`, `close_issue`, `reopen_issue`, `assign_issue`, `add_label`, `remove_label`, `list_labels`, `get_user`, `list_users`, `list_milestones`, `get_milestone`, `link_pull_request`, `list_comments`, `edit_comment`, `delete_comment`, `get_project`, `list_projects`, `subscribe`, `unsubscribe`, `get_activity`, `export_issues`.

Mark the readers `readOnlyHint: true`. The evals run with no approver, so writers like `add_comment` must be read-only too, or they'd be refused. Give `add_comment` `readOnlyHint: true` with a comment saying why: an eval fixture that writes only its own output file.

- [ ] **Step 2: Wiring in `evals/run.ts`**

- A task folder may have `mcp.json`: `{ "server": "tracker", "data": "<file in evals/mcp/data>" }`. For such a task, `runOne` passes `mcpServers: { tracker: { command: process.execPath, args: [join(import.meta.dir, "mcp", "tracker.ts"), dataFile, outFile] } }` to `createSession`. `outFile` lives in the run's temp folder, beside the workspace, not in it. Before the check runs, `out.json` is copied into the workspace as `__tracker_comments__.json`, the way `check/` is copied in after the run.
- `--mcp-tools auto|deferred|eager` (validated like `--diagnostics`), passed as `mcpTools`. `RunResult` gains `mcpTools` and `mcpCalls` (count of `mcp_call` plus direct `mcp__` calls); the log keeps `tool_search` calls like any tool.
- `--verify`: unchanged for the checks. A task's `solution/` includes `__tracker_comments__.json` where its check needs one.
- `formatSummary`: add prompt tokens per request, if it isn't there yet. That's the number deferral should move.

- [ ] **Step 3: The tasks**

- `mcp-fix-from-issue`: "Fix the bug in issue #12 of the tracker." Issue 12, in `data/fix-from-issue.json`, describes a wrong boundary in `src/range.js` with a reproduction. The check is the repo's hidden tests. The task.md doesn't name the file, so the model must read the issue.
- `mcp-comment`: "Add `--json` output to `src/report.js`, then comment on issue #7 with a one-line summary of the change." The check runs the tests and reads `__tracker_comments__.json`: one comment on issue 7, mentioning `--json`.
- `mcp-unused`: a plain small JS feature (shaped like `add-feature`) with the tracker connected and never needed. It measures what the specs cost when no MCP tool is used.

- [ ] **Step 4: Test the fixture**

`tests/eval-mcp.test.ts`: start `evals/mcp/tracker.ts` through `McpManager` against a temp data file. `chooseMode("auto", m.specs, 32_768).mode` is `"deferred"` and `chooseMode("auto", m.specs, undefined).specTokens > 4000`, so the tasks really measure deferral. `add_comment` writes to the out file.

Run: `bun test tests/eval-mcp && bun run eval --verify && bun run typecheck`
Expected: PASS. Each new check fails on `repo/` and passes with `solution/`.

- [ ] **Step 5: Commit**

```bash
git add evals/ tests/eval-mcp.test.ts
git commit -m "Evals: a 25-tool tracker MCP fixture, three MCP tasks, --mcp-tools (#14)"
```

---

### Task 7: Docs

**Files:**
- Modify: `CLAUDE.md`, `CHANGELOG.md`, `README.md` (if it documents config keys)

- [ ] **Step 1: CLAUDE.md**

In the **MCP servers** bullet, after "The tool list is fixed once the servers have settled", add a sentence on deferral. It should cover:
- the `src/mcp/deferred.ts` names (`chooseMode`, `announcement`, `deferredTools`, `asMcpCall`);
- the threshold (min(4000, 10% of the window), saving at least half, decided once per conversation when a turn first has MCP tools, saved with it);
- what's offered instead (`tool_search`, `mcp_call`, which runs the real tool through `runTool`);
- where the list goes: a note at the start of the turn's message, only names not yet announced, gone ones on one line, never in the transcript;
- the resets: compaction (and mid-turn, after `MID_TURN_NOTE`), `clear()`, `resume()`;
- `rewriteCall`;
- the setting: config/SDK `mcpTools`;
- the `mcp_note` record.

In **Agent loop**, mention `rewriteCall`. In **Prompt cache**, add that the deferred tool list is constant and new MCP tools reach the model only in appended notes. In **Evals**, mention `--mcp-tools`, the tracker fixture and the `mcp-*` tasks.

- [ ] **Step 2: CHANGELOG.md**

Under `## [Unreleased]`:

```markdown
- With many MCP tools connected, Marv no longer sends every tool's full schema with every request. Past min(4000 tokens, 10% of the context window), the model gets the tools' names in a note and loads a tool's parameters with `tool_search` when it needs it, then runs it with `mcp_call` (approvals unchanged). Fewer tokens per request, and the prompt cache holds when a server is trusted mid-session. Config and SDK option `mcpTools`: `auto` (default), `deferred`, `eager`.
```

- [ ] **Step 3: Commit**

```bash
git add CLAUDE.md CHANGELOG.md README.md
git commit -m "Docs: deferred MCP tools (#14)"
```

---

### Task 8: Measure (spends money: ask the user first)

About 2 arms × 3 models × 3 MCP tasks × 3 repetitions = 54 runs, plus the regression check below. Same day, same flags.

- [ ] **Step 1: Run both arms on the MCP tasks**

```bash
M=anthropic/claude-sonnet-5.5,openai/gpt-oss-120b,qwen/qwen3.5-9b
T=mcp-fix-from-issue,mcp-comment,mcp-unused
bun run eval --models $M --tasks $T --repeat 3 --mcp-tools eager --label mcp-eager --budget 1
bun run eval --models $M --tasks $T --repeat 3 --mcp-tools deferred --label mcp-deferred --budget 1
```

Pick a small model that OpenRouter serves today and that is close to what people run on Ollama (around 9B). Check the model ids against the model list first.

- [ ] **Step 2: Regression check**

The 13 existing tasks connect no server, so `--mcp-tools` can't change them. Run one model once on them with this branch and compare with the latest baseline of the same day.

- [ ] **Step 3: Compare and report on #14**

```bash
bun run eval --report evals/results/*-mcp-eager/results.jsonl evals/results/*-mcp-deferred/results.jsonl
```

Per model, compare pass rate, steps, prompt tokens per request and cost. Also look at how often the model called `tool_search` before `mcp_call`, made a direct `mcp__` call (repaired), or guessed arguments and got the schema in the error. Ship `auto` as the default only if deferred is no worse on pass rate and clearly cheaper on `mcp-unused`. Comment the table and a recommendation on #14. The user decides whether to merge.
