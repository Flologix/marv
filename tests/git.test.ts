import { afterEach, describe, expect, test } from "bun:test";
import { chmodSync, existsSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { runGit, runGitAsync } from "../src/git.ts";

const dirs: string[] = [];
afterEach(() => {
  for (const dir of dirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

function repo(): string {
  const dir = mkdtempSync(join(tmpdir(), "marv-git-"));
  dirs.push(dir);
  runGit(dir, ["init", "-q"]);
  runGit(dir, ["config", "user.email", "t@t"]);
  runGit(dir, ["config", "user.name", "t"]);
  writeFileSync(join(dir, "a.txt"), "a\n");
  runGit(dir, ["add", "a.txt"]);
  runGit(dir, ["commit", "-q", "-m", "init"]);
  return dir;
}

describe("runGitAsync", () => {
  test("has runGit's result shape", async () => {
    const dir = repo();
    const sync = runGit(dir, ["rev-parse", "HEAD"]);
    const async = await runGitAsync(dir, ["rev-parse", "HEAD"]);
    expect(async.ok).toBe(true);
    expect(async.out).toBe(sync.out);
    expect(async.timedOut).toBe(false);
    const failed = await runGitAsync(dir, ["rev-parse", "no-such-ref"]);
    expect(failed.ok).toBe(false);
    expect(failed.out).toContain("no-such-ref");
  });

  test("keeps stdout's bytes", async () => {
    const dir = repo();
    const bytes = new Uint8Array([0, 255, 10, 1]);
    writeFileSync(join(dir, "bin"), bytes);
    const hashed = await runGitAsync(dir, ["hash-object", "-w", "bin"]);
    const blob = await runGitAsync(dir, ["cat-file", "blob", hashed.out.trim()]);
    expect([...blob.bytes]).toEqual([...bytes]);
  });

  test("never runs hooks or fsmonitor", async () => {
    const dir = repo();
    const marker = join(dir, "ran");
    const script = join(dir, "touch.sh");
    writeFileSync(script, `#!/bin/sh\ntouch ${marker}\n`);
    chmodSync(script, 0o755);
    writeFileSync(join(dir, ".git", "hooks", "pre-commit"), `#!/bin/sh\ntouch ${marker}\n`);
    chmodSync(join(dir, ".git", "hooks", "pre-commit"), 0o755);
    runGit(dir, ["config", "core.fsmonitor", script]);
    writeFileSync(join(dir, "a.txt"), "changed\n");
    await runGitAsync(dir, ["status", "--porcelain"]);
    await runGitAsync(dir, ["commit", "-q", "-am", "x"]);
    expect(existsSync(marker)).toBe(false);
  });

  test("ignores an inherited GIT_DIR", async () => {
    const dir = repo();
    const other = repo();
    const saved = process.env.GIT_DIR;
    process.env.GIT_DIR = join(other, ".git");
    try {
      const top = await runGitAsync(dir, ["rev-parse", "--show-toplevel"]);
      expect(top.out.trim()).toBe(runGit(dir, ["rev-parse", "--show-toplevel"]).out.trim());
    } finally {
      if (saved === undefined) delete process.env.GIT_DIR;
      else process.env.GIT_DIR = saved;
    }
  });

  test("a timeout kills git", async () => {
    const dir = repo();
    // An alias that runs a shell command for a long time stands in for a git that hangs.
    const started = Date.now();
    const result = await runGitAsync(dir, ["-c", "alias.wait=!sleep 5", "wait"], { timeoutMs: 200 });
    expect(result.timedOut).toBe(true);
    expect(result.ok).toBe(false);
    expect(Date.now() - started).toBeLessThan(3000);
  });

  test("an abort kills git", async () => {
    const dir = repo();
    const stop = new AbortController();
    setTimeout(() => stop.abort(), 100);
    const started = Date.now();
    const result = await runGitAsync(dir, ["-c", "alias.wait=!sleep 5", "wait"], { signal: stop.signal });
    expect(result.ok).toBe(false);
    expect(Date.now() - started).toBeLessThan(3000);
  });
});
