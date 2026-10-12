import { existsSync } from "node:fs";
import { z } from "zod";
import { looksBinary, isDirectory, projectPath, requireRegularFile, resolveInProject } from "./files.ts";
import { repairPathField } from "./repair.ts";
import { ToolError, type Tool } from "./types.ts";

const DEFAULT_LIMIT = 400;
const MAX_LINE_CHARS = 500;
// A read of a few lines gives each a share of this, so a long line (an AGENTS.md bullet, issue #40) can be read
// whole with limit=1; a long read stays at MAX_LINE_CHARS per line (a minified file would otherwise flood it).
const LINE_BUDGET_CHARS = 30_000;

const input = z.object({
  path: z.string().describe("File path, relative to the project root (e.g. src/app.ts)."),
  offset: z.number().int().min(1).max(1_000_000).optional().describe("First line to read (1-based). Default 1."),
  limit: z.number().int().min(1).max(2000).optional().describe(`How many lines to read. Default ${DEFAULT_LIMIT}.`),
});

/**
 * A line as read_file shows it: its number, →, then the line exactly as in the file. Not a tab, as cat -n uses:
 * in a tab-indented file the separator and the file's own tab look the same, and models copied old_string with
 * the first line's tab missing and one too many on every other line (eval, 2026-10-06).
 */
export const numberedLine = (n: number, line: string) => `${String(n).padStart(5)}→${line}`;

export const readFile: Tool<typeof input> = {
  name: "read_file",
  description:
    "Read a text file from the project. Returns each line as its line number, →, then the line exactly as in the file (what to copy into edit_file's old_string starts after the →). " +
    `Reads up to ${DEFAULT_LIMIT} lines at a time; use offset and limit to page through longer files. A line longer than ${MAX_LINE_CHARS} characters is cut short in a long read; limit=1 shows it whole.`,
  input,
  label: ({ path }) => path,
  repair: (raw, { root }) => repairPathField(raw, root),

  async run({ path, offset = 1, limit = DEFAULT_LIMIT }, { root }) {
    const absolute = resolveInProject(root, path);
    const shown = projectPath(root, absolute);
    if (isDirectory(absolute)) throw new ToolError(`"${shown}" is a directory. Use glob to list its files.`);
    if (!existsSync(absolute)) throw new ToolError(`File not found: ${path}`);
    requireRegularFile(absolute, shown);
    const file = Bun.file(absolute);

    const bytes = new Uint8Array(await file.arrayBuffer());
    if (looksBinary(bytes)) return { output: `Binary file (${bytes.length} bytes), not shown.`, summary: "binary file" };

    const text = new TextDecoder().decode(bytes);
    if (text === "") return { output: "(empty file)", summary: "empty" };
    const lines = text.replace(/\n$/, "").split("\n");
    const total = lines.length;
    if (offset > total) throw new ToolError(`${shown} has only ${total} lines; offset ${offset} is past the end.`);

    const end = Math.min(total, offset + limit - 1);
    const cap = Math.max(MAX_LINE_CHARS, Math.floor(LINE_BUDGET_CHARS / (end - offset + 1)));
    const numbered = lines
      .slice(offset - 1, end)
      .map((line, i) => {
        if (line.length <= cap) return numberedLine(offset + i, line);
        const whole = line.length <= LINE_BUDGET_CHARS ? "reads it whole" : `reads ${LINE_BUDGET_CHARS} of them`;
        return numberedLine(offset + i, `${line.slice(0, cap)}… (line truncated: ${line.length} characters; offset=${offset + i} and limit=1 ${whole})`);
      })
      .join("\n");

    const partial = offset > 1 || end < total;
    const range = `lines ${offset}-${end} of ${total}`;
    const more = end < total ? ` Use offset=${end + 1} to read more.` : "";
    return {
      output: partial ? `${numbered}\n\n(Showing ${range}.${more})` : numbered,
      summary: partial ? range : `${total} line${total === 1 ? "" : "s"}`,
    };
  },
};
