// One test per caller of the store (seven, in five files), the compatibility wrapper, and the converted tests.
import { expect, test } from "bun:test";
import { mkdtempSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import * as store from "./src/store.js";
import { addUser, findUser, listUsers } from "./src/users.js";
import { getSetting, setSetting } from "./src/settings.js";
import { increment } from "./src/counter.js";
import { summary } from "./src/report.js";
import { main } from "./src/cli.js";

const dir = () => mkdtempSync(join(tmpdir(), "eval-store-"));
const isPromise = (value) => value instanceof Promise;

test("load and save return promises", async () => {
  const path = join(dir(), "x.json");
  const saving = store.save(path, { a: 1 });
  expect(isPromise(saving)).toBe(true);
  await saving;
  const loading = store.load(path);
  expect(isPromise(loading)).toBe(true);
  expect(await loading).toEqual({ a: 1 });
  expect(await store.load(join(dir(), "missing.json"))).toEqual({});
  const bad = join(dir(), "bad.json");
  writeFileSync(bad, "{nope");
  await expect(store.load(bad)).rejects.toThrow("isn't valid JSON");
});

test("loadCallback keeps the old callback API", async () => {
  const path = join(dir(), "x.json");
  writeFileSync(path, '{"b":2}');
  const got = await new Promise((resolve, reject) => store.loadCallback(path, (err, data) => (err ? reject(err) : resolve(data))));
  expect(got).toEqual({ b: 2 });
  const bad = join(dir(), "bad.json");
  writeFileSync(bad, "{nope");
  const err = await new Promise((resolve) => store.loadCallback(bad, (e) => resolve(e)));
  expect(err?.message).toContain("isn't valid JSON");
});

test("addUser", async () => {
  const path = join(dir(), "users.json");
  expect(await addUser(path, "bo", "2026-01-02")).toEqual({ name: "bo", joined: "2026-01-02" });
  await expect(addUser(path, "bo", "x")).rejects.toThrow("already exists");
});

test("findUser", async () => {
  const path = join(dir(), "users.json");
  await addUser(path, "ana", "d");
  expect(await findUser(path, "ana")).toEqual({ name: "ana", joined: "d" });
  expect(await findUser(path, "zed")).toBeNull();
});

test("listUsers", async () => {
  const path = join(dir(), "users.json");
  await addUser(path, "cy", "d");
  await addUser(path, "ana", "d");
  expect(await listUsers(path)).toEqual(["ana", "cy"]);
});

test("getSetting and setSetting", async () => {
  const path = join(dir(), "settings.json");
  expect(await getSetting(path, "title", "Report")).toBe("Report");
  expect(await setSetting(path, "title", "Weekly")).toBeUndefined();
  expect(await setSetting(path, "title", "Daily")).toBe("Weekly");
  expect(await getSetting(path, "title", "Report")).toBe("Daily");
});

test("increment", async () => {
  const path = join(dir(), "counters.json");
  expect(await increment(path, "visits")).toBe(1);
  expect(await increment(path, "visits", 5)).toBe(6);
});

test("summary", async () => {
  const d = dir();
  const f = { users: join(d, "u.json"), settings: join(d, "s.json"), counters: join(d, "c.json") };
  await addUser(f.users, "bo", "d");
  await increment(f.counters, "visits", 3);
  expect(await summary(f)).toBe("Report: 1 users (bo); 3 visits");
});

test("main", async () => {
  const d = dir();
  expect(await main(d, ["add-user", "ana"])).toBe("added ana");
  expect(await main(d, ["visit"])).toBe("visit 1");
  expect(await main(d, ["set", "title", "Weekly"])).toBe("title = Weekly");
  expect(await main(d, ["report"])).toBe("Weekly: 1 users (ana); 1 visits");
  await expect(main(d, ["nope"])).rejects.toThrow("unknown command");
});

test("no callbacks are left in the code or the tests (loadCallback aside)", () => {
  const src = readdirSync("src").map((f) => [`src/${f}`, readFileSync(join("src", f), "utf8")]);
  const tests = readdirSync("test").filter((f) => f.endsWith(".test.js")).map((f) => [`test/${f}`, readFileSync(join("test", f), "utf8")]);
  expect(tests.length).toBeGreaterThanOrEqual(3);
  for (const [name, text] of [...src, ...tests]) {
    // Comments may describe the old API; the wrapper is the one callback allowed (up to the next blank line).
    // Strings don't count either (a test's "cb.json" isn't a callback), and a test may call the wrapper with one.
    const bare = text.replace(/\/\*[\s\S]*?\*\//g, "").replace(/\/\/.*$/gm, "").replace(/(["'`])(?:\\.|(?!\1)[^\\\n])*\1/g, '""');
    const code = name === "src/store.js"
      ? bare.replace(/export\s+(async\s+)?(function|const|let)\s+loadCallback[\s\S]*?(\n\s*\n|$)/, "")
      : bare.replace(/\bloadCallback\s*\(.*$/gm, "");
    expect({ name, callbacks: /\((err\w*|e), /.test(code) || /\bdone\)/.test(code) || /\bcb\b/.test(code) }).toEqual({ name, callbacks: false });
  }
});
