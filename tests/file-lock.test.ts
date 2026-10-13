import { describe, expect, test } from "bun:test";
import { existsSync, mkdtempSync, readFileSync, rmSync, utimesSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { releaseLock, takeOverStale, withFileLock } from "../src/file-lock.ts";

const dir = () => mkdtempSync(join(tmpdir(), "marv-lock-"));
const age = (path: string, ms: number) => {
  const then = new Date(Date.now() - ms);
  utimesSync(path, then, then);
};

describe("withFileLock", () => {
  test("held during the work, gone after it, also when the work throws", async () => {
    const path = join(dir(), "f.json");
    await withFileLock(path, async () => {
      expect(readFileSync(`${path}.lock`, "utf8")).toMatch(new RegExp(`^${process.pid} [0-9a-f]+$`));
    });
    expect(existsSync(`${path}.lock`)).toBe(false);
    await expect(withFileLock(path, async () => Promise.reject(new Error("boom")))).rejects.toThrow("boom");
    expect(existsSync(`${path}.lock`)).toBe(false);
  });

  test("release never removes another holder's lock", async () => {
    const path = join(dir(), "f.json");
    await withFileLock(path, async () => {
      // This hold looked dead: it was taken over, and another process holds the lock now.
      writeFileSync(`${path}.lock`, "222 someone-else");
    });
    expect(readFileSync(`${path}.lock`, "utf8")).toBe("222 someone-else");
    // And releaseLock itself checks the token.
    await releaseLock(`${path}.lock`, "111 mine");
    expect(readFileSync(`${path}.lock`, "utf8")).toBe("222 someone-else");
    await releaseLock(`${path}.lock`, "222 someone-else");
    expect(existsSync(`${path}.lock`)).toBe(false);
  });

  test("a slow holder keeps its lock fresh, so it isn't taken over as stale", async () => {
    const path = join(dir(), "f.json");
    const order: string[] = [];
    const slow = withFileLock(path, async () => {
      order.push("slow in");
      await Bun.sleep(400);
      order.push("slow out");
    }, { staleMs: 120 });
    await Bun.sleep(20);
    // A second Marv process: same file, its own queue, so it really contends for the lock.
    const child = Bun.spawn([process.execPath, "-e", `
      const { withFileLock } = await import(${JSON.stringify(join(import.meta.dir, "..", "src", "file-lock.ts"))});
      await withFileLock(${JSON.stringify(path)}, async () => {}, { staleMs: 120 });
      console.log(Date.now());
    `], { stdout: "pipe" });
    await slow;
    const outAt = Date.now();
    expect(await child.exited).toBe(0);
    expect(Number((await new Response(child.stdout).text()).trim())).toBeGreaterThanOrEqual(outAt - 50);
    expect(order).toEqual(["slow in", "slow out"]);
  });
});

describe("taking over a stale lock", () => {
  test("a stale lock is removed; a fresh one is left alone", async () => {
    const lock = join(dir(), "f.json.lock");
    writeFileSync(lock, "111 dead");
    age(lock, 60_000);
    expect(await takeOverStale(lock, 1_000)).toBe(true);
    expect(existsSync(lock)).toBe(false);
    writeFileSync(lock, "222 alive");
    expect(await takeOverStale(lock, 1_000)).toBe(false);
    expect(readFileSync(lock, "utf8")).toBe("222 alive");
  });

  test("a third contender during takeover: the lock it created while another waiter took over is left alone", async () => {
    const lock = join(dir(), "f.json.lock");
    writeFileSync(lock, "111 dead");
    age(lock, 60_000);
    // Another waiter is in the middle of taking the stale lock over (it holds the guard)...
    writeFileSync(`${lock}.takeover`, "");
    // ...when this one sees the lock is stale too.
    const ours = takeOverStale(lock, 1_000);
    await Bun.sleep(30);
    expect(readFileSync(lock, "utf8")).toBe("111 dead"); // nothing happens without the guard
    // The other waiter removes the stale lock and lets go; a third process takes the lock, fresh.
    rmSync(lock);
    writeFileSync(lock, "333 fresh");
    rmSync(`${lock}.takeover`);
    expect(await ours).toBe(false);
    expect(readFileSync(lock, "utf8")).toBe("333 fresh");
  });

  test("a guard left by a process that died is taken over after its own, longer timeout", async () => {
    const lock = join(dir(), "f.json.lock");
    writeFileSync(lock, "111 dead");
    age(lock, 60_000);
    writeFileSync(`${lock}.takeover`, "");
    age(`${lock}.takeover`, 60_000);
    expect(await takeOverStale(lock, 1_000)).toBe(true);
    expect(existsSync(lock)).toBe(false);
    expect(existsSync(`${lock}.takeover`)).toBe(false);
  });

  test("several processes with crashed holders among them: never two holders at once", async () => {
    const path = join(dir(), "f.json");
    const contender = join(import.meta.dir, "fixtures", "file-lock-contender.ts");
    const children = Array.from({ length: 4 }, () => Bun.spawn([process.execPath, contender, path, "16", "150"], { stderr: "pipe" }));
    for (const child of children) {
      const code = await child.exited;
      if (code !== 0) console.error(await new Response(child.stderr).text());
      expect(code).toBe(0);
    }
    expect(existsSync(`${path}.holder`)).toBe(false);
    expect(existsSync(`${path}.lock.takeover`)).toBe(false);
  }, 30_000);
});
