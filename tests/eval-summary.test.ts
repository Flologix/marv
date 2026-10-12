import { describe, expect, test } from "bun:test";
import { eventCost, formatSummary, summarizeRuns, type RunResult } from "../evals/summary.ts";

const run = (over: Partial<RunResult> = {}): RunResult => ({
  diagnostics: true,
  label: "base",
  model: "m1",
  task: "t1",
  rep: 0,
  pass: true,
  reason: "end",
  requests: 4,
  promptTokens: 1000,
  cachedTokens: 500,
  completionTokens: 100,
  cost: 0.01,
  ms: 1000,
  tools: {},
  editErrors: [],
  ...over,
});

describe("summarizeRuns", () => {
  test("groups by label and model, in first-seen order", () => {
    const rows = summarizeRuns([run({ model: "b" }), run({ model: "a" }), run({ model: "b", label: "new" })]);
    expect(rows.map((r) => `${r.label}/${r.model}`)).toEqual(["base/b", "base/a", "new/b"]);
  });

  test("pass rate, edit failure rate, means, totals and the median time", () => {
    const [row] = summarizeRuns([
      run({ pass: true, requests: 2, ms: 1000, cost: 0.01, tools: { edit_file: { calls: 3, errors: 1 }, read_file: { calls: 1, errors: 0 } } }),
      run({ pass: false, requests: 6, ms: 5000, cost: 0.03, tools: { edit_file: { calls: 1, errors: 1 } } }),
      run({ pass: true, requests: 4, ms: 2000, cost: 0.02 }),
    ]);
    expect(row).toMatchObject({ runs: 3, passed: 2, editCalls: 4, editErrors: 2, toolCalls: 5, requests: 12, medianMs: 2000 });
    expect(row!.cost).toBeCloseTo(0.06);
  });

  test("a run that never started (budget, a provider error) still counts as a failure", () => {
    const [row] = summarizeRuns([run({ pass: false, reason: "error", requests: 0, cost: 0 }), run({})]);
    expect(row).toMatchObject({ runs: 2, passed: 1 });
  });
});

describe("eventCost", () => {
  const usage = (cost: number) => ({ promptTokens: 10, cachedTokens: 5, completionTokens: 2, cost });
  test("the turn's usage and a subagent's carry cost", () => {
    expect(eventCost({ type: "usage", usage: usage(0.0002) })).toBeCloseTo(0.0002);
    expect(eventCost({ type: "subagent", callId: "c1", event: { type: "usage", usage: usage(0.0001) } })).toBeCloseTo(0.0001);
  });

  test("the other events carry none", () => {
    expect(eventCost({ type: "done", reason: "end" })).toBe(0);
    expect(eventCost({ type: "subagent", callId: "c1", event: { type: "done", reason: "end" } })).toBe(0);
  });
});

describe("formatSummary", () => {
  test("a row per group with pass and edit-failure percentages, then the per-task grid", () => {
    const text = formatSummary([
      run({ task: "tabs", pass: true, tools: { edit_file: { calls: 4, errors: 1 } } }),
      run({ task: "tabs", pass: false, rep: 1 }),
      run({ task: "json", pass: true }),
    ]);
    expect(text).toContain("base");
    expect(text).toContain("67%"); // 2 of 3 passed
    expect(text).toContain("25%"); // 1 of 4 edits failed
    expect(text).toMatch(/tabs\s+1\/2/);
    expect(text).toMatch(/json\s+1\/1/);
  });

  test("prompt tokens per request: what deferred MCP tools (#14) should move", () => {
    const rows = summarizeRuns([run({ requests: 2, promptTokens: 3000 }), run({ requests: 4, promptTokens: 9000 })]);
    expect(rows[0]!.promptTokens).toBe(12_000);
    expect(formatSummary([run({ requests: 2, promptTokens: 3000 }), run({ requests: 4, promptTokens: 9000 })])).toMatch(/prompt\/req[\s\S]*\b2000\b/);
  });

  test("MCP calls per run, a column only when a run made one", () => {
    expect(formatSummary([run({ mcpCalls: 3 }), run({ mcpCalls: 1 })])).toMatch(/mcp\/run[\s\S]*\b2\.0\b/);
    expect(formatSummary([run()])).not.toMatch(/mcp\/run/);
  });

  test("no runs", () => {
    expect(formatSummary([])).toBe("No runs.");
  });
});
