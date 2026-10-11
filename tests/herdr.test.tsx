import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { createServer, type Server } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { cleanup, render } from "ink-testing-library";
import { App } from "../src/app.tsx";
import { ConfigStore, type FileConfig } from "../src/config/config.ts";
import { HerdrReporter, herdrTarget, type HerdrSend } from "../src/herdr.ts";
import { SessionStore } from "../src/sessions.ts";
import { ScriptedProvider } from "./fake-provider.ts";

const ENTER = "\r";
const tick = (ms = 50) => Bun.sleep(ms);
const PANE = { socketPath: "/unused", paneId: "w1:p7" };

type Request = { id: string; method: string; params: Record<string, unknown> };

/** A send that records every request and answers at once. */
function recorder() {
  const requests: Request[] = [];
  const send: HerdrSend = async (request) => {
    requests.push(request as Request);
    return true;
  };
  const states = () => requests.filter((r) => r.method === "pane.report_agent").map((r) => r.params.state);
  return { requests, send, states };
}

let dir: string;
beforeEach(async () => {
  dir = await mkdtemp(join(tmpdir(), "marv-herdr-"));
});
afterEach(async () => {
  cleanup();
  await rm(dir, { recursive: true, force: true });
});

describe("herdrTarget", () => {
  test("is the pane Herdr says Marv runs in", () => {
    expect(herdrTarget({ HERDR_ENV: "1", HERDR_SOCKET_PATH: "/run/h.sock", HERDR_PANE_ID: "w1:p2" })).toEqual({
      socketPath: "/run/h.sock",
      paneId: "w1:p2",
    });
  });

  test("is nothing outside Herdr, or with half its variables", () => {
    expect(herdrTarget({})).toBeUndefined();
    expect(herdrTarget({ HERDR_SOCKET_PATH: "/run/h.sock", HERDR_PANE_ID: "w1:p2" })).toBeUndefined();
    expect(herdrTarget({ HERDR_ENV: "1", HERDR_PANE_ID: "w1:p2" })).toBeUndefined();
    expect(herdrTarget({ HERDR_ENV: "1", HERDR_SOCKET_PATH: "/run/h.sock" })).toBeUndefined();
  });
});

describe("HerdrReporter", () => {
  test("speaks Herdr's protocol: one JSON line per request on the pane's socket", async () => {
    const socketPath = join(dir, "herdr.sock");
    const lines: Request[] = [];
    const server: Server = createServer((socket) => {
      socket.on("data", (data) => {
        for (const line of data.toString().split("\n").filter(Boolean)) lines.push(JSON.parse(line));
        socket.write('{"result":{"type":"ok"}}\n');
      });
    });
    await new Promise<void>((resolve) => server.listen(socketPath, resolve));
    try {
      const herdr = new HerdrReporter({ socketPath, paneId: "w1:p3" });
      await herdr.session("abc", "/home/me/.marv/sessions/p/abc.json");
      herdr.state("working");
      await herdr.settled();
      await herdr.release();

      expect(lines.map((l) => l.method)).toEqual(["pane.report_agent_session", "pane.report_agent", "pane.release_agent"]);
      for (const l of lines) expect(l.params).toMatchObject({ pane_id: "w1:p3", source: "herdr:marv", agent: "marv" });
      expect(lines[0]!.params).toMatchObject({ agent_session_id: "abc", agent_session_path: "/home/me/.marv/sessions/p/abc.json" });
      // A state report carries the session too, so Herdr ties the state to it.
      expect(lines[1]!.params).toMatchObject({ state: "working", agent_session_id: "abc" });
      // Herdr ignores a report that isn't newer than the last one from this source.
      const seqs = lines.map((l) => l.params.seq as number);
      expect(seqs).toEqual([...seqs].sort((a, b) => a - b));
      expect(new Set(seqs).size).toBe(3);
    } finally {
      server.close();
    }
  });

  test("skips a state that didn't change", async () => {
    const { send, states } = recorder();
    const herdr = new HerdrReporter(PANE, send);
    herdr.state("idle");
    await herdr.settled();
    herdr.state("idle");
    herdr.state("blocked", "bash: npm test");
    await herdr.settled();
    herdr.state("blocked", "bash: npm test");
    await herdr.settled();
    expect(states()).toEqual(["idle", "blocked"]);
  });

  test("sends only the newest state when several arrive while one is on its way", async () => {
    const sent: unknown[] = [];
    let answer!: () => void;
    const send: HerdrSend = (request) => {
      sent.push((request as Request).params.state);
      return new Promise((resolve) => (answer = () => resolve(true)));
    };
    const herdr = new HerdrReporter(PANE, send);
    herdr.state("working");
    herdr.state("blocked");
    herdr.state("working");
    herdr.state("idle");
    answer();
    await tick(10);
    answer();
    await herdr.settled();
    expect(sent).toEqual(["working", "idle"]);
  });

  test("a Herdr that doesn't answer costs one retry, then is let go", async () => {
    let calls = 0;
    const herdr = new HerdrReporter(PANE, async () => (calls++, false));
    herdr.state("working");
    await herdr.settled();
    await herdr.release();
    expect(calls).toBe(4); // the state and the release, each tried twice
  });

  test("a socket nobody listens on doesn't throw", async () => {
    const herdr = new HerdrReporter({ socketPath: join(dir, "missing.sock"), paneId: "w1:p1" });
    herdr.state("working");
    await herdr.settled();
    await herdr.release();
  });
});

describe("App in a Herdr pane", () => {
  test("reports idle, working while a turn runs, blocked at an approval, and idle when done", async () => {
    const project = await mkdtemp(join(dir, "project-"));
    await writeFile(join(project, "notes.txt"), "hi\n");
    const sessions = new SessionStore(join(dir, "sessions"));
    const model = new ScriptedProvider([
      [{ type: "tool_call", call: { id: "c1", name: "write_file", arguments: '{"path":"made.txt","content":"hello\\n"}' } }, { type: "done" }],
      [{ type: "text_delta", text: "Created it." }, { type: "done" }],
    ]);
    const { requests, send, states } = recorder();
    const herdr = new HerdrReporter(PANE, send);
    const asks: FileConfig = { provider: "ollama", model: "qwen3.5:9b", yolo: false };
    const { stdin, lastFrame } = render(
      <App
        store={new ConfigStore(join(dir, "config"))}
        initialFile={asks}
        env={{}}
        version="9.9.9"
        cwd="~/x"
        root={project}
        splashMs={0}
        makeProvider={() => model}
        loadModels={async () => [{ id: "qwen3.5:9b", local: true, tools: true }]}
        sessions={sessions}
        herdr={herdr}
      />,
    );
    await tick();
    expect(states()).toEqual(["idle"]);
    const session = requests.find((r) => r.method === "pane.report_agent_session")!;
    expect(session.params.agent_session_path).toBe(sessions.file(project, session.params.agent_session_id as string));

    stdin.write("make a file");
    await tick();
    stdin.write(ENTER);
    await tick(150);
    expect(lastFrame()).toContain("Do you want to proceed?");
    const blocked = requests.find((r) => r.params.state === "blocked")!;
    expect(blocked.params.message).toBe("made.txt");

    stdin.write(ENTER); // Yes
    await tick(200);
    await herdr.settled();
    expect(lastFrame()).toContain("Created it.");
    const seen = states();
    expect(seen.indexOf("working")).toBeLessThan(seen.indexOf("blocked"));
    expect(seen.at(-1)).toBe("idle");
  });
});
