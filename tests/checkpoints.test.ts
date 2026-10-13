import { afterEach, describe, expect, test } from "bun:test";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, statSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { CheckpointStore, parseMessage } from "../src/checkpoints/store.ts";
import { applyUndo, nextUndoable, planUndo, undoSummary } from "../src/checkpoints/undo.ts";
import { lstatSync, readlinkSync, unlinkSync } from "node:fs";

const dirs: string[] = [];
afterEach(() => {
  for (const dir of dirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

export function tempDir(prefix = "marv-cp-"): string {
  const dir = realpathSync(mkdtempSync(join(tmpdir(), prefix)));
  dirs.push(dir);
  return dir;
}

export const git = (cwd: string, ...args: string[]) => {
  const r = Bun.spawnSync(["git", ...args], { cwd });
  return r.stdout.toString().trim();
};

/** A repository with one commit; `email: false` leaves user.email unset (the commit gets it from -c). */
export function makeRepo({ email = true }: { email?: boolean } = {}): string {
  const dir = tempDir("marv-cp-repo-");
  git(dir, "init", "-q", "-b", "main");
  if (email) {
    git(dir, "config", "user.email", "t@example.com");
    git(dir, "config", "user.name", "T");
  }
  writeFileSync(join(dir, "a.txt"), "one\n");
  writeFileSync(join(dir, ".gitignore"), "ignored.txt\nbuild/\n");
  git(dir, "add", "-A");
  git(dir, "-c", "user.email=t@example.com", "-c", "user.name=T", "commit", "-q", "-m", "first");
  return dir;
}

const store = (root: string, session = "s1", ownDir = tempDir("marv-cp-own-")) => new CheckpointStore({ root, ownDir, session });
const treeFiles = (cwd: string, sha: string) => git(cwd, "ls-tree", "-r", "--name-only", sha).split("\n").filter(Boolean);

function state(repo: string) {
  return {
    head: git(repo, "rev-parse", "HEAD"),
    index: readFileSync(join(repo, ".git", "index")),
    branches: git(repo, "branch", "-a"),
    stash: git(repo, "stash", "list"),
    status: git(repo, "status", "--porcelain"),
  };
}

describe("locate", () => {
  test("a repository's top, a subfolder, no repository", async () => {
    const repo = makeRepo();
    expect(await store(repo).locate()).toMatchObject({ kind: "repo", gitDir: join(repo, ".git"), top: repo, prefix: "" });
    mkdirSync(join(repo, "pkg"));
    expect(await store(join(repo, "pkg")).locate()).toMatchObject({ kind: "repo", top: repo, prefix: "pkg/" });
    const plain = tempDir();
    const own = tempDir();
    const located = await store(plain, "s1", own).locate();
    expect(located).toMatchObject({ kind: "own", gitDir: join(own, "git"), top: plain, prefix: "" });
    expect(existsSync(join(own, "git", "HEAD"))).toBe(true);
    expect(readFileSync(join(own, "git", "info", "exclude"), "utf8")).toContain("node_modules/");
  });

  test("a git worktree: its own git dir for the index", async () => {
    const repo = makeRepo();
    const wt = join(tempDir(), "wt");
    git(repo, "worktree", "add", "-q", "-b", "side", wt);
    const located = await store(wt).locate();
    expect(located).toMatchObject({ kind: "repo", top: wt, prefix: "" });
    expect(located && "gitDir" in located && located.gitDir).toStartWith(join(repo, ".git", "worktrees"));
  });
});

describe("snapshot", () => {
  test("tracked, modified, untracked and force-added files, not ignored ones; the user's state is untouched", async () => {
    const repo = makeRepo();
    writeFileSync(join(repo, "forced.txt"), "f\n");
    writeFileSync(join(repo, ".gitignore"), "ignored.txt\nbuild/\nforced.txt\n");
    git(repo, "add", "-f", "forced.txt", ".gitignore");
    git(repo, "commit", "-q", "-m", "forced");
    writeFileSync(join(repo, "a.txt"), "changed\n");
    writeFileSync(join(repo, "new.txt"), "new\n");
    writeFileSync(join(repo, "ignored.txt"), "secret\n");
    git(repo, "stash", "list");
    const before = state(repo);
    const s = store(repo);
    const result = await s.snapshot({ turn: 1, kind: "before", label: "do it" });
    expect(result.ok).toBe(true);
    if (!result.ok) return;
    const files = treeFiles(repo, result.sha);
    expect(files.sort()).toEqual([".gitignore", "a.txt", "forced.txt", "new.txt"]);
    expect(git(repo, "show", `${result.sha}:a.txt`)).toBe("changed");
    expect(state(repo)).toEqual(before);
    expect(git(repo, "rev-parse", "refs/marv/checkpoints/s1")).toBe(result.sha);

    const second = await s.snapshot({ turn: 1, kind: "after", label: "do it" });
    expect(second.ok && git(repo, "rev-parse", `${second.sha}^`)).toBe(result.sha);
    expect(state(repo)).toEqual(before);
  });

  test("a subfolder project's tree holds only its prefix", async () => {
    const repo = makeRepo();
    mkdirSync(join(repo, "pkg"));
    writeFileSync(join(repo, "pkg", "x.txt"), "x\n");
    writeFileSync(join(repo, "outside.txt"), "o\n");
    const result = await store(join(repo, "pkg")).snapshot({ turn: 1, kind: "before", label: "" });
    expect(result.ok).toBe(true);
    if (!result.ok) return;
    const files = treeFiles(repo, result.sha);
    expect(files).toContain("pkg/x.txt");
    expect(files).not.toContain("outside.txt");
  });

  test("works without user.email", async () => {
    const repo = makeRepo({ email: false });
    const result = await store(repo).snapshot({ turn: 1, kind: "before", label: "" });
    expect(result.ok).toBe(true);
    expect(git(repo, "log", "-1", "--format=%an <%ae>", "refs/marv/checkpoints/s1")).toBe("Marv <marv@localhost>");
  });

  test("never runs a hook or fsmonitor", async () => {
    const repo = makeRepo();
    const marker = join(repo, "..", `ran-${Date.now()}`);
    const script = join(repo, ".git", "touch.sh");
    writeFileSync(script, `#!/bin/sh\ntouch ${marker}\n`);
    chmodSync(script, 0o755);
    for (const hook of ["pre-commit", "post-commit", "reference-transaction"]) {
      writeFileSync(join(repo, ".git", "hooks", hook), `#!/bin/sh\ntouch ${marker}\n`);
      chmodSync(join(repo, ".git", "hooks", hook), 0o755);
    }
    git(repo, "config", "core.fsmonitor", script);
    writeFileSync(join(repo, "a.txt"), "x\n");
    expect((await store(repo).snapshot({ turn: 1, kind: "before", label: "" })).ok).toBe(true);
    expect(existsSync(marker)).toBe(false);
  });

  test("own git dir: the skip list applies, the index persists, too large stops", async () => {
    const plain = tempDir();
    const own = tempDir();
    mkdirSync(join(plain, "node_modules"));
    writeFileSync(join(plain, "node_modules", "x.js"), "x");
    writeFileSync(join(plain, "a.txt"), "a\n");
    const s = store(plain, "s1", own);
    const result = await s.snapshot({ turn: 1, kind: "before", label: "" });
    expect(result.ok).toBe(true);
    if (!result.ok) return;
    expect(treeFiles(join(own, "git"), result.sha)).toEqual(["a.txt"]);
    expect(existsSync(join(own, "git", "marv-index"))).toBe(true);

    const big = tempDir();
    for (let i = 0; i < 6; i++) writeFileSync(join(big, `f${i}.txt`), "x");
    const small = new CheckpointStore({ root: big, ownDir: tempDir(), session: "s1", limits: { files: 5, bytes: 1e9 } });
    const tooLarge = await small.snapshot({ turn: 1, kind: "before", label: "" });
    expect(tooLarge).toMatchObject({ ok: false, reason: "too-large" });
    expect(small.off).toContain("too large");
    const sized = new CheckpointStore({ root: big, ownDir: tempDir(), session: "s1", limits: { files: 100, bytes: 3 } });
    expect(await sized.snapshot({ turn: 1, kind: "before", label: "" })).toMatchObject({ ok: false, reason: "too-large" });
  });
});

describe("the chain", () => {
  test("dropTo moves the ref back; chain() parses turn and kind, newest first", async () => {
    const repo = makeRepo();
    const s = store(repo);
    const one = await s.snapshot({ turn: 1, kind: "before", label: "add the parser" });
    const two = await s.snapshot({ turn: 1, kind: "after", label: "add the parser" });
    const chain = await s.chain();
    expect(chain.map((c) => [c.turn, c.kind, c.label])).toEqual([
      [1, "after", "add the parser"],
      [1, "before", "add the parser"],
    ]);
    expect(two.ok && chain[0]!.sha).toBe(two.ok ? two.sha : "");
    await s.dropTo(one.ok ? one.sha : null);
    expect((await s.chain()).length).toBe(1);
    await s.dropTo(null);
    expect(await s.chain()).toEqual([]);
    expect(git(repo, "for-each-ref", "refs/marv/")).toBe("");
  });

  test("parseMessage", () => {
    expect(parseMessage("marv checkpoint: turn 3 pre-undo\n\nfix it\n")).toEqual({ turn: 3, kind: "pre-undo", label: "fix it" });
    expect(parseMessage("something else")).toBeNull();
  });

  test("prune keeps the newest sessions and the recent ones", async () => {
    const repo = makeRepo();
    const day = 86_400_000;
    const now = Date.now();
    // Five sessions: two old, three recent. Keep 2 + anything newer than 14 days.
    for (const [i, age] of [40, 30, 3, 2, 1].entries()) {
      const s = new CheckpointStore({ root: repo, ownDir: tempDir(), session: `s${i}`, now: () => now - age * day });
      expect((await s.snapshot({ turn: 1, kind: "before", label: "" })).ok).toBe(true);
    }
    await new CheckpointStore({ root: repo, ownDir: tempDir(), session: "s4" }).prune({ keep: 2, days: 14 });
    expect(git(repo, "for-each-ref", "--format=%(refname:lstrip=3)", "refs/marv/checkpoints/").split("\n").sort()).toEqual(["s2", "s3", "s4"]);
  });

  test("prune in the own git dir runs gc at most once a day", async () => {
    const plain = tempDir();
    const own = tempDir();
    writeFileSync(join(plain, "a.txt"), "a");
    const s = store(plain, "s1", own);
    await s.snapshot({ turn: 1, kind: "before", label: "" });
    await s.prune({ keep: 20, days: 14 });
    const stamp = statSync(join(own, "gc-stamp")).mtimeMs;
    await Bun.sleep(20);
    await s.prune({ keep: 20, days: 14 });
    expect(statSync(join(own, "gc-stamp")).mtimeMs).toBe(stamp);
  });
});


describe("undo", () => {
  /** A turn: snapshot before, `change` the files, snapshot after. */
  async function turn(s: CheckpointStore, n: number, change: () => void, label = `turn ${n}`) {
    expect((await s.snapshot({ turn: n, kind: "before", label })).ok).toBe(true);
    change();
    expect((await s.snapshot({ turn: n, kind: "after", label })).ok).toBe(true);
  }

  test("modified, created, deleted, mode change and symlink are put back; empty folders go", async () => {
    const repo = makeRepo();
    writeFileSync(join(repo, "gone.txt"), "keep me\n");
    mkdirSync(join(repo, "deep"));
    writeFileSync(join(repo, "deep", "gone2.txt"), "deep\n");
    writeFileSync(join(repo, "run.sh"), "echo hi\n");
    symlinkSync("a.txt", join(repo, "link"));
    const s = store(repo);
    await turn(s, 1, () => {
      writeFileSync(join(repo, "a.txt"), "two\n");
      mkdirSync(join(repo, "new", "nested"), { recursive: true });
      writeFileSync(join(repo, "new", "nested", "made.txt"), "made\n");
      unlinkSync(join(repo, "gone.txt"));
      rmSync(join(repo, "deep"), { recursive: true });
      chmodSync(join(repo, "run.sh"), 0o755);
      unlinkSync(join(repo, "link"));
      symlinkSync("gone.txt", join(repo, "link"));
    }, "add the parser");
    const plan = await planUndo(s);
    if ("reason" in plan) throw new Error(plan.reason);
    expect(plan.turn).toBe(1);
    expect(plan.label).toBe("add the parser");
    const byPath = Object.fromEntries(plan.files.map((f) => [f.path, f.action]));
    expect(byPath).toEqual({
      "a.txt": "restore",
      "new/nested/made.txt": "delete",
      "gone.txt": "recreate",
      "deep/gone2.txt": "recreate",
      "run.sh": "restore",
      link: "restore",
    });
    expect(plan.files.find((f) => f.path === "a.txt")!.diff!.lines.map((l) => `${l.kind}:${l.text}`)).toEqual(["del:two", "add:one"]);
    expect(plan.files.every((f) => !f.changedSince)).toBe(true);

    const result = await applyUndo(s, plan);
    expect(result.refused).toEqual([]);
    expect(readFileSync(join(repo, "a.txt"), "utf8")).toBe("one\n");
    expect(existsSync(join(repo, "new"))).toBe(false);
    expect(readFileSync(join(repo, "gone.txt"), "utf8")).toBe("keep me\n");
    expect(readFileSync(join(repo, "deep", "gone2.txt"), "utf8")).toBe("deep\n");
    expect(statSync(join(repo, "run.sh")).mode & 0o111).toBe(0);
    expect(lstatSync(join(repo, "link")).isSymbolicLink()).toBe(true);
    expect(readlinkSync(join(repo, "link"))).toBe("a.txt");
    expect(result.restored.length + result.deleted.length + result.recreated.length).toBe(6);
  });

  test("a file edited since the turn is marked, and still restored; reported paths in neither tree aren't restored", async () => {
    const repo = makeRepo();
    const s = store(repo);
    await turn(s, 1, () => {
      writeFileSync(join(repo, "a.txt"), "two\n");
      writeFileSync(join(repo, "ignored.txt"), "secret\n");
    });
    writeFileSync(join(repo, "a.txt"), "three\n");
    const plan = await planUndo(s, { reported: ["a.txt", "ignored.txt"] });
    if ("reason" in plan) throw new Error(plan.reason);
    expect(plan.files).toMatchObject([{ path: "a.txt", changedSince: true }]);
    expect(plan.notRestored).toEqual(["ignored.txt"]);
    await applyUndo(s, plan);
    expect(readFileSync(join(repo, "a.txt"), "utf8")).toBe("one\n");
  });

  test("a parent that now leads outside the project is refused", async () => {
    const repo = makeRepo();
    const outside = tempDir();
    mkdirSync(join(repo, "sub"));
    writeFileSync(join(repo, "sub", "f.txt"), "f\n");
    git(repo, "add", "-A");
    git(repo, "commit", "-q", "-m", "sub");
    const s = store(repo);
    await turn(s, 1, () => writeFileSync(join(repo, "sub", "f.txt"), "changed\n"));
    rmSync(join(repo, "sub"), { recursive: true });
    symlinkSync(outside, join(repo, "sub"));
    const plan = await planUndo(s);
    if ("reason" in plan) throw new Error(plan.reason);
    const result = await applyUndo(s, plan);
    expect(result.refused.map((r) => r.path)).toEqual(["sub/f.txt"]);
    expect(existsSync(join(outside, "f.txt"))).toBe(false);
  });

  test("a binary file is named, not diffed", async () => {
    const repo = makeRepo();
    const s = store(repo);
    await turn(s, 1, () => writeFileSync(join(repo, "img.bin"), new Uint8Array([0, 1, 2, 0])));
    const plan = await planUndo(s);
    if ("reason" in plan) throw new Error(plan.reason);
    expect(plan.files).toMatchObject([{ path: "img.bin", action: "delete", binary: true }]);
    expect(plan.files[0]!.diff).toBeUndefined();
  });

  test("pre-undo snapshot and undo marker; two undos walk back two turns", async () => {
    const repo = makeRepo();
    const s = store(repo);
    await turn(s, 1, () => writeFileSync(join(repo, "a.txt"), "two\n"));
    await turn(s, 2, () => writeFileSync(join(repo, "a.txt"), "three\n"));
    const first = await planUndo(s);
    if ("reason" in first) throw new Error(first.reason);
    expect(first.turn).toBe(2);
    const applied = await applyUndo(s, first);
    expect(applied.preUndo).toBeTruthy();
    expect(git(repo, "show", `${applied.preUndo}:a.txt`)).toBe("three");
    expect(readFileSync(join(repo, "a.txt"), "utf8")).toBe("two\n");
    const kinds = (await s.chain()).map((c) => `${c.turn}:${c.kind}`);
    expect(kinds.slice(0, 2)).toEqual(["2:undo", "2:pre-undo"]);
    expect(nextUndoable(await s.chain())?.after.turn).toBe(1);

    const second = await planUndo(s);
    if ("reason" in second) throw new Error(second.reason);
    expect(second.turn).toBe(1);
    await applyUndo(s, second);
    expect(readFileSync(join(repo, "a.txt"), "utf8")).toBe("one\n");
    expect(await planUndo(s)).toMatchObject({ reason: expect.stringContaining("nothing") });
  });

  test("a failed undo marker still counts: the next /undo goes one turn further back, and the summary says so", async () => {
    const repo = makeRepo();
    const s = store(repo);
    await turn(s, 1, () => writeFileSync(join(repo, "a.txt"), "two\n"));
    await turn(s, 2, () => writeFileSync(join(repo, "a.txt"), "three\n"));
    const first = await planUndo(s);
    if ("reason" in first) throw new Error(first.reason);
    const real = s.snapshot.bind(s);
    s.snapshot = async (options) => (options.kind === "undo" ? { ok: false, reason: "failed", message: "disk full", ms: 0 } : real(options));
    const applied = await applyUndo(s, first);
    s.snapshot = real;
    expect(applied.markerFailed).toBe("disk full");
    expect(undoSummary(first, applied)).toContain("couldn't be recorded (disk full)");
    writeFileSync(join(repo, "a.txt"), "edited by the user\n");
    const second = await planUndo(s);
    if ("reason" in second) throw new Error(second.reason);
    expect(second.turn).toBe(1);
  });

  test("a plan made before another checkpoint is refused", async () => {
    const repo = makeRepo();
    const s = store(repo);
    await turn(s, 1, () => writeFileSync(join(repo, "a.txt"), "two\n"));
    const plan = await planUndo(s);
    if ("reason" in plan) throw new Error(plan.reason);
    await turn(s, 2, () => writeFileSync(join(repo, "a.txt"), "three\n"));
    await expect(applyUndo(s, plan)).rejects.toThrow(/changed/);
    expect(readFileSync(join(repo, "a.txt"), "utf8")).toBe("three\n");
  });

  test("a subfolder project's paths are relative to it", async () => {
    const repo = makeRepo();
    mkdirSync(join(repo, "pkg"));
    const s = store(join(repo, "pkg"));
    await turn(s, 1, () => writeFileSync(join(repo, "pkg", "x.txt"), "x\n"));
    const plan = await planUndo(s);
    if ("reason" in plan) throw new Error(plan.reason);
    expect(plan.files.map((f) => f.path)).toEqual(["x.txt"]);
    await applyUndo(s, plan);
    expect(existsSync(join(repo, "pkg", "x.txt"))).toBe(false);
    expect(existsSync(join(repo, "pkg"))).toBe(true); // the project's own folder is never removed
  });
});
