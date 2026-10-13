// The public entry point: `import { createSession } from "@flologixai/marv/sdk"`.
//
// A session is Marv without its terminal: the agent loop, the tools, subagents, MCP servers, compaction, saved
// sessions and trajectories, driven by your code. createSession() reads nothing from disk unless asked
// (`sources`), and without an `approve` callback only what yolo mode vouches for runs (reading, edits outside .git,
// sandboxed commands without network); anything else goes back to the model as refused. Bun only, for now.
import "./bun-guard.ts"; // first: see the file
import { statSync } from "node:fs";
import { join, resolve } from "node:path";
import pkg from "../package.json";
import { defaultConfigDir } from "./config/config.ts";
import { parseMcpServers, type McpServerConfig } from "./mcp/config.ts";
import { McpManager } from "./mcp/manager.ts";
import { McpTokenStore } from "./mcp/oauth-store.ts";
import { McpTrust } from "./mcp/trust.ts";
import { homeOrAbove, projectKey } from "./paths.ts";
import type { ProviderOption } from "./provider/factory.ts";
import { MarvSession, type Session } from "./session.ts";
import { SessionStore } from "./sessions.ts";
import { loadSources, type Source } from "./sources.ts";
import { TrajectoryStore } from "./trajectory.ts";
import type { ApprovalRequest, Decision, Tool } from "./tools/types.ts";
import type { Message } from "./types.ts";

/** One MCP server in .mcp.json's format (validated when the session is created; a bad entry is reported in `problems`). */
export type McpServerEntry =
  | { type?: "stdio"; command: string; args?: string[]; env?: Record<string, string>; timeout?: number }
  | { type: "http" | "streamable-http"; url: string; headers?: Record<string, string>; timeout?: number };

export interface SessionOptions {
  /**
   * The project folder: the tools can't reach outside it. Your home folder, a folder above it, or `/` is refused
   * unless you pass `yolo: false`: yolo's edits run before any approver is asked, so they'd change your dotfiles
   * (~/.bashrc, ~/.ssh/config) unasked. With `yolo: false` every change goes to `approve` (or is refused without
   * it), and configure() can't turn yolo back on there.
   */
  cwd: string;
  /** { kind: "openrouter", apiKey, model? }, { kind: "ollama", model, host?, contextLength? }, or a Provider of your own. */
  provider: ProviderOption;
  /**
   * What to read from disk: "user" (~/.marv: your skills, agents, memory, MCP servers) and "project" (AGENTS.md,
   * .marv/, .mcp.json; its MCP servers still need trusting). Default: nothing. If `cwd` is your home folder,
   * "project" also finds ~/.marv's skills and agents, as the CLI does.
   */
  sources?: Source[];
  /** Replaces Marv's system prompt, or adds to it. */
  systemPrompt?: string | { append: string };
  /**
   * Your own tools, offered to the main agent next to the built-in ones (subagents don't get them). `input` must be a
   * zod 4 schema: it's described to the model with z.toJSONSchema. Names must be their own: one of a built-in tool
   * (tool_search and mcp_call included), one used twice, or one starting with `mcp__` (MCP servers' tools) makes
   * createSession throw.
   */
  tools?: Tool[];
  /**
   * MCP servers in .mcp.json's format: { name: { command, args?, env? } | { type: "http", url, headers? } }.
   * Servers given here are trusted like your own (no trust prompt) and start in your home folder, like the personal
   * servers in ~/.marv/mcp.json, so a relative path like `./server.js` won't resolve against the project: use an
   * absolute path or `${MARV_PROJECT_DIR}`. Never pass config read from a repository you don't control: use
   * `sources: ["project"]`, whose servers still need trusting.
   */
  mcpServers?: Record<string, McpServerEntry>;
  /**
   * Asked for every call that needs a yes. Without it, only what runs without asking does: read-only tools, what
   * yolo mode vouches for (edits outside .git, bash in the sandbox without network: Linux with bubblewrap only, so
   * elsewhere every command needs an approver), subagents (in the shared folder, or in a worktree when sandboxed;
   * either way under these same rules), and MCP tools their server says are read-only. Anything else goes back to
   * the model as refused.
   */
  approve?: (request: ApprovalRequest) => Promise<Decision>;
  /**
   * Steps a turn runs before it stops (without `approve`) or asks `approve` whether to keep going (again at each
   * multiple). Default 25. Unattended runs that should finish long tasks raise it; 0 never asks and never stops
   * the main agent (subagents keep their own limit).
   */
  maxSteps?: number;
  /**
   * After a step that changed files in a TypeScript project (a tsconfig.json and node_modules/.bin/tsc), run the
   * project's typecheck in the sandbox, read-only, and tell the model about new errors. Default true. Without the
   * sandbox it doesn't run (the compiler is the project's own code).
   */
  diagnostics?: boolean;
  /**
   * After a step that changed files, run the project's own checks (one file per rule in `.marv/checks/`: a pattern
   * that must or must not appear, or a bun script) whose glob matches a changed file, in the sandbox, read-only, and
   * tell the model about new violations with their why. Default true; nothing runs without that folder.
   */
  checks?: boolean;
  /**
   * Before a turn's first change, snapshot the project (git commits under a hidden ref, refs/marv/checkpoints/<session>,
   * or a git dir of Marv's own in configDir outside a repository), and again when it ends, so `session.undoPlan()`
   * and `session.undo(plan)` can put that turn's files back. Default true.
   */
  checkpoints?: boolean;
  /**
   * MCP tools' specs in every request ("eager"), or only their names, in a note, with tool_search and mcp_call to
   * load and run them ("deferred"). Default "auto": deferred when the specs would cost more than min(4000 tokens,
   * 10% of the context window). Decided once per conversation, at its first turn with MCP tools.
   */
  mcpTools?: "auto" | "deferred" | "eager";
  /** Run bash in the bubblewrap sandbox (default true). */
  sandbox?: boolean;
  /** Run what the sandbox confines without asking (default true). Must be false when `cwd` is your home folder or above it. */
  yolo?: boolean;
  /** Let thinking models reason before they answer (default false). */
  thinking?: boolean;
  /** Save the conversation (true: in ~/.marv/sessions, where `marv -r` finds it). Default false. */
  persist?: boolean | SessionStore;
  /** Log every turn for later analysis (true: in ~/.marv/trajectories). Default false. */
  trajectories?: boolean | TrajectoryStore;
  /** What the user saw, saved with the session; default: the messages and replies. */
  transcript?: () => Message[];
  /**
   * Continue a saved session (needs `persist`): its id, or "latest" (with nothing saved yet, a new session starts).
   * An unknown id throws. Call `session.resume()` yourself if you need what was resumed (its transcript).
   */
  resume?: string | "latest";
  /**
   * Where Marv keeps its files (default ~/.marv, or $MARV_CONFIG_DIR): memory, mcp.json, sessions, trajectories,
   * worktrees and MCP trust. Personal skills and agents are still read from ~/.marv in your home folder (`sources:
   * ["user"]`), as the CLI does. A project's MCP servers can only be trusted through the CLI's /mcp trust, which
   * writes this folder's mcp-trust.json.
   */
  configDir?: string;
  /** Something you should hear about that isn't a turn's event (a trajectory that can't be written). */
  onWarning?: (text: string) => void;
}

/** true → the default store; a store → that one; false or absent → none. */
function storeFor<T>(option: boolean | T | undefined, make: () => T): T | undefined {
  if (option === true) return make();
  return option || undefined;
}

export async function createSession(options: SessionOptions): Promise<Session> {
  const root = resolve(options.cwd);
  let isFolder = false;
  try {
    isFolder = statSync(root).isDirectory();
  } catch {}
  if (!isFolder) throw new Error(`cwd ${root} isn't a folder`);
  // Yolo-safe edits run before any approver is asked (that's what yolo means), so in the home folder (or above it)
  // yolo would change every dotfile unasked, approver or not. There, yolo must be off, and stay off (noYolo).
  const where = root === "/" ? "the root of the file system" : "your home folder (or above it)";
  const noYolo = homeOrAbove(root)
    ? `cwd ${root} is ${where}: with yolo on, Marv would change files there without asking, dotfiles included. Use a project folder, or pass yolo: false (then every change goes to your approver, or is refused without one).`
    : undefined;
  if (noYolo && (options.yolo ?? true)) throw new Error(noYolo);
  if (options.resume && !options.persist) throw new Error("resume needs persist (there's nowhere to resume from)");
  const configDir = options.configDir ?? defaultConfigDir(process.env);
  const loaded = await loadSources({ root, sources: options.sources ?? [], configDir });
  const given = options.mcpServers ? parseMcpServers(options.mcpServers, { ...process.env, MARV_PROJECT_DIR: root }) : { servers: [], problems: [] };
  const problems = [...loaded.problems, ...given.problems];
  // Servers given in code first; one from the config files with the same name is skipped.
  const servers: McpServerConfig[] = [...given.servers];
  for (const server of loaded.mcpServers) {
    if (servers.some((s) => s.name === server.name)) problems.push(`MCP server "${server.name}" from the config files was ignored: mcpServers has one with that name.`);
    else servers.push(server);
  }
  // Creating the manager spawns nothing; start() does. So it only starts once the session exists (its constructor
  // can throw: an unknown provider kind, a tool name that's taken), and anything that fails after that closes the
  // session, servers included.
  // Logins made with /mcp login in the TUI (same configDir) are used and refreshed; a library never starts one.
  const tokensFile = join(configDir, "mcp-tokens.json");
  const mcp = servers.length
    ? new McpManager(servers, { root, version: pkg.version, trust: new McpTrust(join(configDir, "mcp-trust.json")), logins: new McpTokenStore(tokensFile) })
    : undefined;

  const session = new MarvSession({
    root,
    provider: options.provider,
    thinking: options.thinking,
    version: pkg.version,
    instructions: loaded.instructions,
    skills: loaded.skills,
    agents: loaded.agents,
    memory: loaded.memory,
    mcp,
    ownsMcp: true,
    tools: options.tools,
    systemPrompt: options.systemPrompt,
    approve: options.approve,
    maxSteps: options.maxSteps,
    diagnostics: options.diagnostics,
    checks: options.checks,
    checkpoints: options.checkpoints,
    checkpointsDir: join(configDir, "checkpoints", projectKey(root)),
    mcpTools: options.mcpTools,
    sandbox: options.sandbox,
    yolo: options.yolo,
    noYolo,
    sessions: storeFor(options.persist, () => new SessionStore(join(configDir, "sessions"))),
    trajectories: storeFor(options.trajectories, () => new TrajectoryStore(join(configDir, "trajectories"))),
    worktreesDir: join(configDir, "worktrees", projectKey(root)),
    transcript: options.transcript,
    onWarning: options.onWarning,
    problems,
  });
  // A server that needs a login can't get one here (no prompt to paste into, no browser to open): say how, once per
  // server, whether it needed one at startup or its login ran out mid-session (a refused refresh flips it then).
  if (mcp) {
    const warned = new Set<string>();
    mcp.onChange = () => {
      for (const s of mcp.status()) {
        if (s.state !== "needs-login") {
          // Connected again (another process logged in): a later expiry is worth saying again.
          if (s.state === "connected") warned.delete(s.name);
          continue;
        }
        if (warned.has(s.name)) continue;
        warned.add(s.name);
        try {
          options.onWarning?.(`MCP server "${s.name}" needs a login: run marv in a terminal and /mcp login ${s.name}; logins are stored in ${tokensFile}`);
        } catch {} // the caller's handler must not break the manager's state change
      }
    };
    void mcp.start();
  }
  if (options.resume) {
    try {
      const resumed = await session.resume(options.resume);
      if (!resumed && options.resume !== "latest") throw new Error(`There's no saved session "${options.resume}" for ${root}.`);
    } catch (err) {
      await session.close();
      throw err;
    }
  }
  return session;
}

export type { Session, SessionEvent, TurnEndReason, CompactResult, Resumed } from "./session.ts";
export type { ProviderOption } from "./provider/factory.ts";
export type { Source } from "./sources.ts";
export { ToolError } from "./tools/types.ts";
export type { AgentHost, AgentProgress, Tool, ToolContext, ToolResult, ApprovalRequest, Decision, Preview, Scope, DiffLine, DiffShown } from "./tools/types.ts";
export type { Provider, AgentEvent, ChatTurn, StreamOptions, ToolCall, ToolSpec, Usage } from "./provider/types.ts";
export { OllamaProvider } from "./provider/ollama.ts";
export { OpenAICompatProvider } from "./provider/openai-compat.ts";
export type { LoopEvent } from "./agent.ts";
export type { CheckResult } from "./diagnostics/turn.ts";
export type { TsError } from "./diagnostics/tsc.ts";
export type { ProjectCheckResult } from "./checks/kind.ts";
export type { UndoFile, UndoPlan, UndoResult } from "./checkpoints/undo.ts";
export type { Violation } from "./checks/load.ts";
export type { Totals } from "./usage.ts";
export type { Message } from "./types.ts";
export type { ModelInfo } from "./provider/models.ts";
export type { Memories } from "./memory.ts";
export { SessionStore } from "./sessions.ts";
export { TrajectoryStore } from "./trajectory.ts";
