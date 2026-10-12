import { describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, statSync, writeFileSync } from "node:fs";
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
    // And an update over a broken file starts afresh rather than failing.
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
});
