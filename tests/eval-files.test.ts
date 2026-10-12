import { afterEach, beforeEach, expect, test } from "bun:test";
import { lstatSync, mkdirSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { copyFresh, writeFresh } from "../evals/files.ts";

// The eval runner puts files into a workspace a model has just worked in: a planted symlink there must not make it
// write to the file the link points at, outside the workspace.
let base: string;
let dir: string;
let outside: string;
beforeEach(() => {
  base = mkdtempSync(join(tmpdir(), "marv-eval-files-"));
  dir = join(base, "workspace");
  mkdirSync(dir);
  outside = join(base, "host-file");
  writeFileSync(outside, "precious\n");
});
afterEach(() => rmSync(base, { recursive: true, force: true }));

test("writeFresh replaces a planted symlink instead of writing through it", () => {
  symlinkSync(outside, join(dir, "__tracker_comments__.json"));
  writeFresh(join(dir, "__tracker_comments__.json"), "[]");
  expect(readFileSync(outside, "utf8")).toBe("precious\n");
  expect(lstatSync(join(dir, "__tracker_comments__.json")).isSymbolicLink()).toBe(false);
  expect(readFileSync(join(dir, "__tracker_comments__.json"), "utf8")).toBe("[]");
});

test("copyFresh replaces planted symlinks, to files and to folders, at every entry it copies", () => {
  const from = join(base, "check");
  mkdirSync(join(from, "fixtures"), { recursive: true });
  writeFileSync(join(from, "__eval__.test.js"), "test\n");
  writeFileSync(join(from, "fixtures", "a.json"), "{}\n");
  const outsideDir = join(base, "host-dir");
  mkdirSync(outsideDir);
  symlinkSync(outside, join(dir, "__eval__.test.js"));
  symlinkSync(outsideDir, join(dir, "fixtures"));
  copyFresh(from, dir);
  expect(readFileSync(outside, "utf8")).toBe("precious\n");
  expect(() => readFileSync(join(outsideDir, "a.json"))).toThrow();
  expect(readFileSync(join(dir, "__eval__.test.js"), "utf8")).toBe("test\n");
  expect(readFileSync(join(dir, "fixtures", "a.json"), "utf8")).toBe("{}\n");
});
