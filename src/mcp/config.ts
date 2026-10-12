// Which MCP servers to connect to. The format is Claude Code's, so the same
// files work in both:
//
//   .mcp.json (the project)  and  ~/.marv/mcp.json (yours, every project)
//   { "mcpServers": {
//       "files": { "command": "npx", "args": ["-y", "some-server"], "env": { … } },
//       "docs":  { "type": "http", "url": "https://…/mcp", "headers": { "Authorization": "Bearer ${TOKEN}" } } } }
//
// ${VAR} and ${VAR:-default} are filled in from the environment, so tokens
// can stay out of the file. A project's servers only start once the user
// trusts them (src/mcp/trust.ts): a cloned repository could otherwise run any
// program the moment Marv starts.
import { createHash } from "node:crypto";
import { existsSync, readFileSync, statSync } from "node:fs";
import { dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { z } from "zod";
import type { Env } from "../config/config.ts";

export const PROJECT_FILE = ".mcp.json";
export const PERSONAL_FILE = "mcp.json";

export type McpTransport =
  | { type: "stdio"; command: string; args?: string[]; env?: Record<string, string> }
  | { type: "http"; url: string; headers?: Record<string, string>; oauth?: McpOAuthConfig };

/**
 * How Marv logs in to a server that wants OAuth (#18), all optional. A server without dynamic client registration
 * needs a client registered by hand: its id, its secret (a ${VAR}, like headers) and the port its redirect URI names.
 * `scope` overrides the scope the server asks for.
 */
export interface McpOAuthConfig {
  clientId?: string;
  clientSecret?: string;
  callbackPort?: number;
  scope?: string;
}

export interface McpServerConfig {
  name: string;
  source: "project" | "personal";
  transport: McpTransport;
  /** Seconds a tool call may take (default 120). */
  timeout?: number;
  /**
   * Identifies this exact config (before ${VAR}s are filled in), plus the content of the project files its
   * command line names: trust is given to it, and lost when either changes.
   */
  key: string;
  /** What the user is shown: the raw config, ${VAR}s unexpanded, so secrets never appear on screen. */
  display?: string;
  /** Environment variables it reads through ${VAR} (a project server reading $OPENROUTER_API_KEY should stand out). */
  reads?: string[];
  /** Project files its command line names (a project server's): covered by `key`, but not the files they load. */
  runsProjectFiles?: string[];
}

const strings = z.record(z.string(), z.string());
const Stdio = z.object({
  type: z.literal("stdio").optional(),
  command: z.string().min(1),
  args: z.array(z.string()).optional(),
  env: strings.optional(),
  timeout: z.number().positive().optional(),
});
const OAuth = z
  .object({
    clientId: z.string().min(1).optional(),
    clientSecret: z.string().min(1).optional(),
    // A client registered by hand names one redirect URI; below 1024 needs root to listen on.
    callbackPort: z.number().int().min(1024).max(65535).optional(),
    scope: z.string().trim().min(1).optional(),
  })
  .strict();
const Http = z.object({
  type: z.enum(["http", "streamable-http"]),
  url: z.string().min(1),
  headers: strings.optional(),
  oauth: OAuth.optional(),
  timeout: z.number().positive().optional(),
});
const FileSchema = z.object({ mcpServers: z.record(z.string(), z.unknown()).optional() });

/** Server names become part of tool names (mcp__<server>__<tool>), which providers restrict. */
const NAME = /^[A-Za-z0-9_-]{1,32}$/;

/** A name parseServer accepts (see there for why "__" and a trailing "_" aren't). */
export const validServerName = (name: string) => NAME.test(name) && !name.includes("__") && !name.endsWith("_");

/** Fills in ${VAR} and ${VAR:-default}; throws naming the first variable that isn't set. */
function expand(text: string, env: Env): string {
  return text.replace(/\$\{([A-Za-z_][A-Za-z0-9_]*)(?::-([^}]*))?\}/g, (_, name: string, fallback?: string) => {
    const value = env[name] ?? fallback;
    if (value === undefined) throw new Error(`\${${name}} isn't set`);
    return value;
  });
}
const expandAll = (values: Record<string, string> | undefined, env: Env) =>
  values && Object.fromEntries(Object.entries(values).map(([k, v]) => [k, expand(v, env)]));

const VARIABLE = /\$\{([A-Za-z_][A-Za-z0-9_]*)(?::-[^}]*)?\}/g;
const listed = (values: Record<string, string> | undefined, sep: string) =>
  Object.entries(values ?? {})
    .map(([k, v]) => `${k}${sep}${v}`)
    .join(", ");

/** The raw config, as the user wrote it, on one line. */
export function displayOf(raw: z.infer<typeof Stdio> | z.infer<typeof Http>): string {
  if ("url" in raw) return [raw.url, raw.headers && `headers ${listed(raw.headers, ": ")}`, "oauth" in raw && raw.oauth && oauthShown(raw.oauth)].filter(Boolean).join(" · ");
  const env = raw.env && Object.keys(raw.env).length ? `env ${listed(raw.env, "=")}` : "";
  return [[raw.command, ...(raw.args ?? [])].join(" "), env].filter(Boolean).join(" · ");
}

/** "oauth client c, port 33418, scope repo": never the secret (it should be a ${VAR}, but a literal one mustn't show). */
function oauthShown(o: McpOAuthConfig): string {
  const parts = [o.clientId && `client ${o.clientId}`, o.callbackPort && `port ${o.callbackPort}`, o.scope && `scope ${o.scope}`].filter(Boolean);
  return parts.length ? `oauth ${parts.join(", ")}` : "oauth";
}

/** The variables a config reads from the environment, in order (not MARV_PROJECT_DIR: that's Marv's own). */
export function readsOf(raw: unknown): string[] {
  const names = [...JSON.stringify(raw).matchAll(VARIABLE)].map((m) => m[1]!).filter((n) => n !== "MARV_PROJECT_DIR");
  return [...new Set(names)];
}

/**
 * A project server's command line names project files ("node mcp/server.js"): an agent could change them without
 * asking (they're ordinary project files), and Marv would run the new code on the next start. So their content
 * is part of the trust key. Files those files load aren't covered (the notice says it runs project files).
 */
function projectFiles(root: string, transport: McpTransport): { files: string[]; digest: string } {
  if (transport.type !== "stdio") return { files: [], digest: "" };
  const files: string[] = [];
  const digest = createHash("sha256");
  for (const word of [transport.command, ...(transport.args ?? [])]) {
    const path = resolve(root, word);
    const rel = relative(root, path);
    if (!rel || rel.startsWith("..") || isAbsolute(rel)) continue;
    try {
      if (!statSync(path).isFile()) continue;
      digest.update(`${rel}\0`).update(readFileSync(path)).update("\0");
      files.push(rel.split(sep).join("/"));
    } catch {
      // not a file: an ordinary argument
    }
  }
  return { files, digest: files.length ? digest.digest("hex") : "" };
}

function parseServer(name: string, raw: unknown, source: McpServerConfig["source"], env: Env, where: string): McpServerConfig {
  if (!NAME.test(name)) throw new Error(`"${name}" in ${where}: use letters, digits, - and _ (at most 32)`);
  // mcp__<server>__<tool> must read one way only: with "a__b" or "a_" allowed, server "a" + tool "b__c" and server
  // "a__b" + tool "c" (or "a" + "_x" and "a_" + "x") would get the same tool name.
  if (name.includes("__") || name.endsWith("_")) throw new Error(`"${name}" in ${where}: a server name can't contain "__" or end in "_"`);
  const type = (raw as { type?: unknown } | null)?.type;
  if (type === "sse") throw new Error(`"${name}" in ${where}: the old sse transport isn't supported; most servers also speak "type": "http"`);
  const key = createHash("sha256").update(JSON.stringify(raw)).digest("hex");
  const fail = (err: unknown) => new Error(`"${name}" in ${where}: ${err instanceof Error ? err.message : String(err)}`);
  if (type === "http" || type === "streamable-http") {
    const parsed = Http.safeParse(raw);
    if (!parsed.success) throw fail(z.prettifyError(parsed.error));
    try {
      const { url, headers, timeout, oauth } = parsed.data;
      return {
        name,
        source,
        key,
        timeout,
        display: displayOf(parsed.data),
        reads: readsOf(raw),
        transport: {
          type: "http",
          url: expand(url, env),
          ...(headers ? { headers: expandAll(headers, env) } : {}),
          ...(oauth ? { oauth: { ...oauth, ...(oauth.clientSecret ? { clientSecret: expand(oauth.clientSecret, env) } : {}) } } : {}),
        },
      };
    } catch (err) {
      throw fail(err);
    }
  }
  // Unknown keys of a stdio entry are dropped, but this one would otherwise look like it did something.
  if (raw && typeof raw === "object" && "oauth" in raw) throw fail(new Error('"oauth" is only for http servers (a login to a remote server)'));
  const parsed = Stdio.safeParse(raw);
  if (!parsed.success) throw fail(z.prettifyError(parsed.error));
  try {
    const { command, args, env: serverEnv, timeout } = parsed.data;
    return {
      name,
      source,
      key,
      timeout,
      display: displayOf(parsed.data),
      reads: readsOf(raw),
      transport: {
        type: "stdio",
        command: expand(command, env),
        ...(args ? { args: args.map((a) => expand(a, env)) } : {}),
        ...(serverEnv ? { env: expandAll(serverEnv, env) } : {}),
      },
    };
  } catch (err) {
    throw fail(err);
  }
}

async function readFile(path: string, source: McpServerConfig["source"], env: Env, problems: string[]): Promise<McpServerConfig[]> {
  if (!existsSync(path)) return [];
  let entries: [string, unknown][];
  try {
    const parsed = FileSchema.safeParse(await Bun.file(path).json());
    if (!parsed.success) throw new Error(z.prettifyError(parsed.error));
    entries = Object.entries(parsed.data.mcpServers ?? {});
  } catch (err) {
    problems.push(`${path}: ${err instanceof Error ? err.message : String(err)}`);
    return [];
  }
  const servers: McpServerConfig[] = [];
  for (const [name, raw] of entries) {
    try {
      const server = parseServer(name, raw, source, env, path);
      if (source === "project") {
        const { files, digest } = projectFiles(dirname(path), server.transport);
        if (files.length) {
          server.key = createHash("sha256").update(`${server.key}\0${digest}`).digest("hex");
          server.runsProjectFiles = files;
        }
      }
      servers.push(server);
    } catch (err) {
      problems.push((err as Error).message);
    }
  }
  return servers;
}

/** The servers to connect to (yours first, then the project's), and what couldn't be read, and why. `include` leaves either file out. */
export async function loadMcpConfig({
  root,
  configDir,
  env: outer,
  include = { personal: true, project: true },
}: {
  root: string;
  configDir: string;
  env: Env;
  include?: { personal: boolean; project: boolean };
}) {
  const problems: string[] = [];
  // Servers of yours start in your home folder (see McpManager.cwdFor); one that works on the project gets it this way.
  const env = { ...outer, MARV_PROJECT_DIR: root };
  const projectPath = join(root, PROJECT_FILE);
  const project = include.project ? await readFile(projectPath, "project", env, problems) : [];
  const personal = include.personal ? await readFile(join(configDir, PERSONAL_FILE), "personal", env, problems) : [];
  const servers = [...personal];
  for (const server of project) {
    if (personal.some((p) => p.name === server.name)) {
      problems.push(`"${server.name}" in ${projectPath} was ignored: you have a server of your own with that name.`);
    } else {
      servers.push(server);
    }
  }
  return { servers, problems };
}

/**
 * Throws what loading `raw` as server `name` would report, except an unset ${VAR} (that's the user's environment,
 * not the entry): /mcp add checks an entry with it before writing it, so it never writes one Marv can't load.
 */
export function checkEntry(name: string, raw: unknown): void {
  const anything = new Proxy({} as Env, { get: () => "x" });
  parseServer(name, raw, "personal", anything, "mcp.json");
}

/**
 * Servers given in code (the SDK's `mcpServers`), in .mcp.json's format. Trusted like your own: whoever wrote the
 * program chose them, so they never wait for /mcp trust.
 */
export function parseMcpServers(entries: Record<string, unknown>, env: Env): { servers: McpServerConfig[]; problems: string[] } {
  const servers: McpServerConfig[] = [];
  const problems: string[] = [];
  for (const [name, raw] of Object.entries(entries)) {
    try {
      servers.push(parseServer(name, raw, "personal", env, "mcpServers"));
    } catch (err) {
      problems.push((err as Error).message);
    }
  }
  return { servers, problems };
}
