// Agent types: the kinds of subagent Marv can start, as Markdown files.
//
//   .marv/agents/<name>.md      (the project's)
//   ~/.marv/agents/<name>.md    (your personal ones)
//
// YAML frontmatter (name, description, optionally tools and model), then the
// agent's own instructions, which start its system prompt. The format matches
// Claude Code's agent files, so ones written for it (like superpowers'
// code-reviewer.md) work unchanged. Like skills, only names and descriptions
// go into the main system prompt; the model picks a type when it starts one.
import { existsSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { parseFrontmatter } from "./frontmatter.ts";

export interface AgentType {
  name: string;
  description: string;
  /** Its own instructions ("You are a code reviewer…"); empty for general-purpose. */
  body: string;
  /** The tools it may use (Marv's names). */
  tools: readonly string[];
  /** A model on the same provider; undefined means the session's model. */
  model?: string;
  source: "built-in" | "project" | "personal";
}

/**
 * What subagents may use. Never `agent` (no subagents of subagents: no runaway
 * recursion) or `memory` (memory outlives the session, so only the main agent
 * changes it, with the user's approval).
 */
export const SUBAGENT_TOOLS: readonly string[] = ["read_file", "glob", "grep", "web_fetch", "github", "skill", "edit_file", "write_file", "bash"];

/** Claude Code's tool names, so agent files written for it work here. */
const ALIASES: Record<string, string> = {
  Read: "read_file",
  Glob: "glob",
  Grep: "grep",
  WebFetch: "web_fetch",
  Edit: "edit_file",
  Write: "write_file",
  MultiEdit: "edit_file",
  LS: "glob",
  Bash: "bash",
  Skill: "skill",
};

export const GENERAL_PURPOSE: AgentType = {
  name: "general-purpose",
  description: "Any self-contained task: research across many files, implementing one planned task, running and fixing tests.",
  body: "",
  tools: SUBAGENT_TOOLS,
  source: "built-in",
};

/** Claude Code's model shorthands: not ids on Marv's providers, so they mean the session's model, like inherit. */
const CLAUDE_CODE_MODELS = ["inherit", "sonnet", "opus", "haiku"];
const NAME = /^[a-z0-9][a-z0-9-]{0,63}$/;
const MAX_BODY_CHARS = 50_000;

export const agentsDir = (base: string) => join(base, ".marv", "agents");

function parseTools(value: unknown, shown: string): readonly string[] | string {
  if (value === undefined || value === null) return SUBAGENT_TOOLS;
  const list = Array.isArray(value) ? value.map(String) : typeof value === "string" ? value.split(",") : null;
  if (!list) return `${shown}: tools should be a list of tool names.`;
  const tools: string[] = [];
  for (const raw of list.map((t) => t.trim()).filter(Boolean)) {
    const name = ALIASES[raw] ?? raw;
    if (!SUBAGENT_TOOLS.includes(name)) return `${shown}: subagents can't use "${raw}". They can use: ${SUBAGENT_TOOLS.join(", ")}.`;
    if (!tools.includes(name)) tools.push(name);
  }
  return tools;
}

async function readAgent(path: string, file: string, source: AgentType["source"], shown: string): Promise<AgentType | string> {
  const parsed = parseFrontmatter(await Bun.file(path).text(), shown);
  if ("error" in parsed) return parsed.error;
  const { fields } = parsed;
  const name = typeof fields.name === "string" ? fields.name.trim() : file.replace(/\.md$/, "");
  const description = typeof fields.description === "string" ? fields.description.trim() : "";
  if (!NAME.test(name)) return `${shown}: the name "${name}" should be lowercase letters, digits and dashes.`;
  if (!description) return `${shown} needs a description: it's how the model knows when to use the agent.`;
  const tools = parseTools(fields.tools, shown);
  if (typeof tools === "string") return tools;
  const rawModel = typeof fields.model === "string" ? fields.model.trim() : "";
  let body = parsed.body;
  if (body.length > MAX_BODY_CHARS) body = `${body.slice(0, MAX_BODY_CHARS)}\n\n(${file} was cut off here: it's longer than ${MAX_BODY_CHARS} characters.)`;
  return { name, description, body, tools, model: rawModel && !CLAUDE_CODE_MODELS.includes(rawModel) ? rawModel : undefined, source };
}

/** Built-in, then personal (under `home`), then project agents (under `root`; later wins a name clash). Either can be left out. Skips broken ones, saying why. */
export async function loadAgents({ root, home }: { root?: string; home?: string }): Promise<{ agents: AgentType[]; problems: string[] }> {
  const byName = new Map<string, AgentType>([[GENERAL_PURPOSE.name, GENERAL_PURPOSE]]);
  const problems: string[] = [];
  const sources = [
    { base: home, source: "personal" as const, prefix: "~/" },
    { base: root, source: "project" as const, prefix: "" },
  ];
  for (const { base, source, prefix } of sources) {
    if (base === undefined) continue;
    const dir = agentsDir(base);
    if (!existsSync(dir)) continue;
    for (const file of readdirSync(dir).sort()) {
      if (!file.endsWith(".md")) continue;
      const shown = `${prefix}.marv/agents/${file}`;
      let result: AgentType | string;
      try {
        result = await readAgent(join(dir, file), file, source, shown);
      } catch (err) {
        result = `${shown}: couldn't be read (${(err as Error).message}).`;
      }
      if (typeof result === "string") problems.push(result);
      else byName.set(result.name, result);
    }
  }
  return { agents: [...byName.values()].sort((a, b) => a.name.localeCompare(b.name)), problems };
}

/**
 * Finds a type by name. Skills written for Claude Code name plugin agents with
 * a prefix ("superpowers:code-reviewer"), so that's tried without it too.
 */
export function findAgent(agents: AgentType[], name: string): AgentType | undefined {
  const exact = agents.find((a) => a.name === name);
  if (exact || !name.includes(":")) return exact;
  const bare = name.slice(name.lastIndexOf(":") + 1);
  return agents.find((a) => a.name === bare);
}
