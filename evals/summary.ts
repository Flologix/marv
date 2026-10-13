// What an eval run records, and the table that compares them. Pure: evals/run.ts does the running and the
// reading of result files; this only counts.

import type { SessionEvent } from "../src/sdk.ts";

/** The cost one event carries (the turn's usage, or a subagent's): the runner adds it up as events arrive. */
export function eventCost(event: SessionEvent): number {
  if (event.type === "usage") return event.usage.cost ?? 0;
  if (event.type === "subagent" && event.event.type === "usage") return event.event.usage.cost ?? 0;
  return 0;
}

/** One model on one task, once: a line of an evals/results/*.jsonl file. */
export interface RunResult {
  /** What's being compared, e.g. "baseline" or "fuzzy-edits" (--label). */
  label: string;
  model: string;
  task: string;
  /** Which repetition (0-based). */
  rep: number;
  /** The task's hidden check passed. */
  pass: boolean;
  /** How the agent's run ended: the loop's done reason, "timeout", or "error" when it never got going. */
  reason: string;
  error?: string;
  requests: number;
  promptTokens: number;
  cachedTokens: number;
  completionTokens: number;
  /** USD, as OpenRouter reported it. */
  cost: number;
  ms: number;
  /** Calls and failed calls, per tool. */
  tools: Record<string, { calls: number; errors: number }>;
  /** Replies with no text and no tool calls (each retried, nudged or ending the run). */
  emptyReplies?: number;
  /** File changes after which Marv said the file no longer parses. */
  syntaxNotes?: number;
  /** The model ran a typecheck itself (a guess from the command's text: a bash command naming tsc, tsgo or `run typecheck`). */
  ranTypecheck?: boolean;
  /** Steps after which Marv told the model about new type errors (post-edit diagnostics). */
  checkNotes?: number;
  /** Whether Marv's typecheck after file changes was on for this run. */
  diagnostics: boolean;
  /** The start of each failed edit_file result: what went wrong. */
  editErrors: string[];
  /** The tail of the check's output, for a failure. */
  checkOutput?: string;
  /** Marv's git commit, and whether its working tree had changes. */
  marv?: string;
  /** How MCP tools were offered (--mcp-tools; #14), for runs with an MCP server. */
  mcpTools?: string;
  /** MCP tools run (mcp_call, or an mcp__ tool offered directly). */
  mcpCalls?: number;
  /** Whether the plan tool was offered (--plan; #15). */
  plan?: boolean;
  /** Calls to the plan tool. */
  planCalls?: number;
  /** The run ended with an answer while its last plan still had unfinished items: the model let it go stale. */
  planOpen?: boolean;
  /** The hidden check's tests that passed, and how many it ran (from bun test's summary): partial credit. */
  testsPassed?: number;
  testsTotal?: number;
}

/**
 * The plan tool's measures from a run's plan calls (their arguments, in order) and how it ended: how often the model
 * planned, and whether it answered with its last readable plan still unfinished. Only an answer ("end") counts as
 * letting it go stale: a run cut off by the step limit or a timeout didn't choose to stop.
 */
export function planOutcome(calls: string[], reason: string): { planCalls: number; planOpen: boolean } {
  let last: { status?: unknown }[] | null = null;
  for (const args of calls) {
    try {
      const items = (JSON.parse(args) as { items?: unknown }).items;
      if (Array.isArray(items)) last = items as { status?: unknown }[];
    } catch {
      // A malformed call was refused by the tool: the plan is still the previous one.
    }
  }
  return { planCalls: calls.length, planOpen: reason === "end" && Boolean(last?.some((i) => i?.status !== "done")) };
}

/** Passed and total tests from a `bun test` summary (" 5 pass", " 1 fail"), or nothing when there's none to read. */
export function testCounts(output: string): { testsPassed?: number; testsTotal?: number } {
  const pass = /^\s*(\d+) pass\b/m.exec(output);
  const fail = /^\s*(\d+) fail\b/m.exec(output);
  if (!pass || !fail) return {};
  const testsPassed = Number(pass[1]);
  return { testsPassed, testsTotal: testsPassed + Number(fail[1]) };
}

export interface SummaryRow {
  label: string;
  model: string;
  runs: number;
  passed: number;
  editCalls: number;
  editErrors: number;
  toolCalls: number;
  requests: number;
  /** Prompt tokens over all requests: per request, what deferring MCP tools (#14) should move. */
  promptTokens: number;
  mcpCalls: number;
  /** Tests passed and run, over the runs that reported them (#15). */
  testsPassed: number;
  testsTotal: number;
  /** Plan calls, and runs that answered with the plan unfinished; `planRuns` counts the runs that recorded them. */
  planCalls: number;
  planOpen: number;
  planRuns: number;
  cost: number;
  medianMs: number;
  /** Which runs the row is (label and model), for the per-task grid. */
  group: string;
}

const median = (values: number[]) => {
  if (values.length === 0) return 0;
  const sorted = [...values].sort((a, b) => a - b);
  const mid = Math.floor(sorted.length / 2);
  return sorted.length % 2 ? sorted[mid]! : (sorted[mid - 1]! + sorted[mid]!) / 2;
};

const groupKey = (r: RunResult) => `${r.label}\u0000${r.model}`;

/** One row per label and model, in the order they first appear. */
export function summarizeRuns(results: RunResult[]): SummaryRow[] {
  const groups = new Map<string, RunResult[]>();
  for (const r of results) groups.set(groupKey(r), [...(groups.get(groupKey(r)) ?? []), r]);
  return [...groups.values()].map((runs) => {
    const tools = runs.flatMap((r) => Object.entries(r.tools));
    const edits = tools.filter(([name]) => name === "edit_file").map(([, t]) => t);
    return {
      label: runs[0]!.label,
      model: runs[0]!.model,
      runs: runs.length,
      passed: runs.filter((r) => r.pass).length,
      editCalls: edits.reduce((n, t) => n + t.calls, 0),
      editErrors: edits.reduce((n, t) => n + t.errors, 0),
      toolCalls: tools.reduce((n, [, t]) => n + t.calls, 0),
      requests: runs.reduce((n, r) => n + r.requests, 0),
      promptTokens: runs.reduce((n, r) => n + r.promptTokens, 0),
      mcpCalls: runs.reduce((n, r) => n + (r.mcpCalls ?? 0), 0),
      testsPassed: runs.reduce((n, r) => n + (r.testsPassed ?? 0), 0),
      testsTotal: runs.reduce((n, r) => n + (r.testsTotal ?? 0), 0),
      planCalls: runs.reduce((n, r) => n + (r.planCalls ?? 0), 0),
      planOpen: runs.filter((r) => r.planOpen).length,
      planRuns: runs.filter((r) => r.planCalls !== undefined).length,
      cost: runs.reduce((n, r) => n + r.cost, 0),
      medianMs: median(runs.map((r) => r.ms)),
      group: groupKey(runs[0]!),
    };
  });
}

const pct = (part: number, whole: number) => (whole ? `${Math.round((part / whole) * 100)}%` : "-");

function table(rows: string[][]): string {
  const widths = rows[0]!.map((_, col) => Math.max(...rows.map((row) => row[col]!.length)));
  return rows.map((row) => row.map((cell, col) => cell.padEnd(widths[col]!)).join("  ").trimEnd()).join("\n");
}

/** The comparison table, then which tasks each group passed (passed/runs). */
export function formatSummary(results: RunResult[]): string {
  if (results.length === 0) return "No runs.";
  const rows = summarizeRuns(results);
  // Likewise MCP calls: only runs with a server make any.
  const showMcp = rows.some((r) => r.mcpCalls);
  // Partial credit and the plan tool's measures (#15): only runs that recorded them.
  const showTests = rows.some((r) => r.testsTotal);
  const showPlan = rows.some((r) => r.planRuns);
  const head = [
    "label", "model", "runs", "pass", ...(showTests ? ["tests"] : []), "edits", "edit fail", "steps/run", "tools/run",
    ...(showMcp ? ["mcp/run"] : []), ...(showPlan ? ["plan/run", "plan open"] : []), "prompt/req", "$/run", "$ total", "median",
  ];
  const overview = table([
    head,
    ...rows.map((r) => [
      r.label,
      r.model,
      String(r.runs),
      pct(r.passed, r.runs),
      ...(showTests ? [pct(r.testsPassed, r.testsTotal)] : []),
      String(r.editCalls),
      pct(r.editErrors, r.editCalls),
      (r.requests / r.runs).toFixed(1),
      (r.toolCalls / r.runs).toFixed(1),
      ...(showMcp ? [(r.mcpCalls / r.runs).toFixed(1)] : []),
      ...(showPlan ? [r.planRuns ? (r.planCalls / r.planRuns).toFixed(1) : "-", r.planRuns ? String(r.planOpen) : "-"] : []),
      r.requests ? String(Math.round(r.promptTokens / r.requests)) : "-",
      (r.cost / r.runs).toFixed(4),
      r.cost.toFixed(3),
      `${(r.medianMs / 1000).toFixed(0)}s`,
    ]),
  ]);
  const tasks = [...new Set(results.map((r) => r.task))].sort();
  const grid = table([
    ["task", ...rows.map((r) => `${r.label}/${r.model}`)],
    ...tasks.map((task) => [
      task,
      ...rows.map((row) => {
        const runs = results.filter((r) => groupKey(r) === row.group && r.task === task);
        return runs.length ? `${runs.filter((r) => r.pass).length}/${runs.length}` : "-";
      }),
    ]),
  ]);
  return `${overview}\n\n${grid}`;
}
