// The project's own checks (.marv/checks/, src/checks): loading them, the runner that runs them in the sandbox, the
// baseline-and-new-violations machinery they share with the typecheck, and the session telling the model.
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { checksNote, parseRunner, projectCheckNotice, ProjectChecks, runnerCommand, type ProjectCheckResult } from "../src/checks/kind.ts";
import { concerns, loadChecks, type ProjectCheck } from "../src/checks/load.ts";
import { Diagnostics, type RunCheck } from "../src/diagnostics/turn.ts";
import type { AgentEvent, ToolCall } from "../src/provider/types.ts";
import { sandboxAvailable } from "../src/sandbox.ts";
import { MarvSession, type SessionEvent } from "../src/session.ts";
import { runCommand } from "../src/tools/bash.ts";
import type { ToolResult } from "../src/tools/types.ts";
import { ScriptedProvider } from "./fake-provider.ts";

let root: string;
beforeEach(() => {
  root = mkdtempSync(join(tmpdir(), "marv-project-checks-"));
});
afterEach(() => rmSync(root, { recursive: true, force: true }));

function put(path: string, content: string) {
  mkdirSync(join(root, path, ".."), { recursive: true });
  writeFileSync(join(root, path), content);
}

const NO_DEFAULT = "glob: 'src/**/*.ts'\nforbid: 'export default'\nmessage: no default exports\nwhy: named exports are easier to find\n";

/** Runs the checks the way the session does, but outside the sandbox (the logic, not the confinement, is tested here). */
const direct: RunCheck = (command, { root, signal, timeoutMs }) => runCommand({ command, root, sandbox: false, network: false, timeoutMs, signal });

async function runAll(dir = root) {
  const { checks } = loadChecks(dir);
  const result = await direct(runnerCommand(checks), { root: dir, signal: new AbortController().signal, timeoutMs: 60_000 });
  return parseRunner(result.output, result.exitCode);
}

describe("loading .marv/checks/", () => {
  test("no folder: no checks, no problems", () => {
    expect(loadChecks(root)).toEqual({ checks: [], problems: [] });
  });

  test("declarative checks (YAML or JSON) and scripts with their leading glob comments", () => {
    put(".marv/checks/no-default.yaml", NO_DEFAULT);
    put(".marv/checks/license.json", JSON.stringify({ glob: ["src/**/*.ts"], exclude: "src/gen/**", require: "^// SPDX", flags: "m", message: "no license header" }));
    put(".marv/checks/custom.ts", "#!/usr/bin/env bun\n// glob: src/**/*.ts, lib/*.ts\n// exclude: src/gen/**\nconsole.log('x');\n// glob: ignored.ts\n");
    put(".marv/checks/anywhere.ts", "console.log('x');\n");
    put(".marv/checks/README.md", "notes, not a check");
    const { checks, problems } = loadChecks(root);
    expect(problems).toEqual([]);
    expect(checks.map((c) => c.name)).toEqual(["anywhere", "custom", "license", "no-default"]);
    expect(checks.find((c) => c.name === "no-default")).toEqual({
      kind: "pattern",
      name: "no-default",
      file: ".marv/checks/no-default.yaml",
      glob: ["src/**/*.ts"],
      exclude: [],
      forbid: "export default",
      flags: "",
      message: "no default exports",
      why: "named exports are easier to find",
    });
    expect(checks.find((c) => c.name === "license")).toMatchObject({ require: "^// SPDX", flags: "m", exclude: ["src/gen/**"] });
    expect(checks.find((c) => c.name === "custom")).toEqual({ kind: "script", name: "custom", file: ".marv/checks/custom.ts", glob: ["src/**/*.ts", "lib/*.ts"], exclude: ["src/gen/**"] });
    expect(checks.find((c) => c.name === "anywhere")).toMatchObject({ glob: ["**"] });
  });

  test("broken ones are skipped and say why", () => {
    put(".marv/checks/a-no-glob.yaml", "forbid: x\nmessage: m\n");
    put(".marv/checks/b-both.yaml", "glob: x\nforbid: a\nrequire: b\nmessage: m\n");
    put(".marv/checks/c-bad-regex.yaml", "glob: x\nforbid: '('\nmessage: m\n");
    put(".marv/checks/d-no-message.yaml", "glob: x\nforbid: a\n");
    put(".marv/checks/e-flags.yaml", "glob: x\nforbid: a\nflags: g\nmessage: m\n");
    put(".marv/checks/f-yaml.yaml", "glob: [unclosed\n");
    put(".marv/checks/g.yaml", NO_DEFAULT);
    put(".marv/checks/g.ts", "");
    const { checks, problems } = loadChecks(root);
    expect(checks.map((c) => `${c.name}:${c.kind}`)).toEqual(["g:script"]);
    expect(problems).toHaveLength(7);
    expect(problems.join("\n")).toContain("a-no-glob.yaml: needs a glob");
    expect(problems.join("\n")).toContain("b-both.yaml: needs exactly one of forbid and require");
    expect(problems.join("\n")).toContain("c-bad-regex.yaml: has a regular expression that doesn't compile");
    expect(problems.join("\n")).toContain("d-no-message.yaml: needs a message");
    expect(problems.join("\n")).toContain("e-flags.yaml: has flags other than");
    expect(problems.join("\n")).toContain("f-yaml.yaml: isn't valid YAML");
    expect(problems.join("\n")).toContain('g.yaml: another check is already named "g"');
  });

  test("a change concerns a check when its glob matches and its exclude doesn't, or when it's the check's own file", () => {
    const check: ProjectCheck = { kind: "script", name: "c", file: ".marv/checks/c.ts", glob: ["src/**/*.ts"], exclude: ["src/gen/**"] };
    expect(concerns(check, "src/a/b.ts")).toBe(true);
    expect(concerns(check, "src/gen/x.ts")).toBe(false);
    expect(concerns(check, "README.md")).toBe(false);
    expect(concerns(check, ".marv/checks/c.ts")).toBe(true);
  });
});

describe("the runner", () => {
  test("a forbidden pattern is reported at each match's line; a required one when a file lacks it; excluded files aren't read", async () => {
    put(".marv/checks/no-default.yaml", NO_DEFAULT);
    put(".marv/checks/header.yaml", "glob: 'src/**/*.ts'\nexclude: 'src/gen/**'\nrequire: '^// header'\nflags: m\nmessage: no header\n");
    put("src/a.ts", "// header\nexport const a = 1;\n\nexport default a;\n");
    put("src/b.ts", "export default 2;\n");
    put("src/gen/c.ts", "export default 3;\n");
    put("README.md", "export default\n");
    expect(await runAll()).toEqual([
      { check: "header", file: "src/b.ts", message: "no header" },
      { check: "no-default", file: "src/a.ts", line: 4, message: "no default exports", why: "named exports are easier to find" },
      { check: "no-default", file: "src/b.ts", line: 1, message: "no default exports", why: "named exports are easier to find" },
      { check: "no-default", file: "src/gen/c.ts", line: 1, message: "no default exports", why: "named exports are easier to find" },
    ]);
  });

  test("a script's JSON lines become violations; a script that crashes or prints something else is a failed check", async () => {
    put(".marv/checks/good.ts", 'console.log(JSON.stringify({ file: "src/a.ts", line: 2, message: "bad", why: "because" }));\nconsole.log(JSON.stringify({ message: "project-wide" }));\n');
    put(".marv/checks/crash.ts", 'throw new Error("boom");\n');
    put(".marv/checks/chatty.ts", 'console.log("checking…");\n');
    const found = await runAll();
    expect(found).toContainEqual({ check: "good", file: "src/a.ts", line: 2, message: "bad", why: "because" });
    expect(found).toContainEqual({ check: "good", message: "project-wide" });
    expect(found).toContainEqual({ check: "crash", message: expect.stringMatching(/^the check failed: exited with code 1/), failed: true });
    expect(found).toContainEqual({ check: "chatty", message: expect.stringContaining("printed a line that isn't a violation"), failed: true });
  });

  test("output that isn't the runner's whole report can't be read", () => {
    expect(parseRunner('{"check":"a","message":"m"}\n{"done":true}\n', 0)).toEqual([{ check: "a", message: "m" }]);
    expect(parseRunner('{"check":"a","message":"m"}\n', 0)).toBeNull(); // no "done": cut off
    expect(parseRunner('preloaded!\n{"done":true}\n', 0)).toBeNull();
    expect(parseRunner('{"done":true}\n', 1)).toBeNull();
    expect(parseRunner("", 0)).toBeNull();
  });

  test("regular expressions and messages with quotes survive the shell", async () => {
    put(".marv/checks/quotes.yaml", `glob: "*.txt"\nforbid: "it's \\"here\\""\nmessage: "don't say it's \\"here\\""\n`);
    put("a.txt", `it's "here"\n`);
    expect(await runAll()).toEqual([{ check: "quotes", file: "a.txt", line: 1, message: `don't say it's "here"` }]);
  });
});

describe("after a step", () => {
  const edited = (path: string): ToolResult[] => [{ output: "ok", summary: "+1", diff: { lines: [], more: 0 }, changed: path }];

  function turn(checks: ProjectChecks) {
    const results: ProjectCheckResult[] = [];
    const statuses: boolean[] = [];
    const t = checks.turn({ root, sandbox: true, signal: new AbortController().signal, onStatus: (s) => statuses.push(s), onResult: (r) => results.push(r) });
    return { checks: t, results, statuses };
  }
  /** The direct runner, counting runs and noting which checks each one ran. */
  function counting() {
    const ran: string[][] = [];
    const run: RunCheck = async (command, opts) => {
      ran.push(["no-default", "header", "notes"].filter((name) => command.includes(`"name":"${name}"`)));
      return direct(command, opts);
    };
    return { run, ran };
  }

  test("a broken check reaches the model with its why; one that was already broken isn't blamed on it", async () => {
    put(".marv/checks/no-default.yaml", NO_DEFAULT);
    put("src/old.ts", "export default 0;\n");
    const { run } = counting();
    const { checks, results } = turn(new ProjectChecks({ run, sandboxWorks: () => true }));
    await checks.beforeChange();
    put("src/a.ts", "export default 1;\n");
    const note = await checks.afterStep(edited("src/a.ts"));
    expect(note).toBe(
      [
        "Marv: the project's checks (.marv/checks/) after your changes found 1 new problem:",
        "src/a.ts:1 [no-default] no default exports",
        "Why [no-default]: named exports are easier to find",
      ].join("\n"),
    );
    expect(results).toEqual([{ status: "done", ms: expect.any(Number), before: 1, errors: 2, added: [expect.objectContaining({ file: "src/a.ts" })] }]);
    // Told once: the same violation after the next step is old news.
    expect(await checks.afterStep(edited("src/a.ts"))).toBeNull();
  });

  test("an obeyed check costs nothing, and a change no check concerns doesn't run any", async () => {
    put(".marv/checks/no-default.yaml", NO_DEFAULT);
    put(".marv/checks/notes.yaml", "glob: 'notes/*.md'\nrequire: '# '\nmessage: a note needs a title\n");
    const { run, ran } = counting();
    const { checks, statuses } = turn(new ProjectChecks({ run, sandboxWorks: () => true }));
    await checks.beforeChange();
    put("src/a.ts", "export const a = 1;\n");
    expect(await checks.afterStep(edited("src/a.ts"))).toBeNull();
    expect(await checks.afterStep(edited("README.md"))).toBeNull();
    // The baseline ran both; the step only the check its file concerns; README.md none.
    expect(ran).toEqual([["no-default", "notes"], ["no-default"]]);
    expect(statuses).toEqual([true, false]);
  });

  test("a pass re-checks only its checks: the others' baseline stands", async () => {
    put(".marv/checks/no-default.yaml", NO_DEFAULT);
    put(".marv/checks/notes.yaml", "glob: 'notes/*.md'\nrequire: '# '\nmessage: a note needs a title\n");
    put("notes/a.md", "untitled\n");
    const { run } = counting();
    const { checks, results } = turn(new ProjectChecks({ run, sandboxWorks: () => true }));
    await checks.beforeChange();
    put("src/a.ts", "export const a = 1;\n");
    await checks.afterStep(edited("src/a.ts"));
    put("notes/b.md", "also untitled\n");
    const note = await checks.afterStep(edited("notes/b.md"));
    expect(note).toContain("notes/b.md [notes] a note needs a title");
    expect(note).not.toContain("notes/a.md");
    expect(results.map((r) => (r.status === "done" ? [r.before, r.errors, r.added.length] : r.status))).toEqual([
      [1, 1, 0],
      [1, 2, 1],
    ]);
  });

  test("a project without checks runs nothing", async () => {
    const { run, ran } = counting();
    const { checks, results } = turn(new ProjectChecks({ run, sandboxWorks: () => true }));
    await checks.beforeChange();
    expect(await checks.afterStep(edited("src/a.ts"))).toBeNull();
    expect(ran).toEqual([]);
    expect(results).toEqual([]);
  });

  test("without the sandbox nothing runs, and the first change says so", async () => {
    put(".marv/checks/no-default.yaml", NO_DEFAULT);
    const { run, ran } = counting();
    const { checks, results } = turn(new ProjectChecks({ run, sandboxWorks: () => false }));
    await checks.beforeChange();
    expect(ran).toEqual([]);
    expect(results).toEqual([{ status: "off", reason: "no-sandbox" }]);
    expect(projectCheckNotice(results[0]!)).toContain("only in the sandbox");
  });

  test("bun missing from the sandbox is said plainly, once", async () => {
    put(".marv/checks/no-default.yaml", NO_DEFAULT);
    const run: RunCheck = async () => ({ output: "bash: line 2: bun: command not found\n", exitCode: 127, timedOut: false, aborted: false });
    const { checks, results } = turn(new ProjectChecks({ run, sandboxWorks: () => true }));
    await checks.beforeChange();
    expect(results).toEqual([{ status: "failed", ms: expect.any(Number), reason: "missing-interpreter", tool: "bun", notify: true }]);
    expect(projectCheckNotice(results[0]!)).toContain("need bun");
  });

  test("the note shows 20 violations, counts the rest, and gives each check's why once", () => {
    const added = Array.from({ length: 23 }, (_, i) => ({ check: "x", file: `f${i}.ts`, line: 1, message: "m", why: "w" }));
    const note = checksNote(added);
    expect(note).toContain("found 23 new problems");
    expect(note).toContain("f19.ts:1 [x] m");
    expect(note).not.toContain("f20.ts");
    expect(note).toContain("and 3 more.");
    expect(note.match(/Why \[x\]/g)).toHaveLength(1);
  });
});

/** Marv's own starter checks, copied into a scratch project: each fires on a violation and stays quiet otherwise. */
describe("Marv's starter checks", () => {
  const repo = join(import.meta.dir, "..");
  beforeEach(() => {
    for (const file of ["prompt-cache.ts", "write-shrink.ts", "imports.yaml", "render-options.yaml"]) {
      put(`.marv/checks/${file}`, require("node:fs").readFileSync(join(repo, ".marv", "checks", file), "utf8"));
    }
  });
  const only = async (check: string) => (await runAll())!.filter((v) => v.check === check);

  test("they load, and run clean on Marv's own repository", async () => {
    expect(loadChecks(repo).checks.map((c) => c.name)).toEqual(["imports", "prompt-cache", "render-options", "write-shrink"]);
    expect(loadChecks(repo).problems).toEqual([]);
    // write-shrink compares with HEAD, so a work in progress could trip it; the rules from the code can't.
    expect((await runAll(repo))!.filter((v) => v.check !== "write-shrink")).toEqual([]);
  });

  test("imports: a relative import without its extension", async () => {
    put("src/ok.ts", 'import { a } from "./a.ts";\nimport type { B } from "../b.tsx";\nimport data from "./d.json";\nconst x = await import("./x.ts");\nexport { y } from "./y.ts";\n');
    expect(await only("imports")).toEqual([]);
    put("src/bad.ts", 'import { a } from "./a";\nimport {\n  b,\n} from "../b";\nimport "./side";\nconst x = await import("./x");\nexport * from "./y";\nimport { z } from "zod";\n');
    expect((await only("imports")).map((v) => `${v.file}:${v.line}`)).toEqual(["src/bad.ts:1", "src/bad.ts:4", "src/bad.ts:5", "src/bad.ts:6", "src/bad.ts:7"]);
    expect((await only("imports"))[0]!.why).toContain("allowImportingTsExtensions");
  });

  test("render options: incrementalRendering must stay true", async () => {
    put("src/render-options.ts", "export const renderOptions = {\n  alternateScreen: true,\n  incrementalRendering: true,\n};\n");
    expect(await only("render-options")).toEqual([]);
    put("src/render-options.ts", "export const renderOptions = {\n  alternateScreen: true,\n  incrementalRendering: false,\n};\n");
    expect(await only("render-options")).toEqual([expect.objectContaining({ file: "src/render-options.ts", why: expect.stringContaining("flickers") })]);
  });

  test("prompt cache: buildSystem() and this.system = only in the constructor and clear()", async () => {
    const session = (extra: string) =>
      [
        "export class MarvSession {",
        "  private system: string;",
        "  constructor() {",
        "    this.system = this.buildSystem();",
        "  }",
        "  clear(): Promise<void> {",
        "    const reload = (async () => {",
        "      this.system = this.buildSystem();",
        "    })();",
        "    return reload;",
        "  }",
        extra,
        "  private buildSystem(): string {",
        '    return "x";',
        "  }",
        "}",
        "",
      ].join("\n");
    put("src/session.ts", session(""));
    put("src/prompt.ts", "export function systemPrompt() {\n  return 'x';\n}\n");
    expect(await only("prompt-cache")).toEqual([]);
    put("src/session.ts", session("  async send(text: string) {\n    this.system = this.buildSystem() + text;\n  }"));
    put("src/app.tsx", "session.buildSystem();\n");
    const found = await only("prompt-cache");
    expect(found.map((v) => `${v.file}:${v.line} ${v.message}`)).toEqual([
      "src/app.tsx:1 buildSystem() called outside MarvSession: the system prompt is built only in the constructor and clear()",
      "src/session.ts:13 buildSystem() called in send(): the system prompt is built only in the constructor and clear()",
    ]);
    expect(found[0]!.why).toContain("cache");
  });

  test("write_file shrink: a tracked file that lost more than half its committed lines", async () => {
    const git = (...args: string[]) => Bun.spawnSync(["git", "-c", "user.name=t", "-c", "user.email=t@t", ...args], { cwd: root, stdout: "pipe", stderr: "pipe" });
    const lines = (n: number) => Array.from({ length: n }, (_, i) => `line ${i}`).join("\n") + "\n";
    put("src/bash.ts", lines(301));
    put("src/small.ts", lines(10));
    expect(await only("write-shrink")).toEqual([]); // not a repository: nothing to compare with
    git("init", "-q");
    git("add", ".");
    git("commit", "-qm", "start");
    put("src/bash.ts", lines(200));
    put("src/small.ts", lines(2)); // too small to matter
    put("src/new.ts", lines(1));
    expect(await only("write-shrink")).toEqual([]);
    put("src/bash.ts", lines(72));
    expect(await only("write-shrink")).toEqual([
      { check: "write-shrink", file: "src/bash.ts", message: "shrank to less than half of its 301 committed lines", why: expect.stringContaining("edit_file") },
    ]);
  });
});

const say = (text: string): AgentEvent[] => [{ type: "text_delta", text }, { type: "done", reason: "stop" }];
const write = (path: string, content: string): AgentEvent[] => [
  { type: "tool_call", call: { id: `w-${path}`, name: "write_file", arguments: JSON.stringify({ path, content }) } satisfies ToolCall },
  { type: "done", reason: "tool_calls" },
];

describe("in a session", () => {
  test("the typecheck's and the checks' notes reach the model as one message, and \"checking\" lasts until both are done", async () => {
    put(".marv/checks/no-default.yaml", NO_DEFAULT);
    put("tsconfig.json", "{}");
    put("node_modules/.bin/tsc", "");
    const answers = [
      { output: "", exitCode: 0, timedOut: false, aborted: false },
      { output: "src/a.ts(1,1): error TS2304: Cannot find name 'x'.\n", exitCode: 2, timedOut: false, aborted: false },
    ];
    const tsc: RunCheck = async () => (await Bun.sleep(20), answers.shift()!);
    const provider = new ScriptedProvider([write("src/a.ts", "export default x;\n"), say("Done.")]);
    const session = new MarvSession({
      root,
      provider: { id: "test", model: "test", make: () => provider },
      typecheck: new Diagnostics({ run: tsc, sandboxWorks: () => true }),
      projectChecks: new ProjectChecks({ run: direct, sandboxWorks: () => true }),
    });
    const events: SessionEvent[] = [];
    for await (const event of session.send("go")) events.push(event);
    await session.close();
    const note = provider.requests[1]!.history.at(-1);
    expect(note?.role).toBe("user");
    expect(note?.text).toStartWith("Marv: the typecheck (tsc) after your changes found 1 new error");
    expect(note?.text).toContain("\n\nMarv: the project's checks (.marv/checks/) after your changes found 1 new problem:\nsrc/a.ts:1 [no-default]");
    const statuses = events.flatMap((e) => (e.type === "status" ? [e.status] : []));
    expect(statuses).toEqual(["running", "checking", "running"]);
    expect(events.filter((e) => e.type === "check" || e.type === "checks").map((e) => e.type).sort()).toEqual(["check", "checks"]);
  });
});

describe.if(sandboxAvailable())("in a session, in the real sandbox", () => {
  async function turn(init: { checks?: boolean } = {}, ...steps: AgentEvent[][]) {
    const provider = new ScriptedProvider([...steps, say("Done.")]);
    const session = new MarvSession({ root, provider: { id: "test", model: "test", make: () => provider }, ...init });
    const events: SessionEvent[] = [];
    for await (const event of session.send("go")) events.push(event);
    await session.close();
    return { provider, events };
  }
  beforeEach(() => put(".marv/checks/no-default.yaml", NO_DEFAULT));

  test("a check the change breaks is in the model's next request, with its why", async () => {
    const { provider, events } = await turn({}, write("src/a.ts", "export default 1;\n"));
    const note = provider.requests[1]!.history.at(-1);
    expect(note?.role).toBe("user");
    expect(note?.text).toContain("src/a.ts:1 [no-default] no default exports");
    expect(note?.text).toContain("Why [no-default]: named exports are easier to find");
    expect(events).toContainEqual({ type: "checks", result: expect.objectContaining({ status: "done", added: [expect.objectContaining({ check: "no-default" })] }) });
  }, 30_000);

  test("an obeyed check adds nothing, and an unrelated change runs no check after the step", async () => {
    const obeyed = await turn({}, write("src/a.ts", "export const a = 1;\n"));
    expect(obeyed.provider.requests[1]!.history.at(-1)?.role).toBe("tool");
    const unrelated = await turn({}, write("README.md", "export default\n"));
    expect(unrelated.provider.requests[1]!.history.at(-1)?.role).toBe("tool");
    expect(unrelated.events.some((e) => e.type === "checks")).toBe(false);
  }, 30_000);

  test("checks: false runs nothing", async () => {
    const { provider, events } = await turn({ checks: false }, write("src/a.ts", "export default 1;\n"));
    expect(provider.requests[1]!.history.at(-1)?.role).toBe("tool");
    expect(events.some((e) => e.type === "checks")).toBe(false);
  }, 30_000);
});
