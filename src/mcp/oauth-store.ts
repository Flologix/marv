// Where MCP logins are kept (#18): the OAuth client a server registered for Marv, its tokens and what discovery
// found, one entry per owner and server URL.
//
//   ~/.marv/mcp-tokens.json   (0600, written with writePrivate)
//   { "v": 1, "logins": { "personal https://mcp.example.com/": { "server": "x", "client": {…}, "tokens": {…}, … } } }
//
// Keyed by URL, not by the server's name: renaming a server keeps its login, and two names for one URL share it.
// Keyed by owner too: your servers share one login per URL across projects, while a project's server gets its own,
// per project. Trusting a project's .mcp.json means "run this server", not "act as my account": a repository that
// names your server's URL must not get your login by the trust you gave it for something else.
import { closeSync, existsSync, openSync, statSync, unlinkSync, writeSync } from "node:fs";
import { mkdir } from "node:fs/promises";
import { dirname } from "node:path";
import { z } from "zod";
import { projectKey } from "../paths.ts";
import { writePrivate } from "../private-file.ts";

/** The SDK's objects are stored as it hands them over (its `issuer` stamp included), so they're passed through. */
const Loose = z.object({}).passthrough();
const LoginShape = z.object({
  /** The server's name when it was saved: only for display. */
  server: z.string(),
  /** The client registered for Marv (or the configured one's port), plus the loopback port its redirect URI names. */
  client: Loose.extend({ client_id: z.string(), port: z.number().int().optional() }).optional(),
  tokens: Loose.extend({ access_token: z.string(), refresh_token: z.string().optional(), expires_in: z.number().optional() }).optional(),
  /** When `tokens` was saved (ms), so /mcp can say when the access token expires. */
  savedAt: z.number().optional(),
  /** The SDK's discovery state, so a startup with tokens makes no discovery requests. */
  discovery: Loose.extend({ authorizationServerUrl: z.string() }).optional(),
});
const FileShape = z.object({ v: z.literal(1), logins: z.record(z.string(), LoginShape) });

export type Login = z.infer<typeof LoginShape>;
type File = z.infer<typeof FileShape>;

/** The store key: owner, then the URL as expanded (one host can serve several resources, each with its own tokens). */
export function loginKey(server: { source: "personal" | "project"; url: string }, root: string): string {
  return server.source === "personal" ? `personal ${server.url}` : `project:${projectKey(root)} ${server.url}`;
}

/** Read-modify-writes in this process, per file, one after the other (two managers can share one file). */
const queues = new Map<string, Promise<unknown>>();

/** A lock older than this was left by a process that died mid-update (an update takes milliseconds). */
const STALE_LOCK_MS = 10_000;
/** How long an update waits for another process's lock: long enough to outlast a stale one. */
const LOCK_WAIT_MS = STALE_LOCK_MS + 5_000;

/** Why update() refused: the file exists but this Marv can't read it, so writing would destroy what it holds. */
export class UnusableTokenStore extends Error {
  constructor(path: string) {
    super(`${path} can't be read or is a format this Marv doesn't know (a newer version, or a hand edit): Marv won't write over it. Fix or remove the file, then log in again.`);
  }
}

/**
 * Holds `<store>.lock` across one read-modify-write, so two Marv processes can't both read the same snapshot and
 * the later write drop what the other just saved (a rotated token, a registration, a logout). O_EXCL creation is
 * the lock (atomic on every local filesystem); a lock whose mtime is older than STALE_LOCK_MS is a crashed
 * process's and is taken over.
 */
async function withFileLock<T>(path: string, fn: () => Promise<T>): Promise<T> {
  const lock = `${path}.lock`;
  await mkdir(dirname(path), { recursive: true, mode: 0o700 });
  const started = Date.now();
  for (;;) {
    try {
      const fd = openSync(lock, "wx", 0o600);
      writeSync(fd, String(process.pid));
      closeSync(fd);
      break;
    } catch (err) {
      if ((err as NodeJS.ErrnoException).code !== "EEXIST") throw err;
    }
    try {
      // Known limit: two waiters that both find it stale can race here (one may remove the other's fresh lock).
      if (Date.now() - statSync(lock).mtimeMs > STALE_LOCK_MS) unlinkSync(lock);
    } catch {
      // released meanwhile: try again
    }
    if (Date.now() - started > LOCK_WAIT_MS) throw new Error(`${lock} is held by another Marv process: try again in a moment.`);
    await Bun.sleep(5 + Math.random() * 20);
  }
  try {
    return await fn();
  } finally {
    try {
      unlinkSync(lock);
    } catch {
      // already gone (taken over as stale): nothing to release
    }
  }
}

export class McpTokenStore {
  constructor(readonly path: string) {}

  /**
   * Read every time: another Marv process may have refreshed a token meanwhile (see McpOAuthProvider). A file that
   * can't be read or has the wrong shape is "unusable": reads treat it as no logins, update() refuses to write.
   */
  private async read(): Promise<File | "unusable"> {
    if (!existsSync(this.path)) return { v: 1, logins: {} };
    try {
      const text = await Bun.file(this.path).text();
      // Empty (nothing to lose): no logins, and an update may start afresh.
      if (!text.trim()) return { v: 1, logins: {} };
      const parsed = FileShape.safeParse(JSON.parse(text));
      return parsed.success ? parsed.data : "unusable";
    } catch {
      return "unusable";
    }
  }

  /** Unreadable or the wrong shape (a hand edit, a newer format): no logins, everyone logs in again. Never a crash. */
  private async logins(): Promise<Record<string, Login>> {
    const file = await this.read();
    return file === "unusable" ? {} : file.logins;
  }

  async get(key: string): Promise<Login | undefined> {
    return (await this.logins())[key];
  }

  /** Every login (for /mcp's display of who is logged in where). */
  async all(): Promise<Record<string, Login>> {
    return this.logins();
  }

  /**
   * Replaces one entry with what `fn` makes of it; `undefined` removes it. Queued per file in this process and
   * locked across processes; throws UnusableTokenStore rather than write over a file it can't read.
   */
  update(key: string, fn: (entry: Login | undefined) => Login | undefined): Promise<void> {
    const previous = queues.get(this.path) ?? Promise.resolve();
    const next = previous
      .catch(() => {})
      .then(() =>
        withFileLock(this.path, async () => {
          const file = await this.read();
          if (file === "unusable") throw new UnusableTokenStore(this.path);
          const entry = fn(file.logins[key]);
          if (entry) file.logins[key] = entry;
          else delete file.logins[key];
          await writePrivate(this.path, JSON.stringify(file, null, 2));
        }),
      );
    queues.set(this.path, next);
    void next.catch(() => {}).finally(() => queues.get(this.path) === next && queues.delete(this.path));
    return next;
  }

  remove(key: string): Promise<void> {
    return this.update(key, () => undefined);
  }
}
