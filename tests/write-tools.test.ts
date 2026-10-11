import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, readFile, rm, symlink, writeFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { runTool } from "../src/tools/index.ts";
import type { ApprovalRequest, Decision } from "../src/tools/types.ts";
import type { ToolCall } from "../src/provider/types.ts";

let root: string;
let outside: string;
let asked: ApprovalRequest[];
let answer: Decision;

beforeEach(async () => {
  root = await mkdtemp(join(tmpdir(), "marv-write-"));
  outside = await mkdtemp(join(tmpdir(), "marv-outside-"));
  await writeFile(join(root, "greet.ts"), "export function greet(name: string) {\n  return `Hello, ${name}!`;\n}\n");
  await writeFile(join(root, "twice.ts"), "x = 1;\nx = 1;\n");
  asked = [];
  answer = "yes";
});
afterEach(async () => {
  await rm(root, { recursive: true, force: true });
  await rm(outside, { recursive: true, force: true });
});

let nextId = 0;
const call = (name: string, args: unknown): ToolCall => ({ id: `c${nextId++}`, name, arguments: JSON.stringify(args) });
const run = (name: string, args: unknown) =>
  runTool(call(name, args), {
    root,
    sandbox: false,
    approve: async (request) => {
      asked.push(request);
      return answer;
    },
  });
const read = (path: string) => readFile(join(root, path), "utf8");

describe("edit_file", () => {
  test("replaces exactly one match, after showing the diff for approval", async () => {
    const result = await run("edit_file", { path: "greet.ts", old_string: "Hello", new_string: "Hi" });
    expect(result.isError).toBeFalsy();
    expect(await read("greet.ts")).toContain("`Hi, ${name}!`");
    expect(result.summary).toBe("+1 −1");
    expect(asked).toHaveLength(1);
    expect(asked[0]!.preview.title).toBe("Edit greet.ts");
    expect(asked[0]!.preview.diff).toContainEqual({ kind: "del", text: "  return `Hello, ${name}!`;", oldLine: 2 });
    expect(asked[0]!.preview.diff).toContainEqual({ kind: "add", text: "  return `Hi, ${name}!`;", newLine: 2 });
  });

  test("doesn't ask when the text isn't there, and tells the model how to fix it", async () => {
    const result = await run("edit_file", { path: "greet.ts", old_string: "Goodbye", new_string: "x" });
    expect(result.isError).toBe(true);
    expect(result.output).toContain("not found");
    expect(result.output).toContain("read_file"); // tells the model how to recover
    expect(asked).toHaveLength(0);
  });

  test("refuses an ambiguous match unless replace_all is set", async () => {
    const ambiguous = await run("edit_file", { path: "twice.ts", old_string: "x = 1;", new_string: "x = 2;" });
    expect(ambiguous).toMatchObject({ isError: true, output: expect.stringContaining("2 times") });
    expect(asked).toHaveLength(0);

    const all = await run("edit_file", { path: "twice.ts", old_string: "x = 1;", new_string: "x = 2;", replace_all: true });
    expect(all.isError).toBeFalsy();
    expect(await read("twice.ts")).toBe("x = 2;\nx = 2;\n");
  });

  test("a declined edit changes nothing", async () => {
    answer = "no";
    const result = await run("edit_file", { path: "greet.ts", old_string: "Hello", new_string: "Hi" });
    expect(result).toMatchObject({ declined: true, summary: "declined" });
    expect(await read("greet.ts")).toContain("Hello");
  });

  test("an old_string with the wrong indentation still applies, re-indented in the file's style, and says so", async () => {
    await writeFile(join(root, "tabs.js"), "function f() {\n\tif (x) {\n\t\treturn 1;\n\t}\n}\n");
    const result = await run("edit_file", { path: "tabs.js", old_string: "    if (x) {\n        return 1;\n    }", new_string: "    if (x) {\n        return 2;\n    }" });
    expect(result.isError).toBeFalsy();
    expect(await read("tabs.js")).toBe("function f() {\n\tif (x) {\n\t\treturn 2;\n\t}\n}\n");
    expect(result.output).toContain("indentation");
    expect(asked[0]!.preview.diff).toContainEqual({ kind: "add", text: "\t\treturn 2;", newLine: 3 });
  });

  test("when nothing matches, the error shows the closest lines as they are now", async () => {
    const result = await run("edit_file", { path: "greet.ts", old_string: "  return `Hi, ${name}!!`;", new_string: "x" });
    expect(result.isError).toBe(true);
    expect(result.output).toContain("closest");
    expect(result.output).toContain("    2→  return `Hello, ${name}!`;");
  });

  test("a CRLF file keeps its line endings, from edit_file and write_file", async () => {
    await writeFile(join(root, "win.js"), "a();\r\nb();\r\n");
    await run("edit_file", { path: "win.js", old_string: "a();", new_string: "a();\nc();" });
    expect(await read("win.js")).toBe("a();\r\nc();\r\nb();\r\n");
    const written = await run("write_file", { path: "win.js", content: "x();\ny();\n" });
    expect(await read("win.js")).toBe("x();\r\ny();\r\n");
    expect(written.output).toContain("CRLF");
  });

  test("all file changes share one 'don't ask again' scope", async () => {
    await run("edit_file", { path: "greet.ts", old_string: "Hello", new_string: "Hi" });
    await run("write_file", { path: "new.ts", content: "x" });
    expect(asked.map((r) => r.scope.key)).toEqual(["files", "files"]);
  });
});

describe("write_file", () => {
  test("creates a new file, and its folders", async () => {
    const result = await run("write_file", { path: "src/deep/new.ts", content: "export const a = 1;\n" });
    expect(result.isError).toBeFalsy();
    expect(await read("src/deep/new.ts")).toBe("export const a = 1;\n");
    expect(result.summary).toBe("created · 1 line");
    expect(asked[0]!.preview.title).toBe("Create src/deep/new.ts");
  });

  test("a new file's preview carries all its lines (the prompt counts what it doesn't show), and no note", async () => {
    const content = Array.from({ length: 100 }, (_, i) => `l${i + 1}`).join("\n") + "\n";
    await run("write_file", { path: "big.ts", content });
    expect(asked[0]!.preview.diff).toHaveLength(100);
    expect(asked[0]!.preview.diff!.at(-1)).toEqual({ kind: "add", text: "l100", newLine: 100 });
    expect(asked[0]!.preview.note).toBeUndefined();
  });

  test("an absolute path inside the project is shown relative to it", async () => {
    const result = await run("write_file", { path: join(root, "abs.ts"), content: "x" });
    expect(result.label).toBe("abs.ts");
    expect(asked[0]!.label).toBe("abs.ts");
  });

  test("overwriting shows what changes", async () => {
    await run("write_file", { path: "greet.ts", content: "export const greet = () => 'hi';\n" });
    expect(asked[0]!.preview.title).toBe("Overwrite greet.ts");
    expect(asked[0]!.preview.diff!.some((l) => l.kind === "del")).toBe(true);
  });

  describe("append: true", () => {
    test("adds at the end instead of replacing", async () => {
      await writeFile(join(root, "log.txt"), "first\n");
      const result = await run("write_file", { path: "log.txt", content: "second\n", append: true });
      expect(result.isError).toBeFalsy();
      expect(await read("log.txt")).toBe("first\nsecond\n");
      expect(result.output).toContain("Appended to log.txt");
      expect(result.summary).toBe("+1 −0");
      expect(asked[0]!.preview.title).toBe("Append log.txt");
      expect(asked[0]!.preview.diff).toEqual([
        { kind: "ctx", text: "first", oldLine: 1, newLine: 1 },
        { kind: "add", text: "second", newLine: 2 },
      ]);
    });

    test("a last line without a newline gets one first, and the result says so", async () => {
      await writeFile(join(root, "no-eol.txt"), "no newline yet");
      const result = await run("write_file", { path: "no-eol.txt", content: "more\n", append: true });
      expect(result.isError).toBeFalsy();
      expect(await read("no-eol.txt")).toBe("no newline yet\nmore\n");
      expect(result.output).toContain("added a newline to its last line");
    });

    test("on a file that doesn't exist it just creates it", async () => {
      const result = await run("write_file", { path: "new.txt", content: "x\n", append: true });
      expect(result.isError).toBeFalsy();
      expect(await read("new.txt")).toBe("x\n");
      expect(asked[0]!.preview.title).toBe("Create new.txt");
      expect(result.output).toContain("Created new.txt (1 lines)");
    });

    test("a CRLF file keeps its line endings", async () => {
      await writeFile(join(root, "win.txt"), "a\r\n");
      const result = await run("write_file", { path: "win.txt", content: "b\n", append: true });
      expect(await read("win.txt")).toBe("a\r\nb\r\n");
      expect(result.output).toContain("CRLF");
    });

    test("a CRLF file without a newline on its last line gets a CRLF one, not a bare \\n", async () => {
      await writeFile(join(root, "win2.txt"), "a\r\nb");
      await run("write_file", { path: "win2.txt", content: "c\n", append: true });
      expect(await read("win2.txt")).toBe("a\r\nb\r\nc\r\n");
    });
  });

  test.each([
    ["../escape.ts", "outside the project"],
    ["/tmp/escape.ts", "outside the project"],
  ])("refuses %s", async (path, message) => {
    const result = await run("write_file", { path, content: "x" });
    expect(result).toMatchObject({ isError: true, output: expect.stringContaining(message) });
    expect(asked).toHaveLength(0);
  });

  test("can't escape through a symlinked folder, even to create a new file", async () => {
    await symlink(outside, join(root, "linked"));
    const result = await run("write_file", { path: "linked/escape.ts", content: "x" });
    expect(result).toMatchObject({ isError: true, output: expect.stringContaining("outside the project") });
    expect(existsSync(join(outside, "escape.ts"))).toBe(false);
  });
});

describe("a change that breaks the file's syntax", () => {
  test("edit_file applies it, and says so with the line", async () => {
    await writeFile(join(root, "a.js"), "export const a = 1;\nexport const b = 2;\n");
    const result = await run("edit_file", { path: "a.js", old_string: "const b = 2;", new_string: "const b = ;" });
    expect(result.isError).toBeFalsy();
    expect(await read("a.js")).toContain("const b = ;");
    expect(result.output).toContain("doesn't parse any more after this change");
    expect(result.output).toContain("    2→export const b = ;");
  });

  test("write_file says so for a new file too", async () => {
    const result = await run("write_file", { path: "b.ts", content: "export function f(x: number) {\n  return x +;\n}\n" });
    expect(result.output).toContain("b.ts doesn't parse");
  });

  test("append: true too", async () => {
    const result = await run("write_file", { path: "b.ts", content: "broken(\n", append: true });
    expect(result.output).toContain("b.ts doesn't parse");
  });

  test("a file that was already broken isn't blamed on the change, and a fine change says nothing", async () => {
    await writeFile(join(root, "c.js"), "const a = ;\nconst b = 1;\n");
    expect((await run("edit_file", { path: "c.js", old_string: "const b = 1;", new_string: "const b = 2;" })).output).not.toContain("parse");
    expect((await run("edit_file", { path: "greet.ts", old_string: "Hello", new_string: "Hi" })).output).not.toContain("parse");
  });
});

describe("approval", () => {
  test("read-only tools never ask", async () => {
    await run("read_file", { path: "greet.ts" });
    await run("grep", { pattern: "greet" });
    expect(asked).toHaveLength(0);
  });

  test("bash asks, shows the command, and scopes 'don't ask again' to that exact command", async () => {
    const result = await run("bash", { command: "echo hi" });
    expect(result.output).toContain("hi");
    expect(asked[0]!.preview.command).toBe("echo hi");
    expect(asked[0]!.scope).toEqual({ key: "bash:echo hi", description: "this exact command" });
  });

  test("Esc at an approval (no + abort) is an interrupt, not the user's \"no\"", async () => {
    const controller = new AbortController();
    const result = await runTool(call("write_file", { path: "x.ts", content: "x" }), {
      root,
      sandbox: false,
      signal: controller.signal,
      approve: async () => (controller.abort(), "no"),
    });
    expect(result).toMatchObject({ output: expect.stringContaining("Interrupted"), summary: "interrupted", declined: true, approval: "interrupted" });
    expect(existsSync(join(root, "x.ts"))).toBe(false);
  });

  test("stopped while the preview was made: interrupted, never asked", async () => {
    const controller = new AbortController();
    controller.abort();
    const result = await runTool(call("write_file", { path: "x.ts", content: "x" }), { root, sandbox: false, signal: controller.signal, approve: async () => "yes" });
    expect(result).toMatchObject({ summary: "interrupted", approval: "interrupted" });
  });

  test("without an approver, changes are refused", async () => {
    const result = await runTool(call("write_file", { path: "x.ts", content: "x" }), { root });
    expect(result.isError).toBe(true);
    expect(existsSync(join(root, "x.ts"))).toBe(false);
  });
});

test("a folder in the way is reported, not overwritten", async () => {
  await mkdir(join(root, "dir"));
  const result = await run("write_file", { path: "dir", content: "x" });
  expect(result).toMatchObject({ isError: true, output: expect.stringContaining("is a directory") });
});

describe("the diff a change carries (for the transcript)", () => {
  test("edit_file: the change with line numbers", async () => {
    await writeFile(join(root, "a.txt"), "one\ntwo\nthree\n");
    const result = await run("edit_file", { path: "a.txt", old_string: "two", new_string: "TWO" });
    expect(result.diff).toEqual({
      lines: [
        { kind: "ctx", text: "one", oldLine: 1, newLine: 1 },
        { kind: "del", text: "two", oldLine: 2 },
        { kind: "add", text: "TWO", newLine: 2 },
        { kind: "ctx", text: "three", oldLine: 3, newLine: 3 },
      ],
      more: 0,
    });
  });

  test("write_file: a new file is its lines as additions, capped", async () => {
    const content = Array.from({ length: 450 }, (_, i) => `l${i + 1}`).join("\n") + "\n";
    const result = await run("write_file", { path: "big.txt", content });
    expect(result.diff!.lines[0]).toEqual({ kind: "add", text: "l1", newLine: 1 });
    expect(result.diff!.lines).toHaveLength(400);
    expect(result.diff!.more).toBe(50);
  });

  test("write_file: overwriting shows what changed", async () => {
    await writeFile(join(root, "b.txt"), "old\n");
    const result = await run("write_file", { path: "b.txt", content: "new\n" });
    expect(result.diff!.lines).toEqual([
      { kind: "del", text: "old", oldLine: 1 },
      { kind: "add", text: "new", newLine: 1 },
    ]);
  });

  test("write_file append: true carries what was added", async () => {
    await writeFile(join(root, "b.txt"), "old\n");
    const result = await run("write_file", { path: "b.txt", content: "new\n", append: true });
    expect(result.diff!.lines).toEqual([
      { kind: "ctx", text: "old", oldLine: 1, newLine: 1 },
      { kind: "add", text: "new", newLine: 2 },
    ]);
  });
});

describe("beforeChange", () => {
  const cases = [
    ["edit_file", { path: "greet.ts", old_string: "Hello", new_string: "Hi" }, "greet.ts"],
    ["write_file", { path: "new.ts", content: "export {};\n" }, "new.ts"],
  ] as const;
  for (const [name, args, path] of cases) {
    test(`${name}: the preview starts it (before the user is asked), and the file is written only once it's done`, async () => {
      let started = 0;
      let release!: () => void;
      const baseline = new Promise<void>((resolve) => (release = resolve));
      const inRun = Promise.withResolvers<void>();
      const startedWhenAsked: number[] = [];
      const pending = runTool(call(name, args), {
        root,
        sandbox: false,
        approve: async () => (startedWhenAsked.push(started), "yes"),
        beforeChange: () => {
          if (++started === 2) inRun.resolve();
          return baseline;
        },
      });
      // The second call is run() reaching its await.
      await inRun.promise;
      expect(started).toBe(2);
      expect(startedWhenAsked).toEqual([1]);
      // Approved, but still waiting for the baseline: nothing written yet.
      if (path === "greet.ts") expect(await read(path)).toContain("Hello");
      else expect(existsSync(join(root, path))).toBe(false);
      release();
      expect((await pending).isError).toBeFalsy();
      if (path === "greet.ts") expect(await read(path)).toContain("Hi");
      else expect(await read(path)).toBe("export {};\n");
    });

    test(`${name}: stopped while waiting for the baseline, nothing is written`, async () => {
      const stop = new AbortController();
      const inRun = Promise.withResolvers<void>();
      let started = 0;
      const pending = runTool(call(name, args), {
        root,
        sandbox: false,
        signal: stop.signal,
        approve: async () => "yes",
        // Resolves only once the abort happens, like the real baseline (it ends on abort).
        beforeChange: () => {
          if (++started === 2) inRun.resolve();
          return new Promise<void>((resolve) => stop.signal.addEventListener("abort", () => resolve()));
        },
      });
      await inRun.promise;
      stop.abort();
      const result = await pending;
      expect(result.summary).toBe("interrupted");
      if (path === "greet.ts") expect(await read(path)).toContain("Hello");
      else expect(existsSync(join(root, path))).toBe(false);
    });
  }
});
