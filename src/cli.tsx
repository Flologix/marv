#!/usr/bin/env bun
// Entrypoint for the `marv` command (see "bin" in package.json).
import { homedir } from "node:os";
import { join } from "node:path";
import { render } from "ink";
import pkg from "../package.json";
import { loadAgents } from "./agents.ts";
import { App } from "./app.tsx";
import { ConfigError, ConfigStore, defaultConfigDir, migrateLegacyConfig } from "./config/config.ts";
import { HerdrReporter, herdrTarget } from "./herdr.ts";
import { filterMouseInput, MOUSE_OFF, MOUSE_ON } from "./mouse.ts";
import { projectKey, shortenHome } from "./paths.ts";
import { readInstructions } from "./prompt.ts";
// PATH folders under the home folder the sandbox hides are found once here and reported at startup (issue #34:
// with node from mise, the post-edit typecheck and other node_modules scripts silently never ran).
import { hiddenPathFolders } from "./sandbox.ts";
import { loadSkills } from "./skills.ts";
import { SessionStore } from "./sessions.ts";
import { TrajectoryStore } from "./trajectory.ts";
import { loadMcpConfig } from "./mcp/config.ts";
import { McpManager } from "./mcp/manager.ts";
import { McpTrust } from "./mcp/trust.ts";
import { loadMemory, memoryPaths } from "./memory.ts";
import { renderOptions } from "./render-options.ts";
import { selection } from "./selection.ts";

const HELP = `Marv v${pkg.version}: a terminal coding agent

Usage:
  marv              start an interactive session
  marv -c           continue the latest session in this folder (--continue)
  marv -r           pick an earlier session to resume (--resume)
  marv -p <prompt>  run one non-interactive turn and print the reply (--prompt)
  marv -p -         read the prompt from stdin
  marv run <prompt> alias for marv -p (run one non-interactive turn)
  marv --version    print the version
  marv --help       show this help

Options for -p:
  --output text         print the final reply as text (default)
  --output json         print a result object as JSON
  --output stream-json  print each event as a JSON line
  --approve <scope>     auto-approve everything in this scope (e.g. git_write, network)
  --max-steps <N>       steps before the turn stops (0 never stops; default: your config's maxSteps)
  --model <name>        use a different model
  -c, --continue        continue the latest saved session

Config:
  ~/.marv/config.json   created by the setup screen (/setup to change it)
  ~/.marv/sessions/     saved conversations, one folder per project
  ~/.marv/trajectories/ every turn, step by step, with your ratings (/trajectories)
  ~/.marv/agents/       your subagent types (a project's go in .marv/agents/)
  ~/.marv/mcp.json      your MCP servers (a project's go in .mcp.json; /mcp)
  ~/.marv/worktrees/    subagents' worktrees while they run
  OPENROUTER_API_KEY    overrides the saved OpenRouter key
  OLLAMA_HOST           where Ollama runs (default localhost:11434)
  MARV_MODEL            overrides the saved model
  MARV_MAX_STEPS        overrides the saved step limit (0 or off: never ask)
  MARV_CONFIG_DIR       use a different config directory`;

const args = process.argv.slice(2);

if (args.includes("--version") || args.includes("-v")) {
  console.log(pkg.version);
  process.exit(0);
}
if (args.includes("--help") || args.includes("-h")) {
  console.log(HELP);
  process.exit(0);
}

// `marv run <prompt>` as an alias for `marv -p <prompt>`.
if (args[0] === "run") {
  const rest = args.slice(1);
  args.length = 0;
  args.push("-p", ...(rest.length ? rest : ["-"]));
}

// Headless mode: `marv -p "<prompt>"` or `marv run` — one turn, no UI.

/** Returns the value after `--flag` (or undefined if not present). */
function parseHeadlessArg(all: string[], flag: string): string | undefined {
  const i = all.findLastIndex((a) => a === flag);
  return i !== -1 && i + 1 < all.length ? all[i + 1]! : undefined;
}

/** Returns the whole number (0 or more) after `--flag`, undefined when absent; exits on anything else (a typo must not quietly mean the saved setting). */
function parseHeadlessInt(all: string[], flag: string): number | undefined {
  const value = parseHeadlessArg(all, flag);
  if (value === undefined) return undefined;
  const n = Number(value);
  if (value.trim() === "" || !Number.isInteger(n) || n < 0) {
    console.error(`${flag} must be a whole number, 0 or more. Got: "${value}"`);
    process.exit(1);
  }
  return n;
}

/** Returns all values after `--flag <scope>` repeated (e.g. `--approve git_write --approve network`). */
function parseHeadlessList(all: string[], flag: string): string[] {
  const scopes: string[] = [];
  for (let i = 0; i < all.length; i++) {
    if (all[i] === flag && i + 1 < all.length) {
      scopes.push(all[i + 1]!);
      i++; // skip the value
    }
  }
  return scopes;
}

const promptIndex = args.findLastIndex((a) => a === "-p" || a === "--prompt");
if (promptIndex !== -1) {
  let prompt: string | undefined;
  if (promptIndex + 1 < args.length) {
    const value = args[promptIndex + 1]!;
    if (value === "-") {
      prompt = await Bun.stdin.text();
    } else {
      prompt = value;
    }
  } else {
    console.error("-p needs a prompt (or - for stdin)");
    process.exit(1);
  }

  if (!prompt) {
    console.error("No prompt given. Usage: marv -p \"<prompt>\" or marv run <prompt>");
    process.exit(1);
  }

  const outputFormatRaw = parseHeadlessArg(args, "--output") || "text";
  if (!["text", "json", "stream-json"].includes(outputFormatRaw)) {
    console.error(`--output must be one of: text, json, stream-json. Got: "${outputFormatRaw}"`);
    process.exit(1);
  }

  const { runHeadless } = await import("./headless.ts");
  const exitCode = await runHeadless({
    prompt,
    output: outputFormatRaw as "text" | "json" | "stream-json",
    model: parseHeadlessArg(args, "--model") || parseHeadlessArg(args, "-m"),
    maxSteps: parseHeadlessInt(args, "--max-steps"),
    approveScopes: parseHeadlessList(args, "--approve"),
    resume: args.includes("-c") || args.includes("--continue") ? "latest" : undefined,
  });
  process.exit(exitCode);
}

// The project is wherever Marv was started; the tools can't reach outside it.
const root = process.cwd();
const agentsMd = await readInstructions(root);
const { skills, problems: skillProblems } = await loadSkills({ root, home: homedir() });
const { agents, problems: agentProblems } = await loadAgents({ root, home: homedir() });
// MCP servers start now, in the background; the first request waits for them (see McpManager).
const { servers: mcpServers, problems: mcpProblems } = await loadMcpConfig({ root, configDir: defaultConfigDir(process.env), env: process.env });
const mcp = mcpServers.length
  ? new McpManager(mcpServers, { root, version: pkg.version, trust: new McpTrust(join(defaultConfigDir(process.env), "mcp-trust.json")) })
  : undefined;
void mcp?.start();
// Local servers must not outlive Marv, however it exits.
process.on("exit", () => mcp?.kill());
const memoryAt = memoryPaths(defaultConfigDir(process.env), root);
const memory = { paths: memoryAt, initial: await loadMemory(memoryAt) };

// Marv was called Ekko: carry its config over (only for the default location).
if (!process.env.MARV_CONFIG_DIR) await migrateLegacyConfig();
const store = new ConfigStore(defaultConfigDir(process.env));
let initialFile;
try {
  initialFile = await store.load();
} catch (err) {
  if (!(err instanceof ConfigError)) throw err;
  console.error(`${err.message}\n\nFix the file, or delete it to run setup again.`);
  process.exit(1);
}

// Mouse wheel scrolling and drag-to-select (see src/mouse.ts, src/selection.ts). Mouse reporting is a terminal-wide
// mode, so it must be switched off on every way out, or the shell would start
// receiving mouse codes after Marv quits.
if (process.stdin.isTTY && process.stdout.isTTY) {
  filterMouseInput(process.stdin);
  process.stdout.write(MOUSE_ON);
  process.on("exit", () => process.stdout.write(MOUSE_OFF));
  // The tab would otherwise say "bun" (the process actually running; Bun's process.title doesn't rename it).
  // Push the current title, set ours (OSC 0), and pop it back on the way out.
  process.stdout.write("\x1b[22;0t\x1b]0;marv\x07");
  process.on("exit", () => process.stdout.write("\x1b[23;0t"));
}

// In a Herdr pane, Herdr shows what Marv is doing (src/herdr.ts).
const herdrPane = herdrTarget(process.env);
const herdr = herdrPane && new HerdrReporter(herdrPane);

let flush: () => Promise<void> = async () => {};
const instance = render(
  <App
    store={store}
    initialFile={initialFile}
    env={process.env}
    version={pkg.version}
    cwd={shortenHome(root)}
    root={root}
    instructions={agentsMd?.text}
    instructionsCut={agentsMd?.cut}
    skills={skills}
    skillProblems={skillProblems}
    hiddenPathFolders={hiddenPathFolders(process.env.PATH ?? "", homedir(), root).map((dir) => shortenHome(dir))}
    agents={agents}
    agentProblems={agentProblems}
    worktreesDir={join(defaultConfigDir(process.env), "worktrees", projectKey(root))}
    sessions={new SessionStore(join(defaultConfigDir(process.env), "sessions"))}
    trajectories={new TrajectoryStore(join(defaultConfigDir(process.env), "trajectories"))}
    mcp={mcp}
    mcpProblems={mcpProblems}
    herdr={herdr}
    onFlush={(f) => (flush = f)}
    memory={memory}
    resume={args.includes("-c") || args.includes("--continue") ? "latest" : args.includes("-r") || args.includes("--resume") ? "pick" : undefined}
  />,
  renderOptions,
);
// Moving the selection highlight redraws the last frame instead of re-rendering.
selection.repaint = instance.repaint;

await instance.waitUntilExit();
// The last turn and pending trajectory records reach the disk (quitting cancels the delayed save).
await Promise.race([flush(), Bun.sleep(2000)]);
// Let servers shut down cleanly, but don't hang on one that won't.
await Promise.race([mcp?.close(), Bun.sleep(2000)]);
// Herdr stops showing an agent in this pane; no waiting on a Herdr that doesn't answer.
await Promise.race([herdr?.release(), Bun.sleep(1000)]);
process.exit(0);
