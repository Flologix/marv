import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { createServer, type Server } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { listeningPorts, MAX_LOG_CHARS, nameFrom, parseNetTcp, ProcessManager } from "../src/processes.ts";
import { sandboxAvailable } from "../src/sandbox.ts";

const TCP = `  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:12AD 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 111 1 0000000079661a00 100 0 0 10 0
   1: 0100007F:0050 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 112 1 0000000079661a00 100 0 0 10 0
   2: 0100007F:12AD 0100007F:D431 01 00000000:00000000 00:00000000 00000000  1000        0 113 1 0000000079661a00 100 0 0 10 0
`;
const TCP6 = `  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000001000000:1F90 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 211 1 000000003b38aaa7 100 0 0 10 0
`;

describe("reading /proc/<pid>/net/tcp", () => {
  test("listening ports (state 0A), hex, v4 and v6, at least 1024, each once", () => {
    expect(parseNetTcp(TCP).map((r) => [r.port, r.listening])).toEqual([[4781, true], [80, true], [4781, false]]);
    expect(listeningPorts(TCP)).toEqual([4781]); // 80 is below 1024; the established row isn't listening
    expect(listeningPorts(TCP6)).toEqual([8080]);
    expect(listeningPorts(TCP + TCP6.split("\n").slice(1).join("\n"))).toEqual([4781, 8080]);
  });

  test("only the given sockets, for a sandbox on the host's network", () => {
    expect(listeningPorts(TCP + TCP6.split("\n").slice(1).join("\n"), new Set(["211"]))).toEqual([8080]);
  });
});

test("a name from the command", () => {
  expect(nameFrom("bun run dev")).toBe("bun-run-dev");
  expect(nameFrom("./node_modules/.bin/vite --port 5173 && echo")).toBe("vite---port-5173");
  expect(nameFrom("$$$")).toBe("process");
});

/** A free port on the host's 127.0.0.1 right now. */
async function freePort(): Promise<number> {
  const server = createServer();
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const port = (server.address() as { port: number }).port;
  await new Promise((resolve) => server.close(resolve));
  return port;
}

/** A command that serves `text` on each port, from inside the process's sandbox (Marv's bun is at /run/marv-bun). */
const serve = (ports: number[], text = "hello from the process") =>
  `echo starting; /run/marv-bun -e 'for (const port of ${JSON.stringify(ports)}) Bun.serve({ port, hostname: "127.0.0.1", fetch: () => new Response(${JSON.stringify(text)}) }); console.log("listening")'`;

const pgrep = (marker: string) => Bun.spawnSync(["pgrep", "-f", marker]).stdout.toString().trim();

describe.if(sandboxAvailable())("ProcessManager (in a real sandbox)", () => {
  let root: string;
  let home: string;
  let runtime: string;
  let manager: ProcessManager;
  const held: Server[] = [];
  beforeEach(() => {
    root = mkdtempSync(join(tmpdir(), "marv-proc-root-"));
    home = mkdtempSync(join(tmpdir(), "marv-proc-home-"));
    runtime = mkdtempSync(join(tmpdir(), "mp-"));
    manager = new ProcessManager({ root, home, runtimeDir: runtime, pollMs: 100, startWaitMs: 1000 });
  });
  afterEach(async () => {
    await manager.closeAll();
    for (const server of held.splice(0)) server.close();
    for (const dir of [root, home, runtime]) rmSync(dir, { recursive: true, force: true });
  });

  test("start returns the forwarded port and the first lines; the host's 127.0.0.1 gets the page; stop ends it", async () => {
    const port = await freePort();
    const started = await manager.start({ command: serve([port]) });
    expect(started.info.name).toBe("echo-starting");
    expect(started.info).toMatchObject({ state: "running", ports: [port] });
    expect(started.output).toContain("starting");
    expect(await (await fetch(`http://127.0.0.1:${port}/`)).text()).toBe("hello from the process");
    expect(manager.forwards()).toEqual([{ port, id: expect.any(String), dir: expect.any(String) }]);
    expect(manager.list()).toHaveLength(1);
    expect(await manager.stop(started.info.name)).toBe(true);
    expect(manager.list()).toEqual([]);
    await expect(fetch(`http://127.0.0.1:${port}/`)).rejects.toThrow();
    expect(await manager.stop(started.info.name)).toBe(false);
  });

  test("a forwarder that fails later is closed and its port dropped, with a note; Marv doesn't crash", async () => {
    const port = await freePort();
    const started = await manager.start({ command: serve([port]), name: "web" });
    expect(started.info.ports).toEqual([port]);
    const entry = (manager as unknown as { entries: Map<string, { listeners: Server[] }> }).entries.get("web")!;
    // With no handler, an 'error' on a net.Server throws: here it would take the test (and Marv) down.
    entry.listeners[0]!.emit("error", new Error("EMFILE: too many open files"));
    expect(entry.listeners).toEqual([]);
    const info = manager.list().find((p) => p.name === "web")!;
    expect(info.ports).toEqual([]);
    expect(info.notes.join()).toContain(`port ${port} stopped being forwarded (EMFILE: too many open files)`);
    await expect(fetch(`http://127.0.0.1:${port}/`)).rejects.toThrow();
  });

  test("a port taken on the host isn't forwarded, and the model is told", async () => {
    const port = await freePort();
    const blocker = createServer();
    held.push(blocker);
    await new Promise<void>((resolve) => blocker.listen(port, "127.0.0.1", resolve));
    const started = await manager.start({ command: serve([port]), name: "web" });
    expect(started.info.ports).toEqual([]);
    expect(started.info.notes.join()).toContain(`port ${port} is in use on the user's machine`);
  });

  test("at most 4 ports per process, 4 processes at once; names are unique", async () => {
    const ports = await Promise.all([1, 2, 3, 4, 5].map(() => freePort()));
    const many = await manager.start({ command: `${serve(ports)}; sleep 100`, name: "many" });
    await Bun.sleep(500);
    const info = manager.list().find((p) => p.name === "many")!;
    expect(info.ports).toHaveLength(4);
    expect(info.notes.join()).toContain("at most 4 ports per process");
    expect(many.info.state).toBe("running");
    const quick = new ProcessManager({ root, home, runtimeDir: runtime, startWaitMs: 200 });
    try {
      for (let i = 0; i < 4; i++) await quick.start({ command: "sleep 100" });
      expect(quick.list().map((p) => p.name)).toEqual(["sleep-100", "sleep-100-2", "sleep-100-3", "sleep-100-4"]);
      await expect(quick.start({ command: "sleep 100" })).rejects.toThrow("At most 4 processes");
      await expect(quick.start({ command: "true", name: "sleep-100" })).rejects.toThrow("already running");
    } finally {
      await quick.closeAll();
    }
  });

  test("two managers in one program (two SDK sessions) each get their own socket folder; closing one leaves the other's (Greptile on #71)", async () => {
    const [portA, portB] = await Promise.all([freePort(), freePort()]);
    // The same parent folder, as two sessions in one process get by default.
    const other = new ProcessManager({ root, home, runtimeDir: runtime, pollMs: 100, startWaitMs: 1000 });
    try {
      const a = await manager.start({ command: serve([portA], "from a") });
      const b = await other.start({ command: serve([portB], "from b") });
      expect(a.info.ports).toEqual([portA]);
      expect(b.info.ports).toEqual([portB]);
      expect(await (await fetch(`http://127.0.0.1:${portB}/`)).text()).toBe("from b");
      await manager.closeAll();
      expect(await (await fetch(`http://127.0.0.1:${portB}/`)).text()).toBe("from b");
    } finally {
      await other.closeAll();
    }
  });

  test("an exited process stays listed with its code and logs, until replaced", async () => {
    const started = await manager.start({ command: "echo bye; exit 3", name: "short" });
    expect(started.info).toMatchObject({ state: "exited", exitCode: 3 });
    expect(started.output).toBe("bye");
    expect(manager.logs("short")).toBe("bye");
    const again = await manager.start({ command: "echo again; exit 0", name: "short" });
    expect(again.output).toBe("again");
    expect(manager.list()).toHaveLength(1);
  });

  test("logs keep the last 1 MB, and logs(lines) is the tail", async () => {
    await manager.start({ command: "seq 1 300000; sleep 100", name: "noisy" });
    await Bun.sleep(1000);
    expect(manager.logs("noisy", 3)).toBe("299998\n299999\n300000");
    expect(manager.logs("noisy", 10_000).split("\n")).toHaveLength(400);
    // 300000 lines of up to 7 characters: about 2 MB written, at most 1 MB kept, starting at a whole line.
    const kept = (manager as unknown as { entries: Map<string, { log: string }> }).entries.get("noisy")!.log;
    expect(kept.length).toBeLessThanOrEqual(MAX_LOG_CHARS);
    expect(kept.length).toBeGreaterThan(MAX_LOG_CHARS - 10);
    expect(kept.startsWith("\n")).toBe(false);
    expect(() => manager.logs("nope")).toThrow('No process named "nope"');
  });

  test("stop and closeAll end everything the process started", async () => {
    const marker = `marv-proc-marker-${Date.now()}`;
    await manager.start({ command: `sleep 100 & sleep 101; echo ${marker}`, name: "a" });
    await manager.start({ command: `sleep 102; echo ${marker}`, name: "b" });
    expect(pgrep(marker)).not.toBe("");
    await manager.stop("a");
    expect(manager.list().map((p) => p.name)).toEqual(["b"]);
    await manager.closeAll();
    expect(pgrep(marker)).toBe("");
    await expect(manager.start({ command: "true" })).rejects.toThrow("shut down");
  });
});
