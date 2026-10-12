import { expect, test } from "bun:test";
import { mkdtempSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { loadMcpConfig } from "../src/mcp/config.ts";
import { addPersonalServer, removePersonalServer } from "../src/mcp/personal-file.ts";

const dir = () => mkdtempSync(join(tmpdir(), "marv-mcp-file-"));
const entry = { command: "npx", args: ["-y", "weather@1.2.0"], env: { API_KEY: "${API_KEY}" } };
const file = (d: string) => JSON.parse(readFileSync(join(d, "mcp.json"), "utf8"));

test("adds beside what's there, 0600, and loads back unchanged", async () => {
  const d = dir();
  writeFileSync(join(d, "mcp.json"), JSON.stringify({ other: true, mcpServers: { docs: { type: "http", url: "https://x/mcp" } } }));
  await addPersonalServer(d, "weather", entry);
  const written = file(d);
  expect(written.other).toBe(true);
  expect(written.mcpServers.docs).toEqual({ type: "http", url: "https://x/mcp" });
  expect(written.mcpServers.weather).toEqual(entry);
  expect(statSync(join(d, "mcp.json")).mode & 0o777).toBe(0o600);
  const { servers, problems } = await loadMcpConfig({ root: d, configDir: d, env: { API_KEY: "k" }, include: { personal: true, project: false } });
  expect(problems).toEqual([]);
  expect(servers.map((s) => s.name)).toEqual(["docs", "weather"]);
  expect(servers[1]!.transport).toEqual({ type: "stdio", command: "npx", args: ["-y", "weather@1.2.0"], env: { API_KEY: "k" } });
});

test("a missing file is created", async () => {
  const d = dir();
  await addPersonalServer(d, "weather", entry);
  expect(file(d)).toEqual({ mcpServers: { weather: entry } });
});

test("an unset ${VAR} doesn't stop the write: that's the environment, not the entry", async () => {
  const d = dir();
  await addPersonalServer(d, "weather", { command: "npx", args: ["${NOT_SET_ANYWHERE_123}"] });
  expect(file(d).mcpServers.weather.args).toEqual(["${NOT_SET_ANYWHERE_123}"]);
});

test("refuses a name that's taken, a broken file, and an entry Marv couldn't load", async () => {
  const d = dir();
  await addPersonalServer(d, "weather", entry);
  await expect(addPersonalServer(d, "weather", entry)).rejects.toThrow(/--name/);
  writeFileSync(join(d, "mcp.json"), "{ broken");
  await expect(addPersonalServer(d, "x", entry)).rejects.toThrow(/couldn't read/i);
  expect(readFileSync(join(d, "mcp.json"), "utf8")).toBe("{ broken"); // never overwritten
  writeFileSync(join(d, "mcp.json"), '{"mcpServers": []}');
  await expect(addPersonalServer(d, "x", entry)).rejects.toThrow(/couldn't read/i);
  await expect(addPersonalServer(dir(), "x", { type: "sse", url: "https://x" })).rejects.toThrow(/sse/);
  await expect(addPersonalServer(dir(), "a__b", entry)).rejects.toThrow(/__/);
});

test("two adds right after each other both land", async () => {
  const d = dir();
  await Promise.all([addPersonalServer(d, "a", entry), addPersonalServer(d, "b", entry)]);
  expect(Object.keys(file(d).mcpServers)).toEqual(["a", "b"]);
});

test("remove takes out one server and keeps the rest", async () => {
  const d = dir();
  await addPersonalServer(d, "a", entry);
  await addPersonalServer(d, "b", entry);
  await removePersonalServer(d, "a");
  expect(Object.keys(file(d).mcpServers)).toEqual(["b"]);
  await expect(removePersonalServer(d, "nope")).rejects.toThrow(/no server "nope"/);
});
