import { z } from "zod";
import { filesUnder, isDirectory, projectPath, resolveInProject } from "./files.ts";
import { repairPathField } from "./repair.ts";
import { ToolError, type Tool } from "./types.ts";

const MAX_RESULTS = 200;

const input = z.object({
  pattern: z.string().describe('Glob pattern, e.g. "**/*.ts" (all .ts files) or "src/*.tsx" (only directly in src).'),
  path: z.string().optional().describe("Directory to search in, relative to the project root. Default: the whole project."),
});

export const glob: Tool<typeof input> = {
  name: "glob",
  description:
    "Find files in the project by name pattern. Returns matching paths, sorted. " +
    "Respects .gitignore, so dependencies and build output are skipped. Use this to explore the project's structure.",
  input,
  label: ({ pattern, path }) => (path ? `${pattern} in ${path}` : pattern),
  repair: (raw, { root }) => repairPathField(raw, root),

  async run({ pattern, path = "." }, { root, gitEnv }) {
    const dir = resolveInProject(root, path);
    if (!isDirectory(dir)) throw new ToolError(`"${path}" is not a directory.`);
    const base = projectPath(root, dir);
    const matcher = new Bun.Glob(pattern);

    const matches = (await filesUnder(root, base, gitEnv)).filter((file) =>
      matcher.match(base === "." ? file : file.slice(base.length + 1)),
    );
    if (matches.length === 0) return { output: "No files match.", summary: "0 files" };

    const shown = matches.slice(0, MAX_RESULTS).join("\n");
    const more = matches.length > MAX_RESULTS ? `\n\n(${matches.length - MAX_RESULTS} more not shown; narrow the pattern.)` : "";
    return { output: shown + more, summary: `${matches.length} file${matches.length === 1 ? "" : "s"}` };
  },
};
