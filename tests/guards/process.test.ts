// Guard suite (#53) for the process door (#51): what a background process must never manage, with the real
// bubblewrap. Each was checked to fail with its guard removed (see the PR). Skipped only where bwrap can't run; CI
// requires it.
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { networkInterfaces, tmpdir } from "node:os";
import { join } from "node:path";
import { ProcessManager } from "../../src/processes.ts";
import { sandboxAvailable } from "../../src/sandbox.ts";
import { subagentContext } from "../../src/subagent.ts";
import { runTool } from "../../src/tools/index.ts";
import { NO_PROCESS_DOOR } from "../../src/tools/process.ts";
import type { ApprovalRequest, ToolContext } from "../../src/tools/types.ts";

const PARENT = join(import.meta.dir, "fixtures", "process-parent.ts");

async function freePort(): Promise<number> {
  const server = createServer();
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const port = (server.address() as { port: number }).port;
  await new Promise((resolve) => server.close(resolve));
  return port;
}

async function until(check: () => boolean, ms = 5_000): Promise<boolean> {
  const end = Date.now() + ms;
  while (Date.now() < end) {
    if (check()) return true;
    await Bun.sleep(50);
  }
  return check();
}

const pgrep = (marker: string) => Bun.spawnSync(["pgrep", "-f", marker]).stdout.toString().trim();

describe.if(sandboxAvailable())("guard: a background process", () => {
  // Not under /tmp: the sandbox's private /tmp would hide these anyway, and the tests would pass without the guards.
  let decoys: string;
  let home: string;
  let root: string;
  let outside: string;
  let runtime: string;
  let manager: ProcessManager;
  beforeEach(() => {
    decoys = mkdtempSync(join("/var/tmp", "marv-guard-proc-"));
    home = join(decoys, "home");
    root = join(home, "proj");
    outside = join(decoys, "outside");
    for (const dir of [root, outside, join(root, ".git", "hooks")]) mkdirSync(dir, { recursive: true });
    writeFileSync(join(home, "secret.txt"), "DECOY-HOME-SECRET");
    writeFileSync(join(root, "readme.txt"), "PROJECT-FILE");
    runtime = mkdtempSync(join(tmpdir(), "mpg-"));
    manager = new ProcessManager({ root, home, runtimeDir: runtime, pollMs: 100, startWaitMs: 1500 });
  });
  afterEach(async () => {
    await manager.closeAll();
    rmSync(decoys, { recursive: true, force: true });
    rmSync(runtime, { recursive: true, force: true });
  });

  /** Starts `command`, waits for it to end, and returns its logs. */
  async function output(command: string, network = false): Promise<string> {
    const { info } = await manager.start({ command: `${command}; echo END`, network });
    await until(() => manager.logs(info.name).includes("END"), 8_000);
    return manager.logs(info.name);
  }

  test("can't write outside the project (but can inside it)", async () => {
    await output(`echo x > ${outside}/planted; echo x > ${root}/made`);
    expect(existsSync(join(outside, "planted"))).toBe(false);
    expect(existsSync(join(root, "made"))).toBe(true); // control
  });

  test("can't change the project's .git", async () => {
    await output(`echo hook > .git/hooks/pre-commit`);
    expect(existsSync(join(root, ".git", "hooks", "pre-commit"))).toBe(false);
  });

  test("can't read a file in the real home folder (but can read the project)", async () => {
    const logs = await output(`cat ${home}/secret.txt; cat ${root}/readme.txt`);
    expect(logs).not.toContain("DECOY-HOME-SECRET");
    expect(logs).toContain("PROJECT-FILE"); // control
  });

  test("can't reach the host's 127.0.0.1 without network: true (and can with it)", async () => {
    const port = await freePort();
    const server = Bun.serve({ port, hostname: "127.0.0.1", fetch: () => new Response("HOST-SERVICE") });
    try {
      const curl = `curl -s --max-time 3 http://127.0.0.1:${port}/`;
      expect(await output(curl)).not.toContain("HOST-SERVICE");
      expect(await output(curl, true)).toContain("HOST-SERVICE"); // control
    } finally {
      server.stop(true);
    }
  });

  test("its server is reachable on the host's 127.0.0.1, not on another interface", async () => {
    const port = await freePort();
    const { info } = await manager.start({
      command: `/run/marv-bun -e 'Bun.serve({ port: ${port}, hostname: "0.0.0.0", fetch: () => new Response("PROC") })'`,
    });
    expect(info.ports).toEqual([port]);
    expect(await (await fetch(`http://127.0.0.1:${port}/`)).text()).toBe("PROC");
    const other = Object.values(networkInterfaces())
      .flat()
      .find((a) => a && a.family === "IPv4" && !a.internal)?.address;
    if (other) await expect(fetch(`http://${other}:${port}/`, { signal: AbortSignal.timeout(2000) })).rejects.toThrow();
  });

  test("dies on stop", async () => {
    const marker = `marv-guard-stop-${Date.now()}`;
    await manager.start({ command: `sleep 300; echo ${marker}`, name: "s" });
    expect(await until(() => pgrep(marker) !== "")).toBe(true);
    await manager.stop("s");
    expect(pgrep(marker)).toBe("");
  });

  test("dies when Marv is SIGKILLed", async () => {
    const marker = `marv-guard-kill-${Date.now()}`;
    const parent = Bun.spawn(["bun", PARENT, root, runtime, `sleep 300; echo ${marker}`], { stdout: "pipe", stderr: "inherit" });
    const reader = parent.stdout.getReader();
    const { value } = await reader.read();
    expect(new TextDecoder().decode(value)).toContain("started");
    expect(pgrep(marker)).not.toBe("");
    parent.kill("SIGKILL");
    await parent.exited;
    expect(await until(() => pgrep(marker) === "")).toBe(true);
  });

  describe("the door", () => {
    const start = (ctx: ToolContext) => runTool({ id: "g", name: "process", arguments: JSON.stringify({ action: "start", command: "sleep 100" }) }, ctx);

    test("start asks in yolo, and a no starts nothing", async () => {
      const asked: ApprovalRequest[] = [];
      const result = await start({ root, yolo: true, doors: { process: true }, processes: manager, approve: async (r) => (asked.push(r), "no") });
      expect(asked).toHaveLength(1);
      expect(asked[0]).toMatchObject({ door: "process", scope: { key: "process:sleep 100" } });
      expect(result.declined).toBe(true);
      expect(manager.list()).toEqual([]);
    });

    test("a subagent's context can't start one, even when the parent's could: refused before any ask", async () => {
      const asked: ApprovalRequest[] = [];
      const parent: ToolContext = { root, yolo: true, doors: { process: true }, processes: manager, approve: async (r) => (asked.push(r), "yes") };
      const control = await start(parent); // the parent can
      expect(control.isError).toBeFalsy();
      await manager.closeAll();
      manager = new ProcessManager({ root, home, runtimeDir: runtime, startWaitMs: 300 });
      const result = await start(subagentContext({ ...parent, processes: manager }, "general-purpose · x"));
      expect(result.output).toBe(NO_PROCESS_DOOR);
      expect(asked).toHaveLength(1); // only the parent's
      expect(manager.list()).toEqual([]);
    });
  });
});
