// The process tool (#51): servers, watchers and anything else that should keep running after a turn, each in its
// own sandbox, with the ports it listens on forwarded to the user's 127.0.0.1 (src/processes.ts). Starting one asks
// (a door out of the sandbox: it outlives the turn and the user can reach it); reading its logs, listing and stopping
// don't (stopping your own process can't hurt).
import { z } from "zod";
import { MAX_LOG_LINES, type ProcessInfo } from "../processes.ts";
import { ToolError, type Tool } from "./types.ts";

/** Said when a call has no process door: subagents (their process would outlive their report), or a session without one. */
export const NO_PROCESS_DOOR = "Background processes can only be started by the main agent, in a session that offers them.";

const input = z
  .object({
    action: z.enum(["start", "logs", "stop", "list"]).describe("start a process, read its logs, stop it, or list them all."),
    command: z.string().min(1).optional().describe("start: the shell command (bash), run in the project folder, e.g. bun run dev."),
    name: z.string().min(1).optional().describe("start: a short name for it (default: from the command). logs, stop: which process."),
    network: z
      .boolean()
      .optional()
      .describe("start: share the user's network (it can reach out, and its ports are the user's own, not forwarded). Off by default."),
    lines: z.number().int().min(1).max(MAX_LOG_LINES).optional().describe(`logs: how many of the last lines (default 50, at most ${MAX_LOG_LINES}).`),
  })
  .superRefine((value, ctx) => {
    if (value.action === "start" && !value.command) ctx.addIssue({ code: "custom", path: ["command"], message: "start needs a command" });
    if ((value.action === "logs" || value.action === "stop") && !value.name) {
      ctx.addIssue({ code: "custom", path: ["name"], message: `${value.action} needs the process's name (list shows them)` });
    }
  });

const url = (port: number) => `http://localhost:${port}`;

/** "running · http://localhost:4789" / "exited (code 1)". */
function state(info: ProcessInfo): string {
  if (info.state === "exited") return `exited (code ${info.exitCode ?? "?"})`;
  return ["running", ...info.ports.map(url)].join(" · ");
}

function uptime(info: ProcessInfo, now = Date.now()): string {
  const s = Math.max(0, Math.round((now - info.startedAt) / 1000));
  return s < 60 ? `${s}s` : s < 3600 ? `${Math.floor(s / 60)}m` : `${Math.floor(s / 3600)}h${Math.floor((s % 3600) / 60)}m`;
}

/** One line per process, for list and /processes. */
export function processLine(info: ProcessInfo, now = Date.now()): string {
  const net = info.network ? " · on the user's network" : "";
  return `${info.name}: ${state(info)}${net} · up ${uptime(info, now)} · $ ${info.command}`;
}

export const processTool: Tool<typeof input> = {
  name: "process",
  description:
    "Run something that should keep running (a dev server, a watcher) in its own sandbox, across turns, until stopped or Marv exits. " +
    "start runs a shell command and waits up to 10 s for it to listen on a port: the ports it listens on (1024 and up) are forwarded to the user's localhost, and your bash commands can reach them too (curl localhost:<port>). " +
    "logs reads its output, stop ends it and everything it started, list shows them all. The user is asked before each start. " +
    "Like bash's sandbox: only the project folder is writable, no network unless network: true.",
  input,
  kind: "execute",
  // Only starting asks: a door out of the sandbox. Logs, list and stop touch nothing but the session's own processes.
  needsApproval: ({ action }) => action === "start",
  label: ({ action, command, name }) => (action === "start" ? (command ?? "start") : action === "list" ? "list" : `${action} ${name ?? ""}`.trim()),
  scope: ({ command, network }) => ({ key: `process:${network ? "net:" : ""}${command}`, description: "starting this exact command in the background" }),
  door: ({ action }) => (action === "start" ? "process" : undefined),
  usesNetwork: ({ action, network }) => action === "start" && Boolean(network),
  // No autoSafe: yolo never starts one unasked (it keeps running, and the user can reach it).

  async preview({ command, network, name }, { doors, processes }) {
    // Refused before any ask: the door is the context's to open (set for the main agent only), not the approver's.
    if (!doors?.process || !processes) throw new ToolError(NO_PROCESS_DOOR);
    return {
      title: `Start a background process${name ? ` (${name})` : ""}`,
      command,
      note: network
        ? "its own sandbox · shares your network: it can reach out, and its ports are your machine's"
        : "its own sandbox · no network · its ports forwarded to your 127.0.0.1",
    };
  },

  async run({ action, command, name, network, lines }, { doors, processes }) {
    if (!processes) throw new ToolError(NO_PROCESS_DOOR);
    if (action === "list") {
      const all = processes.list();
      if (!all.length) return { output: "No background processes.", summary: "none" };
      return { output: all.map((p) => `- ${processLine(p)}`).join("\n"), summary: `${all.length} process${all.length === 1 ? "" : "es"}` };
    }
    if (action === "logs") {
      const text = processes.logs(name!, lines);
      const info = processes.list().find((p) => p.name === name);
      const shown = text === "" ? 0 : text.split("\n").length;
      return {
        output: `Logs of ${name} (${info ? state(info) : "gone"}), last ${shown} line${shown === 1 ? "" : "s"}:\n${text || "(no output yet)"}`,
        summary: `${shown} line${shown === 1 ? "" : "s"}`,
      };
    }
    if (action === "stop") {
      if (!(await processes.stop(name!))) {
        const names = processes.list().map((p) => p.name);
        throw new ToolError(`No process named "${name}".${names.length ? ` Processes: ${names.join(", ")}.` : " None is running."}`);
      }
      return { output: `Stopped ${name} and everything it started.`, summary: "stopped" };
    }
    // Checked again here, so no path to run() skips the door (preview refuses first).
    if (!doors?.process) throw new ToolError(NO_PROCESS_DOOR);
    const { info, output } = await processes.start({ command: command!, name, network });
    const parts = [`Process ${info.name}: ${state(info)}.`];
    if (info.state === "exited") {
      parts.push("It ended at once: nothing is running. Fix the problem its output shows, then start it again.");
    } else if (info.ports.length) {
      parts.push(
        info.network
          ? `It shares the user's network, so it listens on their machine directly: ${info.ports.map(url).join(", ")}.`
          : `Forwarded to the user's machine: ${info.ports.map(url).join(", ")} (your bash commands reach it there too).`,
      );
    } else {
      parts.push("It's running but hasn't listened on a port yet (waited 10 s): check its logs, or list later.");
    }
    for (const note of info.notes) parts.push(`Note: ${note}`);
    parts.push(output ? `First output:\n${output}` : "No output yet.");
    return { output: parts.join("\n"), summary: info.state === "exited" ? `exited (code ${info.exitCode ?? "?"})` : state(info).replace("http://", "") };
  },
};
