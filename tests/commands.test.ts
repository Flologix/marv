import { describe, expect, test } from "bun:test";
import { GENERAL_PURPOSE } from "../src/agents.ts";
import { isCommand, runCommand as run, type CommandContext } from "../src/commands/index.ts";

const ctx: CommandContext = {
  config: {
    provider: "openrouter",
    model: "z-ai/glm-5.3",
    baseUrl: "https://openrouter.ai/api/v1",
    apiKey: "sk-or-v1-abcdefghijklmnop-wxyz",
    apiKeySource: "env",
    thinking: false,
    contextLength: 32768,
    sandbox: true,
    yolo: true,
    trajectories: true,
    diagnostics: true,
    checks: true,
    checkpoints: true,
    mcpTools: "auto",
    maxSteps: 25,
  },
  configPath: "~/.marv/config.json",
};
const runCommand = (input: string) => run(input, ctx);

describe("slash commands", () => {
  test("detects commands by leading slash", () => {
    expect(isCommand("/help")).toBe(true);
    expect(isCommand("hello /help")).toBe(false);
  });

  test("/help lists every command", () => {
    const action = runCommand("/help");
    expect(action.type).toBe("print");
    if (action.type !== "print") return;
    expect(action.text).toContain("/help");
    expect(action.text).toContain("/clear");
    expect(action.text).toContain("/exit");
  });

  test("/clear and /exit map to actions, case-insensitively", () => {
    expect(runCommand("/clear")).toEqual({ type: "clear" });
    expect(runCommand("/EXIT")).toEqual({ type: "exit" });
  });

  test("/config shows a masked key and where it came from", () => {
    const action = runCommand("/config");
    if (action.type !== "print") throw new Error("expected print");
    expect(action.text).toContain("Provider:  OpenRouter");
    expect(action.text).toContain("Endpoint:  https://openrouter.ai/api/v1");
    expect(action.text).toContain("sk-or-v1-a…wxyz (from OPENROUTER_API_KEY)");
    expect(action.text).not.toContain("abcdefghijklmnop");
  });

  test("/config says when no key is needed (Ollama)", () => {
    const action = run("/config", { ...ctx, config: { provider: "ollama", model: "qwen3.5:9b", baseUrl: "http://localhost:11434", thinking: false, contextLength: 32768, sandbox: true, yolo: true, trajectories: true, diagnostics: true, checks: true, checkpoints: true, mcpTools: "auto", maxSteps: 25 } });
    expect(action).toMatchObject({ type: "print", text: expect.stringContaining("API key:   not needed") });
  });

  test("/model opens the picker, or switches straight to a given id", () => {
    expect(runCommand("/model")).toEqual({ type: "model" });
    expect(runCommand("/model openai/gpt-5.6-sol")).toEqual({ type: "model", id: "openai/gpt-5.6-sol" });
  });

  test("/sandbox shows the sandbox, or turns it on and off", () => {
    expect(runCommand("/sandbox off")).toEqual({ type: "sandbox", on: false });
    expect(runCommand("/sandbox on")).toEqual({ type: "sandbox", on: true });
    expect(runCommand("/sandbox")).toMatchObject({ type: "print", text: expect.stringMatching(/^Sandbox: on/) });
    expect(runCommand("/sandbox maybe")).toMatchObject({ type: "print", isError: true });
  });

  test("/sandbox lists PATH folders the sandbox hides, if there are any", () => {
    const action = run("/sandbox", { ...ctx, hiddenPathFolders: ["~/.asdf/shims"] });
    if (action.type !== "print") throw new Error("expected print");
    expect(action.text).toContain("Your PATH has 1 folder the sandbox can't see:");
    expect(action.text).toContain("~/.asdf/shims");
  });

  test("/yolo toggles, or takes on/off", () => {
    expect(runCommand("/yolo")).toEqual({ type: "yolo", on: false }); // on by default
    expect(run("/yolo", { ...ctx, config: { ...ctx.config, yolo: false } })).toEqual({ type: "yolo", on: true });
    expect(runCommand("/yolo on")).toEqual({ type: "yolo", on: true });
    expect(runCommand("/yolo maybe")).toMatchObject({ type: "print", isError: true });
    expect(runCommand("/config")).toMatchObject({ type: "print", text: expect.stringContaining("Yolo:      on") });
  });

  test("/good and /bad rate the last turn, with an optional note; /label tags it", () => {
    expect(runCommand("/good")).toEqual({ type: "feedback", score: 1 });
    expect(runCommand("/bad  edited the wrong file ")).toEqual({ type: "feedback", score: -1, note: "edited the wrong file" });
    expect(runCommand("/label refactor, flaky-test  ui")).toEqual({ type: "feedback", score: 0, labels: ["refactor", "flaky-test", "ui"] });
    expect(runCommand("/label")).toMatchObject({ type: "print", isError: true });
  });

  test("/trajectories shows where runs are logged, or turns logging on and off", () => {
    expect(runCommand("/trajectories off")).toEqual({ type: "trajectories", on: false });
    expect(runCommand("/trajectories on")).toEqual({ type: "trajectories", on: true });
    expect(run("/trajectories", { ...ctx, trajectoriesPath: "~/.marv/trajectories/-p" })).toMatchObject({
      type: "print",
      text: expect.stringContaining("on: every turn is logged to ~/.marv/trajectories/-p"),
    });
    expect(runCommand("/trajectories maybe")).toMatchObject({ type: "print", isError: true });
    expect(runCommand("/config")).toMatchObject({ type: "print", text: expect.stringContaining("Trajectories: on") });
  });

  test("/checks lists the project's checks, what couldn't load, and the last result; or turns them on and off", () => {
    expect(runCommand("/checks off")).toEqual({ type: "checks", on: false });
    expect(runCommand("/checks on")).toEqual({ type: "checks", on: true });
    expect(runCommand("/checks maybe")).toMatchObject({ type: "print", isError: true });
    expect(runCommand("/checks")).toMatchObject({ type: "print", markdown: true, text: expect.stringContaining("This project has none") });
    const action = run("/checks", {
      ...ctx,
      checks: {
        list: [
          { kind: "pattern", name: "imports", file: ".marv/checks/imports.yaml", glob: ["src/**/*.ts"], exclude: [], forbid: "x", flags: "", message: "relative import without its extension" },
          { kind: "script", name: "prompt-cache", file: ".marv/checks/prompt-cache.ts", glob: ["src/**/*.ts"], exclude: ["src/gen/**"] },
        ],
        problems: [".marv/checks/bad.yaml: needs a glob"],
        last: { status: "done", ms: 120, before: 0, errors: 1, added: [{ check: "imports", file: "src/a.ts", line: 3, message: "relative import without its extension\u001b]52;c;x\u0007" }] },
      },
    });
    if (action.type !== "print") throw new Error("expected print");
    expect(action.text).toContain("`imports` *(forbids a pattern: relative import without its extension)*");
    expect(action.text).toContain("`prompt-cache` *(script)*\n  src/**/*.ts · not src/gen/**");
    expect(action.text).toContain("- .marv/checks/bad.yaml: needs a glob");
    expect(action.text).toContain("**Last result:** 1 new (1 violation in all, 120 ms):\n- src/a.ts:3 [imports] relative import without its extension");
    expect(action.text).not.toContain("\u001b");
    const quiet = run("/checks", { ...ctx, config: { ...ctx.config, checks: false }, checks: { list: [], problems: [], last: { status: "off", reason: "no-sandbox" } } });
    if (quiet.type !== "print") throw new Error("expected print");
    expect(quiet.text).toStartWith("**Checks**: off.");
    expect(quiet.text).toEndWith("**Last result:** off: they run only in the sandbox.");
  });

  test("/undo, and /checkpoints says what it does (and the last failure), or turns it on and off", () => {
    expect(runCommand("/undo")).toEqual({ type: "undo" });
    expect(runCommand("/undo 3")).toMatchObject({ type: "print", isError: true });
    expect(runCommand("/checkpoints off")).toEqual({ type: "checkpoints", on: false });
    expect(runCommand("/checkpoints on")).toEqual({ type: "checkpoints", on: true });
    expect(runCommand("/checkpoints")).toMatchObject({ type: "print", text: expect.stringContaining("/undo") });
    expect(run("/checkpoints", { ...ctx, checkpoints: { failure: "disk full" } })).toMatchObject({ text: expect.stringContaining("disk full") });
    expect(run("/checkpoints", { ...ctx, config: { ...ctx.config, checkpoints: false } })).toMatchObject({ text: "Checkpoints: off" });
    expect(runCommand("/checkpoints maybe")).toMatchObject({ type: "print", isError: true });
  });

  test("/diagnostics says what it does, or turns it on and off", () => {
    expect(runCommand("/diagnostics off")).toEqual({ type: "diagnostics", on: false });
    expect(runCommand("/diagnostics on")).toEqual({ type: "diagnostics", on: true });
    expect(runCommand("/diagnostics")).toMatchObject({ type: "print", text: expect.stringContaining("typecheck") });
    expect(runCommand("/diagnostics maybe")).toMatchObject({ type: "print", isError: true });
    expect(runCommand("/config")).toMatchObject({ type: "print", text: expect.stringContaining("Diagnostics: on") });
  });

  test("/steps shows the setting, or sets and saves it", () => {
    expect(runCommand("/steps")).toMatchObject({ type: "print", text: expect.stringContaining("after 25 steps") });
    expect(runCommand("/steps 40")).toEqual({ type: "steps", every: 40 });
    expect(runCommand("/steps off")).toEqual({ type: "steps", every: 0 });
    expect(runCommand("/steps 0")).toEqual({ type: "steps", every: 0 });
    expect(runCommand("/STEPS 50")).toEqual({ type: "steps", every: 50 });
    // A number too big for the config (it would be saved as null and break loading) is refused.
    for (const bad of ["maybe", "4.5", "-3", "1e2", "1 2", "1", "9".repeat(400)]) {
      expect(runCommand(`/steps ${bad}`)).toMatchObject({ type: "print", isError: true });
    }
    const show = run("/steps", { ...ctx, config: { ...ctx.config, maxSteps: 0 } });
    expect(show).toMatchObject({ type: "print", text: expect.stringContaining("never asks") });
  });

  test("/mcp lists servers, their state and tools, or explains how to add one; /mcp trust starts the project's", () => {
    expect(runCommand("/mcp")).toMatchObject({ type: "print", text: expect.stringContaining(".mcp.json") });
    const listed = run("/mcp", {
      ...ctx,
      mcp: {
        servers: [
          { name: "gh", source: "personal", target: "https://api.example.com/mcp", reads: [], runsProjectFiles: [], state: "connected", tools: ["mcp__gh__search", "mcp__gh__create_issue"] },
          { name: "files", source: "project", target: "node mcp/files.js", reads: ["FILES_TOKEN"], runsProjectFiles: ["mcp/files.js"], state: "untrusted", tools: [] },
          { name: "db", source: "personal", target: "db-mcp", reads: [], runsProjectFiles: [], state: "failed", tools: [], error: "spawn db-mcp ENOENT", stderr: "boom" },
        ],
        problems: ["\"old\" in ~/.marv/mcp.json: the old sse transport isn't supported"],
      },
    });
    if (listed.type !== "print") throw new Error("expected print");
    expect(listed.markdown).toBe(true);
    expect(listed.text).toContain("- **gh** (yours): connected · 2 tools: `search`, `create_issue`");
    expect(listed.text).toContain(
      "- **files** (this project): not trusted yet: `node mcp/files.js`; reads `$FILES_TOKEN` from your environment; runs this project's `mcp/files.js` (`/mcp trust` to start it)",
    );
    expect(listed.text).toContain("- **db** (yours): failed: spawn db-mcp ENOENT");
    expect(listed.text).toContain("sse transport");
    expect(runCommand("/mcp trust")).toEqual({ type: "mcp-trust" });
    expect(runCommand("/mcp nope")).toMatchObject({ type: "print", isError: true });
  });

  test("/mcp add, search and remove", () => {
    expect(runCommand("/mcp add x")).toEqual({ type: "mcp-add", args: "x" });
    expect(runCommand('/mcp add weather --env KEY --header "A: Bearer ${T}"')).toEqual({ type: "mcp-add", args: 'weather --env KEY --header "A: Bearer ${T}"' });
    expect(runCommand("/mcp search a b")).toEqual({ type: "mcp-search", words: "a b" });
    expect(runCommand("/mcp remove x")).toEqual({ type: "mcp-remove", name: "x" });
    for (const bad of ["/mcp add", "/mcp search", "/mcp remove", "/mcp remove a b", "/mcp trust x"]) {
      const action = runCommand(bad);
      expect(action).toMatchObject({ type: "print", isError: true });
      expect((action as { text: string }).text).toContain("/mcp add <npm package");
      expect((action as { text: string }).text).toContain("/mcp search <words>");
      expect((action as { text: string }).text).toContain("/mcp remove <name>");
    }
    expect(runCommand("/mcp")).toMatchObject({ text: expect.stringContaining("/mcp add") });
    const removed = run("/mcp", {
      ...ctx,
      mcp: { servers: [{ name: "gone", source: "personal", target: "npx -y x@1.0.0", reads: [], runsProjectFiles: [], state: "removed", tools: ["mcp__gone__a"] }], problems: [] },
    });
    expect(removed).toMatchObject({ text: expect.stringContaining("- **gone** (yours): removed") });
  });

  test("/mcp login and logout (#18): an action for a server that can log in, a usage error naming them otherwise", () => {
    const base = { source: "personal" as const, reads: [], runsProjectFiles: [] };
    const withServers = {
      ...ctx,
      mcp: {
        servers: [
          { ...base, name: "github", target: "https://api.example.com/mcp", state: "connected" as const, tools: ["mcp__github__search"], login: { loggedIn: true, host: "github.com" } },
          { ...base, name: "linear", target: "https://mcp.linear.test/mcp", state: "needs-login" as const, tools: [], login: { loggedIn: false } },
          { ...base, name: "files", target: "npx -y files", state: "connected" as const, tools: [] },
        ],
        problems: [],
      },
    };
    expect(run("/mcp login linear", withServers)).toEqual({ type: "mcp-login", server: "linear" });
    expect(run("/mcp logout github", withServers)).toEqual({ type: "mcp-logout", server: "github" });
    for (const bad of ["/mcp login", "/mcp login files", "/mcp login nope", "/mcp logout", "/mcp login a b"]) {
      const action = run(bad, withServers);
      expect(action).toMatchObject({ type: "print", isError: true });
      expect((action as { text: string }).text).toContain("`github`, `linear`");
    }
    // No server can log in: say what can.
    expect(run("/mcp login x", ctx)).toMatchObject({ type: "print", isError: true, text: expect.stringContaining("remote (http)") });
    expect(runCommand("/mcp nope")).toMatchObject({ text: expect.stringContaining("/mcp login <name>") });

    const listed = run("/mcp", withServers);
    if (listed.type !== "print") throw new Error("expected print");
    expect(listed.text).toContain("- **github** (yours): connected · 1 tool: `search` · logged in at github.com (renewed automatically)");
    expect(listed.text).toContain("- **linear** (yours): needs login: `/mcp login linear`");
    expect(listed.text).not.toContain("**files** (yours): connected · 0 tools · logged");
  });

  test("/skills lists skills, or explains how to add one", () => {
    const review = { name: "review", description: "Review code.", body: "b", dir: "/x", files: [], source: "project" as const };
    const listed = run("/skills", { ...ctx, skills: [review], skillProblems: ["bad/SKILL.md needs a description"] });
    if (listed.type !== "print") throw new Error("expected print");
    expect(listed.markdown).toBe(true); // rendered as a list, with hanging indents
    expect(listed.text).toContain("- `/review`: Review code.");
    expect(listed.text).toContain("Couldn't load:");
    expect(run("/skills", ctx)).toMatchObject({ type: "print", text: expect.stringContaining("SKILL.md") });
  });

  test("/<skill> runs that skill with the rest as its request; built-in commands win", () => {
    const review = { name: "review", description: "d", body: "b", dir: "/x", files: [], source: "project" as const };
    const help = { ...review, name: "help" };
    expect(run("/review src/app.tsx now", { ...ctx, skills: [review] })).toEqual({ type: "skill", skill: review, args: "src/app.tsx now" });
    expect(run("/help", { ...ctx, skills: [help] }).type).toBe("print");
  });

  test("/remember saves to personal memory, or to the project with project:", () => {
    expect(runCommand("/remember uses bun, not npm")).toEqual({ type: "remember", scope: "personal", text: "uses bun, not npm" });
    expect(runCommand("/remember project: tests need Ollama")).toEqual({ type: "remember", scope: "project", text: "tests need Ollama" });
    expect(runCommand("/remember")).toMatchObject({ type: "print", isError: true });
    expect(runCommand("/forget ollama")).toEqual({ type: "forget", text: "ollama" });
    expect(runCommand("/memory")).toEqual({ type: "memory" });
  });

  test("/think toggles, or takes on/off", () => {
    expect(runCommand("/think")).toEqual({ type: "thinking", on: true });
    expect(run("/think", { ...ctx, config: { ...ctx.config, thinking: true } })).toEqual({ type: "thinking", on: false });
    expect(runCommand("/think off")).toEqual({ type: "thinking", on: false });
    expect(runCommand("/think maybe")).toMatchObject({ type: "print", isError: true });
  });

  test("/setup opens the setup screen", () => {
    expect(runCommand("/setup")).toEqual({ type: "setup" });
  });

  test("unknown commands return an error", () => {
    const action = runCommand("/nope");
    expect(action).toMatchObject({ type: "print", isError: true });
  });
  test("/agents lists agent types and files that couldn't be loaded", () => {
    const reviewer = { ...GENERAL_PURPOSE, name: "code-reviewer", description: "Review a step.", tools: ["read_file", "grep"], model: "small", source: "personal" as const };
    const action = run("/agents", { ...ctx, agents: [GENERAL_PURPOSE, reviewer], agentProblems: ["~/.marv/agents/bad.md needs a description"] });
    expect(action).toMatchObject({ type: "print", markdown: true });
    const text = (action as { text: string }).text;
    expect(text).toContain("`general-purpose` *(built-in)*");
    expect(text).toContain("`code-reviewer` *(personal)*: Review a step.");
    expect(text).toContain("tools: read_file, grep · model: small");
    expect(text).toContain("bad.md needs a description");
  });
});

test("/mcp with a name an object inherits (__proto__, constructor) is a usage error, not a crash", () => {
  for (const sub of ["__proto__", "constructor", "toString", "hasOwnProperty"]) {
    expect(runCommand(`/mcp ${sub}`)).toMatchObject({ type: "print", isError: true });
  }
});
