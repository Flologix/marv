import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { existsSync } from "node:fs";
import { mkdir, mkdtemp, rm, symlink, writeFile } from "node:fs/promises";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";
import { runTool, toolSpecs } from "../src/tools/index.ts";
import { listProjectFiles, resolveInProject } from "../src/tools/files.ts";
import { ToolError } from "../src/tools/types.ts";
import { z } from "zod";
import type { ToolCall } from "../src/provider/types.ts";
import { readFile } from "../src/tools/read-file.ts";
import type { ApprovalRequest, Tool } from "../src/tools/types.ts";

let root: string;
let outside: string;

async function files(tree: Record<string, string | Buffer>) {
  for (const [path, content] of Object.entries(tree)) {
    await mkdir(join(root, path, ".."), { recursive: true });
    await writeFile(join(root, path), content);
  }
}

beforeEach(async () => {
  root = await mkdtemp(join(tmpdir(), "marv-tools-"));
  outside = await mkdtemp(join(tmpdir(), "marv-outside-"));
  await files({
    "package.json": '{ "name": "demo" }\n',
    "src/app.ts": "export function main() {\n  return greet('world');\n}\n",
    "src/greet.ts": "export function greet(name: string) {\n  return `Hello, ${name}!`;\n}\n",
    "src/ui/view.tsx": "// TODO: render the greeting\nexport const View = () => null;\n",
    "node_modules/dep/index.js": "function greet() {}\n",
    "logo.png": Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x00, 0x00, 0x01]),
  });
  await writeFile(join(outside, "secret.txt"), "top secret\n");
});
afterEach(async () => {
  await rm(root, { recursive: true, force: true });
  await rm(outside, { recursive: true, force: true });
});

let nextId = 0;
const call = (name: string, args: unknown): ToolCall => ({ id: `call_${nextId++}`, name, arguments: JSON.stringify(args) });
const run = (name: string, args: unknown) => runTool(call(name, args), { root });

describe("read_file", () => {
  test("returns numbered lines and a summary", async () => {
    const result = await run("read_file", { path: "src/greet.ts" });
    expect(result.isError).toBeFalsy();
    expect(result.output).toBe("    1→export function greet(name: string) {\n    2→  return `Hello, ${name}!`;\n    3→}");
    expect(result.summary).toBe("3 lines");
    expect(result.label).toBe("src/greet.ts");
  });

  test("reads a slice with offset and limit, and says how to continue", async () => {
    const lines = Array.from({ length: 50 }, (_, i) => `line ${i + 1}`).join("\n");
    await files({ "long.txt": lines });
    const result = await run("read_file", { path: "long.txt", offset: 10, limit: 3 });
    expect(result.output).toStartWith("   10→line 10\n   11→line 11\n   12→line 12");
    expect(result.output).toContain("lines 10-12 of 50");
    expect(result.output).toContain("offset=13");
    expect(result.summary).toBe("lines 10-12 of 50");
  });

  // Issue #40: AGENTS.md bullets of several thousand characters on one line were unreadable past 500.
  test("a long line is cut in a long read, and read whole with a small limit", async () => {
    const long = `- **Topic**: ${"word ".repeat(1600)}end`;
    await files({ "notes.md": `# Notes\n${long}\n${"short\n".repeat(300)}` });
    const many = await run("read_file", { path: "notes.md" });
    expect(many.output).not.toContain("end");
    expect(many.output).toContain(`(line truncated: ${long.length} characters; offset=2 and limit=1 reads it whole)`);
    const one = await run("read_file", { path: "notes.md", offset: 2, limit: 1 });
    expect(one.output).toContain(`    2→${long}\n`);
    expect((await run("read_file", { path: "notes.md", offset: 2, limit: 3 })).output).toContain(`${long}\n`);
  });

  test("accepts an absolute path inside the project", async () => {
    expect((await run("read_file", { path: join(root, "package.json") })).output).toContain('"demo"');
  });

  test.each([
    ["../outside.txt", "outside the project"],
    ["/etc/hostname", "outside the project"],
    ["missing.ts", "File not found: missing.ts"],
    ["src", "is a directory"],
  ])("refuses %s", async (path, message) => {
    const result = await run("read_file", { path });
    expect(result.isError).toBe(true);
    expect(result.output).toContain(message);
  });

  test("won't follow a symlink out of the project", async () => {
    await symlink(join(outside, "secret.txt"), join(root, "link.txt"));
    const result = await run("read_file", { path: "link.txt" });
    expect(result.isError).toBe(true);
    expect(result.output).not.toContain("top secret");
  });

  test("doesn't dump binary files", async () => {
    const result = await run("read_file", { path: "logo.png" });
    expect(result.output).toBe("Binary file (7 bytes), not shown.");
  });
});

describe("~/ paths (#22)", () => {
  test("a leading ~/ is the home folder, not a folder named ~", () => {
    expect(resolveInProject(root, "~/src/app.ts", root)).toBe(join(root, "src/app.ts"));
    expect(resolveInProject(root, "~", root)).toBe(root);
    expect(resolveInProject(root, "~src/app.ts", root)).toBe(join(root, "~src/app.ts")); // not ~user
  });

  test.each([
    ["read_file", { path: "~/x.txt" }],
    ["edit_file", { path: "~/x.txt", old_string: "a", new_string: "b" }],
    ["write_file", { path: "~/x.txt", content: "hi\n" }],
  ])("%s with ~/ outside the project is refused, and creates nothing", async (name, args) => {
    expect(root.startsWith(homedir() + "/")).toBe(false); // the temp project isn't the home folder
    const result = await run(name, args);
    expect(result.isError).toBe(true);
    expect(result.output).toContain("outside the project");
    expect(result.output).not.toContain("write_file");
    expect(existsSync(join(root, "~"))).toBe(false);
  });

  test("edit_file suggests write_file only when the folder exists", async () => {
    const inFolder = await run("edit_file", { path: "src/new.ts", old_string: "a", new_string: "b" });
    expect(inFolder.output).toBe("File not found: src/new.ts. Use write_file to create a new file.");
    const noFolder = await run("edit_file", { path: "nope/new.ts", old_string: "a", new_string: "b" });
    expect(noFolder.output).toBe("File not found: nope/new.ts.");
  });
});

describe("glob", () => {
  test("finds files by pattern, sorted, relative to the project", async () => {
    const result = await run("glob", { pattern: "**/*.ts" });
    expect(result.output).toBe("src/app.ts\nsrc/greet.ts");
    expect(result.summary).toBe("2 files");
  });

  test("searches inside a directory", async () => {
    expect((await run("glob", { pattern: "**/*", path: "src/ui" })).output).toBe("src/ui/view.tsx");
  });

  test("skips node_modules even outside git", async () => {
    expect((await run("glob", { pattern: "**/*.js" })).output).toBe("No files match.");
  });

  test("respects .gitignore inside a git repo", async () => {
    Bun.spawnSync(["git", "init", "-q"], { cwd: root });
    await files({ ".gitignore": "src/ui/\n" });
    expect((await run("glob", { pattern: "**/*.tsx" })).output).toBe("No files match.");
  });
});

describe("grep", () => {
  test("finds matching lines with file and line number", async () => {
    const result = await run("grep", { pattern: "greet\\(" });
    expect(result.output).toBe("src/app.ts:2:  return greet('world');\nsrc/greet.ts:1:export function greet(name: string) {");
    expect(result.summary).toBe("2 matches in 2 files");
  });

  test("filters files with a glob and can ignore case", async () => {
    const result = await run("grep", { pattern: "todo", glob: "*.tsx", ignoreCase: true });
    expect(result.output).toBe("src/ui/view.tsx:1:// TODO: render the greeting");
  });

  test("reports no matches and bad patterns clearly", async () => {
    expect((await run("grep", { pattern: "nothing-like-this" })).output).toBe("No matches.");
    const bad = await run("grep", { pattern: "(" });
    expect(bad.isError).toBe(true);
    expect(bad.output).toContain("Invalid regex");
  });
});

describe("runTool", () => {
  test("rejects unknown tools, broken JSON, and invalid input, as results the model can read", async () => {
    expect((await run("delete_everything", {})).output).toContain("Unknown tool");
    const broken = await runTool({ id: "x", name: "read_file", arguments: "{not json" }, { root });
    expect(broken).toMatchObject({ isError: true, output: expect.stringContaining("not valid JSON") });
    const invalid = await run("read_file", { path: 42 });
    expect(invalid).toMatchObject({ isError: true, output: expect.stringContaining("path") });
  });
});

describe("toolSpecs", () => {
  test("describe every tool with a JSON schema, without the $schema noise", () => {
    expect(toolSpecs.map((t) => t.name)).toEqual(["read_file", "glob", "grep", "web_fetch", "skill", "edit_file", "write_file", "bash", "memory", "plan", "agent"]);
    for (const spec of toolSpecs) {
      expect(spec.description.length).toBeGreaterThan(20);
      expect(spec.parameters).toMatchObject({ type: "object" });
      expect(spec.parameters).not.toHaveProperty("$schema");
    }
  });
});

describe("runTool: subagent support", () => {
  const sometimes: Tool = {
    name: "sometimes",
    description: "Needs approval only when asked to.",
    input: z.object({ ask: z.boolean() }),
    label: () => "x",
    needsApproval: ({ ask }: { ask: boolean }) => ask,
    preview: async () => ({ title: "Sometimes" }),
    run: async (_input, ctx) => ({ output: `ran as ${ctx.callId}`, summary: "ok" }),
  };
  const call = (name: string, args: unknown, id = "c1") => ({ id, name, arguments: JSON.stringify(args) });

  test("only the tools in `available` can be called", async () => {
    const result = await runTool(call("memory", { action: "add", text: "x" }), { root }, [readFile as Tool]);
    expect(result.isError).toBe(true);
    expect(result.output).toContain('Unknown tool "memory". Available tools: read_file.');
  });

  test("needsApproval decides per call, and run() gets the call's id", async () => {
    const asked: ApprovalRequest[] = [];
    const ctx = { root, approve: async (r: ApprovalRequest) => (asked.push(r), "yes" as const) };
    expect((await runTool(call("sometimes", { ask: false }), ctx, [sometimes])).output).toBe("ran as c1");
    expect(asked).toHaveLength(0);
    expect((await runTool(call("sometimes", { ask: true }, "c2"), ctx, [sometimes])).output).toBe("ran as c2");
    expect(asked).toHaveLength(1);
  });

  test("a bash call with network: true is flagged in its approval request", async () => {
    const asked: ApprovalRequest[] = [];
    const approve = async (r: ApprovalRequest) => (asked.push(r), "no" as const);
    await runTool(call("bash", { command: "curl example.com", network: true }), { root, approve });
    await runTool(call("bash", { command: "ls" }), { root, approve });
    expect(asked.map((r) => r.network)).toEqual([true, undefined]);
  });

  test("a run stopped while the preview was being made doesn't raise an approval prompt", async () => {
    // A parallel subagent can be mid-preview when the user presses Esc.
    const controller = new AbortController();
    const asked: ApprovalRequest[] = [];
    let ran = false;
    const slow: Tool = {
      ...sometimes,
      preview: async () => {
        controller.abort();
        return { title: "Sometimes" };
      },
      run: async () => ((ran = true), { output: "ran", summary: "ok" }),
    };
    const approve = async (r: ApprovalRequest) => (asked.push(r), "yes" as const);
    const result = await runTool(call("sometimes", { ask: true }), { root, approve, signal: controller.signal }, [slow]);
    expect(asked).toHaveLength(0);
    expect(ran).toBe(false);
    // Reported as the interrupt it was, not as a "no" (declined still stops the loop).
    expect(result).toMatchObject({ declined: true, summary: "interrupted", output: "Interrupted by the user before this tool ran." });
  });
});

describe("the file listing never runs a repository's programs", () => {
  const gitIn = (cwd: string, ...args: string[]) => Bun.spawnSync(["git", ...args], { cwd });
  const globCall = { id: "g1", name: "glob", arguments: JSON.stringify({ pattern: "**/*.txt" }) };

  test("core.fsmonitor in the project's own config isn't run", async () => {
    const pwned = join(outside, "PWNED");
    gitIn(root, "init", "-q");
    gitIn(root, "config", "core.fsmonitor", `touch ${pwned}`);
    await writeFile(join(root, "a.txt"), "a\n");
    // Negative control: plain git, without Marv's protections, does run the program.
    gitIn(root, "ls-files", "--others");
    expect(existsSync(pwned)).toBe(true);
    await rm(pwned);
    const result = await runTool(globCall, { root });
    expect(result.output).toContain("a.txt");
    expect(existsSync(pwned)).toBe(false);
  });

  test("core.fsmonitor in a per-worktree config (config.worktree) isn't run", async () => {
    const pwned = join(outside, "PWNED");
    gitIn(root, "init", "-q");
    gitIn(root, "config", "extensions.worktreeConfig", "true");
    await writeFile(join(root, ".git", "config.worktree"), `[core]\n\tfsmonitor = touch ${pwned}\n`);
    await writeFile(join(root, "w.txt"), "w\n");
    const result = await runTool(globCall, { root });
    expect(result.output).toContain("w.txt");
    expect(existsSync(pwned)).toBe(false);
  });

  test("with gitEnv, a redirected .git file is ignored", async () => {
    const pwned = join(outside, "PWNED");
    // Two usable repositories outside the work folder. Only the fake one hides hidden.txt.
    const real = join(outside, "real.git");
    const fake = join(outside, "fake.git");
    for (const dir of [real, fake]) {
      gitIn(outside, "init", "-q", "--bare", dir);
      gitIn(dir, "config", "core.bare", "false");
    }
    gitIn(fake, "config", "core.fsmonitor", `touch ${pwned}`);
    await writeFile(join(fake, "info", "exclude"), "hidden.txt\n");
    // The .git file in the work folder points to the fake one.
    await writeFile(join(root, ".git"), `gitdir: ${fake}\n`);
    await writeFile(join(root, "shown.txt"), "s\n");
    await writeFile(join(root, "hidden.txt"), "h\n");

    // Unpinned, git follows the .git file: the fake repository's view.
    const followed = await runTool(globCall, { root });
    expect(followed.output).toContain("shown.txt");
    expect(followed.output).not.toContain("hidden.txt");

    // Pinned to the real repository: its view (hidden.txt isn't excluded there).
    const pinned = await runTool(globCall, { root, gitEnv: { GIT_DIR: real, GIT_COMMON_DIR: real, GIT_WORK_TREE: root } });
    expect(pinned.output).toContain("shown.txt");
    expect(pinned.output).toContain("hidden.txt");
    expect(existsSync(pwned)).toBe(false);
  });
});

describe("special files can't hang Marv", () => {
  const mkfifo = (path: string) => expect(Bun.spawnSync(["mkfifo", path]).exitCode).toBe(0);

  test("a FIFO named .gitignore makes the listing time out with a ToolError, quickly", async () => {
    Bun.spawnSync(["git", "init", "-q"], { cwd: root });
    mkfifo(join(root, ".gitignore"));
    const started = Date.now();
    await expect(listProjectFiles(root, undefined, 300)).rejects.toBeInstanceOf(ToolError);
    expect(Date.now() - started).toBeLessThan(3000);
  });

  test("read_file refuses a FIFO", async () => {
    mkfifo(join(root, "pipe"));
    const result = await runTool({ id: "r", name: "read_file", arguments: JSON.stringify({ path: "pipe" }) }, { root });
    expect(result.isError).toBe(true);
    expect(result.output).toContain("not a regular file");
  });

  test("edit_file refuses a FIFO", async () => {
    mkfifo(join(root, "pipe"));
    const call = { id: "e", name: "edit_file", arguments: JSON.stringify({ path: "pipe", old_string: "a", new_string: "b" }) };
    const result = await runTool(call, { root, approve: async () => "yes" });
    expect(result.isError).toBe(true);
    expect(result.output).toContain("not a regular file");
  });

  test("a folder-wide grep doesn't follow a symlink out of the project", async () => {
    Bun.spawnSync(["git", "init", "-q"], { cwd: root }); // the git listing includes symlinks
    await symlink(join(outside, "secret.txt"), join(root, "notes.txt"));
    const call = { id: "s", name: "grep", arguments: JSON.stringify({ pattern: "top secret|greet" }) };
    const result = await runTool(call, { root });
    expect(result.output).not.toContain("top secret");
    expect(result.output).not.toContain("notes.txt");
    expect(result.output).toContain("src/greet.ts");
  });

  test("grep skips a FIFO in a directory, and refuses one named explicitly", async () => {
    mkfifo(join(root, "src", "pipe"));
    const dir = await runTool({ id: "g", name: "grep", arguments: JSON.stringify({ pattern: "greet", path: "src" }) }, { root });
    expect(dir.isError).toBeFalsy();
    expect(dir.output).toContain("src/greet.ts");
    const named = await runTool({ id: "g2", name: "grep", arguments: JSON.stringify({ pattern: "x", path: "src/pipe" }) }, { root });
    expect(named.isError).toBe(true);
    expect(named.output).toContain("not a regular file");
  });

  test("grep on a path that doesn't exist says so, not that it's a special file", async () => {
    // A model was told a missing file was "a pipe, socket or device" (trajectory, 2026-10-05).
    const result = await runTool({ id: "g", name: "grep", arguments: JSON.stringify({ pattern: "x", path: "src/missing.ts" }) }, { root });
    expect(result.isError).toBe(true);
    expect(result.output).toContain("doesn't exist");
    expect(result.output).not.toContain("not a regular file");
  });
});
