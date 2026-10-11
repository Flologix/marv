// Herdr is a terminal multiplexer for coding agents. When Marv runs in one of its panes, it tells Herdr what it's
// doing, so Herdr's sidebar shows working / blocked / done, and `herdr agent list|get|wait|read` see Marv.
//
// Herdr puts HERDR_ENV=1, HERDR_SOCKET_PATH and HERDR_PANE_ID into every pane's environment. A report is one line of
// JSON-RPC on that Unix socket, the same protocol Herdr's own integrations (Claude Code hooks, pi extensions) speak.
// Everything here is best effort: Herdr not answering must never slow Marv down or break it.
//
// As of Herdr 0.8.2, some things work only for the agent kinds Herdr has built in: it accepts Marv's session report
// but doesn't store it, and `herdr agent prompt|send-keys` refuse Marv ("not an active named agent"; `herdr pane
// send-text|send-keys` work). Marv reports its session anyway, so that works as soon as Herdr knows Marv.
import { createConnection } from "node:net";
import type { Env } from "./config/config.ts";

/** What Herdr shows for an agent. `idle` becomes `done` in Herdr when the pane's tab wasn't being looked at. */
export type HerdrState = "idle" | "working" | "blocked";

export interface HerdrTarget {
  socketPath: string;
  paneId: string;
}

/** The pane Marv runs in, or undefined outside Herdr. */
export function herdrTarget(env: Env): HerdrTarget | undefined {
  const socketPath = env.HERDR_SOCKET_PATH;
  const paneId = env.HERDR_PANE_ID;
  if (env.HERDR_ENV !== "1" || !socketPath || !paneId) return undefined;
  return { socketPath, paneId };
}

/** Sends one request; true once Herdr answered. Never throws. */
export type HerdrSend = (request: object, timeoutMs: number) => Promise<boolean>;

function socketSender(socketPath: string): HerdrSend {
  return (request, timeoutMs) =>
    new Promise((resolve) => {
      let done = false;
      const finish = (answered: boolean) => {
        if (done) return;
        done = true;
        clearTimeout(timer);
        socket.destroy();
        resolve(answered);
      };
      const socket = createConnection(socketPath);
      socket.on("error", () => finish(false));
      socket.on("connect", () => socket.write(`${JSON.stringify(request)}\n`));
      socket.on("data", () => finish(true));
      socket.on("end", () => finish(false));
      const timer = setTimeout(() => finish(false), timeoutMs);
      timer.unref?.();
    });
}

const AGENT = "marv";
const SOURCE = "herdr:marv";

export class HerdrReporter {
  private readonly send: HerdrSend;
  // Herdr drops a report whose seq isn't newer than the last one it took from this source, so a slow, older
  // report can't overwrite a newer state. Microseconds since the epoch, then +1 per report.
  private seq = Date.now() * 1000;
  private sessionRef: { agent_session_id: string; agent_session_path?: string } | undefined;
  private last: string | undefined;
  // Only the newest state matters: while one is being sent, a newer one replaces whatever was waiting.
  private queued: { state: HerdrState; message?: string; seq: number } | undefined;
  private sending: Promise<void> | undefined;

  constructor(
    private readonly target: HerdrTarget,
    send?: HerdrSend,
  ) {
    this.send = send ?? socketSender(target.socketPath);
  }

  /** Reports Marv's state (repeats of the same state are skipped). */
  state(state: HerdrState, message?: string): void {
    const key = `${state}\n${message ?? ""}`;
    if (key === this.last) return;
    this.last = key;
    this.queued = { state, message, seq: this.nextSeq() };
    this.sending ??= this.drain();
  }

  /** Tells Herdr which saved session the pane holds (again after /clear or a resume, which change it). */
  session(id: string, path?: string): Promise<void> {
    this.sessionRef = { agent_session_id: id, ...(path && { agent_session_path: path }) };
    return this.request("pane.report_agent_session", { ...this.sessionRef });
  }

  /** Marv is leaving the pane: Herdr stops showing an agent there. Waits for what's still being sent first. */
  async release(): Promise<void> {
    this.queued = undefined;
    await this.sending;
    await this.request("pane.release_agent", {});
  }

  /** Waits until the latest state has been sent (for tests). */
  async settled(): Promise<void> {
    while (this.sending) await this.sending;
  }

  private nextSeq(): number {
    return ++this.seq;
  }

  private async drain(): Promise<void> {
    try {
      while (this.queued) {
        const { state, message, seq } = this.queued;
        this.queued = undefined;
        await this.request("pane.report_agent", { state, ...(message && { message }), ...this.sessionRef }, seq);
      }
    } finally {
      this.sending = undefined;
    }
  }

  private async request(method: string, params: object, seq = this.nextSeq()): Promise<void> {
    const request = {
      id: `${SOURCE}:${seq}`,
      method,
      params: { pane_id: this.target.paneId, source: SOURCE, agent: AGENT, seq, ...params },
    };
    // One quick try, then one patient one (Herdr may be busy redrawing), then give up quietly.
    if (await this.send(request, 500)) return;
    await this.send(request, 1500);
  }
}
