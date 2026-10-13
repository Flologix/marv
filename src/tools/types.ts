import type { z } from "zod";
import type { LoopEvent } from "../agent.ts";
import type { AgentType } from "../agents.ts";
import type { MemoryPaths } from "../memory.ts";
import type { Provider, Usage } from "../provider/types.ts";
import type { Skill } from "../skills.ts";

/** One line of a diff, for showing a change before it's approved. */
export interface DiffLine {
  kind: "add" | "del" | "ctx" | "gap";
  text: string;
  /** The line's number before the change (removed and unchanged lines). */
  oldLine?: number;
  /** The line's number after the change (added and unchanged lines). */
  newLine?: number;
}

/** A change as the transcript shows it: at most MAX_SHOWN_DIFF lines, and how many more there were. */
export interface DiffShown {
  lines: DiffLine[];
  more: number;
}

/** What the approval prompt shows before a tool changes something. */
export interface Preview {
  /** e.g. "Edit src/app.ts", "Run a command". */
  title: string;
  /** The change, as a diff. */
  diff?: DiffLine[];
  /** A shell command, shown as "$ command". */
  command?: string;
  /** Plain text, e.g. a memory to save. */
  text?: string;
  /** A small detail line, e.g. "sandboxed · no network". */
  note?: string;
  /** Shown in the warning color, e.g. "runs WITHOUT a sandbox". */
  warning?: string;
}

/** "Don't ask again this session" applies to everything with the same key. */
export interface Scope {
  key: string;
  /** For the prompt: "Yes, don't ask again for <description>". */
  description: string;
}

export interface ApprovalRequest {
  tool: string;
  label: string;
  preview: Preview;
  scope: Scope;
  /** Which subagent is asking ("implementer · Task 2"); absent for the main agent. */
  agent?: string;
  /** The action can reach the network (bash with network: true). */
  network?: boolean;
  /** Yes or no only, no "don't ask again": each one is its own decision (/mcp add). */
  once?: boolean;
}

/** yes: run it. always: run it, and everything in its scope this session. no: don't. */
/**
 * An approver's answer. "unavailable": there's no one who can approve this call (a headless run's `--approve` list
 * doesn't cover it), refused like a call with no approver at all, as an error the model can work around; "no" is a
 * person declining, which stops the run so they can say what to do instead.
 */
export type Decision = "yes" | "always" | "no" | "unavailable";

export interface ToolContext {
  /** Absolute path of the project root; tools may not reach outside it. */
  root: string;
  signal?: AbortSignal;
  /** Asks the user before a tool that changes something runs. Without it, such tools are refused. */
  approve?: (request: ApprovalRequest) => Promise<Decision>;
  /** Run bash in the bubblewrap sandbox (default true). */
  sandbox?: boolean;
  /** Yolo mode: calls whose tool says they're safe (`autoSafe`) run without asking. */
  yolo?: boolean;
  /** Set by runTool for a call that runs without asking: bash then sees the project's .git read-only. */
  confined?: boolean;
  /** Skills the `skill` tool can load. */
  skills?: Skill[];
  /** Where the memory tool reads and writes. */
  memory?: MemoryPaths;
  /** The id of the call being run (set by runTool). */
  callId?: string;
  /** Extra folders bash may read but not change: a worktree's shared .git, so git status/diff/log work. */
  readOnly?: string[];
  /** Lets the agent tool start subagents. Only the main agent has one, so subagents can't start subagents. */
  agentHost?: AgentHost;
  /** Pins where git looks for this agent's file listing (a worktree subagent: its own .git file is within its reach). */
  gitEnv?: Record<string, string>;
  /**
   * Before a file change: the preview starts it (not awaited, so it can run while the user decides), and the change
   * waits for it before writing. The session takes the typecheck's baseline there (src/diagnostics), so the
   * baseline sees the project as it was. Never rejects.
   */
  beforeChange?: () => Promise<void>;
}

export interface ToolResult {
  /** What the model sees. */
  output: string;
  /** A few words for the transcript, e.g. "42 lines". */
  summary: string;
  /**
   * What a file change did, for the transcript only: never sent to the model (runAgent appends `output`) or
   * written to trajectories (they record named fields).
   */
  diff?: DiffShown;
  /** The file a change wrote (with `diff`), relative to the project root: which project checks run after the step. */
  changed?: string;
  isError?: boolean;
  /** The user said no: the agent stops so they can say what to do instead. */
  declined?: boolean;
  /**
   * How a call that needed approval got it (for trajectories): "auto" in yolo
   * mode, otherwise the answer (a "yes" may come from a session's "don't ask
   * again", or a worktree's own rule); "interrupted" when the run was stopped before an answer counted. Absent:
   * it needed none.
   */
  approval?: "auto" | "interrupted" | Decision;
}

/**
 * A tool is a name, a description the model reads to decide when to use it,
 * a zod schema for its input (sent to the model as JSON schema, and used to
 * validate what the model sends back), and the code that runs it.
 */
export interface Tool<S extends z.ZodType = z.ZodType> {
  name: string;
  description: string;
  input: S;
  /** Short label for the transcript, e.g. the file path. */
  label(input: z.infer<S>): string;
  /** "read" (the default) runs freely; "write" and "execute" need the user's approval first (unless `needsApproval` is defined: then it alone decides). */
  kind?: "read" | "write" | "execute";
  /** Calls of this tool that come together in one reply run at the same time (subagents). */
  parallel?: boolean;
  /** Overrides `kind` for one call: whether it needs the user's approval. */
  needsApproval?(input: z.infer<S>): boolean;
  /** Whether this call can reach the network (approvals inside a worktree still ask then). */
  usesNetwork?(input: z.infer<S>): boolean;
  /** What the approval prompt shows. Throw a ToolError here for a call that can't succeed, so the user isn't asked to approve it. */
  preview?(input: z.infer<S>, ctx: ToolContext): Promise<Preview>;
  /** What "don't ask again" covers. */
  scope?(input: z.infer<S>): Scope;
  /**
   * Whether yolo mode may run this call without asking: what it can change is
   * confined to the project (never its .git, whose hooks and config git runs
   * outside the sandbox) and it can't reach the network. Absent: always ask.
   */
  autoSafe?(input: z.infer<S>, ctx: ToolContext): boolean;
  /**
   * A malformed call whose meaning is beyond doubt, fixed before the input is
   * validated (issue #10): bash's `timeout` given in milliseconds, a project
   * path written with a leading slash. Returns the input to use and what to
   * tell the model (a "Marv:" note on the result), or null to leave the call
   * as it is — anything it could still mean two ways is an error the model
   * can recover from. Never throws.
   */
  repair?(raw: unknown, ctx: ToolContext): { input: unknown; note: string | null } | null;
  run(input: z.infer<S>, ctx: ToolContext): Promise<ToolResult>;
}

/** An expected failure (bad path, missing file); its message goes to the model as-is. */
export class ToolError extends Error {}

/** A running subagent's state, for its transcript entry. */
export interface AgentProgress {
  /** "worktree marv/x-1a2b · 3 tools · read_file src/a.ts" */
  line: string;
  /** Its finished tool calls, newest last ("read_file src/a.ts · 120 lines"). */
  steps: string[];
}

/** What the agent tool needs from the session to start subagents. */
export interface AgentHost {
  agents: AgentType[];
  /** The session's provider, or a new one when an agent type names another model. */
  providerFor(model?: string): Provider;
  /** For the subagent's system prompt. */
  cwd: string;
  instructions?: string;
  /** Where worktrees go (~/.marv/worktrees/<project>); without it, isolation isn't available. */
  worktreesDir?: string;
  /** A subagent's request, for the session's tokens and cost. */
  onUsage(usage: Usage): void;
  onProgress(callId: string, progress: AgentProgress): void;
  /** Everything the subagent's loop does (its text, thinking and tool calls), for its own view. */
  onEvent?(callId: string, event: LoopEvent): void;
}
