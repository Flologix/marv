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
import { withFileLock } from "../file-lock.ts";
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

/** Why update() refused: the file exists but this Marv can't read it, so writing would destroy what it holds. */
export class UnusableTokenStore extends Error {
  constructor(path: string) {
    super(`${path} can't be read or is a format this Marv doesn't know (a newer version, or a hand edit): Marv won't write over it. Fix or remove the file, then log in again.`);
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
   * Replaces one entry with what `fn` makes of it; `undefined` removes it. Held under `<store>.lock`
   * (src/file-lock.ts), so two Marv processes can't both read one snapshot and drop each other's update; throws
   * UnusableTokenStore rather than write over a file it can't read.
   */
  update(key: string, fn: (entry: Login | undefined) => Login | undefined): Promise<void> {
    return withFileLock(this.path, async () => {
      const file = await this.read();
      if (file === "unusable") throw new UnusableTokenStore(this.path);
      const entry = fn(file.logins[key]);
      if (entry) file.logins[key] = entry;
      else delete file.logins[key];
      await writePrivate(this.path, JSON.stringify(file, null, 2));
    });
  }

  remove(key: string): Promise<void> {
    return this.update(key, () => undefined);
  }
}
