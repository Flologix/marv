// Where a session's checkpoints go (issue #16): git commits of the project's files, written through a temporary
// index so the user's index, HEAD, branches and stash are never touched. In a repository they go into its own
// object store under a hidden ref, refs/marv/checkpoints/<session>, which no branch list, push, fetch or status
// shows; outside one, into a bare git dir of Marv's own (~/.marv/checkpoints/<project>/git). Each checkpoint's
// parent is the one before it, so the ref keeps the whole chain reachable and one `update-ref -d` lets it go.
import { copyFile, lstat, mkdir, mkdtemp, readFile, rm, stat, writeFile } from "node:fs/promises";
import { homedir, tmpdir } from "node:os";
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
/** Files keepOwnBytes stats at once. */
const STAT_BATCH = 256;
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

/** A ref lock older than this was left by a killed git: update-ref holds one for milliseconds. */
const STALE_REF_LOCK_MS = 60_000;

/**
 * Locks a killed git left behind (runGitAsync SIGKILLs on a timeout or a stop), which would fail every later
 * snapshot. Each goes only once it's old enough that no live git can hold it: another Marv may have resumed the
 * same session, and Marv's own index outside a repository is shared by every Marv on the project.
 */
async function clearStaleLocks(where: Location, ref: string): Promise<void> {
  const stale = async (lock: string, ms: number) => {
    const info = await stat(lock).catch(() => null);
    if (info && Date.now() - info.mtimeMs > ms) await rm(lock, { force: true });
  };
  await stale(join(where.gitDir, `${ref}.lock`), STALE_REF_LOCK_MS);
  if (where.kind === "own") await stale(join(where.gitDir, "marv-index.lock"), STALE_LOCK_MS);
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
  /**
   * The working bytes' blob of each file whose stored blob git's filters changed (keepOwnBytes), with the file's
   * stat when it was hashed: a big LFS asset is read once a session while it doesn't change, not every turn.
   */
  private readonly ownBytes = new Map<string, { stamp: string; sha: string }>();

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
  async git(args: string[], { index, signal, timeoutMs, input }: { index?: string; signal?: AbortSignal; timeoutMs?: number; input?: string } = {}): Promise<GitAsyncResult> {
    const where = await this.locate();
    if ("error" in where) return { ok: false, out: where.error, bytes: new Uint8Array(), timedOut: false };
    const time = this.options.now ? `${Math.floor(this.options.now() / 1000)} +0000` : undefined;
    const env: Record<string, string> = { ...IDENTITY, GIT_DIR: where.gitDir, GIT_WORK_TREE: where.top, ...(time ? { GIT_AUTHOR_DATE: time, GIT_COMMITTER_DATE: time } : {}) };
    if (index) env.GIT_INDEX_FILE = index;
    return runGitAsync(where.top, args, { env, signal, timeoutMs, input });
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
      const written = await this.git(["write-tree"], { index, signal });
      if (!written.ok) return fail(written.out || "git write-tree failed");
      const tree = await this.keepOwnBytes(where, index, written.out.trim(), signal);
      if (!tree.ok) return fail(tree.out);
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

  /**
   * `add -A` stores what git's clean side makes of a file: an LFS pointer, LF for a CRLF file (eol, autocrlf), a
   * collapsed `$Id$`, UTF-8 for a re-encoded file. Undo writes blobs back without running checkout's smudge side (a
   * program, maybe a download), so the snapshot must hold the file's own bytes: each blob whose size differs from
   * the file's (an eol conversion only ever drops CRs) or whose path has a `filter`, `ident` or
   * `working-tree-encoding` attribute (a filter may keep the size) is hashed again from the file with
   * `--no-filters` and put in the temporary index. Returns the tree to commit.
   */
  private async keepOwnBytes(where: Location, index: string, tree: string, signal?: AbortSignal): Promise<{ ok: true; out: string } | { ok: false; out: string }> {
    if (!(await this.mayConvert(where, index, signal))) return { ok: true, out: tree };
    const listed = await this.git(["ls-tree", "-r", "-l", "-z", tree], { signal });
    if (!listed.ok) return { ok: false, out: listed.out || "git ls-tree failed" };
    const blobs: { mode: string; sha: string; size: number; path: string }[] = [];
    for (const entry of listed.out.split("\0")) {
      const tab = entry.indexOf("\t");
      const [mode = "", type = "", sha = "", size = ""] = entry.slice(0, Math.max(tab, 0)).split(/ +/);
      const path = entry.slice(tab + 1);
      // Symlinks and submodules go through no filter; a path with a newline can't go through --stdin-paths.
      if (tab < 0 || type !== "blob" || mode === "120000" || !path.startsWith(where.prefix) || path.includes("\n")) continue;
      blobs.push({ mode, sha, size: Number(size), path });
    }
    if (!blobs.length) return { ok: true, out: tree };
    const attrs = await this.git(["check-attr", "-z", "--stdin", "filter", "ident", "working-tree-encoding"], {
      index,
      signal,
      input: blobs.map((b) => `${b.path}\0`).join(""),
    });
    if (!attrs.ok) return { ok: false, out: attrs.out || "git check-attr failed" };
    const filtered = new Set<string>();
    const fields = attrs.out.split("\0");
    for (let i = 0; i + 2 < fields.length; i += 3) if (fields[i + 2] !== "unspecified" && fields[i + 2] !== "unset") filtered.add(fields[i]!);

    // The files' sizes as `add -A` just saw them: the index's stat data, which it refreshed (stat'ing every file
    // from here took most of the snapshot's time). A path it doesn't give is stat'ed below.
    const debug = await this.git(["ls-files", "--debug", "-s", "-z", "--", await this.pathspec()], { index, signal });
    const sizes = new Map<string, number>();
    if (debug.ok) for (const m of debug.out.matchAll(/\d{6} [0-9a-f]+ \d\t([^\0]*)\0[^\0]*?size: (\d+)\t/g)) sizes.set(m[1]!, Number(m[2]));
    const candidates = blobs.filter((b) => filtered.has(b.path) || sizes.get(b.path) !== b.size);
    const changed: { mode: string; path: string; stamp: string }[] = [];
    for (let start = 0; start < candidates.length; start += STAT_BATCH) {
      const batch = candidates.slice(start, start + STAT_BATCH);
      const infos = await Promise.all(batch.map((b) => lstat(join(where.top, b.path), { bigint: true }).catch(() => null)));
      batch.forEach((blob, i) => {
        const info = infos[i];
        // Not a file here (skip-worktree, deleted meanwhile), or the same size after all: git's blob is the file.
        if (!info?.isFile() || (Number(info.size) === blob.size && !filtered.has(blob.path))) return;
        changed.push({ mode: blob.mode, path: blob.path, stamp: `${info.size}:${info.mtimeNs}:${info.ctimeNs}:${info.ino}` });
      });
    }
    if (!changed.length) return { ok: true, out: tree };
    // Hashed before and unchanged since: reused, if the object is still there (a dropped pair's may be collected).
    const known = changed.filter((c) => this.ownBytes.get(c.path)?.stamp === c.stamp);
    const present = new Set<string>();
    if (known.length) {
      const checked = await this.git(["cat-file", "--batch-check=%(objectname)"], { signal, input: known.map((c) => `${this.ownBytes.get(c.path)!.sha}\n`).join("") });
      if (checked.ok) for (const line of checked.out.split("\n")) if (/^[0-9a-f]+$/.test(line)) present.add(line);
    }
    const shas = new Map<string, string>();
    for (const c of known) {
      const sha = this.ownBytes.get(c.path)!.sha;
      if (present.has(sha)) shas.set(c.path, sha);
    }
    const fresh = changed.filter((c) => !shas.has(c.path));
    if (fresh.length) {
      const hashed = await this.git(["hash-object", "-w", "--no-filters", "--stdin-paths"], { signal, input: fresh.map((c) => `${c.path}\n`).join("") });
      if (!hashed.ok) return { ok: false, out: hashed.out || "git hash-object failed" };
      const lines = hashed.out.split("\n").filter(Boolean);
      if (lines.length !== fresh.length) return { ok: false, out: "git hash-object answered for fewer files than asked" };
      fresh.forEach((c, i) => {
        shas.set(c.path, lines[i]!);
        this.ownBytes.set(c.path, { stamp: c.stamp, sha: lines[i]! });
      });
    }
    const updated = await this.git(["update-index", "-z", "--index-info"], {
      index,
      signal,
      input: changed.map((c) => `${c.mode} ${shas.get(c.path)}\t${c.path}\0`).join(""),
    });
    if (!updated.ok) return { ok: false, out: updated.out || "git update-index failed" };
    const rewritten = await this.git(["write-tree"], { index, signal });
    return rewritten.ok ? { ok: true, out: rewritten.out.trim() } : { ok: false, out: rewritten.out || "git write-tree failed" };
  }

  /**
   * Whether git could store anything but a file's own bytes here: core.autocrlf is on, or some attributes file
   * (the tree's .gitattributes, info/attributes, the user's and the system's) names an attribute that converts.
   * Read as text, so a comment or a macro using one counts too: a needless check costs time, never bytes. Most
   * projects have none, and the check (check-attr over every path) is most of a snapshot's time on a big tree.
   */
  private async mayConvert(where: Location, index: string, signal?: AbortSignal): Promise<boolean> {
    // Every .gitattributes in the work tree (a parent folder's applies to a subfolder project too).
    const [config, listed, info] = await Promise.all([
      this.git(["config", "-z", "--type=path", "--get-regexp", "^core\\.(autocrlf|attributesfile)$"], { signal }),
      this.git(["ls-files", "-z", "--full-name", "--", ":(top,glob)**/.gitattributes"], { index, signal }),
      this.git(["rev-parse", "--git-path", "info/attributes"], { signal }),
    ]);
    const attributeFiles = listed.ok ? listed.out.split("\0").filter(Boolean) : [];
    let userFile = join(process.env.XDG_CONFIG_HOME || join(homedir(), ".config"), "git", "attributes");
    for (const pair of config.ok ? config.out.split("\0") : []) {
      const [key = "", value = ""] = pair.split("\n");
      if (key === "core.autocrlf" && ["true", "input"].includes(value.trim().toLowerCase())) return true;
      if (key === "core.attributesfile") userFile = resolve(where.top, value);
    }
    const files = [...attributeFiles.map((path) => join(where.top, path)), userFile, "/etc/gitattributes"];
    if (info.ok) files.push(resolve(where.top, info.out.trim()));
    const texts = await Promise.all(files.map((file) => readFile(file, "utf8").catch(() => "")));
    return texts.some((text) => /\b(filter|ident|working-tree-encoding|text|eol|crlf)\b/.test(text));
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
    // A chain that exists but can't be read isn't an empty one: numbering from 0 would collide with its turns.
    if (!listed.ok) throw new Error(`the checkpoints can't be read (${listed.timedOut ? "git timed out" : listed.out.trim() || "git rev-list failed"})`);
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
