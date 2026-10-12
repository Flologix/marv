// An MCP server's tool, as one of Marv's tools: the model sees it as
// mcp__<server>__<tool>, with the server's own description and JSON schema,
// and runTool gates it like any other tool. MCP servers run outside the
// sandbox, with the user's permissions and network, so a call asks first,
// unless the server marks the tool read-only (readOnlyHint), and yolo mode
// never skips the asking (no autoSafe: nothing contains what it does).
import { createHash } from "node:crypto";
import { z } from "zod";
import type { ToolSpec } from "../provider/types.ts";
import { ToolError, type Tool } from "../tools/types.ts";
import type { McpServerConfig } from "./config.ts";
import { LoginNeeded } from "./oauth.ts";

/** What a server says about one of its tools (tools/list). */
export interface McpToolInfo {
  name: string;
  description?: string;
  inputSchema: Record<string, unknown>;
  annotations?: { readOnlyHint?: boolean; title?: string };
}

/** One content block of a tools/call result. */
type Block = { type: string; text?: string; mimeType?: string; data?: string; uri?: string; name?: string; resource?: { uri?: string; text?: string } };
export interface McpCallResult {
  content?: Block[];
  structuredContent?: unknown;
  isError?: boolean;
}

export type McpCall = (tool: string, args: Record<string, unknown>, signal?: AbortSignal) => Promise<McpCallResult>;

const MAX_NAME = 64; // OpenAI-compatible APIs reject longer tool names
const MAX_DESCRIPTION = 2000;
const MAX_OUTPUT = 30_000;
const PREVIEW_CHARS = 1500;

const clean = (text: string) => text.replace(/[^A-Za-z0-9_-]/g, "_");
const hash = (text: string) => createHash("sha256").update(text).digest("hex").slice(0, 8);

/** mcp__<server>__<tool>, only characters every provider accepts, at most 64 long (a hash keeps a shortened name unique). */
export function mcpToolName(server: string, tool: string): string {
  const name = `mcp__${server}__${clean(tool)}`;
  if (name.length <= MAX_NAME && clean(tool) === tool) return name;
  return `${name.slice(0, MAX_NAME - 9)}_${hash(tool)}`;
}

/** The model reads text: images and other binary blocks are named, not sent. */
export function formatMcpResult(result: McpCallResult): string {
  const parts = (result.content ?? []).map((block) => {
    switch (block.type) {
      case "text":
        return block.text ?? "";
      case "image":
      case "audio":
        return `[${block.type} (${block.mimeType ?? "unknown type"}), ${Math.round(((block.data?.length ?? 0) * 3) / 4 / 1024)} KB, not shown: Marv passes text only]`;
      case "resource":
        return block.resource?.text ?? `[resource ${block.resource?.uri ?? ""}]`;
      case "resource_link":
        return `[link: ${block.uri ?? ""}${block.name ? ` (${block.name})` : ""}]`;
      default:
        return `[${block.type} content, not shown]`;
    }
  });
  let text = parts.join("\n");
  if (!text && result.structuredContent !== undefined) text = JSON.stringify(result.structuredContent, null, 2);
  if (!text) text = "(no output)";
  if (text.length > MAX_OUTPUT) {
    text = `${text.slice(0, MAX_OUTPUT / 3)}\n\n… (${text.length - MAX_OUTPUT} characters omitted) …\n\n${text.slice(-((2 * MAX_OUTPUT) / 3))}`;
  }
  return text;
}

/** For the transcript: the first short string argument, or the arguments as JSON. */
function labelOf(args: Record<string, unknown>): string {
  const first = Object.values(args).find((v) => typeof v === "string" && v.length > 0) as string | undefined;
  const text = first ?? (Object.keys(args).length ? JSON.stringify(args) : "");
  return text.length > 60 ? `${text.slice(0, 59)}…` : text;
}

const input = z.record(z.string(), z.unknown());

/** The tool Marv runs, and the spec the model is sent (the server's own schema, unchanged). */
export function makeMcpTool(server: McpServerConfig, info: McpToolInfo, call: McpCall): { tool: Tool; spec: ToolSpec } {
  const name = mcpToolName(server.name, info.name);
  const about = info.description?.trim() ?? "";
  const description = `(MCP server "${server.name}") ${about.length > MAX_DESCRIPTION ? `${about.slice(0, MAX_DESCRIPTION)}…` : about}`.trim();
  const readOnly = info.annotations?.readOnlyHint === true;
  const where = `MCP server "${server.name}" (${server.source === "project" ? "this project's .mcp.json" : "yours"}) · runs outside the sandbox`;

  const tool: Tool<typeof input> = {
    name,
    description,
    input,
    label: labelOf,
    // The server's own claim: you chose to run this server, so its read-only tools run freely.
    kind: readOnly ? "read" : "execute",
    usesNetwork: () => true,
    scope: () => ({ key: `mcp:${server.name}:${info.name}`, description: `${server.name}'s ${info.name}` }),
    async preview(args) {
      const json = JSON.stringify(args, null, 2);
      return {
        title: `${server.name}: ${info.annotations?.title ?? info.name}`,
        text: json === "{}" ? undefined : json.length > PREVIEW_CHARS ? `${json.slice(0, PREVIEW_CHARS)}…` : json,
        note: where,
      };
    },
    async run(args, { signal }) {
      let result: McpCallResult;
      try {
        result = await call(info.name, args, signal);
      } catch (err) {
        // Marv's own sentence, as it is: the model should tell the user to log in, not retry.
        if (err instanceof LoginNeeded) throw new ToolError(err.message);
        throw new ToolError(`${server.name}'s ${info.name} failed: ${err instanceof Error ? err.message : String(err)}`);
      }
      const output = formatMcpResult(result);
      if (result.isError) return { output, summary: "error", isError: true };
      const lines = output.split("\n").length;
      return { output, summary: `${lines} line${lines === 1 ? "" : "s"}` };
    },
  };
  return { tool: tool as Tool, spec: { name, description, parameters: acceptableSchema(info.inputSchema) } };
}

/**
 * The server's schema, made acceptable to every provider: one tool they reject fails every request of the
 * session (the tool list can't change), so it's better to send a looser schema; the server validates the
 * arguments anyway. `$schema` (which SDK servers include) is dropped; a root that isn't a plain object
 * (anyOf/oneOf/allOf, or another type) becomes an open object; there's always a `properties`.
 */
export function acceptableSchema(schema: Record<string, unknown>): Record<string, unknown> {
  const { $schema: _, ...rest } = schema;
  const plainObject = (rest.type === undefined || rest.type === "object") && !("anyOf" in rest || "oneOf" in rest || "allOf" in rest);
  if (!plainObject) return { type: "object", properties: {} };
  return { ...rest, type: "object", properties: rest.properties ?? {} };
}
