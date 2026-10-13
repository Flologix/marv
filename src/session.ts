// The headless Marv session: everything that turns the agent loop (runAgent, src/agent.ts) into Marv, with no
// UI. It holds the conversation (what the model sees), builds the system prompt and tool list once per
// conversation, waits for MCP servers, compacts when the context is nearly full, starts subagents, records
// trajectories, saves the session, and hands each turn back as a stream of events. The TUI (src/app.tsx) is one
// client of it; the SDK (src/sdk.ts) gives it to other programs.
//
// One turn at a time: send() while a turn runs throws. The history is only appended to (compaction is the one
// exception), and the system prompt and tool specs stay the same objects for a whole conversation: that's what
// keeps the provider's prompt cache hitting (see CLAUDE.md, "Prompt cache").
import { z } from "zod";
import { agentArgs, answerAllCalls, DEFAULT_MAX_STEPS, runAgent, type LoopEvent } from "./agent.ts";
import { GENERAL_PURPOSE, type AgentType } from "./agents.ts";
import { COMPACT_AT, compactedHistory, MID_TURN_NOTE, summarize } from "./compact.ts";
import { diagnosticsNote } from "./diagnostics/tsc.ts";
import { checksNote, ProjectChecks, type ProjectCheckResult } from "./checks/kind.ts";
import { combineChecks, Diagnostics, type CheckOutcome, type CheckResult, type TurnChecks } from "./diagnostics/turn.ts";
import { EventQueue } from "./event-queue.ts";
import { classifyReply } from "./feedback.ts";
import { runGit } from "./git.ts";
import {
  announcement,
  asMcpCall,
  chooseMode,
  DEFERRED_NAMES,
  DEFERRED_SPECS,
  deferredTools,
  MCP_TOOLS_SETTINGS,
  type McpMode,
  type McpToolsSetting,
  type ModeChoice,
} from "./mcp/deferred.ts";
import type { McpManager } from "./mcp/manager.ts";
import { loadMemory, type Memories, type MemoryPaths } from "./memory.ts";
import { shortenHome } from "./paths.ts";
import { systemPrompt } from "./prompt.ts";
import { isFactory, providerFactory, type ProviderFactory, type ProviderOption } from "./provider/factory.ts";
import type { ErrorHints } from "./provider/errors.ts";
import type { ModelInfo } from "./provider/models.ts";
import type { ChatTurn, Provider, ToolSpec, Usage } from "./provider/types.ts";
import { newSession, type SavedSession, type SessionStore } from "./sessions.ts";
import type { Skill } from "./skills.ts";
import { AgentRecorder, type Trajectory, type TrajectoryRecord, type TrajectoryStore } from "./trajectory.ts";
import { isParallelCall, runTool, specOf, tools as builtinTools, toolSpecsFor } from "./tools/index.ts";
import { withNote } from "./tools/repair.ts";
import { pastedScopes } from "./tools/web-fetch.ts";
import type { AgentHost, AgentProgress, ApprovalRequest, Decision, Doors, PlanHost, PlanItem, Tool } from "./tools/types.ts";
import type { Message } from "./types.ts";
import { addUsage, emptyTotals, type Totals } from "./usage.ts";

/** How a turn ended: how the agent loop ended, or "interrupted" when it was stopped before the loop began. */
export type TurnEndReason = Extract<LoopEvent, { type: "done" }>["reason"] | "interrupted";

export type CompactResult =
  | { compacted: true; summary: string; tokensBefore?: number }
  | { compacted: false; reason: "empty" | "stopped" | "failed"; error: string };

export type SessionEvent =
  /** Always first. */
  | { type: "turn_start"; turn: string }
  /** What the turn is doing between the model's replies: `running` once the message is sent, and again after each `checking`. */
  | { type: "status"; status: "waiting_for_mcp" | "compacting" | "checking" | "running" }
  /** The main agent, exactly as runAgent yields them. */
  | LoopEvent
  /** A subagent's loop event; `callId` is the agent tool call that started it. */
  | { type: "subagent"; callId: string; event: LoopEvent }
  | { type: "subagent_progress"; callId: string; progress: AgentProgress }
  /** An automatic compaction (the context was nearly full), and how it went. */
  | { type: "compaction"; result: CompactResult }
  /** The typecheck after a step's file changes (src/diagnostics): new errors, a failure, or that it's off. */
  | { type: "check"; result: CheckResult }
  /** The project's own checks (.marv/checks/, src/checks) after a step's file changes: new violations, a failure, or that they're off. */
  | { type: "checks"; result: ProjectCheckResult }
  /** Always last, exactly once, however the turn ended. */
  | { type: "turn_end"; turn: string; reason: TurnEndReason };

/** A saved session, brought back. */
export interface Resumed {
  id: string;
  updatedAt: number;
  model: string;
  /** What the user saw (the client's transcript when it gave one, otherwise the messages and replies). */
  transcript: Message[];
  totals: Totals;
  /** The plan tool's list as it was saved (#15), or null. */
  plan: PlanItem[] | null;
}

export interface SessionInit {
  /** Absolute project root: the tools can't reach outside it. */
  root: string;
  /** Shown to the model ("~/Projects/app"); default: the root, with the home folder shortened. */
  cwd?: string;
  provider: ProviderFactory | ProviderOption;
  /** For a ProviderOption: let thinking models reason first. (A factory has it built in.) */
  thinking?: boolean;
  /**
   * What a bad key or an unknown model tells the user to change (the TUI's commands, or the SDK's options). Only
   * read with a ProviderOption: the default is the SDK's advice, and the CLI's headless mode passes the TUI's.
   */
  errorHints?: ErrorHints;
  /** Marv's version, for trajectories. */
  version?: string;
  /** The project's AGENTS.md. */
  instructions?: string;
  skills?: Skill[];
  /** Agent types the agent tool can start (default: general-purpose). */
  agents?: AgentType[];
  memory?: { paths: MemoryPaths; initial: Memories };
  /** MCP servers, already starting; their tools join the built-in ones once they're ready. */
  mcp?: McpManager;
  /** close() closes the MCP servers (the session started them; the CLI closes its own). */
  ownsMcp?: boolean;
  /** Extra tools, offered (to the main agent only) after the built-in ones. Their names must be their own: the constructor throws on a clash. */
  tools?: Tool[];
  /** Replaces Marv's system prompt, or appends to it. */
  systemPrompt?: string | { append: string };
  /**
   * Asked for every call that needs a yes; without it, only what yolo vouches for runs. Parallel subagents can ask
   * at once: requests already waiting when you answer "always" for their scope are still asked (the TUI answers its
   * queued requests in that scope itself; an approver can do the same).
   */
  approve?: (request: ApprovalRequest) => Promise<Decision>;
  /**
   * Steps a turn runs before it stops (without an approver) or asks whether to keep going (with one, again at each
   * multiple). Default 25. 0 never asks and never stops the main agent: the guard is off for the session's own
   * turns, while subagents keep their own hard limit (SUBAGENT_MAX_STEPS, 50).
   */
  maxSteps?: number;
  /** Typecheck after each step that changed files (TypeScript projects, in the sandbox), and tell the model about new errors. Default true. */
  diagnostics?: boolean;
  /** The typecheck's session state; tests pass one with a stand-in runner. */
  typecheck?: Diagnostics;
  /**
   * Run the project's own checks (.marv/checks/, in the sandbox) after each step that changed files a check
   * concerns, and tell the model about new violations. Default true; nothing runs without that folder.
   */
  checks?: boolean;
  /** The project checks' session state; tests pass one with a stand-in runner. */
  projectChecks?: ProjectChecks;
  /**
   * Offer the plan tool (#15): a checklist the main agent keeps for a long task. Default true. Fixed for the
   * session's life, since it changes the tool list (prompt cache).
   */
  plan?: boolean;
  /**
   * MCP tools' specs in every request ("eager"), or only their names, in a note, with tool_search and mcp_call to
   * load and run them ("deferred", #14). Default "auto": deferred when the specs would cost more than min(4000
   * tokens, 10% of the context window). Read when a conversation decides its mode (its first turn with MCP tools).
   */
  mcpTools?: McpToolsSetting;
  sandbox?: boolean;
  /**
   * Let bash run a command outside the sandbox (`sandbox: false`, #52), asked every time, never remembered. Only
   * where someone approves each command (the TUI passes true); default false: there it's refused before any ask,
   * whatever the approver would say. The main agent only: subagents never get it.
   */
  hostCommands?: boolean;
  yolo?: boolean;
  /**
   * Yolo isn't allowed here, and why (the SDK sets it when the project is the home folder or above it, where yolo's
   * unasked edits would reach every dotfile). Constructing with yolo on, or configure({ yolo: true }), throws this.
   */
  noYolo?: string;
  sessions?: SessionStore;
  trajectories?: TrajectoryStore;
  /** Whether to log to `trajectories` (default true); configure({ trajectories }) changes it. */
  logTrajectories?: boolean;
  /** Where worktree subagents work (~/.marv/worktrees/<project>); without it, no worktrees. */
  worktreesDir?: string;
  /** What the user saw, saved with the session (the TUI's transcript); default: the messages and replies. */
  transcript?: () => Message[];
  /** Something a UI shows changed outside a turn: the model's info arrived, memory was reloaded. */
  onChange?: () => void;
  /** Something the user should hear about (a trajectory that can't be written). */
  onWarning?: (text: string) => void;
  /** Config files that couldn't be read, for the client to show. */
  problems?: string[];
}

const DEFAULT_AGENTS: AgentType[] = [GENERAL_PURPOSE];
/** Saved this long after a turn ends (and again on flush), so a burst of turns is one write. */
const SAVE_DELAY_MS = 200;
const BUSY = "Marv is working on a turn: wait for it to end, or interrupt() it, first.";

/** Resolves once the signal fires; at once if it already has. */
const stopped = (signal: AbortSignal) =>
  new Promise<void>((resolve) => (signal.aborted ? resolve() : signal.addEventListener("abort", () => resolve(), { once: true })));

/** The project's commit, so a trajectory says what code a run started from. */
function gitHead(root: string): string | undefined {
  const head = runGit(root, ["rev-parse", "HEAD"], { timeoutMs: 2000 });
  return head.ok ? head.out.trim() : undefined;
}

const PlanShape = z.array(z.object({ text: z.string(), status: z.enum(["pending", "in_progress", "done"]) }));
/** A saved session's plan, or null for none, an old file or a hand edit (the plan is dropped, the session still loads). */
function planOf(value: unknown): PlanItem[] | null {
  const parsed = PlanShape.safeParse(value);
  return parsed.success && parsed.data.length ? parsed.data : null;
}

const ListingShape = z.object({ mode: z.enum(["deferred", "eager"]), announced: z.array(z.string()) });
/** A saved session's MCP listing, or null for an old file or a hand edit (then nothing counts as announced). */
function listingOf(value: unknown): { mode: McpMode; announced: string[] } | null {
  const parsed = ListingShape.safeParse(value);
  return parsed.success ? { mode: parsed.data.mode, announced: [...parsed.data.announced] } : null;
}

/** The step-limit question: asked like any approval, so the client's "stop everything" answers it too. */
const stepLimitRequest = (steps: number, every: number): ApprovalRequest => ({
  tool: "continue",
  label: `${steps} steps`,
  preview: {
    title: `Keep going? Marv has taken ${steps} steps on this request without finishing`,
    note: `it asks again after another ${every}; no stops it here, and you can say what to do next`,
  },
  scope: { key: "continue", description: "the step limit" },
});

/**
 * Tools of the caller's own must have names of their own. runTool() runs the first tool with the call's name, so a
 * custom "bash" would be offered next to the built-in one (two specs with one name) while every call ran the
 * built-in; `mcp__` names are where MCP servers' tools go, and tool_search and mcp_call are how a deferred
 * conversation reaches them (#14). Checked before anything starts.
 */
function checkToolNames(custom: Tool[]): void {
  const seen = new Set<string>();
  for (const { name } of custom) {
    if (builtinTools.some((t) => t.name === name) || DEFERRED_NAMES.includes(name)) {
      throw new Error(`A tool named "${name}" is already built in: give yours another name.`);
    }
    if (name.startsWith("mcp__")) throw new Error(`Tool names starting with "mcp__" are for MCP servers' tools.`);
    if (seen.has(name)) throw new Error(`Two tools are named "${name}".`);
    seen.add(name);
  }
}

/** What `createSession()` returns: a conversation you drive with send(). (The class below has more; this is the public surface.) */
export interface Session {
  /** The session's id: its file name under sessions/, and what `marv -r` and `resume` take. */
  readonly id: string;
  /** A turn (or a compaction) is running. */
  readonly busy: boolean;
  /** "openrouter · x/y", for a status bar. */
  readonly providerName: string;
  /** The model's context window in tokens, when known. */
  readonly contextLength: number | undefined;
  /** What the provider's model list says about the model, when it has been looked up. */
  readonly modelInfo: ModelInfo | undefined;
  /** Memory as of the start of this conversation. */
  readonly memory: Memories | undefined;
  /** Files and servers that couldn't be read, and why. */
  readonly problems: readonly string[];
  /** The plan tool's list (#15), or null when there's none; each change also arrives as a tool_end's `result.plan`. */
  readonly plan: readonly PlanItem[] | null;
  /** The last request's tokens (how full the context is), and the whole session's (survives clear()). */
  usage(): { last?: Usage; totals: Totals; contextLength?: number };
  /** Runs one turn and yields what happens; leaving the loop early interrupts it. Throws if a turn is running, or after close(). */
  send(text: string, options?: { forModel?: string; signal?: AbortSignal }): AsyncIterable<SessionEvent>;
  /** Stops the running turn or compaction. Approvals still waiting are answered "no". */
  interrupt(): void;
  /** Summarizes the conversation now and continues from the summary. Rejects during a turn. */
  compact(focus?: string): Promise<CompactResult>;
  /** A new conversation (and session file); totals stay. Saves the previous one first, reloads memory for the new system prompt, and throws at once during a turn. */
  clear(): Promise<void>;
  /** Brings back a saved session (its id, or "latest"); null if there is none. Rejects during a turn. */
  resume(id: string | "latest"): Promise<Resumed | null>;
  /**
   * New settings; each turn reads them when it starts, so a change during a turn applies from the next one. Throws
   * only for an invalid provider (an unknown kind) or for turning yolo on where it isn't allowed (cwd your home
   * folder or above it), and then nothing changed.
   */
  configure(changes: {
    provider?: ProviderOption;
    thinking?: boolean;
    sandbox?: boolean;
    yolo?: boolean;
    trajectories?: boolean;
    diagnostics?: boolean;
    checks?: boolean;
    /** The main agent's step limit (0 never asks); subagents keep their own. */
    maxSteps?: number;
    /** How MCP tools are offered (#14); applies from the next conversation that decides (after clear()). */
    mcpTools?: McpToolsSetting;
  }): void;
  /** Feedback on the last turn, in its trajectory. */
  rate(feedback: { score: 1 | -1 | 0; note?: string; labels?: string[] }): "rated" | "off" | "nothing";
  /** Saves the conversation now (it's also saved shortly after every turn). */
  save(): Promise<void>;
  /** The pending save, and the trajectory's queued records, on disk. */
  flush(): Promise<void>;
  /** Stops what's running, waits for it to wind down, flushes, and closes the MCP servers the session started. After it, send(), compact(), clear() and resume() refuse; calling it again is fine. */
  close(): Promise<void>;
}

export class MarvSession implements Session {
  readonly problems: readonly string[];
  private option: ProviderFactory | ProviderOption;
  private thinking: boolean;
  private factory: ProviderFactory;
  private provider: Provider;
  /** What the model list says about the model, once it has answered. */
  private info: ModelInfo | undefined;
  private lookups = 0;
  /** What the model sees: user and assistant turns, tool calls and their results. */
  private conversation: ChatTurn[] = [];
  /** The file this conversation is saved to (a new one after clear(), the old one after resume()). */
  private saved: SavedSession;
  private totals: Totals = emptyTotals();
  private last: Usage | undefined;
  /**
   * A mid-turn summary already failed this turn: don't ask for another one between later steps (each would be
   * another paid request likely failing the same way). Reset when the turn ends; the next turn tries again.
   */
  private midTurnCompactFailed = false;
  /** The main agent's doors out of the sandbox: fixed for the session, since the prompt names them (cache). */
  private get doors(): Doors {
    return { host: Boolean(this.init.hostCommands) };
  }
  /** "Yes, don't ask again" scopes, shared by the main agent and its subagents. */
  private readonly always = new Set<string>();
  /** Stops what's running (a turn or a compaction); null when idle. */
  private current: AbortController | null = null;
  private running = false;
  private memories: Memories | undefined;
  /** clear() reloading memory for the next system prompt; the next turn waits for it. */
  private reloading: Promise<void> = Promise.resolve();
  private readonly specs: ToolSpec[];
  private system: string;
  private sandbox: boolean;
  private diagnostics: boolean;
  private projectChecksOn: boolean;
  /**
   * The step limit for the main agent's turns; 0 never asks (and, without an approver, never stops). Subagents
   * keep their own SUBAGENT_MAX_STEPS (50) whatever this is.
   */
  private maxSteps: number | undefined;
  private readonly typecheck: Diagnostics;
  private readonly projectChecks: ProjectChecks;
  private yolo: boolean;
  private logging: boolean;
  private log: Trajectory | null = null;
  /** The session whose "session" record this process wrote. */
  private sessionLogged: string | null = null;
  /** The latest logged turn: what rate() and the next message's tone rate. */
  private lastTurn: { id: string; session: string } | null = null;
  /** The default transcript (when the client keeps none of its own): what was asked, and the replies. */
  private messages: Message[] = [];
  private saveTimer: ReturnType<typeof setTimeout> | null = null;
  /** Writes for a conversation the session has moved on from (its last save, its trajectory): flush() waits for them. */
  private flushing: Promise<void> = Promise.resolve();
  /** The running turn's or compaction's work, until it has fully ended: close() waits for it. */
  private working: Promise<unknown> | null = null;
  /** close() was called: no new turns, compactions, clear() or resume() (idle() refuses them). */
  private closed = false;
  private mcpTools: McpToolsSetting;
  /**
   * This conversation's MCP mode and the tools it has been told about (#14); null until a turn has MCP tools to
   * decide on. Kept for the conversation (saved with it): switching modes would change the tool list.
   */
  private listing: { mode: McpMode; announced: string[] } | null = null;
  /** MCP tools whose schema this conversation has seen (tool_search, or a rejected call): their errors don't repeat it. */
  private readonly loaded = new Set<string>();
  /** The running turn's MCP specs: what a mid-turn compaction announces again. */
  private turnMcpSpecs: ToolSpec[] = [];
  /** The plan tool's list (#15): set only by its calls, saved with the conversation, re-injected after compaction. */
  private planItems: PlanItem[] | null = null;
  /** Whether this session offers the plan tool (decided at construction: it changes the tool list). */
  private readonly planOn: boolean;

  constructor(private readonly init: SessionInit) {
    checkToolNames(init.tools ?? []);
    if (init.noYolo && (init.yolo ?? true)) throw new Error(init.noYolo);
    if (init.maxSteps !== undefined && !(Number.isInteger(init.maxSteps) && init.maxSteps >= 0)) {
      throw new Error(`maxSteps must be a whole number of at least 0 (0 never asks), not ${init.maxSteps}`);
    }
    if (init.mcpTools !== undefined && !MCP_TOOLS_SETTINGS.includes(init.mcpTools)) {
      throw new Error(`mcpTools must be one of ${MCP_TOOLS_SETTINGS.join(", ")}, not ${init.mcpTools}`);
    }
    this.mcpTools = init.mcpTools ?? "auto";
    this.problems = init.problems ?? [];
    this.option = init.provider;
    this.thinking = init.thinking ?? false;
    this.factory = isFactory(init.provider) ? init.provider : providerFactory(init.provider, this.thinking, init.errorHints);
    this.provider = this.factory.make();
    this.sandbox = init.sandbox ?? true;
    this.yolo = init.yolo ?? true;
    this.diagnostics = init.diagnostics ?? true;
    this.maxSteps = init.maxSteps;
    this.projectChecksOn = init.checks ?? true;
    this.typecheck = init.typecheck ?? new Diagnostics();
    this.projectChecks = init.projectChecks ?? new ProjectChecks();
    this.logging = init.logTrajectories ?? true;
    this.memories = init.memory?.initial;
    this.planOn = init.plan ?? true;
    // Built once, so every request sends byte-identical tool definitions (the skill tool only when there are skills,
    // the plan tool unless it's off).
    this.specs = [...toolSpecsFor({ hasSkills: (init.skills?.length ?? 0) > 0, plan: this.planOn }), ...(init.tools ?? []).map(specOf)];
    this.system = this.buildSystem();
    this.saved = newSession(init.root, { provider: this.factory.id, model: this.factory.model });
    this.lookup();
  }

  get id(): string {
    return this.saved.id;
  }
  /** A turn (or a compaction) is running. */
  get busy(): boolean {
    return this.running;
  }
  /** "openrouter · x/y", for a status bar. */
  get providerName(): string {
    return this.provider.name;
  }
  /** The model's context window in tokens, when known (Ollama's num_ctx, or OpenRouter's model list). */
  get contextLength(): number | undefined {
    return this.provider.contextLength ?? this.info?.context;
  }
  get modelInfo(): ModelInfo | undefined {
    return this.info;
  }
  /** Memory as of the start of this conversation. */
  get memory(): Memories | undefined {
    return this.memories;
  }
  /** The plan tool's list, or null. */
  get plan(): readonly PlanItem[] | null {
    return this.planItems;
  }

  /** The last request's tokens (how full the context is), and the whole session's (survives clear()). */
  usage(): { last?: Usage; totals: Totals; contextLength?: number } {
    return { last: this.last, totals: this.totals, contextLength: this.contextLength };
  }

  /**
   * Runs one turn: the user's message, and everything the agent does until it stops. The turn starts at once;
   * iterate to see its events. Leaving the loop early (break) interrupts it; a turn you never read still runs to
   * the end (with no approver, only what yolo vouches for runs). `forModel` is what the model gets when it differs
   * from what the user typed (a /skill's instructions). Throws if a turn is running.
   */
  send(text: string, options: { forModel?: string; signal?: AbortSignal } = {}): AsyncIterable<SessionEvent> {
    this.idle();
    // Sites the user pasted are theirs to fetch: web_fetch reads them without asking. Only what they typed counts
    // (not a /skill's instructions in `forModel`, a file or a page), since links found there are where a planted
    // instruction would send the model.
    for (const key of pastedScopes(text)) this.always.add(key);
    this.running = true;
    const stop = new AbortController();
    this.current = stop;
    // Started here, not when the client first reads: the turn's end (in runTurn) is what frees the session, so it
    // must not depend on the client reading. Events wait in the queue until it does.
    const queue = new EventQueue<SessionEvent>();
    const work = this.runTurn(text, options.forModel ?? text, stop, options.signal, (event) => queue.push(event)).finally(() => queue.close());
    this.track(work);
    return this.read(queue, stop, work);
  }

  /** Stops the running turn or compaction (Esc). Approvals still waiting are answered "no". */
  interrupt(): void {
    this.current?.abort();
  }

  /** /compact: summarizes the conversation now, and continues from the summary. Rejects during a turn. */
  async compact(focus?: string): Promise<CompactResult> {
    this.idle();
    this.running = true;
    const stop = new AbortController();
    this.current = stop;
    try {
      return await this.track(this.summarizeInto(focus, stop.signal, this.provider, this.factory, this.info));
    } finally {
      this.current = null;
      this.running = false;
      this.scheduleSave();
    }
  }

  /** /clear: a new conversation (and session file), with the memories saved during the last one. Totals stay. */
  clear(): Promise<void> {
    this.idle();
    this.saveNow(); // the last turn's save may still be waiting: it must go to the old file, with the old conversation
    this.saved = newSession(this.init.root, { provider: this.factory.id, model: this.factory.model });
    this.conversation = [];
    this.messages = [];
    this.last = undefined;
    this.listing = null; // a new conversation decides its MCP mode again
    this.planItems = null; // a new conversation, a new plan
    this.loaded.clear();
    // Servers removed with /mcp remove kept their tools listed for the old conversation's sake; not for a new one.
    this.init.mcp?.forgetRemoved?.();
    const memory = this.init.memory;
    const reload = (async () => {
      if (memory) this.memories = await loadMemory(memory.paths).catch(() => this.memories);
      this.system = this.buildSystem();
      this.changed();
    })();
    // The caller sees a failure; the next turns only wait for the reload to be over (they'd fail on it forever).
    this.reloading = reload.catch(() => {});
    return reload;
  }

  /** Brings back a saved session (its id, or the latest here); it keeps saving to the same file. Rejects during a turn. */
  async resume(id: string | "latest"): Promise<Resumed | null> {
    this.idle();
    const store = this.init.sessions;
    if (!store) return null;
    const saved = id === "latest" ? await store.latest(this.init.root) : await store.load(this.init.root, id);
    if (!saved) return null;
    // A turn may have started while it loaded: swapping the conversation under it would mix the two.
    this.idle();
    this.saveNow(); // the last turn's save may still be waiting: it belongs to the conversation being left
    this.saved = saved;
    this.conversation = [...saved.conversation];
    this.messages = [...saved.transcript];
    this.totals = saved.totals;
    this.last = undefined;
    this.listing = listingOf(saved.mcp);
    // The model needs nothing extra: its last plan call (or the summary holding the plan) is still in the conversation.
    this.planItems = planOf(saved.plan);
    this.loaded.clear();
    return { id: saved.id, updatedAt: saved.updatedAt, model: saved.model, transcript: saved.transcript, totals: saved.totals, plan: this.planItems };
  }

  /**
   * New settings. Each turn reads them when it starts, so a change during a turn applies from the next one.
   * All or nothing: the new provider is made first, and only once that worked does anything change. An invalid
   * provider (an unknown kind) throws and leaves the session exactly as it was; had it been kept, every later
   * configure() would rebuild from it and throw too. So does turning yolo on where it isn't allowed (`noYolo`).
   */
  configure(changes: {
    provider?: ProviderFactory | ProviderOption;
    thinking?: boolean;
    sandbox?: boolean;
    yolo?: boolean;
    trajectories?: boolean;
    diagnostics?: boolean;
    checks?: boolean;
    maxSteps?: number;
    mcpTools?: McpToolsSetting;
  }): void {
    if (changes.yolo && this.init.noYolo) throw new Error(this.init.noYolo);
    if (changes.mcpTools !== undefined && !MCP_TOOLS_SETTINGS.includes(changes.mcpTools)) {
      throw new Error(`mcpTools must be one of ${MCP_TOOLS_SETTINGS.join(", ")}, not ${changes.mcpTools}`);
    }
    if (
      changes.maxSteps !== undefined &&
      !(Number.isInteger(changes.maxSteps) && changes.maxSteps >= 0)
    ) {
      throw new Error(`maxSteps must be a whole number of at least 0 (0 never asks), not ${changes.maxSteps}`);
    }
    let remade: { option: ProviderFactory | ProviderOption; thinking: boolean; factory: ProviderFactory; provider: Provider; info: ModelInfo | undefined } | undefined;
    if (changes.provider !== undefined || changes.thinking !== undefined) {
      const option = changes.provider ?? this.option;
      const thinking = changes.thinking ?? this.thinking;
      const factory = isFactory(option) ? option : providerFactory(option, thinking, this.init.errorHints); // throws for an unknown kind
      // Same model (a /think or /yolo): what the list said about it still holds, so the provider keeps its reasoning switch.
      const info = factory.id === this.factory.id && factory.model === this.factory.model ? this.info : undefined;
      const provider = factory.make(undefined, info?.reasoning ? { reasoning: info.reasoning } : undefined);
      remade = { option, thinking, factory, provider, info };
    }
    if (changes.sandbox !== undefined) this.sandbox = changes.sandbox;
    if (changes.yolo !== undefined) this.yolo = changes.yolo;
    if (changes.trajectories !== undefined) this.logging = changes.trajectories;
    if (changes.diagnostics !== undefined) {
      // Turned back on: forget what made checking give up (timeouts), so /diagnostics on really does.
      if (changes.diagnostics && !this.diagnostics) this.typecheck.reset();
      this.diagnostics = changes.diagnostics;
    }
    if (changes.checks !== undefined) {
      if (changes.checks && !this.projectChecksOn) this.projectChecks.reset();
      this.projectChecksOn = changes.checks;
    }
    if (changes.maxSteps !== undefined) this.maxSteps = changes.maxSteps;
    if (changes.mcpTools !== undefined) this.mcpTools = changes.mcpTools;
    if (!remade) return;
    ({ option: this.option, thinking: this.thinking, factory: this.factory, provider: this.provider, info: this.info } = remade);
    this.lookup();
  }

  /** /good, /bad, /label: feedback on the last turn, in its trajectory. */
  rate({ score, note, labels }: { score: 1 | -1 | 0; note?: string; labels?: string[] }): "rated" | "off" | "nothing" {
    const log = this.trajectory();
    if (!log) return "off";
    if (!this.lastTurn || this.lastTurn.session !== log.session) return "nothing";
    log.write({ type: "feedback", turn: this.lastTurn.id, score, source: "explicit", ...(labels ? { labels } : {}), ...(note ? { note } : {}) });
    return "rated";
  }

  /** Saves the conversation now (it's also saved shortly after every turn). */
  async save(): Promise<void> {
    const store = this.init.sessions;
    if (!store) return;
    const transcript = this.init.transcript?.() ?? this.messages;
    // answerAllCalls: on the way out a run may still be winding down, with tool calls not yet answered.
    this.saved = {
      ...this.saved,
      provider: this.factory.id,
      model: this.factory.model,
      conversation: answerAllCalls(this.conversation),
      transcript,
      totals: this.totals,
      ...(this.listing ? { mcp: { mode: this.listing.mode, announced: [...this.listing.announced] } } : {}),
      plan: this.planItems ?? undefined,
    };
    await store.save(this.saved);
  }

  /** Before exiting: the pending save, and the trajectory's queued records, on disk. */
  async flush(): Promise<void> {
    if (this.saveTimer) clearTimeout(this.saveTimer);
    this.saveTimer = null;
    await Promise.all([this.save().catch(() => {}), this.log?.flush(), this.flushing]);
  }

  /** Stops what's running, waits for it to wind down, flushes, and closes the MCP servers the session started. */
  async close(): Promise<void> {
    this.closed = true;
    this.interrupt();
    // A turn's end saves its conversation (and arms a save timer): waiting for it means that save is part of this
    // flush, rather than one that lands after close() returned.
    await this.working?.catch(() => {});
    await this.flush();
    if (this.init.ownsMcp) await this.init.mcp?.close();
  }

  private get cwd(): string {
    return this.init.cwd ?? shortenHome(this.init.root);
  }

  /** Remembers what's running until it settles (for close()); returns it unchanged. */
  private track<T>(work: Promise<T>): Promise<T> {
    this.working = work;
    const done = () => {
      if (this.working === work) this.working = null;
    };
    work.then(done, done);
    return work;
  }

  /**
   * Refuses new work: after close() (a turn or compaction would run with its MCP servers closed, and save after the
   * final flush; clear() and resume() would swap the conversation that was just saved), and during a turn.
   */
  private idle(): void {
    if (this.closed) throw new Error("This session is closed.");
    if (this.running) throw new Error(BUSY);
  }

  private buildSystem(): string {
    const custom = this.init.systemPrompt;
    if (typeof custom === "string") return custom;
    const base = systemPrompt({
      cwd: this.cwd,
      tools: this.specs.map((t) => t.name),
      instructions: this.init.instructions,
      skills: this.init.skills,
      memory: this.memories,
      agents: this.init.agents ?? DEFAULT_AGENTS,
      // Whenever there's a manager, even an empty one: a server added mid-conversation (/mcp add) is then covered
      // without changing the system prompt (the prompt cache).
      mcp: Boolean(this.init.mcp),
      doors: this.doors,
    });
    return custom ? `${base}\n\n${custom.append}` : base;
  }

  /**
   * What the model is offered: the built-in tools and the caller's, then the MCP servers' (fixed once they've
   * started), or in a deferred conversation tool_search and mcp_call in their place (#14).
   */
  private offered(): { specs: ToolSpec[]; tools: Tool[] } {
    const mcp = this.init.mcp;
    const tools = [...(this.planOn ? builtinTools : builtinTools.filter((t) => t.name !== "plan")), ...(this.init.tools ?? [])];
    if (!mcp) return { specs: this.specs, tools };
    // Deferred: two constant specs instead of the servers', whatever the servers do (prompt cache).
    if (this.listing?.mode === "deferred") {
      return { specs: [...this.specs, ...DEFERRED_SPECS], tools: [...tools, ...deferredTools({ tools: mcp.tools, specs: mcp.specs, loaded: this.loaded })] };
    }
    return { specs: [...this.specs, ...mcp.specs], tools: [...tools, ...mcp.tools] };
  }

  /** The first turn with MCP tools decides the conversation's mode; it's kept until clear() (switching would change the tool list). */
  private decideMcp(): ModeChoice | null {
    const mcp = this.init.mcp;
    if (this.listing || !mcp?.specs.length) return null;
    const choice = chooseMode(this.mcpTools, mcp.specs, this.contextLength);
    this.listing = { mode: choice.mode, announced: [] };
    return choice;
  }

  /** What the model hasn't been told about the MCP tools (new ones, gone ones), now marked as told; null when not deferred or nothing changed. */
  private announce(specs: ToolSpec[]): string | null {
    const listing = this.listing;
    if (listing?.mode !== "deferred") return null;
    const names = new Set(specs.map((s) => s.name));
    const note = announcement(
      specs.filter((s) => !listing.announced.includes(s.name)),
      listing.announced.filter((n) => !names.has(n)),
    );
    listing.announced = [...names];
    return note;
  }

  /** Reads the model list in the background: the context window, prices, and whether reasoning can be turned off. */
  private lookup(): void {
    const factory = this.factory;
    const ticket = ++this.lookups;
    factory.lookup?.().then(
      (info) => {
        if (ticket !== this.lookups || !info) return; // the provider changed meanwhile
        this.info = info;
        // Remade once the list says whether this model's reasoning can be turned off (/think on OpenRouter).
        if (info.reasoning) this.provider = factory.make(undefined, { reasoning: info.reasoning });
        this.changed();
      },
      () => {}, // offline: no context size or price estimates, that's all
    );
  }

  /** Tells the client something changed. Its callback failing is the client's problem, not the session's. */
  private changed(): void {
    try {
      this.init.onChange?.();
    } catch {
      // A UI that can't redraw now will on its next update.
    }
  }

  /** The caller's approver, with the session's "don't ask again" scopes, answered "no" once the run is stopped. */
  private approver(signal: AbortSignal): ((request: ApprovalRequest) => Promise<Decision>) | undefined {
    const ask = this.init.approve;
    if (!ask) return undefined;
    return async (request) => {
      // A `once` request (a door to the host, /mcp add) is its own decision every time: never answered from the
      // "always" set, nor added to it, whatever the client answers (a client could offer "always" anyway).
      if (!request.once && this.always.has(request.scope.key)) return "yes";
      if (signal.aborted) return "no";
      const decision = await Promise.race([ask(request), stopped(signal).then((): Decision => "no")]);
      if (decision === "always") {
        if (request.once) return "yes";
        this.always.add(request.scope.key);
      }
      return decision;
    };
  }

  /**
   * Adds a request's tokens and cost to the totals (the main agent's, subagents', summaries'). `factory` made the
   * provider the request went to, and `info` is what the model list said about its model when the turn started: a
   * model switched to meanwhile neither prices nor counts as local the requests of the one the turn started with.
   * Without `info` (the list hadn't answered yet), the session's current info prices it only if it's still about
   * the same model; another model's prices would make the estimate wrong, so then there's no estimate.
   */
  private count(usage: Usage, factory: ProviderFactory, info: ModelInfo | undefined): void {
    const local = Boolean(factory.local);
    const sameModel = factory.id === this.factory.id && factory.model === this.factory.model;
    const prices = info ?? (sameModel ? this.info : undefined);
    this.totals = { ...addUsage(this.totals, usage, prices), local: (this.totals.requests === 0 || Boolean(this.totals.local)) && local };
  }

  private note(role: "user" | "assistant", text: string): void {
    this.messages.push({ id: (this.messages.at(-1)?.id ?? 0) + 1, role, text });
  }

  private nearlyFull(): boolean {
    const context = this.contextLength;
    return Boolean(context && this.last && this.last.promptTokens + this.last.completionTokens >= context * COMPACT_AT);
  }

  /** Whether compaction should be tried at all now: nearly full, and no mid-turn summary has failed this turn. */
  private tryCompact(): boolean {
    return this.nearlyFull() && !this.midTurnCompactFailed;
  }

  /**
   * Whether the context is nearly full; when it is, summarizes the conversation and continues from the summary,
   * right where the turn is (also between steps: a long turn can fill the window by itself). Emits nothing: the
   * caller emits the status and the compaction event around it, so the App shows one notice per compaction.
   */
  private async compactIfFull(
    provider: Provider,
    factory: ProviderFactory,
    info: ModelInfo | undefined,
    stop: AbortController,
    /** Between two steps of a turn (not before a user message): the summary then needs a "continue" note after it. */
    midTurn: boolean,
  ): Promise<CompactResult> {
    if (!this.tryCompact()) return { compacted: false, reason: "empty", error: "The context isn't nearly full." };
    const result = await this.summarizeInto(undefined, stop.signal, provider, factory, info);
    if (result.compacted && midTurn) {
      // The compacted history ends with the model's own "Got it", so mid-turn the next request needs an
      // instruction to go on: it follows the summary here, before the loop's next request. Before a user message,
      // that message is the instruction.
      const note = this.announce(this.turnMcpSpecs);
      this.conversation.push({ role: "user", text: note ? `${MID_TURN_NOTE}\n\n${note}` : MID_TURN_NOTE });
    } else if (!result.compacted) {
      // A summary that failed isn't tried again within the turn: the next step would ask for the same expensive
      // summary that just failed. The next turn (a new user message) tries again, as before.
      this.midTurnCompactFailed = true;
    }
    return result;
  }

  /** Summarizes the conversation and continues from the summary (src/compact.ts). Only what the model sees changes. */
  private async summarizeInto(
    focus: string | undefined,
    signal: AbortSignal,
    // Passed explicitly (no defaults): a turn's `info` can be undefined, and a default would swap in the current one.
    provider: Provider,
    factory: ProviderFactory,
    info: ModelInfo | undefined,
  ): Promise<CompactResult> {
    if (this.conversation.length === 0) return { compacted: false, reason: "empty", error: "Nothing to compact yet." };
    const before = this.last;
    const result = await summarize({
      provider,
      history: this.conversation,
      system: this.system,
      tools: this.offered().specs,
      signal,
      focus,
      onUsage: (usage) => this.count(usage, factory, info),
    });
    if ("error" in result) return { compacted: false, reason: signal.aborted ? "stopped" : "failed", error: result.error };
    // In place: runAgent holds this very array for the whole turn, so a compaction between its steps must
    // rewrite it where it stands — replacing this.conversation would leave the loop on the full history.
    this.conversation.splice(0, this.conversation.length, ...compactedHistory(result.summary, this.planItems));
    // A summary may drop the list of MCP tools: the next message announces them all again (the cache starts over
    // here anyway), and schemas seen before it may be gone too.
    if (this.listing) this.listing.announced = [];
    this.loaded.clear();
    // From here the model sees the summary, not the turns before it: the trajectory needs it to say what the model saw.
    // The turn it follows only if that turn is in this file (after resume(), the last turn was in another one).
    const log = this.trajectory();
    const after = this.lastTurn && this.lastTurn.session === log?.session ? { turn: this.lastTurn.id } : {};
    log?.write({ type: "compact", ...after, summary: result.summary });
    this.last = undefined;
    return { compacted: true, summary: result.summary, ...(before ? { tokensBefore: before.promptTokens + before.completionTokens } : {}) };
  }

  /** This session's trajectory log, or null when logging is off. */
  private trajectory(): Trajectory | null {
    const store = this.init.trajectories;
    if (!store || !this.logging) return null;
    if (this.log?.session !== this.saved.id) {
      // The previous conversation's log may still have records queued: flush() must wait for those too.
      const old = this.log;
      if (old) this.flushing = Promise.all([this.flushing, old.flush().catch(() => {})]).then(() => {});
      this.log = store.open(this.init.root, this.saved.id);
      this.log.onError = (text) => this.init.onWarning?.(text);
    }
    return this.log;
  }

  /** The session's setup (once per file), what this message says about the last turn, and the new turn. */
  private startTurnLog(log: Trajectory | null, turn: string, text: string, forModel: string, specs: ToolSpec[], mcpChoice: ModeChoice | null): void {
    if (log) {
      if (this.sessionLogged !== log.session) {
        log.write({
          type: "session",
          root: this.init.root,
          marv: this.init.version ?? "",
          provider: this.factory.id,
          model: this.factory.model,
          system: this.system,
          tools: specs.map((t) => t.name),
          git: gitHead(this.init.root),
        });
        this.sessionLogged = log.session;
      }
      const previous = this.lastTurn;
      const signal = forModel === text ? classifyReply(text) : null; // a /skill's arguments aren't a reply
      if (previous?.session === log.session && signal) {
        log.write({ type: "feedback", turn: previous.id, score: signal.score, source: "implicit", phrase: signal.phrase });
      }
      log.write({
        type: "turn_start",
        turn,
        text,
        ...(forModel !== text ? { forModel } : {}),
        provider: this.factory.id,
        model: this.factory.model,
        yolo: this.yolo,
        sandbox: this.sandbox,
        diagnostics: this.diagnostics,
        checks: this.projectChecksOn,
        ...(mcpChoice ? { mcpTools: mcpChoice } : {}),
      });
    }
    // Only a logged turn can be rated: feedback for a turn the file never recorded would be an orphan.
    this.lastTurn = log ? { id: turn, session: log.session } : null;
  }

  /**
   * The waiting save, now: before the conversation is swapped (clear, resume), or the timer would save the new one
   * and the old one's last turn would never reach its file. save() reads everything before its first await, so
   * starting it here is enough; flush() waits for it to land.
   */
  private saveNow(): void {
    if (!this.saveTimer) return;
    clearTimeout(this.saveTimer);
    this.saveTimer = null;
    this.flushing = Promise.all([this.flushing, this.save().catch(() => {})]).then(() => {});
  }

  /** Saves shortly (a burst of turns is one write). When the timer fires it's saveNow(), so flush() waits for it too. */
  private scheduleSave(): void {
    if (!this.init.sessions) return;
    if (this.saveTimer) clearTimeout(this.saveTimer);
    this.saveTimer = setTimeout(() => this.saveNow(), SAVE_DELAY_MS);
  }

  /**
   * The stream send() returns: it only reads the queue the turn's work feeds. The session's bookkeeping (busy,
   * saving) is runTurn's, so it happens whether or not anyone reads.
   */
  private async *read(queue: EventQueue<SessionEvent>, stop: AbortController, work: Promise<void>): AsyncGenerator<SessionEvent> {
    try {
      yield* queue;
    } finally {
      // The client left before the end (break, or it threw): that's an interrupt. A no-op after a normal end, and
      // it's this turn's own controller, so it can't stop a newer turn the client started at turn_end.
      stop.abort();
      // Wait for the loop's cleanup, so after a break the history is valid (every tool call answered) before the
      // client goes on.
      await work;
    }
  }

  /** One turn's work. Never rejects: whatever happens ends in turn_end. */
  private async runTurn(
    text: string,
    forModel: string,
    stop: AbortController,
    signal: AbortSignal | undefined,
    push: (event: SessionEvent) => void,
  ): Promise<void> {
    const turn = `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 6)}`;
    // Nothing after turn_end: a subagent's callback arriving late (a microtask after the turn ended) is dropped.
    let ended = false;
    const emit = (event: SessionEvent) => {
      if (!ended) push(event);
    };
    // The caller's signal stops the turn like interrupt() does.
    const onAbort = () => stop.abort();
    if (signal?.aborted) stop.abort();
    else signal?.addEventListener("abort", onAbort, { once: true });
    emit({ type: "turn_start", turn });
    let reason: TurnEndReason = "error";
    let main: AgentRecorder | undefined;
    // Subagents' recorders, by call id; ids are unique within the turn.
    const subRecorders = new Map<string, AgentRecorder>();
    try {
      await this.reloading; // clear() may still be reading memory for the new system prompt
      // MCP servers still starting: their tools must be in place before the first request (the list can't change
      // after it, or the prompt cache would start over). Interrupting the wait ends the turn.
      const mcp = this.init.mcp;
      if (mcp && !mcp.settled) {
        emit({ type: "status", status: "waiting_for_mcp" });
        // A failed start still settles: the turn goes on with whatever tools there are.
        const ready = mcp.ready.then(
          () => "ready" as const,
          () => "ready" as const,
        );
        if ((await Promise.race([ready, stopped(stop.signal).then(() => "stopped" as const)])) === "stopped") {
          reason = "interrupted";
          return;
        }
      }
      // Stopped before the message was even sent (an already-aborted signal, or interrupt() during the waits above):
      // it isn't sent, so the conversation stays as it was, and nothing is logged (a turn the model never saw would
      // otherwise be in the trajectory, and the next message's tone would rate it).
      if (stop.signal.aborted) {
        reason = "interrupted";
        return;
      }
      // What this turn runs with, all read now: settings changed while it runs (configure(), or the model list
      // arriving and remaking the provider) apply from the next turn. That includes the factory, so a subagent
      // naming its own model gets it from the same provider as the turn, and its usage is counted (local or not) as
      // that one's, priced with what the model list said about the turn's model.
      // The servers have settled: the first turn with MCP tools decides whether this conversation defers them (#14).
      const mcpChoice = this.decideMcp();
      this.turnMcpSpecs = mcp?.specs ?? [];
      const { specs, tools } = this.offered();
      const { sandbox, yolo, factory, provider, info, diagnostics, projectChecksOn, maxSteps } = this;
      const log = this.trajectory();
      const record = (r: TrajectoryRecord) => log?.write(r);
      this.startTurnLog(log, turn, text, forModel, specs, mcpChoice);
      main = new AgentRecorder(record, { turn, agent: "main" });

      // Nearly out of context: summarize first, so this message (and what follows) fits.
      if (this.tryCompact()) {
        emit({ type: "status", status: "compacting" });
        const result = await this.compactIfFull(provider, factory, info, stop, false);
        emit({ type: "compaction", result });
        if (!result.compacted && result.reason === "stopped") {
          // Stopping meant "stop": the message too (it would run on the nearly full context).
          main.finish("aborted");
          reason = "interrupted";
          return;
        }
      }
      // After a compaction above (which forgets what was announced), so the note then lists everything again. It
      // goes at the start of the user's message, never as a message of its own (two user messages in a row), and
      // never into the transcript.
      const mcpNote = this.announce(this.turnMcpSpecs);
      if (mcpNote) record({ type: "mcp_note", turn, text: mcpNote });
      this.conversation.push({ role: "user", text: mcpNote ? `${mcpNote}\n\n${forModel}` : forModel });
      this.note("user", text);
      emit({ type: "status", status: "running" });

      const approve = this.approver(stop.signal);
      // The typecheck and the project's checks after file changes: a baseline before the first, a check after each
      // step that made one. Both can run at once: "checking" lasts until the last of them is done.
      let checking = 0;
      const onStatus = (on: boolean) => {
        checking += on ? 1 : -1;
        if (on && checking === 1) emit({ type: "status", status: "checking" });
        if (!on && checking === 0) {
          emit({ type: "status", status: "running" });
          main?.restartClock();
        }
      };
      const checkRecord = <E>(result: CheckOutcome<E>, extra: { source?: "checks"; note?: (added: E[]) => string }): TrajectoryRecord =>
        result.status === "done"
          ? {
              type: "check",
              turn,
              agent: "main",
              ...(extra.source ? { source: extra.source } : {}),
              status: "done",
              ms: result.ms,
              before: result.before,
              errors: result.errors,
              added: result.added.length,
              ...(result.added.length && extra.note ? { note: extra.note(result.added) } : {}),
            }
          : result.status === "failed"
            ? { type: "check", turn, agent: "main", ...(extra.source ? { source: extra.source } : {}), status: "failed", ms: result.ms, reason: result.reason }
            : { type: "check", turn, agent: "main", ...(extra.source ? { source: extra.source } : {}), status: "off", reason: result.reason };
      const parts: TurnChecks[] = [];
      if (diagnostics) {
        parts.push(
          this.typecheck.turn({
            root: this.init.root,
            sandbox,
            signal: stop.signal,
            onStatus,
            onResult: (result) => {
              emit({ type: "check", result });
              record(checkRecord(result, { note: diagnosticsNote }));
            },
          }),
        );
      }
      if (projectChecksOn) {
        parts.push(
          this.projectChecks.turn({
            root: this.init.root,
            sandbox,
            signal: stop.signal,
            onStatus,
            onResult: (result) => {
              emit({ type: "checks", result });
              record(checkRecord(result, { source: "checks", note: checksNote }));
            },
          }),
        );
      }
      const checks = parts.length ? combineChecks(parts) : undefined;
      let subagents = 0;
      // What the agent tool needs to start subagents. Only the main agent gets one; what a subagent does reaches
      // this conversation only as its tool result, and the client through "subagent" events.
      // Where the plan tool writes; only the main agent gets it (as it does agentHost), so subagents have no plan.
      const plan: PlanHost | undefined = this.planOn
        ? {
            set: (items) => {
              this.planItems = items.length ? items : null;
            },
          }
        : undefined;
      const agentHost: AgentHost = {
        agents: this.init.agents ?? DEFAULT_AGENTS,
        cwd: this.cwd,
        instructions: this.init.instructions,
        worktreesDir: this.init.worktreesDir,
        providerFor: (model) => (model ? factory.make(model) : provider),
        onUsage: (usage) => this.count(usage, factory, info),
        onProgress: (callId, progress) => emit({ type: "subagent_progress", callId, progress }),
        onEvent: (callId, event) => {
          subRecorders.get(callId)?.event(event);
          emit({ type: "subagent", callId, event });
        },
      };

      // A direct call to a deferred MCP tool runs as the mcp_call it meant (#14), and its result says so.
      const mcpNames = new Set(this.listing?.mode === "deferred" ? this.turnMcpSpecs.map((s) => s.name) : []);
      const rewritten = new Map<string, string>(); // call id → the repair's note

      for await (const event of runAgent({
        provider,
        history: this.conversation,
        system: this.system,
        tools: specs,
        rewriteCall: mcpNames.size
          ? (call) => {
              const fixed = asMcpCall(call, mcpNames);
              if (fixed) rewritten.set(call.id, fixed.note);
              return fixed?.call ?? null;
            }
          : undefined,
        runTool: async (call) => {
          const result = await runTool(
            call,
            { root: this.init.root, signal: stop.signal, approve, sandbox, yolo, skills: this.init.skills, memory: this.init.memory?.paths, agentHost, plan, beforeChange: checks?.beforeChange, doors: this.doors },
            tools,
          );
          const note = rewritten.get(call.id);
          return note ? { ...result, output: withNote(result.output, note) } : result;
        },
        signal: stop.signal,
        isParallel: isParallelCall,
        maxSteps,
        afterStep: checks?.afterStep,
        // A long turn can fill the window by itself: check between steps, and compact there too. The events say
        // when ("Compacting the conversation…") and how it went, and the loop goes on from the summary.
        onFull: async () => {
          if (!this.tryCompact()) return false;
          emit({ type: "status", status: "compacting" });
          const result = await this.compactIfFull(provider, factory, info, stop, true);
          emit({ type: "compaction", result });
          if (result.compacted) return true;
          // Esc during the summary meant "stop the turn", not "keep going on the old context".
          if (result.reason === "stopped") stop.abort();
          return false;
        },
        // At the step limit, ask instead of stopping dead; with no one to ask, it stops there.
        // maxSteps 0 (the guard off) makes onLimit undefined, so there is nothing to ask.
        onLimit:
          approve && maxSteps !== 0
            ? async (steps) => {
                // Only a yes goes on: "unavailable" (marv -p --approve answers that for anything outside its list)
                // means no one could say so, and stops like a hard limit.
                const answer = await approve(stepLimitRequest(steps, maxSteps ?? DEFAULT_MAX_STEPS));
                return answer === "yes" || answer === "always" ? true : answer === "unavailable" ? "unavailable" : false;
              }
            : undefined,
      })) {
        main.event(event);
        switch (event.type) {
          case "usage":
            this.last = event.usage;
            this.count(event.usage, factory, info);
            break;
          case "assistant":
            this.note("assistant", event.text);
            break;
          case "tool_start":
            if (event.call.name === "agent") {
              const args = agentArgs(event.call.arguments);
              const subagent = `${turn}.${++subagents}`;
              record({
                type: "subagent_start",
                turn,
                agent: "main",
                subagent,
                call: event.call.id,
                agentType: args.type,
                description: args.description,
                prompt: args.prompt,
                isolation: args.isolation,
              });
              subRecorders.set(event.call.id, new AgentRecorder(record, { turn, agent: subagent }));
            }
            break;
          case "tool_end":
            // A subagent that failed before its loop started never sent done.
            subRecorders.get(event.call.id)?.finish(event.result.isError ? "error" : "end");
            subRecorders.delete(event.call.id);
            break;
          case "done":
            reason = event.reason;
            break;
        }
        emit(event);
      }
    } catch (err) {
      // A provider (or a tool's own code) that throws instead of reporting an error: the turn ends, the session lives on.
      emit({ type: "error", message: `Error: ${err instanceof Error ? err.message : String(err)}` });
      reason = "error";
    } finally {
      this.midTurnCompactFailed = false; // the next turn tries compaction again
      for (const sub of subRecorders.values()) sub.finish("interrupted");
      main?.finish("error"); // only if the loop threw: otherwise its done already ended the turn
      // Stop anything still running (a no-op after a normal end): subagents still running in a parallel group
      // would otherwise carry on unseen, and could still ask for approvals.
      stop.abort();
      signal?.removeEventListener("abort", onAbort);
      // The session is free before turn_end goes out, so a client can send the next message as soon as it sees it,
      // and a turn nobody reads frees it too. (A newer turn can't have started yet: send() refuses while running.)
      if (this.current === stop) this.current = null;
      this.running = false;
      this.scheduleSave();
      emit({ type: "turn_end", turn, reason });
      ended = true;
    }
  }
}
