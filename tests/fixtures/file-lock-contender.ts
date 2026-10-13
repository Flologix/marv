// A contender for tests/file-lock.test.ts: `bun file-lock-contender.ts <path> <count> <staleMs>` takes the lock on
// <path> <count> times. While holding it, it creates `<path>.holder` with O_EXCL: if another process holds the lock
// at the same moment, that fails and this process exits 1. Every few rounds it plays a crashed holder instead: it
// leaves a lock behind that is already stale, so the others keep taking stale locks over while live ones come and go.
import { openSync, closeSync, unlinkSync, utimesSync, writeFileSync } from "node:fs";
import { withFileLock } from "../../src/file-lock.ts";

const [path, count, staleMs] = process.argv.slice(2);
const options = { staleMs: Number(staleMs), waitMs: 20_000 };
for (let i = 0; i < Number(count); i++) {
  if (i % 4 === 3) {
    try {
      writeFileSync(`${path}.lock`, `dead ${process.pid} ${i}`, { flag: "wx" });
      const old = new Date(Date.now() - 60_000);
      utimesSync(`${path}.lock`, old, old);
    } catch {
      // someone holds it: no crash this round
    }
  }
  await withFileLock(path!, async () => {
    closeSync(openSync(`${path}.holder`, "wx")); // throws if another holder is inside
    await Bun.sleep(10 + Math.random() * 20);
    unlinkSync(`${path}.holder`);
  }, options);
}
