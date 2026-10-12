// MCP OAuth logins against a fake authorization server (#18): what a background connection may do (never register,
// never open anything), /mcp login and logout, refresh, and two Marv processes sharing one token store.
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { McpOAuthConfig, McpServerConfig } from "../src/mcp/config.ts";
import { McpManager } from "../src/mcp/manager.ts";
import { probeLogin } from "../src/mcp/oauth.ts";
import { loginKey, McpTokenStore } from "../src/mcp/oauth-store.ts";
import { McpTrust } from "../src/mcp/trust.ts";
import type { ToolCall } from "../src/provider/types.ts";
import { runTool } from "../src/tools/index.ts";
import { oauthServer, type OAuthServerOptions } from "./fixtures/oauth-server.ts";

let root: string;
let dir: string;
let store: McpTokenStore;
let managers: McpManager[] = [];
let servers: ReturnType<typeof oauthServer>[] = [];
beforeEach(async () => {
  root = await mkdtemp(join(tmpdir(), "marv-login-root-"));
  dir = await mkdtemp(join(tmpdir(), "marv-login-conf-"));
  store = new McpTokenStore(join(dir, "mcp-tokens.json"));
});
afterEach(async () => {
  await Promise.all(managers.map((m) => m.close()));
  managers = [];
  for (const s of servers) s.close();
  servers = [];
  await rm(root, { recursive: true, force: true });
  await rm(dir, { recursive: true, force: true });
});

function fixture(opts: OAuthServerOptions = {}) {
  const s = oauthServer(opts);
  servers.push(s);
  return s;
}
const remote = (url: string, over: { oauth?: McpOAuthConfig; headers?: Record<string, string>; source?: "personal" | "project"; name?: string } = {}): McpServerConfig => ({
  name: over.name ?? "remote",
  source: over.source ?? "personal",
  key: `k-${over.name ?? "remote"}`,
  transport: { type: "http", url, ...(over.headers ? { headers: over.headers } : {}), ...(over.oauth ? { oauth: over.oauth } : {}) },
});
function manager(list: McpServerConfig[], opts: Partial<ConstructorParameters<typeof McpManager>[1]> = {}) {
  const m = new McpManager(list, { root, version: "9.9.9", logins: store, ...opts });
  managers.push(m);
  return m;
}

/** What a browser does: open the authorization URL, follow its redirect back to the loopback listener. */
async function browse(url: URL) {
  const back = await fetch(url, { redirect: "manual" });
  const location = back.headers.get("location");
  if (!location) throw new Error(`no redirect: ${back.status}`);
  await fetch(location);
}
/** The authorization server's redirect, without following it (a browser on another machine): for pasting. */
async function redirectOf(url: URL) {
  return (await fetch(url, { redirect: "manual" })).headers.get("location")!;
}

let nextId = 0;
async function call(m: McpManager, name: string, args: unknown) {
  const toolCall: ToolCall = { id: `l${nextId++}`, name, arguments: JSON.stringify(args) };
  return runTool(toolCall, { root, approve: async () => "yes" }, m.tools);
}

async function logIn(m: McpManager, name = "remote") {
  const login = m.login(name, { authorize: (url) => void browse(url) });
  return login.done;
}

describe("a server that wants a login, at startup", () => {
  test("without a stored login: needs-login, and nothing is registered or opened", async () => {
    const s = fixture();
    const m = manager([remote(s.url)]);
    const started = Date.now();
    await m.start();
    expect(Date.now() - started).toBeLessThan(5000);
    expect(m.status()[0]).toMatchObject({ state: "needs-login", tools: [] });
    expect(m.status()[0]!.error).toBeUndefined();
    expect(s.paths()).not.toContain("/register");
    expect(s.paths()).not.toContain("/authorize");
    expect(s.paths()).not.toContain("/token");
  });

  test("a server with its own Authorization header that's refused: failed, saying so, not needs-login", async () => {
    const s = fixture();
    const m = manager([remote(s.url, { headers: { Authorization: "Bearer wrong" } })]);
    await m.start();
    expect(m.status()[0]).toMatchObject({ state: "failed" });
    expect(m.status()[0]!.error).toContain("refused the token in headers");
  });
});

describe("/mcp login", () => {
  test("logs in through the browser: connected, tools listed, the store has client (with its port), tokens and discovery", async () => {
    const s = fixture();
    const m = manager([remote(s.url)]);
    await m.start();
    const opened: URL[] = [];
    const login = m.login("remote", { authorize: (url) => (opened.push(url), void browse(url)) });
    const status = await login.done;
    expect(status).toMatchObject({ state: "connected", login: { loggedIn: true, host: expect.stringContaining("127.0.0.1") } });
    expect(m.specs.map((x) => x.name)).toContain("mcp__remote__echo");
    expect(opened).toHaveLength(1);
    expect(opened[0]!.searchParams.get("code_challenge_method")).toBe("S256");
    const saved = await store.get(loginKey({ source: "personal", url: s.url }, root));
    expect(saved?.client?.port).toBeGreaterThan(0);
    expect(saved?.tokens?.access_token).toStartWith("at-");
    expect(saved?.discovery?.authorizationServerUrl).toContain("127.0.0.1");
    expect((await call(m, "mcp__remote__echo", { text: "hi" })).output).toBe("echo: hi");
  });

  test("the same through a pasted callback address", async () => {
    const s = fixture();
    const m = manager([remote(s.url)]);
    await m.start();
    let location: Promise<string> | undefined;
    const login = m.login("remote", { authorize: (url) => void (location = redirectOf(url)) });
    // The browser's address, once the authorization server has sent it back (here, before anything listens).
    while (!location) await Bun.sleep(5);
    expect(login.paste(await location)).toEqual({ ok: true, code: expect.any(String) });
    expect((await login.done).state).toBe("connected");
  });

  test("a restart reuses the login: connects without discovery or a browser", async () => {
    const s = fixture();
    const first = manager([remote(s.url)]);
    await first.start();
    await logIn(first);
    const before = s.requests.length;
    const second = manager([remote(s.url)]);
    await second.start();
    expect(second.status()[0]).toMatchObject({ state: "connected" });
    const after = s.paths().slice(before);
    expect(after.every((p) => p === "/mcp")).toBe(true);
  });

  test("renaming the server keeps the login (it's keyed by URL); a project server with the same URL doesn't get it", async () => {
    const s = fixture();
    const first = manager([remote(s.url)]);
    await first.start();
    await logIn(first);
    const renamed = manager([remote(s.url, { name: "other" }), { ...remote(s.url, { name: "proj", source: "project" }) }]);
    await renamed.start();
    expect(renamed.status().map((x) => x.state)).toEqual(["connected", "needs-login"]);
  });

  test("a second login reuses the registered port; with that port taken, it registers again", async () => {
    const s = fixture();
    const m = manager([remote(s.url)]);
    await m.start();
    await logIn(m);
    const port = (await store.get(loginKey({ source: "personal", url: s.url }, root)))!.client!.port!;
    await logIn(m);
    expect(s.paths().filter((p) => p === "/register")).toHaveLength(1);
    const busy = Bun.serve({ hostname: "127.0.0.1", port, fetch: () => new Response("") });
    try {
      await logIn(m);
    } finally {
      busy.stop(true);
    }
    expect(s.paths().filter((p) => p === "/register")).toHaveLength(2);
    expect((await store.get(loginKey({ source: "personal", url: s.url }, root)))!.client!.port).not.toBe(port);
  });

  test("a server without dynamic registration needs oauth.clientId; with one configured, it never registers", async () => {
    const s = fixture({ dcr: false });
    const m = manager([remote(s.url)]);
    await m.start();
    await expect(logIn(m)).rejects.toThrow("this server needs a registered client: add oauth.clientId to its config");

    const port = 40000 + Math.floor(Math.random() * 20000);
    const pre = fixture({ dcr: false, preRegistered: { clientId: "hand-made", secret: "shh", redirectUri: `http://127.0.0.1:${port}/callback` } });
    const n = manager([remote(pre.url, { oauth: { clientId: "hand-made", clientSecret: "shh", callbackPort: port } })]);
    await n.start();
    expect((await logIn(n)).state).toBe("connected");
    expect(pre.paths()).not.toContain("/register");
    // The configured secret isn't copied into the store.
    expect(JSON.stringify(await store.all())).not.toContain("shh");
  });

  test("oauth.scope is what Marv asks for, overriding the server's", async () => {
    const s = fixture();
    const m = manager([remote(s.url, { oauth: { scope: "mcp:read" } })]);
    await m.start();
    await logIn(m);
    expect(s.scopes).toEqual(["mcp:read"]);
    const t = fixture();
    const n = manager([remote(t.url, { name: "plain" })]);
    await n.start();
    await logIn(n, "plain");
    expect(t.scopes).toEqual(["mcp:read mcp:write"]);
  });

  test("an untrusted project server refuses to log in, and the server sees no request at all", async () => {
    const s = fixture();
    const m = manager([remote(s.url, { source: "project" })], { trust: new McpTrust(join(dir, "trust.json")) });
    await m.start();
    expect(m.status()[0]!.state).toBe("untrusted");
    await expect(logIn(m)).rejects.toThrow("trust it first: /mcp trust");
    expect(s.requests).toEqual([]);
  });

  test("a server that was never started (untrusted) can't be logged out of or in, even with a login stored for it", async () => {
    const s = fixture();
    const config = remote(s.url, { source: "project" });
    const m = manager([config], { trust: new McpTrust(join(dir, "trust.json")) });
    await m.start();
    const key = loginKey({ source: "project", url: s.url }, root);
    await store.update(key, () => ({ server: "remote", tokens: { access_token: "a", refresh_token: "r", token_type: "Bearer" } }));
    await expect(m.logout("remote")).rejects.toThrow("trust it first: /mcp trust");
    expect(m.status()[0]!.state).toBe("untrusted");
    expect(await store.get(key)).toBeDefined();
    // runLogin checks trust itself, not only the state string.
    (m as unknown as { connections: { state: string }[] }).connections[0]!.state = "needs-login";
    await expect(logIn(m)).rejects.toThrow("trust it first: /mcp trust");
    await expect(m.logout("remote")).rejects.toThrow("trust it first: /mcp trust");
    expect(s.requests).toEqual([]);
  });

  test("a token store Marv can't read (a newer format) is never written over: the login fails, saying so", async () => {
    const s = fixture();
    const m = manager([remote(s.url)]);
    await m.start();
    await Bun.write(store.path, '{"v":2,"logins":{}}');
    await expect(logIn(m)).rejects.toThrow(/won't write over it/);
    expect(await Bun.file(store.path).text()).toBe('{"v":2,"logins":{}}');
  });

  test("a call while a login reconnects the server gets an answer or Marv's sentence, never the SDK's closed-client error", async () => {
    const s = fixture();
    const m = manager([remote(s.url)]);
    await m.start();
    await logIn(m);
    const results: Promise<Awaited<ReturnType<typeof call>>>[] = [];
    m.onChange = () => {
      if (m.status()[0]!.state === "connecting") results.push(call(m, "mcp__remote__echo", { text: "during" }));
    };
    await logIn(m);
    expect(results.length).toBeGreaterThan(0);
    for (const r of await Promise.all(results)) {
      expect(["echo: during", "remote's echo failed: the server is reconnecting after a login: call it again in a moment.", "remote needs a new login: the user can run /mcp login remote"]).toContain(r.output);
    }
    expect((await call(m, "mcp__remote__echo", { text: "after" })).output).toBe("echo: after");
  });

  test("a stdio server, one with its own Authorization header, or an unknown name: refused, saying why", async () => {
    const s = fixture();
    const m = manager([
      { name: "local", source: "personal", key: "k", transport: { type: "stdio", command: "true" } },
      remote(s.url, { name: "headed", headers: { authorization: "Bearer x" } }),
    ]);
    await m.start();
    await expect(logIn(m, "local")).rejects.toThrow("only remote (http) servers log in");
    await expect(logIn(m, "headed")).rejects.toThrow("Authorization header");
    expect(() => m.login("nope", { authorize: () => {} })).toThrow('no MCP server named "nope"');
  });

  test("a second login cancels the first; a login times out", async () => {
    const s = fixture();
    const m = manager([remote(s.url)], { loginTimeoutMs: 300 });
    await m.start();
    const first = m.login("remote", { authorize: () => {} });
    const second = m.login("remote", { authorize: () => {} });
    await expect(first.done).rejects.toThrow("cancelled");
    await expect(second.done).rejects.toThrow("timed out");
    expect(m.status()[0]!.state).toBe("needs-login");
  });

  test("a wrong state is refused, and the login keeps waiting for the right one", async () => {
    const s = fixture();
    const m = manager([remote(s.url)]);
    await m.start();
    let location: Promise<string> | undefined;
    const login = m.login("remote", { authorize: (url) => void (location = redirectOf(url)) });
    while (!location) await Bun.sleep(5);
    const real = new URL(await location);
    const forged = new URL(real);
    forged.searchParams.set("state", "guessed");
    expect(login.paste(forged.href)).toEqual({ ok: false, why: "state" });
    expect(login.paste(real.href).ok).toBe(true);
    expect((await login.done).state).toBe("connected");
  });
});

describe("refresh", () => {
  test("an expired access token is refreshed transparently, and the store holds the new tokens", async () => {
    const s = fixture();
    const m = manager([remote(s.url)]);
    await m.start();
    await logIn(m);
    const key = loginKey({ source: "personal", url: s.url }, root);
    const old = (await store.get(key))!.tokens!;
    s.expire();
    expect((await call(m, "mcp__remote__echo", { text: "again" })).output).toBe("echo: again");
    const now = (await store.get(key))!.tokens!;
    expect(now.access_token).not.toBe(old.access_token);
    expect(now.refresh_token).not.toBe(old.refresh_token);
  });

  test("a refused refresh mid-call: the model reads Marv's sentence, the server needs a login, nothing is opened", async () => {
    const s = fixture();
    const m = manager([remote(s.url)]);
    await m.start();
    await logIn(m);
    let changes = 0;
    m.onChange = () => void changes++;
    const authorizes = s.paths().filter((p) => p === "/authorize").length;
    s.expire();
    s.refuseRefresh();
    const result = await call(m, "mcp__remote__echo", { text: "x" });
    expect(result.isError).toBe(true);
    expect(result.output).toBe("remote needs a new login: the user can run /mcp login remote");
    expect(m.status()[0]!.state).toBe("needs-login");
    expect(changes).toBeGreaterThan(0);
    expect(s.paths().filter((p) => p === "/authorize")).toHaveLength(authorizes);
    // Its tools stay listed (the tool list is fixed), and the next call fails the same way without asking the server.
    expect(m.tools.map((t) => t.name)).toContain("mcp__remote__echo");
    const before = s.requests.length;
    expect((await call(m, "mcp__remote__echo", { text: "y" })).output).toBe("remote needs a new login: the user can run /mcp login remote");
    expect(s.requests.length).toBe(before);
    // A login brings it back.
    await logIn(m);
    expect((await call(m, "mcp__remote__echo", { text: "z" })).output).toBe("echo: z");
  });

  test("two managers on one store survive a rotating refresh", async () => {
    const s = fixture({ rotate: true });
    const a = manager([remote(s.url)]);
    await a.start();
    await logIn(a);
    const b = manager([remote(s.url)], { logins: new McpTokenStore(store.path) });
    await b.start();
    expect(b.status()[0]!.state).toBe("connected");
    s.expire();
    // A refreshes (rotating the refresh token B read); B's next call still works.
    expect((await call(a, "mcp__remote__echo", { text: "a" })).output).toBe("echo: a");
    expect((await call(b, "mcp__remote__echo", { text: "b" })).output).toBe("echo: b");
    s.expire();
    expect((await call(b, "mcp__remote__echo", { text: "b2" })).output).toBe("echo: b2");
    expect((await call(a, "mcp__remote__echo", { text: "a2" })).output).toBe("echo: a2");
  });
});

describe("/mcp logout", () => {
  test("revokes the refresh token, forgets the login, and the next call needs a login", async () => {
    const s = fixture();
    const m = manager([remote(s.url)]);
    await m.start();
    await logIn(m);
    const key = loginKey({ source: "personal", url: s.url }, root);
    const refresh = (await store.get(key))!.tokens!.refresh_token!;
    expect(await m.logout("remote")).toEqual({ revoked: "revoked" });
    expect(s.revoked).toEqual([refresh]);
    expect(await store.get(key)).toBeUndefined();
    expect(m.status()[0]!.state).toBe("needs-login");
    expect((await call(m, "mcp__remote__echo", { text: "x" })).output).toBe("remote needs a new login: the user can run /mcp login remote");
  });

  test("a server without a revocation endpoint still logs out, and says the grant remains", async () => {
    const s = fixture({ revocation: false });
    const m = manager([remote(s.url)]);
    await m.start();
    await logIn(m);
    expect(await m.logout("remote")).toEqual({ revoked: "unsupported" });
    expect(await store.get(loginKey({ source: "personal", url: s.url }, root))).toBeUndefined();
  });

  test("logging out of a server that isn't logged in says so", async () => {
    const s = fixture();
    const m = manager([remote(s.url)]);
    await m.start();
    await expect(m.logout("remote")).rejects.toThrow("isn't logged in");
  });
});

describe("probeLogin", () => {
  test("true for a server that wants a login, false for one that doesn't, false for a dead port", async () => {
    const s = fixture();
    const open = fixture({ open: true });
    expect(await probeLogin(s.url)).toBe(true);
    expect(await probeLogin(open.url)).toBe(false);
    const dead = Bun.serve({ port: 0, fetch: () => new Response("") });
    const port = dead.port;
    dead.stop(true);
    expect(await probeLogin(`http://127.0.0.1:${port}/mcp`, { timeoutMs: 2000 })).toBe(false);
    expect(s.paths()).not.toContain("/register");
  });
});
