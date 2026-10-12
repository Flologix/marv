import { afterEach, beforeEach, expect, test } from "bun:test";
import { chmodSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

// scripts/review-gate.ts against a fake `ocr` that prints a multi-line version (as the real one does) and counts
// the reviews it runs: a commit that passed once must not be reviewed again.
const GATE = join(import.meta.dir, "..", "scripts", "review-gate.ts");
let dir: string;
let repo: string;
let bin: string;

beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "marv-gate-"));
  repo = join(dir, "repo");
  bin = join(dir, "bin");
  const sh = (cmd: string[], cwd = dir) => {
    const r = Bun.spawnSync(cmd, { cwd, env: { ...process.env, GIT_CONFIG_GLOBAL: "/dev/null", GIT_CONFIG_NOSYSTEM: "1" } });
    if (r.exitCode !== 0) throw new Error(r.stderr.toString());
  };
  sh(["mkdir", "-p", repo, bin]);
  const git = (...a: string[]) => sh(["git", "-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgSign=false", ...a], repo);
  git("init", "-q", "-b", "main");
  writeFileSync(join(repo, "a.txt"), "a\n");
  git("add", ".");
  git("commit", "-qm", "base");
  git("checkout", "-qb", "work");
  writeFileSync(join(repo, "a.txt"), "b\n");
  git("commit", "-qam", "change");
  writeFileSync(
    join(bin, "ocr"),
    `#!/bin/sh
if [ "$1" = "--version" ]; then printf 'open-code-review v1.0.0 (abc)\\nbuilt at: 2026-01-01\\nhttps://example.com\\n'; exit 0; fi
echo review >> "${join(dir, "reviews")}"
echo '{"status":"complete","comments":[]}'
`,
  );
  chmodSync(join(bin, "ocr"), 0o755);
});
afterEach(() => rmSync(dir, { recursive: true, force: true }));

const gate = () =>
  Bun.spawnSync(["bun", GATE, "--base", "main"], { cwd: repo, env: { ...process.env, PATH: `${bin}:${process.env.PATH}`, REVIEW_GATE: "" } });
const reviews = () => readFileSync(join(dir, "reviews"), "utf8").trim().split("\n").length;

test("a commit that passed is not reviewed again (ocr's multi-line --version included)", () => {
  expect(gate().exitCode).toBe(0);
  expect(gate().exitCode).toBe(0);
  expect(reviews()).toBe(1);
});
