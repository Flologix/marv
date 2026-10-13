import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { existsSync } from "node:fs";
import { mkdir, mkdtemp, readFile, rm, symlink, writeFile } from "node:fs/promises";
import { homedir, tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { z } from "zod";
import * as sdk from "@flologixai/marv/sdk";
import { createSession, type SessionEvent, type Tool } from "@flologixai/marv/sdk";
import { homeOrAbove } from "../src/paths.ts";
import type { AgentEvent } from "../src/provider/types.ts";
import { sandboxAvailable } from "../src/sandbox.ts";
import { ScriptedProvider } from "./fake-provider.ts";
import { McpManager } from "../src/mcp/manager.ts";
import { McpTokenStore } from "../src/mcp/oauth-store.ts";
import { oauthServer } from "./fixtures/oauth-server.ts";

let root: string;
let configDir: string;
beforeEach(async () => {
  root = await mkdtemp(join(tmpdir(), "marv-sdk-project-"));
  configDir = await mkdtemp(join(tmpdir(), "marv-sdk-config-"));
  await writeFile(join(root, "AGENTS.md"), "Always answer in French.");
  await mkdir(join(root, ".marv", "skills", "release-notes"), { recursive: true });
  await writeFile(join(root, ".marv", "skills", "release-notes", "SKILL.md"), "---\nname: release-notes\ndescription: Write release notes from git log\n---\nRun git log.");
});
afterEach(async () => {
  await rm(root, { recursive: true, force: true });
  await rm(configDir, { recursive: true, force: true });
});

const say = (text: string): AgentEvent[] => [
  { type: "text_delta", text },
  { type: "done", reason: "stop" },
];
async function collect(events: AsyncIterable<SessionEvent>): Promise<SessionEvent[]> {
  const out: SessionEvent[] = [];
  for await (const event of events) out.push(event);
  return out;
}

test("the package entry exports what the README promises", () => {
  for (const name of ["createSession", "ToolError", "OllamaProvider", "OpenAICompatProvider", "SessionStore", "TrajectoryStore"]) {
    expect(sdk).toHaveProperty(name);
  }
});

test("by default nothing is read from disk", async () => {
  const provider = new ScriptedProvider([say("Hello.")]);
  const session = await createSession({ cwd: root, provider, configDir });
  await collect(session.send("hi"));
  const { system, tools } = provider.requests[0]!.options;
  expect(system).not.toContain("Always answer in French.");
  expect(tools!.map((t) => t.name)).not.toContain("skill");
  await session.close();
});

test("sources: ['project'] reads the repository's AGENTS.md and skills", async () => {
  const provider = new ScriptedProvider([say("Bonjour.")]);
  const session = await createSession({ cwd: root, provider, configDir, sources: ["project"] });
  await collect(session.send("hi"));
  const { system, tools } = provider.requests[0]!.options;
  expect(system).toContain("Always answer in French.");
  expect(system).toContain("release-notes");
  expect(tools!.map((t) => t.name)).toContain("skill");
  await session.close();
});

test("a tool of your own is offered and runs (the README example)", async () => {
  const clock: Tool = {
    name: "clock",
    description: "The current time.",
    input: z.object({}),
    label: () => "now",
    run: async () => ({ output: "12:00", summary: "12:00" }),
  };
  const provider = new ScriptedProvider([
    [{ type: "tool_call", call: { id: "c1", name: "clock", arguments: "{}" } }, { type: "done", reason: "tool_calls" }],
    say("It's noon."),
  ]);
  const session = await createSession({ cwd: root, provider, configDir, tools: [clock] });
  const events = await collect(session.send("what time is it?"));
  expect(provider.requests[0]!.options.tools!.map((t) => t.name)).toContain("clock");
  expect(events).toContainEqual(expect.objectContaining({ type: "tool_end", result: expect.objectContaining({ output: "12:00" }) }));
  expect(events.at(-1)).toMatchObject({ type: "turn_end", reason: "end" });
  await session.close();
});

test("persist: true saves where marv -r finds it, and resume picks it up", async () => {
  const first = await createSession({ cwd: root, provider: new ScriptedProvider([say("Noted.")]), configDir, persist: true });
  await collect(first.send("remember 42"));
  await first.close();
  const provider = new ScriptedProvider([say("42.")]);
  const second = await createSession({ cwd: root, provider, configDir, persist: true, resume: "latest" });
  expect(second.id).toBe(first.id);
  await collect(second.send("what was it?"));
  expect(provider.requests[0]!.history.map((t) => t.text)).toEqual(["remember 42", "Noted.", "what was it?"]);
  await second.close();
});

test("resuming an id that doesn't exist fails", async () => {
  await expect(createSession({ cwd: root, provider: new ScriptedProvider([]), configDir, persist: true, resume: "nope" })).rejects.toThrow(/no saved session/);
});

test("a bad MCP server entry is reported in problems, not thrown", async () => {
  const session = await createSession({ cwd: root, provider: new ScriptedProvider([]), configDir, mcpServers: { bad__name: { command: "x" } } });
  expect(session.problems.join("\n")).toContain("bad__name");
  await session.close();
});

test("${MARV_PROJECT_DIR} in a server given in code expands to the project", async () => {
  // An unset variable without a default is a problem, so no problem means it was expanded.
  const session = await createSession({ cwd: root, provider: new ScriptedProvider([]), configDir, mcpServers: { fs: { command: "true", args: ["${MARV_PROJECT_DIR}"] } } });
  expect(session.problems).toEqual([]);
  await session.close();
});

const FIXTURE = join(import.meta.dir, "fixtures", "mcp-server.ts");
/** Lines of `pgrep -f` for processes whose command line has this marker. */
function running(marker: string): string[] {
  return Bun.spawnSync(["pgrep", "-f", marker]).stdout.toString().split("\n").filter(Boolean);
}
async function gone(marker: string): Promise<boolean> {
  for (let i = 0; i < 50; i++) {
    if (running(marker).length === 0) return true;
    await Bun.sleep(100);
  }
  return false;
}

test("a session that fails to be created leaves no MCP server running (bad provider)", async () => {
  const marker = `leak-kind-${crypto.randomUUID()}`;
  const mcpServers = { fx: { command: process.execPath, args: [FIXTURE, marker] } };
  await expect(createSession({ cwd: root, provider: { kind: "nope" } as unknown as sdk.ProviderOption, configDir, mcpServers })).rejects.toThrow();
  await Bun.sleep(500);
  expect(running(marker)).toEqual([]);
});

test("an unknown resume id closes the MCP servers it started", async () => {
  const marker = `leak-resume-${crypto.randomUUID()}`;
  const mcpServers = { fx: { command: process.execPath, args: [FIXTURE, marker] } };
  await expect(createSession({ cwd: root, provider: new ScriptedProvider([]), configDir, persist: true, resume: "nope", mcpServers })).rejects.toThrow(/no saved session "nope"/);
  expect(await gone(marker)).toBe(true);
});

test("resume without persist, and a cwd that isn't a folder, fail before anything starts", async () => {
  await expect(createSession({ cwd: root, provider: new ScriptedProvider([]), configDir, resume: "latest" })).rejects.toThrow(/resume needs persist/);
  const file = join(root, "AGENTS.md");
  await expect(createSession({ cwd: file, provider: new ScriptedProvider([]), configDir })).rejects.toThrow(`cwd ${file} isn't a folder`);
  await expect(createSession({ cwd: join(root, "missing"), provider: new ScriptedProvider([]), configDir })).rejects.toThrow(/isn't a folder/);
});

test("a stdio entry may say type: 'stdio' (many .mcp.json files do)", async () => {
  const session = await createSession({ cwd: root, provider: new ScriptedProvider([]), configDir, mcpServers: { fs: { type: "stdio", command: "true" } } });
  expect(session.problems).toEqual([]);
  await session.close();
});

const mine = (name: string): Tool => ({
  name,
  description: "mine",
  input: z.object({}),
  label: () => name,
  run: async () => ({ output: "ran mine", summary: "mine" }),
});

test("a tool named like a built-in, another of yours, or an MCP tool is refused, and no MCP server is left running", async () => {
  const marker = `leak-clash-${crypto.randomUUID()}`;
  const mcpServers = { fx: { command: process.execPath, args: [FIXTURE, marker] } };
  const create = (tools: Tool[]) => createSession({ cwd: root, provider: new ScriptedProvider([]), configDir, tools, mcpServers });
  await expect(create([mine("bash")])).rejects.toThrow('A tool named "bash" is already built in: give yours another name.');
  await expect(create([mine("x"), mine("x")])).rejects.toThrow('Two tools are named "x".');
  await expect(create([mine("mcp__fx__echo")])).rejects.toThrow(/"mcp__" are for MCP servers' tools/);
  await Bun.sleep(500);
  expect(running(marker)).toEqual([]);
});

test("mcpTools: \"deferred\" offers tool_search and mcp_call, and names the server's tools in the first message (#14)", async () => {
  const marker = `deferred-${crypto.randomUUID()}`;
  const provider = new ScriptedProvider([[{ type: "text_delta", text: "Hi." }, { type: "done", reason: "stop" }]]);
  const session = await createSession({ cwd: root, provider, configDir, mcpTools: "deferred", mcpServers: { fx: { command: process.execPath, args: [FIXTURE, marker] } } });
  try {
    for await (const _ of session.send("hi"));
    const names = provider.requests[0]!.options.tools!.map((t) => t.name);
    expect(names).toContain("mcp_call");
    expect(names.filter((n) => n.startsWith("mcp__"))).toEqual([]);
    const message = provider.requests[0]!.history.at(-1)!.text;
    expect(message).toStartWith("Marv: MCP tools you can use");
    expect(message).toContain("- mcp__fx__echo: Echo the text back.");
    expect(message).toEndWith("\n\nhi");
  } finally {
    await session.close();
  }
});

// Yolo runs its safe edits before any approver is asked, so in the home folder (or above it) yolo is what's refused.
test("with yolo on, the home folder (or anything above it) isn't a project, approver or not", async () => {
  const provider = new ScriptedProvider([]);
  const asked: string[] = [];
  const approve = async (request: sdk.ApprovalRequest) => (asked.push(request.tool), "no" as const);
  await expect(createSession({ cwd: homedir(), provider, configDir })).rejects.toThrow(
    `cwd ${homedir()} is your home folder (or above it): with yolo on, Marv would change files there without asking, dotfiles included. Use a project folder, or pass yolo: false (then every change goes to your approver, or is refused without one).`,
  );
  await expect(createSession({ cwd: homedir(), provider, configDir, approve })).rejects.toThrow(/with yolo on/);
  await expect(createSession({ cwd: dirname(homedir()), provider, configDir, approve })).rejects.toThrow(/is your home folder \(or above it\): with yolo on/);
  await expect(createSession({ cwd: "/", provider, configDir, approve })).rejects.toThrow(/cwd \/ is the root of the file system: with yolo on/);
  expect(asked).toEqual([]);
});

/** A turn in the home folder that writes a file of its own; the file is removed whatever happens. */
async function writeInHome(options: Partial<sdk.SessionOptions>): Promise<{ events: SessionEvent[]; written: boolean }> {
  const name = `.marv-sdk-test-${crypto.randomUUID()}`;
  const target = join(homedir(), name);
  const provider = new ScriptedProvider([
    [{ type: "tool_call", call: { id: "w1", name: "write_file", arguments: JSON.stringify({ path: name, content: "x\n" }) } }, { type: "done", reason: "tool_calls" }],
    say("Done."),
  ]);
  try {
    const session = await createSession({ cwd: homedir(), provider, configDir, yolo: false, ...options });
    const events = await collect(session.send("write it"));
    await session.close();
    return { events, written: existsSync(target) };
  } finally {
    await rm(target, { force: true });
  }
}

test("with yolo: false the home folder is allowed: with no approver a change is refused", async () => {
  const { events, written } = await writeInHome({});
  expect(written).toBe(false);
  expect(events).toContainEqual(expect.objectContaining({ type: "tool_end", result: expect.objectContaining({ isError: true, output: expect.stringContaining("there's no one to ask") }) }));
});

test("with yolo: false and an approver, a change in the home folder is asked about", async () => {
  const asked: string[] = [];
  const { written } = await writeInHome({ approve: async (request) => (asked.push(request.tool), "no") });
  expect(asked).toEqual(["write_file"]);
  expect(written).toBe(false);
});

// That the refused configure() changes nothing is checked in tests/session.test.ts, away from the real home folder.
test("yolo can't be turned back on in the home folder", async () => {
  const session = await createSession({ cwd: homedir(), provider: new ScriptedProvider([]), configDir, yolo: false });
  expect(() => session.configure({ yolo: true })).toThrow(/with yolo on, Marv would change files there without asking/);
  await session.close();
});

test("the home check sees through symlinks", async () => {
  const home = join(root, "home");
  await mkdir(home);
  await symlink(home, join(root, "link"));
  expect(homeOrAbove(join(root, "link"), home)).toBe(true);
  expect(homeOrAbove(root, join(root, "link"))).toBe(true);
  expect(homeOrAbove(join(home, "project"), home)).toBe(false);
  expect(homeOrAbove(`${home}2`, home)).toBe(false);
});

test("onWarning hears what the user should know (a trajectory that can't be written)", async () => {
  const warnings: string[] = [];
  const trajectories = new sdk.TrajectoryStore(join(root, "AGENTS.md")); // a file, so its folders can't be made
  const session = await createSession({ cwd: root, provider: new ScriptedProvider([say("Hi.")]), configDir, trajectories, onWarning: (text) => warnings.push(text) });
  await collect(session.send("hi"));
  await session.flush();
  expect(warnings.join("\n")).toContain("Couldn't write the trajectory log");
  await session.close();
});

test.skipIf(!sandboxAvailable())("with no approver, a sandboxed command without network runs on its own", async () => {
  const provider = new ScriptedProvider([
    [{ type: "tool_call", call: { id: "b1", name: "bash", arguments: JSON.stringify({ command: "echo hi > out.txt" }) } }, { type: "done", reason: "tool_calls" }],
    say("Done."),
  ]);
  const session = await createSession({ cwd: root, provider, configDir });
  const events = await collect(session.send("write it"));
  expect(events).toContainEqual(expect.objectContaining({ type: "tool_end", result: expect.objectContaining({ approval: "auto" }) }));
  expect(await readFile(join(root, "out.txt"), "utf8")).toBe("hi\n");
  await session.close();
});

describe("MCP logins (#18)", () => {
  const lookup = (id: string): AgentEvent[] => [{ type: "tool_call", call: { id, name: "mcp__remote__lookup", arguments: JSON.stringify({ word: "w" }) } }, { type: "done", reason: "tool_calls" }];
  const toolResult = (provider: ScriptedProvider, i: number) => JSON.stringify(provider.requests[i]!.history.at(-1));

  test("a login stored in configDir is used, and refreshed", async () => {
    const fixture = oauthServer();
    try {
      // Logged in once (as /mcp login does in the TUI), into configDir's store.
      const tui = new McpManager([{ name: "remote", source: "personal", key: "k", transport: { type: "http", url: fixture.url } }], {
        root,
        version: "9.9.9",
        logins: new McpTokenStore(join(configDir, "mcp-tokens.json")),
      });
      await tui.start();
      await tui.login("remote", {
        authorize: (url) => void fetch(url, { redirect: "manual" }).then((r) => fetch(r.headers.get("location")!)),
      }).done;
      await tui.close();
      const before = fixture.paths().filter((p) => p === "/register" || p === "/authorize").length;

      const provider = new ScriptedProvider([lookup("a"), say("ok"), lookup("b"), say("ok again")]);
      const warnings: string[] = [];
      const session = await createSession({ cwd: root, provider, configDir, mcpServers: { remote: { type: "http", url: fixture.url } }, onWarning: (w) => warnings.push(w) });
      await collect(session.send("look"));
      expect(toolResult(provider, 1)).toContain("w: a word");
      fixture.expire();
      await collect(session.send("again"));
      expect(toolResult(provider, 3)).toContain("w: a word");
      expect(warnings).toEqual([]);
      expect(fixture.paths().filter((p) => p === "/register" || p === "/authorize")).toHaveLength(before);
      await session.close();
    } finally {
      fixture.close();
    }
  });

  test("a login refused mid-session: one warning, as at startup, however many calls fail", async () => {
    const fixture = oauthServer();
    try {
      const tui = new McpManager([{ name: "remote", source: "personal", key: "k", transport: { type: "http", url: fixture.url } }], {
        root,
        version: "9.9.9",
        logins: new McpTokenStore(join(configDir, "mcp-tokens.json")),
      });
      await tui.start();
      await tui.login("remote", {
        authorize: (url) => void fetch(url, { redirect: "manual" }).then((r) => fetch(r.headers.get("location")!)),
      }).done;
      await tui.close();

      const provider = new ScriptedProvider([lookup("a"), say("ok"), lookup("b"), say("ok again"), lookup("c"), say("still")]);
      const warnings: string[] = [];
      const session = await createSession({ cwd: root, provider, configDir, mcpTools: "eager", mcpServers: { remote: { type: "http", url: fixture.url } }, onWarning: (w) => warnings.push(w) });
      await collect(session.send("look"));
      expect(warnings).toEqual([]);
      fixture.expire();
      fixture.refuseRefresh();
      await collect(session.send("again"));
      expect(toolResult(provider, 3)).toContain("remote needs a new login");
      await collect(session.send("once more"));
      expect(warnings).toEqual([
        `MCP server "remote" needs a login: run marv in a terminal and /mcp login remote; logins are stored in ${join(configDir, "mcp-tokens.json")}`,
      ]);
      await session.close();
    } finally {
      fixture.close();
    }
  });

  test("without a login: one warning saying how to log in; nothing is registered or opened", async () => {
    const fixture = oauthServer();
    try {
      const provider = new ScriptedProvider([say("ok")]);
      const warnings: string[] = [];
      const session = await createSession({ cwd: root, provider, configDir, mcpTools: "eager", mcpServers: { remote: { type: "http", url: fixture.url } }, onWarning: (w) => warnings.push(w) });
      await collect(session.send("look"));
      expect(warnings).toEqual([
        `MCP server "remote" needs a login: run marv in a terminal and /mcp login remote; logins are stored in ${join(configDir, "mcp-tokens.json")}`,
      ]);
      // It never connected, so it has no tools to offer (one whose login runs out mid-session keeps its tools, and
      // their calls fail with "remote needs a new login: …": tests/mcp-login.test.ts).
      expect(provider.requests[0]!.options.tools!.map((t) => t.name).filter((n) => n.startsWith("mcp__"))).toEqual([]);
      expect(fixture.paths()).not.toContain("/register");
      expect(fixture.paths()).not.toContain("/authorize");
      await session.close();
    } finally {
      fixture.close();
    }
  });
});
