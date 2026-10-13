import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { mkdtemp, rm } from "node:fs/promises";
import { realpathSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { CheckpointStore } from "../src/checkpoints/store.ts";
import type { ProviderFactory } from "../src/provider/factory.ts";
import type { AgentEvent, ChatTurn, Provider, StreamOptions, ToolCall } from "../src/provider/types.ts";
import { MarvSession, type SessionEvent, type SessionInit } from "../src/session.ts";
import { SessionStore } from "../src/sessions.ts";
import { TrajectoryStore } from "../src/trajectory.ts";
import { RoutedProvider, ScriptedProvider } from "./fake-provider.ts";

const say = (text: string): AgentEvent[] => [
  { type: "text_delta", text },
  { type: "done", reason: "stop" },
];
const useTools = (...calls: ToolCall[]): AgentEvent[] => [...calls.map((c) => ({ type: "tool_call" as const, call: c })), { type: "done", reason: "tool_calls" }];
const call = (id: string, name: string, args: Record<string, unknown>): ToolCall => ({ id, name, arguments: JSON.stringify(args) });
const fixed = (provider: Provider): ProviderFactory => ({ id: "test", model: "test-model", make: () => provider });
const git = (cwd: string, ...args: string[]) => Bun.spawnSync(["git", ...args], { cwd }).stdout.toString().trim();

async function collect(events: AsyncIterable<SessionEvent>): Promise<SessionEvent[]> {
  const out: SessionEvent[] = [];
  for await (const event of events) out.push(event);
  return out;
}

let project: string;
let dir: string;
beforeEach(async () => {
  project = realpathSync(await mkdtemp(join(tmpdir(), "marv-cps-project-")));
  dir = await mkdtemp(join(tmpdir(), "marv-cps-"));
  git(project, "init", "-q", "-b", "main");
  writeFileSync(join(project, "a.txt"), "one\n");
  git(project, "add", "-A");
  git(project, "-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "first");
});
afterEach(async () => {
  await rm(project, { recursive: true, force: true });
  await rm(dir, { recursive: true, force: true });
});

function makeSession(provider: Provider, init: Partial<SessionInit> = {}): MarvSession {
  return new MarvSession({
    root: project,
    provider: fixed(provider),
    sandbox: false,
    approve: async () => "yes",
    diagnostics: false,
    checks: false,
    ...init,
  });
}
const refs = () => git(project, "for-each-ref", "--format=%(refname)", "refs/marv/");
const write = (id: string, path: string, content: string) => call(id, "write_file", { path, content });
const bash = (id: string, command: string) => call(id, "bash", { command });

describe("a turn's checkpoints", () => {
  test("a read-only turn takes none", async () => {
    const session = makeSession(new ScriptedProvider([useTools(call("r", "read_file", { path: "a.txt" })), say("read")]));
    await collect(session.send("look"));
    expect(refs()).toBe("");
  });

  test("write_file and a bash edit: one pair, covering both", async () => {
    const session = makeSession(
      new ScriptedProvider([useTools(write("w", "new.txt", "new\n")), useTools(bash("b", "echo two > a.txt")), say("done")]),
    );
    await collect(session.send("change things"));
    const chain = git(project, "log", "--format=%s", `refs/marv/checkpoints/${session.id}`).split("\n");
    expect(chain).toEqual(["marv checkpoint: turn 1 after", "marv checkpoint: turn 1 before"]);
    const plan = await session.undoPlan();
    if ("reason" in plan) throw new Error(plan.reason);
    expect(plan.files.map((f) => [f.path, f.action]).sort()).toEqual([
      ["a.txt", "restore"],
      ["new.txt", "delete"],
    ]);
    expect(plan.label).toBe("change things");
  });

  test("a no-op bash turn's pair is dropped", async () => {
    const session = makeSession(new ScriptedProvider([useTools(bash("b", "ls")), say("listed")]));
    await collect(session.send("list"));
    expect(refs()).toBe("");
    expect(await session.undoPlan()).toMatchObject({ reason: expect.stringContaining("nothing to undo") });
  });

  test("the after is taken on Esc and at the step limit", async () => {
    // Esc: the model writes, then hangs until stopped.
    class WriteThenHang implements Provider {
      readonly name = "hang";
      n = 0;
      async *stream(_h: ChatTurn[], { signal }: StreamOptions = {}): AsyncIterable<AgentEvent> {
        if (this.n++ === 0) {
          yield* useTools(write("w", "x.txt", "x\n"));
          return;
        }
        await new Promise<void>((resolve) => signal?.addEventListener("abort", () => resolve(), { once: true }));
        yield { type: "done" };
      }
    }
    const session = makeSession(new WriteThenHang());
    const events = session.send("go");
    let seen = 0;
    for await (const event of events) {
      if (event.type === "tool_end" && ++seen === 1) setTimeout(() => session.interrupt(), 20);
    }
    expect(git(project, "log", "-1", "--format=%s", `refs/marv/checkpoints/${session.id}`)).toBe("marv checkpoint: turn 1 after");

    const limited = makeSession(new ScriptedProvider([useTools(write("w", "y.txt", "y\n")), useTools(write("w2", "z.txt", "z\n"))]), {
      approve: undefined,
      yolo: true,
      maxSteps: 1,
    });
    await collect(limited.send("go"));
    expect(git(project, "log", "-1", "--format=%s", `refs/marv/checkpoints/${limited.id}`)).toBe("marv checkpoint: turn 1 after");
  });

  test("undo restores; the next message tells the model; every request extends the previous one", async () => {
    const provider = new ScriptedProvider([useTools(write("w", "a.txt", "changed\n")), say("changed it"), say("ok")]);
    const session = makeSession(provider);
    await collect(session.send("add the parser"));
    const plan = await session.undoPlan();
    if ("reason" in plan) throw new Error(plan.reason);
    const result = await session.undo(plan);
    expect(result.restored).toEqual(["a.txt"]);
    expect(readFileSync(join(project, "a.txt"), "utf8")).toBe("one\n");
    await collect(session.send("what now?"));
    const last = provider.requests.at(-1)!.history.at(-1)!;
    expect(last.role).toBe("user");
    expect(last.role === "user" && last.text).toStartWith('Marv: the user undid your changes from an earlier turn ("add the parser"): a.txt is back as it was before it.');
    expect(last.role === "user" && last.text).toEndWith("what now?");
    for (let i = 1; i < provider.requests.length; i++) {
      const previous = provider.requests[i - 1]!.history;
      expect(provider.requests[i]!.history.slice(0, previous.length)).toEqual(previous);
    }
    // Only once.
    await collect(session.send("and now?"));
    const again = provider.requests.at(-1)!.history.at(-1)!;
    expect(again.role === "user" && again.text).toBe("and now?");
  });

  test("undoPlan() and undo() refuse during a turn", async () => {
    class Hang implements Provider {
      readonly name = "hang";
      async *stream(_h: ChatTurn[], { signal }: StreamOptions = {}): AsyncIterable<AgentEvent> {
        await new Promise<void>((resolve) => signal?.addEventListener("abort", () => resolve(), { once: true }));
        yield { type: "done" };
      }
    }
    const session = makeSession(new Hang());
    const turn = collect(session.send("go"));
    await expect(session.undoPlan()).rejects.toThrow(/working on a turn/);
    session.interrupt();
    await turn;
  });

  test("checkpoints off (init or configure) takes none", async () => {
    const off = makeSession(new ScriptedProvider([useTools(write("w", "n.txt", "n\n")), say("ok")]), { checkpoints: false });
    await collect(off.send("go"));
    expect(refs()).toBe("");
    expect(await off.undoPlan()).toMatchObject({ reason: expect.stringContaining("off") });
    const on = makeSession(new ScriptedProvider([useTools(write("w", "m.txt", "m\n")), say("ok")]));
    on.configure({ checkpoints: false });
    await collect(on.send("go"));
    expect(refs()).toBe("");
  });

  test("clear() starts a new chain, resume() finds the old one", async () => {
    const sessions = new SessionStore(join(dir, "sessions"));
    const session = makeSession(
      new ScriptedProvider([useTools(write("w", "first.txt", "1\n")), say("ok"), useTools(write("w2", "second.txt", "2\n")), say("ok")]),
      { sessions },
    );
    await collect(session.send("first"));
    const old = session.id;
    await session.clear();
    expect(await session.undoPlan()).toMatchObject({ reason: expect.stringContaining("nothing to undo") });
    await collect(session.send("second"));
    const plan = await session.undoPlan();
    expect("reason" in plan ? "" : plan.label).toBe("second");
    await session.flush();
    await session.resume(old);
    const back = await session.undoPlan();
    expect("reason" in back ? back.reason : back.label).toBe("first");
    expect(refs().split("\n").length).toBe(2);
  });

  test("resume() into a chain that can't be read: new turns go on a chain of their own, never numbered over its turns", async () => {
    const sessions = new SessionStore(join(dir, "sessions"));
    const store = new CheckpointStore({ root: project, session: "x" });
    const session = makeSession(
      new ScriptedProvider([useTools(write("w", "first.txt", "1\n")), say("ok"), useTools(write("w2", "again.txt", "2\n")), say("ok")]),
      { sessions, checkpointStore: store },
    );
    await collect(session.send("first"));
    const old = session.id;
    await session.flush();
    await session.clear();
    const real = store.chain.bind(store);
    store.chain = async () => {
      throw new Error("the checkpoints can't be read (git timed out)");
    };
    await session.resume(old);
    store.chain = real;
    expect(session.checkpointProblem().failure).toContain("the turns before this resume can't be undone");
    await collect(session.send("again"));
    expect(store.ref).toStartWith(`refs/marv/checkpoints/${old}.`);
    const plan = await session.undoPlan();
    expect("reason" in plan ? plan.reason : [plan.turn, plan.label]).toEqual([1, "again"]);
    // The old chain is untouched: its turn 1 is still "first".
    expect(git(project, "log", "-1", "--format=%B", `refs/marv/checkpoints/${old}`)).toContain("first");
  });

  test("a failing snapshot doesn't stop the turn, and says so once", async () => {
    const store = new CheckpointStore({ root: project, session: "x" });
    store.snapshot = async () => ({ ok: false, reason: "failed", message: "disk full", ms: 1 });
    const session = makeSession(new ScriptedProvider([useTools(write("w", "a.txt", "two\n"), write("w2", "b.txt", "b\n")), say("done")]), {
      checkpointStore: store,
    });
    const events = await collect(session.send("go"));
    expect(readFileSync(join(project, "a.txt"), "utf8")).toBe("two\n");
    expect(existsSync(join(project, "b.txt"))).toBe(true);
    expect(events.filter((e) => e.type === "checkpoint")).toEqual([{ type: "checkpoint", status: "failed", message: "disk full" }]);
    expect(events.at(-1)).toMatchObject({ type: "turn_end", reason: "end" });
    expect(await session.undoPlan()).toMatchObject({ reason: expect.stringContaining("disk full") });
  });

  test("parallel shared-folder subagents share one before", async () => {
    const sub = (key: string, file: string) =>
      new ScriptedProvider([useTools(write(`${key}w`, file, `${key}\n`)), say(`${key} done`)]);
    const provider = new RoutedProvider({
      "do the work": new ScriptedProvider([
        useTools(
          call("a1", "agent", { description: "one", prompt: "task-one" }),
          call("a2", "agent", { description: "two", prompt: "task-two" }),
        ),
        say("both done"),
      ]),
      "task-one": sub("one", "one.txt"),
      "task-two": sub("two", "two.txt"),
    });
    const session = makeSession(provider);
    await collect(session.send("do the work"));
    const chain = git(project, "log", "--format=%s", `refs/marv/checkpoints/${session.id}`).split("\n");
    expect(chain).toEqual(["marv checkpoint: turn 1 after", "marv checkpoint: turn 1 before"]);
    const plan = await session.undoPlan();
    expect("reason" in plan ? [] : plan.files.map((f) => f.path).sort()).toEqual(["one.txt", "two.txt"]);
  });

  test("trajectories get checkpoint and undo records", async () => {
    const trajectories = new TrajectoryStore(join(dir, "trajectories"));
    const session = makeSession(new ScriptedProvider([useTools(write("w", "t.txt", "t\n")), say("ok")]), { trajectories });
    await collect(session.send("go"));
    const plan = await session.undoPlan();
    if ("reason" in plan) throw new Error(plan.reason);
    await session.undo(plan);
    await session.flush();
    const file = trajectories.open(project, session.id).path;
    const records = readFileSync(file, "utf8").trim().split("\n").map((line) => JSON.parse(line));
    expect(records.filter((r) => r.type === "checkpoint").map((r) => [r.kind, r.ok])).toEqual([
      ["before", true],
      ["after", true],
      ["pre-undo", true],
    ]);
    expect(records.find((r) => r.type === "undo")).toMatchObject({ turn: 1, paths: ["t.txt"], skipped: [] });
  });
});
