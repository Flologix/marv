import { existsSync } from "node:fs";
import { mkdir } from "node:fs/promises";
import { dirname } from "node:path";
import { z } from "zod";
import { addedLines, changeSummary, diffText, shownDiff } from "./diff.ts";
import { lineEnding, withLineEnding } from "./edit-match.ts";
import { brokenBy, syntaxNote } from "./syntax.ts";
import { isDirectory, projectPath, refuseGit, requireRegularFile, resolveInProject, touchesGit } from "./files.ts";
import { repairPathField } from "./repair.ts";
import { withFileLock } from "./file-lock.ts";
import { ToolError, type Tool, type ToolContext } from "./types.ts";

const input = z.object({
  path: z.string().describe("File to write, relative to the project root. Folders are created as needed."),
  content: z.string().describe("The complete contents of the file, or with append: true the part to add at the end."),
  append: z.boolean().optional().describe("Add content at the end of an existing file instead of replacing it. A file that doesn't exist is just created; if the file's last line has no newline, one is added first."),
});
type Input = z.infer<typeof input>;

/**
 * The file as it will be written, and the diff that shows the change. `before` is null for a file that doesn't
 * exist (an append then creates it). An append to a file whose last line has no `\n` gets one first, so the new
 * part doesn't join that line — and the result can say so.
 */
async function plan({ path, content, append }: Input, { root, confined }: ToolContext) {
  const absolute = resolveInProject(root, path);
  if (confined) refuseGit(root, absolute);
  const shown = projectPath(root, absolute);
  if (isDirectory(absolute)) throw new ToolError(`"${shown}" is a directory.`);
  const exists = existsSync(absolute);
  if (exists) requireRegularFile(absolute, shown);
  const before = exists ? await Bun.file(absolute).text() : null;
  const newLineMissing = append && before !== null && before !== "" && !before.endsWith("\n");
  // A model writes \n: rewriting a CRLF file would otherwise change every line, and its line endings with them.
  const crlf = before !== null && lineEnding(before) === "\r\n";
  let body = crlf ? withLineEnding(content, "\r\n") : content;
  if (newLineMissing) body = (crlf ? "\r\n" : "\n") + body;
  const after = before === null ? body : append ? before + body : body;
  const diff = before === null ? { lines: addedLines(body), added: 0, removed: 0 } : diffText(before, after);
  return { absolute, shown, before, body, after, diff, newLineMissing, crlf };
}

const lineCount = (text: string) => (text === "" ? 0 : text.replace(/\n$/, "").split("\n").length);

export const writeFile: Tool<typeof input> = {
  name: "write_file",
  description:
    "Create a new file, or replace a file's entire contents. With append: true, add content at the end of an existing file instead " +
    "(to write a long file in parts: write the first part, then append the rest). For changing part of a file another way, use edit_file instead. " +
    "The user may be asked to approve each change.",
  input,
  kind: "write",
  label: ({ path }) => path,
  scope: () => ({ key: "files", description: "file changes" }),
  autoSafe: ({ path }, { root }) => !touchesGit(root, path),
  repair: (raw, { root }) => repairPathField(raw, root),

  async preview(args, ctx) {
    void ctx.beforeChange?.().catch(() => {});
    const { shown, before, diff } = await plan(args, ctx);
    // All of it: the prompt shows what fits and counts the rest (a preview is never saved).
    if (before === null) return { title: `Create ${shown}`, diff: diff.lines };
    if (args.append) return { title: `Append ${shown}`, diff: diff.lines };
    return { title: `Overwrite ${shown}`, diff: diff.lines };
  },

  async run(args, ctx) {
    await ctx.beforeChange?.();
    // Esc while waiting for the baseline: it resolves on abort, but the user said stop, so nothing is written.
    if (ctx.signal?.aborted) return { output: "Interrupted before the change was written: nothing changed.", summary: "interrupted" };
    // Read and written as one step per file, so a parallel append (or edit) can't be lost.
    const { absolute, shown, before, body, after, diff, newLineMissing, crlf } = await withFileLock(ctx.root, args.path, async () => {
      const planned = await plan(args, ctx);
      await mkdir(dirname(planned.absolute), { recursive: true });
      await Bun.write(planned.absolute, planned.after);
      return planned;
    });
    const broken = await brokenBy(shown, before, after);
    const note = broken ? `\n\n${syntaxNote(shown, broken, after, before !== null)}` : "";
    const nl = newLineMissing ? "; added a newline to its last line first" : "";
    const crlfNote = crlf ? "; kept its CRLF line endings" : "";
    if (before === null) {
      const n = lineCount(body);
      return { output: `Created ${shown} (${n} lines).${note}`, summary: `created · ${n} line${n === 1 ? "" : "s"}`, diff: shownDiff(addedLines(body)) };
    }
    const what = args.append ? "Appended to" : "Wrote";
    return { output: `${what} ${shown} (${changeSummary(diff)} lines${nl}${crlfNote}).${note}`, summary: changeSummary(diff), diff: shownDiff(diff.lines) };
  },
};
