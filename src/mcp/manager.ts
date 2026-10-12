// The session's MCP connections: starts the servers (yours, and the project's
// once trusted), lists their tools, and keeps them as Marv tools and specs.
//
// The tool list is fixed once the servers have settled (connected or failed):
// every request must send the same tool definitions, or the provider's prompt
// cache starts over. `ready` resolves when they have; the session's turn
// waits for it before the first request. Only the user adds tools later: trusting a project's servers
// (/mcp trust) or adding one (/mcp add). Removing one (/mcp remove) never takes tools away mid-conversation.
//
// A remote server that wants an OAuth login (#18) is "needs-login" until the user runs /mcp login: only that
// command opens a browser or registers a client (see src/mcp/oauth.ts); startup and tool calls use stored tokens.
import { UnauthorizedError } from "@modelcontextprotocol/sdk/client/auth.js";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";
import { StreamableHTTPClientTransport, StreamableHTTPError } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import type { Transport } from "@modelcontextprotocol/sdk/shared/transport.js";
import { homedir } from "node:os";
import type { ToolSpec } from "../provider/types.ts";
import type { Tool } from "../tools/types.ts";
import type { McpServerConfig, McpTransport } from "./config.ts";
import { expectCallback, listen, LoginNeeded, McpOAuthProvider, newState, revoke, type CallbackCheck } from "./oauth.ts";
import { loginKey, type Login, type McpTokenStore } from "./oauth-store.ts";
import { makeMcpTool, type McpCallResult, type McpToolInfo } from "./tool.ts";
import type { McpTrust } from "./trust.ts";

/** Long enough for `npx -y some-server` to download it the first time. */
const CONNECT_TIMEOUT_MS = 60_000;
const DEFAULT_CALL_TIMEOUT_S = 120;
const STDERR_LINES = 20;
/** Pages of tools/list read at most (a server listing hundreds of tools would swamp the context anyway). */
const MAX_PAGES = 50;

/** A login gives up after this long (the user may have walked away from the browser). */
const LOGIN_TIMEOUT_MS = 5 * 60_000;

/**
 * "removed": taken out with /mcp remove; its tools stay listed (the specs can't shrink) and their calls fail.
 * "needs-login": a remote server that wants an OAuth login the user hasn't given (or that ran out): /mcp login.
 */
export type McpState = "untrusted" | "connecting" | "connected" | "failed" | "removed" | "needs-login";

/** A remote server's login, for /mcp: never the token itself. */
export interface McpLoginStatus {
  loggedIn: boolean;
  /** When the access token runs out (ms); the SDK renews it with the refresh token. */
  expiresAt?: number;
  /** The authorization server's host ("log in at github.com"). */
  host?: string;
}

/** What /mcp login hands the UI while it waits: where to send a pasted callback address, and the outcome. */
export interface McpLogin {
  paste(text: string): CallbackCheck;
  done: Promise<McpServerStatus>;
}

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
  /** A remote server that can log in (no Authorization header of its own): whether it is. */
  login?: McpLoginStatus;
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
  login?: McpLoginStatus;
}

/** A server whose config sends its own Authorization header: it logs in with that, never with OAuth. */
const ownAuthorization = (t: McpTransport) => t.type === "http" && Object.keys(t.headers ?? {}).some((k) => k.toLowerCase() === "authorization");

/** What a server that wants a (new) login answers: the SDK's 401s, or the passive provider refusing to open anything. */
const wantsLogin = (err: unknown) => err instanceof LoginNeeded || err instanceof UnauthorizedError || (err instanceof StreamableHTTPError && err.code === 401);

function loginStatus(entry: Login | undefined): McpLoginStatus {
  const expiresIn = entry?.tokens?.expires_in;
  let host: string | undefined;
  try {
    host = entry?.discovery ? new URL(entry.discovery.authorizationServerUrl).host : undefined;
  } catch {}
  return {
    loggedIn: Boolean(entry?.tokens),
    ...(entry?.tokens && entry.savedAt && expiresIn ? { expiresAt: entry.savedAt + expiresIn * 1000 } : {}),
    ...(host ? { host } : {}),
  };
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
  /** The login waiting for its callback (one at a time: a new /mcp login or /mcp logout cancels it). */
  private pending?: { server: string; cancel(err: Error): void };

  constructor(
    servers: McpServerConfig[],
    private readonly opts: {
      root: string;
      version: string;
      trust?: McpTrust;
      connectTimeoutMs?: number;
      /** Where logins are kept (~/.marv/mcp-tokens.json). Without it, servers that want a login just need one. */
      logins?: McpTokenStore;
      loginTimeoutMs?: number;
    },
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

  /** Remote servers that can log in with /mcp login (no Authorization header of their own), by name. */
  loginable(): string[] {
    return this.connections.filter((c) => c.state !== "removed" && this.canLogin(c)).map((c) => c.config.name);
  }

  private canLogin(c: Connection): boolean {
    return Boolean(this.opts.logins) && c.config.transport.type === "http" && !ownAuthorization(c.config.transport);
  }

  /**
   * /mcp login: logs in to a remote server (OAuth, through the SDK) and reconnects it. `authorize` gets the
   * authorization URL to open or show; the browser comes back to a loopback listener, or the user pastes the address
   * it ended up on (`paste`). Throws at once for an unknown server; everything else is `done`'s rejection.
   */
  login(name: string, ui: { authorize(url: URL): void }): McpLogin {
    const c = this.connections.find((x) => x.config.name === name && x.state !== "removed");
    if (!c) throw new Error(`There's no MCP server named "${name}".`);
    this.pending?.cancel(new Error("cancelled: another login started"));
    const state = newState();
    const callback = expectCallback(state);
    const pending = { server: name, cancel: (err: Error) => callback.cancel(err) };
    this.pending = pending;
    const done = this.runLogin(c, ui, state, callback).finally(() => {
      if (this.pending === pending) this.pending = undefined;
    });
    return {
      paste: (text) => {
        try {
          return callback.check(new URL(text.trim()));
        } catch {
          return { ok: false, why: "path" };
        }
      },
      done,
    };
  }

  private async runLogin(c: Connection, ui: { authorize(url: URL): void }, state: string, callback: ReturnType<typeof expectCallback>): Promise<McpServerStatus> {
    const { name } = c.config;
    // Startup must have said which servers are trusted: a project's server gets no network contact before that.
    await this.ready;
    await this.requireTrusted(c);
    if ((c.state as McpState) === "removed") throw new Error(`${name} was removed with /mcp remove.`);
    const t = c.config.transport;
    if (t.type !== "http") throw new Error(`${name} is a local server: only remote (http) servers log in.`);
    if (ownAuthorization(t)) throw new Error(`${name} sends its own Authorization header (in its config): remove it to log in with OAuth instead.`);
    const store = this.opts.logins;
    if (!store) throw new Error("Logins can't be stored here.");
    const key = loginKey({ source: c.config.source, url: t.url }, this.opts.root);
    const stored = await store.get(key);

    // A registered client's redirect URI names its port, and not every server accepts another loopback port.
    const listener = listen((url) => callback.check(url), t.oauth?.callbackPort ?? stored?.client?.port);
    const timeoutMs = this.opts.loginTimeoutMs ?? LOGIN_TIMEOUT_MS;
    const timer = setTimeout(() => callback.cancel(new Error(`the login timed out after ${timeoutMs >= 60_000 ? `${Math.round(timeoutMs / 60_000)} minutes` : `${Math.round(timeoutMs / 1000)} s`}`)), timeoutMs);
    const client = new Client({ name: "marv", version: this.opts.version });
    try {
      if (listener.fellBack && t.oauth?.callbackPort) throw new Error(`port ${t.oauth.callbackPort} (oauth.callbackPort) is taken: close what uses it and try again.`);
      // Its client was registered with the old port: register again with the new redirect URI.
      if (listener.fellBack) await store.update(key, (e) => e && { ...e, client: undefined });
      let redirected = false;
      const provider = new McpOAuthProvider({
        store,
        key,
        server: name,
        mode: "interactive",
        port: listener.port,
        oauth: t.oauth,
        state,
        onAuthorize: (url) => {
          redirected = true;
          ui.authorize(url);
        },
      });
      const transport = new StreamableHTTPClientTransport(new URL(t.url), { authProvider: provider, fetch: provider.fetch, requestInit: { headers: t.headers } });
      // A cancel or timeout ends the wait wherever it is.
      const stopped = callback.code.then(() => new Promise<never>(() => {}));
      try {
        await Promise.race([withTimeout(client.connect(transport), this.opts.connectTimeoutMs ?? CONNECT_TIMEOUT_MS, "connecting"), stopped]);
      } catch (err) {
        if (!(err instanceof UnauthorizedError) || !redirected) throw loginError(err);
      }
      // Connected without asking for a login: the reconnect below is all there is to do.
      if (redirected) {
        const code = await callback.code;
        await transport.finishAuth(code).catch((err) => {
          throw loginError(err);
        });
      }
    } finally {
      clearTimeout(timer);
      listener.close();
      await client.close().catch(() => {});
    }
    return this.reconnect(c);
  }

  /**
   * Refuses a project server the user hasn't trusted: it gets no network contact (login, logout's revocation) and
   * its state isn't touched. Asks the trust store itself, not only the state string another path may have set.
   */
  private async requireTrusted(c: Connection): Promise<void> {
    const trusted = c.state !== "untrusted" && (this.opts.trust ? await this.opts.trust.isTrusted(this.opts.root, c.config).catch(() => false) : true);
    if (!trusted) throw new Error(`${c.config.name} is this project's server and isn't trusted yet: trust it first: /mcp trust`);
  }

  /** Reconnects one server (after a login), as add() starts one: a turn starting meanwhile waits for it. */
  private async reconnect(c: Connection): Promise<McpServerStatus> {
    // The old client stays in c.client until the new one is connected (connect() swaps and closes it), so a call
    // in between never lands on a closed client.
    const before = this.ready;
    this.settled = false;
    let ready: Promise<void> | undefined = undefined;
    ready = (async () => {
      await before;
      await this.connect(c);
      this.rebuild();
      if (this.ready === ready) this.settled = true;
    })();
    this.ready = ready;
    await ready;
    return this.status().find((s) => s.name === c.config.name)!;
  }

  /**
   * /mcp logout: revokes the refresh token where the authorization server can (best effort), forgets the login and
   * disconnects. Its tools stay listed and fail with the login sentence until the next /mcp login.
   */
  async logout(name: string): Promise<{ revoked: "revoked" | "unsupported" | "failed" }> {
    const c = this.connections.find((x) => x.config.name === name && x.state !== "removed");
    if (!c) throw new Error(`There's no MCP server named "${name}".`);
    const t = c.config.transport;
    if (t.type !== "http" || ownAuthorization(t) || !this.opts.logins) throw new Error(`${name} doesn't log in with /mcp login.`);
    await this.ready;
    await this.requireTrusted(c);
    if (this.pending?.server === name) this.pending.cancel(new Error("cancelled: /mcp logout"));
    const key = loginKey({ source: c.config.source, url: t.url }, this.opts.root);
    const entry = await this.opts.logins.get(key);
    if (!entry?.tokens) throw new Error(`${name} isn't logged in.`);
    const revoked = await revoke(entry, t.oauth);
    await this.opts.logins.remove(key);
    c.state = "needs-login";
    c.error = undefined;
    c.login = { loggedIn: false };
    this.onChange();
    const client = c.client;
    c.client = undefined;
    await client?.close().catch(() => {});
    return { revoked };
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
      ...(this.canLogin(c) ? { login: c.login ?? { loggedIn: false } } : {}),
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
      const authProvider = await this.storedLogin(c);
      const transport =
        t.type === "stdio"
          ? // stderr piped, never inherited: it would scribble over the TUI. The SDK passes only a safe base
            // environment (PATH, HOME, …) plus `env`, so API keys don't reach the server.
            new StdioClientTransport({ command: t.command, args: t.args, env: t.env, cwd: this.cwdFor(c.config), stderr: "pipe" })
          : new StreamableHTTPClientTransport(new URL(t.url), { requestInit: { headers: t.headers }, ...(authProvider ? { authProvider, fetch: authProvider.fetch } : {}) });
      if (transport instanceof StdioClientTransport) {
        transport.stderr?.on("data", (chunk: Buffer) => {
          c.stderr.push(...chunk.toString().split("\n").filter(Boolean));
          c.stderr.splice(0, Math.max(0, c.stderr.length - STDERR_LINES));
        });
      }
      c.transport = transport;
      const client = new Client({ name: "marv", version: this.opts.version });
      // c.client is still the previous client (a login's reconnect) until this one is connected and listed.
      const previous = c.client;
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
      const call = async (name: string, args: Record<string, unknown>, signal?: AbortSignal) => {
        if (c.state === "removed") throw new Error("the server was removed with /mcp remove (its tools go away in the next conversation)");
        if (c.state === "needs-login") throw new LoginNeeded(c.config.name);
        // The current client: a login reconnects the server, and tools a running turn already holds use the new one.
        const used = c.client ?? client;
        try {
          return (await used.callTool({ name, arguments: args }, undefined, {
            signal,
            timeout,
            // A progress report restarts the timeout, so a long tool that keeps saying how far it got isn't cut off.
            // The SDK only asks the server for progress when there's a handler, hence the empty one.
            resetTimeoutOnProgress: true,
            onprogress: () => {},
          })) as McpCallResult;
        } catch (err) {
          // The client was replaced or closed under the call (a login's reconnect): Marv's sentence for the state
          // it's in now, never the SDK's "Not connected".
          if (!wantsLogin(err) && (used !== c.client || c.state !== "connected")) throw stateError(c);
          // The token ran out and couldn't be refreshed: the passive provider refused to open a browser. The model
          // reads Marv's sentence, never the server's or the OAuth error's text.
          if (!wantsLogin(err) || ownAuthorization(t)) throw err;
          if (c.state === "connected") {
            c.state = "needs-login";
            c.login = { ...c.login, loggedIn: false };
            this.onChange();
          }
          throw new LoginNeeded(c.config.name);
        }
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
      c.client = client;
      c.state = "connected";
      await previous?.close().catch(() => {});
      // A server that exits later: its tools stay listed (the specs can't change), and calls fail with a clear error.
      client.onclose = () => {
        // Not when a login replaced this client with a new one.
        if (c.state === "connected" && c.client === client) {
          c.state = "failed";
          c.error = "the server stopped";
          this.onChange();
        }
      };
    } catch (err) {
      if ((c.state as Connection["state"]) === "removed") {
        // stays removed
      } else if (wantsLogin(err) && ownAuthorization(t)) {
        c.state = "failed";
        c.error = "the server refused the token in headers";
      } else if (wantsLogin(err)) {
        c.state = "needs-login";
        c.login = { ...c.login, loggedIn: false };
      } else {
        c.state = "failed";
        c.error = err instanceof Error ? err.message : String(err);
      }
      await c.transport?.close().catch(() => {});
      // A login's reconnect that failed: the previous client is no use either.
      if (c.client) {
        const old = c.client;
        c.client = undefined;
        await old.close().catch(() => {});
      }
    }
    this.onChange();
  }

  /**
   * A remote server's stored login, as a passive provider: it refreshes tokens but never opens a browser or registers
   * a client. None without stored tokens, so a server nobody logged in to is never sent discovery or registration
   * requests in the background: its 401 just means "needs login".
   */
  private async storedLogin(c: Connection): Promise<McpOAuthProvider | undefined> {
    const t = c.config.transport;
    const store = this.opts.logins;
    if (t.type !== "http" || ownAuthorization(t) || !store) return undefined;
    const key = loginKey({ source: c.config.source, url: t.url }, this.opts.root);
    const entry = await store.get(key);
    c.login = loginStatus(entry);
    if (!entry?.tokens) return undefined;
    return new McpOAuthProvider({ store, key, server: c.config.name, mode: "passive", port: entry.client?.port, oauth: t.oauth });
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
    this.pending?.cancel(new Error("cancelled: Marv is closing"));
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

/** What a call that lost its client tells the model: the server's state in Marv's words. */
function stateError(c: Connection): Error {
  const { name } = c.config;
  if (c.state === "needs-login") return new LoginNeeded(name);
  if (c.state === "removed") return new Error("the server was removed with /mcp remove (its tools go away in the next conversation)");
  if (c.state === "failed") return new Error(`the server isn't available: ${c.error ?? "it failed"}`);
  return new Error("the server is reconnecting after a login: call it again in a moment.");
}

/**
 * A login's failure, in Marv's words where it can tell what went wrong (the transcript shows it; the model never
 * does). A server without dynamic registration needs a client registered by hand.
 */
function loginError(err: unknown): Error {
  const message = err instanceof Error ? err.message : String(err);
  if (/dynamic client registration/i.test(message)) return new Error("this server needs a registered client: add oauth.clientId to its config (see the README's MCP section)");
  return new Error(`the login failed: ${message.length > 300 ? `${message.slice(0, 300)}…` : message}`);
}
