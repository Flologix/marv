// The tool registry: what the model is offered, and how its calls are run.
import { isAbsolute, sep } from "node:path";
import { z } from "zod";
import { NOT_RUN } from "../agent.ts";
import type { ToolCall, ToolSpec } from "../provider/types.ts";
import { agent } from "./agent.ts";
import { bash } from "./bash.ts";
import { editFile } from "./edit-file.ts";
import { projectPath } from "./files.ts";
import { glob } from "./glob.ts";
import { grep } from "./grep.ts";
import { memory } from "./memory.ts";
import { readFile } from "./read-file.ts";
import { repairJsonArgs, withNote } from "./repair.ts";
import { skill } from "./skill.ts";
import { webFetch } from "./web-fetch.ts";
import { writeFile } from "./write-file.ts";
import { ToolError, type Tool, type ToolContext, type ToolResult } from "./types.ts";

export const tools: Tool[] = [readFile, glob, grep, webFetch, skill, editFile, writeFile, bash, memory, agent] as Tool[];

/** Whether a call may run at the same time as its neighbours (subagents). */
export const isParallelCall = (call: ToolCall) => tools.some((t) => t.name === call.name && t.parallel);

/**
 * What the model is told about each tool. Built once, so every request sends
 * byte-identical tool definitions (anything else would break the prompt cache).
 */
export const toolSpecs: ToolSpec[] = tools.map(specOf);

/** A tool as the model is told about it: name, description, and its input as JSON schema. */
export function specOf(tool: Tool): ToolSpec {
  const { $schema: _, ...parameters } = z.toJSONSchema(tool.input) as Record<string, unknown>;
  return { name: tool.name, description: tool.description, parameters };
}

/** The specs for a session: the skill tool only when there are skills to load. Same objects every time (cache). */
export function toolSpecsFor({ hasSkills }: { hasSkills: boolean }): ToolSpec[] {
  return hasSkills ? toolSpecs : toolSpecs.filter((spec) => spec.name !== "skill");
}

/**
 * Runs a call. Never throws: every failure becomes a result the model can read
 * and recover from. `available` is what this agent was offered (a subagent
 * gets fewer tools); anything else is unknown, even if the model names it.
 */
export async function runTool(call: ToolCall, ctx: ToolContext, available: Tool[] = tools): Promise<ToolResult & { label: string }> {
  const tool = available.find((t) => t.name === call.name);
  const fail = (output: string, label = call.name) => ({ output, summary: "error", isError: true, label });
  if (!tool) return fail(`Unknown tool "${call.name}". Available tools: ${available.map((t) => t.name).join(", ")}.`);

  let raw: unknown;
  let jsonNote: string | null = null;
  try {
    raw = JSON.parse(call.arguments || "{}");
  } catch {
    // A weaker model puts literal tabs or newlines inside JSON strings (issue
    // #10), which JSON.parse rejects. Escaping those is unambiguous; anything
    // else stays an error the model can recover from.
    const repaired = repairJsonArgs(call.arguments || "{}");
    if (!repaired) return fail(`The arguments for ${call.name} are not valid JSON: ${call.arguments}`);
    raw = repaired.value;
    jsonNote = repaired.note;
  }
  // A tool's own repair of what it can read with certainty (bash's timeout in
  // milliseconds, a path with a leading slash that only exists relative to the
  // project), applied before validation so the call runs as meant.
  let repairedInput: unknown = raw;
  let repairNote: string | null = null;
  try {
    const perTool = tool.repair?.(raw, ctx);
    if (perTool) {
      repairedInput = perTool.input;
      repairNote = perTool.note;
    }
  } catch {
    repairedInput = raw; // A repair must never break a call that would have run.
  }
  // A repaired call says so in its result (a "Marv:" note): the model learns
  // the right form instead of repeating the malformed one. The JSON repair and
  // the tool's own repair can both fire in one call; their notes then go out
  // together, separated by a blank line (or none, when neither fired).
  const note = [jsonNote, repairNote].filter(Boolean).join("\n\n") || null;
  // Every result from here on (a run, an approval's answer, a failure) carries
  // the note, so the model reads what was repaired however the call ended.
  const noted = (result: ToolResult & { label: string }): ToolResult & { label: string } => ({
    ...result,
    output: withNote(result.output, note),
  });
  const parsed = tool.input.safeParse(repairedInput);
  if (!parsed.success) return noted(fail(`Invalid input for ${call.name}:\n${z.prettifyError(parsed.error)}`));

  const label = shortLabel(tool.label(parsed.data), ctx.root);
  try {
    // Tools that change something need the user's go-ahead. The preview runs
    // first, so a call that can't succeed fails here instead of being approved.
    const gated = tool.needsApproval ? tool.needsApproval(parsed.data) : Boolean(tool.kind && tool.kind !== "read");
    if (gated) {
      const preview = await tool.preview!(parsed.data, ctx);
      const scope = tool.scope?.(parsed.data) ?? { key: call.name, description: call.name };
      const network = tool.usesNetwork?.(parsed.data) ? { network: true } : {};
      // Stopped while the preview was being made (a parallel subagent when the
      // user pressed Esc): don't raise a prompt for a run that's over. Summed up
      // as the interrupt it was; `declined` stops the loop like any other stop
      // (runAgent checks the abort first, so the model reads "Interrupted").
      if (ctx.signal?.aborted) {
        return noted({ output: NOT_RUN.aborted, summary: "interrupted", declined: true, label, approval: "interrupted" });
      }
      // Yolo: a call the tool vouches for runs without asking, confined (bash
      // then can't touch .git). It still went through the preview above, so
      // a call that can't succeed fails the same way.
      if (ctx.yolo && tool.autoSafe?.(parsed.data, ctx)) {
        return noted({ ...(await tool.run(parsed.data, { ...ctx, callId: call.id, confined: true })), label, approval: "auto" });
      }
      // No one to ask (a program using the SDK without an approver): refused, as an error the model can read and
      // work around, rather than a "no" that would stop the run.
      if (!ctx.approve) return noted(fail(`${call.name} needs the user's approval, and there's no one to ask.`, label));
      const decision = await ctx.approve({ tool: call.name, label, preview, scope, ...network });
      if (decision === "unavailable") return noted(fail(`${call.name} needs the user's approval, and there's no one to ask.`, label));
      // Esc at the prompt answers "no" and then aborts the run: that's an interrupt, and the model should read it
      // as one (like the calls queued behind it), not as "the user declined this, don't retry".
      if (decision === "no" && ctx.signal?.aborted) {
        return noted({ output: NOT_RUN.aborted, summary: "interrupted", declined: true, label, approval: "interrupted" });
      }
      if (decision === "no") {
        return noted({
          output: "The user declined this. Don't retry it: stop and wait for them to say how to proceed.",
          summary: "declined",
          declined: true,
          label,
          approval: "no",
        });
      }
      return noted({ ...(await tool.run(parsed.data, { ...ctx, callId: call.id })), label, approval: decision });
    }
    return noted({ ...(await tool.run(parsed.data, { ...ctx, callId: call.id })), label });
  } catch (err) {
    if (err instanceof ToolError) return noted(fail(err.message, label));
    return noted(fail(`${call.name} failed: ${(err as Error).message}`, label));
  }
}

/** Models sometimes pass absolute paths; the transcript shows them relative to the project. */
function shortLabel(label: string, root: string): string {
  return isAbsolute(label) && label.startsWith(root + sep) ? projectPath(root, label) : label;
}
