import { afterEach, beforeEach, describe, expect, spyOn, test } from "bun:test";
import { existsSync, readdirSync } from "node:fs";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { cleanup, render } from "ink-testing-library";
import * as agentModule from "../src/agent.ts";
import { App } from "../src/app.tsx";
import { ConfigStore, type Config, type FileConfig } from "../src/config/config.ts";
import { mouse } from "../src/mouse.ts";
import type { AgentEvent, ChatTurn, Provider, StreamOptions } from "../src/provider/types.ts";
import { selection } from "../src/selection.ts";
import type { Skill } from "../src/skills.ts";
import type { InstructionsCut } from "../src/prompt.ts";
import type { ModelInfo } from "../src/provider/models.ts";
import { SessionStore } from "../src/sessions.ts";
import { loadMemory, memoryPaths, type MemoryPaths } from "../src/memory.ts";
import { GENERAL_PURPOSE } from "../src/agents.ts";
import { SUBAGENT_MAX_STEPS } from "../src/subagent.ts";
import { projectKey } from "../src/paths.ts";
import { TrajectoryStore } from "../src/trajectory.ts";
import { McpManager } from "../src/mcp/manager.ts";
import { McpTrust } from "../src/mcp/trust.ts";
import { dns } from "../src/tools/web/address.ts";
import { FakeProvider, RoutedProvider, ScriptedProvider } from "./fake-provider.ts";

const ENTER = "\r";
const DOWN = "\x1b[B";

// Let React and the async provider stream settle.
const tick = (ms = 50) => Bun.sleep(ms);
/** Waits until `ready()` (up to 5 s), then a moment more: a component's key handler attaches just after it's drawn. */
async function until(ready: () => boolean) {
  for (let i = 0; i < 100 && !ready(); i++) await tick();
  await tick();
}

async function type(stdin: { write: (data: string) => void }, text: string) {
  stdin.write(text);
  await tick();
  stdin.write(ENTER);
}

// Any saved provider + model will do; makeProvider swaps in a fake, so nothing hits the network.
const LOCAL: FileConfig = { provider: "ollama", model: "qwen3.5:9b" };
/** Yolo off: every change asks (yolo would run these tests' edits without asking). */
const ASKS: FileConfig = { ...LOCAL, yolo: false };

let dir: string;
let project: string;
let store: ConfigStore;

beforeEach(async () => {
  dir = await mkdtemp(join(tmpdir(), "marv-app-"));
  store = new ConfigStore(dir);
  // The project the agent works in: its tools can read files here.
  project = await mkdtemp(join(tmpdir(), "marv-project-"));
  await writeFile(join(project, "notes.txt"), "remember the milk\n");
});
afterEach(async () => {
  cleanup();
  await rm(dir, { recursive: true, force: true });
  await rm(project, { recursive: true, force: true });
});

function renderApp(
  initialFile: FileConfig | null,
  splashMs = 0,
  copy = async (_text: string) => "test",
  makeProvider: (config: Config) => Provider = () => new FakeProvider(),
  instructions?: string,
  skills: Skill[] = [],
  skillProblems: string[] = [],
  hiddenPathFolders: string[] = [],
  instructionsCut?: InstructionsCut,
) {
  return render(
    <App
      store={store}
      initialFile={initialFile}
      env={{}}
      version="9.9.9"
      cwd="~/x"
      root={project}
      instructions={instructions}
      skills={skills}
      skillProblems={skillProblems}
      hiddenPathFolders={hiddenPathFolders}
      instructionsCut={instructionsCut}
      splashMs={splashMs}
      makeProvider={makeProvider}
      copy={copy}
      loadModels={async () => [{ id: "qwen3.5:9b", local: true, tools: true }]}
    />,
  );
}

describe("App", () => {
  test("shows the splash, then the main view after a key press", async () => {
    const { lastFrame, stdin } = renderApp(LOCAL, 60_000);
    expect(lastFrame()).toContain("press any key");

    stdin.write("x");
    await tick();
    expect(lastFrame()).toContain("Welcome to Marv");
  });

  test("first run opens setup and saves the result", async () => {
    const { lastFrame, frames, stdin } = renderApp(null);
    await tick();
    expect(lastFrame()).toContain("Marv setup");

    stdin.write(DOWN);
    await tick();
    stdin.write(ENTER); // Ollama
    await tick();
    stdin.write(ENTER); // the only model the fake list offers
    await tick();

    expect(await store.load()).toEqual(LOCAL);
    await tick(); // the "Saved" notice renders just after the file is written
    expect(frames.join("\n")).toContain("Saved to");
    expect(lastFrame()).toContain("Type a message");
  });

  test("streams a reply through the provider", async () => {
    const { frames, stdin } = renderApp(LOCAL);
    await type(stdin, "hello world");
    await tick(200);
    expect(frames.join("\n")).toContain("You said: hello world");
  });

  test("/help prints the command list", async () => {
    const { frames, stdin } = renderApp(LOCAL);
    await type(stdin, "/help");
    await tick();
    expect(frames.join("\n")).toContain("Clear the conversation");
  });

  test("/config shows the provider and model", async () => {
    const { frames, stdin } = renderApp(LOCAL);
    await type(stdin, "/config");
    await tick();
    const output = frames.join("\n");
    expect(output).toContain("Provider:  Ollama");
    expect(output).toContain("API key:   not needed");
  });

  test("dragging over text copies it and says so", async () => {
    const copied: string[] = [];
    const { lastFrame } = renderApp(LOCAL, 0, async (text) => {
      copied.push(text);
      return "wl-copy";
    });
    await tick();
    // The real app feeds frames to the store through Ink's transformOutput hook.
    selection.transformOutput(lastFrame()!);
    const y = lastFrame()!.split("\n").findIndex((line) => line.includes("Welcome"));
    const x = lastFrame()!.split("\n")[y]!.indexOf("Welcome");

    mouse.emit("event", { type: "press", x, y });
    mouse.emit("event", { type: "drag", x: x + 6, y });
    mouse.emit("event", { type: "release", x: x + 6, y });
    await tick();

    expect(copied).toEqual(["Welcome"]);
    expect(lastFrame()).toContain("Copied 7 chars");
    selection.clear();
  });

  test("sends the system prompt and the conversation to the provider", async () => {
    const calls: { history: ChatTurn[]; options?: StreamOptions }[] = [];
    const spy: Provider = {
      name: "spy",
      async *stream(history, options) {
        calls.push({ history: [...history], options });
        yield { type: "text_delta", text: "ok" };
        yield { type: "done" };
      },
    };
    const { stdin } = renderApp(LOCAL, 0, undefined, () => spy);
    await type(stdin, "first");
    await tick(100);
    await type(stdin, "second");
    await tick(100);

    expect(calls[0]!.options?.system).toContain("You are Marv");
    expect(calls[0]!.options?.system).toContain("~/x");
    // Stateless API: the second request carries the whole conversation so far.
    expect(calls[1]!.history).toEqual([
      { role: "user", text: "first" },
      { role: "assistant", text: "ok" },
      { role: "user", text: "second" },
    ]);
  });

  test("/model <id> switches model and saves it", async () => {
    const ollama: FileConfig = { provider: "ollama", model: "gemma4:12b" };
    const { lastFrame, stdin } = renderApp(ollama);
    await type(stdin, "/model qwen3.5:9b");
    await tick(100);
    expect(await store.load()).toEqual({ provider: "ollama", model: "qwen3.5:9b" });
    expect(lastFrame()).toContain("Ollama · qwen3.5:9b");
  });

  test("/model opens the picker for the current provider", async () => {
    const { lastFrame, stdin } = renderApp({ provider: "ollama", model: "gemma4:12b" });
    await type(stdin, "/model");
    await tick(100);
    expect(lastFrame()).toContain("Switch Ollama model");
    expect(lastFrame()).toContain("qwen3.5:9b");
  });

  test("shows thinking while the model reasons, then keeps only a short note", async () => {
    const calls: ChatTurn[][] = [];
    let release!: () => void;
    const gate = new Promise<void>((resolve) => (release = resolve));
    const thinker: Provider = {
      name: "thinker",
      async *stream(history) {
        calls.push([...history]);
        yield { type: "thinking_delta", text: "Step 1: read the question.\nStep 2: answer briefly." };
        await gate;
        yield { type: "text_delta", text: "Hi!" };
        yield { type: "done" };
      },
    };
    const { lastFrame, stdin } = renderApp(LOCAL, 0, undefined, () => thinker);
    await type(stdin, "hello");
    await tick(100);
    expect(lastFrame()).toContain("Thinking… (~13 tokens)");
    expect(lastFrame()).toContain("Step 2: answer briefly.");

    release();
    await tick(100);
    expect(lastFrame()).toContain("✻ Thought for");
    expect(lastFrame()).toContain("Hi!");
    expect(lastFrame()).not.toContain("Step 2");

    // The reasoning is never sent back: the next request has only the reply.
    await type(stdin, "again");
    await tick(100);
    expect(calls[1]).toEqual([
      { role: "user", text: "hello" },
      { role: "assistant", text: "Hi!" },
      { role: "user", text: "again" },
    ]);
  });

  test("/think toggles thinking and saves it", async () => {
    const { lastFrame, stdin } = renderApp({ provider: "ollama", model: "qwen3.5:9b" });
    await type(stdin, "/think");
    await tick(100);
    expect(await store.load()).toEqual({ provider: "ollama", model: "qwen3.5:9b", thinking: true });
    expect(lastFrame()).toContain("Thinking on");
  });

  test("a tool call shows as it streams: what it writes, and the code so far", async () => {
    let release!: () => void;
    const gate = new Promise<void>((resolve) => (release = resolve));
    const writer: Provider = {
      name: "writer",
      async *stream(history) {
        if (history.at(-1)?.role === "tool") {
          yield { type: "text_delta", text: "Written." };
          yield { type: "done" };
          return;
        }
        yield { type: "text_delta", text: "I'll write it:" };
        yield { type: "tool_call_delta", index: 0, name: "write_file", text: '{"path": "page.js", "content": "const a = 1;\\nfunction draw() {\\n  return a;' };
        await gate;
        yield { type: "tool_call", call: { id: "w1", name: "write_file", arguments: '{"path":"page.js","content":"const a = 1;\\n"}' } };
        yield { type: "done" };
      },
    };
    const { lastFrame, stdin } = renderApp(LOCAL, 0, undefined, () => writer);
    await type(stdin, "write page.js");
    await tick(150);
    const frame = lastFrame()!;
    expect(frame).toContain("I'll write it:"); // the text before the call stays
    expect(frame).toContain("Writing page.js…");
    expect(frame).toContain("tokens)");
    expect(frame).toContain("    return a;"); // indentation kept: it's code
    release();
    await tick(200);
    expect(lastFrame()).not.toContain("Writing page.js…");
    expect(lastFrame()).toContain("Written.");
  });

  test("empty replies: a retry says nothing, a nudge and giving up are shown", async () => {
    const empty = [{ type: "done" as const }];
    const model = new ScriptedProvider([empty, empty, empty]);
    const { lastFrame, stdin } = renderApp(LOCAL, 0, undefined, () => model);
    await type(stdin, "hello?");
    await tick(200);
    const frame = lastFrame()!;
    expect(model.requests).toHaveLength(3);
    expect(frame).toContain("two empty replies in a row");
    expect(frame).toContain("replies stayed empty");
  });

  test("runs the tools the model asks for and shows them in the transcript", async () => {
    const model = new ScriptedProvider([
      [{ type: "tool_call", call: { id: "c1", name: "read_file", arguments: '{"path":"notes.txt"}' } }, { type: "done" }],
      [{ type: "text_delta", text: "It says to remember the milk." }, { type: "done" }],
    ]);
    const { lastFrame, stdin } = renderApp(LOCAL, 0, undefined, () => model);
    await type(stdin, "what's in my notes?");
    await tick(200);

    const frame = lastFrame()!;
    expect(frame).toContain("● read_file notes.txt");
    expect(frame).toContain("⎿ 1 line");
    expect(frame).toContain("It says to remember the milk.");
    // The real tool ran on the project and its output went back to the model.
    expect(model.requests[1]!.history.at(-1)).toEqual({ role: "tool", callId: "c1", name: "read_file", text: "    1→remember the milk" });
    expect(model.requests[0]!.options.tools!.map((t) => t.name)).toEqual(["read_file", "glob", "grep", "web_fetch", "edit_file", "write_file", "bash", "memory", "agent"]);
  });

  test("a failing tool shows its error, and the model gets it to recover from", async () => {
    const model = new ScriptedProvider([
      [{ type: "tool_call", call: { id: "c1", name: "read_file", arguments: '{"path":"/etc/passwd"}' } }, { type: "done" }],
      [{ type: "text_delta", text: "I can't read that." }, { type: "done" }],
    ]);
    const { lastFrame, stdin } = renderApp(LOCAL, 0, undefined, () => model);
    await type(stdin, "read /etc/passwd");
    await tick(200);
    expect(lastFrame()).toContain("is outside the project");
    expect(model.requests[1]!.history.at(-1)).toMatchObject({ role: "tool", text: expect.stringContaining("outside the project") });
  });

  test("shows token usage and cache hits in the status bar", async () => {
    const model = new ScriptedProvider([
      [
        { type: "text_delta", text: "ok" },
        { type: "usage", usage: { promptTokens: 1200, completionTokens: 30, cachedTokens: 1000 } },
        { type: "done" },
      ],
    ]);
    const { lastFrame, stdin } = renderApp(LOCAL, 0, undefined, () => model);
    await type(stdin, "hi");
    await tick(150);
    expect(lastFrame()).toContain("1.2k tokens · 83% cached");
  });

  describe("compaction", () => {
    const step = (text: string, promptTokens: number): AgentEvent[] => [
      { type: "text_delta", text },
      { type: "usage", usage: { promptTokens, completionTokens: 10 } },
      { type: "done" },
    ];

    test("near a full context, the next message first compacts the conversation", async () => {
      const model = new ScriptedProvider(
        [step("first answer", 900), step("SUMMARY: user asked a question; answered.", 950), step("second answer", 120)],
        1000,
      );
      const { lastFrame, stdin } = renderApp(LOCAL, 0, undefined, () => model);
      await type(stdin, "one");
      await tick(150);
      expect(lastFrame()).toContain("910/1k ctx");

      await type(stdin, "two");
      await tick(300);
      // Request 2 asked for a summary; request 3 carried on from it.
      expect((model.requests[1]!.history.at(-1) as { text: string }).text).toContain("Summarize our conversation");
      expect(model.requests[2]!.history).toEqual([
        { role: "user", text: expect.stringContaining("SUMMARY: user asked a question; answered.") },
        { role: "assistant", text: expect.any(String) },
        { role: "user", text: "two" },
      ]);
      const frame = lastFrame()!;
      expect(frame).toContain("Compacted the conversation (the context was 91% full)");
      expect(frame).toContain("first answer"); // the transcript keeps everything
      expect(frame).toContain("second answer");
    });

    test("/compact runs on demand, with what to focus on", async () => {
      const model = new ScriptedProvider([step("answer", 100), step("SUMMARY", 120), step("next", 50)]);
      const { lastFrame, stdin } = renderApp(LOCAL, 0, undefined, () => model);
      await type(stdin, "hello");
      await tick(150);
      await type(stdin, "/compact keep the file names");
      await tick(200);
      expect((model.requests[1]!.history.at(-1) as { text: string }).text).toContain("Focus especially on: keep the file names");
      expect(lastFrame()).toContain("Compacted the conversation: 110 → about");
    });

    test("/compact with nothing to compact says so", async () => {
      const { lastFrame, stdin } = renderApp(LOCAL);
      await type(stdin, "/compact");
      await tick();
      expect(lastFrame()).toContain("Nothing to compact yet.");
    });

    test("Esc stops a compaction and changes nothing", async () => {
      const slowSummary: Provider = {
        name: "slow",
        async *stream(history, options) {
          if ((history.at(-1) as { text: string }).text.includes("Summarize")) {
            while (!options?.signal?.aborted) await Bun.sleep(10);
            return;
          }
          yield { type: "text_delta", text: "answer" };
          yield { type: "done" };
        },
      };
      const { lastFrame, stdin } = renderApp(LOCAL, 0, undefined, () => slowSummary);
      await type(stdin, "hello");
      await tick(150);
      await type(stdin, "/compact");
      await tick(100);
      expect(lastFrame()).toContain("Compacting the conversation…");
      stdin.write("\x1b");
      await tick(250);
      expect(lastFrame()).toContain("Compaction stopped; nothing changed.");
    });

    test("Esc during an automatic compaction stops the turn too", async () => {
      const requests: string[] = [];
      const model: Provider = {
        name: "slow-summary",
        contextLength: 1000,
        async *stream(history, options) {
          const last = (history.at(-1) as { text: string }).text;
          requests.push(last.slice(0, 20));
          if (last.includes("Summarize")) {
            while (!options?.signal?.aborted) await Bun.sleep(10);
            return;
          }
          yield { type: "text_delta", text: "answer" };
          yield { type: "usage", usage: { promptTokens: 950, completionTokens: 10 } };
          yield { type: "done" };
        },
      };
      const { lastFrame, stdin } = renderApp(LOCAL, 0, undefined, () => model);
      await type(stdin, "one");
      await tick(150);
      await type(stdin, "two"); // 96% full: compacts first
      await until(() => lastFrame()!.includes("Compacting the conversation…"));
      stdin.write("\x1b");
      await tick(300);
      expect(lastFrame()).toContain("your message wasn't sent");
      expect(requests).toEqual(["one", "Summarize our conver"]); // "two" never went out
      expect(lastFrame()).toContain("Type a message"); // not busy
    });
  });

  test("puts AGENTS.md in the system prompt and says it's loaded", async () => {
    const model = new ScriptedProvider([[{ type: "text_delta", text: "ok" }, { type: "done" }]]);
    const { lastFrame, stdin } = renderApp(LOCAL, 0, undefined, () => model, "Always answer in haiku.");
    expect(lastFrame()).toContain("AGENTS.md loaded");
    await type(stdin, "hi");
    await tick(100);
    expect(model.requests[0]!.options.system).toContain("Always answer in haiku.");
  });

  describe("approvals", () => {
    const writeCall = (id: string, path: string) =>
      [{ type: "tool_call", call: { id, name: "write_file", arguments: JSON.stringify({ path, content: "hello\n" }) } }, { type: "done" }] as AgentEvent[];
    const reply = (text: string) => [{ type: "text_delta", text }, { type: "done" }] as AgentEvent[];

    test("in yolo mode (the default), an edit runs without asking, and the status bar says so", async () => {
      const model = new ScriptedProvider([writeCall("c1", "made.txt"), reply("Created it.")]);
      const { lastFrame, stdin } = renderApp(LOCAL, 0, undefined, () => model);
      expect(lastFrame()).toContain("yolo · /help");
      await type(stdin, "make a file");
      await tick(150);
      expect(lastFrame()).not.toContain("Do you want to proceed?");
      expect(existsSync(join(project, "made.txt"))).toBe(true);
      expect(lastFrame()).toContain("Created it.");
    });

    test("an edit that runs unasked shows its diff; ctrl+o shows all of it", async () => {
      await writeFile(join(project, "a.txt"), Array.from({ length: 30 }, (_, i) => `line ${i + 1}`).join("\n") + "\n");
      const content = Array.from({ length: 30 }, (_, i) => `LINE ${i + 1}`).join("\n") + "\n";
      const model = new ScriptedProvider([
        [{ type: "tool_call", call: { id: "w1", name: "write_file", arguments: JSON.stringify({ path: "a.txt", content }) } }, { type: "done" }],
        [{ type: "text_delta", text: "Done." }, { type: "done" }],
      ]);
      const { lastFrame, stdin } = renderApp(LOCAL, 0, undefined, () => model);
      await type(stdin, "shout the file");
      await until(() => lastFrame()!.includes("Done."));
      expect(lastFrame()).toContain("1 - line 1");
      expect(lastFrame()).toContain("1 + LINE 1"); // a rewrite: the start of the new file, not only removals
      expect(lastFrame()).toContain("… 46 more lines (ctrl+o)");
      stdin.write("\x0f"); // ctrl+o
      await until(() => lastFrame()!.includes("30 + LINE 30"));
      expect(lastFrame()).not.toContain("(ctrl+o)");
    });

    test("/yolo off turns it off and saves it; then changes ask", async () => {
      const model = new ScriptedProvider([writeCall("c1", "made.txt"), reply("Created it.")]);
      const { lastFrame, stdin } = renderApp(LOCAL, 0, undefined, () => model);
      await type(stdin, "/yolo off");
      await tick(100);
      expect(await store.load()).toEqual({ ...LOCAL, yolo: false });
      expect(lastFrame()).toContain("Yolo off");
      expect(lastFrame()).not.toContain("yolo · /help");
      await type(stdin, "make a file");
      await tick(150);
      expect(lastFrame()).toContain("Do you want to proceed?");
    });

    describe("at the step limit", () => {
      const reads = (n: number) =>
        Array.from({ length: n }, (_, i) => [{ type: "tool_call", call: { id: `r${i}`, name: "read_file", arguments: `{"path":"notes-${i}.txt"}` } }, { type: "done" }] as AgentEvent[]);

      test("it asks to keep going instead of stopping, and yes carries on", async () => {
        const model = new ScriptedProvider([...reads(25), reply("Finally done.")]);
        const { lastFrame, stdin } = renderApp(LOCAL, 0, undefined, () => model);
        await type(stdin, "read it a lot");
        await until(() => lastFrame()!.includes("Keep going?"));
        expect(lastFrame()).toContain("Keep going? Marv has taken 25 steps on this request without finishing");
        expect(model.requests).toHaveLength(25); // waiting: nothing more is sent until the answer
        stdin.write(ENTER);
        await tick(150);
        expect(lastFrame()).toContain("Finally done.");
        expect(lastFrame()).not.toContain("Stopped");
      });

      test("no stops it, and says how to continue", async () => {
        const model = new ScriptedProvider([...reads(25), reply("never")]);
        const { lastFrame, stdin } = renderApp(LOCAL, 0, undefined, () => model);
        await type(stdin, "read it a lot");
        await until(() => lastFrame()!.includes("Keep going?"));
        stdin.write(DOWN);
        stdin.write(DOWN);
        await tick();
        stdin.write(ENTER);
        await tick(150);
        expect(lastFrame()).toContain('Stopped at 25 steps, as you asked. Say "continue" to pick up where it left off.');
        expect(model.requests).toHaveLength(25);
      });

      test("/steps off saves the config, and the run then goes past 25 steps without asking", async () => {
        const model = new ScriptedProvider([...reads(30), reply("Finally done.")]);
        const { lastFrame, stdin } = renderApp(LOCAL, 0, undefined, () => model);
        await type(stdin, "/steps off");
        await tick(100);
        expect(await store.load()).toEqual({ ...LOCAL, maxSteps: 0 });
        expect(lastFrame()).toContain("never asks");
        await type(stdin, "read it a lot");
        await until(() => lastFrame()!.includes("Finally done."));
        expect(lastFrame()).not.toContain("Keep going?");
        expect(model.requests).toHaveLength(31);
        await type(stdin, "/steps");
        await tick();
        expect(lastFrame()).toContain("never asks");
      });

      test("/steps 40 moves the limit to 40 and saves it", async () => {
        const model = new ScriptedProvider([...reads(40), reply("Finally done.")]);
        const { lastFrame, stdin } = renderApp(LOCAL, 0, undefined, () => model);
        await type(stdin, "/steps 40");
        await tick(100);
        expect(await store.load()).toEqual({ ...LOCAL, maxSteps: 40 });
        expect(lastFrame()).toContain("Keep going? after 40 steps");
        await type(stdin, "read it a lot");
        await until(() => lastFrame()!.includes("Keep going? Marv has taken 40 steps"));
        expect(model.requests).toHaveLength(40); // waiting: nothing more is sent until the answer
        stdin.write(ENTER); // yes
        await tick(150);
        expect(lastFrame()).toContain("Finally done.");
        expect(model.requests).toHaveLength(41);
      });

      test("/steps with a bad argument says so and changes nothing", async () => {
        const { lastFrame, stdin } = renderApp(LOCAL, 0, undefined, () => new FakeProvider());
        await type(stdin, "/steps maybe");
        await tick(100);
        expect(lastFrame()).toContain("Usage: /steps");
      });
    });

    test("a reply stuck repeating itself is stopped with a notice, and the model goes on (review, #42)", async () => {
      const looping: AgentEvent[] = [{ type: "text_delta", text: "Fix the test scripts now. ".repeat(60) }, { type: "done" }];
      const model = new ScriptedProvider([looping, [{ type: "text_delta", text: "Trying another way." }, { type: "done" }]]);
      const { lastFrame, stdin } = renderApp(LOCAL, 0, undefined, () => model);
      await type(stdin, "go");
      await until(() => lastFrame()!.includes("Trying another way."));
      expect(lastFrame()).toContain("stuck repeating itself");
      expect(lastFrame()).not.toContain("Fix the test scripts now. Fix the test scripts now.");
    });

    test("a change waits for approval, then runs and the model carries on", async () => {
      const model = new ScriptedProvider([writeCall("c1", "made.txt"), reply("Created it.")]);
      const { lastFrame, stdin } = renderApp(ASKS, 0, undefined, () => model);
      await type(stdin, "make a file");
      await tick(150);
      expect(lastFrame()).toContain("Create made.txt");
      expect(lastFrame()).toContain("Do you want to proceed?");
      expect(existsSync(join(project, "made.txt"))).toBe(false); // nothing happens before the answer

      stdin.write(ENTER); // Yes
      await tick(200);
      expect(await Bun.file(join(project, "made.txt")).text()).toBe("hello\n");
      expect(lastFrame()).toContain("⎿ created · 1 line");
      expect(lastFrame()).toContain("Created it.");
      expect(lastFrame()).not.toContain("Do you want to proceed?");
    });

    test("no stops the agent and leaves the file alone", async () => {
      const model = new ScriptedProvider([writeCall("c1", "made.txt"), reply("should not get here")]);
      const { lastFrame, stdin } = renderApp(ASKS, 0, undefined, () => model);
      await type(stdin, "make a file");
      await tick(150);
      stdin.write(DOWN);
      await tick();
      stdin.write(DOWN);
      await tick();
      stdin.write(ENTER); // No
      await tick(200);
      expect(existsSync(join(project, "made.txt"))).toBe(false);
      expect(lastFrame()).toContain("⎿ declined");
      expect(lastFrame()).toContain("Stopped. Tell Marv what to do instead.");
      expect(model.requests).toHaveLength(1);
    });

    test("'don't ask again' covers later changes this session", async () => {
      const model = new ScriptedProvider([writeCall("c1", "one.txt"), writeCall("c2", "two.txt"), reply("Both done.")]);
      const { lastFrame, stdin } = renderApp(ASKS, 0, undefined, () => model);
      await type(stdin, "make two files");
      await tick(150);
      stdin.write(DOWN);
      await tick();
      stdin.write(ENTER); // Yes, and don't ask again
      await tick(300);
      expect(existsSync(join(project, "one.txt"))).toBe(true);
      expect(existsSync(join(project, "two.txt"))).toBe(true); // no second prompt
      expect(lastFrame()).toContain("Both done.");
    });

    test("Esc at the prompt declines and stops the run", async () => {
      const model = new ScriptedProvider([writeCall("c1", "made.txt"), reply("should not get here")]);
      const { lastFrame, stdin } = renderApp(ASKS, 0, undefined, () => model);
      await type(stdin, "make a file");
      await tick(150);
      stdin.write("\x1b");
      await tick(300);
      expect(existsSync(join(project, "made.txt"))).toBe(false);
      expect(lastFrame()).toContain("Interrupted.");
      expect(lastFrame()).toContain("Type a message");
      expect(model.requests).toHaveLength(1);
    });

    test("ctrl+c at the prompt declines and stops", async () => {
      const model = new ScriptedProvider([writeCall("c1", "made.txt")]);
      const { lastFrame, stdin } = renderApp(ASKS, 0, undefined, () => model);
      await type(stdin, "make a file");
      await tick(150);
      stdin.write("\x03");
      await tick(200);
      expect(existsSync(join(project, "made.txt"))).toBe(false);
      expect(lastFrame()).toContain("Interrupted.");
      expect(lastFrame()).toContain("Type a message");
    });
  });

  describe("web_fetch approvals", () => {
    const fetchCall = (id: string, url: string) =>
      [{ type: "tool_call", call: { id, name: "web_fetch", arguments: JSON.stringify({ url }) } }, { type: "done" }] as AgentEvent[];
    const readCall = (id: string, path: string) =>
      [{ type: "tool_call", call: { id, name: "read_file", arguments: JSON.stringify({ path }) } }, { type: "done" }] as AgentEvent[];
    const reply = (text: string) => [{ type: "text_delta", text }, { type: "done" }] as AgentEvent[];
    const PAGE = "<html><head><title>Page</title></head><body><p>The page says hello.</p></body></html>";
    let fetched: string[];
    let restore: (() => void)[] = [];

    beforeEach(() => {
      fetched = [];
      const lookup = spyOn(dns, "lookup").mockResolvedValue(["93.184.215.14"]);
      const net = spyOn(globalThis, "fetch").mockImplementation((async (input: string | URL | Request) => {
        fetched.push(String(input));
        return new Response(PAGE, { headers: { "content-type": "text/html" } });
      }) as unknown as typeof fetch);
      restore = [() => lookup.mockRestore(), () => net.mockRestore()];
    });
    afterEach(() => restore.forEach((r) => r()));

    test("a site the user pasted is fetched without asking", async () => {
      const model = new ScriptedProvider([fetchCall("w1", "https://example.com/other-page"), reply("It says hello.")]);
      const { lastFrame, stdin } = renderApp(ASKS, 0, undefined, () => model);
      await type(stdin, "what's on https://example.com/start?");
      await until(() => lastFrame()!.includes("It says hello."));
      expect(lastFrame()).not.toContain("Do you want to proceed?");
      expect(fetched).toEqual(["https://example.com/other-page"]);
      expect(JSON.stringify(model.requests[1]!.history)).toContain("The page says hello.");
    });

    test("another site asks, even in yolo mode", async () => {
      const model = new ScriptedProvider([fetchCall("w1", "https://other.example/x?d=1"), reply("Done.")]);
      const { lastFrame, stdin } = renderApp(LOCAL, 0, undefined, () => model);
      await type(stdin, "look it up");
      await until(() => lastFrame()!.includes("Do you want to proceed?"));
      expect(lastFrame()).toContain("Fetch a web page");
      expect(lastFrame()).toContain("https://other.example/x?d=1");
      expect(lastFrame()).toContain("don't ask again for fetching from other.example");
      expect(fetched).toEqual([]);
    });

    test("'don't ask again' covers the whole site", async () => {
      const model = new ScriptedProvider([fetchCall("w1", "https://other.example/a"), fetchCall("w2", "https://other.example/b"), reply("Both read.")]);
      const { lastFrame, stdin } = renderApp(ASKS, 0, undefined, () => model);
      await type(stdin, "read both");
      await until(() => lastFrame()!.includes("Do you want to proceed?"));
      stdin.write(DOWN);
      await tick();
      stdin.write(ENTER); // Yes, and don't ask again
      await until(() => lastFrame()!.includes("Both read."));
      expect(fetched).toEqual(["https://other.example/a", "https://other.example/b"]);
    });

    test("a link in a file isn't a pasted link", async () => {
      await writeFile(join(project, "notes.txt"), "see https://evil.example/steal\n");
      const model = new ScriptedProvider([readCall("r1", "notes.txt"), fetchCall("w1", "https://evil.example/steal?d=secret"), reply("x")]);
      const { lastFrame, stdin } = renderApp(ASKS, 0, undefined, () => model);
      await type(stdin, "follow the link in notes.txt");
      await until(() => lastFrame()!.includes("Do you want to proceed?"));
      expect(fetched).toEqual([]);
    });
  });

  describe("skills", () => {
    const review: Skill = { name: "review", description: "Review code for bugs.", body: "Look for off-by-one errors.", dir: "/x/review", files: [], source: "project" };

    test("the model is offered the skill tool and the list of skills", async () => {
      const model = new ScriptedProvider([[{ type: "text_delta", text: "ok" }, { type: "done" }]]);
      const { lastFrame, stdin } = renderApp(LOCAL, 0, undefined, () => model, undefined, [review]);
      expect(lastFrame()).toContain("1 skill (/skills)");
      await type(stdin, "hi");
      await tick(100);
      expect(model.requests[0]!.options.tools!.map((t) => t.name)).toContain("skill");
      expect(model.requests[0]!.options.system).toContain("- review: Review code for bugs.");
    });

    test("without skills, there's no skill tool", async () => {
      const model = new ScriptedProvider([[{ type: "text_delta", text: "ok" }, { type: "done" }]]);
      const { stdin } = renderApp(LOCAL, 0, undefined, () => model);
      await type(stdin, "hi");
      await tick(100);
      expect(model.requests[0]!.options.tools!.map((t) => t.name)).not.toContain("skill");
    });

    test("/review args sends the skill's instructions with the request, but shows what you typed", async () => {
      const model = new ScriptedProvider([[{ type: "text_delta", text: "Reviewed." }, { type: "done" }]]);
      const { lastFrame, stdin } = renderApp(LOCAL, 0, undefined, () => model, undefined, [review]);
      stdin.write("/rev");
      await tick();
      expect(lastFrame()).toContain("/review"); // in the / menu
      expect(lastFrame()).toContain("Review code for bugs.");
      stdin.write("iew src/app.tsx");
      await tick();
      stdin.write(ENTER);
      await tick(150);
      const sent = model.requests[0]!.history.at(-1)!;
      expect(sent.role).toBe("user");
      expect((sent as { text: string }).text).toContain("Look for off-by-one errors.");
      expect((sent as { text: string }).text).toContain("src/app.tsx");
      expect(lastFrame()).toContain("> /review src/app.tsx");
      expect(lastFrame()).not.toContain("off-by-one");
    });

    test("startup problems become ONE notice, one bullet each", async () => {
      const cut = { chars: 86_000, totalLines: 400, nextLine: 120, sections: [{ heading: "## Conventions", line: 300 }] };
      const { lastFrame } = renderApp(LOCAL, 0, undefined, undefined, "x", [], [".marv/skills/x/SKILL.md needs a description"], ["~/.local/share/mise/installs/node/26.8.2/bin"], cut);
      await tick();
      const frame = lastFrame()!;
      // Exactly one Startup notice, naming how many problems, and it holds them all as bullets.
      expect(frame.split("Startup · 3 notes").length).toBe(2);
      expect(frame).toContain("1 skill couldn't be loaded (see /skills)");
      expect(frame).toContain("1 PATH folder is hidden from the sandbox");
      expect(frame).toContain("~/.local/share/mise/installs/node/26.8.2/bin");
      expect(frame).toContain("AGENTS.md is long");
    });

    test("the startup notice carries both a PATH and an AGENTS.md item", async () => {
      const cut = { chars: 86_000, totalLines: 400, nextLine: 120, sections: [{ heading: "## Conventions", line: 300 }, { heading: "## Prompt cache", line: 340 }] };
      const { lastFrame } = renderApp(LOCAL, 0, undefined, undefined, "x", [], [], ["~/.cache/.bun/bin"], cut);
      await tick();
      const frame = lastFrame()!;
      expect(frame.split("Startup · 2 notes").length).toBe(2);
      expect(frame).toContain("AGENTS.md is long: the model sees the first 20,000 of 86,000 characters");
      expect(frame).toContain("Up front: lines 1-119");
      expect(frame).toContain("1 PATH folder is hidden from the sandbox: ~/.cache/.bun/bin");
      expect(frame).toContain("Move the tool to a folder the sandbox shows");
    });

    test("with no startup problems, there's no Startup notice", async () => {
      const { lastFrame } = renderApp(LOCAL);
      await tick();
      expect(lastFrame()).not.toContain("Startup ·");
    });
  });

  test("Esc interrupts a running reply", async () => {
    const slow: Provider = {
      name: "slow",
      async *stream(_history, options) {
        for (let i = 0; i < 100 && !options?.signal?.aborted; i++) {
          await Bun.sleep(20);
          yield { type: "text_delta", text: "word " };
        }
        yield { type: "done" };
      },
    };
    const { lastFrame, stdin } = renderApp(LOCAL, 0, undefined, () => slow);
    await type(stdin, "go");
    await tick(150);
    expect(lastFrame()).toContain("esc to interrupt");
    stdin.write("\x1b");
    await tick(250); // a lone ESC is held briefly to tell it apart from escape sequences
    expect(lastFrame()).toContain("Interrupted.");
    expect(lastFrame()).toContain("Type a message");
  });

  describe("tokens and cost", () => {
    const reply = (usage: Record<string, number>) =>
      [{ type: "text_delta", text: "ok" }, { type: "usage", usage }, { type: "done" }] as AgentEvent[];

    function renderOpenRouter(model: Provider, models: ModelInfo[]) {
      return render(
        <App
          store={store}
          initialFile={{ provider: "openrouter", model: "acme/model", apiKey: "sk-or-test" }}
          env={{}}
          version="9.9.9"
          cwd="~/x"
          root={project}
          splashMs={0}
          makeProvider={() => model}
          loadModels={async () => models}
        />,
      );
    }

    test("shows the reported cost, and the context window from the model list", async () => {
      const model = new ScriptedProvider([
        reply({ promptTokens: 12_000, completionTokens: 300, cachedTokens: 0, cost: 0.0251 }),
        reply({ promptTokens: 12_400, completionTokens: 200, cachedTokens: 12_000, cost: 0.0042 }),
      ]);
      const { lastFrame, stdin } = renderOpenRouter(model, [{ id: "acme/model", tools: true, context: 200_000 }]);
      await tick();
      await type(stdin, "one");
      await tick(150);
      expect(lastFrame()).toContain("12.3k/200k ctx · 0% cached · $0.025");
      await type(stdin, "two");
      await tick(150);
      expect(lastFrame()).toContain("$0.029"); // the session total

      await type(stdin, "/cost");
      await tick();
      const frame = lastFrame()!;
      expect(frame).toContain("This session: 2 requests");
      expect(frame).toContain("24.4k input tokens (12k from the cache, 49%)");
      expect(frame).toContain("Cost: $0.029");
      expect(frame).toContain("Context: 12.6k of 200k tokens (6%)");
    });

    test("estimates the cost from the model's prices when none is reported", async () => {
      const model = new ScriptedProvider([reply({ promptTokens: 10_000, completionTokens: 1000 })]);
      const { lastFrame, stdin } = renderOpenRouter(model, [{ id: "acme/model", tools: true, priceIn: 2, priceOut: 10 }]);
      await tick();
      await type(stdin, "hi");
      await tick(150);
      expect(lastFrame()).toContain("~$0.030"); // 10k in at $2/M + 1k out at $10/M
    });

    test("a local model costs nothing", async () => {
      const model = new ScriptedProvider([reply({ promptTokens: 900, completionTokens: 10 })], 32_768);
      const { lastFrame, stdin } = renderApp(LOCAL, 0, undefined, () => model);
      await type(stdin, "hi");
      await tick(150);
      expect(lastFrame()).toContain("910/32.8k ctx · local");
    });
  });

  describe("sessions", () => {
    let sessions: SessionStore;
    beforeEach(() => {
      sessions = new SessionStore(join(dir, "sessions"));
    });

    function renderWithSessions(model: Provider, resume?: "latest" | "pick") {
      return render(
        <App
          store={store}
          initialFile={LOCAL}
          env={{}}
          version="9.9.9"
          cwd="~/x"
          root={project}
          splashMs={0}
          makeProvider={() => model}
          loadModels={async () => []}
          sessions={sessions}
          resume={resume}
        />,
      );
    }
    const say = (text: string) => [{ type: "text_delta", text }, { type: "done" }] as AgentEvent[];

    /** The real store, but loading takes `ms` (a big session on a slow disk). */
    function slowStore(ms: number): SessionStore {
      return Object.assign(Object.create(sessions) as SessionStore, {
        latest: async (root: string) => (await Bun.sleep(ms), sessions.latest(root)),
        list: async (root: string) => (await Bun.sleep(ms), sessions.list(root)),
      });
    }

    test("marv -c: a message typed while the session loads waits, then goes on top of it", async () => {
      const first = renderWithSessions(new ScriptedProvider([say("OLD-REPLY")]));
      await type(first.stdin, "OLD");
      await tick(400);
      first.unmount();

      const model = new ScriptedProvider([say("NEW-REPLY")]);
      const { lastFrame, stdin } = render(
        <App store={store} initialFile={LOCAL} env={{}} version="9.9.9" cwd="~/x" root={project} splashMs={0} makeProvider={() => model} loadModels={async () => []} sessions={slowStore(300)} resume="latest" />,
      );
      await type(stdin, "NEW"); // before the session has loaded
      await tick(400);
      expect(model.requests).toHaveLength(0);
      expect(lastFrame()).toContain("Resumed a session");
      expect(lastFrame()).toContain("> NEW"); // still in the prompt
      stdin.write(ENTER);
      await tick(200);
      expect(model.requests[0]!.history.map((t) => t.text)).toEqual(["OLD", "OLD-REPLY", "NEW"]);
    });

    test("a session can't be resumed into a turn that's running", async () => {
      const first = renderWithSessions(new ScriptedProvider([say("OLD-REPLY")]));
      await type(first.stdin, "OLD");
      await tick(400);
      first.unmount();

      const inner = new ScriptedProvider([say("still mine")]);
      const slow: Provider = { name: "slow", async *stream(h, o) { await Bun.sleep(500); yield* inner.stream(h, o); } };
      const { lastFrame, stdin } = render(
        <App store={store} initialFile={LOCAL} env={{}} version="9.9.9" cwd="~/x" root={project} splashMs={0} makeProvider={() => slow} loadModels={async () => []} sessions={slowStore(200)} />,
      );
      await type(stdin, "/resume");
      await type(stdin, "go"); // the list is still loading: this starts a turn
      await tick(300);
      expect(lastFrame()).toContain("stop the current turn");
      await tick(600);
      expect(lastFrame()).toContain("still mine");
      expect(lastFrame()).not.toContain("OLD-REPLY");
    });

    test("quitting right after a turn still saves it (the save timer hasn't fired yet)", async () => {
      let flush = async () => {};
      const { stdin } = render(
        <App
          store={store}
          initialFile={LOCAL}
          env={{}}
          version="9.9.9"
          cwd="~/x"
          root={project}
          splashMs={0}
          makeProvider={() => new ScriptedProvider([say("Paris.")])}
          loadModels={async () => []}
          sessions={sessions}
          onFlush={(f) => (flush = f)}
        />,
      );
      await type(stdin, "capital of France?");
      await tick(60); // the reply is in, the 200 ms save timer isn't
      stdin.write("\x03");
      stdin.write("\x03"); // quit
      await flush(); // what cli.tsx does before exiting
      const [saved] = await sessions.list(project);
      expect(saved?.title).toBe("capital of France?");
      expect((await sessions.latest(project))!.conversation.map((t) => t.text)).toEqual(["capital of France?", "Paris."]);
    });

    test("each turn is saved, with both the transcript and the conversation", async () => {
      const { stdin } = renderWithSessions(new ScriptedProvider([say("Paris.")]));
      await type(stdin, "capital of France?");
      await tick(500);
      const [summary] = await sessions.list(project);
      expect(summary).toMatchObject({ title: "capital of France?", messages: 2 });
      const saved = await sessions.load(project, summary!.id);
      expect(saved!.conversation).toEqual([
        { role: "user", text: "capital of France?" },
        { role: "assistant", text: "Paris." },
      ]);
    });

    test("/resume picks an earlier session and carries on from it", async () => {
      // An earlier session…
      const first = renderWithSessions(new ScriptedProvider([say("Paris.")]));
      await type(first.stdin, "capital of France?");
      await tick(500);
      first.unmount();

      // …picked up in a new one.
      const model = new ScriptedProvider([say("About 2.1 million.")]);
      const { lastFrame, stdin } = renderWithSessions(model);
      await type(stdin, "/resume");
      await tick(150);
      expect(lastFrame()).toContain("Resume a session");
      expect(lastFrame()).toContain("capital of France?");
      stdin.write(ENTER);
      await tick(200);
      expect(lastFrame()).toContain("Paris."); // the old transcript is back
      expect(lastFrame()).toContain("Resumed");

      await type(stdin, "and its population?");
      await tick(200);
      // The model gets the earlier conversation too.
      expect(model.requests[0]!.history).toEqual([
        { role: "user", text: "capital of France?" },
        { role: "assistant", text: "Paris." },
        { role: "user", text: "and its population?" },
      ]);
      await tick(400);
      expect(await sessions.list(project)).toHaveLength(1); // still the same session
    });

    test("--continue resumes the latest session at startup", async () => {
      const first = renderWithSessions(new ScriptedProvider([say("Paris.")]));
      await type(first.stdin, "capital of France?");
      await tick(500);
      first.unmount();

      const { lastFrame } = renderWithSessions(new ScriptedProvider([]), "latest");
      await tick(300);
      expect(lastFrame()).toContain("capital of France?");
      expect(lastFrame()).toContain("Paris.");
    });

    test("/resume with nothing saved says so", async () => {
      const { lastFrame, stdin } = renderWithSessions(new ScriptedProvider([]));
      await type(stdin, "/resume");
      await tick(150);
      expect(lastFrame()).toContain("No saved sessions for this project yet.");
    });

    test("/clear starts a new session and keeps the old one", async () => {
      const { stdin } = renderWithSessions(new ScriptedProvider([say("one"), say("two")]));
      await type(stdin, "first");
      await tick(500);
      await type(stdin, "/clear");
      await tick();
      await type(stdin, "second");
      await tick(500);
      expect((await sessions.list(project)).map((s) => s.title)).toEqual(["second", "first"]);
    });
  });

  describe("memory", () => {
    let paths: MemoryPaths;
    beforeEach(() => {
      paths = memoryPaths(dir, project);
    });

    async function renderWithMemory(model: Provider) {
      return render(
        <App
          store={store}
          initialFile={LOCAL}
          env={{}}
          version="9.9.9"
          cwd="~/x"
          root={project}
          splashMs={0}
          makeProvider={() => model}
          loadModels={async () => []}
          memory={{ paths, initial: await loadMemory(paths) }}
        />,
      );
    }

    test("the model saves a memory (with approval), and the next conversation knows it", async () => {
      const remember = [
        { type: "tool_call", call: { id: "m1", name: "memory", arguments: JSON.stringify({ action: "add", scope: "personal", text: "Prefers short answers." }) } },
        { type: "done" },
      ] as AgentEvent[];
      const first = new ScriptedProvider([remember, [{ type: "text_delta", text: "Noted." }, { type: "done" }]]);
      const app = await renderWithMemory(first);
      await type(app.stdin, "please keep answers short from now on");
      await tick(150);
      expect(app.lastFrame()).toContain("Remember (personal, all projects)");
      expect(app.lastFrame()).toContain("Prefers short answers.");
      app.stdin.write(ENTER); // approve
      await tick(200);
      expect((await loadMemory(paths)).personal).toEqual(["Prefers short answers."]);
      app.unmount();

      const next = new ScriptedProvider([[{ type: "text_delta", text: "ok" }, { type: "done" }]]);
      const later = await renderWithMemory(next);
      expect(later.lastFrame()).toContain("1 memory (/memory)");
      await type(later.stdin, "hi");
      await tick(100);
      expect(next.requests[0]!.options.system).toContain("Personal (all projects):\n- Prefers short answers.");
    });

    test("/remember, /memory and /forget edit memory directly", async () => {
      const { lastFrame, stdin } = await renderWithMemory(new ScriptedProvider([]));
      // Each message appears only after its file write, so waiting for it also waits for the file.
      await type(stdin, "/remember uses bun, not npm");
      await until(() => lastFrame()!.includes("Saved to personal memory."));
      await type(stdin, "/remember project: tests need Ollama running");
      await until(() => lastFrame()!.includes("Saved to project memory."));
      expect(lastFrame()).toContain("Saved to project memory.");
      expect(await loadMemory(paths)).toEqual({ personal: ["uses bun, not npm"], project: ["tests need Ollama running"] });

      await type(stdin, "/memory");
      await until(() => lastFrame()!.includes("• tests need Ollama running"));
      expect(lastFrame()).toContain("• uses bun, not npm");
      expect(lastFrame()).toContain("• tests need Ollama running");

      await type(stdin, "/forget ollama");
      await until(() => lastFrame()!.includes("Forgot:"));
      expect(lastFrame()).toContain("Forgot: tests need Ollama running");
      expect((await loadMemory(paths)).project).toEqual([]);
    });

    test("/clear starts a conversation that includes memories saved since", async () => {
      const model = new ScriptedProvider([[{ type: "text_delta", text: "ok" }, { type: "done" }], [{ type: "text_delta", text: "ok" }, { type: "done" }]]);
      const { stdin } = await renderWithMemory(model);
      await type(stdin, "/remember likes tabs");
      await tick();
      await type(stdin, "hi");
      await tick(100);
      expect(model.requests[0]!.options.system).not.toContain("likes tabs"); // fixed for this conversation
      await type(stdin, "/clear");
      await tick(100);
      await type(stdin, "hi again");
      await tick(100);
      expect(model.requests[1]!.options.system).toContain("- likes tabs");
    });
  });
});

describe("trajectories", () => {
  const reply = (text: string, usage = { promptTokens: 100, completionTokens: 10 }) =>
    [{ type: "text_delta", text }, { type: "usage", usage }, { type: "done" }] as AgentEvent[];
  const calls = (...list: { id: string; name: string; args: unknown }[]) =>
    [...list.map(({ id, name, args }) => ({ type: "tool_call", call: { id, name, arguments: JSON.stringify(args) } })), { type: "done" }] as AgentEvent[];

  let trajDir: string;
  beforeEach(async () => {
    trajDir = await mkdtemp(join(tmpdir(), "marv-traj-app-"));
  });
  afterEach(async () => {
    await rm(trajDir, { recursive: true, force: true });
  });

  function renderLogged(model: Provider, file: FileConfig = LOCAL) {
    const store2 = new TrajectoryStore(trajDir);
    const app = render(
      <App
        store={store}
        initialFile={file}
        env={{}}
        version="9.9.9"
        cwd="~/x"
        root={project}
        splashMs={0}
        makeProvider={() => model}
        loadModels={async () => []}
        agents={[GENERAL_PURPOSE]}
        trajectories={store2}
      />,
    );
    const records = async () => {
      const folder = join(trajDir, projectKey(project));
      if (!existsSync(folder)) return [];
      const files = readdirSync(folder);
      expect(files).toHaveLength(1);
      return (await Bun.file(join(folder, files[0]!)).text())
        .trim()
        .split("\n")
        .map((line) => JSON.parse(line) as Record<string, any>);
    };
    return { ...app, records };
  }

  test("a turn is logged step by step, a subagent's steps under its own id", async () => {
    const model = new ScriptedProvider([
      calls({ id: "w1", name: "write_file", args: { path: "made.txt", content: "hi\n" } }),
      calls({ id: "a1", name: "agent", args: { description: "Read notes", prompt: "What's in notes.txt?" } }),
      calls({ id: "r1", name: "read_file", args: { path: "notes.txt" } }),
      reply("Milk."),
      reply("Done: made it, and the notes say milk."),
    ]);
    const { stdin, records } = renderLogged(model);
    await type(stdin, "make a file and check the notes");
    await tick(400);
    const log = await records();
    expect(log.map((r) => [r.type, r.agent])).toEqual([
      ["session", undefined],
      ["turn_start", undefined],
      ["tool", "main"],
      ["subagent_start", "main"],
      ["tool", expect.stringMatching(/\.1$/)],
      ["request", expect.stringMatching(/\.1$/)],
      ["assistant", expect.stringMatching(/\.1$/)],
      ["agent_end", expect.stringMatching(/\.1$/)],
      ["tool", "main"],
      ["request", "main"],
      ["assistant", "main"],
      ["agent_end", "main"],
    ]);
    const turn = log[1]!.turn;
    expect(log.slice(1).every((r) => r.turn === turn)).toBe(true);
    expect(log[0]).toMatchObject({ v: 1, root: project, marv: "9.9.9", model: "qwen3.5:9b", tools: expect.arrayContaining(["bash", "agent"]) });
    expect(log[0]!.system).toContain("Marv");
    expect(log[1]).toMatchObject({ text: "make a file and check the notes", model: "qwen3.5:9b", yolo: true });
    expect(log[2]).toMatchObject({ call: { name: "write_file" }, approval: "auto", isError: false });
    expect(log[3]).toMatchObject({ subagent: log[4]!.agent, agentType: "general-purpose", description: "Read notes", prompt: "What's in notes.txt?" });
    expect(log[4]).toMatchObject({ call: { name: "read_file" }, approval: "none", output: expect.stringContaining("remember the milk") });
    expect(log.at(-1)).toMatchObject({ reason: "end", steps: 1, tools: 2 });
  });

  test("/good and /bad rate the last turn; the next message's praise or correction is logged too", async () => {
    const model = new ScriptedProvider([reply("Hi."), reply("Fixed."), reply("Ok.")]);
    const { stdin, lastFrame, records } = renderLogged(model);
    await type(stdin, "/good");
    await tick();
    expect(lastFrame()).toContain("Nothing to rate yet");

    await type(stdin, "hello");
    await tick(150);
    await type(stdin, "/bad too terse");
    await tick();
    expect(lastFrame()).toContain("Rated the last turn: bad (too terse).");
    await type(stdin, "/label greeting, smalltalk");
    await tick();
    expect(lastFrame()).toContain("Labeled the last turn: greeting, smalltalk.");
    await type(stdin, "that's wrong, try again");
    await tick(150);
    await type(stdin, "perfect, thanks");
    await tick(150);

    const log = await records();
    const turns = log.filter((r) => r.type === "turn_start").map((r) => r.turn);
    expect(turns).toHaveLength(3);
    expect(log.filter((r) => r.type === "feedback")).toEqual([
      expect.objectContaining({ turn: turns[0], score: -1, source: "explicit", note: "too terse" }),
      expect.objectContaining({ turn: turns[0], score: 0, source: "explicit", labels: ["greeting", "smalltalk"] }),
      expect.objectContaining({ turn: turns[0], score: -1, source: "implicit", phrase: "that's wrong" }),
      expect.objectContaining({ turn: turns[1], score: 1, source: "implicit", phrase: "perfect" }),
    ]);
  });

  test("a turn that wasn't logged can't be rated after logging is turned back on", async () => {
    const { stdin, lastFrame, records } = renderLogged(new ScriptedProvider([reply("Hi.")]), { ...LOCAL, trajectories: false });
    await type(stdin, "hello");
    await tick(150);
    await type(stdin, "/trajectories on");
    await tick(100);
    await type(stdin, "/bad wrong file");
    await tick();
    expect(lastFrame()).toContain("Nothing to rate yet");
    expect(await records()).toEqual([]);
  });

  test("/trajectories off stops logging and saves it", async () => {
    const { stdin, lastFrame, records } = renderLogged(new ScriptedProvider([reply("Hi.")]));
    await type(stdin, "/trajectories off");
    await tick(100);
    expect(await store.load()).toEqual({ ...LOCAL, trajectories: false });
    await type(stdin, "hello");
    await tick(150);
    await type(stdin, "/good");
    await tick();
    expect(lastFrame()).toContain("Trajectory logging is off");
    expect(await records()).toEqual([]);
  });
});

describe("MCP servers", () => {
  const FIXTURE = join(import.meta.dir, "fixtures", "mcp-server.ts");
  const server = (source: "personal" | "project" = "personal") => ({
    name: "test",
    source,
    key: "k",
    transport: { type: "stdio" as const, command: process.execPath, args: [FIXTURE] },
  });
  const reply = (text: string) => [{ type: "text_delta", text }, { type: "done" }] as AgentEvent[];
  const useTool = (id: string, name: string, args: unknown) =>
    [{ type: "tool_call", call: { id, name, arguments: JSON.stringify(args) } }, { type: "done" }] as AgentEvent[];
  let managers: McpManager[] = [];
  afterEach(async () => {
    await Promise.all(managers.map((m) => m.close()));
    managers = [];
  });

  function renderWithMcp(model: Provider, mcp: McpManager) {
    managers.push(mcp);
    void mcp.start(); // as cli.tsx does: not awaited
    return render(
      <App store={store} initialFile={LOCAL} env={{}} version="9.9.9" cwd="~/x" root={project} splashMs={0} makeProvider={() => model} loadModels={async () => []} mcp={mcp} />,
    );
  }

  test("the first request waits for the servers; their tools are offered and called, after asking", async () => {
    const model = new ScriptedProvider([useTool("e1", "mcp__test__echo", { text: "hi" }), reply("The server said hi.")]);
    const { lastFrame, stdin } = renderWithMcp(model, new McpManager([server()], { root: project, version: "9.9.9" }));
    await type(stdin, "use the echo tool"); // sent before the server is up
    await until(() => lastFrame()!.includes("Do you want to proceed?"));
    expect(lastFrame()).toContain("test: echo");
    expect(lastFrame()).toContain("runs outside the sandbox");
    const first = model.requests[0]!;
    expect(first.options.tools!.map((t) => t.name)).toContain("mcp__test__echo");
    expect(first.options.system).toContain("Tools named mcp__<server>__<tool>");
    stdin.write(ENTER);
    await tick(300);
    expect(lastFrame()).toContain("mcp__test__echo hi");
    expect(lastFrame()).toContain("The server said hi.");
    expect(model.requests[1]!.history.at(-1)).toMatchObject({ role: "tool", text: "echo: hi" });
  });

  /** An McpManager stand-in that stays "starting" until the test says it's ready. */
  function slowMcp() {
    let finish: () => void = () => {};
    const fake = {
      settled: false,
      ready: new Promise<void>((resolve) => (finish = resolve)),
      tools: [],
      specs: [],
      status: () => [],
      untrusted: () => [],
    };
    return { mcp: fake as unknown as McpManager, finish: () => ((fake.settled = true), finish()) };
  }

  test("while servers start, a message waits (with a spinner), and a second one can't start a second run", async () => {
    const model = new ScriptedProvider([reply("one done"), reply("never")]);
    const { mcp, finish } = slowMcp();
    const { lastFrame, stdin } = render(
      <App store={store} initialFile={LOCAL} env={{}} version="9.9.9" cwd="~/x" root={project} splashMs={0} makeProvider={() => model} loadModels={async () => []} mcp={mcp} />,
    );
    await type(stdin, "one");
    await tick(100);
    expect(lastFrame()).toContain("Waiting for MCP servers to start…");
    await type(stdin, "two"); // busy: not sent
    await tick(100);
    finish();
    await tick(200);
    expect(model.requests.map((r) => r.history.filter((t) => t.role === "user").map((t) => t.text))).toEqual([["one"]]);
    expect(lastFrame()).toContain("one done");
  });

  test("Esc cancels a message that's waiting for servers to start", async () => {
    const model = new ScriptedProvider([reply("never")]);
    const { mcp, finish } = slowMcp();
    const { lastFrame, stdin } = render(
      <App store={store} initialFile={LOCAL} env={{}} version="9.9.9" cwd="~/x" root={project} splashMs={0} makeProvider={() => model} loadModels={async () => []} mcp={mcp} />,
    );
    await type(stdin, "one");
    await tick(100);
    stdin.write("\x1b");
    await tick(100);
    expect(lastFrame()).toContain("Interrupted.");
    finish();
    await tick(200);
    expect(model.requests).toHaveLength(0);
  });

  test("a project's server waits for /mcp trust, and says so", async () => {
    const trust = new McpTrust(join(dir, "trust.json"));
    const { lastFrame, stdin } = renderWithMcp(new ScriptedProvider([]), new McpManager([server("project")], { root: project, version: "9.9.9", trust }));
    await tick(300);
    expect(lastFrame()).toContain("1 MCP server (/mcp)");
    expect(lastFrame()).toContain("MCP · 1 note");
    expect(lastFrame()).toContain("1 MCP server waiting for /mcp trust");
    expect(lastFrame()).toContain("test (");
    await type(stdin, "/mcp trust");
    for (let i = 0; i < 100 && !lastFrame()!.includes("test: connected"); i++) await tick();
    expect(lastFrame()).toContain("test: connected · 8 tools");
    expect(await trust.isTrusted(project, server("project"))).toBe(true);
  });

  test("the trust notice shows the raw config, the variables it reads, and the project files it runs", async () => {
    const shady = {
      ...server("project"),
      display: "node mcp/server.js --key=${OPENROUTER_API_KEY} · env NODE_OPTIONS=--require=./x.js",
      reads: ["OPENROUTER_API_KEY"],
      runsProjectFiles: ["mcp/server.js"],
    };
    const { lastFrame } = renderWithMcp(new ScriptedProvider([]), new McpManager([shady], { root: project, version: "9.9.9", trust: new McpTrust(join(dir, "trust.json")) }));
    await tick(300);
    const frame = lastFrame()!.replace(/\s+/g, " ");
    expect(frame).toContain("test (node mcp/server.js --key=${OPENROUTER_API_KEY} · env NODE_OPTIONS=--require=./x.js)");
    expect(frame).toContain("reads $OPENROUTER_API_KEY from your environment");
    expect(frame).toContain("runs this project's mcp/server.js");
  });
});

describe("subagents", () => {
  const ESC = "\x1b";
  const CTRL_O = "\x0f";
  const reply = (text: string) => [{ type: "text_delta", text }, { type: "done" }] as AgentEvent[];
  const calls = (...list: { id: string; name: string; args: unknown }[]) =>
    [...list.map(({ id, name, args }) => ({ type: "tool_call", call: { id, name, arguments: JSON.stringify(args) } })), { type: "done" }] as AgentEvent[];

  function renderAgents(model: Provider, file = ASKS) {
    return render(
      <App store={store} initialFile={file} env={{}} version="9.9.9" cwd="~/x" root={project} splashMs={0} makeProvider={() => model} loadModels={async () => []} agents={[GENERAL_PURPOSE]} />,
    );
  }

  test("a subagent is one entry that ends with its summary; ctrl+o shows its steps", async () => {
    const model = new ScriptedProvider([
      calls({ id: "a1", name: "agent", args: { description: "Read notes", prompt: "What's in notes.txt?" } }),
      calls({ id: "r1", name: "read_file", args: { path: "notes.txt" } }),
      reply("It says remember the milk."),
      reply("The subagent says: milk."),
    ]);
    const { lastFrame, stdin } = renderAgents(model);
    await type(stdin, "check the notes");
    await tick(300);
    expect(lastFrame()).toContain("agent general-purpose · Read notes");
    expect(lastFrame()).toContain("done · 1 tool");
    expect(lastFrame()).toContain("The subagent says: milk.");
    expect(model.requests[3]!.history.at(-1)).toMatchObject({ role: "tool", text: "It says remember the milk." });
    expect(lastFrame()).not.toContain("read_file notes.txt · 1 line");

    stdin.write(CTRL_O);
    await tick();
    expect(lastFrame()).toContain("read_file notes.txt · 1 line");
    stdin.write(CTRL_O); // and again hides them
    await tick();
    expect(lastFrame()).not.toContain("read_file notes.txt · 1 line");
  });

  /** Waits before answering the requests listed (by index), so a subagent can be seen mid-run. */
  const slowAt = (inner: ScriptedProvider, waits: Record<number, number>): Provider => ({
    name: "slow",
    async *stream(history, options) {
      const wait = waits[inner.requests.length];
      if (wait) await Bun.sleep(wait);
      yield* inner.stream(history, options);
    },
  });

  test("while a subagent runs, its entry shows what it's doing", async () => {
    const inner = new ScriptedProvider([
      calls({ id: "a1", name: "agent", args: { description: "Read notes", prompt: "What's in notes.txt?" } }),
      calls({ id: "r1", name: "read_file", args: { path: "notes.txt" } }),
      reply("It says remember the milk."), // asked for after a pause
      reply("Done."),
    ]);
    const { lastFrame, stdin } = renderAgents(slowAt(inner, { 2: 400 }));
    await type(stdin, "check the notes");
    await tick(200);
    expect(lastFrame()).toContain("⎿ shared folder · 1 tool · thinking…");
    expect(lastFrame()).toContain("1 agent running");
    stdin.write(CTRL_O);
    await tick();
    expect(lastFrame()).toContain("    · read_file notes.txt · 1 line"); // its steps so far
    await tick(500);
    expect(lastFrame()).toContain("done · 1 tool");
    expect(lastFrame()).not.toContain("agent running");
  });

  /** Clicks the first screen row containing `text` (the real app also feeds each frame to the selection store). */
  function clickOn(frame: string, text: string) {
    selection.transformOutput(frame);
    const lines = frame.split("\n");
    const y = lines.findIndex((line) => line.includes(text));
    if (y < 0) throw new Error(`"${text}" isn't on screen`);
    const x = lines[y]!.indexOf(text);
    mouse.emit("event", { type: "press", x, y });
    mouse.emit("event", { type: "release", x, y });
  }

  describe("clicking a subagent opens its own view", () => {
    test("live while it runs; Esc goes back without stopping it", async () => {
      const inner = new ScriptedProvider([
        calls({ id: "a1", name: "agent", args: { description: "Read notes", prompt: "What's in notes.txt?" } }),
        calls({ id: "r1", name: "read_file", args: { path: "notes.txt" } }),
        reply("It says remember the milk."), // asked for after a pause
        reply("Done."),
      ]);
      const { lastFrame, stdin } = renderAgents(slowAt(inner, { 2: 400 }));
      await type(stdin, "check the notes");
      await tick(200);
      clickOn(lastFrame()!, "shared folder · 1 tool"); // its second line counts too
      await tick();
      let frame = lastFrame()!;
      expect(frame).toContain("● agent general-purpose · Read notes · running");
      expect(frame).toContain("esc to go back");
      expect(frame).toContain("> What's in notes.txt?"); // its task
      expect(frame).toContain("read_file notes.txt");
      expect(frame).toContain("⎿ 1 line");
      expect(frame).not.toContain("check the notes"); // the main transcript is hidden
      expect(frame).toContain(" esc to go back · 1 agent running"); // Esc doesn't interrupt here

      await tick(400);
      frame = lastFrame()!;
      expect(frame).toContain("It says remember the milk."); // its reply arrives live
      expect(frame).toContain("Read notes · finished");

      stdin.write(ESC);
      await tick();
      frame = lastFrame()!;
      expect(frame).not.toContain("esc to go back");
      expect(frame).toContain("> check the notes");
      expect(frame).toContain("Done.");
      expect(frame).not.toContain("Interrupted");
    });

    test("Esc while it runs only closes the view", async () => {
      const inner = new ScriptedProvider([
        calls({ id: "a1", name: "agent", args: { description: "Read notes", prompt: "p" } }),
        calls({ id: "r1", name: "read_file", args: { path: "notes.txt" } }),
        reply("Milk."),
        reply("Done."),
      ]);
      const { lastFrame, stdin } = renderAgents(slowAt(inner, { 2: 300 }));
      await type(stdin, "check the notes");
      await tick(150);
      clickOn(lastFrame()!, "agent general-purpose · Read notes");
      await tick();
      expect(lastFrame()).toContain("esc to go back");
      stdin.write(ESC);
      await tick(400);
      expect(lastFrame()).not.toContain("Interrupted");
      expect(lastFrame()).toContain("Done.");
    });

    test("an approval it asks for shows below its view; Esc there doesn't decline it", async () => {
      const inner = new ScriptedProvider([
        calls({ id: "a1", name: "agent", args: { description: "Make file", prompt: "Create made.txt" } }),
        calls({ id: "w1", name: "write_file", args: { path: "made.txt", content: "hi\n" } }),
        reply("Made it."),
        reply("Done."),
      ]);
      const { lastFrame, stdin } = renderAgents(inner);
      await type(stdin, "make a file");
      await tick(150);
      expect(lastFrame()).toContain("Do you want to proceed?");
      clickOn(lastFrame()!, "agent general-purpose · Make file");
      await tick();
      expect(lastFrame()).toContain("> Create made.txt");
      expect(lastFrame()).toContain("Do you want to proceed?");
      stdin.write(ESC);
      await tick();
      expect(lastFrame()).not.toContain("esc to go back");
      expect(lastFrame()).toContain("Do you want to proceed?"); // still asking
      stdin.write(ENTER); // yes
      await tick(200);
      expect(existsSync(join(project, "made.txt"))).toBe(true);
      expect(lastFrame()).toContain("Done.");
    });

    test("sending a message closes the view; a click elsewhere opens nothing", async () => {
      const model = new ScriptedProvider([
        calls({ id: "a1", name: "agent", args: { description: "Read notes", prompt: "p" } }),
        reply("Milk."),
        reply("Done."),
        reply("Hi again."),
      ]);
      const { lastFrame, stdin } = renderAgents(model);
      await type(stdin, "check the notes");
      await tick(200);
      clickOn(lastFrame()!, "check the notes");
      await tick();
      expect(lastFrame()).not.toContain("esc to go back");
      clickOn(lastFrame()!, "agent general-purpose · Read notes");
      await tick();
      expect(lastFrame()).toContain("● Milk.");
      await type(stdin, "hello");
      await tick(150);
      expect(lastFrame()).not.toContain("esc to go back");
      expect(lastFrame()).toContain("Hi again.");
    });
  });

  test("an agent call with arguments that aren't an object doesn't break the session", async () => {
    const model = new ScriptedProvider([
      [{ type: "tool_call", call: { id: "x1", name: "agent", arguments: "null" } }, { type: "done" }] as AgentEvent[],
      reply("Sorry, let me try again."),
      reply("Hi again."),
    ]);
    const { lastFrame, stdin } = renderAgents(model);
    await type(stdin, "go");
    await tick(300);
    expect(lastFrame()).not.toContain("null is not an object");
    await type(stdin, "again");
    await tick(200);
    // Every tool call in the history has its result before the next user turn.
    const history = model.requests.at(-1)!.history;
    const callAt = history.findIndex((t) => t.role === "assistant" && t.toolCalls?.length);
    expect(history[callAt + 1]).toMatchObject({ role: "tool", callId: "x1" });
    expect(lastFrame()).toContain("Hi again.");
  });

  test("a call id reused in a later step doesn't inherit a subagent's steps", async () => {
    // Providers number calls per reply (Ollama's call_0, call_1…), so ids repeat.
    const model = new ScriptedProvider([
      calls({ id: "call_0", name: "agent", args: { description: "Read notes", prompt: "What's in notes.txt?" } }),
      calls({ id: "r1", name: "read_file", args: { path: "notes.txt" } }), // the subagent
      reply("It says remember the milk."), // the subagent
      calls({ id: "call_0", name: "read_file", args: { path: "notes.txt" } }), // the parent, next step
      reply("Checked it myself."),
    ]);
    const { lastFrame, stdin } = renderAgents(model);
    await type(stdin, "check the notes");
    await tick(300);
    expect(lastFrame()).toContain("Checked it myself.");
    stdin.write(CTRL_O);
    await tick();
    const lines = lastFrame()!.split("\n");
    const own = lines.findIndex((line) => line.includes("● read_file notes.txt"));
    expect(own).toBeGreaterThan(-1);
    expect(lines[own + 1]).toContain("⎿ 1 line");
    expect(lines[own + 2]).not.toContain("· read_file");
  });

  test("if the loop fails mid-run, its entries end and a subagent still running can't bring the run back", async () => {
    const real = agentModule.runAgent;
    const spy = spyOn(agentModule, "runAgent").mockImplementation(async function* (options) {
      if (options.maxSteps === SUBAGENT_MAX_STEPS) return yield* real(options); // a subagent: the real loop
      const call = { id: "a1", name: "agent", arguments: JSON.stringify({ description: "write", prompt: "write one.txt" }) };
      yield { type: "tool_start", call, label: "general-purpose · write" };
      void options.runTool(call);
      await Bun.sleep(100); // the subagent is waiting at its approval prompt
      throw new Error("boom");
    });
    try {
      const writer = new ScriptedProvider([calls({ id: "w1", name: "write_file", args: { path: "one.txt", content: "x\n" } })]);
      const { lastFrame, stdin } = renderAgents(writer);
      await type(stdin, "go");
      await tick(400);
      expect(lastFrame()).toContain("Error: boom");
      expect(lastFrame()).toContain("⎿ interrupted");
      expect(lastFrame()).not.toContain("Do you want to proceed?");
      expect(lastFrame()).not.toContain("esc to interrupt");
      expect(lastFrame()).toContain("Type a message");
      expect(existsSync(join(project, "one.txt"))).toBe(false);
    } finally {
      spy.mockRestore();
    }
  });

  function twoWriters() {
    const parent = new ScriptedProvider([
      calls(
        { id: "a1", name: "agent", args: { description: "one", prompt: "write one.txt" } },
        { id: "a2", name: "agent", args: { description: "two", prompt: "write two.txt" } },
      ),
      reply("Both done."),
    ]);
    const writer = (path: string) => new ScriptedProvider([calls({ id: `w-${path}`, name: "write_file", args: { path, content: "x\n" } }), reply(`wrote ${path}`)]);
    return new RoutedProvider({ "make two": parent, "write one.txt": writer("one.txt"), "write two.txt": writer("two.txt") });
  }

  test("parallel subagents' approvals queue up, one at a time", async () => {
    const { lastFrame, stdin } = renderAgents(twoWriters());
    await type(stdin, "make two files");
    await tick(200);
    expect(lastFrame()).toContain("Do you want to proceed?");
    expect(lastFrame()).toContain("1 more waiting");
    expect(lastFrame()).toContain("2 agents running");

    stdin.write(ENTER);
    await tick(100);
    expect(lastFrame()).toContain("Do you want to proceed?"); // the second one
    expect(lastFrame()).not.toContain("more waiting");
    stdin.write(ENTER);
    await tick(300);
    expect(existsSync(join(project, "one.txt"))).toBe(true);
    expect(existsSync(join(project, "two.txt"))).toBe(true);
    expect(lastFrame()).toContain("Both done.");
    expect(lastFrame()).not.toContain("agents running");
  });

  test("each queued approval starts with its cursor on Yes", async () => {
    const { lastFrame, stdin } = renderAgents(twoWriters());
    await type(stdin, "make two files");
    await tick(200);
    expect(lastFrame()).toContain("1 more waiting");
    stdin.write(DOWN);
    await tick();
    stdin.write(DOWN);
    await tick();
    expect(lastFrame()).toContain("❯ No, and tell Marv");
    stdin.write(ENTER); // No, for the first subagent
    await tick(100);
    expect(lastFrame()).toContain("Do you want to proceed?"); // the second one, a fresh prompt
    expect(lastFrame()).toContain("❯ Yes");
    expect(lastFrame()).not.toContain("❯ Yes, and");
    expect(lastFrame()).not.toContain("❯ No");
  });

  test("Esc at a queued approval declines them all and stops the run", async () => {
    const { lastFrame, stdin } = renderAgents(twoWriters());
    await type(stdin, "make two files");
    await tick(200);
    stdin.write(ESC);
    await tick(300);
    expect(existsSync(join(project, "one.txt"))).toBe(false);
    expect(existsSync(join(project, "two.txt"))).toBe(false);
    expect(lastFrame()).toContain("Interrupted.");
    expect(lastFrame()).not.toContain("Do you want to proceed?");
    expect(lastFrame()).not.toContain("agents running");
  });

  test("'always' answers the other queued requests in the same scope", async () => {
    const { lastFrame, stdin } = renderAgents(twoWriters());
    await type(stdin, "make two files");
    await tick(200);
    expect(lastFrame()).toContain("1 more waiting");
    stdin.write(DOWN);
    await tick();
    stdin.write(ENTER); // Yes, and don't ask again for file changes
    await tick(300);
    expect(existsSync(join(project, "one.txt"))).toBe(true);
    expect(existsSync(join(project, "two.txt"))).toBe(true); // no second prompt
    expect(lastFrame()).toContain("Both done.");
  });

  test("agent files that couldn't be loaded are reported", async () => {
    const { lastFrame } = render(
      <App store={store} initialFile={LOCAL} env={{}} version="9.9.9" cwd="~/x" root={project} splashMs={0} makeProvider={() => new FakeProvider()} loadModels={async () => []} agentProblems={[".marv/agents/x.md needs a description"]} />,
    );
    await tick();
    expect(lastFrame()).toContain("1 agent file couldn't be loaded");
    expect(lastFrame()).toContain(".marv/agents/x.md needs a description");
  });
});
