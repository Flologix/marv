import { SUBAGENT_TOOLS, type AgentType } from "../agents.ts";
import { maskKey, PRESETS, type Config } from "../config/config.ts";
import { sandboxAvailable } from "../sandbox.ts";
import { hiddenPathFoldersNotice } from "./sandbox-notice.ts";
import type { McpServerStatus } from "../mcp/manager.ts";
import type { Skill } from "../skills.ts";
import { usageReport, type Totals } from "../usage.ts";
import type { Usage } from "../provider/types.ts";

// Slash commands are handled locally and never reach the LLM.
// Each command returns an action; the App decides how to apply it,
// which keeps this module pure and easy to test.

export type CommandAction =
  | { type: "print"; text: string; isError?: boolean; markdown?: boolean }
  | { type: "clear" }
  | { type: "setup" }
  /** With an id: switch straight to it. Without: open the model picker. */
  | { type: "model"; id?: string }
  | { type: "thinking"; on: boolean }
  | { type: "sandbox"; on: boolean }
  | { type: "yolo"; on: boolean }
  | { type: "trajectories"; on: boolean }
  | { type: "diagnostics"; on: boolean }
  /**
   * The step limit: show it (no `every`), set it to a whole number of steps (0 = never ask), or turn it
   * off. The App saves it in the config and passes it to the session.
   */
  | { type: "steps"; every?: number }
  /** Trust and start the project's MCP servers. */
  | { type: "mcp-trust" }
  /** Rate or tag the last turn: 1 good, -1 bad, 0 labels only. */
  | { type: "feedback"; score: 1 | -1 | 0; note?: string; labels?: string[] }
  | { type: "resume" }
  | { type: "remember"; scope: "personal" | "project"; text: string }
  | { type: "forget"; text: string }
  | { type: "memory" }
  /** Summarize the conversation so far and continue from the summary. */
  | { type: "compact"; focus?: string }
  /** The user ran a skill: /<name> <args>. */
  | { type: "skill"; skill: Skill; args: string }
  | { type: "exit" };

/** Read-only facts a command may need. */
export interface CommandContext {
  config: Config;
  configPath: string;
  skills?: Skill[];
  agents?: AgentType[];
  /** Agent files that couldn't be loaded, and why. */
  agentProblems?: string[];
  /** Skills that couldn't be loaded, and why. */
  skillProblems?: string[];
  /** MCP servers as they are now, and config problems, for /mcp. */
  mcp?: { servers: McpServerStatus[]; problems: string[] };
  /** Where this project's trajectories are logged, for /trajectories. */
  trajectoriesPath?: string;
  /** Tokens and cost so far, for /cost. */
  usage?: { totals: Totals; last: Usage | null; contextLength?: number };
  /** PATH folders under the home folder the sandbox can't see, shortened (~) for display; empty when there are none. */
  hiddenPathFolders?: string[];
}

interface Command {
  name: string;
  description: string;
  run: (args: string, ctx: CommandContext) => CommandAction;
}

export const commands: Command[] = [
  {
    name: "help",
    description: "Show available commands",
    run: () => ({ type: "print", text: helpText() }),
  },
  {
    name: "config",
    description: "Show the current configuration",
    run: (_args, ctx) => ({ type: "print", text: configText(ctx) }),
  },
  {
    name: "setup",
    description: "Change provider, model, or API key",
    run: () => ({ type: "setup" }),
  },
  {
    name: "model",
    description: "Switch model (/model to pick, /model <id> to set)",
    run: (args) => (args ? { type: "model", id: args } : { type: "model" }),
  },
  {
    name: "think",
    description: "Toggle model thinking (/think on, /think off)",
    run: (args, { config }) => {
      const arg = args.toLowerCase();
      if (arg && arg !== "on" && arg !== "off") return { type: "print", text: "Usage: /think, /think on, or /think off", isError: true };
      return { type: "thinking", on: arg ? arg === "on" : !config.thinking };
    },
  },
  {
    name: "memory",
    description: "Show what Marv remembers",
    run: () => ({ type: "memory" }),
  },
  {
    name: "remember",
    description: "Save a note to memory (/remember <note>, or /remember project: <note>)",
    run: (args) => {
      const project = /^project:\s*/i.exec(args);
      const text = project ? args.slice(project[0].length) : args;
      if (!text.trim()) return { type: "print", text: "Usage: /remember <note> (personal), or /remember project: <note>", isError: true };
      return { type: "remember", scope: project ? "project" : "personal", text };
    },
  },
  {
    name: "forget",
    description: "Remove a memory (/forget <text from it>)",
    run: (args) => (args.trim() ? { type: "forget", text: args } : { type: "print", text: "Usage: /forget <text from the memory>", isError: true }),
  },
  {
    name: "compact",
    description: "Summarize the conversation to free up context (/compact <what to keep>)",
    run: (args) => ({ type: "compact", ...(args ? { focus: args } : {}) }),
  },
  {
    name: "resume",
    description: "Pick up an earlier session in this project",
    run: () => ({ type: "resume" }),
  },
  {
    name: "cost",
    description: "Show tokens used and cost so far",
    run: (_args, { usage }) => ({
      type: "print",
      text: usage ? usageReport(usage.totals, usage.last, usage.contextLength) : "No requests yet this session.",
    }),
  },
  {
    name: "agents",
    description: "List the subagent types Marv can start",
    run: (_args, { agents = [], agentProblems = [] }) => ({ type: "print", text: agentsText(agents, agentProblems), markdown: true }),
  },
  {
    name: "skills",
    description: "List the skills Marv can use",
    run: (_args, { skills = [], skillProblems = [] }) => ({ type: "print", text: skillsText(skills, skillProblems), markdown: true }),
  },
  {
    name: "sandbox",
    description: "Show or set the bash sandbox (/sandbox on, /sandbox off)",
    run: (args, { config, hiddenPathFolders = [] }) => {
      const arg = args.toLowerCase();
      if (arg === "on" || arg === "off") return { type: "sandbox", on: arg === "on" };
      if (arg) return { type: "print", text: "Usage: /sandbox, /sandbox on, or /sandbox off", isError: true };
      return { type: "print", text: `Sandbox: ${sandboxStatus(config, hiddenPathFolders)}` };
    },
  },
  {
    name: "yolo",
    description: "Run what the sandbox confines without asking (/yolo on, /yolo off)",
    run: (args, { config }) => {
      const arg = args.toLowerCase();
      if (arg && arg !== "on" && arg !== "off") return { type: "print", text: "Usage: /yolo, /yolo on, or /yolo off", isError: true };
      return { type: "yolo", on: arg ? arg === "on" : !config.yolo };
    },
  },
  {
    name: "mcp",
    description: "Show MCP servers and their tools (/mcp trust starts the project's)",
    run: (args, { mcp }) => {
      const arg = args.trim().toLowerCase();
      if (arg === "trust") return { type: "mcp-trust" };
      if (arg) return { type: "print", text: "Usage: /mcp, or /mcp trust", isError: true };
      return { type: "print", text: mcpText(mcp), markdown: true };
    },
  },
  {
    name: "good",
    description: "Rate the last turn as good (/good <optional note>)",
    run: (args) => ({ type: "feedback", score: 1, ...(args.trim() ? { note: args.trim() } : {}) }),
  },
  {
    name: "bad",
    description: "Rate the last turn as bad (/bad <what went wrong>)",
    run: (args) => ({ type: "feedback", score: -1, ...(args.trim() ? { note: args.trim() } : {}) }),
  },
  {
    name: "label",
    description: "Tag the last turn (/label refactor, tests)",
    run: (args) => {
      const labels = args.split(/[\s,]+/).filter(Boolean);
      return labels.length ? { type: "feedback", score: 0, labels } : { type: "print", text: "Usage: /label <tag> [more tags]", isError: true };
    },
  },
  {
    name: "trajectories",
    description: "Show or set run logging (/trajectories on, /trajectories off)",
    run: (args, { config, trajectoriesPath }) => {
      const arg = args.toLowerCase();
      if (arg === "on" || arg === "off") return { type: "trajectories", on: arg === "on" };
      if (arg) return { type: "print", text: "Usage: /trajectories, /trajectories on, or /trajectories off", isError: true };
      return { type: "print", text: `Trajectories: ${trajectoriesStatus(config, trajectoriesPath)}` };
    },
  },
  {
    name: "diagnostics",
    description: "Typecheck after the model's edits (/diagnostics on, /diagnostics off)",
    run: (args, { config }) => {
      const arg = args.toLowerCase();
      if (arg === "on" || arg === "off") return { type: "diagnostics", on: arg === "on" };
      if (arg) return { type: "print", text: "Usage: /diagnostics, /diagnostics on, or /diagnostics off", isError: true };
      return { type: "print", text: `Diagnostics: ${diagnosticsStatus(config)}` };
    },
  },
  {
    name: "steps",
    description: "Show or set the step limit (/steps 40, /steps off never asks)",
    run: (args, { config }) => {
      const arg = args.trim();
      if (/^off$/i.test(arg)) return { type: "steps", every: 0 };
      if (/^\d+$/.test(arg)) {
        const every = Number(arg);
        // 1 would ask after every step; use /steps off for never asking.
        if (every === 1) return { type: "print", text: "The smallest interval is 2 steps; /steps off never asks.", isError: true };
        return { type: "steps", every };
      }
      if (arg) return { type: "print", text: "Usage: /steps, /steps <number>, or /steps off", isError: true };
      return { type: "print", text: stepsStatus(config) };
    },
  },
  {
    name: "clear",
    description: "Clear the conversation",
    run: () => ({ type: "clear" }),
  },
  {
    name: "exit",
    description: "Quit Marv",
    run: () => ({ type: "exit" }),
  },
];

export function isCommand(input: string): boolean {
  return input.startsWith("/");
}

export function runCommand(input: string, ctx: CommandContext): CommandAction {
  const [rawName = "", ...rest] = input.slice(1).trim().split(/\s+/);
  const name = rawName.toLowerCase();
  const command = commands.find((c) => c.name === name);
  const skill = ctx.skills?.find((s) => s.name === name);
  if (!command && skill) return { type: "skill", skill, args: rest.join(" ") };
  if (!command) {
    return { type: "print", text: `Unknown command: /${name}. Type /help for a list.`, isError: true };
  }
  return command.run(rest.join(" "), ctx);
}

function helpText(): string {
  const width = Math.max(...commands.map((c) => c.name.length)) + 2;
  const lines = commands.map((c) => `  /${c.name.padEnd(width)}${c.description}`);
  return ["Commands:", ...lines, "", "Shortcuts:", "  ↑/↓       input history", "  esc       stop a reply or tool", "  ctrl+o    show subagents' steps and whole diffs", "  ctrl+c    clear input · press twice to exit"].join("\n");
}

function configText({ config, configPath }: CommandContext): string {
  const preset = PRESETS[config.provider];
  const key = !preset.keyEnv
    ? "not needed"
    : config.apiKey
      ? `${maskKey(config.apiKey)} (from ${config.apiKeySource === "env" ? preset.keyEnv : "config file"})`
      : "not set";
  return [
    `Provider:  ${preset.label}`,
    `Model:     ${config.model}`,
    `Thinking:  ${config.thinking ? "on" : "off"}`,
    `Sandbox:   ${sandboxStatus(config)}`,
    `Yolo:      ${config.yolo ? "on" : "off"} (/yolo)`,
    `Trajectories: ${config.trajectories ? "on" : "off"} (/trajectories)`,
    `Diagnostics: ${config.diagnostics ? "on" : "off"} (/diagnostics)`,
    `Steps: ${stepsStatus(config)}`,
    `Endpoint:  ${config.baseUrl}`,
    ...(config.provider === "ollama" ? [`Context:   ${config.contextLength.toLocaleString("en-US")} tokens (contextLength in the config file)`] : []),
    `API key:   ${key}`,
    `File:      ${configPath}`,
  ].join("\n");
}

export function diagnosticsStatus(config: Config): string {
  return config.diagnostics
    ? "on: after the model changes files in a TypeScript project, Marv runs the project's typecheck (in the sandbox, read-only) and tells the model about new errors"
    : "off";
}

/** What the step limit does with this config, for /steps and the notice when it's changed. */
export function stepsStatus(config: Config): string {
  return config.maxSteps === 0
    ? "off: Marv never asks to keep going, and a turn without an approver doesn't stop at a step limit either (subagents still stop at their own 50)."
    : `Keep going? after ${config.maxSteps} steps (0, or off, never asks). Without someone to ask, the turn stops there.`;
}

export function trajectoriesStatus(config: Config, path = "~/.marv/trajectories"): string {
  return config.trajectories
    ? `on: every turn is logged to ${path} (rate turns with /good, /bad, /label)`
    : "off: turns aren't logged";
}

/** What yolo mode does with this config, for the notice when it's turned on. */
export function yoloStatus(config: Config): string {
  if (!config.yolo) return "Yolo off: every change asks first.";
  const sandboxed = config.sandbox && sandboxAvailable();
  return sandboxed
    ? "Yolo on: sandboxed commands and file edits run without asking. Network access, git changes (git_write), edits inside .git, and memory still ask."
    : `Yolo on: file edits run without asking. Commands still ask: ${config.sandbox ? "bubblewrap isn't available here" : "the sandbox is off"}, so nothing confines them.`;
}

function sandboxStatus(config: Config, hiddenPathFolders: string[] = []): string {
  if (!config.sandbox) return "off: bash commands run directly on your system (after approval)";
  if (!sandboxAvailable()) return "on, but bubblewrap isn't available here, so bash runs WITHOUT a sandbox";
  const hidden = hiddenPathFolders.length ? ` ${hiddenPathFoldersNotice(hiddenPathFolders)}` : "";
  return `on: bash runs in bubblewrap (project writable, home hidden, no network unless asked).${hidden}`;
}

const MCP_HELP = `No MCP servers yet. Add them to \`.mcp.json\` in the project, or \`~/.marv/mcp.json\` for every project (the same format as Claude Code's), then restart Marv:

\`\`\`json
{ "mcpServers": {
    "files": { "command": "npx", "args": ["-y", "some-mcp-server"] },
    "docs": { "type": "http", "url": "https://example.com/mcp", "headers": { "Authorization": "Bearer \${DOCS_TOKEN}" } } } }
\`\`\``;

/** Markdown: one bullet per server, with its state and tools. */
function mcpText(mcp: CommandContext["mcp"]): string {
  if (!mcp || (mcp.servers.length === 0 && mcp.problems.length === 0)) return MCP_HELP;
  const lines = mcp.servers.map((s) => {
    const who = `- **${s.name}** (${s.source === "project" ? "this project" : "yours"}): `;
    switch (s.state) {
      case "connected": {
        const tools = s.tools.map((t) => `\`${t.replace(`mcp__${s.name}__`, "")}\``).join(", ");
        const skipped = s.skipped?.length ? `; not offered (another server's tool has the name): ${s.skipped.map((t) => `\`${t}\``).join(", ")}` : "";
        return `${who}connected · ${s.tools.length} tool${s.tools.length === 1 ? "" : "s"}${tools ? `: ${tools}` : ""}${skipped}`;
      }
      case "untrusted": {
        const reads = s.reads.length ? `; reads ${s.reads.map((v) => `\`$${v}\``).join(", ")} from your environment` : "";
        const files = s.runsProjectFiles.length ? `; runs this project's ${s.runsProjectFiles.map((f) => `\`${f}\``).join(", ")}` : "";
        return `${who}not trusted yet: \`${s.target}\`${reads}${files} (\`/mcp trust\` to start it)`;
      }
      case "connecting":
        return `${who}starting…`;
      case "failed":
        return `${who}failed: ${s.error ?? "unknown error"}${s.stderr ? `\n\n  \`\`\`\n  ${s.stderr.split("\n").slice(-5).join("\n  ")}\n  \`\`\`` : ""}`;
    }
  });
  const problems = mcp.problems.length ? ["", "Couldn't load:", ...mcp.problems.map((p) => `- ${p}`)] : [];
  return [...lines, ...problems].join("\n");
}

/** Markdown, so long descriptions wrap with a hanging indent under each skill. */
function skillsText(skills: Skill[], problems: string[]): string {
  const parts = skills.length
    ? [
        "**Skills**: run one with `/<name>`, or let Marv pick when a request matches.",
        skills.map((s) => `- \`/${s.name}\`: ${s.description}${s.source === "personal" ? " *(personal)*" : ""}`).join("\n"),
      ]
    : [
        "**No skills yet.** Add one as `.marv/skills/<name>/SKILL.md` in the project, or in `~/.marv/skills/` for all projects:",
        "```markdown\n---\nname: <name>\ndescription: <what it does, and when to use it>\n---\n\n<instructions>\n```",
      ];
  if (problems.length) parts.push("**Couldn't load:**", problems.map((p) => `- ${p}`).join("\n"));
  return parts.join("\n\n");
}

/** Markdown, like /skills. Descriptions are cut at their first line (Claude Code ones carry long examples). */
function agentsText(agents: AgentType[], problems: string[]): string {
  const parts = [
    "**Agents**: Marv can hand a task to one of these. Add your own as `.marv/agents/<name>.md` (project) or `~/.marv/agents/<name>.md`.",
    agents
      .map((a) => {
        const details = [a.tools.length === SUBAGENT_TOOLS.length ? "" : `tools: ${a.tools.join(", ")}`, a.model ? `model: ${a.model}` : ""].filter(Boolean).join(" · ");
        return `- \`${a.name}\` *(${a.source})*: ${a.description.split("\n")[0]}${details ? `\n  ${details}` : ""}`;
      })
      .join("\n"),
  ];
  if (problems.length) parts.push("**Couldn't load:**", problems.map((p) => `- ${p}`).join("\n"));
  return parts.join("\n\n");
}
