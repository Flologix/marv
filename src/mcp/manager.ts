// The session's MCP connections: starts the servers (yours, and the project's
// once trusted), lists their tools, and keeps them as Marv tools and specs.
//
// The tool list is fixed once the servers have settled (connected or failed):
// every request must send the same tool definitions, or the provider's prompt
// cache starts over. `ready` resolves when they have; the session's turn
// waits for it before the first request. Only the user adds tools later: trusting a project's servers
// (/mcp trust) or adding one (/mcp add). Removing one (/mcp remove) never takes tools away mid-conversation.
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import type { Transport } from "@modelcontextprotocol/sdk/shared/transport.js";
import { homedir } from "node:os";
import type { ToolSpec } from "../provider/types.ts";
import type { Tool } from "../tools/types.ts";
import type { McpServerConfig } from "./config.ts";
import { makeMcpTool, type McpCallResult, type McpToolInfo } from "./tool.ts";
import type { McpTrust } from "./trust.ts";

/** Long enough for `npx -y some-server` to download it the first time. */
const CONNECT_TIMEOUT_MS = 60_000;
const DEFAULT_CALL_TIMEOUT_S = 120;
const STDERR_LINES = 20;
/** Pages of tools/list read at most (a server listing hundreds of tools would swamp the context anyway). */
const MAX_PAGES = 50;

/** "removed": taken out with /mcp remove; its tools stay listed (the specs can't shrink) and their calls fail. */
export type McpState = "untrusted" | "connecting" | "connected" | "failed" | "removed";

export interface McpServerStatus {
  name: string;
  source: McpServerConfig["source"];
  /** "npx -y server" or the URL, as written in the config (${VAR}s unexpanded, so no secrets). */
  target: string;
  /** Environment variables it reads. */
  reads: string[];
  /** Project files its command line runs (a project server's). */
  runsProjectFiles: string[];
  state: McpState;
  /** Model-facing tool names, once connected. */
  tools: string[];
  error?: string;
  /** The end of a local server's stderr (it's kept off the screen). */
  stderr?: string;
  /** Its tools not offered because another server's tool already has that name. */
  skipped?: string[];
}

interface Connection {
  config: McpServerConfig;
  state: McpState;
  client?: Client;
  transport?: Transport;
  tools: { tool: Tool; spec: ToolSpec }[];
  /** Tool names already taken by an earlier server (set by rebuild). */
  skipped?: string[];
  error?: string;
  stderr: string[];
}

const describe = ({ transport }: McpServerConfig) =>
  transport.type === "http" ? transport.url : [transport.command, ...(transport.args ?? [])].join(" ");

function withTimeout<T>(promise: Promise<T>, ms: number, what: string): Promise<T> {
  let timer: ReturnType<typeof setTimeout>;
  return Promise.race([
    promise,
    new Promise<never>((_, reject) => {
      timer = setTimeout(() => reject(new Error(`${what} timed out after ${Math.round(ms / 1000)}s`)), ms);
    }),
  ]).finally(() => clearTimeout(timer));
}

export class McpManager {
  /** Every tool of every connected server, in config order. Replaced (never changed) when servers connect. */
  tools: Tool[] = [];
  specs: ToolSpec[] = [];
  /** Resolves once every server Marv was allowed to start has connected or failed. */
  ready: Promise<void> = Promise.resolve();
  /** Whether `ready` has resolved (so the session's turn knows whether to say it's waiting). */
  settled = true;
  /** Told when a server's state changes (for the UI). */
  onChange: () => void = () => {};
  private closed = false;
  private connections: Connection[];

  constructor(
    servers: McpServerConfig[],
    private readonly opts: { root: string; version: string; trust?: McpTrust; connectTimeoutMs?: number },
  ) {
    // "connecting" until start() has checked trust: calling them "untrusted" before that would let /mcp trust
    // start your own servers a second time.
    this.connections = servers.map((config) => ({ config, state: "connecting", tools: [], stderr: [] }));
  }

  /**
   * Starts one more server now (/mcp add), as startup would (your own: in your home folder, the safe base
   * environment). A turn that starts meanwhile waits for it, as for /mcp trust. Never rejects for a server
   * that fails to start: its status says why.
   */
  async add(config: McpServerConfig): Promise<McpServerStatus> {
    if (this.closed) throw new Error("Marv is closing.");
    const same = this.connections.find((c) => c.config.name === config.name);
    // A server removed in this conversation keeps its tools listed (the list can't shrink under it), and the new
    // one's tools would take the same names: it can come back in the next conversation.
    if (same?.state === "removed") {
      throw new Error(`"${config.name}" was removed in this conversation and its tools stay listed until the next one: /clear first, or pick another --name.`);
    }
    if (same) throw new Error(`There's already an MCP server named "${config.name}".`);
    const c: Connection = { config, state: "connecting", tools: [], stderr: [] };
    this.connections.push(c);
    const before = this.ready;
    this.settled = false;
    // Declared first: the wait below compares itself with this.ready once it's done.
    let ready: Promise<void> | undefined = undefined;
    ready = (async () => {
      await before;
      await this.connect(c);
      this.rebuild();
      // Only the latest wait says the servers have settled: another add() may have started meanwhile.
      if (this.ready === ready) this.settled = true;
    })();
    this.ready = ready;
    await ready;
    return this.status().find((s) => s.name === config.name)!;
  }

  /**
   * Stops a server (/mcp remove). Its tools stay in the tool list until the next conversation (an eager
   * conversation's history may call them, and the list can't shrink under it), and their calls fail clearly.
   */
  async remove(name: string): Promise<void> {
    const find = () => this.connections.find((x) => x.config.name === name && x.state !== "removed");
    // One still starting is removed once it has settled, so connect() can't mark it connected afterwards.
    if (find()?.state === "connecting") await this.ready;
    const c = find();
    if (!c) throw new Error(`There's no MCP server named "${name}" running.`);
    c.state = "removed";
    c.error = undefined;
    this.onChange();
    await c.client?.close().catch(() => {});
  }

  /** A new conversation: tools of servers removed with /mcp remove aren't offered any more. */
  forgetRemoved(): void {
    if (!this.connections.some((c) => c.state === "removed")) return;
    this.connections = this.connections.filter((c) => c.state !== "removed");
    this.rebuild();
    this.onChange();
  }

  /** Starts every trusted server. Project servers the user hasn't trusted wait for /mcp trust. */
  start(): Promise<void> {
    this.settled = false;
    this.ready = (async () => {
      // `ready` must never reject (nothing could start, and a rejection nobody handles ends the process): a trust
      // check that fails counts as "not trusted", and the server waits for /mcp trust.
      const trusted = await Promise.all(
        this.connections.map(async (c) => (this.opts.trust ? this.opts.trust.isTrusted(this.opts.root, c.config).catch(() => false) : true)),
      );
      this.connections.forEach((c, i) => {
        if (!trusted[i]) c.state = "untrusted";
      });
      this.onChange();
      await Promise.all(this.connections.filter((_, i) => trusted[i]).map((c) => this.connect(c)));
      this.rebuild();
      this.settled = true;
    })();
    return this.ready;
  }

  /** Project servers waiting for the user's trust. */
  untrusted(): McpServerConfig[] {
    return this.connections.filter((c) => c.state === "untrusted").map((c) => c.config);
  }

  /** Trusts the waiting project servers (remembered for this exact config) and starts them. */
  async trustAll(): Promise<McpServerStatus[]> {
    // Only once startup has said which servers are untrusted; and marked as starting before anything else is
    // awaited, so a second /mcp trust right after this one finds nothing left to start.
    await this.ready;
    const waiting = this.connections.filter((c) => c.state === "untrusted");
    for (const c of waiting) c.state = "connecting";
    await this.opts.trust?.trust(
      this.opts.root,
      waiting.map((c) => c.config),
    );
    this.settled = false;
    this.ready = Promise.all(waiting.map((c) => this.connect(c))).then(() => {
      this.rebuild();
      this.settled = true;
    });
    await this.ready;
    return this.status().filter((s) => waiting.some((c) => c.config.name === s.name));
  }

  status(): McpServerStatus[] {
    return this.connections.map((c) => ({
      name: c.config.name,
      source: c.config.source,
      target: c.config.display ?? describe(c.config),
      reads: c.config.reads ?? [],
      runsProjectFiles: c.config.runsProjectFiles ?? [],
      state: c.state,
      tools: c.tools.map((t) => t.tool.name).filter((name) => !c.skipped?.includes(name)),
      ...(c.skipped?.length ? { skipped: c.skipped } : {}),
      ...(c.error ? { error: c.error } : {}),
      ...(c.stderr.length ? { stderr: c.stderr.join("\n") } : {}),
    }));
  }

  private async connect(c: Connection): Promise<void> {
    if (this.closed) {
      c.state = "failed";
      c.error = "closed";
      return;
    }
    c.state = "connecting";
    c.error = undefined;
    this.onChange();
    const { transport: t } = c.config;
    try {
      const transport =
        t.type === "stdio"
          ? // stderr piped, never inherited: it would scribble over the TUI. The SDK passes only a safe base
            // environment (PATH, HOME, …) plus `env`, so API keys don't reach the server.
            new StdioClientTransport({ command: t.command, args: t.args, env: t.env, cwd: this.cwdFor(c.config), stderr: "pipe" })
          : new StreamableHTTPClientTransport(new URL(t.url), { requestInit: { headers: t.headers } });
      if (transport instanceof StdioClientTransport) {
        transport.stderr?.on("data", (chunk: Buffer) => {
          c.stderr.push(...chunk.toString().split("\n").filter(Boolean));
          c.stderr.splice(0, Math.max(0, c.stderr.length - STDERR_LINES));
        });
      }
      c.transport = transport;
      const client = new Client({ name: "marv", version: this.opts.version });
      c.client = client;
      await withTimeout(client.connect(transport), this.opts.connectTimeoutMs ?? CONNECT_TIMEOUT_MS, "connecting");
      // close() may have been called while this was connecting: don't leave the server running.
      if (this.closed) throw new Error("closed");
      const infos: McpToolInfo[] = [];
      // A server can keep saying there's another page (the same cursor, or an endless list): stop at a cursor
      // seen before or after MAX_PAGES, or startup would never settle.
      const seen = new Set<string>();
      let cursor: string | undefined;
      for (let pages = 0; pages < MAX_PAGES; pages++) {
        const page = await withTimeout(client.listTools(cursor ? { cursor } : undefined), this.opts.connectTimeoutMs ?? CONNECT_TIMEOUT_MS, "listing tools");
        infos.push(...(page.tools as McpToolInfo[]));
        cursor = page.nextCursor;
        if (!cursor || seen.has(cursor)) break;
        seen.add(cursor);
      }
      const timeout = (c.config.timeout ?? DEFAULT_CALL_TIMEOUT_S) * 1000;
      const call = (name: string, args: Record<string, unknown>, signal?: AbortSignal) => {
        if (c.state === "removed") return Promise.reject(new Error("the server was removed with /mcp remove (its tools go away in the next conversation)"));
        return client.callTool({ name, arguments: args }, undefined, {
          signal,
          timeout,
          // A progress report restarts the timeout, so a long tool that keeps saying how far it got isn't cut off.
          // The SDK only asks the server for progress when there's a handler, hence the empty one.
          resetTimeoutOnProgress: true,
          onprogress: () => {},
        }) as Promise<McpCallResult>;
      };
      // A tool listed twice (a server repeating a page) is still one tool.
      const unique = [...new Map(infos.map((info) => [info.name, info])).values()];
      // Removed (/mcp remove) while this was connecting: it stays removed, and the server it started is stopped.
      if ((c.state as Connection["state"]) === "removed") {
        await client.close().catch(() => {});
        this.onChange();
        return;
      }
      c.tools = unique.map((info) => makeMcpTool(c.config, info, call));
      c.state = "connected";
      // A server that exits later: its tools stay listed (the specs can't change), and calls fail with a clear error.
      client.onclose = () => {
        if (c.state === "connected") {
          c.state = "failed";
          c.error = "the server stopped";
          this.onChange();
        }
      };
    } catch (err) {
      if ((c.state as Connection["state"]) !== "removed") {
        c.state = "failed";
        c.error = err instanceof Error ? err.message : String(err);
      }
      await c.transport?.close().catch(() => {});
    }
    this.onChange();
  }

  /** New arrays, so anything holding the old ones keeps a consistent set. Tool names must be unique across servers. */
  private rebuild() {
    const seen = new Set<string>();
    const all = this.connections.flatMap((c) => {
      // Names are unique within a server; across servers the first one keeps a name, and the others say so in /mcp.
      c.skipped = c.tools.filter(({ tool }) => seen.has(tool.name)).map(({ tool }) => tool.name);
      const kept = c.tools.filter(({ tool }) => !seen.has(tool.name));
      for (const { tool } of kept) seen.add(tool.name);
      return kept;
    });
    this.tools = all.map((t) => t.tool);
    this.specs = all.map((t) => t.spec);
  }

  /**
   * Where a local server starts. Yours start in your home folder, never the project: they skip the trust check,
   * and `npx -y <pkg>` would run a <pkg> planted in the project's node_modules (by a command that ran without
   * asking, or a cloned repository) with your permissions. A project's servers were trusted for that project and
   * usually run its files, so they start there. Yours can still get the project as ${MARV_PROJECT_DIR}.
   */
  private cwdFor(config: McpServerConfig): string {
    return config.source === "project" ? this.opts.root : homedir();
  }

  /** Stops every server (on exit). */
  async close(): Promise<void> {
    this.closed = true;
    await Promise.all(this.connections.map((c) => c.client?.close().catch(() => {})));
  }

  /** Last resort on exit, synchronous: local servers must not outlive Marv. */
  kill(): void {
    for (const c of this.connections) {
      const pid = c.transport instanceof StdioClientTransport ? c.transport.pid : null;
      if (pid) {
        try {
          process.kill(pid);
        } catch {}
      }
    }
  }
}
