import { afterEach, expect, test } from "bun:test";
import { mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { CheckpointStore } from "../src/checkpoints/store.ts";
import { CheckpointTurn } from "../src/checkpoints/turn.ts";

const dirs: string[] = [];
afterEach(() => {
  for (const dir of dirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});
const temp = () => {
  const dir = realpathSync(mkdtempSync(join(tmpdir(), "marv-cpt-")));
  dirs.push(dir);
  return dir;
};
function repo(): string {
  const dir = temp();
  for (const args of [["init", "-q"], ["config", "user.email", "t@t"], ["config", "user.name", "t"]]) Bun.spawnSync(["git", ...args], { cwd: dir });
  writeFileSync(join(dir, "a.txt"), "a\n");
  Bun.spawnSync(["git", "add", "-A"], { cwd: dir });
  Bun.spawnSync(["git", "commit", "-q", "-m", "x"], { cwd: dir });
  return dir;
}

test("concurrent checkpoint() calls take one snapshot; finish keeps a pair that changed something", async () => {
  const root = repo();
  const store = new CheckpointStore({ root, ownDir: temp(), session: "s" });
  const turn = new CheckpointTurn({ store, turn: 1, label: "go" });
  await Promise.all([turn.checkpoint(), turn.checkpoint(), turn.checkpoint()]);
  expect((await store.chain()).length).toBe(1);
  writeFileSync(join(root, "a.txt"), "b\n");
  expect(await turn.finish()).toMatchObject({ status: "kept" });
  expect((await store.chain()).map((c) => c.kind)).toEqual(["after", "before"]);
});

test("a pair that changed nothing is dropped; a turn without a checkpoint does nothing", async () => {
  const root = repo();
  const store = new CheckpointStore({ root, ownDir: temp(), session: "s" });
  const turn = new CheckpointTurn({ store, turn: 1, label: "go" });
  await turn.checkpoint();
  expect(await turn.finish()).toMatchObject({ status: "dropped" });
  expect(await store.chain()).toEqual([]);
  expect(await new CheckpointTurn({ store, turn: 2, label: "" }).finish()).toEqual({ status: "none" });
});

test("a failing snapshot is reported once and never rejects", async () => {
  const root = repo();
  const store = new CheckpointStore({ root, ownDir: temp(), session: "s" });
  store.snapshot = async () => ({ ok: false, reason: "failed", message: "disk full", ms: 1 });
  const failures: string[] = [];
  const turn = new CheckpointTurn({ store, turn: 1, label: "go", onFailed: (m) => failures.push(m) });
  await turn.checkpoint();
  await turn.checkpoint();
  expect(failures).toEqual(["disk full"]);
  expect(await turn.finish()).toMatchObject({ status: "failed", message: "disk full" });
});

test("a slow snapshot says so after 300 ms", async () => {
  const root = repo();
  const store = new CheckpointStore({ root, ownDir: temp(), session: "s" });
  const real = store.snapshot.bind(store);
  store.snapshot = async (a) => (await Bun.sleep(350), real(a));
  const slow: boolean[] = [];
  const turn = new CheckpointTurn({ store, turn: 1, label: "go", onSlow: (on) => slow.push(on) });
  await turn.checkpoint();
  expect(slow).toEqual([true, false]);
});

test("outside a repository, without a git dir of Marv's own: no checkpoint, and no failure", async () => {
  const store = new CheckpointStore({ root: temp(), session: "s" });
  const failures: string[] = [];
  const turn = new CheckpointTurn({ store, turn: 1, label: "go", onFailed: (m) => failures.push(m) });
  await turn.checkpoint();
  expect(await turn.finish()).toEqual({ status: "none" });
  expect(failures).toEqual([]);
});
