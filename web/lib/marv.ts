import "server-only";
import { spawn, execFileSync, type ChildProcess } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import path from "node:path";

// Where the web server keeps Marv's state: its own HOME (so Marv never sees the server user's ~/.marv skills, memory
// or MCP servers) and one workspace folder per chat.
const DATA_DIR = process.env.MARV_WEB_DATA ?? path.join(process.cwd(), ".data");
const HOME = path.join(DATA_DIR, "home");
const WORKSPACES = path.join(DATA_DIR, "workspaces");
const MARV_BIN = process.env.MARV_BIN ?? "marv";

export const DEFAULT_MODEL = process.env.MARV_WEB_MODEL ?? "z-ai/glm-5.3-flash";

// `--approve` scopes the UI may turn on. `github` is left out (it runs the server user's gh login), and so are mesh
// receptacles and the tainted/protected opt-ins.
export const APPROVE_SCOPES = ["web", "network", "git_write", "process", "files", "memory"] as const;
export type ApproveScope = (typeof APPROVE_SCOPES)[number];

const CHAT_ID = /^[0-9a-f-]{36}$/;
export const isChatId = (id: unknown): id is string => typeof id === "string" && CHAT_ID.test(id);

// OpenRouter model ids: "vendor/model" with an optional ":variant".
const MODEL_ID = /^[\w.-]+\/[\w.:-]+$/;
export const isModelId = (id: unknown): id is string => typeof id === "string" && id.length < 200 && MODEL_ID.test(id);

/** The OpenRouter key: the server's env, else (for local dev) the key the server user saved with `marv` setup. */
function apiKey(): string | undefined {
  if (process.env.OPENROUTER_API_KEY) return process.env.OPENROUTER_API_KEY;
  try {
    const saved = JSON.parse(readFileSync(path.join(homedir(), ".marv", "config.json"), "utf8"));
    return typeof saved.apiKey === "string" ? saved.apiKey : undefined;
  } catch {
    return undefined;
  }
}

function ensureHome() {
  const marvDir = path.join(HOME, ".marv");
  const config = path.join(marvDir, "config.json");
  if (existsSync(config)) return;
  mkdirSync(marvDir, { recursive: true, mode: 0o700 });
  // No key in the file: it goes in through the env on every run.
  writeFileSync(config, JSON.stringify({ provider: "openrouter", model: DEFAULT_MODEL }, null, 2), { mode: 0o600 });
}

/** The chat's folder, created (as a git repo, which `/undo` checkpoints and subagent worktrees need) on first use. */
function workspace(chatId: string): string {
  const dir = path.join(WORKSPACES, chatId);
  if (!existsSync(dir)) {
    mkdirSync(dir, { recursive: true });
    execFileSync("git", ["init", "-q"], { cwd: dir });
  }
  return dir;
}

export interface RunOptions {
  chatId: string;
  prompt: string;
  model: string;
  approve: ApproveScope[];
  maxSteps: number;
  /** Continue the chat's saved session (`-c`) rather than start one. */
  resume: boolean;
}

const running = new Map<string, ChildProcess>();
export const isRunning = (chatId: string) => running.has(chatId);

/**
 * Runs one `marv -p` turn in the chat's workspace and returns its stdout as NDJSON: Marv's stream-json events, then
 * `{type:"stderr"}` lines if it wrote any, then `{type:"exit", code}`. Aborting `signal` interrupts Marv.
 */
export function runTurn(options: RunOptions, signal: AbortSignal): ReadableStream<Uint8Array> {
  const key = apiKey();
  if (!key) throw new Error("No OpenRouter key: set OPENROUTER_API_KEY for the web server.");
  ensureHome();
  const cwd = workspace(options.chatId);
  const args = ["-p", "-", "--output", "stream-json", "-m", options.model, "--max-steps", String(options.maxSteps)];
  for (const scope of options.approve) args.push("--approve", scope);
  if (options.resume) args.push("-c");

  // Only what Marv needs: the server's own env (its secrets) stays out.
  const child = spawn(MARV_BIN, args, {
    cwd,
    env: { PATH: process.env.PATH ?? "/usr/bin:/bin", HOME, LANG: "C.UTF-8", TERM: "dumb", OPENROUTER_API_KEY: key, NODE_ENV: process.env.NODE_ENV },
    stdio: ["pipe", "pipe", "pipe"],
  });
  running.set(options.chatId, child);
  child.stdin.end(options.prompt);

  const encoder = new TextEncoder();
  let stderr = "";
  child.stderr.on("data", (chunk: Buffer) => (stderr += chunk.toString()));

  const stop = () => {
    child.kill("SIGINT");
    setTimeout(() => child.exitCode === null && child.kill("SIGKILL"), 3000).unref();
  };
  signal.addEventListener("abort", stop, { once: true });

  return new ReadableStream({
    start(controller) {
      const send = (value: unknown) => {
        try {
          controller.enqueue(encoder.encode(JSON.stringify(value) + "\n"));
        } catch {
          // The browser went away; Marv is being stopped.
        }
      };
      child.stdout.on("data", (chunk: Buffer) => {
        try {
          controller.enqueue(chunk);
        } catch {}
      });
      child.on("error", (error) => send({ type: "error", message: `Couldn't start ${MARV_BIN}: ${error.message}` }));
      child.on("close", (code) => {
        running.delete(options.chatId);
        signal.removeEventListener("abort", stop);
        if (stderr.trim()) send({ type: "stderr", text: stderr.trim() });
        send({ type: "exit", code });
        try {
          controller.close();
        } catch {}
      });
    },
    cancel: stop,
  });
}
