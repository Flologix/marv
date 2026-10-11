import { afterEach, beforeEach, describe, expect, spyOn, test } from "bun:test";
import { existsSync } from "node:fs";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { runAgent } from "../src/agent.ts";
import { GENERAL_PURPOSE, type AgentType } from "../src/agents.ts";
import type { MemoryPaths } from "../src/memory.ts";
import type { AgentEvent, ChatTurn, Provider, StreamOptions, ToolCall, Usage } from "../src/provider/types.ts";
import { sandboxAvailable } from "../src/sandbox.ts";
import { SUBAGENT_MAX_STEPS, subagentContext } from "../src/subagent.ts";
import { isParallelCall, runTool } from "../src/tools/index.ts";
import type { AgentHost, AgentProgress, ApprovalRequest, Decision, ToolContext } from "../src/tools/types.ts";
import * as worktreeModule from "../src/worktree.ts";
import { NOT_A_REPO, worktreeEnv, type Worktree } from "../src/worktree.ts";
import { RoutedProvider, ScriptedProvider } from "./fake-provider.ts";

let root: string;
let trees: string;
const git = (cwd: string, ...args: string[]) => Bun.spawnSync(["git", ...args], { cwd }).stdout.toString().trim();

beforeEach(async () => {
  root = await mkdtemp(join(tmpdir(), "marv-agent-tool-"));
  trees = await mkdtemp(join(tmpdir(), "marv-agent-trees-"));
  await writeFile(join(root, "notes.txt"), "remember the milk\n");
});
afterEach(async () => {
  await rm(root, { recursive: true, force: true });
  await rm(trees, { recursive: true, force: true });
});

const say = (text: string, usage?: Usage): AgentEvent[] => [
  { type: "text_delta", text },
  ...(usage ? [{ type: "usage" as const, usage }] : []),
  { type: "done", reason: "stop" },
];
const useTool = (id: string, name: string, args: unknown): AgentEvent[] => [
  { type: "tool_call", call: { id, name, arguments: JSON.stringify(args) } },
  { type: "done", reason: "tool_calls" },
];
const agentCall = (args: Record<string, unknown>, id = "a1"): ToolCall => ({ id, name: "agent", arguments: JSON.stringify(args) });

function makeHost(provider: Provider, over: Partial<AgentHost> = {}) {
  const seen = { usage: [] as Usage[], progress: [] as AgentProgress[], models: [] as (string | undefined)[] };
  const host: AgentHost = {
    agents: [GENERAL_PURPOSE],
    providerFor: (model) => (seen.models.push(model), provider),
    cwd: "~/proj",
    worktreesDir: trees,
    onUsage: (u) => seen.usage.push(u),
    onProgress: (_id, p) => seen.progress.push(p),
    ...over,
  };
  return { host, seen };
}

function ctxWith(host: AgentHost, decide: (r: ApprovalRequest) => Decision = () => "yes") {
  const asked: ApprovalRequest[] = [];
  const ctx: ToolContext = { root, agentHost: host, approve: async (r) => (asked.push(r), decide(r)) };
  return { ctx, asked };
}

describe("the agent tool", () => {
  test("the subagent starts fresh, and only its last message comes back", async () => {
    const provider = new ScriptedProvider([say("Found 3 callers.", { promptTokens: 1000, completionTokens: 200 })]);
    const { host, seen } = makeHost(provider);
    const { ctx, asked } = ctxWith(host);
    const result = await runTool(agentCall({ description: "find callers", prompt: "Find callers of foo" }), ctx);

    expect(provider.requests[0]!.history).toEqual([{ role: "user", text: "Find callers of foo" }]);
    expect(result.output).toBe("Found 3 callers.");
    expect(result.summary).toBe('done · 0 tools · 1.2k tokens · "Found 3 callers."');
    expect(result.label).toBe("general-purpose · find callers");
    expect(asked).toHaveLength(0); // a shared-folder dispatch doesn't ask
    expect(seen.usage).toEqual([{ promptTokens: 1000, completionTokens: 200 }]);
  });

  test("it gets subagent tools only, and its type's instructions", async () => {
    const reviewer: AgentType = { ...GENERAL_PURPOSE, name: "reviewer", body: "You review code.", tools: ["read_file", "grep"], source: "project" };
    const provider = new ScriptedProvider([say("ok"), say("ok")]);
    const { host } = makeHost(provider, { agents: [GENERAL_PURPOSE, reviewer] });
    const { ctx } = ctxWith(host);
    await runTool(agentCall({ description: "x", prompt: "p" }), ctx);
    await runTool(agentCall({ type: "superpowers:reviewer", description: "x", prompt: "p" }), ctx);

    expect(provider.requests[0]!.options.tools!.map((t) => t.name)).toEqual(["read_file", "glob", "grep", "web_fetch", "edit_file", "write_file", "bash"]);
    expect(provider.requests[1]!.options.tools!.map((t) => t.name)).toEqual(["read_file", "grep"]);
    expect(provider.requests[1]!.options.system).toStartWith("You review code.");
  });

  test("a tool it wasn't given is refused, even if the model names it", async () => {
    const provider = new ScriptedProvider([useTool("m1", "memory", { action: "add", text: "x" }), say("ok")]);
    const { host } = makeHost(provider);
    await runTool(agentCall({ description: "x", prompt: "p" }), ctxWith(host).ctx);
    expect(provider.requests[1]!.history.at(-1)).toMatchObject({ role: "tool", text: expect.stringContaining('Unknown tool "memory"') });
  });

  test("it works in the project, and reports progress", async () => {
    const provider = new ScriptedProvider([useTool("r1", "read_file", { path: "notes.txt" }), say("It says milk.")]);
    const { host, seen } = makeHost(provider);
    const result = await runTool(agentCall({ description: "read notes", prompt: "What's in notes.txt?" }), ctxWith(host).ctx);
    expect(result.summary).toStartWith("done · 1 tool ·");
    expect(seen.progress.map((p) => p.line)).toContain("shared folder · 1 tool · read_file notes.txt");
    expect(seen.progress.at(-1)!.steps).toEqual(["read_file notes.txt · 1 line"]);
  });

  test("everything its loop does reaches the host, for its own view", async () => {
    const provider = new ScriptedProvider([useTool("r1", "read_file", { path: "notes.txt" }), say("It says milk.")]);
    const events: [string, string][] = [];
    const { host } = makeHost(provider, { onEvent: (id, e) => events.push([id, e.type]) });
    await runTool(agentCall({ description: "read notes", prompt: "p" }, "a7"), ctxWith(host).ctx);
    expect(events.every(([id]) => id === "a7")).toBe(true);
    expect(events.map(([, type]) => type)).toEqual(["tool_start", "tool_end", "text_delta", "assistant", "done"]);
  });

  test("an error returns what it had, marked as an error", async () => {
    const provider = new ScriptedProvider([[{ type: "text_delta", text: "Partial findings." }, { type: "error", message: "Rate limited" }]]);
    const { host } = makeHost(provider);
    const result = await runTool(agentCall({ description: "x", prompt: "p" }), ctxWith(host).ctx);
    expect(result.isError).toBe(true);
    expect(result.output).toStartWith("Partial findings.");
    expect(result.output).toContain("[Marv: It stopped on an error: Rate limited]");
    expect(result.summary).toStartWith("stopped ·");
  });

  test("a subagent at its step limit says to start a new one, not to say \"continue\" (its history is gone)", async () => {
    const steps = Array.from({ length: SUBAGENT_MAX_STEPS }, (_, i) => useTool(`r${i}`, "read_file", { path: `notes-${i}.txt` }));
    const { host } = makeHost(new ScriptedProvider(steps));
    const result = await runTool(agentCall({ description: "endless", prompt: "p" }), ctxWith(host).ctx);
    expect(result.output).toContain(`It hit its ${SUBAGENT_MAX_STEPS}-step limit before finishing`);
    expect(result.output).toContain("start a new subagent");
    expect(result.output).not.toContain("continue");
  });

  test("a type's model gets its own provider", async () => {
    const fast: AgentType = { ...GENERAL_PURPOSE, name: "fast", model: "small-model", source: "project" };
    const { host, seen } = makeHost(new ScriptedProvider([say("ok")]), { agents: [GENERAL_PURPOSE, fast] });
    await runTool(agentCall({ type: "fast", description: "x", prompt: "p" }), ctxWith(host).ctx);
    expect(seen.models).toEqual(["small-model"]);
  });

  test("unknown types and nesting are errors the model can read", async () => {
    const { host } = makeHost(new ScriptedProvider([]));
    expect((await runTool(agentCall({ type: "nope", description: "x", prompt: "p" }), ctxWith(host).ctx)).output).toContain('no agent type "nope". Available: general-purpose');
    expect((await runTool(agentCall({ description: "x", prompt: "p" }), { root })).output).toContain("Subagents aren't available here.");
  });

  test("a shared-folder subagent's changes are approved by the user, labeled with who asks", async () => {
    const provider = new ScriptedProvider([useTool("w1", "write_file", { path: "made.txt", content: "hi\n" }), say("Made it.")]);
    const { host } = makeHost(provider);
    const { ctx, asked } = ctxWith(host);
    await runTool(agentCall({ description: "make file", prompt: "p" }), ctx);
    expect(asked.map((r) => [r.tool, r.agent])).toEqual([["write_file", "general-purpose · make file"]]);
    expect(existsSync(join(root, "made.txt"))).toBe(true);
  });

  test("in yolo mode, a shared-folder subagent's edits run without asking", async () => {
    const provider = new ScriptedProvider([useTool("w1", "write_file", { path: "made.txt", content: "hi\n" }), say("Made it.")]);
    const { host } = makeHost(provider);
    const { ctx, asked } = ctxWith(host);
    await runTool(agentCall({ description: "make file", prompt: "p" }), { ...ctx, yolo: true });
    expect(asked).toHaveLength(0);
    expect(existsSync(join(root, "made.txt"))).toBe(true);
  });

  test("in yolo mode, what isn't safe still asks, labeled with who asks", async () => {
    const provider = new ScriptedProvider([useTool("b1", "bash", { command: "curl x", network: true }), say("never")]);
    const { host } = makeHost(provider);
    const { ctx, asked } = ctxWith(host, () => "no");
    await runTool(agentCall({ description: "fetch", prompt: "p" }), { ...ctx, yolo: true });
    expect(asked.map((r) => [r.tool, r.agent])).toEqual([["bash", "general-purpose · fetch"]]);
  });

  test("a no inside the subagent stops it and the parent", async () => {
    const provider = new ScriptedProvider([
      [{ type: "text_delta", text: "I'll write made.txt now." }, ...useTool("w1", "write_file", { path: "made.txt", content: "hi\n" })],
      say("never"),
    ]);
    const { host } = makeHost(provider);
    const result = await runTool(agentCall({ description: "x", prompt: "p" }), ctxWith(host, () => "no").ctx);
    expect(result.declined).toBe(true);
    expect(provider.requests).toHaveLength(1);
    // The parent must learn the write didn't happen, not just read "I'll write made.txt now."
    expect(result.output).toBe(
      "I'll write made.txt now.\n\n[Marv: The user declined its write_file made.txt, so it stopped there. Don't retry: wait for the user to say how to proceed.]",
    );
  });

  test("stopped by the user (Esc declines what's waiting and aborts), it's reported as interrupted, not declined", async () => {
    const controller = new AbortController();
    const provider = new ScriptedProvider([useTool("w1", "write_file", { path: "made.txt", content: "hi\n" }), say("never")]);
    const { host } = makeHost(provider);
    const { ctx } = ctxWith(host, () => (controller.abort(), "no"));
    const result = await runTool(agentCall({ description: "x", prompt: "p" }), { ...ctx, signal: controller.signal });
    expect(result.declined).toBeUndefined();
    expect(result.isError).toBe(true);
    expect(result.summary).toStartWith("stopped ·");
    expect(result.output).toContain("[Marv: Interrupted by the user before it finished.]");
    expect(existsSync(join(root, "made.txt"))).toBe(false);
  });

  describe("in a worktree", () => {
    beforeEach(() => {
      git(root, "init", "-q", "-b", "main");
      git(root, "config", "user.email", "test@example.com");
      git(root, "config", "user.name", "Test");
      git(root, "add", "-A");
      git(root, "commit", "-q", "-m", "first");
    });

    test("approve once to start it; its edits land on its own branch", async () => {
      const provider = new ScriptedProvider([useTool("w1", "write_file", { path: "new.txt", content: "new\n" }), say("Added new.txt.")]);
      const { host } = makeHost(provider);
      const { ctx, asked } = ctxWith(host);
      const result = await runTool(agentCall({ description: "add file", prompt: "p", isolation: "worktree" }), ctx);

      // The dispatch is approved; inside the worktree, edits run without asking (when the sandbox works).
      expect(asked[0]!.tool).toBe("agent");
      expect(asked[0]!.scope.key).toBe("agent:worktree");
      expect(asked).toHaveLength(sandboxAvailable() ? 1 : 2);
      expect(result.output).toStartWith("Added new.txt.");
      const branch = /\n\nMarv: Branch (marv\/add-file-[0-9a-f]{4}): 1 commit/.exec(result.output)?.[1];
      expect(branch).toBeDefined();
      expect(git(root, "show", `${branch}:new.txt`)).toBe("new");
      expect(existsSync(join(root, "new.txt"))).toBe(false);
      expect(provider.requests[0]!.options.system).toContain(`branch ${branch}`);
    });

    test.if(sandboxAvailable())("in yolo mode, starting it doesn't ask either", async () => {
      const provider = new ScriptedProvider([useTool("w1", "write_file", { path: "new.txt", content: "new\n" }), say("Added.")]);
      const { host } = makeHost(provider);
      const { ctx, asked } = ctxWith(host);
      const result = await runTool(agentCall({ description: "add file", prompt: "p", isolation: "worktree" }), { ...ctx, yolo: true });
      expect(asked).toHaveLength(0);
      expect(result.output).toContain("1 commit");
    });

    test("in yolo mode without the sandbox, starting it still asks", async () => {
      const provider = new ScriptedProvider([say("Nothing to do.")]);
      const { host } = makeHost(provider);
      const { ctx, asked } = ctxWith(host);
      await runTool(agentCall({ description: "noop", prompt: "p", isolation: "worktree" }), { ...ctx, yolo: true, sandbox: false });
      expect(asked.map((r) => r.tool)).toEqual(["agent"]);
    });

    test("started from a subfolder, it works in that subfolder of its worktree", async () => {
      await mkdir(join(root, "pkg"));
      await writeFile(join(root, "pkg", "p.txt"), "p\n");
      await writeFile(join(root, "top.txt"), "top\n");
      git(root, "add", "-A");
      git(root, "commit", "-q", "-m", "pkg");
      const provider = new ScriptedProvider([
        [
          { type: "tool_call", call: { id: "r1", name: "read_file", arguments: JSON.stringify({ path: "../top.txt" }) } },
          { type: "tool_call", call: { id: "g1", name: "glob", arguments: JSON.stringify({ pattern: "*" }) } },
          { type: "tool_call", call: { id: "w1", name: "write_file", arguments: JSON.stringify({ path: "made.txt", content: "m\n" }) } },
          ...(sandboxAvailable() ? [{ type: "tool_call" as const, call: { id: "b1", name: "bash", arguments: JSON.stringify({ command: "git status --porcelain" }) } }] : []),
          { type: "done", reason: "tool_calls" },
        ],
        say("Made it."),
      ]);
      const { host } = makeHost(provider);
      const { ctx } = ctxWith(host);
      const result = await runTool(agentCall({ description: "sub", prompt: "p", isolation: "worktree" }), { ...ctx, root: join(root, "pkg") });
      const results = provider.requests[1]!.history.filter((t) => t.role === "tool").map((t) => (t.role === "tool" ? t.text : ""));
      expect(results[0]).toContain("outside the project");
      expect(results[1]).toBe("p.txt"); // listed relative to pkg, nothing from the top
      // In the sandbox git works, and the rest of the worktree is there (read-only), not listed as deleted.
      if (sandboxAvailable()) expect(results[3]).toBe("?? pkg/made.txt\n\n[exit code 0]");
      expect(provider.requests[0]!.options.system).toMatch(new RegExp(`${trees.replace(/^.*\//, "")}/sub-[0-9a-f]{4}/pkg`));
      const branch = /Marv: Branch (marv\/sub-[0-9a-f]{4}): 1 commit/.exec(result.output)?.[1];
      expect(branch).toBeDefined();
      expect(git(root, "show", `${branch}:pkg/made.txt`)).toBe("m");
      expect(git(root, "show", `${branch}:top.txt`)).toBe("top");
    });

    test("network commands still ask", async () => {
      const provider = new ScriptedProvider([useTool("b1", "bash", { command: "bun install", network: true }), say("never")]);
      const { host } = makeHost(provider);
      const { ctx, asked } = ctxWith(host, (r) => (r.tool === "agent" ? "yes" : "no"));
      const result = await runTool(agentCall({ description: "install", prompt: "p", isolation: "worktree" }), ctx);
      expect(asked.map((r) => [r.tool, r.network, r.agent])).toEqual([
        ["agent", undefined, undefined],
        ["bash", true, "general-purpose · install"],
      ]);
      expect(result.declined).toBe(true);
    });

    test("a provider that throws still finishes the worktree and reports its branch", async () => {
      const throwing: Provider = {
        name: "throwing",
        async *stream() {
          throw new Error("connection reset");
        },
      };
      const { host } = makeHost(throwing);
      const result = await runTool(agentCall({ description: "boom", prompt: "p", isolation: "worktree" }), ctxWith(host).ctx);
      expect(result.isError).toBe(true);
      expect(result.output).toContain("connection reset");
      expect(result.output).toMatch(/Marv: No changes \(branch marv\/boom-[0-9a-f]{4} removed\)/);
      expect(git(root, "worktree", "list").split("\n")).toHaveLength(1);
    });

    test("with the sandbox off, its edits ask the user, labeled with who asks", async () => {
      const provider = new ScriptedProvider([useTool("w1", "write_file", { path: "new.txt", content: "new\n" }), say("Added.")]);
      const { host } = makeHost(provider);
      const { ctx, asked } = ctxWith(host);
      await runTool(agentCall({ description: "x", prompt: "p", isolation: "worktree" }), { ...ctx, sandbox: false });
      expect(asked.map((r) => [r.tool, r.agent])).toEqual([
        ["agent", undefined],
        ["write_file", "general-purpose · x"],
      ]);
    });

    test("an interrupt mid-run commits what it did, marked interrupted", async () => {
      const controller = new AbortController();
      let step = 0;
      const provider: Provider = {
        name: "interrupted",
        async *stream() {
          step++;
          if (step === 1) {
            yield* useTool("w1", "write_file", { path: "half.txt", content: "half\n" });
            return;
          }
          yield { type: "text_delta", text: "Halfway there." };
          controller.abort();
          yield { type: "done" };
        },
      };
      const { host } = makeHost(provider);
      const { ctx } = ctxWith(host);
      const result = await runTool(agentCall({ description: "halt", prompt: "p", isolation: "worktree" }), { ...ctx, signal: controller.signal });
      expect(result.isError).toBe(true);
      expect(result.output).toStartWith("Halfway there.");
      expect(result.output).toContain("[Marv: Interrupted by the user before it finished.]");
      const branch = /\n\nMarv: Branch (marv\/halt-[0-9a-f]{4}): 1 commit/.exec(result.output)?.[1];
      expect(branch).toBeDefined();
      expect(git(root, "log", "-1", "--format=%s", branch!)).toBe("marv: halt (interrupted)");
      expect(git(root, "show", `${branch}:half.txt`)).toBe("half");
    });

    test("if finishing the worktree throws, the report says where it is", async () => {
      const finish = spyOn(worktreeModule, "finishWorktree").mockImplementation(() => {
        throw new Error("disk on fire");
      });
      try {
        const { host } = makeHost(new ScriptedProvider([say("Done.")]));
        const result = await runTool(agentCall({ description: "fire", prompt: "p", isolation: "worktree" }), ctxWith(host).ctx);
        expect(result.output).toMatch(new RegExp(`^Done\\.\\n\\nMarv: couldn't finish the worktree \\(disk on fire\\); it's at ${trees}/fire-[0-9a-f]{4}\\.$`));
      } finally {
        finish.mockRestore();
      }
    });

    test.skipIf(!sandboxAvailable())("in the sandbox, git status works but the shared .git is read-only", async () => {
      const gitFile = join(root, ".git", "planted");
      const provider = new ScriptedProvider([
        [
          { type: "tool_call", call: { id: "b1", name: "bash", arguments: JSON.stringify({ command: "git status" }) } },
          { type: "tool_call", call: { id: "b2", name: "bash", arguments: JSON.stringify({ command: `touch ${gitFile}` }) } },
          { type: "done", reason: "tool_calls" },
        ],
        say("Checked."),
      ]);
      const { host } = makeHost(provider);
      await runTool(agentCall({ description: "probe", prompt: "p", isolation: "worktree" }), ctxWith(host).ctx);
      const results = provider.requests[1]!.history.filter((t) => t.role === "tool").map((t) => (t.role === "tool" ? t.text : ""));
      expect(results[0]).toMatch(/On branch marv\/probe-[0-9a-f]{4}[\s\S]*\[exit code 0\]$/);
      expect(results[1]).toMatch(/Read-only file system[\s\S]*\[exit code 1\]$/);
      expect(existsSync(gitFile)).toBe(false);
    });

    test("without a worktrees folder, a worktree dispatch fails before asking", async () => {
      const { host } = makeHost(new ScriptedProvider([]), { worktreesDir: undefined });
      const { ctx, asked } = ctxWith(host);
      const result = await runTool(agentCall({ description: "x", prompt: "p", isolation: "worktree" }), ctx);
      expect(result.output).toContain("Worktrees aren't available here.");
      expect(asked).toHaveLength(0);
    });

    test("the approval warns about uncommitted changes", async () => {
      await writeFile(join(root, "dirty.txt"), "x\n");
      const { host } = makeHost(new ScriptedProvider([say("ok")]));
      const { ctx, asked } = ctxWith(host);
      await runTool(agentCall({ description: "x", prompt: "p", isolation: "worktree" }), ctx);
      expect(asked[0]!.preview.warning).toContain("1 uncommitted change in the project won't be in the worktree");
    });

    test("the approval says what will run without asking, and that network commands still ask", async () => {
      const { host } = makeHost(new ScriptedProvider([say("ok"), say("ok")]));
      const { ctx, asked } = ctxWith(host);
      await runTool(agentCall({ description: "x", prompt: "p", isolation: "worktree" }), ctx);
      await runTool(agentCall({ description: "y", prompt: "p", isolation: "worktree" }), { ...ctx, sandbox: false });
      if (sandboxAvailable()) {
        expect(asked[0]!.preview.note).toEndWith("edits and sandboxed commands run without asking inside its worktree; network commands still ask");
      }
      expect(asked[1]!.preview.note).toEndWith("asks before each change (no sandbox)");
    });

    test("from a subfolder, the approval says where it works, and that the count is the whole repository's", async () => {
      await mkdir(join(root, "pkg"));
      await writeFile(join(root, "dirty.txt"), "x\n");
      const { host } = makeHost(new ScriptedProvider([say("ok")]));
      const { ctx, asked } = ctxWith(host);
      await runTool(agentCall({ description: "x", prompt: "p", isolation: "worktree" }), { ...ctx, root: join(root, "pkg") });
      expect(asked[0]!.preview.note).toContain(", working in pkg/");
      expect(asked[0]!.preview.warning).toContain("1 uncommitted change in the repository won't be in the worktree");
    });
  });

  test("a description is short", async () => {
    const { host } = makeHost(new ScriptedProvider([]));
    const result = await runTool(agentCall({ description: "x".repeat(121), prompt: "p" }), ctxWith(host).ctx);
    expect(result.output).toStartWith("Invalid input for agent");
  });

  describe("a subagent's tool context", () => {
    const wt: Worktree = {
      dir: "/trees/x-1a2b",
      branch: "marv/x-1a2b",
      base: "abc1234",
      repo: "/proj",
      gitDir: "/proj/.git",
      adminDir: "/proj/.git/worktrees/x-1a2b",
      prefix: "",
      workDir: "/trees/x-1a2b",
    };
    const parent = (host: AgentHost): ToolContext => ({
      root: "/proj",
      agentHost: host,
      memory: { personal: "/m/p.md", project: "/m/q.md" } satisfies MemoryPaths,
      callId: "a1",
      sandbox: false,
      approve: async () => "no",
    });

    test("in a worktree: its folder, the shared .git read-only, git pinned, no host or memory", async () => {
      const { host } = makeHost(new ScriptedProvider([]));
      const sub = subagentContext(parent(host), "general-purpose · x", wt);
      expect(sub.root).toBe(wt.dir);
      expect(sub.readOnly).toEqual([wt.gitDir]);
      expect(sub.gitEnv).toEqual(worktreeEnv(wt));
      expect(sub.agentHost).toBeUndefined();
      expect(sub.memory).toBeUndefined();
      expect(sub.callId).toBeUndefined();
      expect(sub.sandbox).toBe(false);
    });

    test("in a worktree started from a subfolder: that subfolder, with the rest of the worktree read-only", async () => {
      const { host } = makeHost(new ScriptedProvider([]));
      const sub = subagentContext({ ...parent(host), root: "/proj/pkg" }, "general-purpose · x", { ...wt, repo: "/proj/pkg", prefix: "pkg/", workDir: "/trees/x-1a2b/pkg" });
      expect(sub.root).toBe("/trees/x-1a2b/pkg");
      expect(sub.readOnly).toEqual([wt.dir, wt.gitDir]);
      expect(sub.gitEnv).toEqual(worktreeEnv(wt)); // git still sees the whole worktree
    });

    test("in the shared folder: the project, nothing extra", async () => {
      const { host } = makeHost(new ScriptedProvider([]));
      const sub = subagentContext(parent(host), "general-purpose · x");
      expect(sub.root).toBe("/proj");
      expect(sub.readOnly).toBeUndefined();
      expect(sub.gitEnv).toBeUndefined();
      expect(sub.agentHost).toBeUndefined();
      expect(sub.memory).toBeUndefined();
    });

    test("with the sandbox off, even a worktree's requests go to the user", async () => {
      const { host } = makeHost(new ScriptedProvider([]));
      const asked: ApprovalRequest[] = [];
      const sub = subagentContext({ ...parent(host), approve: async (r) => (asked.push(r), "no") }, "general-purpose · x", wt);
      const request = { tool: "write_file", label: "a.txt", preview: { title: "Write a.txt" }, scope: { key: "write_file", description: "" } };
      expect(await sub.approve!(request)).toBe("no");
      expect(asked).toEqual([{ ...request, agent: "general-purpose · x" }]);
    });
  });

  test("outside a git repo, a worktree dispatch fails before asking", async () => {
    const { host } = makeHost(new ScriptedProvider([]));
    const { ctx, asked } = ctxWith(host);
    const result = await runTool(agentCall({ description: "x", prompt: "p", isolation: "worktree" }), ctx);
    expect(result.output).toBe(NOT_A_REPO);
    expect(asked).toHaveLength(0);
  });

  test("two subagents in one reply run at the same time", async () => {
    let active = 0;
    let max = 0;
    const slow = (text: string): Provider => ({
      name: "slow",
      async *stream(_h: ChatTurn[], _o?: StreamOptions) {
        active++;
        max = Math.max(max, active);
        await Bun.sleep(40);
        active--;
        yield { type: "text_delta", text } as AgentEvent;
        yield { type: "done" } as AgentEvent;
      },
    });
    const parent = new ScriptedProvider([
      [
        { type: "tool_call", call: agentCall({ description: "a", prompt: "alpha task" }, "a1") },
        { type: "tool_call", call: agentCall({ description: "b", prompt: "beta task" }, "a2") },
        { type: "done" },
      ],
      say("Both reported."),
    ]);
    const provider = new RoutedProvider({ "start two": parent, "alpha task": slow("A done"), "beta task": slow("B done") });
    const { host } = makeHost(provider);
    const history: ChatTurn[] = [{ role: "user", text: "start two" }];
    for await (const _ of runAgent({
      provider,
      history,
      system: "S",
      tools: [],
      runTool: (c) => runTool(c, { root, agentHost: host }),
      signal: new AbortController().signal,
      isParallel: isParallelCall,
    }));
    expect(max).toBe(2);
    expect(history.filter((t) => t.role === "tool").map((t) => t.role === "tool" && t.text)).toEqual(["A done", "B done"]);
  });
});
