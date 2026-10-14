// Guard suite (issue #53): what a sandboxed command must never manage, with the real bubblewrap. Each test is a
// thing that must stay impossible however Marv's scope grows; each was checked to fail with its guard removed (#53).
// Skipped only where bwrap can't run; CI requires it.
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";
import type { ToolResult } from "../../src/tools/types.ts";
import { sandboxArgs, sandboxAvailable, TOOLCHAINS } from "../../src/sandbox.ts";
import { runCommand } from "../../src/tools/bash.ts";
import { runTool } from "../../src/tools/index.ts";
import type { ApprovalRequest } from "../../src/tools/types.ts";

const FIXTURE = join(import.meta.dir, "fixtures", "bash-tool.ts");

let root: string;
let asked: ApprovalRequest[];
beforeEach(async () => {
  root = await mkdtemp(join(tmpdir(), "marv-guard-"));
  asked = [];
});
afterEach(() => rm(root, { recursive: true, force: true }));

/** Marv's bash tool as yolo runs it; anything it asks about is recorded and approved. */
const bash = (args: Record<string, unknown>) =>
  runTool({ id: "g", name: "bash", arguments: JSON.stringify(args) }, {
    root,
    sandbox: true,
    yolo: true,
    approve: async (request) => {
      asked.push(request);
      return "yes";
    },
  });

describe.if(sandboxAvailable())("guard: a sandboxed command", () => {
  describe("can't read the home folder's secrets", () => {
    // A decoy home with every secret the sandbox must hide, the project inside it (the usual layout), and one
    // folder it must show (personal skills), so a missing file proves the hiding, not a wrong path. Not under /tmp:
    // the sandbox's private /tmp would hide it anyway, and the test would pass without the home being hidden.
    let decoys: string;
    let home: string;
    let project: string;
    const secrets: Record<string, string> = {
      ".ssh/id_ed25519": "DECOY-SSH-KEY",
      ".marv/config.json": '{"apiKey":"DECOY-MARV-KEY"}',
      ".marv/mcp-tokens.json": '{"DECOY-MCP-TOKEN":1}',
      ".config/gh/hosts.yml": "github.com:\n  oauth_token: DECOY-GH-TOKEN\n",
      "notes/private.txt": "DECOY-ANY-FILE",
    };
    beforeEach(async () => {
      decoys = await mkdtemp(join("/var/tmp", "marv-guard-home-"));
      home = join(decoys, "home");
      project = join(home, "proj");
      mkdirSync(project, { recursive: true });
      for (const [path, content] of Object.entries(secrets)) {
        mkdirSync(join(home, path, ".."), { recursive: true });
        writeFileSync(join(home, path), content);
      }
      mkdirSync(join(home, ".marv/skills/demo"), { recursive: true });
      writeFileSync(join(home, ".marv/skills/demo/SKILL.md"), "VISIBLE-SKILL");
    });
    afterEach(() => rm(decoys, { recursive: true, force: true }));

    const attempts = [
      ...Object.keys(secrets).map((path) => `cat ~/${path}`),
      'grep -rs DECOY "$HOME" 2>/dev/null', // anything anywhere under the home folder, by content
      "find ~ -maxdepth 4 2>/dev/null", // or by name
      "cat ~/.marv/skills/demo/SKILL.md", // what it may see
    ].join("; ");

    test("in the sandbox itself (runCommand with the decoy home)", async () => {
      const result = await runCommand({ command: `${attempts}; echo ran`, root: project, home, sandbox: true, network: false, timeoutMs: 20_000 });
      expect(result.output).toContain("ran");
      expect(result.output).toContain("VISIBLE-SKILL");
      expect(result.output).not.toContain("DECOY");
      // What `find` listed: the project and the way to the skill, nothing else.
      const listed = result.output.split("\n").filter((line) => line.startsWith(`${home}/`)).map((line) => line.slice(home.length + 1));
      expect(listed.length).toBeGreaterThan(0);
      for (const path of listed) expect(path === "proj" || ".marv/skills/demo/SKILL.md".startsWith(path)).toBe(true);
    });

    test("through the bash tool, with yolo running it unasked (HOME is the decoy home)", async () => {
      const proc = Bun.spawn([process.execPath, FIXTURE, project, `${attempts}; echo ran`], {
        env: { ...process.env, HOME: home },
        stdout: "pipe",
        stderr: "pipe",
      });
      const [stdout, stderr] = await Promise.all([new Response(proc.stdout).text(), new Response(proc.stderr).text()]);
      expect(await proc.exited, stderr).toBe(0);
      const result = JSON.parse(stdout) as ToolResult;
      expect(result.approval).toBe("auto");
      expect(result.output).toContain("ran");
      expect(result.output).toContain("VISIBLE-SKILL");
      expect(result.output).not.toContain("DECOY");
    });

    test("of the real home folder, only toolchain folders show through", async () => {
      // Nothing is written to the real home: whatever a command can see there must be a toolchain (or a folder on
      // the way to one), never anything else.
      const result = await bash({ command: 'find "$HOME" -mindepth 1 -maxdepth 3 2>/dev/null; echo ran' });
      expect(result.output).toContain("ran");
      const home = homedir();
      const seen = result.output.split("\n").filter((line) => line.startsWith(`${home}/`)).map((line) => line.slice(home.length + 1));
      for (const path of seen) {
        const allowed = TOOLCHAINS.some((dir) => path === dir || path.startsWith(`${dir}/`) || dir.startsWith(`${path}/`));
        expect(allowed, `${path} is visible in the sandbox`).toBe(true);
      }
    });
  });

  describe("can't reach the network without asking", () => {
    let server: ReturnType<typeof Bun.serve>;
    let hits = 0;
    beforeAll(() => {
      server = Bun.serve({ hostname: "127.0.0.1", port: 0, fetch: () => (hits++, new Response("REACHED")) });
    });
    afterAll(() => server.stop(true));
    const probe = () => `bun -e 'console.log(await (await fetch("http://127.0.0.1:${server.port}/", { signal: AbortSignal.timeout(3000) })).text())' 2>&1; echo exit=$?`;

    test("a command without network: true runs unasked, and can't connect to a server on this machine", async () => {
      hits = 0;
      const result = await bash({ command: probe() });
      expect(asked).toHaveLength(0);
      expect(result.output).toMatch(/exit=[1-9]/);
      expect(result.output).not.toContain("REACHED");
      expect(hits).toBe(0);
    });

    test("network: true asks first (and then it connects: the probe works)", async () => {
      hits = 0;
      const result = await bash({ command: probe(), network: true });
      expect(asked).toHaveLength(1);
      expect(asked[0]!.network).toBe(true);
      expect(result.output).toContain("REACHED");
      expect(hits).toBe(1);
    });
  });

  describe("can't change the project's .git (git runs its hooks and config outside the sandbox)", () => {
    test("a hook or config written unasked doesn't land", async () => {
      mkdirSync(join(root, ".git", "hooks"), { recursive: true });
      writeFileSync(join(root, ".git", "config"), "[core]\n");
      const result = await bash({ command: "echo evil > .git/hooks/pre-commit; echo '[core]\n\tfsmonitor = evil' >> .git/config; echo ran" });
      expect(result.approval).toBe("auto");
      expect(result.output).toContain("ran");
      expect(existsSync(join(root, ".git", "hooks", "pre-commit"))).toBe(false);
      expect(await readFile(join(root, ".git", "config"), "utf8")).toBe("[core]\n");
    });

    test("nor create one where there is none", async () => {
      const result = await bash({ command: "git init -q . 2>&1; mkdir -p .git/hooks 2>&1; echo ran" });
      expect(result.output).toContain("ran");
      expect(existsSync(join(root, ".git", "HEAD"))).toBe(false);
      expect(existsSync(join(root, ".git", "hooks"))).toBe(false);
    });
  });

  test("sees no host sockets under /run (D-Bus, GPG and SSH agents, keyring, docker)", async () => {
    const result = await bash({
      command: 'echo "run:[$(ls -A /run)]"; for s in /run/user/$(id -u)/bus /run/dbus/system_bus_socket /run/docker.sock; do test -S "$s" && echo "SOCKET $s"; done; echo ran',
    });
    expect(result.output).toContain("ran");
    expect(result.output).toContain("run:[]");
    expect(result.output).not.toContain("SOCKET");
  });

  describe("gets a cleared environment: Marv's API keys and tokens aren't visible", () => {
    const planted = { OPENROUTER_API_KEY: "sk-or-GUARD-SECRET", GITHUB_TOKEN: "ghp_GUARD-SECRET", MARV_GUARD_SECRET: "GUARD-SECRET" };
    beforeAll(() => Object.assign(process.env, planted));
    afterAll(() => {
      for (const key of Object.keys(planted)) delete process.env[key];
    });

    test("in the sandbox", async () => {
      // Two layers keep it out: bwrap starts from an empty environment (--clearenv), and runCommand hands bwrap
      // itself only PATH. The command's output catches both gone; the arguments catch --clearenv alone.
      expect(sandboxArgs({ root, home: homedir(), network: false, path: "/usr/bin" })).toContain("--clearenv");
      const result = await bash({ command: "env; echo ran" });
      expect(result.output).toContain("ran");
      expect(result.output).not.toContain("GUARD-SECRET");
    });

    test("and with the sandbox off (an approved command)", async () => {
      const result = await runTool({ id: "g", name: "bash", arguments: JSON.stringify({ command: "env; echo ran" }) }, { root, sandbox: false, yolo: true, approve: async () => "yes" });
      expect(result.output).toContain("ran");
      expect(result.output).not.toContain("GUARD-SECRET");
    });
  });
});
