import { z } from "zod";
import { addMemory, findMemory, removeMemory, type MemoryScope } from "../memory.ts";
import { shortenHome } from "../paths.ts";
import { ToolError, type Tool, type ToolContext } from "./types.ts";

const input = z.object({
  action: z.enum(["add", "remove"]).describe("add a memory, or remove one that's wrong or outdated"),
  scope: z
    .enum(["personal", "project"])
    .describe("personal: about the user, applies in every project. project: about this project only."),
  text: z.string().min(1).describe("add: the memory, one short sentence. remove: text from the memory to remove (must match exactly one)."),
});
type Input = z.infer<typeof input>;

const where: Record<MemoryScope, string> = { personal: "personal, all projects", project: "this project" };

function pathFor(scope: MemoryScope, { memory }: ToolContext): string {
  if (!memory) throw new ToolError("Memory isn't available here.");
  return memory[scope];
}

export const memory: Tool<typeof input> = {
  name: "memory",
  description:
    "Save something to remember in future sessions, or remove a memory that's wrong. Use it for the user's preferences and corrections (personal) " +
    "and for project facts that aren't obvious from the code (project). Not for temporary task details or secrets. The user approves each change.",
  input,
  kind: "write",
  checkpoint: false, // ~/.marv, not the project
  label: ({ action, text }) => `${action === "add" ? "remember" : "forget"}: ${text}`,
  scope: () => ({ key: "memory", description: "memory changes" }),

  async preview({ action, scope, text }: Input, ctx) {
    const path = pathFor(scope, ctx);
    if (action === "add") return { title: `Remember (${where[scope]})`, text, note: shortenHome(path) };
    // Check the match now, so the user isn't asked to approve a removal that can't happen.
    const found = await findMemory(path, text);
    if (found.length !== 1) {
      throw new ToolError(found.length ? `${found.length} memories match "${text}"; use more of the text.` : `No ${scope} memory matches "${text}".`);
    }
    return { title: `Forget (${where[scope]})`, text: found[0]!, note: shortenHome(path) };
  },

  async run({ action, scope, text }: Input, ctx) {
    const path = pathFor(scope, ctx);
    if (action === "add") {
      const { added, error } = await addMemory(path, text);
      if (error) throw new ToolError(error);
      return added
        ? { output: `Saved to ${scope} memory: ${text}`, summary: `saved to ${scope} memory` }
        : { output: `Already in ${scope} memory.`, summary: "already remembered" };
    }
    const result = await removeMemory(path, text);
    if ("error" in result) throw new ToolError(result.error);
    return { output: `Removed from ${scope} memory: ${result.removed}`, summary: `removed from ${scope} memory` };
  },
};
