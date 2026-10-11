// Issue #10: malformed tool calls from weaker models, repaired where the
// model's meaning is beyond doubt. Each repair is announced in the result
// ("Marv: …"), so the model learns the right form; anything that could be
// read two ways stays an error the model can recover from.
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { runTool } from "../src/tools/index.ts";
import { repairJsonArgs } from "../src/tools/repair.ts";
import type { ToolCall } from "../src/provider/types.ts";
import type { ApprovalRequest, ToolContext } from "../src/tools/types.ts";

let root: string;
let outside: string;

beforeEach(async () => {
  root = await mkdtemp(join(tmpdir(), "marv-repair-"));
  outside = await mkdtemp(join(tmpdir(), "marv-repair-out-"));
  await mkdir(join(root, "src"), { recursive: true });
  await writeFile(join(root, "src/pagination.js"), "\tconst start = 0;\n\tconst end = 10;\n");
  await writeFile(join(outside, "secret.txt"), "top secret\n");
});
afterEach(async () => {
  await rm(root, { recursive: true, force: true });
  await rm(outside, { recursive: true, force: true });
});

let nextId = 0;
/** A call with its arguments left exactly as the model sent them (JSON.stringify would escape the tabs). */
const rawCall = (name: string, args: string): ToolCall => ({ id: `c${nextId++}`, name, arguments: args });
const yes = async (_request: ApprovalRequest) => "yes" as const;
const run = (name: string, args: string, ctx: ToolContext = { root }) => runTool(rawCall(name, args), ctx);
const runApproved = (name: string, args: string) => runTool(rawCall(name, args), { root, sandbox: false, approve: yes });

describe("bash timeout given in milliseconds (#10)", () => {
  test("100000 becomes 100 s, the command runs, and the result says what was repaired", async () => {
    const result = await runApproved("bash", '{"command": "echo repaired-ok", "timeout": 100000}');
    expect(result.isError).toBeFalsy();
    expect(result.output).toContain("repaired-ok");
    expect(result.output).toContain("Marv: the timeout was given in milliseconds (100000): used 100 s");
  });

  test("the repaired timeout is the one used (1 s, not 1000)", async () => {
    const result = await runApproved("bash", '{"command": "sleep 3", "timeout": 1000}');
    expect(result.output).toContain("[timed out after 1s]");
    expect(result.output).toContain("Marv: the timeout was given in milliseconds (1000): used 1 s");
  });

  test("beyond the maximum it's divided and capped there", async () => {
    const result = await runApproved("bash", '{"command": "echo hi", "timeout": 900000}');
    expect(result.isError).toBeFalsy();
    expect(result.output).toContain("used 600 s");
  });

  test("a timeout that could still be seconds is left to fail", async () => {
    const result = await runApproved("bash", '{"command": "echo hi", "timeout": 750}');
    expect(result.isError).toBe(true);
    expect(result.output).toContain("Invalid input for bash");
    expect(result.output).not.toContain("Marv:");
  });

  test("a timeout in seconds gets no note", async () => {
    const result = await runApproved("bash", '{"command": "echo hi", "timeout": 2}');
    expect(result.isError).toBeFalsy();
    expect(result.output).not.toContain("Marv:");
  });
});

describe("raw control characters in the arguments' JSON strings (#10)", () => {
  test("literal tabs in edit_file's strings are escaped and the edit applies", async () => {
    const args = '{"path": "src/pagination.js", "old_string": "\tconst start = 0;", "new_string": "\tconst start = 0; // counted once"}';
    const result = await runApproved("edit_file", args);
    expect(result.isError).toBeFalsy();
    expect(await readFile(join(root, "src/pagination.js"), "utf8")).toContain("\tconst start = 0; // counted once");
    expect(result.output).toContain("Marv: the arguments contained 2 raw control characters");
    expect(result.output).toContain("escaped");
  });

  test("a literal newline in write_file's content writes the two lines", async () => {
    const result = await runApproved("write_file", '{"path": "notes.txt", "content": "first line\nsecond line"}');
    expect(result.isError).toBeFalsy();
    expect(await readFile(join(root, "notes.txt"), "utf8")).toBe("first line\nsecond line");
    expect(result.output).toContain("Marv:");
  });

  test("an escape sequence is left alone, and control characters outside strings don't rescue broken JSON", () => {
    expect(repairJsonArgs('{"a": "x\\ty"}')).toBeNull(); // backslash-t is already the valid encoding
    expect(repairJsonArgs('{\n"a": 1}')).toBeNull(); // a raw newline between tokens is legal whitespace
    expect(repairJsonArgs("{not json\t}")).toBeNull(); // a tab outside strings doesn't make broken JSON parse
    const saved = repairJsonArgs('{"a": "x\ty"}');
    expect(saved).not.toBeNull();
    expect(saved!.value).toEqual({ a: "x\ty" }); // the string's value is what the model meant
  });

  test("broken some other way still says not valid JSON", async () => {
    const result = await run("read_file", "{not json");
    expect(result.isError).toBe(true);
    expect(result.output).toContain("not valid JSON");
  });
});

describe("a project path written with a leading slash (#10)", () => {
  test("edit_file's /src/pagination.js becomes src/pagination.js and the edit applies", async () => {
    const result = await runApproved(
      "edit_file",
      JSON.stringify({ path: "/src/pagination.js", old_string: "\tconst start = 0;", new_string: "\tconst start = 0; // counted once" }),
    );
    expect(result.isError).toBeFalsy();
    expect(await readFile(join(root, "src/pagination.js"), "utf8")).toContain("// counted once");
    expect(result.output).toContain("Marv: the path was written with a leading slash (/src/pagination.js)");
    expect(result.output).toContain("used src/pagination.js");
  });

  test("read_file repairs the same way (read tools run freely)", async () => {
    const result = await run("read_file", JSON.stringify({ path: "/src/pagination.js" }));
    expect(result.isError).toBeFalsy();
    expect(result.output).toContain("const start = 0;");
    expect(result.output).toContain("Marv: the path was written with a leading slash");
  });

  test("an absolute path that exists keeps its error", async () => {
    const result = await run("read_file", JSON.stringify({ path: join(outside, "secret.txt") }));
    expect(result.isError).toBe(true);
    expect(result.output).toContain("outside the project");
    expect(result.output).not.toContain("Marv:");
  });

  test("no relative file with that name either: the error stands", async () => {
    const result = await run("read_file", JSON.stringify({ path: "/missing/thing.ts" }));
    expect(result.isError).toBe(true);
    expect(result.output).toContain("outside the project");
    expect(result.output).not.toContain("Marv:");
  });

  test("an escape out of the project isn't smoothed into a repair", async () => {
    const result = await run("read_file", JSON.stringify({ path: "/../secret.txt" }));
    expect(result.isError).toBe(true);
    expect(result.output).toContain("outside the project");
  });

  test("glob and grep take the same repair for their path", async () => {
    const globbed = await run("glob", JSON.stringify({ pattern: "**/*.js", path: "/src" }));
    expect(globbed.output).toContain("src/pagination.js");
    expect(globbed.output).toContain("Marv: the path was written with a leading slash (/src)");
    const grepped = await run("grep", JSON.stringify({ pattern: "start", path: "/src" }));
    expect(grepped.output).toContain("src/pagination.js:1:");
    expect(grepped.output).toContain("Marv:");
  });
});
