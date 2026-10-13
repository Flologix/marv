// Guard suite (#47): the github tool runs gh outside the sandbox with the user's login, so what it can never do must
// never reach gh, whatever the approver says: every form of auth, config, extensions, aliases (an unknown word),
// gh api's --input, --web, another host. Writes, other API methods, graphql and sensitive reads ask (yolo or not);
// tokens in gh's output never reach the model, the transcript, the saved session or the trajectory; a subagent
// can't write. gh is a fake on PATH (tests/fixtures/fake-gh.ts) that logs every argv: an empty log means nothing
// ran. The controls (reads, an approved write) show the log does fill when a call is allowed. Each guard was
// checked to fail with its line removed (see the PR).
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { readdir, readFile, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { HeadlessResult } from "../../src/headless.ts";
import type { AgentEvent, ToolCall } from "../../src/provider/types.ts";
import { createSession, type ApprovalRequest, type Decision, type SessionOptions, type ToolResult } from "../../src/sdk.ts";
import { ScriptedProvider } from "../fake-provider.ts";
import { fakeGhCalls, installFakeGh } from "../fixtures/fake-gh.ts";

const TOKEN = `ghp_${"Z".repeat(36)}`;

let bin: string;
let root: string;
let configDir: string;
let log: string;
let server: ReturnType<typeof Bun.serve> | undefined;
const saved: Record<string, string | undefined> = {};
const ENV = ["PATH", "FAKE_GH_LOG", "FAKE_GH_LOGGED_IN", "FAKE_GH_OUTPUT"];
beforeEach(async () => {
  for (const name of ENV) saved[name] = process.env[name];
  bin = await installFakeGh();
  root = await mkdtemp(join(tmpdir(), "marv-guard-gh-"));
  configDir = await mkdtemp(join(tmpdir(), "marv-guard-gh-config-"));
  log = join(bin, "calls.jsonl");
  Bun.spawnSync(["git", "init", "-q"], { cwd: root });
  Bun.spawnSync(["git", "remote", "add", "origin", "https://github.com/o/r.git"], { cwd: root });
  process.env.PATH = `${bin}:${saved.PATH}`;
  process.env.FAKE_GH_LOG = log;
  process.env.FAKE_GH_LOGGED_IN = "1";
});
afterEach(async () => {
  server?.stop(true);
  server = undefined;
  for (const name of ENV) {
    if (saved[name] === undefined) delete process.env[name];
    else process.env[name] = saved[name];
  }
  for (const dir of [bin, root, configDir]) await rm(dir, { recursive: true, force: true });
});

const toolCall = (id: string, name: string, args: Record<string, unknown>): ToolCall => ({ id, name, arguments: JSON.stringify(args) });
const gh = (id: string, args: string[]) => toolCall(id, "github", { args });
const useTools = (...calls: ToolCall[]): AgentEvent[] => [...calls.map((call) => ({ type: "tool_call" as const, call })), { type: "done", reason: "tool_calls" }];
const say = (text: string): AgentEvent[] => [{ type: "text_delta", text }, { type: "done", reason: "stop" }];
/** gh's calls that weren't Marv's own availability check. */
const ghRuns = async () => (await fakeGhCalls(log)).filter((c) => !(c.args[0] === "auth" && c.args[1] === "status" && c.args[2] === "--active" && c.args.length === 3)).map((c) => c.args);

/** One SDK turn (yolo on, the default) with a scripted model; returns each call's result and every approval request. */
async function turn(script: AgentEvent[][], decide: Decision, options: Partial<SessionOptions> = {}) {
  const asked: ApprovalRequest[] = [];
  const session = await createSession({ cwd: root, provider: new ScriptedProvider(script), configDir, github: true, approve: async (r) => (asked.push(r), decide), ...options });
  try {
    const events = await Array.fromAsync(session.send("check the project"));
    return { asked, events, results: events.flatMap((e): ToolResult[] => (e.type === "tool_end" ? [e.result] : [])) };
  } finally {
    await session.close();
  }
}

const NEVER: string[][] = [
  ["auth", "token"],
  ["auth", "status", "--show-token"],
  ["auth", "status", "--show-token", "--hostname", "github.com"],
  ["-h", "auth", "token"],
  ["--hostname", "x", "auth", "token"],
  ["auth", "login", "--with-token"],
  ["config", "get", "oauth_token"],
  ["extension", "exec", "evil"],
  ["myalias"],
  ["myalias", "run"],
  ["alias", "set", "co", "pr checkout"],
  ["secret", "list"],
  ["api", "--input", "/etc/passwd", "repos/o/r/issues"],
  ["api", "--verbose", "user"],
  ["pr", "view", "1", "--web"],
  ["pr", "view", "1", "-w"],
  ["pr", "list", "-R", "evil.example/o/r"],
  ["api", "--hostname", "evil.example", "user"],
  ["pr", "view", "https://evil.example/o/r/pull/1"],
  ["pr", "checkout", "1"],
  ["repo", "clone", "o/r"],
  ["run", "download", "1"],
];

describe("guard: the github tool", () => {
  test("what can never run never reaches gh, even with an approver that says yes to everything, and isn't asked", async () => {
    const calls = NEVER.map((args, i) => gh(`n${i}`, args));
    const { asked, results } = await turn([useTools(...calls), say("Done.")], "yes");
    expect(results).toHaveLength(NEVER.length);
    for (const result of results) expect(result.isError).toBe(true);
    // Each refused for its own rule, said so the model learns it (not only by the catch-all for unknown words).
    const reason = (args: string[]) => results[NEVER.findIndex((a) => a.join(" ") === args.join(" "))]!.output;
    expect(reason(["auth", "token"])).toContain("Marv keeps the token away from the conversation");
    expect(reason(["config", "get", "oauth_token"])).toContain("gh config … is never run");
    expect(reason(["extension", "exec", "evil"])).toContain("an extension is a program");
    expect(reason(["-h", "auth", "token"])).toContain("Put the command first");
    expect(reason(["--hostname", "x", "auth", "token"])).toContain("Put the command first");
    expect(reason(["pr", "list", "-R", "evil.example/o/r"])).toContain("only reaches the project's own GitHub host");
    expect(asked).toEqual([]);
    expect(await ghRuns()).toEqual([]);
  });

  test("writes, other API methods, fields, graphql and sensitive reads ask in yolo; with a no, nothing runs", async () => {
    const asks: string[][] = [
      ["pr", "comment", "1", "--body", "hi"],
      ["pr", "merge", "1"],
      ["issue", "close", "2"],
      ["api", "-X", "POST", "repos/o/r/issues"],
      ["api", "repos/o/r/issues", "-f", "title=x"],
      ["api", "graphql", "-f", "query=mutation{x}"],
      ["api", "repos/o/r/actions/secrets"],
      ["api", "repos/o/r/hooks"],
    ];
    const { asked } = await turn([useTools(...asks.map((args, i) => gh(`w${i}`, args))), say("Done.")], "no");
    // A "no" stops the run at the first: it's asked, and the rest are "not run".
    expect(asked).toHaveLength(1);
    expect(asked[0]).toMatchObject({ tool: "github", door: "github", scope: { key: "github:pr comment" } });
    for (const [i, args] of asks.entries()) {
      const one = await turn([useTools(gh(`a${i}`, args)), say("Done.")], "no");
      expect(one.asked.map((r) => r.door)).toEqual(["github"]);
    }
    // gh ran only for the repository name the merge and close approvals show.
    expect((await ghRuns()).filter((args) => args[0] !== "repo")).toEqual([]);
  }, 20_000);

  test("control: reads run without asking in yolo, and an approved write runs", async () => {
    const reads = await turn([useTools(gh("r1", ["pr", "list"]), gh("r2", ["run", "view", "5", "--log-failed"]), gh("r3", ["api", "repos/o/r/pulls"])), say("Done.")], "no");
    expect(reads.asked).toEqual([]);
    const write = await turn([useTools(gh("w1", ["pr", "comment", "1", "--body", "hi"])), say("Done.")], "yes");
    expect(write.asked).toHaveLength(1);
    expect(await ghRuns()).toEqual([["pr", "list"], ["run", "view", "5", "--log-failed"], ["api", "repos/o/r/pulls"], ["pr", "comment", "1", "--body", "hi"]]);
  });

  test("a token in gh's output never reaches the model, the transcript, the saved session or the trajectory", async () => {
    process.env.FAKE_GH_OUTPUT = `issue body: here's my token ${TOKEN}`;
    const provider = new ScriptedProvider([useTools(gh("r1", ["issue", "view", "1"])), say("Done.")]);
    const session = await createSession({ cwd: root, provider, configDir, github: true, persist: true, trajectories: true });
    let events: unknown[];
    try {
      events = await Array.fromAsync(session.send("read issue 1"));
    } finally {
      await session.close();
    }
    const end = events.find((e): e is { type: "tool_end"; result: ToolResult } => (e as { type: string }).type === "tool_end");
    expect(end?.result.output).toContain("issue body: here's my token [redacted token]");
    expect(JSON.stringify(events)).not.toContain(TOKEN);
    expect(JSON.stringify(provider.requests)).not.toContain(TOKEN);
    expect(JSON.stringify(provider.requests)).toContain("[redacted token]");
    const files = (await readdir(configDir, { recursive: true })).filter((f) => /\.(json|jsonl)$/.test(f));
    expect(files.some((f) => f.endsWith(".jsonl"))).toBe(true); // the trajectory
    expect(files.some((f) => f.startsWith("sessions"))).toBe(true); // the saved session
    for (const file of files) {
      const text = await readFile(join(configDir, file), "utf8");
      expect(text).not.toContain(TOKEN);
    }
    // The control: gh really printed it.
    expect(await ghRuns()).toEqual([["issue", "view", "1"]]);
  });

  test("a subagent reads, but its write is refused, even with the session's approver saying yes", async () => {
    const { asked, results } = await turn(
      [
        useTools(toolCall("a1", "agent", { description: "review", prompt: "comment on the PR" })),
        useTools(gh("s1", ["pr", "view", "1"]), gh("s2", ["pr", "comment", "1", "--body", "LGTM"])),
        say("Tried."),
        say("Done."),
      ],
      "yes",
    );
    expect(results[0]?.output).toContain("Tried.");
    expect(asked).toEqual([]);
    expect(await ghRuns()).toEqual([["pr", "view", "1"]]);
  });

  test("not offered where gh is logged out: the model can't call it, nor can its subagents", async () => {
    process.env.FAKE_GH_LOGGED_IN = "0";
    const { results } = await turn([useTools(gh("r1", ["pr", "list"])), say("Done.")], "yes");
    expect(results[0]?.output).toContain('Unknown tool "github"');
    await turn([useTools(toolCall("a1", "agent", { description: "look", prompt: "list PRs" })), useTools(gh("s1", ["pr", "list"])), say("Tried."), say("Done.")], "yes");
    expect(await ghRuns()).toEqual([]);
  });
});

/** A stand-in OpenRouter for the real CLI: one github call, then a plain answer. */
function serveGithubCall(args: string[]): string {
  const sse = (...events: unknown[]) =>
    new Response([...events].map((e) => `data: ${JSON.stringify(e)}\n\n`).concat("data: [DONE]\n\n").join(""), { headers: { "content-type": "text/event-stream" } });
  const finish = (reason: string) => ({ choices: [{ delta: {}, finish_reason: reason }] });
  const replies = [
    [{ choices: [{ delta: { tool_calls: [{ index: 0, id: "g1", type: "function", function: { name: "github", arguments: JSON.stringify({ args }) } }] } }] }, finish("tool_calls")],
    [{ choices: [{ delta: { content: "Done." } }] }, finish("stop")],
  ];
  let n = 0;
  server = Bun.serve({
    port: 0,
    fetch: (req) => (new URL(req.url).pathname.endsWith("/chat/completions") ? sse(...replies[Math.min(n++, replies.length - 1)]!) : Response.json({ data: [] })),
  });
  return `http://localhost:${server.port}/v1`;
}

async function headless(args: string[], approve: string[]) {
  const { writeFile } = await import("node:fs/promises");
  server?.stop(true);
  await writeFile(join(configDir, "config.json"), JSON.stringify({ provider: "openrouter", model: "test-model", apiKey: "sk-test", baseUrl: serveGithubCall(args) }));
  // The real CLI, with the fake gh on PATH and the github tool wanted (tests turn it off by default: tests/setup.ts).
  const { MARV_GITHUB: _, ...env } = process.env;
  const proc = Bun.spawn([process.execPath, join(import.meta.dir, "..", "..", "src", "cli.tsx"), "-p", "do it", "--output", "json", ...approve.flatMap((n) => ["--approve", n])], {
    env: { ...env, MARV_CONFIG_DIR: configDir, NO_COLOR: "1" },
    cwd: root,
    stdout: "pipe",
    stderr: "pipe",
  });
  const [stdout, stderr] = await Promise.all([new Response(proc.stdout).text(), new Response(proc.stderr).text()]);
  const exitCode = await proc.exited;
  try {
    return { exitCode, result: JSON.parse(stdout) as HeadlessResult };
  } catch {
    throw new Error(`marv -p printed no JSON (exit ${exitCode}):\n${stdout}\n${stderr}`);
  }
}

describe("guard: marv -p and the github tool", () => {
  test("a write is refused (exit 3) without --approve github, whatever else is approved, and nothing runs", async () => {
    const { exitCode, result } = await headless(["pr", "comment", "1", "--body", "x"], ["git_write", "network", "web", "mcp", "files", "memory", "agent_worktree"]);
    expect(exitCode).toBe(3);
    expect(result.refused?.map((r) => r.tool)).toEqual(["github"]);
    expect(await ghRuns()).toEqual([]);
  }, 20_000);

  test("auth is refused even with --approve github", async () => {
    const { result } = await headless(["auth", "token"], ["github"]);
    expect(result.reply).toBe("Done.");
    expect(await ghRuns()).toEqual([]);
  }, 20_000);

  test("control: reads run with nothing approved, and a write with --approve github", async () => {
    const read = await headless(["pr", "list"], []);
    expect(read.exitCode).toBe(0);
    const write = await headless(["pr", "comment", "1", "--body", "x"], ["github"]);
    expect(write.exitCode).toBe(0);
    expect(await ghRuns()).toEqual([["pr", "list"], ["pr", "comment", "1", "--body", "x"]]);
  }, 30_000);
});
