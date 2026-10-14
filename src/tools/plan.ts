// The plan tool (#15): a checklist the model keeps for a long task. Each call sends the whole list, so the latest
// call in the history *is* the plan: nothing to patch, no ids to carry, and it survives in the conversation without
// notes of Marv's own (compaction re-injects it, src/compact.ts). The list is the session's state (ToolContext.plan);
// the TUI pins it above the prompt.
import { z } from "zod";
import { ToolError, type PlanItem, type Tool } from "./types.ts";

/** At most this many items: a plan, not a backlog (and at most ~1k tokens to resend). */
export const MAX_PLAN_ITEMS = 20;

const input = z.object({
  items: z
    .array(
      z.object({
        text: z.string().min(1).max(200).describe("one step, a short sentence"),
        status: z.enum(["pending", "in_progress", "done"]),
      }),
    )
    .max(MAX_PLAN_ITEMS)
    .refine((items) => items.filter((i) => i.status === "in_progress").length <= 1, {
      message: "At most one item can be in_progress: finish it (done) before starting the next.",
    })
    .describe("the whole plan, in order; an empty list clears it"),
});
type Input = z.infer<typeof input>;

/** "2/5 done": how far a plan is. */
export const planProgress = (items: PlanItem[]) => `${items.filter((i) => i.status === "done").length}/${items.length} done`;

/** A plan with something left to do (an all-done or empty plan has nothing to keep or show). */
export const planOpen = (items: PlanItem[] | null | undefined): items is PlanItem[] =>
  Boolean(items?.some((i) => i.status !== "done"));

export const plan: Tool<typeof input> = {
  name: "plan",
  description:
    "Keep a checklist for a task of three or more steps, or when the user asks for several things. Write the plan before starting; " +
    "mark an item in_progress before working on it and done as soon as it is; add items you discover and remove ones that turn out unnecessary. " +
    "Send the whole list every time (it replaces the previous one). Skip it for a one-step task or a question.",
  input,
  // No kind: it changes nothing outside the session, like a read tool, so it never asks.
  label: () => "Plan",

  async run({ items }: Input, ctx) {
    if (!ctx.plan) throw new ToolError("The plan tool isn't available here.");
    ctx.plan.set(items);
    if (!items.length) return { output: "Plan cleared.", summary: "cleared", plan: [] };
    const done = items.filter((i) => i.status === "done").length;
    const now = items.find((i) => i.status === "in_progress");
    return {
      output: `Plan saved: ${done} of ${items.length} done${now ? `; now: ${now.text.replace(/\.$/, "")}` : ""}.`,
      summary: planProgress(items),
      plan: items,
    };
  },
};
