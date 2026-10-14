// Running a subagent: the same agent loop (runAgent) started again, with a
// fresh conversation, its own system prompt and its own tools. Only its final
// message goes back to the agent that started it, so the files it read and the
// steps it took never fill up the parent's context.
//
// Note: this module and src/tools/index.ts import each other (the registry
// holds the agent tool; a subagent runs the registry's tools). That's fine
// because neither uses the other's exports until a function is called.
import { runAgent } from "./agent.ts";
import { findAgent, SUBAGENT_TOOLS, type AgentType } from "./agents.ts";
import { shortenHome } from "./paths.ts";
import { subagentPrompt } from "./prompt.ts";
import { sandboxAvailable } from "./sandbox.ts";
import { runTool, tools, toolSpecs } from "./tools/index.ts";
import { ToolError, type AgentHost, type Decision, type ToolContext, type ToolResult } from "./tools/types.ts";
import { tokens } from "./usage.ts";
import { createWorktree, finishWorktree, worktreeEnv, type Worktree } from "./worktree.ts";

/** Implementing a task takes more steps than answering a question. */
export const SUBAGENT_MAX_STEPS = 50;
/** Finished steps kept per subagent for ctrl+o: labels and summaries only. */
const MAX_STEPS_KEPT = 20;

export interface SubagentInput {
  type?: string;
  description: string;
  prompt: string;
  isolation?: "worktree";
}

const STOPPED: Record<string, string> = {
  aborted: "Interrupted by the user before it finished.",
  // Its history is gone once it ends, so "continue" can't resume it (runAgent's own message says to).
  max_steps: `It hit its ${SUBAGENT_MAX_STEPS}-step limit before finishing. To go on, start a new subagent with what remains.`,
  length: "Its last reply was cut off by the model's output limit.",
  error: "It stopped on an error:",
};

/** Only the main agent has an AgentHost; a subagent (or a session without one) gets this. */
export const NO_SUBAGENTS = "Subagents aren't available here.";
export const NO_WORKTREES = "Worktrees aren't available here. Start the agent without isolation.";

export function resolveType(host: AgentHost, name = "general-purpose"): AgentType {
  const type = findAgent(host.agents, name);
  if (!type) throw new ToolError(`There's no agent type "${name}". Available: ${host.agents.map((a) => a.name).join(", ")}.`);
  return type;
}

/**
 * How a subagent's changes get approved. In a worktree with the sandbox on,
 * they run without asking: the worktree is the boundary, and merging its
 * branch is the review. Network access still asks (a worktree limits what a
 * command changes, not what it sends out). Otherwise the user is asked, with
 * the request labeled by which subagent is asking. Without an approver (a
 * session with no one to ask) there is no auto-yes either: `approve` stays
 * undefined, so a worktree subagent's changes get the same rule as the main
 * agent's: only what yolo vouches for runs, the rest is refused.
 */
export function subagentApprove(approve: ToolContext["approve"], agent: string, auto: boolean): ToolContext["approve"] {
  if (!approve) return undefined;
  return (request) => (auto && !request.network ? Promise.resolve<Decision>("yes") : approve({ ...request, agent }));
}

/**
 * The tool context a subagent's tools run with. Built from scratch rather than
 * copied from the parent's, so nothing is inherited by accident: no agentHost
 * (it can't start subagents), no memory (only the main agent changes it), no
 * callId. Yolo mode carries over: what's safe runs without asking there too. In a worktree it works in the worktree's folder (or the same
 * subfolder the parent works in), bash sees the shared .git read-only (and,
 * from a subfolder, the rest of the worktree, so git finds the repository and
 * doesn't list the other files as deleted), and Marv's own git calls are
 * pinned to the worktree.
 */
export function subagentContext(ctx: ToolContext, who: string, worktree?: Worktree): ToolContext {
  const sandboxed = (ctx.sandbox ?? true) && sandboxAvailable();
  return {
    root: worktree?.workDir ?? ctx.root,
    signal: ctx.signal,
    sandbox: ctx.sandbox,
    yolo: ctx.yolo,
    skills: ctx.skills,
    readOnly: worktree ? [...(worktree.prefix ? [worktree.dir] : []), worktree.gitDir] : undefined,
    gitEnv: worktree && worktreeEnv(worktree),
    approve: subagentApprove(ctx.approve, who, Boolean(worktree) && sandboxed),
    // Its sandboxed bash reaches the session's background processes like the main agent's (curl localhost:<port>);
    // starting one is the main agent's alone (no processes, no doors).
    forwards: ctx.forwards,
    // A shared-folder subagent edits the same files in the same turn: the turn's one checkpoint covers it. A
    // worktree's changes land on its branch, which is already the way back.
    checkpoint: worktree ? undefined : ctx.checkpoint,
  };
}

/** Marv's own lines in a report, marked so the subagent's text can't pass for them. */
const fromMarv = (text: string) => `Marv: ${text}`;

const plural = (n: number, word: string) => `${n} ${word}${n === 1 ? "" : "s"}`;

export async function runSubagent(input: SubagentInput, ctx: ToolContext): Promise<ToolResult> {
  const host = ctx.agentHost;
  if (!host) throw new ToolError(NO_SUBAGENTS);
  const type = resolveType(host, input.type);
  const who = `${type.name} · ${input.description}`;

  let worktree: Worktree | undefined;
  if (input.isolation === "worktree") {
    if (!host.worktreesDir) throw new ToolError(NO_WORKTREES);
    worktree = createWorktree({ root: ctx.root, baseDir: host.worktreesDir, description: input.description });
  }

  const where = worktree ? `worktree ${worktree.branch}` : "shared folder";
  let toolCount = 0;
  let used = 0;
  let current: string | undefined;
  const steps: string[] = [];
  const report = () => {
    if (ctx.callId) host.onProgress(ctx.callId, { line: `${where} · ${plural(toolCount, "tool")} · ${current ?? "thinking…"}`, steps: steps.slice(-MAX_STEPS_KEPT) });
  };

  let text = "";
  let reason = "error";
  let error: string | undefined;
  /** The step the user said no to ("write_file a.txt"), if any. */
  let declinedStep: string | undefined;
  let branchLine: string | undefined;
  // Everything after the worktree exists runs inside this try, so the
  // worktree is finished (its changes committed, its folder removed) however
  // the subagent ends: a reply, an error, ctrl+c, a "no", or a throw.
  try {
    // Only subagent tools (never agent or memory, whatever the type lists), skill only when there are skills to
    // load, and github only where the session offers it (its main agent has the door; the subagent only reads).
    const hasSkills = (ctx.skills?.length ?? 0) > 0;
    const available = tools.filter(
      (t) =>
        SUBAGENT_TOOLS.includes(t.name) &&
        type.tools.includes(t.name) &&
        (t.name !== "skill" || hasSkills) &&
        (t.name !== "github" || Boolean(ctx.doors?.github)),
    );
    const names = available.map((t) => t.name);
    const subCtx = subagentContext(ctx, who, worktree);
    const system = subagentPrompt({
      cwd: worktree ? shortenHome(worktree.workDir) : host.cwd,
      tools: names,
      body: type.body,
      instructions: host.instructions,
      skills: hasSkills && names.includes("skill") ? ctx.skills : undefined,
      worktree: worktree && { branch: worktree.branch, base: worktree.base },
    });

    report();
    for await (const event of runAgent({
      provider: host.providerFor(type.model),
      history: [{ role: "user", text: input.prompt }],
      system,
      tools: toolSpecs.filter((spec) => names.includes(spec.name)),
      runTool: (call) => runTool(call, subCtx, available),
      signal: ctx.signal ?? new AbortController().signal,
      maxSteps: SUBAGENT_MAX_STEPS,
    })) {
      if (ctx.callId) host.onEvent?.(ctx.callId, event);
      switch (event.type) {
        case "usage":
          used += event.usage.promptTokens + event.usage.completionTokens;
          host.onUsage(event.usage);
          break;
        case "assistant":
          text = event.text;
          break;
        case "tool_start":
          toolCount++;
          current = `${event.call.name} ${event.label}`;
          report();
          break;
        case "tool_end":
          steps.push(`${event.call.name} ${event.result.label} · ${event.result.isError ? "error" : event.result.summary}`);
          if (event.result.declined) declinedStep ??= `${event.call.name} ${event.result.label}`;
          current = undefined;
          report();
          break;
        case "error":
          error = event.message;
          break;
        case "done":
          reason = event.reason;
          // The loop's step-limit message is written for the main agent ("say continue"); STOPPED has the right one.
          if (reason === "max_steps") error = undefined;
          break;
      }
    }
  } catch (err) {
    // A provider that throws instead of sending an error event: report it like
    // one, so the parent still gets what the subagent had (and its branch).
    reason = "error";
    error = err instanceof Error ? err.message : String(err);
  } finally {
    if (worktree) {
      try {
        branchLine = fromMarv(await finishWorktree(worktree, { description: input.description, interrupted: reason !== "end" }));
      } catch (err) {
        branchLine = fromMarv(`couldn't finish the worktree (${err instanceof Error ? err.message : String(err)}); it's at ${worktree.dir}.`);
      }
    }
  }

  const stats = `${plural(toolCount, "tool")} · ${tokens(used)} tokens`;
  const stopped = reason === "end" || reason === "declined" ? "" : `[${fromMarv(STOPPED[reason] ?? `It stopped (${reason}).`)}${error ? ` ${error}` : ""}]`;
  // A "no" that came with an interrupt (Esc declines what's waiting, then
  // stops the run) is reported as the interrupt it was.
  const declined = declinedStep !== undefined && reason !== "aborted";
  // Without this line the parent reads only the subagent's last words ("I'll
  // update a.txt now.") and may later assume the change was made.
  const declinedLine = declined
    ? `[${fromMarv(`The user declined its ${declinedStep}, so it stopped there. Don't retry: wait for the user to say how to proceed.`)}]`
    : "";
  // The subagent's own text first, then Marv's lines.
  const output = [text.trim() || "(The subagent gave no report.)", stopped, declinedLine, branchLine].filter(Boolean).join("\n\n");
  if (declined) return { output, summary: `declined · ${stats}`, declined: true };
  if (stopped) return { output, summary: `stopped · ${stats}`, isError: true };
  const first = text.trim().split("\n")[0]!.slice(0, 80);
  return { output, summary: `done · ${stats}${first ? ` · "${first}"` : ""}` };
}
