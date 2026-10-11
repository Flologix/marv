// Saved sessions: every conversation is kept so it can be picked up again.
//
//   ~/.marv/sessions/<project>/<id>.json
//
// A session holds both histories (kept separate, as in the App): the
// transcript the user saw and the conversation the model saw, plus which
// model it used and what it cost. Files are private (0600): they contain
// your code and prompts. Saved after every turn, atomically (write a temp
// file, then rename; see src/private-file.ts), so a crash never leaves a half-written session.
import { existsSync } from "node:fs";
import { readdir, rm } from "node:fs/promises";
import { join } from "node:path";
import { z } from "zod";
import { legacyProjectKey, projectKey } from "./paths.ts";
import { writePrivate } from "./private-file.ts";
import type { ChatTurn } from "./provider/types.ts";
import type { Message } from "./types.ts";
import { emptyTotals, type Totals } from "./usage.ts";

const VERSION = 1;

const SessionShape = z.looseObject({
  version: z.number(),
  id: z.string(),
  root: z.string(),
  updatedAt: z.number(),
  model: z.string(),
  conversation: z.array(z.looseObject({ role: z.string() })),
  transcript: z.array(z.looseObject({ id: z.number(), role: z.string(), text: z.string() })),
  totals: z.looseObject({}),
});
const TITLE_CHARS = 56;

export interface SavedSession {
  version: number;
  id: string;
  root: string;
  createdAt: number;
  updatedAt: number;
  provider: string;
  model: string;
  /** What the model sees. */
  conversation: ChatTurn[];
  /** What the user saw. */
  transcript: Message[];
  totals: Totals;
}

export interface SessionSummary {
  id: string;
  title: string;
  updatedAt: number;
  messages: number;
  model: string;
}

export function newSession(root: string, { provider, model }: { provider: string; model: string }, now = Date.now()): SavedSession {
  const id = `${new Date(now).toISOString().replace(/[:.]/g, "-")}-${Math.random().toString(36).slice(2, 8)}`;
  return { version: VERSION, id, root, createdAt: now, updatedAt: now, provider, model, conversation: [], transcript: [], totals: emptyTotals() };
}


function titleOf(session: SavedSession): string {
  const first = session.transcript.find((m) => m.role === "user")?.text ?? "(empty)";
  const line = first.replace(/\s+/g, " ").trim();
  return line.length > TITLE_CHARS ? `${line.slice(0, TITLE_CHARS - 1).trimEnd()}…` : line;
}

export class SessionStore {
  constructor(readonly dir: string) {}

  private folder(root: string) {
    return join(this.dir, projectKey(root));
  }

  /** Where sessions were saved before project keys had a hash. Shared by colliding projects, so each session's root is checked. */
  private legacyFolder(root: string) {
    return join(this.dir, legacyProjectKey(root));
  }

  /** Where a session is (or will be, once it has a user message) saved. */
  file(root: string, id: string): string {
    return join(this.folder(root), `${id}.json`);
  }

  /** Saves (or overwrites) a session. Sessions without a user message aren't saved. */
  async save(session: SavedSession): Promise<void> {
    if (!session.transcript.some((m) => m.role === "user")) return;
    await writePrivate(this.file(session.root, session.id), JSON.stringify({ ...session, updatedAt: Date.now() }));
    // Saved under the old key before: it lives in the new folder from now on.
    await rm(join(this.legacyFolder(session.root), `${session.id}.json`), { force: true });
  }

  async load(root: string, id: string): Promise<SavedSession | null> {
    return (await this.read(this.file(root, id), root)) ??(await this.read(join(this.legacyFolder(root), `${id}.json`), root));
  }

  /** A session file of this project, or null: missing, damaged, an old format, or (in a legacy folder) another project's. */
  private async read(file: string, root: string): Promise<SavedSession | null> {
    if (!existsSync(file)) return null;
    try {
      // The fields the app relies on; a file without them (an old format, a hand edit) is skipped, not a crash.
      const parsed = SessionShape.safeParse(await Bun.file(file).json());
      if (!parsed.success) return null;
      const session = parsed.data as unknown as SavedSession;
      return session.version === VERSION && session.root === root ? session : null;
    } catch {
      return null;
    }
  }

  /** This project's sessions, newest first. Damaged files are skipped. */
  async list(root: string): Promise<SessionSummary[]> {
    const names = new Set<string>();
    for (const folder of [this.folder(root), this.legacyFolder(root)]) {
      if (existsSync(folder)) for (const name of await readdir(folder)) if (name.endsWith(".json")) names.add(name);
    }
    const sessions: SavedSession[] = [];
    for (const name of names) {
      const session = await this.load(root, name.slice(0, -".json".length));
      if (session) sessions.push(session);
    }
    // Newest first. Two sessions saved in the same millisecond (updatedAt is the save's Date.now()) tie: the one
    // started later comes first, not whichever file the folder happened to list first.
    sessions.sort((a, b) => b.updatedAt - a.updatedAt || b.createdAt - a.createdAt);
    return sessions.map((session) => ({
      id: session.id,
      title: titleOf(session),
      updatedAt: session.updatedAt,
      messages: session.transcript.filter((m) => m.role === "user" || m.role === "assistant").length,
      model: session.model,
    }));
  }

  async latest(root: string): Promise<SavedSession | null> {
    const [newest] = await this.list(root);
    return newest ? this.load(root, newest.id) : null;
  }
}

export function timeAgo(then: number, now = Date.now()): string {
  const minutes = Math.floor((now - then) / 60_000);
  if (minutes < 1) return "just now";
  if (minutes < 60) return `${minutes} minute${minutes === 1 ? "" : "s"} ago`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return `${hours} hour${hours === 1 ? "" : "s"} ago`;
  const days = Math.round(hours / 24);
  return days === 1 ? "yesterday" : `${days} days ago`;
}
