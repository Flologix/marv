// A lock across Marv processes for one read-change-write of a shared file (~/.marv/mcp-tokens.json, ~/.marv/mcp.json):
// without it, two terminals read the same snapshot and the later write drops what the other just saved.
//
//   <file>.lock            the lock: created with O_EXCL (atomic on every local filesystem), holding a token unique
//                          to this hold ("<pid> <random>"). Its mtime is refreshed while the work runs, so only a lock
//                          whose process died stops being touched and turns stale (after `staleMs`).
//   <file>.lock.takeover   the guard: also O_EXCL, held only for one synchronous check-and-unlink, never across an
//                          await. Removing the lock (a stale takeover, or a holder's own release) happens only under it.
//
// Why the guard: "this lock is stale, remove it" is two steps (look, unlink). Without it, a waiter could look at the
// stale lock, another waiter remove it meanwhile and a third process create a fresh one, and the first unlink the
// fresh lock while its owner works: two holders. Under the guard nothing can remove the lock between the look and the
// unlink, and nothing can create one either (it still exists), so the look is still true when we act. The same goes
// for releasing: a holder unlinks the lock only if it still holds its own token, under the guard, so it never removes
// a lock that was taken over and recreated by someone else.
//
// The guard itself is taken over when its mtime is older than 3 × `staleMs`: that only happens if a process died
// inside a few microseconds of synchronous code. (That takeover is the plain look-and-unlink, so two waiters racing
// over a dead guard could, in theory, both get it; it needs a crash in that instant first.)
//
// In this process, calls for one file are queued, so they don't poll against each other.
import { randomBytes } from "node:crypto";
import { closeSync, openSync, readFileSync, statSync, unlinkSync, utimesSync, writeSync } from "node:fs";
import { mkdir } from "node:fs/promises";
import { dirname, resolve } from "node:path";

export type LockOptions = {
  /** A lock untouched for this long belongs to a process that died (holders refresh it every staleMs / 3). */
  staleMs?: number;
  /** How long to wait for another process's lock before giving up: long enough to outlast a stale one. */
  waitMs?: number;
};

const STALE_LOCK_MS = 10_000;

const queues = new Map<string, Promise<unknown>>();

/** Runs `fn` holding `<path>.lock`: one holder at a time, in this process and across processes. */
export function withFileLock<T>(path: string, fn: () => Promise<T>, options: LockOptions = {}): Promise<T> {
  const key = resolve(path);
  const previous = queues.get(key) ?? Promise.resolve();
  const next = previous.catch(() => {}).then(() => hold(key, fn, options));
  queues.set(key, next);
  void next.catch(() => {}).finally(() => queues.get(key) === next && queues.delete(key));
  return next;
}

async function hold<T>(path: string, fn: () => Promise<T>, { staleMs = STALE_LOCK_MS, waitMs = staleMs + 5_000 }: LockOptions): Promise<T> {
  const lock = `${path}.lock`;
  await mkdir(dirname(path), { recursive: true, mode: 0o700 });
  const token = `${process.pid} ${randomBytes(9).toString("hex")}`;
  const started = Date.now();
  while (!create(lock, token)) {
    await takeOverStale(lock, staleMs);
    if (Date.now() - started > waitMs) throw new Error(`${lock} is held by another Marv process: try again in a moment.`);
    await Bun.sleep(5 + Math.random() * 20);
  }
  // A slow update (a slow disk, a busy machine) mustn't look like a dead one.
  const heartbeat = setInterval(() => touchIfOurs(lock, token), staleMs / 3);
  heartbeat.unref();
  try {
    return await fn();
  } finally {
    clearInterval(heartbeat);
    await releaseLock(lock, token, staleMs);
  }
}

/** Creates the lock with our token, or false when another holder has it. */
function create(lock: string, token: string): boolean {
  let fd: number;
  try {
    fd = openSync(lock, "wx", 0o600);
  } catch (err) {
    if ((err as NodeJS.ErrnoException).code === "EEXIST") return false;
    throw err;
  }
  // Empty until written: harmless, staleness goes by mtime and only the holder compares its token.
  try {
    writeSync(fd, token);
  } finally {
    closeSync(fd);
  }
  return true;
}

const isStale = (path: string, staleMs: number) => Date.now() - statSync(path).mtimeMs > staleMs;

function touchIfOurs(lock: string, token: string): void {
  try {
    if (readFileSync(lock, "utf8") === token) {
      const now = new Date();
      utimesSync(lock, now, now);
    }
  } catch {
    // gone: release will find out
  }
}

/**
 * Runs `work` (synchronous: the guard is never held across an await) holding `<lock>.takeover`; undefined when the
 * guard couldn't be had in time.
 */
async function underGuard<R>(lock: string, staleMs: number, work: () => R): Promise<R | undefined> {
  const guard = `${lock}.takeover`;
  const guardStaleMs = staleMs * 3;
  const started = Date.now();
  for (;;) {
    try {
      closeSync(openSync(guard, "wx", 0o600));
      break;
    } catch (err) {
      if ((err as NodeJS.ErrnoException).code !== "EEXIST") throw err;
    }
    try {
      if (isStale(guard, guardStaleMs)) unlinkSync(guard);
    } catch {
      // released meanwhile
    }
    if (Date.now() - started > guardStaleMs + staleMs) return undefined;
    await Bun.sleep(1 + Math.random() * 4);
  }
  try {
    return work();
  } finally {
    try {
      unlinkSync(guard);
    } catch {
      // taken over as stale: nothing to release
    }
  }
}

/**
 * Removes the lock if it is stale, deciding under the guard: true when it removed one. A lock that is fresh by then
 * (the stale one was already taken over and someone created a new one) is left alone.
 */
export async function takeOverStale(lock: string, staleMs: number = STALE_LOCK_MS): Promise<boolean> {
  try {
    if (!isStale(lock, staleMs)) return false; // the cheap look first: a live lock is just waited for
  } catch {
    return false; // released meanwhile
  }
  const removed = await underGuard(lock, staleMs, () => {
    try {
      if (!isStale(lock, staleMs)) return false;
      unlinkSync(lock);
      return true;
    } catch {
      return false;
    }
  });
  return removed ?? false;
}

/**
 * Releases the lock only if it still holds `token`. If our lock was taken over (we looked dead) and someone else holds
 * it now, theirs stays. Without the guard the lock stays too: it goes stale and is taken over.
 */
export async function releaseLock(lock: string, token: string, staleMs: number = STALE_LOCK_MS): Promise<void> {
  await underGuard(lock, staleMs, () => {
    try {
      if (readFileSync(lock, "utf8") === token) unlinkSync(lock);
    } catch {
      // already gone
    }
  });
}
