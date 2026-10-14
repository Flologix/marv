// The relay that carries TCP connections across a sandbox's network namespace (#51). bwrap can't join or share part
// of a network namespace, but a Unix socket in a folder bind-mounted into the sandbox is reachable from both sides.
// `relayMain` is sent as source text to `bun -e` inside the sandbox, which can't see Marv's own files (they're under
// the hidden home folder): like the checks runner it must be self-contained, using only its argument and globals,
// with no imports and nothing from this module.

/** One port reached through a Unix socket: a connection to the socket sends `<port>\n` first, then the bytes. */
export interface RelayForward {
  port: number;
  socket: string;
}

export interface RelaySpec {
  /**
   * Forward mode (a process's sandbox): listen on this Unix socket; each connection's first line names a port, and the
   * rest is piped to this sandbox's 127.0.0.1:<port> (then ::1, for a server bound to localhost).
   */
  listen?: string;
  /**
   * Reverse mode (a bash command's sandbox, while processes run): listen on this sandbox's 127.0.0.1:<port> for each,
   * and pipe each connection to its socket, so the command's `curl localhost:<port>` reaches a running process.
   */
  connect?: RelayForward[];
  /** What to run once the relay listens, with the relay's stdio; the relay exits with its exit code. */
  argv: string[];
}

/** The relay's source for `bun -e`, with its spec. */
export function relaySource(spec: RelaySpec): string {
  return `(${relayMain.toString()})(${JSON.stringify(spec)})`;
}

export async function relayMain(spec: RelaySpec): Promise<void> {
  // getBuiltinModule, not require: the published CLI is a bundle, and a bundler rewrites require() into a helper
  // (__require) that doesn't exist where this source runs on its own (tests/process-relay.test.ts).
  const net = process.getBuiltinModule("node:net") as typeof import("node:net");
  const os = process.getBuiltinModule("node:os") as typeof import("node:os");
  const MAX_LINE = 16;
  type Socket = import("node:net").Socket;

  /** Both ways, until either side ends; an error on one closes the other. */
  const join = (a: Socket, b: Socket) => {
    a.pipe(b);
    b.pipe(a);
    a.on("error", () => b.destroy());
    b.on("error", () => a.destroy());
    a.on("close", () => b.destroy());
    b.on("close", () => a.destroy());
  };

  /** A connection to the first of the hosts that answers on the port, or null. */
  const connectLocal = (port: number, hosts: string[]): Promise<Socket | null> =>
    new Promise((resolve) => {
      const [host, ...rest] = hosts;
      if (host === undefined) return resolve(null);
      const socket = net.connect({ host, port });
      socket.once("connect", () => {
        socket.removeAllListeners("error");
        resolve(socket);
      });
      socket.once("error", () => {
        socket.destroy();
        connectLocal(port, rest).then(resolve);
      });
    });

  const listening = (server: import("node:net").Server, ...args: unknown[]): Promise<void> =>
    new Promise((resolve) => {
      server.once("listening", () => resolve());
      // A port this sandbox can't take isn't relayed; the command runs anyway.
      server.once("error", () => resolve());
      (server.listen as (...a: unknown[]) => void)(...args);
    });

  const servers: Promise<void>[] = [];
  if (spec.listen) {
    const server = net.createServer((client) => {
      let head = Buffer.alloc(0);
      client.on("error", () => client.destroy());
      const onData = (chunk: Buffer) => {
        head = Buffer.concat([head, chunk]);
        const newline = head.indexOf(10);
        if (newline < 0) {
          if (head.length > MAX_LINE) client.destroy();
          return;
        }
        client.off("data", onData);
        // Paused until the server answers, so nothing that arrives meanwhile is lost (pipe resumes it).
        client.pause();
        const line = head.subarray(0, newline).toString();
        const rest = head.subarray(newline + 1);
        const port = /^\d{1,5}$/.test(line) ? Number(line) : 0;
        // Only unprivileged ports: the same rule as the host side, whatever reaches the socket.
        if (port < 1024 || port > 65535) return void client.destroy();
        connectLocal(port, ["127.0.0.1", "::1"]).then((upstream) => {
          if (!upstream || client.destroyed) {
            upstream?.destroy();
            return void client.destroy();
          }
          if (rest.length) upstream.write(rest);
          join(client, upstream);
        });
      };
      client.on("data", onData);
    });
    servers.push(listening(server, spec.listen));
  }
  for (const { port, socket } of spec.connect ?? []) {
    const server = net.createServer((client) => {
      client.pause();
      client.on("error", () => client.destroy());
      const upstream = net.connect({ path: socket });
      upstream.once("error", () => {
        upstream.destroy();
        client.destroy();
      });
      upstream.once("connect", () => {
        upstream.write(`${port}\n`);
        join(client, upstream);
      });
    });
    servers.push(listening(server, port, "127.0.0.1"));
  }
  await Promise.all(servers);

  const child = Bun.spawn(spec.argv, { stdio: ["inherit", "inherit", "inherit"] });
  for (const signal of ["SIGTERM", "SIGINT", "SIGHUP"] as const) process.on(signal, () => child.kill(signal));
  const code = await child.exited;
  const signal = child.signalCode as keyof typeof os.constants.signals | null;
  process.exit(signal ? 128 + (os.constants.signals[signal] ?? 9) : code);
}
