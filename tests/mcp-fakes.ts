// A stand-in for an MCP tool (what makeMcpTool builds), for tests that don't need a real server.
import { z } from "zod";
import { ToolError, type Tool } from "../src/tools/types.ts";

export function fakeTool(name: string, opts: { kind?: "read" | "execute"; fail?: string } = {}): Tool {
  return {
    name,
    description: `(MCP server "x") Does ${name}.`,
    input: z.record(z.string(), z.unknown()),
    label: () => name,
    kind: opts.kind ?? "execute",
    scope: () => ({ key: `mcp:x:${name}`, description: name }),
    async preview(args: unknown) {
      return { title: name, text: JSON.stringify(args) };
    },
    async run(args: unknown) {
      if (opts.fail) throw new ToolError(opts.fail);
      return { output: `${name} got ${JSON.stringify(args)}`, summary: "ok" };
    },
  } as Tool;
}
