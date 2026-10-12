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
import { existsSync } from "node:fs";
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

export class McpTokenStore {
  constructor(readonly path: string) {}

  /** Read every time: another Marv process may have refreshed a token meanwhile (see McpOAuthProvider). */
  private async read(): Promise<File> {
    if (!existsSync(this.path)) return { v: 1, logins: {} };
    try {
      const parsed = FileShape.safeParse(await Bun.file(this.path).json());
      // Unreadable or the wrong shape (a hand edit, a newer format): no logins, everyone logs in again. Never a crash.
      return parsed.success ? parsed.data : { v: 1, logins: {} };
    } catch {
      return { v: 1, logins: {} };
    }
  }

  async get(key: string): Promise<Login | undefined> {
    return (await this.read()).logins[key];
  }

  /** Every login (for /mcp's display of who is logged in where). */
  async all(): Promise<Record<string, Login>> {
    return (await this.read()).logins;
  }

  /** Replaces one entry with what `fn` makes of it; `undefined` removes it. */
  update(key: string, fn: (entry: Login | undefined) => Login | undefined): Promise<void> {
    const previous = queues.get(this.path) ?? Promise.resolve();
    const next = previous
      .catch(() => {})
      .then(async () => {
        const file = await this.read();
        const entry = fn(file.logins[key]);
        if (entry) file.logins[key] = entry;
        else delete file.logins[key];
        await writePrivate(this.path, JSON.stringify(file, null, 2));
      });
    queues.set(this.path, next);
    void next.catch(() => {}).finally(() => queues.get(this.path) === next && queues.delete(this.path));
    return next;
  }

  remove(key: string): Promise<void> {
    return this.update(key, () => undefined);
  }
}
