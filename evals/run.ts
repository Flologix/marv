// Runs Marv on the tasks in evals/tasks with real models, checks each result with the task's hidden tests, and
// prints a comparison. The point: measure whether a change to Marv (a tool, a prompt) helps, across models.
//
//   bun evals/run.ts --verify                         every check fails on the task's repo and passes with its solution
//   bun evals/run.ts --models a,b --label baseline    each model on each task (OPENROUTER_API_KEY, or ~/.marv's key)
//         [--tasks x,y] [--repeat 2] [--budget 1.50] [--concurrency 4] [--timeout 300] [--max-steps 50] [--diagnostics on|off]
//         [--mcp servers.json] [--mcp-tools auto|deferred|eager]   MCP servers for every run, and how they're offered (#14)
//   bun evals/run.ts --report evals/results/a/results.jsonl evals/results/b/results.jsonl
//
// A task is a folder: task.md (the request), repo/ (the starting files), check/ (tests copied in only after the
// agent finishes, so it can neither see nor change them), solution/ (files that make the check pass, for --verify).
// Each run gets a fresh git repository in a temp folder and a session with no approver, so what yolo mode allows
// runs (edits, sandboxed bash without network) and everything else is refused, the way an unattended run would be.
// A task with an mcp.json (`{ "server": "tracker", "data": "<file in evals/mcp/data>" }`) gets the tracker MCP server
// (evals/mcp/tracker.ts); the comments it records are copied into the workspace as __tracker_comments__.json before
// the check runs, the way check/ is.
// --budget stops starting runs once the money spent (as OpenRouter reports it) reaches it; runs already going
// finish, so it can be overshot by a few runs' worth.
import { appendFileSync, cpSync, existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { parseArgs } from "./args.ts";
import { defaultConfigDir } from "../src/config/config.ts";
import { sandboxAvailable } from "../src/sandbox.ts";
import { createSession, type McpServerEntry } from "../src/sdk.ts";
import { runCommand } from "../src/tools/bash.ts";
import { eventCost, formatSummary, type RunResult } from "./summary.ts";
import { addTypeScript, ranTypecheck } from "./typescript.ts";

const TASKS = join(import.meta.dir, "tasks");
const RESULTS = join(import.meta.dir, "results");
const CHECK = "bun test ./__eval__.test.js";
const TRACKER = join(import.meta.dir, "mcp", "tracker.ts");
const MCP_DATA = join(import.meta.dir, "mcp", "data");
/** Where the tracker's recorded comments go in the workspace, for the check (a task's solution/ has its own copy). */
const COMMENTS = "__tracker_comments__.json";
const MCP_TOOLS = ["auto", "deferred", "eager"];

/** A task's MCP server, from its mcp.json; undefined for a task without one. */
function taskMcp(task: string): { server: "tracker"; data: string } | undefined {
  const file = join(TASKS, task, "mcp.json");
  if (!existsSync(file)) return undefined;
  const config = JSON.parse(readFileSync(file, "utf8")) as { server?: string; data?: string };
  if (config.server !== "tracker" || !config.data) throw new Error(`${task}/mcp.json: expected { "server": "tracker", "data": "<file>" }`);
  return { server: "tracker", data: config.data };
}

/** A fresh copy of the task's repo (plus `extra` folders on top), committed, in a temp folder. */
function workspace(task: string, ...extra: string[]): string {
  const dir = mkdtempSync(join(tmpdir(), `marv-eval-${task}-`));
  for (const from of ["repo", ...extra]) cpSync(join(TASKS, task, from), dir, { recursive: true });
  const git = (...args: string[]) => {
    const r = Bun.spawnSync(["git", "-c", "user.name=eval", "-c", "user.email=eval@localhost", "-c", "commit.gpgSign=false", ...args], { cwd: dir });
    if (r.exitCode !== 0) throw new Error(`git ${args[0]}: ${r.stderr.toString()}`);
  };
  git("init", "-q");
  // Excluded here rather than trusting each task repo's .gitignore: the compiler must stay untracked and invisible to
  // `git ls-files`, which Marv's glob and grep use.
  mkdirSync(join(dir, ".git", "info"), { recursive: true });
  appendFileSync(join(dir, ".git", "info", "exclude"), "node_modules/\n");
  git("add", "-A");
  git("commit", "-q", "-m", "start");
  // After the commit, and excluded above, so the compiler is never part of the repo.
  addTypeScript(dir);
  return dir;
}

/** Copies the hidden check in and runs it (sandboxed, like the agent's own commands). */
async function check(dir: string, task: string): Promise<{ pass: boolean; output: string }> {
  cpSync(join(TASKS, task, "check"), dir, { recursive: true });
  const result = await runCommand({ command: `${CHECK} 2>&1`, root: dir, sandbox: sandboxAvailable(), network: false, timeoutMs: 60_000 });
  return { pass: result.exitCode === 0, output: result.output };
}

const argv = process.argv.slice(2);
const args = parseArgs(argv);

const allTasks = readdirSync(TASKS).filter((name) => existsSync(join(TASKS, name, "task.md"))).sort();
const tasks = args.tasks ?? allTasks;
for (const task of tasks) if (!allTasks.includes(task)) throw new Error(`No task "${task}" (have: ${allTasks.join(", ")})`);

if (args.verify) {
  let ok = true;
  for (const task of tasks) {
    const before = workspace(task);
    const after = workspace(task, "solution");
    const [b, a] = [await check(before, task), await check(after, task)];
    rmSync(before, { recursive: true, force: true });
    rmSync(after, { recursive: true, force: true });
    const good = !b.pass && a.pass;
    ok &&= good;
    console.log(`${good ? "ok  " : "BAD "} ${task}${b.pass ? " (passes before any change)" : ""}${a.pass ? "" : ` (solution fails)\n${a.output}`}`);
  }
  process.exit(ok ? 0 : 1);
}

const readResults = (path: string): RunResult[] =>
  readFileSync(path, "utf8").split("\n").filter(Boolean).map((line) => JSON.parse(line) as RunResult);

if (args.report.length) {
  console.log(formatSummary(args.report.flatMap(readResults)));
  process.exit(0);
}

// --- Running ---
const models = args.models;
if (!models?.length) throw new Error("--models a,b is required (OpenRouter model ids)");
const label = args.label ?? "run";
const repeat = args.repeat;
const budget = args.budget;
const concurrency = args.concurrency;
const timeoutMs = args.timeout * 1000;
// Twice the interactive limit: at 25 steps the TUI asks "Keep going?", and a user would say yes once. With no one to
// ask, 25 was a hard stop, and slow, careful models (one file per step, re-reading after each edit) were measured on
// the limit instead of on their edits.
const maxSteps = args.maxSteps;
// Marv's typecheck after file changes (on by default, as in the CLI); off for the comparison's other arm.
const diagnosticsOption = args.diagnostics;
if (diagnosticsOption !== undefined && diagnosticsOption !== "on" && diagnosticsOption !== "off") throw new Error("--diagnostics takes on or off");
const diagnostics = diagnosticsOption !== "off";
// MCP servers for every run (Claude Code's format, or the bare record), and how they're offered (#14).
const mcpTools = args.mcpTools;
if (mcpTools !== undefined && !MCP_TOOLS.includes(mcpTools)) throw new Error(`--mcp-tools takes ${MCP_TOOLS.join(", ")}`);
const extraServers: Record<string, McpServerEntry> = (() => {
  if (!args.mcp) return {};
  const raw = JSON.parse(readFileSync(args.mcp, "utf8")) as { mcpServers?: Record<string, McpServerEntry> } & Record<string, McpServerEntry>;
  return raw.mcpServers ?? raw;
})();
const savedKey = () => (JSON.parse(readFileSync(join(defaultConfigDir(process.env), "config.json"), "utf8")) as { apiKey?: string }).apiKey;
const apiKey: string = process.env.OPENROUTER_API_KEY ?? savedKey() ?? "";
if (!apiKey) throw new Error("No OpenRouter key: set OPENROUTER_API_KEY or run marv's /setup.");
if (!sandboxAvailable()) throw new Error("The evals need bubblewrap: without the sandbox, yolo mode runs nothing unasked.");

const head = Bun.spawnSync(["git", "rev-parse", "--short", "HEAD"], { cwd: import.meta.dir }).stdout.toString().trim();
const dirty = Bun.spawnSync(["git", "status", "--porcelain", "--", "src"], { cwd: import.meta.dir }).stdout.toString().trim() !== "";
const marv = `${head}${dirty ? "+changes" : ""}`;

const stamp = new Date().toISOString().replace(/[:.]/g, "-").slice(0, 19);
const out = join(RESULTS, `${stamp}-${label}`);
mkdirSync(join(out, "logs"), { recursive: true });
const resultsFile = join(out, "results.jsonl");

// Rep-major order, so a budget that runs out leaves every model with about as many runs.
const jobs = Array.from({ length: repeat }, (_, rep) => tasks.flatMap((task) => models.map((model) => ({ model, task, rep })))).flat();
let spent = 0;
let done = 0;
const short = (text: string, max: number) => (text.length > max ? `${text.slice(0, max)}…` : text);

async function runOne({ model, task, rep }: { model: string; task: string; rep: number }): Promise<RunResult> {
  const dir = workspace(task);
  const started = Date.now();
  const tracker = taskMcp(task);
  // The tracker's output lives beside the workspace, not in it: the model can't write it except through the server.
  const trackerDir = tracker ? mkdtempSync(join(tmpdir(), `marv-eval-${task}-tracker-`)) : undefined;
  const comments = trackerDir ? join(trackerDir, "out.json") : "";
  const mcpServers: Record<string, McpServerEntry> = {
    ...extraServers,
    ...(tracker ? { tracker: { command: process.execPath, args: [TRACKER, join(MCP_DATA, tracker.data), comments] } } : {}),
  };
  const withMcp = Object.keys(mcpServers).length > 0;
  const result: RunResult = {
    label,
    model,
    task,
    rep,
    pass: false,
    reason: "error",
    requests: 0,
    promptTokens: 0,
    cachedTokens: 0,
    completionTokens: 0,
    cost: 0,
    ms: 0,
    tools: {},
    editErrors: [],
    marv,
    diagnostics,
    ...(withMcp ? { mcpTools: mcpTools ?? "auto", mcpCalls: 0 } : {}),
  };
  // What happened, for reading a run afterwards: replies, and every tool call with (the start of) what it returned.
  const log: unknown[] = [];
  let timedOut = false;
  try {
    const session = await createSession({
      cwd: dir,
      provider: { kind: "openrouter", apiKey, model },
      maxSteps,
      diagnostics,
      ...(withMcp ? { mcpServers, mcpTools: mcpTools as "auto" | "deferred" | "eager" | undefined } : {}),
    });
    const timer = setTimeout(() => {
      timedOut = true;
      session.interrupt();
    }, timeoutMs);
    try {
      for await (const event of session.send(readFileSync(join(TASKS, task, "task.md"), "utf8").trim())) {
        spent += eventCost(event);
        if (event.type === "assistant") log.push({ assistant: event.text });
        if (event.type === "error") {
          result.error = event.message;
          log.push({ error: event.message });
        }
        if (event.type === "done") result.reason = event.reason;
        if (event.type === "empty_reply") {
          result.emptyReplies = (result.emptyReplies ?? 0) + 1;
          log.push({ emptyReply: event.next });
        }
        if (event.type === "check") {
          if (event.result.status === "done" && event.result.added.length) result.checkNotes = (result.checkNotes ?? 0) + 1;
          log.push({ check: event.result });
        }
        if (event.type === "tool_end") {
          const name = event.call.name;
          const tool = (result.tools[name] ??= { calls: 0, errors: 0 });
          tool.calls++;
          if (event.result.isError) tool.errors++;
          if (name === "edit_file" && event.result.isError) result.editErrors.push(short(event.result.output, 300));
          if (/^Marv: .* doesn't parse/m.test(event.result.output)) result.syntaxNotes = (result.syntaxNotes ?? 0) + 1;
          if (name === "bash" && ranTypecheck(event.call.arguments)) result.ranTypecheck = true;
          if (name === "mcp_call" || name.startsWith("mcp__")) result.mcpCalls = (result.mcpCalls ?? 0) + 1;
          log.push({ tool: name, args: event.call.arguments, isError: event.result.isError ?? false, output: short(event.result.output, 3000) });
        }
      }
    } finally {
      clearTimeout(timer);
      const { totals } = session.usage();
      Object.assign(result, {
        requests: totals.requests,
        promptTokens: totals.promptTokens,
        cachedTokens: totals.cachedTokens,
        completionTokens: totals.completionTokens,
        cost: totals.cost ?? 0,
      });
      await session.close();
    }
    if (timedOut) result.reason = "timeout";
    // What the tracker recorded, over anything the model wrote under that name itself.
    if (tracker) writeFileSync(join(dir, COMMENTS), existsSync(comments) ? readFileSync(comments) : "[]");
    const checked = await check(dir, task);
    result.pass = checked.pass;
    if (!checked.pass) result.checkOutput = short(checked.output.slice(-2000), 2000);
  } catch (err) {
    result.error = (err as Error).message;
  } finally {
    result.ms = Date.now() - started;
    const diff = Bun.spawnSync(["git", "diff", "HEAD", "--", ".", ":!__eval__.test.js", `:!${COMMENTS}`], { cwd: dir }).stdout.toString();
    mkdirSync(join(out, "logs", model.replace(/[^\w.-]+/g, "_")), { recursive: true });
    writeFileSync(join(out, "logs", model.replace(/[^\w.-]+/g, "_"), `${task}-${rep}.json`), JSON.stringify({ ...result, diff, log }, null, 2));
    rmSync(dir, { recursive: true, force: true });
    if (trackerDir) rmSync(trackerDir, { recursive: true, force: true });
  }
  return result;
}

const queue = [...jobs];
const skipped: typeof jobs = [];
async function worker() {
  for (let job = queue.shift(); job; job = queue.shift()) {
    if (spent >= budget) {
      skipped.push(job);
      continue;
    }
    const result = await runOne(job);
    appendFileSync(resultsFile, `${JSON.stringify(result)}\n`);
    done++;
    const edits = result.tools.edit_file;
    console.log(
      `[${done}/${jobs.length}] $${spent.toFixed(3)}  ${result.pass ? "PASS" : "fail"}  ${job.model}  ${job.task}#${job.rep}  ` +
        `${result.reason} · ${result.requests} steps${edits ? ` · edits ${edits.calls - edits.errors}/${edits.calls}` : ""}${result.error ? ` · ${short(result.error, 80)}` : ""}`,
    );
  }
}

console.log(
  `${jobs.length} runs (${models.length} models × ${tasks.length} tasks × ${repeat}), budget $${budget}, ${maxSteps} steps, diagnostics ${diagnostics ? "on" : "off"}` +
    `${mcpTools ? `, MCP tools ${mcpTools}` : ""}, Marv ${marv} → ${out}`,
);
await Promise.all(Array.from({ length: Math.min(concurrency, jobs.length) }, worker));
if (skipped.length) console.log(`\nBudget reached: ${skipped.length} runs not started.`);
console.log(`\n${formatSummary(readResults(resultsFile))}`);
console.log(`\nSpent $${spent.toFixed(3)}. Results: ${resultsFile}`);
