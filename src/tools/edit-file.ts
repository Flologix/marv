import { existsSync } from "node:fs";
import { dirname } from "node:path";
import { z } from "zod";
import { changeSummary, diffText, shownDiff } from "./diff.ts";
import { applyEdit, closestLines, type Fuzzy } from "./edit-match.ts";
import { numberedLine } from "./read-file.ts";
import { brokenBy, syntaxNote } from "./syntax.ts";
import { isDirectory, projectPath, refuseGit, requireRegularFile, resolveInProject, touchesGit } from "./files.ts";
import { repairPathField } from "./repair.ts";
import { ToolError, type Tool, type ToolContext } from "./types.ts";
import { withFileLock } from "./file-lock.ts";

const input = z.object({
  path: z.string().describe("File to edit, relative to the project root."),
  old_string: z.string().min(1).describe("The exact text to replace, copied from the file including whitespace and indentation."),
  new_string: z.string().describe("What to replace it with."),
  replace_all: z.boolean().optional().describe("Replace every occurrence instead of exactly one."),
});
type Input = z.infer<typeof input>;

/** The file before and after, or a ToolError the model can act on. Used for the preview and again when applying. */
async function plan({ path, old_string, new_string, replace_all }: Input, { root, confined }: ToolContext) {
  const absolute = resolveInProject(root, path);
  if (confined) refuseGit(root, absolute);
  const shown = projectPath(root, absolute);
  if (isDirectory(absolute)) throw new ToolError(`"${shown}" is a directory.`);
  if (!existsSync(absolute)) {
    // Only where write_file would put it next to real files: a mistyped folder would become a stray copy.
    const hint = isDirectory(dirname(absolute)) ? " Use write_file to create a new file." : "";
    throw new ToolError(`File not found: ${shown}.${hint}`);
  }
  requireRegularFile(absolute, shown);
  const file = Bun.file(absolute);
  const before = await file.text();

  const result = applyEdit(before, old_string, new_string, Boolean(replace_all));
  if ("error" in result && result.error === "ambiguous") {
    throw new ToolError(
      `old_string appears ${result.count} times in ${shown}. Include more of the surrounding lines so it matches exactly once, or set replace_all to change all ${result.count}.`,
    );
  }
  if ("error" in result) throw new ToolError(notFound(shown, before, old_string));
  return { absolute, shown, before, after: result.text, count: result.count, fuzzy: result.fuzzy };
}

const MAX_CLOSEST_LINES = 30;
const MAX_CLOSEST_CHARS = 500;

/**
 * Not found: with the file's closest lines as they are now (numbered like read_file), so the model can copy the
 * real text in its next call instead of reading the file again or guessing once more.
 */
function notFound(shown: string, text: string, oldString: string): string {
  const head = `old_string was not found in ${shown}, not even ignoring differences in whitespace and indentation.`;
  const closest = closestLines(text, oldString);
  if (!closest) return `${head} It must match the file exactly. Use read_file to see the current text, then try again.`;
  const end = Math.min(closest.end, closest.start + MAX_CLOSEST_LINES - 1);
  const lines = text
    .split(/\r?\n/)
    .slice(closest.start - 1, end)
    .map((line, i) => numberedLine(closest.start + i, line.length > MAX_CLOSEST_CHARS ? `${line.slice(0, MAX_CLOSEST_CHARS)}… (line truncated)` : line));
  return (
    `${head} The closest text is lines ${closest.start}-${closest.end}, as it is now:\n${lines.join("\n")}\n` +
    "If that's the part you meant, copy old_string from these lines exactly (what follows each →). Otherwise use read_file to find it."
  );
}

const FUZZY_NOTE: Record<Fuzzy, string> = {
  "trailing-whitespace": "old_string matched once trailing whitespace was ignored.",
  indentation: "old_string matched only with different indentation; new_string was re-indented to match the file.",
};

export const editFile: Tool<typeof input> = {
  name: "edit_file",
  description:
    "Change part of a file by replacing exact text: old_string must match the file exactly once (copy it from read_file, " +
    "with its whitespace and indentation, and include enough lines to be unique), or set replace_all. " +
    "Prefer this over write_file for existing files. The user may be asked to approve each change.",
  input,
  kind: "write",
  label: ({ path }) => path,
  scope: () => ({ key: "files", description: "file changes" }),
  autoSafe: ({ path }, { root }) => !touchesGit(root, path),
  repair: (raw, { root }) => repairPathField(raw, root),

  async preview(args, ctx) {
    void ctx.beforeChange?.().catch(() => {});
    const { shown, before, after } = await plan(args, ctx);
    return { title: `Edit ${shown}`, diff: diffText(before, after).lines };
  },

  async run(args, ctx) {
    await ctx.beforeChange?.();
    // Esc while waiting for the baseline: it resolves on abort, but the user said stop, so nothing is written.
    if (ctx.signal?.aborted) return { output: "Interrupted before the change was written: nothing changed.", summary: "interrupted" };
    // Planned again: the file may have changed while the user was deciding.
    // Read and written as one step per file, so a parallel edit (or append) can't be lost.
    const { shown, before, after, count, fuzzy } = await withFileLock(ctx.root, args.path, async () => {
      const planned = await plan(args, ctx);
      await Bun.write(planned.absolute, planned.after);
      return planned;
    });
    const diff = diffText(before, after);
    const broken = await brokenBy(shown, before, after);
    return {
      output:
        `Edited ${shown}: replaced ${count} occurrence${count === 1 ? "" : "s"} (${changeSummary(diff)} lines).${fuzzy ? ` ${FUZZY_NOTE[fuzzy]}` : ""}` +
        (broken ? `\n\n${syntaxNote(shown, broken, after, true)}` : ""),
      summary: changeSummary(diff),
      diff: shownDiff(diff.lines),
      changed: shown,
    };
  },
};
