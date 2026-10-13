// Background processes (#51): servers, watchers, anything that should keep running after the bash command that
// would have started it. Each runs in its own long-lived bwrap sandbox, as a bash command does (only the project
// writable, an empty home, no environment, .git read-only), but with its own network namespace kept: its ports are
// forwarded to the host's 127.0.0.1, one by one, and nothing comes back. Sharing the host's network instead would
// give every server outbound network and reach the user's own local services (databases, other dev servers).
//
// The forwarding goes through a Unix socket, the one thing that crosses a network namespace: the sandbox's relay
// (src/processes/relay.ts) listens on a socket in a folder bind-mounted at /run/marv; Marv reads the sandbox's
// listening ports from /proc/<pid>/net/tcp{,6} and, for each, listens on the same port of the host's 127.0.0.1 and
// pipes every connection into the socket.
import { mkdirSync, readdirSync, readFileSync, readlinkSync, rmSync } from "node:fs";
import { rmdir } from "node:fs/promises";
import { connect, createServer, type Server, type Socket } from "node:net";
import { constants, homedir, tmpdir } from "node:os";
import { join } from "node:path";
import { relaySource } from "./processes/relay.ts";
import { sandboxArgs } from "./sandbox.ts";
import { gitGuard } from "./tools/bash.ts";
import { ToolError } from "./tools/types.ts";

export const MAX_PROCESSES = 4;
export const MAX_PORTS = 4;
/** Logs kept per process (stdout and stderr together); older lines are dropped. */
export const MAX_LOG_CHARS = 1_000_000;
/** Ports below this are never forwarded. */
export const MIN_PORT = 1024;
const START_WAIT_MS = 10_000;
const POLL_MS = 500;
const STOP_GRACE_MS = 2_000;
const START_LINES = 40;
export const MAX_LOG_LINES = 400;

export interface ProcessInfo {
  name: string;
  command: string;
  /** Started with network: it shares the host's network, so its ports are the host's (nothing is forwarded). */
  network: boolean;
  state: "running" | "exited";
  /** When it exited: its code (128 + n when a signal ended it). */
  exitCode: number | null;
  /** Ports the user can open on their 127.0.0.1 (forwarded, or the host's own with network). */
  ports: number[];
  /** What wasn't forwarded, and why ("port 4789 is in use on the user's machine…"). */
  notes: string[];
  startedAt: number;
}

export interface StartResult {
  info: ProcessInfo;
  /** The first lines of its output. */
  output: string;
}

/** For a sandboxed bash command: the running processes' forwarded ports, and where each one's socket folder is. */
export interface ProcessForward {
  port: number;
  /** A short id, for the folder's mount point (/run/marv/<id>). */
  id: string;
  /** The socket folder on the host. */
  dir: string;
}

interface Entry {
  id: string;
  info: ProcessInfo;
  proc: Bun.Subprocess<"ignore", "pipe", "pipe">;
  dir: string;
  log: string;
  listeners: Server[];
  connections: Set<Socket>;
  /** Ports already handled (forwarded, refused or over the limit), so each is tried once. */
  seen: Set<number>;
  timer?: ReturnType<typeof setInterval>;
  exited: Promise<void>;
  placeholders: string[];
}

export interface ProcessManagerOptions {
  root: string;
  /** The home folder the sandbox hides (default: the user's); tests pass a fake one. */
  home?: string;
  /** Where the socket folders go (default: $XDG_RUNTIME_DIR/marv/<pid>). */
  runtimeDir?: string;
  /** Something changed (started, exited, a port forwarded): for the status bar. */
  onChange?: () => void;
  pollMs?: number;
  startWaitMs?: number;
  stopGraceMs?: number;
}

/** One line of /proc/<pid>/net/tcp{,6}: the local port, whether it's listening, and the socket's inode. */
export function parseNetTcp(text: string): { port: number; listening: boolean; inode: string }[] {
  const rows: { port: number; listening: boolean; inode: string }[] = [];
  for (const line of text.split("\n").slice(1)) {
    const fields = line.trim().split(/\s+/);
    // sl local_address rem_address st tx:rx tr:when retrnsmt uid timeout inode
    if (fields.length < 10) continue;
    const port = Number.parseInt(fields[1]!.split(":")[1] ?? "", 16);
    if (!Number.isFinite(port)) continue;
    rows.push({ port, listening: fields[3] === "0A", inode: fields[9]! });
  }
  return rows;
}

/** The listening ports in a net/tcp text (only sockets in `inodes` when given), forwardable ones, sorted. */
export function listeningPorts(text: string, inodes?: Set<string>): number[] {
  const ports = parseNetTcp(text)
    .filter((row) => row.listening && row.port >= MIN_PORT && (!inodes || inodes.has(row.inode)))
    .map((row) => row.port);
  return [...new Set(ports)].sort((a, b) => a - b);
}

const read = (path: string) => {
  try {
    return readFileSync(path, "utf8");
  } catch {
    return "";
  }
};
const link = (path: string) => {
  try {
    return readlinkSync(path);
  } catch {
    return null;
  }
};

/** The sandbox's first process (bwrap's child, in the sandbox's namespaces), or null before it exists. */
function sandboxPid(bwrapPid: number): number | null {
  const first = read(`/proc/${bwrapPid}/task/${bwrapPid}/children`).trim().split(/\s+/)[0];
  return first ? Number(first) : null;
}

/** The socket inodes of every process in the sandbox's PID namespace (for a sandbox on the host's network). */
function sandboxSockets(pid: number): Set<string> {
  const inodes = new Set<string>();
  const ns = link(`/proc/${pid}/ns/pid`);
  if (!ns) return inodes;
  let pids: string[] = [];
  try {
    pids = readdirSync("/proc").filter((p) => /^\d+$/.test(p));
  } catch {
    return inodes;
  }
  for (const p of pids) {
    if (link(`/proc/${p}/ns/pid`) !== ns) continue;
    let fds: string[] = [];
    try {
      fds = readdirSync(`/proc/${p}/fd`);
    } catch {
      continue;
    }
    for (const fd of fds) {
      const match = /^socket:\[(\d+)\]$/.exec(link(`/proc/${p}/fd/${fd}`) ?? "");
      if (match) inodes.add(match[1]!);
    }
  }
  return inodes;
}

/** A name from the command: its words joined (`bun run dev` → `bun-run-dev`), short. */
export function nameFrom(command: string): string {
  const words = command.split(/[;&|\n]/)[0]!.trim().split(/\s+/).map((w) => w.replace(/^.*\//, "").replace(/[^A-Za-z0-9._-]/g, ""));
  const name = words.filter(Boolean).join("-").slice(0, 32).replace(/-+$/, "");
  return name || "process";
}

const NAME = /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/;

export class ProcessManager {
  private readonly entries = new Map<string, Entry>();
  private count = 0;
  private closed = false;
  private readonly runtimeDir: string;

  constructor(private readonly options: ProcessManagerOptions) {
    // A short path: a Unix socket's path is limited to 107 bytes.
    this.runtimeDir = options.runtimeDir ?? join(process.env.XDG_RUNTIME_DIR || join(tmpdir(), `marv-${process.getuid?.() ?? "user"}`), "marv", String(process.pid));
  }

  list(): ProcessInfo[] {
    return [...this.entries.values()].map((e) => ({ ...e.info, ports: [...e.info.ports], notes: [...e.info.notes] }));
  }

  get running(): number {
    return [...this.entries.values()].filter((e) => e.info.state === "running").length;
  }

  forwards(): ProcessForward[] {
    return [...this.entries.values()]
      .filter((e) => e.info.state === "running" && !e.info.network)
      .flatMap((e) => e.info.ports.map((port) => ({ port, id: e.id, dir: e.dir })));
  }

  /** Its last `lines` lines of output (at most MAX_LOG_LINES). */
  logs(name: string, lines = 50): string {
    const entry = this.entries.get(name);
    if (!entry) throw new ToolError(this.unknown(name));
    const all = entry.log.replace(/\n$/, "").split("\n");
    return all.slice(-Math.min(Math.max(1, lines), MAX_LOG_LINES)).join("\n");
  }

  async start({ command, name, network = false }: { command: string; name?: string; network?: boolean }): Promise<StartResult> {
    if (this.closed) throw new ToolError("Processes are shut down: Marv is closing.");
    if (name !== undefined && !NAME.test(name)) throw new ToolError(`A process name uses letters, digits, ., - and _ (at most 64), starting with a letter or digit: "${name}".`);
    const taken = name !== undefined ? this.entries.get(name) : undefined;
    if (taken?.info.state === "running") throw new ToolError(`A process named "${name}" is already running: stop it first, or pick another name.`);
    if (this.running >= MAX_PROCESSES) {
      throw new ToolError(`At most ${MAX_PROCESSES} processes run at once: stop one first (${this.list().filter((p) => p.state === "running").map((p) => p.name).join(", ")}).`);
    }
    // An exited process of that name is replaced.
    if (taken) await this.stop(taken.info.name);
    const finalName = name ?? this.uniqueName(nameFrom(command));

    const id = `p${++this.count}`;
    const dir = join(this.runtimeDir, id);
    mkdirSync(dir, { recursive: true, mode: 0o700 });
    const root = this.options.root;
    // As for a command that runs without asking: a long-lived process has no reason to write the repository, and a
    // hook planted there would run outside the sandbox (src/tools/bash.ts).
    const guard = gitGuard(root);
    const path = process.env.PATH ?? "/usr/bin:/bin";
    const script = `exec 2>&1\n${command}`;
    const args = sandboxArgs({
      root,
      home: this.options.home ?? homedir(),
      network,
      path,
      readOnly: guard.readOnly,
      placeholders: guard.placeholders,
      binds: [
        // Marv's own bun runs the relay: the sandbox may not see one (it lives under the hidden home).
        { source: process.execPath, dest: "/run/marv-bun" },
        { source: dir, dest: "/run/marv", writable: true },
      ],
    });
    // With network it shares the host's: its ports are already the user's, so there's nothing to relay.
    const inner = network ? ["bash", "-c", script] : ["/run/marv-bun", "-e", relaySource({ listen: "/run/marv/fwd.sock", argv: ["bash", "-c", script] })];
    const proc = Bun.spawn(["bwrap", ...args, "--", ...inner], { cwd: root, stdin: "ignore", stdout: "pipe", stderr: "pipe", env: { PATH: path } });

    const info: ProcessInfo = { name: finalName, command, network, state: "running", exitCode: null, ports: [], notes: [], startedAt: Date.now() };
    const entry: Entry = { id, info, proc, dir, log: "", listeners: [], connections: new Set(), seen: new Set(), placeholders: guard.placeholders ?? [], exited: Promise.resolve() };
    this.entries.set(finalName, entry);
    void this.capture(entry, proc.stdout);
    void this.capture(entry, proc.stderr);
    entry.exited = proc.exited.then(async (code) => {
      info.state = "exited";
      const signal = proc.signalCode as keyof typeof constants.signals | null;
      info.exitCode = signal ? 128 + (constants.signals[signal] ?? 9) : code;
      this.release(entry);
      this.changed();
    });
    entry.timer = setInterval(() => this.poll(entry), this.options.pollMs ?? POLL_MS);
    this.changed();

    // Up to 10 s for the first port (or the end), checked more often than the steady poll.
    const until = Date.now() + (this.options.startWaitMs ?? START_WAIT_MS);
    while (Date.now() < until && info.state === "running" && !info.ports.length && !info.notes.length) {
      await this.poll(entry);
      await Promise.race([entry.exited, Bun.sleep(100)]);
    }
    // Let the last output of a process that ended arrive.
    if (info.state === "exited") await Bun.sleep(50);
    const output = entry.log.split("\n").slice(0, START_LINES).join("\n").trimEnd();
    return { info: { ...info, ports: [...info.ports], notes: [...info.notes] }, output };
  }

  /** Stops it (its whole sandbox), closes its forwarded ports and forgets it. False when there's none of that name. */
  async stop(name: string): Promise<boolean> {
    const entry = this.entries.get(name);
    if (!entry) return false;
    this.entries.delete(name);
    if (entry.info.state === "running") {
      // bwrap's sandbox dies with it (--die-with-parent kills everything inside, its own PID namespace has no way out).
      entry.proc.kill("SIGTERM");
      const done = await Promise.race([entry.exited.then(() => true), Bun.sleep(this.options.stopGraceMs ?? STOP_GRACE_MS).then(() => false)]);
      if (!done) entry.proc.kill("SIGKILL");
      await entry.exited;
    }
    this.release(entry);
    this.changed();
    return true;
  }

  /** Stops every process (Marv's exit, session.close()); later starts refuse. */
  async closeAll(): Promise<void> {
    this.closed = true;
    await Promise.all([...this.entries.keys()].map((name) => this.stop(name)));
    rmSync(this.runtimeDir, { recursive: true, force: true });
  }

  private unknown(name: string): string {
    const names = [...this.entries.keys()];
    return `No process named "${name}".${names.length ? ` Processes: ${names.join(", ")}.` : " None is running."}`;
  }

  private uniqueName(base: string): string {
    if (!this.entries.has(base)) return base;
    for (let n = 2; ; n++) if (!this.entries.has(`${base}-${n}`)) return `${base}-${n}`;
  }

  private changed(): void {
    try {
      this.options.onChange?.();
    } catch {
      // The client's redraw failing is the client's problem.
    }
  }

  private async capture(entry: Entry, stream: ReadableStream<Uint8Array>): Promise<void> {
    const decoder = new TextDecoder();
    try {
      for await (const chunk of stream) {
        entry.log += decoder.decode(chunk, { stream: true });
        if (entry.log.length > MAX_LOG_CHARS) {
          const cut = entry.log.length - MAX_LOG_CHARS;
          const newline = entry.log.indexOf("\n", cut);
          entry.log = entry.log.slice(newline >= 0 ? newline + 1 : cut);
        }
      }
    } catch {
      // the stream closed under us: the process is gone
    }
  }

  /** New listening ports, each forwarded once (or noted). */
  private async poll(entry: Entry): Promise<void> {
    if (entry.info.state !== "running") return;
    const pid = sandboxPid(entry.proc.pid);
    if (pid === null) return;
    const text = read(`/proc/${pid}/net/tcp`) + read(`/proc/${pid}/net/tcp6`).split("\n").slice(1).join("\n");
    // On the host's network, /proc/<pid>/net shows every socket on the machine: only the sandbox's own count.
    const ports = listeningPorts(text, entry.info.network ? sandboxSockets(pid) : undefined);
    for (const port of ports) {
      if (entry.seen.has(port)) continue;
      entry.seen.add(port);
      if (entry.info.ports.length >= MAX_PORTS) {
        entry.info.notes.push(`port ${port} isn't forwarded: at most ${MAX_PORTS} ports per process.`);
        continue;
      }
      if (entry.info.network) {
        entry.info.ports.push(port);
        continue;
      }
      const listener = await this.forward(entry, port);
      if (entry.info.state !== "running") {
        listener?.close();
        return;
      }
      if (listener) {
        entry.listeners.push(listener);
        entry.info.ports.push(port);
      } else {
        entry.info.notes.push(`port ${port} is in use on the user's machine, so it isn't forwarded: start the server on another port.`);
      }
      this.changed();
    }
  }

  /** Listens on the host's 127.0.0.1:<port> (loopback only) and pipes each connection through the process's socket. */
  private forward(entry: Entry, port: number): Promise<Server | null> {
    const socket = join(entry.dir, "fwd.sock");
    const server = createServer((client) => {
      client.pause();
      entry.connections.add(client);
      const upstream = connect({ path: socket });
      entry.connections.add(upstream);
      const close = () => {
        client.destroy();
        upstream.destroy();
        entry.connections.delete(client);
        entry.connections.delete(upstream);
      };
      client.on("error", close).on("close", close);
      upstream.on("error", close).on("close", close);
      upstream.once("connect", () => {
        upstream.write(`${port}\n`);
        client.pipe(upstream);
        upstream.pipe(client);
      });
    });
    return new Promise((resolve) => {
      server.once("error", () => resolve(null));
      server.listen({ port, host: "127.0.0.1", exclusive: true }, () => resolve(server));
    });
  }

  /** What a process holds on the host: its poll, listeners, connections, socket folder and .git placeholder. */
  private release(entry: Entry): void {
    if (entry.timer) clearInterval(entry.timer);
    entry.timer = undefined;
    for (const listener of entry.listeners.splice(0)) listener.close();
    for (const conn of entry.connections) conn.destroy();
    entry.connections.clear();
    rmSync(entry.dir, { recursive: true, force: true });
    // bwrap leaves a placeholder's empty mount point; rmdir only removes it while it's empty.
    for (const path of entry.placeholders) void rmdir(path).catch(() => {});
  }
}
