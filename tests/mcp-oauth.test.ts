import { describe, expect, test } from "bun:test";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { McpOAuthConfig } from "../src/mcp/config.ts";
import { callbackPage, canOpenBrowser, expectCallback, isCallback, listen, LoginNeeded, McpOAuthProvider, openBrowser } from "../src/mcp/oauth.ts";
import { McpTokenStore } from "../src/mcp/oauth-store.ts";

const KEY = "personal https://x.test/mcp";
const newStore = () => new McpTokenStore(join(mkdtempSync(join(tmpdir(), "marv-oauth-")), "mcp-tokens.json"));

function provider(opts: { mode: "passive" | "interactive"; port?: number; oauth?: McpOAuthConfig; store?: McpTokenStore; onAuthorize?: (url: URL) => void }) {
  return new McpOAuthProvider({ store: opts.store ?? newStore(), key: KEY, server: "x", mode: opts.mode, port: opts.port, oauth: opts.oauth, onAuthorize: opts.onAuthorize });
}

describe("McpOAuthProvider", () => {
  test("redirectUrl is always a 127.0.0.1 loopback URL; clientMetadata is a public client", () => {
    const p = provider({ mode: "interactive", port: 50214 });
    expect(String(p.redirectUrl)).toBe("http://127.0.0.1:50214/callback");
    expect(p.clientMetadata).toMatchObject({
      client_name: "Marv",
      redirect_uris: ["http://127.0.0.1:50214/callback"],
      grant_types: ["authorization_code", "refresh_token"],
      response_types: ["code"],
      token_endpoint_auth_method: "none",
    });
    // Passive, with no port known: still defined (undefined would make the SDK try a non-interactive grant).
    expect(provider({ mode: "passive" }).redirectUrl).toBeDefined();
  });

  test("passive: redirectToAuthorization throws LoginNeeded and saveClientInformation refuses", async () => {
    const store = newStore();
    const p = provider({ mode: "passive", store });
    expect(() => p.redirectToAuthorization(new URL("https://as.test/authorize"))).toThrow(LoginNeeded);
    expect(() => p.saveClientInformation({ client_id: "x" })).toThrow(LoginNeeded);
    expect(await store.get(KEY)).toBeUndefined();
  });

  test("LoginNeeded is Marv's own sentence, naming the command", () => {
    expect(new LoginNeeded("github").message).toBe("github needs a new login: the user can run /mcp login github");
  });

  test("interactive: hands an https URL to the UI, refuses anything else", async () => {
    const seen: URL[] = [];
    const p = provider({ mode: "interactive", port: 1, onAuthorize: (u) => seen.push(u) });
    await p.redirectToAuthorization(new URL("https://as.test/authorize?x=1"));
    await p.redirectToAuthorization(new URL("http://127.0.0.1:4000/authorize"));
    expect(seen).toHaveLength(2);
    expect(() => p.redirectToAuthorization(new URL("file:///etc/passwd"))).toThrow();
    expect(() => p.redirectToAuthorization(new URL("http://evil.test/authorize"))).toThrow();
    expect(() => p.redirectToAuthorization(new URL("javascript:alert(1)"))).toThrow();
    expect(seen).toHaveLength(2);
  });

  test("interactive: a registered client is stored with the port its redirect URI names", async () => {
    const store = newStore();
    const p = provider({ mode: "interactive", port: 50214, store });
    await p.saveClientInformation({ client_id: "dyn", redirect_uris: ["http://127.0.0.1:50214/callback"] });
    expect((await store.get(KEY))?.client).toMatchObject({ client_id: "dyn", port: 50214 });
    expect(await p.clientInformation()).toEqual({ client_id: "dyn", redirect_uris: ["http://127.0.0.1:50214/callback"] });
  });

  test("config oauth: clientInformation is the configured client; a secret switches to client_secret_post; a fixed port", async () => {
    const plain = provider({ mode: "passive", oauth: { clientId: "c" } });
    expect(await plain.clientInformation()).toEqual({ client_id: "c" });
    const secret = provider({ mode: "interactive", port: 9, oauth: { clientId: "c", clientSecret: "s", callbackPort: 33418 } });
    expect(await secret.clientInformation()).toEqual({ client_id: "c", client_secret: "s", token_endpoint_auth_method: "client_secret_post" });
    expect(String(secret.redirectUrl)).toBe("http://127.0.0.1:33418/callback");
    expect(secret.clientMetadata.token_endpoint_auth_method).toBe("client_secret_post");
    // Saving it (the SDK stamps it with the issuer) doesn't store the secret.
    await secret.saveClientInformation({ client_id: "c", client_secret: "s" });
  });

  test("config scope overrides the scope in the authorization URL", async () => {
    const seen: URL[] = [];
    const p = provider({ mode: "interactive", port: 1, oauth: { scope: "repo read:org" }, onAuthorize: (u) => seen.push(u) });
    expect(p.clientMetadata.scope).toBe("repo read:org");
    await p.redirectToAuthorization(new URL("https://as.test/authorize?scope=everything&state=s"));
    expect(seen[0]!.searchParams.get("scope")).toBe("repo read:org");
    expect(seen[0]!.searchParams.get("state")).toBe("s");
    // Without it, the server's scope stands.
    const q = provider({ mode: "interactive", port: 1, onAuthorize: (u) => seen.push(u) });
    await q.redirectToAuthorization(new URL("https://as.test/authorize?scope=everything"));
    expect(seen[1]!.searchParams.get("scope")).toBe("everything");
  });

  test("tokens() re-reads the store; saveTokens records when", async () => {
    const store = newStore();
    const p = provider({ mode: "passive", store });
    expect(await p.tokens()).toBeUndefined();
    await store.update(KEY, () => ({ server: "x", tokens: { access_token: "a1", token_type: "Bearer", refresh_token: "r1" } }));
    expect((await p.tokens())?.access_token).toBe("a1");
    const before = Date.now();
    await p.saveTokens({ access_token: "a2", token_type: "Bearer", refresh_token: "r2", expires_in: 60 });
    const saved = await store.get(KEY);
    expect(saved?.tokens?.access_token).toBe("a2");
    expect(saved?.savedAt).toBeGreaterThanOrEqual(before);
  });

  test("invalidateCredentials('tokens') keeps a refresh token another process saved meanwhile", async () => {
    const store = newStore();
    await store.update(KEY, () => ({ server: "x", tokens: { access_token: "a1", token_type: "Bearer", refresh_token: "r1" } }));
    const p = provider({ mode: "passive", store });
    expect((await p.tokens())?.refresh_token).toBe("r1");
    // Another Marv refreshed (rotating r1 → r2) after this one read r1.
    await store.update(KEY, (e) => ({ ...e!, tokens: { access_token: "a2", token_type: "Bearer", refresh_token: "r2" } }));
    await p.invalidateCredentials("tokens");
    expect((await p.tokens())?.refresh_token).toBe("r2");
    // Now r2 is the one that failed: it goes.
    await p.invalidateCredentials("tokens");
    expect(await p.tokens()).toBeUndefined();
    expect((await store.get(KEY))?.server).toBe("x");
  });

  test("two refreshes at once (the SSE stream and a request both got a 401): the stale one's failure keeps the newer tokens", async () => {
    const store = newStore();
    await store.update(KEY, () => ({ server: "x", tokens: { access_token: "a1", token_type: "Bearer", refresh_token: "r1" } }));
    const p = provider({ mode: "passive", store });
    await p.tokens(); // both attempts read r1
    await p.saveTokens({ access_token: "a2", token_type: "Bearer", refresh_token: "r2" }); // the first one rotated it
    await p.tokens(); // a request's header read r2 meanwhile
    // The second attempt posts r1, which the server refuses.
    const server = Bun.serve({ port: 0, fetch: () => Response.json({ error: "invalid_grant" }, { status: 400 }) });
    try {
      const response = await p.fetch(`http://127.0.0.1:${server.port}/token`, { method: "POST", body: new URLSearchParams({ grant_type: "refresh_token", refresh_token: "r1" }) });
      expect(response.status).toBe(400);
    } finally {
      server.stop(true);
    }
    await p.invalidateCredentials("tokens");
    expect((await store.get(KEY))?.tokens?.refresh_token).toBe("r2");
  });

  test("the PKCE verifier stays in memory; discovery state is stored", async () => {
    const store = newStore();
    const p = provider({ mode: "interactive", port: 1, store });
    expect(() => p.codeVerifier()).toThrow();
    p.saveCodeVerifier("v");
    expect(p.codeVerifier()).toBe("v");
    await p.saveDiscoveryState({ authorizationServerUrl: "https://as.test/" });
    expect(await provider({ mode: "passive", store }).discoveryState()).toEqual({ authorizationServerUrl: "https://as.test/" });
    expect(JSON.stringify(await store.get(KEY))).not.toContain('"v"');
  });

  test("state() is random and stable for one login", () => {
    const p = provider({ mode: "interactive", port: 1 });
    expect(p.state()).toBe(p.state());
    expect(p.state()).not.toBe(provider({ mode: "interactive", port: 1 }).state());
    expect(p.state().length).toBeGreaterThanOrEqual(40);
  });
});

describe("the callback", () => {
  const at = (q: string) => new URL(`http://127.0.0.1:1/callback?${q}`);

  test("accepts the matching state once, rejects a wrong state, reports error=", async () => {
    const cb = expectCallback("s3cret");
    expect(cb.check(at("code=c&state=nope"))).toEqual({ ok: false, why: "state" });
    expect(cb.check(at("code=c"))).toEqual({ ok: false, why: "state" });
    expect(cb.check(new URL("http://127.0.0.1:1/other?code=c&state=s3cret"))).toEqual({ ok: false, why: "path" });
    expect(cb.check(at("code=c&state=s3cret"))).toEqual({ ok: true, code: "c" });
    expect(cb.check(at("code=c&state=s3cret"))).toEqual({ ok: false, why: "used" });
    expect(await cb.code).toBe("c");

    const refused = expectCallback("s3cret");
    expect(refused.check(at("error=access_denied&state=s3cret"))).toEqual({ ok: false, why: "access_denied" });
    await expect(refused.code).rejects.toThrow("access_denied");
    // The server's error text is shown only as a short code: nothing it sends can draw on the screen.
    const odd = expectCallback("s");
    expect(odd.check(at(`error=${encodeURIComponent("\u001b]52;c;x\u0007bad")}&state=s`))).toEqual({ ok: false, why: "error" });
    await expect(odd.code).rejects.toThrow();
  });

  test("isCallback recognizes a pasted callback address (and nothing else)", () => {
    expect(isCallback("http://127.0.0.1:50214/callback?code=abc&state=xyz")).toBe(true);
    expect(isCallback("  http://127.0.0.1:50214/callback?code=abc&state=xyz\n")).toBe(true);
    expect(isCallback("http://localhost:50214/callback?code=abc&state=xyz")).toBe(true);
    expect(isCallback("http://127.0.0.1:50214/callback?error=access_denied&state=xyz")).toBe(true);
    expect(isCallback("look at http://127.0.0.1:50214/callback?code=abc&state=xyz")).toBe(false);
    expect(isCallback("https://example.com/callback?code=abc&state=xyz")).toBe(false);
    expect(isCallback("http://127.0.0.1:50214/other?code=abc&state=xyz")).toBe(false);
    expect(isCallback("http://127.0.0.1:50214/callback")).toBe(false);
    expect(isCallback("hello")).toBe(false);
  });

  test("the listener answers the browser and hands over the code; another path is 404; a wrong state is refused", async () => {
    const cb = expectCallback("st");
    const listener = listen((url) => cb.check(url));
    try {
      expect(listener.port).toBeGreaterThan(0);
      expect((await fetch(`http://127.0.0.1:${listener.port}/elsewhere`)).status).toBe(404);
      const wrong = await fetch(`http://127.0.0.1:${listener.port}/callback?code=x&state=other`);
      expect(wrong.status).toBe(400);
      const ok = await fetch(`http://127.0.0.1:${listener.port}/callback?code=good&state=st`);
      expect(ok.status).toBe(200);
      expect(await ok.text()).toContain("You can close this tab");
      expect(await cb.code).toBe("good");
    } finally {
      listener.close();
    }
  });

  test("a preferred port that's taken falls back to another, and says so", () => {
    const busy = Bun.serve({ hostname: "127.0.0.1", port: 0, fetch: () => new Response("") });
    try {
      const listener = listen(() => ({ ok: false, why: "state" }), busy.port);
      expect(listener.port).not.toBe(busy.port);
      expect(listener.fellBack).toBe(true);
      listener.close();
    } finally {
      busy.stop(true);
    }
  });

  test("the pages say what happened, and nothing else", () => {
    expect(callbackPage({ ok: true, code: "c" }).status).toBe(200);
    expect(callbackPage({ ok: false, why: "state" }).status).toBe(400);
  });
});

describe("the browser", () => {
  test("not over SSH, not without a display on Linux", () => {
    expect(canOpenBrowser({ SSH_CONNECTION: "1 2 3 4", DISPLAY: ":0" }, "linux")).toBe(false);
    expect(canOpenBrowser({}, "linux")).toBe(false);
    expect(canOpenBrowser({ WAYLAND_DISPLAY: "wayland-1" }, "linux")).toBe(true);
    expect(canOpenBrowser({ DISPLAY: ":0" }, "linux")).toBe(true);
    expect(canOpenBrowser({}, "darwin")).toBe(true);
    expect(canOpenBrowser({ SSH_CONNECTION: "x" }, "darwin")).toBe(false);
  });

  test("openBrowser passes the URL as one argument, no shell", () => {
    const spawned: string[][] = [];
    openBrowser(new URL("https://as.test/a?b=1&c=$(rm -rf ~)"), "linux", (argv) => spawned.push(argv));
    openBrowser(new URL("https://as.test/"), "darwin", (argv) => spawned.push(argv));
    expect(spawned).toEqual([
      ["xdg-open", "https://as.test/a?b=1&c=$(rm%20-rf%20~)"],
      ["open", "https://as.test/"],
    ]);
  });
});

test("probeLogin stops when the caller's signal does (Esc during /mcp add), not only at its timeout", async () => {
  const { probeLogin } = await import("../src/mcp/oauth.ts");
  const stop = new AbortController();
  const hanging = ((_: unknown, init?: RequestInit) =>
    new Promise((_r, reject) => init?.signal?.addEventListener("abort", () => reject(new Error("aborted"))))) as unknown as typeof fetch;
  const started = Date.now();
  setTimeout(() => stop.abort(), 50);
  expect(await probeLogin("https://mcp.example.com/mcp", { timeoutMs: 5000, fetchFn: hanging, signal: stop.signal })).toBe(false);
  expect(Date.now() - started).toBeLessThan(1000);
});

test("a cancelled login saves no discovery state either (like tokens and the client)", async () => {
  const { McpOAuthProvider } = await import("../src/mcp/oauth.ts");
  const { McpTokenStore } = await import("../src/mcp/oauth-store.ts");
  const { mkdtempSync, rmSync } = await import("node:fs");
  const { tmpdir } = await import("node:os");
  const { join } = await import("node:path");
  const dir = mkdtempSync(join(tmpdir(), "marv-disc-"));
  try {
    const store = new McpTokenStore(join(dir, "mcp-tokens.json"));
    const stop = new AbortController();
    const provider = new McpOAuthProvider({ store, key: "personal https://x/", server: "x", mode: "interactive", signal: stop.signal, onAuthorize: () => {} });
    stop.abort(new Error("cancelled: /mcp logout"));
    await expect(provider.saveDiscoveryState({ authorizationServerUrl: "https://auth.example/" } as never)).rejects.toThrow(/cancelled/);
    expect(await store.get("personal https://x/")).toBeUndefined();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
