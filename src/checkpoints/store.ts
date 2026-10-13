// Where a session's checkpoints go (issue #16): git commits of the project's files, written through a temporary
// index so the user's index, HEAD, branches and stash are never touched. In a repository they go into its own
// object store under a hidden ref, refs/marv/checkpoints/<session>, which no branch list, push, fetch or status
// shows; outside one, into a bare git dir of Marv's own (~/.marv/checkpoints/<project>/git). Each checkpoint's
// parent is the one before it, so the ref keeps the whole chain reachable and one `update-ref -d` lets it go.
import { copyFile, mkdir, mkdtemp, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { isAbsolute, join, resolve } from "node:path";
import { runGitAsync, type GitAsyncResult } from "../git.ts";
import { IGNORED_DIRS } from "../tools/files.ts";

export type CheckpointKind = "before" | "after" | "pre-undo" | "undo";

export interface Checkpoint {
  sha: string;
  tree: string;
  turn: number;
  kind: CheckpointKind;
  /** The turn's request, first line ("add the parser"). */
  label: string;
}

export type SnapshotResult =
  | { ok: true; sha: string; tree: string; files: number; ms: number }
  | { ok: false; reason: "too-large" | "failed"; message: string; ms: number };

/** Where snapshots are written: git's dir (refs, objects), the work tree's top, and the project's place in it. */
export type Location =
  | { kind: "repo"; gitDir: string; top: string; prefix: string }
  | { kind: "own"; gitDir: string; top: string; prefix: "" };

/** Outside a repository nothing like .gitignore keeps a home folder's worth of files out: past these, checkpoints stop. */
export const OWN_LIMITS = { files: 20_000, bytes: 200 * 1024 * 1024 };

/** Marv's own identity on every checkpoint, so a repository without user.email works, and they're easy to tell apart. */
const IDENTITY = { GIT_AUTHOR_NAME: "Marv", GIT_AUTHOR_EMAIL: "marv@localhost", GIT_COMMITTER_NAME: "Marv", GIT_COMMITTER_EMAIL: "marv@localhost" };
const REF_PREFIX = "refs/marv/checkpoints/";
const SUBJECT = /^marv checkpoint: turn (\d+) (before|after|pre-undo|undo)$/;
const DAY_MS = 86_400_000;
export const NOT_A_REPOSITORY = "the project isn't in a git repository";

/** A checkpoint's commit message: "marv checkpoint: turn 3 before", then the turn's request on its own line. */
export function checkpointMessage(turn: number, kind: CheckpointKind, label: string): string {
  const line = label.replace(/\s+/g, " ").trim().slice(0, 80);
  return `marv checkpoint: turn ${turn} ${kind}${line ? `\n\n${line}` : ""}\n`;
}

export function parseMessage(message: string): { turn: number; kind: CheckpointKind; label: string } | null {
  const [subject = "", ...rest] = message.split("\n");
  const match = SUBJECT.exec(subject.trim());
  if (!match) return null;
  return { turn: Number(match[1]), kind: match[2] as CheckpointKind, label: rest.join("\n").trim() };
}

export interface StoreOptions {
  /** The project root. */
  root: string;
  /** ~/.marv/checkpoints/<projectKey>: Marv's own git dir, for a project that isn't in a repository. Without it, such a project gets none. */
  ownDir?: string;
  /** The session id: one ref per session. */
  session: string;
  limits?: { files: number; bytes: number };
  /** The checkpoints' commit time (tests). */
  now?: () => number;
}

/** An index lock older than this was left by a git that was killed (runGitAsync's timeouts are far shorter). */
const STALE_LOCK_MS = 10 * 60_000;

/**
 * Locks a killed git left behind (runGitAsync SIGKILLs on a timeout or a stop), which would fail every later
 * snapshot. This session's ref lock is always stale here: its snapshots run one at a time. Marv's own index outside
 * a repository is shared by every Marv on the project, so its lock goes only once it's old.
 */
async function clearStaleLocks(where: Location, ref: string): Promise<void> {
  await rm(join(where.gitDir, `${ref}.lock`), { force: true });
  if (where.kind !== "own") return;
  const lock = join(where.gitDir, "marv-index.lock");
  const info = await stat(lock).catch(() => null);
  if (info && Date.now() - info.mtimeMs > STALE_LOCK_MS) await rm(lock, { force: true });
}

export class CheckpointStore {
  /** Why checkpoints are off for this project this session (too large outside a repository), once it is so. */
  off: string | undefined;
  private session: string;
  private located: Promise<Location | { error: string }> | undefined;
  /**
   * Snapshots, ref moves and pruning, one at a time: a prune's gc running beside a snapshot would collect the tree
   * it just wrote before the commit points at it, and two snapshots would race for the ref.
   */
  private queue: Promise<unknown> = Promise.resolve();

  constructor(private readonly options: StoreOptions) {
    this.session = options.session;
  }

  /**
   * Turns undone whose `undo` marker couldn't be written: without one the chain still shows the turn as the newest
   * undoable one, and a second /undo would put its files back again over what's there now.
   */
  readonly undoneTurns = new Set<number>();

  /** A new conversation (clear) or an old one (resume): its own chain. */
  setSession(id: string): void {
    this.session = id;
    this.undoneTurns.clear();
  }

  get ref(): string {
    return REF_PREFIX + this.session;
  }

  /** Found once per store: the repository around the project, or Marv's own git dir (made on first use). */
  locate(): Promise<Location | { error: string }> {
    this.located ??= this.find();
    return this.located;
  }

  /** git with the checkpoint's git dir and work tree pinned, run in the work tree's top. */
  async git(args: string[], { index, signal, timeoutMs }: { index?: string; signal?: AbortSignal; timeoutMs?: number } = {}): Promise<GitAsyncResult> {
    const where = await this.locate();
    if ("error" in where) return { ok: false, out: where.error, bytes: new Uint8Array(), timedOut: false };
    const time = this.options.now ? `${Math.floor(this.options.now() / 1000)} +0000` : undefined;
    const env: Record<string, string> = { ...IDENTITY, GIT_DIR: where.gitDir, GIT_WORK_TREE: where.top, ...(time ? { GIT_AUTHOR_DATE: time, GIT_COMMITTER_DATE: time } : {}) };
    if (index) env.GIT_INDEX_FILE = index;
    return runGitAsync(where.top, args, { env, signal, timeoutMs });
  }

  /** The project's files, as a pathspec in the work tree ("." or ":(literal)pkg/"). */
  async pathspec(): Promise<string> {
    const where = await this.locate();
    return "error" in where || !where.prefix ? "." : `:(literal)${where.prefix}`;
  }

  /** The newest checkpoint of this session, or null. */
  async head(): Promise<string | null> {
    const found = await this.git(["rev-parse", "--verify", "--quiet", `${this.ref}^{commit}`]);
    return found.ok ? found.out.trim() : null;
  }

  /**
   * Snapshots the project's files (what `git add -A` would stage: tracked plus untracked, non-ignored ones) as a
   * new checkpoint on this session's chain. Never throws.
   */
  snapshot(options: { turn: number; kind: CheckpointKind; label: string; signal?: AbortSignal }): Promise<SnapshotResult> {
    return this.serial(() => this.write(options));
  }

  private async write({ turn, kind, label, signal }: { turn: number; kind: CheckpointKind; label: string; signal?: AbortSignal }): Promise<SnapshotResult> {
    const started = performance.now();
    const ms = () => Math.round(performance.now() - started);
    const fail = (message: string, reason: "failed" | "too-large" = "failed"): SnapshotResult => ({ ok: false, reason, message: message.trim(), ms: ms() });
    if (this.off) return fail(this.off, "too-large");
    const where = await this.locate();
    if ("error" in where) return fail(where.error);
    let temp: string | undefined;
    try {
      await clearStaleLocks(where, this.ref);
      let index: string;
      if (where.kind === "repo") {
        // A copy of the user's index: its stat cache makes `add -A` rehash only what changed, and it's what says a
        // force-added or skip-worktree file is tracked (from an empty index those would look new, and /undo would
        // delete them). The user's own index is only read.
        temp = await mkdtemp(join(tmpdir(), "marv-checkpoint-"));
        index = join(temp, "index");
        await copyFile(join(where.gitDir, "index"), index).catch(() => {}); // a new repository has none: start empty
      } else {
        index = join(where.gitDir, "marv-index"); // Marv's own: kept, for its stat cache
        const big = await this.tooLarge(index, signal);
        if (big) {
          this.off = big;
          return fail(big, "too-large");
        }
      }
      const spec = await this.pathspec();
      const added = await this.git(["add", "-A", "--", spec], { index, signal });
      if (!added.ok) return fail(added.timedOut ? "git add timed out" : added.out || "git add failed");
      const tree = await this.git(["write-tree"], { index, signal });
      if (!tree.ok) return fail(tree.out || "git write-tree failed");
      const listed = await this.git(["ls-files", "-z", "--", spec], { index, signal });
      const files = listed.ok ? listed.out.split("\0").filter(Boolean).length : 0;
      const previous = await this.head();
      const commit = await this.git(
        ["commit-tree", "--no-gpg-sign", tree.out.trim(), ...(previous ? ["-p", previous] : []), "-m", checkpointMessage(turn, kind, label)],
        { signal },
      );
      if (!commit.ok) return fail(commit.out || "git commit-tree failed");
      const sha = commit.out.trim();
      // Compare-and-swap: a ref that moved meanwhile fails loudly rather than losing a checkpoint.
      const moved = await this.git(["update-ref", "-m", "marv checkpoint", this.ref, sha, previous ?? ""], { signal });
      if (!moved.ok) return fail(moved.out || "git update-ref failed");
      return { ok: true, sha, tree: tree.out.trim(), files, ms: ms() };
    } catch (error) {
      return fail((error as Error).message);
    } finally {
      if (temp) await rm(temp, { recursive: true, force: true }).catch(() => {});
    }
  }

  private serial<T>(work: () => Promise<T>): Promise<T> {
    const run = this.queue.then(work, work);
    this.queue = run.catch(() => {});
    return run;
  }

  /** Moves the ref back to `sha` (a pair that changed nothing), or deletes it (null). */
  dropTo(sha: string | null): Promise<boolean> {
    return this.serial(() => this.moveTo(sha));
  }

  private async moveTo(sha: string | null): Promise<boolean> {
    const current = await this.head();
    if (!current) return sha === null;
    const result = sha ? await this.git(["update-ref", this.ref, sha, current]) : await this.git(["update-ref", "-d", this.ref, current]);
    return result.ok;
  }

  /** This session's checkpoints, newest first (at most `max`). */
  async chain(max = 1000): Promise<Checkpoint[]> {
    const head = await this.head();
    if (!head) return [];
    const listed = await this.git(["rev-list", `--max-count=${max}`, "--format=%x1e%H%x00%T%x00%B", head]);
    if (!listed.ok) return [];
    const chain: Checkpoint[] = [];
    for (const chunk of listed.out.split("\x1e").slice(1)) {
      const [sha = "", tree = "", message = ""] = chunk.split("\0");
      const parsed = parseMessage(message.replace(/\ncommit [0-9a-f]+\s*$/, ""));
      if (parsed) chain.push({ sha: sha.trim(), tree: tree.trim(), ...parsed });
    }
    return chain;
  }

  /**
   * Keeps the newest `keep` sessions' chains of this project and anything updated in the last `days`; deletes the
   * other refs (never this session's). In the user's repository that's all (their next gc takes the objects); in
   * Marv's own git dir, Marv collects them, at most once a day.
   */
  prune(options: { keep: number; days: number }): Promise<void> {
    return this.serial(() => this.pruneNow(options));
  }

  private async pruneNow({ keep, days }: { keep: number; days: number }): Promise<void> {
    const where = await this.locate();
    if ("error" in where) return;
    const listed = await this.git(["for-each-ref", "--sort=-committerdate", "--format=%(refname)%00%(committerdate:unix)", REF_PREFIX]);
    if (!listed.ok) return;
    const now = this.options.now?.() ?? Date.now();
    const refs = listed.out.split("\n").filter(Boolean).map((line) => {
      const [name = "", time = "0"] = line.split("\0");
      return { name, ms: Number(time) * 1000 };
    });
    for (const [i, ref] of refs.entries()) {
      if (i < keep || ref.name === this.ref || now - ref.ms < days * DAY_MS) continue;
      await this.git(["update-ref", "-d", ref.name]);
    }
    if (where.kind !== "own" || !this.options.ownDir) return;
    const stamp = join(this.options.ownDir, "gc-stamp");
    const last = await stat(stamp).then((s) => s.mtimeMs, () => 0);
    if (Date.now() - last < DAY_MS) return;
    await writeFile(stamp, "");
    // Not --prune=now: another Marv in the same project may be between writing a tree and committing it.
    await this.git(["gc", "--prune=1.hour.ago", "--quiet"], { timeoutMs: 120_000 });
  }

  private async find(): Promise<Location | { error: string }> {
    const root = this.options.root;
    const found = await runGitAsync(root, ["rev-parse", "--absolute-git-dir", "--show-toplevel", "--show-prefix"], { timeoutMs: 10_000 });
    if (found.ok) {
      const [gitDir = "", top = "", prefix = ""] = found.out.split("\n");
      if (gitDir && top) return { kind: "repo", gitDir, top, prefix };
    }
    // Not a repository (or an unusable one): Marv's own bare git dir, with the project as its work tree.
    const ownDir = this.options.ownDir;
    if (!ownDir) return { error: NOT_A_REPOSITORY };
    const gitDir = join(ownDir, "git");
    try {
      await mkdir(gitDir, { recursive: true });
      const init = await runGitAsync(ownDir, ["init", "--bare", "-q", gitDir], { timeoutMs: 10_000 });
      if (!init.ok) return { error: init.out.trim() || "git init failed" };
      // No .gitignore applies here: the file listing's skip list keeps dependencies and build output out.
      await mkdir(join(gitDir, "info"), { recursive: true });
      await writeFile(join(gitDir, "info", "exclude"), [...IGNORED_DIRS].map((name) => `${name}/`).join("\n") + "\n");
    } catch (error) {
      return { error: (error as Error).message };
    }
    const top = isAbsolute(root) ? root : resolve(root);
    return { kind: "own", gitDir, top, prefix: "" };
  }

  /** Outside a repository: why this project is too large to snapshot (files, or new content), or undefined. */
  private async tooLarge(index: string, signal?: AbortSignal): Promise<string | undefined> {
    const { files, bytes } = this.options.limits ?? OWN_LIMITS;
    const all = await this.git(["ls-files", "-z", "--cached", "--others", "--exclude-standard"], { index, signal });
    if (!all.ok) return undefined; // add -A reports what's wrong
    const count = all.out.split("\0").filter(Boolean).length;
    if (count > files) return `the project is too large for checkpoints outside a git repository (${count} files, the limit is ${files})`;
    const changed = await this.git(["ls-files", "-z", "--others", "--modified", "--exclude-standard"], { index, signal });
    if (!changed.ok) return undefined;
    let total = 0;
    for (const path of changed.out.split("\0").filter(Boolean)) {
      total += await stat(join(this.options.root, path)).then((s) => (s.isFile() ? s.size : 0), () => 0);
      if (total > bytes) return `the project is too large for checkpoints outside a git repository (over ${Math.round(bytes / 1024 / 1024)} MB of new content)`;
    }
    return undefined;
  }
}
