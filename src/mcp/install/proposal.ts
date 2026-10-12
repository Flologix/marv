// What /mcp add proposes, and the one approval that decides it. The rule: the user approves the exact entry that
// gets written, and nothing fetched after the approval changes what runs. So the preview is built from the
// entry itself (displayOf, as /mcp shows servers), and the App writes the JSON the preview showed, parsed back.
import { displayOf, readsOf } from "../config.ts";
import type { Env } from "../../config/config.ts";
import { printable } from "../../printable.ts";
import type { get as realGet } from "../../tools/web/fetch.ts";
import type { ApprovalRequest } from "../../tools/types.ts";
import type { McpServerStatus } from "../manager.ts";
import type { Source } from "./source.ts";

/** An mcp.json entry, as written: ${VAR} references, never values. */
export type McpEntry = { command: string; args: string[]; env?: Record<string, string> } | { type: "http"; url: string; headers?: Record<string, string> };

export interface Proposal {
  /** The key in mcp.json. */
  name: string;
  entry: McpEntry;
  /** Lines for the approval ("npm      pkg 1.2.3, published …"), already printable. */
  facts: string[];
  /** Shown in the warning color: deprecated, brand new, a repository that doesn't match. */
  warnings: string[];
}

/** A GitHub README that nominates several servers: the commands to choose from. */
export interface Choices {
  choices: string[];
}

/** How resolvers reach the network: Marv's own `get` (address checks, time and size limits). Tests point it at Bun.serve. */
export interface ResolveDeps {
  get: typeof realGet;
  signal?: AbortSignal;
  /** Base URLs, for tests. */
  npmRegistry?: string;
  pypi?: string;
  registry?: string;
  /** The README of a GitHub repository (null: it has none). */
  readme?: (owner: string, repo: string, signal?: AbortSignal) => Promise<string | null>;
  now?: () => Date;
  /** Whether a remote server asks for an OAuth login (#18: probeLogin in src/mcp/oauth.ts); unset: not asked. */
  probeLogin?: (url: string, signal?: AbortSignal) => Promise<boolean>;
}

/** What the user passed besides the source. */
export interface ResolveOptions {
  name?: string;
  env: string[];
  headers?: Record<string, string>;
  args: string[];
}

/** "npm      text": the label in a column, so the facts line up. Stranger-written text is made printable here. */
export const fact = (label: string, text: string) => `${label.padEnd(8)} ${printable(text).replace(/\s+/g, " ").trim()}`;

/** `--env NAME` → "NAME": "${NAME}". */
export const envEntries = (names: string[]) => Object.fromEntries(names.map((n) => [n, `\${${n}}`]));

/** "git+https://github.com/o/r.git" → "github.com/o/r": how a repository is shown. */
export function repoOf(url: string | undefined): string | undefined {
  if (!url) return undefined;
  const shown = url
    .trim()
    .replace(/^git\+/, "")
    .replace(/^git@([^:]+):/, "$1/")
    .replace(/^[a-z+]+:\/\//i, "")
    .replace(/^www\./, "")
    .replace(/\.git$/, "")
    .replace(/\/+$/, "");
  return shown ? shown.slice(0, 200) : undefined;
}

/** A remote server: nothing to fetch and nothing to pin. */
export function httpProposal(source: Extract<Source, { kind: "http" }>, opts: { name: string; headers?: Record<string, string> }): Proposal {
  const headers = opts.headers && Object.keys(opts.headers).length ? opts.headers : undefined;
  return {
    name: opts.name,
    entry: { type: "http", url: source.url, ...(headers ? { headers } : {}) },
    facts: [fact("remote", `${new URL(source.url).host}: runs whatever its owner deploys there (nothing to pin)`)],
    warnings: [],
  };
}

let nextScope = 0;
const BIDI = /[\u061c\u200e\u200f\u202a-\u202e\u2066-\u2069]/;

/**
 * The approval for a proposal, and the JSON it shows: the App writes exactly that JSON (parsed back), so what was
 * shown and what's written can't drift. `once`: no "don't ask again", every install is its own decision.
 */
export function approvalFor(proposal: Proposal, ctx: { env: Env; projectServers?: string[] }): { request: ApprovalRequest; json: string } {
  const { name, entry } = proposal;
  const json = JSON.stringify(entry);
  // The screen shows text through printable(): an entry it would change (C1 controls, which JSON doesn't escape) or
  // one that reads differently from what it is (bidi overrides) can't be shown as it is written, so it isn't offered.
  if (printable(json) !== json || BIDI.test(json)) throw new Error(`The entry for "${name}" contains control characters, so it can't be shown exactly as it would be written: not added.`);
  const remote = "url" in entry;
  const lines = [...proposal.facts];
  if (remote) lines.unshift(fact("url", displayOf(entry)));
  for (const variable of readsOf(entry)) {
    lines.push(fact("reads", ctx.env[variable] === undefined ? `${variable} (not set: it won't start until you set it and restart Marv)` : `${variable} (set)`));
  }
  if (ctx.projectServers?.includes(name)) lines.push(fact("note", `this project's .mcp.json also has a server "${name}": it will be ignored, yours wins`));
  lines.push(fact("writes", `${JSON.stringify(name)}: ${json}`));
  return {
    json,
    request: {
      tool: "mcp_add",
      label: `/mcp add ${name}`,
      preview: {
        title: `Add MCP server "${name}" to ~/.marv/mcp.json (yours, every project)`,
        ...(remote ? {} : { command: displayOf(entry) }),
        text: lines.join("\n"),
        note: remote
          ? "a remote server: it gets what Marv's calls send it, and the headers above"
          : "runs outside the sandbox, with your permissions and network, started in your home folder",
        ...(proposal.warnings.length ? { warning: proposal.warnings.map((w) => printable(w)).join("\n") } : {}),
      },
      // Unique, so nothing is ever remembered (and `once` hides the choice anyway).
      scope: { key: `mcp-add:${++nextScope}`, description: `adding ${name}` },
      once: true,
    },
  };
}

/**
 * What /mcp add says once the server was written and started. The hook for servers that need a login (#18): its
 * state then says so, and this is where "then /mcp login <name>" goes.
 */
export function addedNotice(status: McpServerStatus): { text: string; isError: boolean } {
  const where = "~/.marv/mcp.json";
  switch (status.state) {
    case "connected": {
      const n = status.tools.length;
      return { text: `Added ${status.name} to ${where}: connected · ${n} tool${n === 1 ? "" : "s"}. Marv can use ${n === 1 ? "it" : "them"} from your next message.`, isError: false };
    }
    case "needs-login":
      return { text: `Added ${status.name} to ${where}. It needs a login: /mcp login ${status.name} (Marv can use its tools from your next message after that).`, isError: false };
    default:
      return { text: `Added ${status.name} to ${where}, but it didn't start: ${status.error ?? status.state} (see /mcp; fix the entry or /mcp remove ${status.name}).`, isError: true };
  }
}
