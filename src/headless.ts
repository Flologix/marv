// Headless mode: `marv -p "<prompt>"` — one non-interactive turn in the current folder.
//
// Built on the SDK (createSession), it loads what the TUI loads (user + project sources),
// runs one turn with no UI, and writes the final reply to stdout.
//
// Exit codes:
//   0 — finished (the model answered normally)
//   1 — an error (bad config, provider failure, invalid --output, unknown --approve)
//   2 — stopped: max steps, output limit, or loop (no final answer)
//   3 — approval needed but couldn't get it (refused or declined/interrupted)
import { ConfigError, ConfigStore, defaultConfigDir, migrateLegacyConfig, resolveConfig } from "./config/config.ts";
import { createSession, type SessionEvent, type ApprovalRequest, type Decision, type ProviderOption } from "./sdk.ts";
import { TrajectoryStore } from "./trajectory.ts";

export type OutputFormat = "text" | "json" | "stream-json";

/** Known --approve names and the scope-key prefix(es) they match. */
const APPROVE_PREFIXES: Record<string, string[]> = {
  git_write: ["bash:git:", "bash:net:git:"],
  network: ["bash:net:"],
  web: ["web:"],
  mcp: ["mcp:"],
  files: ["files"],
  memory: ["memory"],
  agent_worktree: ["agent:worktree"],
};

const ALL_APPROVE_NAMES = Object.keys(APPROVE_PREFIXES);

export interface HeadlessResult {
  /** The model's final reply text. */
  reply: string;
  /** How the turn ended (one of the TurnEndReason values). */
  reason: string;
  /** How many steps the model took. */
  steps: number;
  /** Token counts and cost for the whole turn (not just the last request). */
  usage?: { promptTokens: number; cachedTokens: number; completionTokens: number; cost: number };
  /** Calls that were refused because no one could approve them. */
  refused?: { tool: string; label: string }[];
}

export interface HeadlessOptions {
  prompt: string;
  /** Output format. Default "text". */
  output?: OutputFormat;
  /** Continue a saved session (its id, or "latest"). */
  resume?: string | "latest";
  /** Override the model. */
  model?: string;
  /** Override the max steps. */
  maxSteps?: number;
  /** Scope names to auto-approve (e.g. "git_write", "network"). */
  approveScopes?: string[];
  /** Config directory (default ~/.marv or $MARV_CONFIG_DIR). */
  configDir?: string;
}

/**
 * Build an approver from the --approve list. Returns [fn, problems]: `problems`
 * lists any unknown scope names; the approver answers "yes" for matching scopes,
 * "unavailable" for everything else (refused as an error the model reads, not a decline that stops the turn).
 */
export function buildApprover(
  names: string[],
): [{ (request: ApprovalRequest): Promise<Decision> } | undefined, string[]] {
  if (!names.length) return [undefined, []];
  const problems: string[] = [];
  const known = new Set(names.filter((n) => APPROVE_PREFIXES[n]));
  // Anything else is an exact scope key: it approves that key and nothing more (never a prefix of others).
  const exact = new Set(names.filter((n) => !APPROVE_PREFIXES[n]));
  for (const name of exact) problems.push(`Unknown --approve scope "${name}". Known: ${ALL_APPROVE_NAMES.join(", ")}`);
  // Prefixes for the names that aren't about bash's flags (web:, mcp:, files, memory, agent:worktree).
  const prefixes = [...known].filter((n) => n !== "git_write" && n !== "network").flatMap((n) => APPROVE_PREFIXES[n]!);
  // Answer "yes" (not "always") so each call is asked individually, even for the same tool. Anything outside the
  // list is "unavailable", not "no": refused as an error the model can work around, as with no --approve at all,
  // rather than a decline that ends the turn.
  const approve = async (request: ApprovalRequest): Promise<Decision> => {
    const key = request.scope.key;
    if (exact.has(key)) return "yes" as const;
    // A bash call needs every door its flags open (bash:[net:][git:]<command>): git_write alone doesn't approve a
    // command that also has network (curl … | sh with git_write: true), nor network alone a git write.
    const flags = /^bash:(net:)?(git:)?/.exec(key);
    if (flags) {
      const needs = [...(flags[1] ? ["network"] : []), ...(flags[2] ? ["git_write"] : [])];
      return needs.length > 0 && needs.every((n) => known.has(n)) ? ("yes" as const) : ("unavailable" as const);
    }
    if (prefixes.some((prefix) => key === prefix || key.startsWith(prefix))) return "yes" as const;
    return "unavailable" as const;
  };
  return [approve, problems];
}

/**
 * Runs one headless turn and writes the result to stdout.
 * Returns the exit code for process.exit().
 */
export async function runHeadless(options: HeadlessOptions): Promise<number> {
  const configDir = options.configDir ?? defaultConfigDir(process.env);
  const root = process.cwd();

  // Validate output format early.
  if (options.output && !["text", "json", "stream-json"].includes(options.output)) {
    console.error(`--output must be one of: text, json, stream-json. Got: "${options.output}"`);
    return 1;
  }

  // Validate --approve names early.
  if (options.approveScopes?.length) {
    for (const name of options.approveScopes) {
      if (!APPROVE_PREFIXES[name] && !ALL_APPROVE_NAMES.includes(name)) {
        console.error(`Unknown --approve scope "${name}". Known: ${ALL_APPROVE_NAMES.join(", ")}`);
        return 1;
      }
    }
  }

  // Load config the same way the TUI does: file + env overrides.
  if (!process.env.MARV_CONFIG_DIR) await migrateLegacyConfig();
  const store = new ConfigStore(configDir);
  let file: Record<string, unknown> | null;
  try {
    file = await store.load();
  } catch (err) {
    if (!(err instanceof ConfigError)) throw err;
    console.error(`${err.message}\n\nFix the file, or delete it to run setup again.`);
    return 1;
  }

  const env = { ...process.env };
  if (options.model) env.MARV_MODEL = options.model;
  const config = resolveConfig(file, env);

  // Approvals: with no one to ask, yolo-safe calls run, everything else is refused.
  const [approve] = buildApprover(options.approveScopes ?? []);

  // Step limit: option beats env/file.
  const maxSteps = options.maxSteps ?? config.maxSteps;

  // Trajectories store: respect the config.trajectories setting.
  const trajectories: boolean | TrajectoryStore = config.trajectories;

  // The SDK takes each provider's own options: Ollama's endpoint is `host` and it has a context length.
  const provider: ProviderOption =
    config.provider === "ollama"
      ? { kind: "ollama", model: config.model, host: config.baseUrl, contextLength: config.contextLength }
      : { kind: "openrouter", apiKey: config.apiKey ?? "", model: config.model, baseUrl: config.baseUrl };

  // Wrap createSession so a missing API key or bad config exits cleanly (no stack trace).
  let session: Awaited<ReturnType<typeof createSession>>;
  try {
    session = await createSession({
      cwd: root,
      provider,
      sources: ["user", "project"],
      approve,
      maxSteps,
      sandbox: config.sandbox,
      yolo: config.yolo,
      thinking: config.thinking,
      persist: true,
      trajectories,
      diagnostics: config.diagnostics,
      checks: config.checks,
      configDir,
      mcpTools: config.mcpTools,
      resume: options.resume,
      // To stderr, so stdout stays the reply (or JSON): an MCP server that needs a login (/mcp login in the TUI), a
      // trajectory that can't be written.
      onWarning: (text) => console.error(`marv: ${text}`),
    });
  } catch (err) {
    console.error(`Failed to create session: ${err instanceof Error ? err.message : String(err)}`);
    return 1;
  }

  try {
    return await collectAndOutput(session, options);
  } finally {
    await Promise.race([session.close(), Bun.sleep(2000)]);
  }
}

/** Key that uniquely identifies a tool_end event with a refused/unapproved call. */
function refusedCallKey(result: { tool?: string; label?: string; output?: string }): string {
  return `${result.tool ?? ""}:${result.label ?? ""}`;
}

async function collectAndOutput(
  session: Awaited<ReturnType<typeof createSession>>,
  options: HeadlessOptions,
): Promise<number> {
  const format = options.output ?? "text";
  let reply = "";
  let reason = "error";
  let lastAssistantText: string | undefined;
  const allEvents: SessionEvent[] = [];
  const errors: string[] = [];
  // Calls that were refused (no approver, output matches "there's no one to ask").
  const refused: { tool: string; label: string }[] = [];
  const seenRefused = new Set<string>();

  const NO_APPROVER = "needs the user's approval, and there's no one to ask.";

  /** Detect a refused call (no approver, or the user said no) and list it once. */
  const noteRefused = (event: Extract<SessionEvent, { type: "tool_end" }>) => {
    const { result } = event;
    if (!(result.output.includes(NO_APPROVER) || result.approval === "no" || result.declined)) return;
    const tool = event.call.name;
    const label = result.label ?? tool;
    const key = refusedCallKey({ tool, label });
    if (seenRefused.has(key)) return;
    seenRefused.add(key);
    refused.push({ tool, label });
  };

  // With -c the session's totals include the resumed conversation's: the turn's usage is what this turn added.
  const before = { ...session.usage().totals };

  for await (const event of session.send(options.prompt)) {
    allEvents.push(event);

    switch (event.type) {
      case "text_delta":
        if (format === "stream-json") {
          const line = JSON.stringify({ type: "text_delta", text: event.text }) + "\n";
          process.stdout.write(line);
        }
        break;
      case "thinking_delta":
        if (format === "stream-json") {
          const line = JSON.stringify({ type: "thinking_delta", text: event.text }) + "\n";
          process.stdout.write(line);
        }
        break;
      case "assistant":
        lastAssistantText = event.text;
        if (format === "stream-json") {
          const line = JSON.stringify({ type: "assistant", text: event.text }) + "\n";
          process.stdout.write(line);
        }
        break;
      case "tool_start":
        if (format === "stream-json") {
          const line = JSON.stringify({ type: "tool_start", tool: event.call.name, label: event.label }) + "\n";
          process.stdout.write(line);
        }
        break;
      case "subagent":
        // A subagent's calls are the turn's work too: one refused there may be missing from its report.
        if (event.event.type === "tool_end") noteRefused(event.event);
        break;
      case "tool_end": {
        const tool = event.call.name;
        const label = event.result.label ?? event.call.name;
        const output = event.result.output;
        noteRefused(event);
        if (format === "stream-json") {
          const line = JSON.stringify({
            type: "tool_end",
            tool,
            label,
            result: output,
            error: event.result.isError,
            approval: event.result.approval,
          }) + "\n";
          process.stdout.write(line);
        }
        break;
      }
      case "tool_skipped":
        if (format === "stream-json") {
          const line = JSON.stringify({ type: "tool_skipped", tool: event.call.name, reason: event.output }) + "\n";
          process.stdout.write(line);
        }
        break;
      case "done":
        reason = event.reason;
        if (format === "stream-json") {
          const line = JSON.stringify({ type: "done", reason: event.reason }) + "\n";
          process.stdout.write(line);
        }
        break;
      case "turn_end":
        reason = event.reason;
        break;
      case "error":
        errors.push(event.message);
        if (format === "text" || format === "json") {
          console.error(event.message);
        } else {
          // stream-json: already emitted the event line above.
        }
        if (format === "stream-json") {
          const line = JSON.stringify({ type: "error", message: event.message }) + "\n";
          process.stdout.write(line);
        }
        break;
      case "status":
        if (format === "stream-json") {
          const line = JSON.stringify({ type: "status", status: event.status }) + "\n";
          process.stdout.write(line);
        }
        break;
      case "step_limit":
        if (format === "stream-json") {
          const line = JSON.stringify({ type: "step_limit", steps: event.steps, continued: event.continued }) + "\n";
          process.stdout.write(line);
        }
        break;
      case "cut_off":
        if (format === "stream-json") {
          const line = JSON.stringify({ type: "cut_off", continued: event.continued }) + "\n";
          process.stdout.write(line);
        }
        break;
      case "compaction":
        if (format === "stream-json") {
          const line = JSON.stringify({ type: "compaction", compacted: event.result.compacted }) + "\n";
          process.stdout.write(line);
        }
        break;
      case "text_loop":
        if (format === "stream-json") {
          const line = JSON.stringify({ type: "text_loop", continued: event.continued }) + "\n";
          process.stdout.write(line);
        }
        break;
      case "tool_loop":
        if (format === "stream-json") {
          const line = JSON.stringify({ type: "tool_loop", continued: event.continued }) + "\n";
          process.stdout.write(line);
        }
        break;
    }
  }

  // The final reply is the last assistant event's text, not the concatenation of every text_delta.
  reply = lastAssistantText ?? "";

  // Steps: the model's replies. A reply ends in its text ("assistant"), its tool calls, or both, so one is counted
  // at the first of those after something new streamed in (not every provider reports usage per request).
  let steps = 0;
  let counted = false;
  for (const e of allEvents) {
    if (e.type === "text_delta" || e.type === "thinking_delta" || e.type === "tool_call_delta" || e.type === "usage") counted = false;
    else if ((e.type === "assistant" || e.type === "tool_start" || e.type === "tool_skipped") && !counted) {
      steps++;
      counted = true;
    }
  }

  // Usage: the turn's totals (what was added across the whole turn), not just the last request.
  const usageInfo = session.usage();
  const totals = usageInfo.totals;
  const hasRequests = totals.requests > before.requests;
  const usageObj = hasRequests
    ? {
        promptTokens: totals.promptTokens - before.promptTokens,
        cachedTokens: totals.cachedTokens - before.cachedTokens,
        completionTokens: totals.completionTokens - before.completionTokens,
        cost: (totals.cost ?? 0) - (before.cost ?? 0),
      }
    : undefined;

  const result: HeadlessResult = {
    reply,
    reason,
    steps,
    usage: usageObj,
    refused: refused.length > 0 ? refused : undefined,
  };

  if (format === "json") {
    console.log(JSON.stringify(result, null, 2));
  } else if (format === "stream-json") {
    const line = JSON.stringify({ type: "result", ...result }) + "\n";
    process.stdout.write(line);
  } else {
    // text: just the reply on stdout
    process.stdout.write(reply);
    if (reply && !reply.endsWith("\n")) process.stdout.write("\n");
  }

  return exitCodeFor(reason, refused.length);
}

/**
 * The exit code for how a turn ended (TurnEndReason: a done reason or "interrupted") and how many calls were refused.
 * A refused call makes even a finished turn exit 3, on purpose (#38: "non-zero for … an approval it couldn't get"):
 * the model may have worked around it, but whoever runs Marv should know it wanted to do something it couldn't
 * (`refused` in the JSON says what), e.g. to rerun it with --approve.
 */
export function exitCodeFor(reason: string, refused: number): number {
  if (refused > 0) return 3;
  if (reason === "end" || reason === "stop") return 0;
  if (reason === "max_steps" || reason === "length" || reason === "loop") return 2;
  if (reason === "aborted" || reason === "interrupted" || reason === "declined") return 3;
  return 1; // "error", or anything unknown
}