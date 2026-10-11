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
import { loadInstructions } from "./prompt.ts";
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
  marv --version    print the version
  marv --help       show this help

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

// The project is wherever Marv was started; the tools can't reach outside it.
const root = process.cwd();
const instructions = await loadInstructions(root);
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
    instructions={instructions}
    skills={skills}
    skillProblems={skillProblems}
    hiddenPathFolders={hiddenPathFolders(process.env.PATH ?? "", homedir()).map((dir) => shortenHome(dir))}
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
