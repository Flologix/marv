// write_file and edit_file read a file, change it, and write the whole file back. Two calls on one file at the same
// time (parallel tool calls, subagents sharing the project folder) would both read the same original, and the later
// write would drop the earlier one's change: two appends would keep only one. Each file's read-change-write runs
// after the previous one on the same path has finished.
import { resolve } from "node:path";

const queues = new Map<string, Promise<unknown>>();

/** Runs `fn` after every earlier `withFileLock` on the same path (relative to `root`) has settled. */
export function withFileLock<T>(root: string, path: string, fn: () => Promise<T>): Promise<T> {
  const key = resolve(root, path);
  const previous = queues.get(key) ?? Promise.resolve();
  const next = previous.then(fn, fn);
  const settled = next.catch(() => {});
  queues.set(key, settled);
  // Forget the path once nothing else is queued behind this call.
  void settled.then(() => {
    if (queues.get(key) === settled) queues.delete(key);
  });
  return next;
}
