import { describe, expect, test } from "bun:test";
import { existsSync, mkdtempSync, readFileSync, rmSync, statSync, utimesSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { loginKey, McpTokenStore } from "../src/mcp/oauth-store.ts";

const store = () => new McpTokenStore(join(mkdtempSync(join(tmpdir(), "marv-oauth-")), "mcp-tokens.json"));
const tokens = { access_token: "a1", refresh_token: "r1", token_type: "Bearer", expires_in: 3600, issuer: "https://as.test/" };
const YOURS = "personal https://x.test/mcp";

describe("loginKey", () => {
  test("yours: one per URL, whatever the name; a project's: per project", () => {
    expect(loginKey({ source: "personal", url: "https://x.test/mcp" }, "/p")).toBe(YOURS);
    expect(loginKey({ source: "personal", url: "https://x.test/mcp" }, "/q")).toBe(YOURS);
    expect(loginKey({ source: "project", url: "https://x.test/mcp" }, "/p")).toMatch(/^project:.+ https:\/\/x\.test\/mcp$/);
    expect(loginKey({ source: "project", url: "https://x.test/mcp" }, "/p")).not.toBe(loginKey({ source: "project", url: "https://x.test/mcp" }, "/q"));
    // A project's server never sees your login for the same URL.
    expect(loginKey({ source: "project", url: "https://x.test/mcp" }, "/p")).not.toBe(YOURS);
    // One host, two resources: two logins.
    expect(loginKey({ source: "personal", url: "https://x.test/other" }, "/p")).not.toBe(YOURS);
  });
});

describe("McpTokenStore", () => {
  test("round trip, written 0600; the SDK's fields (issuer) are kept unchanged", async () => {
    const s = store();
    await s.update(YOURS, (e) => ({ ...e, server: "x", tokens, savedAt: 1 }));
    const got = await s.get(YOURS);
    expect(got?.tokens).toEqual(tokens);
    expect(got?.server).toBe("x");
    expect(statSync(s.path).mode & 0o777).toBe(0o600);
  });

  test("a missing, unreadable or wrong-shape file is no logins, never a throw", async () => {
    const s = store();
    expect(await s.get(YOURS)).toBeUndefined();
    for (const content of ["{not json", "null", '{"v":1,"logins":[]}', '{"v":2,"logins":{}}', '"x"']) {
      writeFileSync(s.path, content);
      expect(await s.get(YOURS)).toBeUndefined();
    }
  });

  test("an update never writes over a file it can't use (a newer format, a hand edit mid-way): it throws, saying so", async () => {
    const s = store();
    for (const content of ["{not json", "null", '{"v":1,"logins":[]}', '{"v":2,"logins":{"personal https://z.test/":{}}}', '"x"']) {
      writeFileSync(s.path, content);
      await expect(s.update(YOURS, (e) => ({ ...e, server: "x", tokens, savedAt: 1 }))).rejects.toThrow(/won't write over it/);
      expect(readFileSync(s.path, "utf8")).toBe(content);
    }
    // An empty file holds nothing to lose: an update starts afresh.
    writeFileSync(s.path, "");
    await s.update(YOURS, (e) => ({ ...e, server: "x", tokens, savedAt: 1 }));
    expect((await s.get(YOURS))?.tokens?.access_token).toBe("a1");
  });

  test("remove deletes one entry and keeps the others", async () => {
    const s = store();
    await s.update(YOURS, (e) => ({ ...e, server: "x", tokens, savedAt: 1 }));
    await s.update("personal https://y.test/mcp", (e) => ({ ...e, server: "y", tokens, savedAt: 1 }));
    await s.remove(YOURS);
    expect(await s.get(YOURS)).toBeUndefined();
    expect((await s.get("personal https://y.test/mcp"))?.server).toBe("y");
  });

  test("concurrent updates of different keys both land", async () => {
    const s = store();
    await Promise.all([
      s.update(YOURS, (e) => ({ ...e, server: "x", tokens, savedAt: 1 })),
      s.update("personal https://y.test/mcp", (e) => ({ ...e, server: "y", tokens, savedAt: 2 })),
    ]);
    const file = JSON.parse(readFileSync(s.path, "utf8"));
    expect(Object.keys(file.logins).sort()).toEqual([YOURS, "personal https://y.test/mcp"]);
  });

  test("two stores on one file (two managers) don't lose each other's writes", async () => {
    const a = store();
    const b = new McpTokenStore(a.path);
    await Promise.all([
      a.update(YOURS, (e) => ({ ...e, server: "x", tokens, savedAt: 1 })),
      b.update("personal https://y.test/mcp", (e) => ({ ...e, server: "y", tokens, savedAt: 2 })),
    ]);
    expect(await a.get("personal https://y.test/mcp")).toBeDefined();
    expect(await b.get(YOURS)).toBeDefined();
  });

  test("two Marv processes updating one file at once: every entry survives (a lock file held across read-modify-write)", async () => {
    const s = store();
    const writer = join(import.meta.dir, "fixtures", "token-store-writer.ts");
    const run = (prefix: string) => Bun.spawn([process.execPath, writer, s.path, prefix, "40"], { stderr: "pipe" });
    const children = [run("a"), run("b"), run("c")];
    // This process joins in too.
    for (let i = 0; i < 40; i++) await s.update(`d https://x.test/${i}`, (e) => ({ ...e, server: `d${i}` }));
    for (const child of children) expect(await child.exited).toBe(0);
    const logins = await s.all();
    expect(Object.keys(logins)).toHaveLength(160);
    expect(existsSync(`${s.path}.lock`)).toBe(false);
  });

  test("a lock left by a crashed process is taken over once it's stale; a live one is waited for", async () => {
    const s = store();
    writeFileSync(`${s.path}.lock`, "12345");
    const old = new Date(Date.now() - 60_000);
    utimesSync(`${s.path}.lock`, old, old);
    await s.update(YOURS, (e) => ({ ...e, server: "x" }));
    expect((await s.get(YOURS))?.server).toBe("x");
    expect(existsSync(`${s.path}.lock`)).toBe(false);

    writeFileSync(`${s.path}.lock`, "12345");
    let done = false;
    const update = s.update(YOURS, (e) => ({ ...e!, server: "y" })).then(() => (done = true));
    await Bun.sleep(100);
    expect(done).toBe(false);
    rmSync(`${s.path}.lock`);
    await update;
    expect((await s.get(YOURS))?.server).toBe("y");
  });
});

describe("taking over a stale lock", () => {
  test("only the lock judged stale is removed: one created after the check is left alone", async () => {
    const { takeOverStale } = await import("../src/mcp/oauth-store.ts");
    const dir = mkdtempSync(join(tmpdir(), "marv-lock-"));
    try {
      const lock = join(dir, "mcp-tokens.json.lock");
      writeFileSync(lock, "111 old-token");
      // Another waiter took the stale lock over, and a third process created a fresh one, before this one acts.
      writeFileSync(lock, "222 fresh-token");
      expect(takeOverStale(lock, "111 old-token")).toBe(false);
      expect(readFileSync(lock, "utf8")).toBe("222 fresh-token");
      // The stale one itself is removed.
      writeFileSync(lock, "333 stale-token");
      expect(takeOverStale(lock, "333 stale-token")).toBe(true);
      expect(existsSync(lock)).toBe(false);
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });
});
