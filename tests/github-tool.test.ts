// The github tool (#47) against a fake gh on PATH (tests/fixtures/fake-gh.ts): reads run unasked, writes ask as the
// github door, gh is spawned without a shell and without Marv's keys, tokens are redacted, and the tool is offered
// only where gh is there and logged in, in the same spec list every time (the prompt cache).
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, symlinkSync } from "node:fs";
import { mkdtemp, mkdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { buildApprover } from "../src/headless.ts";
import { MarvSession } from "../src/session.ts";
import { GH_NOTE, GH_TOOL_NOTE, bash } from "../src/tools/bash.ts";
import { githubAvailable, githubWanted, ghLine, projectHost } from "../src/tools/github/github.ts";
import { runTool, toolSpecsFor } from "../src/tools/index.ts";
import type { ApprovalRequest, Decision } from "../src/tools/types.ts";
import { sandboxAvailable } from "../src/sandbox.ts";
import { ScriptedProvider } from "./fake-provider.ts";
import { fakeGhCalls, installFakeGh } from "./fixtures/fake-gh.ts";

let bin: string;
let root: string;
let log: string;
const saved: Record<string, string | undefined> = {};
const ENV = ["PATH", "FAKE_GH_LOG", "FAKE_GH_OUTPUT", "FAKE_GH_LOGGED_IN", "FAKE_GH_HANG", "FAKE_GH_EXIT", "FAKE_GH_STDERR", "OPENROUTER_API_KEY", "MARV_SECRET", "GH_DEBUG", "GH_CONFIG_DIR", "FAKE_GH_NO_REPO", "GH_HOST"];

beforeEach(async () => {
  for (const name of ENV) saved[name] = process.env[name];
  bin = await installFakeGh();
  root = await mkdtemp(join(tmpdir(), "marv-gh-tool-"));
  log = join(bin, "calls.jsonl");
  Bun.spawnSync(["git", "init", "-q"], { cwd: root });
  Bun.spawnSync(["git", "remote", "add", "origin", "https://github.com/o/r.git"], { cwd: root });
  process.env.PATH = `${bin}:${saved.PATH}`;
  process.env.FAKE_GH_LOG = log;
  process.env.FAKE_GH_LOGGED_IN = "1";
  process.env.OPENROUTER_API_KEY = "sk-or-secret";
  process.env.MARV_SECRET = "marv-secret";
  process.env.GH_DEBUG = "api";
});
afterEach(async () => {
  for (const name of ENV) {
    if (saved[name] === undefined) delete process.env[name];
    else process.env[name] = saved[name];
  }
  await rm(bin, { recursive: true, force: true });
  await rm(root, { recursive: true, force: true });
});

const call = (args: string[], id = "g1") => ({ id, name: "github", arguments: JSON.stringify({ args }) });
const recorder = (answer: Decision = "yes") => {
  const asked: ApprovalRequest[] = [];
  return { asked, approve: async (r: ApprovalRequest) => (asked.push(r), answer) };
};

describe("the github tool", () => {
  for (const yolo of [true, false]) {
    test(`a read runs without asking (yolo ${yolo ? "on" : "off"}), as gh with the project as cwd`, async () => {
      const { asked, approve } = recorder();
      const result = await runTool(call(["pr", "list"]), { root, yolo, approve, doors: { github: true } });
      expect(asked).toEqual([]);
      expect(result).toMatchObject({ label: "gh pr list", door: "github" });
      expect(result.output).toContain("fake gh pr list");
      expect(result.output).toContain("[exit code 0]");
      const calls = await fakeGhCalls(log);
      expect(calls.map((c) => c.args)).toEqual([["pr", "list"]]);
      expect(calls[0]!.cwd).toBe(root);
    });
  }

  test("a write asks, as the github door, scoped by command, and names the repository for a merge", async () => {
    const { asked, approve } = recorder("yes");
    const result = await runTool(call(["pr", "merge", "12", "--squash"]), { root, yolo: true, approve, doors: { github: true } });
    expect(asked).toHaveLength(1);
    expect(asked[0]).toMatchObject({ tool: "github", door: "github", network: true, scope: { key: "github:pr merge", description: "gh pr merge" } });
    expect(asked[0]!.once).toBeUndefined();
    expect(asked[0]!.preview.command).toBe("gh pr merge 12 --squash");
    expect(asked[0]!.preview.warning).toBe("Visible to other people on github.com/o/r.");
    expect(result.approval).toBe("yes");
    expect((await fakeGhCalls(log)).map((c) => c.args).at(-1)).toEqual(["pr", "merge", "12", "--squash"]);
  });

  test("a merge whose repository can't be told says so, rather than naming none", async () => {
    process.env.FAKE_GH_NO_REPO = "1";
    const { asked, approve } = recorder("no");
    await runTool(call(["pr", "merge", "12"]), { root, yolo: true, approve, doors: { github: true } });
    expect(asked[0]!.preview.warning).toBe("Couldn't tell which repository: gh picks it from this folder.");
  });

  test("a declined write never reaches gh", async () => {
    const { approve } = recorder("no");
    const result = await runTool(call(["issue", "comment", "3", "--body", "hi"]), { root, approve, doors: { github: true } });
    expect(result.declined).toBe(true);
    expect(await fakeGhCalls(log)).toEqual([]);
  });

  test("a sensitive API read asks; a field that reads a file says so", async () => {
    const { asked, approve } = recorder("no");
    await runTool(call(["api", "repos/o/r/hooks"]), { root, approve, doors: { github: true } });
    await runTool(call(["api", "repos/o/r/issues", "-F", "body=@/etc/hostname"], "g2"), { root, approve, doors: { github: true } });
    expect(asked.map((r) => r.scope.key)).toEqual(["github:api sensitive", "github:api POST"]);
    expect(asked[0]!.preview.warning).toContain("webhook");
    expect(asked[1]!.preview.warning).toContain("Reads /etc/hostname on your machine");
    // One sensitive path approved "always" must not cover the others (they share a key): each is asked every time.
    expect(asked[0]!.once).toBe(true);
    expect(asked[1]!.once).toBeUndefined();
  });

  test("gh is spawned without a shell: an argument arrives as written", async () => {
    const marker = join(root, "x");
    const result = await runTool(call(["search", "issues", `$(touch ${marker})`]), { root, doors: { github: true } });
    expect(result.isError).toBeUndefined();
    expect((await fakeGhCalls(log))[0]!.args).toEqual(["search", "issues", `$(touch ${marker})`]);
    expect(existsSync(marker)).toBe(false);
  });

  test("its environment: no provider keys or debug switches, never prompts, git without the repository's programs", async () => {
    await runTool(call(["pr", "status"]), { root, doors: { github: true } });
    const { env } = (await fakeGhCalls(log))[0]!;
    expect(env).toMatchObject({ OPENROUTER_API_KEY: null, MARV_SECRET: null, GH_DEBUG: null, GH_PROMPT_DISABLED: "1", GH_PAGER: "cat", NO_COLOR: "1" });
    expect(env).toMatchObject({ GIT_CONFIG_COUNT: "2", GIT_CONFIG_KEY_0: "core.hooksPath", GIT_CONFIG_VALUE_0: "/dev/null", GIT_CONFIG_KEY_1: "core.fsmonitor", GIT_CONFIG_VALUE_1: "false" });
  });

  test("gh missing while the repository is looked up isn't remembered: once it's back, a merge names the repository", async () => {
    const { asked, approve } = recorder("no");
    // git still there (the project's host comes from it), gh not: looking up the repository fails to spawn.
    const gitOnly = join(root, ".git-only-bin");
    mkdirSync(gitOnly);
    symlinkSync(Bun.which("git")!, join(gitOnly, "git"));
    process.env.PATH = gitOnly;
    await runTool(call(["pr", "merge", "12"]), { root, yolo: true, approve, doors: { github: true } });
    process.env.PATH = `${bin}:${saved.PATH}`;
    await runTool(call(["pr", "merge", "12"], "g2"), { root, yolo: true, approve, doors: { github: true } });
    expect(asked.at(-1)!.preview.warning).toBe("Visible to other people on github.com/o/r.");
  });

  test("gh talks to the project's host, whatever GH_HOST says: -R OWNER/REPO can't reach another one", async () => {
    process.env.GH_HOST = "ghe.example.com";
    await runTool(call(["pr", "view", "1", "-R", "someone/else"]), { root, doors: { github: true } });
    expect((await fakeGhCalls(log)).at(-1)!.env.GH_HOST).toBe("github.com");
  });

  test("tokens in gh's output and errors are redacted, and the result says how many", async () => {
    process.env.FAKE_GH_OUTPUT = `body: ghp_${"A".repeat(36)}`;
    process.env.FAKE_GH_STDERR = `warning: github_pat_${"b".repeat(30)}`;
    const result = await runTool(call(["issue", "view", "1"]), { root, doors: { github: true } });
    expect(result.output).not.toContain("ghp_A");
    expect(result.output).not.toContain("github_pat_b");
    expect(result.output).toContain("body: [redacted token]");
    expect(result.output).toContain("Marv: 2 tokens were redacted from this output.");
    expect(result.summary).toContain("2 tokens redacted");
  });

  test("a failing gh reports its exit code", async () => {
    process.env.FAKE_GH_EXIT = "4";
    process.env.FAKE_GH_STDERR = "not found";
    const result = await runTool(call(["pr", "view", "999"]), { root, doors: { github: true } });
    expect(result.output).toContain("not found");
    expect(result.output).toContain("[exit code 4]");
  });

  test("refused calls fail before gh runs, with the reason", async () => {
    const result = await runTool(call(["auth", "token"]), { root, approve: async () => "yes", doors: { github: true } });
    expect(result).toMatchObject({ isError: true });
    expect(result.output).toContain("Marv keeps the token away from the conversation");
    expect(await fakeGhCalls(log)).toEqual([]);
  });

  test("a read naming another host is refused in run (needsApproval can't see the project)", async () => {
    const result = await runTool(call(["pr", "list", "-R", "ghe.corp/o/r"]), { root, doors: { github: true } });
    expect(result.isError).toBe(true);
    expect(result.output).toContain("ghe.corp");
    expect(await fakeGhCalls(log)).toEqual([]);
  });

  test("on GitHub Enterprise, the project's own host is allowed", async () => {
    Bun.spawnSync(["git", "remote", "set-url", "origin", "git@ghe.corp:o/r.git"], { cwd: root });
    expect(projectHost(root)).toBe("ghe.corp");
    const result = await runTool(call(["api", "--hostname", "ghe.corp", "repos/o/r"]), { root, doors: { github: true } });
    expect(result.isError).toBeUndefined();
    const other = await runTool(call(["api", "--hostname", "github.com", "repos/o/r"]), { root, doors: { github: true } });
    expect(other.isError).toBe(true);
  });

  test("outside a repository the host is github.com", async () => {
    const plain = await mkdtemp(join(tmpdir(), "marv-gh-plain-"));
    try {
      expect(projectHost(plain)).toBe("github.com");
    } finally {
      await rm(plain, { recursive: true, force: true });
    }
  });

  test("without the github door (a subagent), writes and sensitive reads are refused; reads run", async () => {
    const { asked, approve } = recorder("yes");
    const write = await runTool(call(["pr", "comment", "1", "--body", "x"]), { root, approve });
    const sensitive = await runTool(call(["api", "user/emails"], "g2"), { root, approve });
    const read = await runTool(call(["pr", "view", "1"], "g3"), { root, approve });
    expect(write.output).toContain("Only the main agent can change GitHub");
    expect(sensitive.output).toContain("Only the main agent");
    expect(read.isError).toBeUndefined();
    expect(asked).toEqual([]);
    expect((await fakeGhCalls(log)).map((c) => c.args)).toEqual([["pr", "view", "1"]]);
  });

  test("the label quotes what a shell would split", () => {
    expect(ghLine(["issue", "create", "--title", "it's broken", "--body", "a b"])).toBe(`gh issue create --title 'it'\\''s broken' --body 'a b'`);
  });

  test("an argument with a NUL, or more than 40, is invalid", async () => {
    expect((await runTool(call(["pr", "view\0x"]), { root })).output).toContain("Invalid input");
    expect((await runTool(call(Array(41).fill("x")), { root })).output).toContain("Invalid input");
  });
});

describe("availability", () => {
  test("offered when gh is on PATH and logged in; not when logged out or missing", async () => {
    expect(await githubAvailable()).toBe(true);
    process.env.FAKE_GH_LOGGED_IN = "0";
    expect(await githubAvailable()).toBe(false);
    expect(await githubAvailable({ env: { PATH: "/nonexistent" } })).toBe(false);
  });

  test("when gh auth status doesn't answer in time, gh's hosts.yml decides", async () => {
    process.env.FAKE_GH_HANG = "1";
    const config = join(bin, "gh-config");
    await mkdir(config);
    process.env.GH_CONFIG_DIR = config;
    expect(await githubAvailable({ timeoutMs: 200 })).toBe(false);
    await writeFile(join(config, "hosts.yml"), "github.com:\n  user: me\n");
    expect(await githubAvailable({ timeoutMs: 200 })).toBe(true);
  });

  test("MARV_GITHUB=off hides it", () => {
    expect(githubWanted({})).toBe(true);
    for (const off of ["off", "0", "false", "NO"]) expect(githubWanted({ MARV_GITHUB: off })).toBe(false);
  });

  test("the spec list has it only when offered, the same objects every time (cache)", () => {
    expect(toolSpecsFor({ hasSkills: false }).map((s) => s.name)).not.toContain("github");
    const a = toolSpecsFor({ hasSkills: false, github: true });
    expect(a.map((s) => s.name)).toContain("github");
    expect(toolSpecsFor({ hasSkills: false, github: true })).toEqual(a);
    expect(toolSpecsFor({ hasSkills: false, github: true })[0]).toBe(a[0]!);
  });

  test("a session with github offers it, names its door in the prompt, and sends the same specs each request", async () => {
    const provider = new ScriptedProvider([
      [{ type: "text_delta", text: "a" }, { type: "done", reason: "stop" }],
      [{ type: "text_delta", text: "b" }, { type: "done", reason: "stop" }],
    ]);
    const session = new MarvSession({ root, provider, github: true });
    await Array.fromAsync(session.send("one"));
    await Array.fromAsync(session.send("two"));
    const [first, second] = provider.requests;
    expect(first!.options.tools!.map((t) => t.name)).toContain("github");
    expect(second!.options.tools).toEqual(first!.options.tools!);
    expect(first!.options.system).toContain("# Outside the sandbox");
    expect(first!.options.system).toContain("github runs the GitHub CLI (gh) with the user's own login");
    await session.close();

    const without = new ScriptedProvider([[{ type: "text_delta", text: "a" }, { type: "done", reason: "stop" }]]);
    const plain = new MarvSession({ root, provider: without });
    await Array.fromAsync(plain.send("one"));
    expect(without.requests[0]!.options.tools!.map((t) => t.name)).not.toContain("github");
    expect(without.requests[0]!.options.system).not.toContain("github runs the GitHub CLI");
    await plain.close();
  });

  test("a session without github doesn't run it, even when the model names it", async () => {
    const provider = new ScriptedProvider([
      [{ type: "tool_call", call: call(["pr", "list"]) }, { type: "done", reason: "tool_calls" }],
      [{ type: "text_delta", text: "ok" }, { type: "done", reason: "stop" }],
    ]);
    const session = new MarvSession({ root, provider });
    const events = await Array.fromAsync(session.send("go"));
    const end = events.find((e) => e.type === "tool_end");
    expect(end?.type === "tool_end" && end.result.output).toContain('Unknown tool "github"');
    expect(await fakeGhCalls(log)).toEqual([]);
    await session.close();
  });

  test("a caller's own tool may be called github where the session doesn't offer it", () => {
    const own = { name: "github", description: "mine", input: bash.input, label: () => "x", run: async () => ({ output: "", summary: "" }) };
    expect(() => new MarvSession({ root, provider: new ScriptedProvider([]), tools: [own] })).not.toThrow();
    expect(() => new MarvSession({ root, provider: new ScriptedProvider([]), tools: [own], github: true })).toThrow("already built in");
  });
});

describe("marv -p", () => {
  test("--approve github approves writes and sensitive reads, nothing else", async () => {
    const [approve] = buildApprover(["github"]);
    const ask = (key: string) => approve!({ tool: "github", label: "", preview: { title: "" }, scope: { key, description: "" } });
    expect(await ask("github:pr merge")).toBe("yes");
    expect(await ask("github:api sensitive")).toBe("yes");
    expect(await ask("bash:echo")).toBe("unavailable");
    const [none] = buildApprover(["network"]);
    expect(await none!({ tool: "github", label: "", preview: { title: "" }, scope: { key: "github:pr merge", description: "" } })).toBe("unavailable");
  });
});

describe.if(sandboxAvailable())("bash's gh note", () => {
  test("points to the github tool where the session has it, to the MCP server where it doesn't", async () => {
    const run = (doors?: { github?: boolean }) => bash.run({ command: "gh pr list || exit 1" }, { root, doors });
    expect((await run({ github: true })).output).toContain(GH_TOOL_NOTE);
    const without = (await run()).output;
    expect(without).toContain(GH_NOTE);
    expect(without).not.toContain(GH_TOOL_NOTE);
  });
});
