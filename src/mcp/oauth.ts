// Logging in to a remote MCP server that wants OAuth (#18). The MCP SDK runs the protocol (discovery, dynamic
// registration, PKCE, the code exchange and refreshes, through StreamableHTTPClientTransport's `authProvider`);
// Marv supplies the provider below, backed by its token store, plus the loopback listener the browser comes back
// to and the browser opener.
//
// The rule the provider enforces: only the user starts a login (/mcp login). Startup, tool calls and the SDK use a
// *passive* provider, which refreshes tokens but never opens anything and never registers a client; an
// *interactive* one exists only while /mcp login runs.
import { discoverOAuthProtectedResourceMetadata } from "@modelcontextprotocol/sdk/client/auth.js";
import type { OAuthClientProvider, OAuthDiscoveryState } from "@modelcontextprotocol/sdk/client/auth.js";
import type { OAuthClientInformationMixed, OAuthClientMetadata, OAuthTokens } from "@modelcontextprotocol/sdk/shared/auth.js";
import { randomBytes, timingSafeEqual } from "node:crypto";
import type { McpOAuthConfig } from "./config.ts";
import type { Login, McpTokenStore } from "./oauth-store.ts";

const REPOSITORY = "https://github.com/FlologixAI/marv";
const LOOPBACK = new Set(["127.0.0.1", "localhost", "[::1]"]);

/**
 * A server needs the user to log in (again). Its message is Marv's own sentence, never a server's or an OAuth
 * error body: it's what the model reads when a tool call fails.
 */
export class LoginNeeded extends Error {
  constructor(readonly server: string) {
    super(`${server} needs a new login: the user can run /mcp login ${server}`);
    this.name = "LoginNeeded";
  }
}

/** A login's `state`: 32 random bytes, so a callback without it (another page, a sandboxed command) is refused. */
export const newState = () => randomBytes(32).toString("base64url");

/** The redirect URI a login on `port` names: 127.0.0.1, not localhost (RFC 8252 §8.3: a name can resolve elsewhere). */
export const redirectUrlFor = (port: number | undefined) => `http://127.0.0.1${port ? `:${port}` : ""}/callback`;

/** An authorization URL Marv will open or show: https, or http on a loopback host (the tests' fake server). */
export function authorizationUrlOk(url: URL): boolean {
  return url.protocol === "https:" || (url.protocol === "http:" && LOOPBACK.has(url.hostname));
}

export interface ProviderOptions {
  store: McpTokenStore;
  /** The store key (loginKey: owner + URL). */
  key: string;
  /** The server's name, for messages and the store's display field. */
  server: string;
  mode: "passive" | "interactive";
  /** The loopback port this login listens on (interactive). */
  port?: number;
  /** The config's oauth block: a client registered by hand, its port, a scope. */
  oauth?: McpOAuthConfig;
  /** The login's `state` (interactive: the login checks the callback against it); random when not given. */
  state?: string;
  /** Interactive: the authorization URL, for the login UI to open or show. */
  onAuthorize?: (url: URL) => void;
  /**
   * Interactive: the login's cancellation (/mcp remove, /mcp logout, another login, closing). Once aborted, nothing
   * is saved: a token exchange that finishes late must not undo a logout or log a removed server back in.
   */
  signal?: AbortSignal;
}

export class McpOAuthProvider implements OAuthClientProvider {
  private verifier?: string;
  /** The refresh token tokens() last handed out: what invalidateCredentials drops when it can't tell which failed. */
  private lastRefresh?: string;
  /** The refresh token the last refused refresh request sent (seen by `fetch`): the one invalidateCredentials drops. */
  private failedRefresh?: string;
  /** Interactive: the tokens this login got. A login starts afresh rather than reusing (or refreshing) stored ones. */
  private fresh?: OAuthTokens;
  private readonly stateValue: string;

  constructor(private readonly opts: ProviderOptions) {
    this.stateValue = opts.state ?? newState();
  }

  private get port(): number | undefined {
    return this.opts.oauth?.callbackPort ?? this.opts.port;
  }

  // Always defined: an undefined redirectUrl makes the SDK try a non-interactive (client credentials) grant.
  get redirectUrl(): string {
    return redirectUrlFor(this.port);
  }

  get clientMetadata(): OAuthClientMetadata {
    return {
      client_name: "Marv",
      client_uri: REPOSITORY,
      redirect_uris: [this.redirectUrl],
      grant_types: ["authorization_code", "refresh_token"],
      response_types: ["code"],
      // A public client: nothing secret can live in a program the user runs, PKCE stands in for it.
      token_endpoint_auth_method: this.opts.oauth?.clientSecret ? "client_secret_post" : "none",
      ...(this.opts.oauth?.scope ? { scope: this.opts.oauth.scope } : {}),
    };
  }

  /** The login's `state`: random, and the same for the whole login, so the callback can be checked against it. */
  state(): string {
    return this.stateValue;
  }

  async clientInformation(): Promise<OAuthClientInformationMixed | undefined> {
    const { oauth } = this.opts;
    if (oauth?.clientId) {
      return { client_id: oauth.clientId, ...(oauth.clientSecret ? { client_secret: oauth.clientSecret, token_endpoint_auth_method: "client_secret_post" } : {}) };
    }
    const client = (await this.opts.store.get(this.opts.key))?.client;
    if (!client) return undefined;
    const { port: _, ...info } = client;
    return info as OAuthClientInformationMixed;
  }

  saveClientInformation(info: OAuthClientInformationMixed): void | Promise<void> {
    // A configured client lives in the config (its secret too): nothing to store.
    if (this.opts.oauth?.clientId) return;
    // A background connection never registers Marv with a server the user didn't log in to.
    if (this.opts.mode === "passive") throw new LoginNeeded(this.opts.server);
    return this.opts.store.update(this.opts.key, (e) => {
      this.opts.signal?.throwIfAborted();
      return { ...e, server: this.opts.server, client: { ...info, port: this.port } } as Login;
    });
  }

  /** Read from the file every time: another Marv process may have refreshed (and rotated) the tokens. */
  async tokens(): Promise<OAuthTokens | undefined> {
    if (this.opts.mode === "interactive") return this.fresh;
    const tokens = (await this.opts.store.get(this.opts.key))?.tokens;
    this.lastRefresh = tokens?.refresh_token;
    return tokens as OAuthTokens | undefined;
  }

  saveTokens(tokens: OAuthTokens): Promise<void> {
    // Checked inside the update too: the abort may come while it waits for the file's lock.
    if (this.opts.signal?.aborted) return Promise.reject(this.opts.signal.reason);
    if (this.opts.mode === "interactive") this.fresh = tokens;
    return this.opts.store.update(this.opts.key, (e) => {
      this.opts.signal?.throwIfAborted();
      return { ...e, server: this.opts.server, tokens, savedAt: Date.now() } as Login;
    });
  }

  redirectToAuthorization(url: URL): void {
    if (this.opts.mode === "passive" || !this.opts.onAuthorize) throw new LoginNeeded(this.opts.server);
    if (!authorizationUrlOk(url)) throw new Error(`the authorization server sent a login address Marv won't open (${url.protocol}//${url.host})`);
    const scope = this.opts.oauth?.scope;
    if (scope) {
      url.searchParams.set("scope", scope);
      if (scope.split(/\s+/).includes("offline_access") && !url.searchParams.has("prompt")) url.searchParams.set("prompt", "consent");
    }
    this.opts.onAuthorize(url);
  }

  /**
   * The transport's fetch (requests to the server and to the authorization server): it notes which refresh token a
   * refused refresh sent. Two refreshes can run at once (the server's event stream and a request both get a 401):
   * the one that sent the stale token fails, and must not delete the tokens the other one just saved.
   */
  readonly fetch = async (input: string | URL | Request, init?: RequestInit): Promise<Response> => {
    const body = init?.body;
    const sent = body instanceof URLSearchParams && body.get("grant_type") === "refresh_token" ? body.get("refresh_token") : null;
    const response = await fetch(input, init);
    if (sent && !response.ok) this.failedRefresh = sent;
    return response;
  };

  /** In memory only: a login lives in one process, and the verifier is what makes a stolen code useless. */
  saveCodeVerifier(verifier: string): void {
    this.verifier = verifier;
  }

  codeVerifier(): string {
    if (!this.verifier) throw new Error("no login is in progress");
    return this.verifier;
  }

  async discoveryState(): Promise<OAuthDiscoveryState | undefined> {
    return (await this.opts.store.get(this.opts.key))?.discovery as OAuthDiscoveryState | undefined;
  }

  saveDiscoveryState(state: OAuthDiscoveryState): Promise<void> {
    return this.opts.store.update(this.opts.key, (e) => ({ ...e, server: this.opts.server, discovery: state }) as Login);
  }

  async invalidateCredentials(scope: "all" | "client" | "tokens" | "verifier" | "discovery"): Promise<void> {
    if (scope === "verifier") {
      this.verifier = undefined;
      return;
    }
    const failed = this.failedRefresh ?? this.lastRefresh;
    this.failedRefresh = undefined;
    await this.opts.store.update(this.opts.key, (e) => {
      if (!e) return e;
      const next = { ...e };
      if (scope === "all" || scope === "client") delete next.client;
      if (scope === "all" || scope === "discovery") delete next.discovery;
      // Another process may have refreshed since this one read the tokens: its newer refresh token is kept, and
      // the SDK's retry refreshes with it. Only the one that failed here is dropped.
      if (scope === "all" || (scope === "tokens" && e.tokens?.refresh_token === failed)) {
        delete next.tokens;
        delete next.savedAt;
      }
      return next;
    });
  }
}

export type CallbackCheck = { ok: true; code: string } | { ok: false; why: string };

/**
 * What a login waits for: one callback carrying its `state`. `check` decides about each address that arrives (from
 * the browser, or pasted), and `code` resolves with the first valid one's code (or rejects with the server's error).
 */
export function expectCallback(state: string) {
  let resolve!: (code: string) => void;
  let reject!: (err: Error) => void;
  const code = new Promise<string>((res, rej) => ((resolve = res), (reject = rej)));
  code.catch(() => {}); // a login nobody waits for any more mustn't be an unhandled rejection
  let used = false;
  const expected = Buffer.from(state);
  const matches = (got: string | null) => {
    if (got === null) return false;
    const b = Buffer.from(got);
    return b.length === expected.length && timingSafeEqual(b, expected);
  };
  return {
    code,
    check(url: URL): CallbackCheck {
      if (url.pathname !== "/callback") return { ok: false, why: "path" };
      // Without the state, nothing a sandboxed command (which shares the network) or another page sends counts.
      if (!matches(url.searchParams.get("state"))) return { ok: false, why: "state" };
      if (used) return { ok: false, why: "used" };
      used = true;
      const error = url.searchParams.get("error");
      if (error !== null) {
        // The server's text goes to the screen only as a short code (access_denied, …).
        const why = /^[A-Za-z0-9_.-]{1,64}$/.test(error) ? error : "error";
        reject(new Error(`the authorization server refused the login (${why})`));
        return { ok: false, why };
      }
      const got = url.searchParams.get("code");
      if (!got) {
        reject(new Error("the authorization server came back without a code"));
        return { ok: false, why: "no code" };
      }
      resolve(got);
      return { ok: true, code: got };
    },
    cancel(err: Error) {
      used = true;
      reject(err);
    },
  };
}

/**
 * A pasted message that's a login callback address (the browser's address bar after the redirect failed, on another
 * machine): the App hands it to the login instead of sending it to the model. Only the address alone counts.
 */
export function isCallback(text: string): boolean {
  const trimmed = text.trim();
  if (!trimmed || /\s/.test(trimmed)) return false;
  let url: URL;
  try {
    url = new URL(trimmed);
  } catch {
    return false;
  }
  return url.protocol === "http:" && LOOPBACK.has(url.hostname) && url.pathname === "/callback" && url.searchParams.has("state") && (url.searchParams.has("code") || url.searchParams.has("error"));
}

const PAGE = (title: string, text: string) =>
  `<!doctype html><meta charset="utf-8"><title>Marv: ${title}</title><body style="font-family:sans-serif;margin:3em"><h1>${title}</h1><p>${text}</p>`;

/** What the browser is shown. Nothing from the request is echoed back. */
export function callbackPage(check: CallbackCheck): Response {
  const html = { "content-type": "text/html; charset=utf-8" };
  if (check.ok) return new Response(PAGE("Logged in", "You can close this tab and go back to Marv."), { headers: html });
  if (check.why === "path") return new Response("Not found", { status: 404 });
  if (check.why === "state" || check.why === "used") return new Response(PAGE("Not this login", "This isn't the login Marv is waiting for."), { status: 400, headers: html });
  return new Response(PAGE("Login failed", "The login was refused. Marv says why; you can close this tab."), { status: 400, headers: html });
}

/**
 * The loopback listener the browser is sent back to: 127.0.0.1 only, `/callback` only. A preferred port (the one a
 * client was registered with) is tried first; if it's taken, any free one, and `fellBack` says so (the client then
 * has to be registered again with the new redirect URI).
 */
export function listen(check: (url: URL) => CallbackCheck, preferred?: number): { port: number; fellBack: boolean; close(): void } {
  const serve = (port: number) => Bun.serve({ hostname: "127.0.0.1", port, fetch: (request) => callbackPage(check(new URL(request.url))) });
  let server: ReturnType<typeof serve>;
  let fellBack = false;
  if (preferred) {
    try {
      server = serve(preferred);
    } catch {
      server = serve(0);
      fellBack = true;
    }
  } else {
    server = serve(0);
  }
  return { port: server.port!, fellBack, close: () => void server.stop(true) };
}

/** Whether a browser can open here: not over SSH (it would open on the far machine, or nowhere), not without a display. */
export function canOpenBrowser(env: Record<string, string | undefined>, platform: string): boolean {
  if (env.SSH_CONNECTION || env.SSH_TTY) return false;
  if (platform === "darwin") return true;
  if (platform === "linux") return Boolean(env.DISPLAY || env.WAYLAND_DISPLAY);
  return false;
}

/** Opens the URL in the browser: the URL is one argument, never through a shell. Failures are ignored: the link is shown anyway. */
export function openBrowser(url: URL, platform: string = process.platform, spawn: (argv: string[]) => void = defaultSpawn): void {
  try {
    spawn([platform === "darwin" ? "open" : "xdg-open", url.href]);
  } catch {
    // no opener installed: the transcript has the link
  }
}

function defaultSpawn(argv: string[]) {
  const child = Bun.spawn(argv, { stdin: "ignore", stdout: "ignore", stderr: "ignore" });
  child.unref();
}

/**
 * Whether a remote server would ask for a login (for /mcp add, before it's installed): an unauthenticated read of
 * its protected-resource metadata (RFC 9728). Never registers, never throws, gives up after `timeoutMs`.
 */
export async function probeLogin(
  url: string,
  { timeoutMs = 5000, fetchFn = fetch, signal: caller }: { timeoutMs?: number; fetchFn?: typeof fetch; signal?: AbortSignal } = {},
): Promise<boolean> {
  // The caller's signal too (Esc during /mcp add), not only the timeout.
  const signal = caller ? AbortSignal.any([AbortSignal.timeout(timeoutMs), caller]) : AbortSignal.timeout(timeoutMs);
  const limited = ((input: string | URL | Request, init?: RequestInit) => fetchFn(input, { ...init, signal })) as typeof fetch;
  try {
    const meta = await discoverOAuthProtectedResourceMetadata(url, undefined, limited);
    return (meta.authorization_servers?.length ?? 0) > 0;
  } catch {
    return false;
  }
}

/**
 * Revokes a login's refresh token (RFC 7009), best effort: "revoked", "unsupported" (the server lists no revocation
 * endpoint, so the grant stays on its side) or "failed".
 */
export async function revoke(login: Login, oauth: McpOAuthConfig | undefined, { timeoutMs = 5000 } = {}): Promise<"revoked" | "unsupported" | "failed"> {
  const meta = (login.discovery as { authorizationServerMetadata?: { revocation_endpoint?: string } } | undefined)?.authorizationServerMetadata;
  const endpoint = meta?.revocation_endpoint;
  const token = login.tokens?.refresh_token ?? login.tokens?.access_token;
  if (!endpoint || !token) return "unsupported";
  let target: URL;
  try {
    target = new URL(endpoint);
  } catch {
    return "failed";
  }
  if (!authorizationUrlOk(target)) return "failed";
  const clientId = oauth?.clientId ?? login.client?.client_id;
  const body = new URLSearchParams({ token, token_type_hint: login.tokens?.refresh_token ? "refresh_token" : "access_token" });
  if (clientId) body.set("client_id", clientId);
  const secret = oauth?.clientId ? oauth.clientSecret : (login.client?.client_secret as string | undefined);
  if (secret) body.set("client_secret", secret);
  try {
    const response = await fetch(target, { method: "POST", body, redirect: "manual", signal: AbortSignal.timeout(timeoutMs) });
    await response.body?.cancel();
    return response.ok ? "revoked" : "failed";
  } catch {
    return "failed";
  }
}
