// Deferred MCP tools (#14). Sending every MCP tool's full spec with every request costs thousands of tokens once a
// few servers are connected, and gives small models dozens of tools to choose from. Past a threshold, the model is
// offered two fixed tools instead, tool_search (a tool's full spec, as a tool result) and mcp_call (runs one), and
// learns the MCP tools' names from a note from Marv appended to the conversation. The tool list and the system
// prompt never change, so the prompt cache holds (CLAUDE.md, "Prompt cache").
import { z } from "zod";
import { printable } from "../printable.ts";
import type { ToolCall, ToolSpec } from "../provider/types.ts";
import { runTool, specOf } from "../tools/index.ts";
import { repairJsonArgs } from "../tools/repair.ts";
import { ToolError, type Tool } from "../tools/types.ts";
import { estimateTokens } from "../usage.ts";

export type McpToolsSetting = "auto" | "deferred" | "eager";
export type McpMode = "deferred" | "eager";
export const MCP_TOOLS_SETTINGS = ["auto", "deferred", "eager"] as const;

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
  // A sentence ends at . ! or ? followed by a space or the end: "v2.1" doesn't end one.
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

const specFrom = (name: string, description: string, input: z.ZodType): ToolSpec => specOf({ name, description, input } as Tool);
/** What a deferred conversation is offered in place of the MCP specs: built once, the same bytes all session. */
export const DEFERRED_SPECS: ToolSpec[] = [specFrom("tool_search", SEARCH_DESCRIPTION, searchInput), specFrom("mcp_call", CALL_DESCRIPTION, callInput)];
export const DEFERRED_NAMES = DEFERRED_SPECS.map((s) => s.name);

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

/** Whether `text` is (or #10's repair makes it) exactly one JSON object. */
function oneObject(text: string): boolean {
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch {
    value = repairJsonArgs(text)?.value;
  }
  return !!value && typeof value === "object" && !Array.isArray(value);
}

/**
 * #10/#14: a direct call to an MCP tool the conversation defers, as the mcp_call it meant. Arguments that are one
 * object (or that #10's JSON repair makes one) are spliced in as they came, for runTool's repair to see; anything
 * else is passed as a string, which mcp_call rejects. Spliced, `{},"tool":"…"` would make a valid mcp_call whose
 * second "tool" key (JSON keeps the last) names another tool than the one called.
 */
export function asMcpCall(call: ToolCall, mcpNames: ReadonlySet<string>): { call: ToolCall; note: string } | null {
  if (!mcpNames.has(call.name)) return null;
  const raw = call.arguments.trim() || "{}";
  const args = oneObject(raw) ? raw : JSON.stringify(raw);
  return {
    call: { ...call, name: "mcp_call", arguments: `{"tool":${JSON.stringify(call.name)},"arguments":${args}}` },
    note: `Marv: ${call.name} isn't offered as a tool of its own: it was run as mcp_call with tool "${call.name}". Call MCP tools that way.`,
  };
}
