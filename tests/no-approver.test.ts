import { afterEach, beforeEach, expect, test } from "bun:test";
import { existsSync } from "node:fs";
import { mkdir, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { runTool } from "../src/tools/index.ts";

let root: string;
beforeEach(async () => {
  root = await mkdtemp(join(tmpdir(), "marv-no-approver-"));
});
afterEach(() => rm(root, { recursive: true, force: true }));

const write = (path: string) => ({ id: "c1", name: "write_file", arguments: JSON.stringify({ path, content: "hi\n" }) });

test("with no one to ask, yolo still runs what it vouches for", async () => {
  const result = await runTool(write("a.txt"), { root, yolo: true });
  expect(result).toMatchObject({ approval: "auto" });
  expect(existsSync(join(root, "a.txt"))).toBe(true);
});

test("with no one to ask, anything else is refused, as an error the model can read", async () => {
  const result = await runTool(write("a.txt"), { root, yolo: false });
  expect(result).toMatchObject({ isError: true, output: "write_file needs the user's approval, and there's no one to ask." });
  expect(existsSync(join(root, "a.txt"))).toBe(false);
});

test("with yolo on, a call it doesn't vouch for is still refused when there's no one to ask", async () => {
  await mkdir(join(root, ".git"));
  const result = await runTool(write(".git/config"), { root, yolo: true });
  expect(result).toMatchObject({ isError: true, output: "write_file needs the user's approval, and there's no one to ask." });
  expect(existsSync(join(root, ".git", "config"))).toBe(false);
});

test("an approver that answers \"unavailable\" refuses like no approver: an error the model can read, not a decline that stops the run", async () => {
  const result = await runTool(write("a.txt"), { root, yolo: false, approve: async () => "unavailable" });
  expect(result).toMatchObject({ isError: true, output: "write_file needs the user's approval, and there's no one to ask." });
  expect(result.declined).toBeUndefined();
  expect(existsSync(join(root, "a.txt"))).toBe(false);
});
