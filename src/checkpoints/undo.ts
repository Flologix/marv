// /undo (issue #16): what a turn changed, between its before and after checkpoints, and putting those paths back.
// Marv writes each file itself rather than through `git checkout`, which would run the repository's checkout
// filters and follow symlinks; every path is checked to stay inside the project as it is now.
import { lstat, mkdir, readFile, readlink, realpath, rename, rmdir, symlink, unlink, writeFile, chmod } from "node:fs/promises";
import { dirname, join, relative, sep, isAbsolute } from "node:path";
import { addedLines, diffText, shownDiff } from "../tools/diff.ts";
import type { ApprovalRequest, DiffShown } from "../tools/types.ts";
import type { Checkpoint, CheckpointStore } from "./store.ts";

/** What /undo does to one path: put the old content back, delete a file the turn created, or recreate one it deleted. */
export type UndoAction = "restore" | "delete" | "recreate";

export interface UndoFile {
  /** Relative to the project root. */
  path: string;
  action: UndoAction;
  /** The file isn't as the turn left it (the user edited it since): it's restored anyway, the pre-undo snapshot keeps the edit. */
  changedSince: boolean;
  /** The change undoing makes (now → before), for the preview; absent for a binary or very large file. */
  diff?: DiffShown;
  binary?: boolean;
}

export interface UndoPlan {
  turn: number;
  /** The turn's request ("add the parser"). */
  label: string;
  before: Checkpoint;
  after: Checkpoint;
  files: UndoFile[];
  /** Paths the turn's tools reported changing that no snapshot holds (git ignores them): left as they are. */
  notRestored: string[];
  /** The chain's newest checkpoint when the plan was made: if it moved, the plan is stale. */
  head: string;
  /** What each path goes back to: its mode and content in the before snapshot (absent: delete it). */
  targets: Map<string, { mode: string; content: Uint8Array } | null>;
}

export interface UndoResult {
  restored: string[];
  deleted: string[];
  recreated: string[];
  /** Paths left alone, and why (a parent that now leads outside the project). */
  refused: { path: string; reason: string }[];
  /** The snapshot taken first, so the undo itself can be undone by hand, and how long it took. */
  preUndo: string;
  preUndoMs: number;
  preUndoFiles: number;
  /** Why the `undo` marker wasn't written (the turn is kept as undone in memory, for this session only). */
  markerFailed?: string;
}

/** Larger files are named in the preview, not diffed. */
const MAX_DIFFED_BYTES = 1024 * 1024;

/** The newest turn that has a pair and hasn't been undone (by a marker on the chain, or in `alsoUndone`). */
export function nextUndoable(chain: Checkpoint[], alsoUndone: ReadonlySet<number> = new Set()): { before: Checkpoint; after: Checkpoint } | null {
  const undone = new Set([...chain.filter((c) => c.kind === "undo").map((c) => c.turn), ...alsoUndone]);
  for (const [i, after] of chain.entries()) {
    if (after.kind !== "after" || undone.has(after.turn)) continue;
    const before = chain.slice(i + 1).find((c) => c.kind === "before" && c.turn === after.turn);
    if (before) return { before, after };
  }
  return null;
}

const isBinary = (bytes: Uint8Array) => bytes.subarray(0, 8000).includes(0);
const sameBytes = (a: Uint8Array, b: Uint8Array) => a.length === b.length && a.every((byte, i) => byte === b[i]);

/** The path as it is now: its git mode and content (a symlink's target), or null when it's missing (or a folder). */
async function current(absolute: string): Promise<{ mode: string; content: Uint8Array } | null> {
  const info = await lstat(absolute).catch(() => null);
  if (!info) return null;
  if (info.isSymbolicLink()) return { mode: "120000", content: new TextEncoder().encode(await readlink(absolute)) };
  if (!info.isFile()) return null;
  return { mode: info.mode & 0o111 ? "100755" : "100644", content: new Uint8Array(await readFile(absolute)) };
}

/**
 * The newest turn to undo, what undoing it does to each path, and whether a path was edited since the turn ended.
 * `reported`: the paths the turn's tools said they changed, so the ones git ignores can be listed as not restored.
 */
export async function planUndo(
  store: CheckpointStore,
  { reported = [], root }: { reported?: string[] | ((turn: number) => string[]); root?: string } = {},
): Promise<UndoPlan | { reason: string }> {
  const where = await store.locate();
  if ("error" in where) return { reason: `checkpoints can't be read: ${where.error}` };
  const head = await store.head();
  const pair = nextUndoable(await store.chain(), store.undoneTurns);
  if (!head || !pair) return { reason: "nothing to undo: no turn in this session has changed files yet (or they're all undone)" };
  const projectRoot = root ?? join(where.top, where.prefix);
  const listed = await store.git(["diff-tree", "-r", "-z", "--no-renames", pair.before.sha, pair.after.sha, "--", await store.pathspec()]);
  if (!listed.ok) return { reason: `git diff-tree failed: ${listed.out.trim()}` };
  const fields = listed.out.split("\0");
  const files: UndoFile[] = [];
  const targets = new Map<string, { mode: string; content: Uint8Array } | null>();
  const notRestored: string[] = [];
  for (let i = 0; i + 1 < fields.length; i += 2) {
    const [meta = "", topPath = ""] = [fields[i], fields[i + 1]];
    const [oldMode = "", newMode = "", oldSha = "", newSha = "", status = ""] = meta.replace(/^:/, "").split(" ");
    if (!topPath) continue;
    const path = topPath.slice(where.prefix.length);
    // A submodule's commit isn't a file Marv can write back.
    if (oldMode === "160000" || newMode === "160000") {
      notRestored.push(path);
      continue;
    }
    const blob = async (sha: string) => (await store.git(["cat-file", "blob", sha])).bytes;
    const before = status === "A" ? null : { mode: oldMode, content: await blob(oldSha) };
    const after = status === "D" ? null : { mode: newMode, content: await blob(newSha) };
    const now = await current(join(projectRoot, path));
    const changedSince = after === null ? now !== null : now === null || now.mode !== after.mode || !sameBytes(now.content, after.content);
    const action: UndoAction = status === "A" ? "delete" : status === "D" ? "recreate" : "restore";
    const from = now?.content ?? new Uint8Array();
    const to = before?.content ?? new Uint8Array();
    const binary = isBinary(from) || isBinary(to);
    const large = from.length > MAX_DIFFED_BYTES || to.length > MAX_DIFFED_BYTES;
    let diff: DiffShown | undefined;
    if (!binary && !large) {
      const text = (bytes: Uint8Array) => new TextDecoder().decode(bytes);
      diff = shownDiff(now === null ? addedLines(text(to)) : diffText(text(from), text(to)).lines);
    }
    files.push({ path, action, changedSince, ...(diff ? { diff } : {}), ...(binary ? { binary: true } : {}) });
    targets.set(path, before);
  }
  // Reported by a tool but in neither snapshot: git ignores it, so there's nothing to put back.
  const inTree = async (tree: string, path: string) => (await store.git(["cat-file", "-e", `${tree}:${where.prefix}${path}`])).ok;
  for (const path of new Set(typeof reported === "function" ? reported(pair.after.turn) : reported)) {
    if (targets.has(path) || notRestored.includes(path)) continue;
    if (!(await inTree(pair.before.sha, path)) && !(await inTree(pair.after.sha, path))) notRestored.push(path);
  }
  return { turn: pair.after.turn, label: pair.after.label, before: pair.before, after: pair.after, files, notRestored, head, targets };
}

const inside = (root: string, path: string) => {
  const rel = relative(root, path);
  return rel === "" || (!rel.startsWith(`..${sep}`) && rel !== ".." && !isAbsolute(rel));
};

/** Why `path`'s folder can't be written now (it leads outside the project, or isn't a folder), or undefined. */
async function unsafeParent(root: string, path: string): Promise<string | undefined> {
  let folder = dirname(join(root, path));
  // The deepest folder that exists: it must really be inside the project, symlinks resolved.
  while (!(await lstat(folder).catch(() => null))) folder = dirname(folder);
  const real = await realpath(folder).catch(() => null);
  if (!real || !inside(root, real)) return "its folder now leads outside the project";
  const info = await lstat(real).catch(() => null);
  if (!info?.isDirectory()) return "its folder is now a file";
  return undefined;
}

/** Removes the folders above `path` that became empty, up to (never including) the project root. */
async function removeEmptyFolders(root: string, path: string): Promise<void> {
  let folder = dirname(join(root, path));
  while (folder !== root && inside(root, folder)) {
    if (!(await rmdir(folder).then(() => true, () => false))) return;
    folder = dirname(folder);
  }
}

/** Writes `content` at `absolute` as git `mode` says (a file, an executable, a symlink), replacing what's there without following it. */
async function put(root: string, absolute: string, { mode, content }: { mode: string; content: Uint8Array }): Promise<void> {
  await mkdir(dirname(absolute), { recursive: true });
  // Checked again now that the folders exist: one swapped for a symlink since unsafeParent looked is refused.
  const real = await realpath(dirname(absolute)).catch(() => null);
  if (!real || !inside(root, real)) throw new Error("its folder now leads outside the project");
  const temp = join(dirname(absolute), `.marv-undo-${process.pid}-${Math.random().toString(36).slice(2, 8)}`);
  if (mode === "120000") {
    await symlink(new TextDecoder().decode(content), temp);
  } else {
    await writeFile(temp, content, { mode: mode === "100755" ? 0o755 : 0o644 });
    await chmod(temp, mode === "100755" ? 0o755 : 0o644); // the umask may have taken bits off
  }
  const existing = await lstat(absolute).catch(() => null);
  if (existing?.isDirectory()) {
    await unlink(temp).catch(() => {});
    throw new Error("a folder is there now");
  }
  await rename(temp, absolute);
}

/**
 * Applies a plan the user said yes to: a pre-undo snapshot first (so the undo can be undone by hand), then each
 * path, created files deleted before anything is written (a file the turn put where a folder was), and an `undo`
 * marker on the chain, so the next /undo goes one turn further back. Throws if the snapshot fails or the chain
 * moved since the plan was made; a path that can't be written safely is refused and reported, not written.
 */
export async function applyUndo(store: CheckpointStore, plan: UndoPlan, { root }: { root?: string } = {}): Promise<UndoResult> {
  const where = await store.locate();
  if ("error" in where) throw new Error(where.error);
  if ((await store.head()) !== plan.head) throw new Error("the checkpoints changed since this undo was planned: run /undo again");
  const projectRoot = await realpath(root ?? join(where.top, where.prefix));
  const pre = await store.snapshot({ turn: plan.turn, kind: "pre-undo", label: plan.label });
  if (!pre.ok) throw new Error(`couldn't save a snapshot before undoing (${pre.message})`);
  const result: UndoResult = { restored: [], deleted: [], recreated: [], refused: [], preUndo: pre.sha, preUndoMs: pre.ms, preUndoFiles: pre.files };
  const ordered = [...plan.files].sort((a, b) => Number(b.action === "delete") - Number(a.action === "delete"));
  for (const file of ordered) {
    const refuse = (reason: string) => result.refused.push({ path: file.path, reason });
    const why = await unsafeParent(projectRoot, file.path);
    if (why) {
      refuse(why);
      continue;
    }
    const absolute = join(projectRoot, file.path);
    const target = plan.targets.get(file.path);
    try {
      if (!target) {
        const info = await lstat(absolute).catch(() => null);
        if (info?.isDirectory()) {
          refuse("a folder is there now");
          continue;
        }
        if (info) await unlink(absolute);
        await removeEmptyFolders(projectRoot, file.path);
        result.deleted.push(file.path);
      } else {
        await put(projectRoot, absolute, target);
        (file.action === "recreate" ? result.recreated : result.restored).push(file.path);
      }
    } catch (error) {
      refuse((error as Error).message);
    }
  }
  const marker = await store.snapshot({ turn: plan.turn, kind: "undo", label: plan.label });
  if (!marker.ok) {
    // The files are already written: the turn is undone either way, so the next /undo must not pick it again.
    store.undoneTurns.add(plan.turn);
    result.markerFailed = marker.message;
  }
  return result;
}

/** "a.ts", "a.ts and b.ts", "a.ts, b.ts and c.ts". */
const listed = (paths: string[]) => (paths.length < 2 ? (paths[0] ?? "") : `${paths.slice(0, -1).join(", ")} and ${paths.at(-1)}`);

/**
 * What the model is told at the start of the next message after an undo: the files it changed aren't as it left
 * them, so what it remembers of them is wrong. Null when nothing was written.
 */
export function undoNote(plan: UndoPlan, result: UndoResult): string | null {
  const back = [...result.restored, ...result.recreated];
  const parts: string[] = [];
  if (back.length) parts.push(`${listed(back)} ${back.length === 1 ? "is back as it was" : "are back as they were"} before it`);
  if (result.deleted.length) parts.push(`${listed(result.deleted)} ${result.deleted.length === 1 ? "was" : "were"} removed`);
  if (!parts.length) return null;
  const label = plan.label.split("\n")[0]!.slice(0, 80);
  return `Marv: the user undid your changes from an earlier turn${label ? ` ("${label}")` : ""}: ${parts.join(", ")}. Re-read files before editing them.`;
}

const plural = (n: number, word: string) => `${n} ${word}${n === 1 ? "" : "s"}`;
const VERB: Record<UndoAction, string> = { restore: "Restore", delete: "Delete", recreate: "Recreate" };

/** /undo's question: each file and what undoing does to it (its diff, now → before), what's left alone, yes or no. */
export function undoRequest(plan: UndoPlan): ApprovalRequest {
  const label = plan.label.split("\n")[0]!.slice(0, 60);
  const changedSince = plan.files.filter((f) => f.changedSince).map((f) => f.path);
  const text = [
    ...(changedSince.length ? [`Changed since the turn: ${changedSince.join(", ")}`] : []),
    ...(plan.notRestored.length ? [`Not restored (ignored by git): ${plan.notRestored.join(", ")}`] : []),
  ].join("\n");
  return {
    tool: "undo",
    label: `turn ${plan.turn}`,
    once: true,
    scope: { key: "undo", description: "undo" },
    preview: {
      title: `Undo turn ${plan.turn}${label ? ` ("${label}")` : ""}? ${plural(plan.files.length, "file")}`,
      files: plan.files.map((f) => ({
        title: `${VERB[f.action]} ${f.path}`,
        ...(f.diff ? { diff: f.diff } : {}),
        ...(f.binary ? { note: "binary" } : !f.diff ? { note: "too large to show" } : f.changedSince ? { note: "changed since" } : {}),
      })),
      ...(text ? { text } : {}),
      ...(changedSince.length
        ? { warning: `${plural(changedSince.length, "file")} changed since the turn: those edits are undone too (a snapshot taken first keeps them)` }
        : {}),
      note: "nothing is written before you say yes",
    },
  };
}

/** The transcript's line after an undo: "↶ Undid turn 3: 4 files restored", and anything left alone. */
export function undoSummary(plan: UndoPlan, result: UndoResult): string {
  const done = result.restored.length + result.recreated.length + result.deleted.length;
  const parts = [`↶ Undid turn ${plan.turn}: ${plural(done, "file")} restored`];
  if (result.refused.length) parts.push(`not written: ${result.refused.map((r) => `${r.path} (${r.reason})`).join(", ")}`);
  if (plan.notRestored.length) parts.push(`left as they are (ignored by git): ${plan.notRestored.join(", ")}`);
  if (result.markerFailed) parts.push(`the undo couldn't be recorded (${result.markerFailed}): after a restart /undo may offer this turn again`);
  return parts.join("; ");
}
