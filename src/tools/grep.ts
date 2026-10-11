import { join } from "node:path";
import { z } from "zod";
import { filesUnder, isDirectory, isRegularFile, requireRegularFile, looksBinary, projectPath, resolveInProject } from "./files.ts";
import { repairPathField } from "./repair.ts";
import { ToolError, type Tool } from "./types.ts";

const MAX_MATCHES = 100;
const MAX_FILE_BYTES = 1_000_000;
const MAX_LINE_CHARS = 300;

const input = z.object({
  pattern: z.string().describe("Regular expression (JavaScript syntax) to search for, e.g. \"function\\\\s+greet\"."),
  path: z.string().optional().describe("File or directory to search, relative to the project root. Default: the whole project."),
  glob: z.string().optional().describe('Only search files matching this glob, e.g. "*.ts" or "src/**/*.tsx".'),
  ignoreCase: z.boolean().optional().describe("Case-insensitive search."),
});

export const grep: Tool<typeof input> = {
  name: "grep",
  description:
    "Search file contents in the project with a regular expression. Returns matching lines as path:line: text. " +
    "Respects .gitignore. Use this to find where something is defined or used.",
  input,
  label: ({ pattern, path }) => (path ? `"${pattern}" in ${path}` : `"${pattern}"`),
  repair: (raw, { root }) => repairPathField(raw, root),

  async run({ pattern, path = ".", glob, ignoreCase }, { root, signal, gitEnv }) {
    let regex: RegExp;
    try {
      regex = new RegExp(pattern, ignoreCase ? "i" : "");
    } catch (err) {
      throw new ToolError(`Invalid regex: ${(err as Error).message}`);
    }
    const target = resolveInProject(root, path);
    const base = projectPath(root, target);
    // A glob without a slash matches the file name anywhere ("*.ts"); with one, the project path.
    const fileFilter = glob ? new Bun.Glob(glob) : null;
    const wanted = (file: string) =>
      !fileFilter || fileFilter.match(glob!.includes("/") ? file : file.slice(file.lastIndexOf("/") + 1));

    let files: string[];
    if (isDirectory(target)) {
      files = (await filesUnder(root, base, gitEnv)).filter((file) => {
        // A symlink in the project can point outside it; resolveInProject refuses those, as read_file does.
        try {
          return isRegularFile(resolveInProject(root, file));
        } catch {
          return false;
        }
      });
    } else {
      requireRegularFile(target, base);
      files = [base];
    }
    const lines: string[] = [];
    let matchCount = 0;
    const matchedFiles = new Set<string>();

    for (const file of files.filter(wanted)) {
      if (signal?.aborted) break;
      const handle = Bun.file(join(root, file));
      if (handle.size > MAX_FILE_BYTES) continue;
      const bytes = new Uint8Array(await handle.arrayBuffer());
      if (looksBinary(bytes)) continue;
      const fileLines = new TextDecoder().decode(bytes).split("\n");
      for (let i = 0; i < fileLines.length; i++) {
        if (!regex.test(fileLines[i]!)) continue;
        matchCount++;
        matchedFiles.add(file);
        if (lines.length < MAX_MATCHES) lines.push(`${file}:${i + 1}:${fileLines[i]!.slice(0, MAX_LINE_CHARS)}`);
      }
    }

    if (matchCount === 0) return { output: "No matches.", summary: "0 matches" };
    const more = matchCount > MAX_MATCHES ? `\n\n(${matchCount - MAX_MATCHES} more matches not shown; narrow the search.)` : "";
    const plural = (n: number, word: string) => `${n} ${word}${n === 1 ? "" : word.endsWith("h") ? "es" : "s"}`;
    return {
      output: lines.join("\n") + more,
      summary: `${plural(matchCount, "match")} in ${plural(matchedFiles.size, "file")}`,
    };
  },
};
