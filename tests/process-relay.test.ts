import { afterEach, describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, rmSync } from "node:fs";
import { connect, createServer, type Server } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { relaySource, type RelaySpec } from "../src/processes/relay.ts";
import { sandboxArgs, sandboxAvailable } from "../src/sandbox.ts";

const dirs: string[] = [];
const procs: Bun.Subprocess[] = [];
const servers: Server[] = [];
afterEach(() => {
  for (const proc of procs.splice(0)) proc.kill("SIGKILL");
  for (const server of servers.splice(0)) server.close();
  for (const dir of dirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

const temp = (prefix: string) => {
  const dir = mkdtempSync(join(tmpdir(), prefix));
  dirs.push(dir);
  return dir;
};

/** A sandbox (no network) running the relay with `spec`, with the socket folder at /run/marv. */
function sandboxed(spec: RelaySpec, sockets: string, writable: boolean) {
  const root = temp("relay-root-");
  const home = temp("relay-home-");
  const args = sandboxArgs({
    root,
    home,
    network: false,
    path: process.env.PATH ?? "/usr/bin:/bin",
    binds: [
      { source: process.execPath, dest: "/run/marv-bun" },
      { source: sockets, dest: "/run/marv", writable },
    ],
  });
  const proc = Bun.spawn(["bwrap", ...args, "--", "/run/marv-bun", "-e", relaySource(spec)], { stdout: "pipe", stderr: "pipe", stdin: "ignore" });
  procs.push(proc);
  return proc;
}

/** Writes `chunks` to the Unix socket (each its own write) and collects everything until it closes. */
function exchange(socket: string, chunks: string[]): Promise<string> {
  return new Promise((resolve) => {
    let got = "";
    const conn = connect({ path: socket }, () => {
      for (const chunk of chunks) conn.write(chunk);
    });
    conn.on("data", (d) => (got += d.toString()));
    conn.on("error", () => resolve(got));
    conn.on("close", () => resolve(got));
  });
}

const SERVER = `Bun.serve({ port: 4789, hostname: "127.0.0.1", fetch: (req) => new Response("hi from the sandbox " + new URL(req.url).pathname) }); setInterval(() => {}, 1000);`;

async function until(check: () => boolean | Promise<boolean>, ms = 10_000) {
  const end = Date.now() + ms;
  while (Date.now() < end) {
    if (await check()) return;
    await Bun.sleep(50);
  }
  throw new Error("timed out");
}

describe.if(sandboxAvailable())("the relay (in a real sandbox)", () => {
  test("forward mode pipes a socket connection to the sandbox's 127.0.0.1:<port>, keeping bytes sent before it connects", async () => {
    const sockets = temp("relay-s-");
    sandboxed({ listen: "/run/marv/fwd.sock", argv: ["/run/marv-bun", "-e", SERVER] }, sockets, true);
    const socket = join(sockets, "fwd.sock");
    await until(() => existsSync(socket));
    let reply = "";
    await until(async () => {
      // The port line and the request in one write, then more in a second one: all of it reaches the server.
      reply = await exchange(socket, ["4789\nGET /a", "bc HTTP/1.0\r\nHost: x\r\n\r\n"]);
      return reply.includes("200");
    });
    expect(reply).toContain("hi from the sandbox /abc");
  });

  test("a port line that isn't 1024–65535 (or a number) closes the connection", async () => {
    const sockets = temp("relay-s-");
    sandboxed({ listen: "/run/marv/fwd.sock", argv: ["/run/marv-bun", "-e", SERVER] }, sockets, true);
    const socket = join(sockets, "fwd.sock");
    await until(() => existsSync(socket));
    await until(async () => (await exchange(socket, ["4789\nGET / HTTP/1.0\r\n\r\n"])).includes("200"));
    for (const line of ["80", "0", "65536", "abc", "4789x"]) {
      expect(await exchange(socket, [`${line}\nGET / HTTP/1.0\r\n\r\n`])).toBe("");
    }
    // A first line that never ends.
    expect(await exchange(socket, ["9".repeat(40)])).toBe("");
  });

  test("reverse mode lets a sandboxed command's curl localhost:<port> reach a socket mounted read-only", async () => {
    const sockets = temp("relay-s-");
    mkdirSync(join(sockets, "p1"));
    const socket = join(sockets, "p1", "fwd.sock");
    let line = "";
    const server = createServer((conn) => {
      conn.once("data", (d) => {
        line = d.toString().split("\n")[0]!;
        conn.end("HTTP/1.0 200 OK\r\nContent-Length: 13\r\n\r\nhost says hi\n");
      });
    });
    servers.push(server);
    await new Promise<void>((resolve) => server.listen(socket, resolve));
    const proc = sandboxed({ connect: [{ port: 4789, socket: "/run/marv/p1/fwd.sock" }], argv: ["bash", "-c", "curl -s --max-time 5 localhost:4789; exit 7"] }, sockets, false);
    const out = await new Response(proc.stdout).text();
    expect(await proc.exited).toBe(7); // the command's exit code
    expect(out).toContain("host says hi");
    expect(line).toBe("4789");
  });
});

// The published CLI is a Bun.build bundle (scripts/release.ts): the relay's source is relayMain.toString() of the
// bundled function, so it must still run on its own after bundling (a bundler turns require() into __require).
test("the relay's source still runs after bundling, as in the published CLI", async () => {
  const dir = mkdtempSync(join(tmpdir(), "marv-relay-bundle-"));
  try {
    const built = await Bun.build({ entrypoints: [join(import.meta.dir, "..", "src", "processes", "relay.ts")], outdir: dir, target: "bun" });
    expect(built.success).toBe(true);
    const bundled = (await import(join(dir, "relay.js"))) as typeof import("../src/processes/relay.ts");
    const source = bundled.relaySource({ listen: join(dir, "r.sock"), argv: ["bash", "-c", "echo relay-ran"] });
    const run = Bun.spawnSync(["bun", "-e", source], { timeout: 10_000 });
    expect(run.stderr.toString()).toBe("");
    expect(run.stdout.toString()).toContain("relay-ran");
    expect(run.exitCode).toBe(0);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
