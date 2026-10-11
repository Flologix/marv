// A session started inside a git worktree (#35): its .git is a file pointing into the main repository's .git, which
// lives outside the project. Commands that run without asking (confined) must see that git data read-only, so git
// works, but only when the .git file checks out: a planted `gitdir:` must not show an outside folder.
import { afterEach, beforeEach, expect, test } from "bun:test";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { sandboxAvailable } from "../src/sandbox.ts";
import { bash } from "../src/tools/bash.ts";

const sandboxed = sandboxAvailable() ? test : test.skip;

let base: string;
let repo: string;
let tree: string;
beforeEach(async () => {
  base = await mkdtemp(join(tmpdir(), "marv-gitwt-"));
  repo = join(base, "repo");
  tree = join(base, "tree");
  await mkdir(repo);
  const git = (cwd: string, ...args: string[]) => {
    const r = Bun.spawnSync(["git", "-c", "user.email=t@t", "-c", "user.name=t", ...args], { cwd });
    if (r.exitCode !== 0) throw new Error(r.stderr.toString());
  };
  git(repo, "init", "-q", "-b", "main");
  await writeFile(join(repo, "a.txt"), "one\n");
  git(repo, "add", ".");
  git(repo, "commit", "-q", "-m", "first");
  git(repo, "worktree", "add", "-q", "-b", "wt", tree);
});
afterEach(() => rm(base, { recursive: true, force: true }));

/** The bash tool as yolo mode runs it: sandboxed and confined. */
const run = (command: string, root = tree) => bash.run({ command }, { root, sandbox: true, confined: true });

sandboxed("in a worktree, git works: the worktree's record and the repository's .git are shown", async () => {
  await writeFile(join(tree, "a.txt"), "two\n");
  const status = await run("git status --short && git log --oneline -1");
  expect(status.output).toContain("M a.txt");
  expect(status.output).toContain("first");
  expect((await run("git diff")).output).toContain("+two");
});

sandboxed("the git data stays read-only, and the .git file can't be repointed", async () => {
  const before = await readFile(join(tree, ".git"), "utf8");
  const result = await run(`git commit -qam x; echo 'gitdir: /elsewhere' > .git; touch ${join(repo, ".git", "hooks", "pre-commit")}`);
  expect(result.output).toMatch(/Read-only file system/);
  expect(await readFile(join(tree, ".git"), "utf8")).toBe(before);
  expect(Bun.spawnSync(["git", "log", "--oneline"], { cwd: repo }).stdout.toString().trim().split("\n")).toHaveLength(1);
});

sandboxed("a planted gitdir pointing anywhere else shows nothing", async () => {
  // A folder outside the project with something private in it, and a project whose .git file points there.
  const secret = join(base, "secret");
  await mkdir(secret);
  await writeFile(join(secret, "id_ed25519"), "PRIVATE KEY");
  const project = join(base, "project");
  await mkdir(project);
  await writeFile(join(project, ".git"), `gitdir: ${secret}\n`);
  expect((await run(`cat ${join(secret, "id_ed25519")}`, project)).output).not.toContain("PRIVATE KEY");

  // Even shaped like a worktree record, it must point back at this project's .git file.
  const fake = join(secret, "worktrees", "x");
  await mkdir(fake, { recursive: true });
  await writeFile(join(fake, "gitdir"), "/somewhere/else/.git\n");
  await writeFile(join(fake, "commondir"), "../..\n");
  await writeFile(join(project, ".git"), `gitdir: ${fake}\n`);
  expect((await run(`cat ${join(secret, "id_ed25519")}`, project)).output).not.toContain("PRIVATE KEY");
});
