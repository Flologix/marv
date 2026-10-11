import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { checkNotice, Diagnostics, type CheckResult, type RunCheck } from "../src/diagnostics/turn.ts";
import { MAX_CAPTURE, type CommandResult } from "../src/tools/bash.ts";
import type { ToolResult } from "../src/tools/types.ts";

let root: string;
beforeEach(() => {
  root = mkdtempSync(join(tmpdir(), "marv-checks-"));
  writeFileSync(join(root, "tsconfig.json"), "{}");
  mkdirSync(join(root, "node_modules", ".bin"), { recursive: true });
  writeFileSync(join(root, "node_modules", ".bin", "tsc"), "");
});
afterEach(() => rmSync(root, { recursive: true, force: true }));

const ok = (output = ""): CommandResult => ({ output, exitCode: 0, timedOut: false, aborted: false });
const failed = (output: string): CommandResult => ({ output, exitCode: 2, timedOut: false, aborted: false });
const ERR_X = "src/a.ts(1,1): error TS2304: Cannot find name 'x'.\n";
const ERR_Y = "src/b.ts(2,3): error TS2304: Cannot find name 'y'.\n";
const changed: ToolResult[] = [{ output: "Edited a.ts", summary: "+1 −1", diff: { lines: [], more: 0 } }];
const unchanged: ToolResult[] = [{ output: "contents", summary: "1 line" }];

/** A stand-in for the sandboxed tsc: answers each run with the next result, and counts the runs. */
function fake(...answers: CommandResult[]): RunCheck & { runs: number } {
  const run = Object.assign(async () => (run.runs++, answers.shift() ?? ok()), { runs: 0 });
  return run;
}

function turn(diagnostics: Diagnostics, { sandbox = true, signal = new AbortController().signal, onResult }: { sandbox?: boolean; signal?: AbortSignal; onResult?: (r: CheckResult) => void } = {}) {
  const results: CheckResult[] = [];
  const statuses: boolean[] = [];
  const checks = diagnostics.turn({ root, sandbox, signal, onStatus: (s) => statuses.push(s), onResult: (r) => (results.push(r), onResult?.(r)) });
  return { checks, results, statuses };
}

describe("a turn's checks", () => {
  test("the baseline is taken once, however many changes ask for it", async () => {
    const run = fake(ok());
    const { checks } = turn(new Diagnostics({ run, sandboxWorks: () => true }));
    await Promise.all([checks.beforeChange(), checks.beforeChange()]);
    await checks.beforeChange();
    expect(run.runs).toBe(1);
  });

  test("after a step that changed a file, only the errors the step added are told, and they become the next baseline", async () => {
    const run = fake(failed(ERR_X), failed(ERR_X + ERR_Y), failed(ERR_X + ERR_Y));
    const { checks, results, statuses } = turn(new Diagnostics({ run, sandboxWorks: () => true }));
    await checks.beforeChange();
    const note = await checks.afterStep(changed);
    expect(note).toContain("found 1 new error");
    expect(note).toContain("src/b.ts:2:3 TS2304 Cannot find name 'y'.");
    expect(note).not.toContain("'x'");
    expect(results).toEqual([{ status: "done", ms: expect.any(Number), before: 1, errors: 2, added: [expect.objectContaining({ file: "src/b.ts" })] }]);
    expect(statuses).toEqual([true, false]);
    // The same errors again: nothing new.
    expect(await checks.afterStep(changed)).toBeNull();
  });

  test("a step that changed no file isn't checked", async () => {
    const run = fake(ok());
    const { checks } = turn(new Diagnostics({ run, sandboxWorks: () => true }));
    await checks.beforeChange();
    expect(await checks.afterStep(unchanged)).toBeNull();
    expect(run.runs).toBe(1);
  });

  test("without a baseline this turn (nothing was changed through the edit tools), nothing runs", async () => {
    const run = fake();
    const { checks } = turn(new Diagnostics({ run, sandboxWorks: () => true }));
    expect(await checks.afterStep(changed)).toBeNull();
    expect(run.runs).toBe(0);
  });

  test("not a TypeScript project: nothing runs", async () => {
    rmSync(join(root, "tsconfig.json"));
    const run = fake();
    const { checks, results } = turn(new Diagnostics({ run, sandboxWorks: () => true }));
    await checks.beforeChange();
    expect(await checks.afterStep(changed)).toBeNull();
    expect(run.runs).toBe(0);
    expect(results).toEqual([]);
  });

  test("without the sandbox nothing runs, and the first change says so once per session", async () => {
    const run = fake();
    const diagnostics = new Diagnostics({ run, sandboxWorks: () => true });
    const first = turn(diagnostics, { sandbox: false });
    await first.checks.beforeChange();
    await first.checks.beforeChange();
    expect(await first.checks.afterStep(changed)).toBeNull();
    const second = turn(diagnostics, { sandbox: false });
    await second.checks.beforeChange();
    expect(run.runs).toBe(0);
    expect([...first.results, ...second.results]).toEqual([{ status: "off", reason: "no-sandbox" }]);
  });

  test("bwrap not working counts as no sandbox", async () => {
    const run = fake();
    const { checks, results } = turn(new Diagnostics({ run, sandboxWorks: () => false }));
    await checks.beforeChange();
    expect(run.runs).toBe(0);
    expect(results).toEqual([{ status: "off", reason: "no-sandbox" }]);
  });

  test("two timeouts turn checking off for the session, said once", async () => {
    const timedOut: CommandResult = { output: "", exitCode: null, timedOut: true, aborted: false };
    const run = fake(timedOut, timedOut);
    const diagnostics = new Diagnostics({ run, sandboxWorks: () => true });
    const first = turn(diagnostics);
    await first.checks.beforeChange();
    expect(await first.checks.afterStep(changed)).toBeNull(); // no baseline: nothing to compare with
    const second = turn(diagnostics);
    await second.checks.beforeChange();
    const third = turn(diagnostics);
    await third.checks.beforeChange();
    expect(run.runs).toBe(2);
    expect([...first.results, ...second.results, ...third.results].map((r) => r.status)).toEqual(["failed", "failed", "off"]);
    expect(second.results.at(-1)).toEqual({ status: "off", reason: "timeouts" });
  });

  test("output that can't be read tells the model nothing", async () => {
    const run = fake(ok(), failed("Segmentation fault\n"));
    const { checks, results } = turn(new Diagnostics({ run, sandboxWorks: () => true }));
    await checks.beforeChange();
    expect(await checks.afterStep(changed)).toBeNull();
    expect(results).toEqual([{ status: "failed", ms: expect.any(Number), reason: "unreadable", notify: true }]);
  });

  test("a missing interpreter (node from mise, hidden by the sandbox) is its own failure", async () => {
    const run = fake(ok(), { output: "env: ‘node’: No such file or directory\n", exitCode: 127, timedOut: false, aborted: false });
    const { checks, results } = turn(new Diagnostics({ run, sandboxWorks: () => true }));
    await checks.beforeChange();
    expect(await checks.afterStep(changed)).toBeNull();
    expect(results).toEqual([{ status: "failed", ms: expect.any(Number), reason: "missing-interpreter", tool: "node", notify: true }]);
  });

  test("a missing interpreter counts as unreadable for the repeat notices", async () => {
    const lost: CommandResult = { output: "env: ‘node’: No such file or directory\n", exitCode: 127, timedOut: false, aborted: false };
    const run = fake(ok(), lost, lost);
    const { checks, results } = turn(new Diagnostics({ run, sandboxWorks: () => true }));
    await checks.beforeChange();
    await checks.afterStep(changed);
    await checks.afterStep(changed);
    expect(results.map((r) => (r as { notify?: boolean }).notify)).toEqual([true, undefined]);
  });

  test("only the session's first unreadable result asks for a notice", async () => {
    const run = fake(ok(), failed("Segmentation fault\n"), failed("Segmentation fault\n"));
    const { checks, results } = turn(new Diagnostics({ run, sandboxWorks: () => true }));
    await checks.beforeChange();
    await checks.afterStep(changed);
    await checks.afterStep(changed);
    expect(results.map((r) => (r as { notify?: boolean }).notify)).toEqual([true, undefined]);
  });

  test("a result arriving after the turn was stopped isn't reported and doesn't use up the one-time notice", async () => {
    const stop = new AbortController();
    const diagnostics = new Diagnostics({ run: async () => (stop.abort(), failed("Segmentation fault\n")), sandboxWorks: () => true });
    const first = turn(diagnostics, { signal: stop.signal });
    await first.checks.beforeChange();
    expect(first.results).toEqual([]);
    const later = turn(new Diagnostics({ run: fake(failed("x\n")), sandboxWorks: () => true }));
    await later.checks.beforeChange();
    expect(later.results).toEqual([{ status: "failed", ms: expect.any(Number), reason: "unreadable", notify: true }]);
    // The same session, in a new turn: the stopped one didn't spend the notice.
    const again = turn(diagnostics);
    await again.checks.beforeChange();
    expect(again.results).toEqual([{ status: "failed", ms: expect.any(Number), reason: "unreadable", notify: true }]);
  });

  test("reset() brings back checking after timeouts, and the one-time notices", async () => {
    const timedOut: CommandResult = { output: "", exitCode: null, timedOut: true, aborted: false };
    const run = fake(timedOut, timedOut, failed("Segmentation fault\n"));
    const diagnostics = new Diagnostics({ run, sandboxWorks: () => true });
    for (let i = 0; i < 2; i++) await turn(diagnostics).checks.beforeChange();
    expect(run.runs).toBe(2);
    await turn(diagnostics).checks.beforeChange();
    expect(run.runs).toBe(2); // off
    diagnostics.reset();
    const { checks, results } = turn(diagnostics);
    await checks.beforeChange();
    expect(run.runs).toBe(3);
    expect(results).toEqual([{ status: "failed", ms: expect.any(Number), reason: "unreadable", notify: true }]);
  });

  test("output at the capture limit may be cut mid-list, so it's unreadable", async () => {
    const run = fake(ok(), failed(ERR_Y.repeat(Math.ceil(MAX_CAPTURE / ERR_Y.length))));
    const { checks, results } = turn(new Diagnostics({ run, sandboxWorks: () => true }));
    await checks.beforeChange();
    expect(await checks.afterStep(changed)).toBeNull();
    expect(results).toEqual([{ status: "failed", ms: expect.any(Number), reason: "unreadable", notify: true }]);
  });

  test("a check stopped by Esc tells the model nothing and isn't a failure", async () => {
    const run = fake(ok(), { output: "", exitCode: null, timedOut: false, aborted: true });
    const { checks, results } = turn(new Diagnostics({ run, sandboxWorks: () => true }));
    await checks.beforeChange();
    expect(await checks.afterStep(changed)).toBeNull();
    expect(results).toEqual([]);
  });

  test("a runner that throws counts as unreadable, and beforeChange never rejects", async () => {
    const run: RunCheck = async () => {
      throw new Error("spawn failed");
    };
    const { checks, results } = turn(new Diagnostics({ run, sandboxWorks: () => true }));
    await checks.beforeChange();
    expect(results).toEqual([{ status: "failed", ms: expect.any(Number), reason: "unreadable", notify: true }]);
  });
});

describe("a turn's checks, when a check can't be used", () => {
  const timedOut: CommandResult = { output: "", exitCode: null, timedOut: true, aborted: false };
  const aborted: CommandResult = { output: "", exitCode: null, timedOut: false, aborted: true };

  test("a step check that can't be read leaves the rest of the turn unchecked; the next turn starts fresh", async () => {
    const run = fake(ok(), failed("Segmentation fault\n"), ok(), failed(ERR_X));
    const diagnostics = new Diagnostics({ run, sandboxWorks: () => true });
    const first = turn(diagnostics);
    await first.checks.beforeChange();
    expect(await first.checks.afterStep(changed)).toBeNull();
    expect(await first.checks.afterStep(changed)).toBeNull();
    expect(run.runs).toBe(2);
    const second = turn(diagnostics);
    await second.checks.beforeChange();
    expect(await second.checks.afterStep(changed)).toContain("found 1 new error");
    expect(run.runs).toBe(4);
  });

  test("after a step check times out, no more checks run this turn; the next turn starts fresh", async () => {
    const run = fake(ok(), timedOut, ok(), failed(ERR_X));
    const diagnostics = new Diagnostics({ run, sandboxWorks: () => true });
    const first = turn(diagnostics);
    await first.checks.beforeChange();
    expect(await first.checks.afterStep(changed)).toBeNull();
    expect(await first.checks.afterStep(changed)).toBeNull();
    expect(run.runs).toBe(2);
    const second = turn(diagnostics);
    await second.checks.beforeChange();
    expect(await second.checks.afterStep(changed)).not.toBeNull();
  });

  test("the second timeout of a session, in a step check, turns checking off", async () => {
    const run = fake(timedOut, ok(), timedOut);
    const diagnostics = new Diagnostics({ run, sandboxWorks: () => true });
    await turn(diagnostics).checks.beforeChange();
    const second = turn(diagnostics);
    await second.checks.beforeChange();
    expect(await second.checks.afterStep(changed)).toBeNull();
    expect(second.results.at(-1)).toEqual({ status: "off", reason: "timeouts" });
    expect(await second.checks.afterStep(changed)).toBeNull();
    expect(run.runs).toBe(3);
  });

  test("a signal aborted before the step: nothing runs, no status", async () => {
    const controller = new AbortController();
    const run = fake(ok());
    const { checks, statuses } = turn(new Diagnostics({ run, sandboxWorks: () => true }), { signal: controller.signal });
    await checks.beforeChange();
    controller.abort();
    expect(await checks.afterStep(changed)).toBeNull();
    expect(run.runs).toBe(1);
    expect(statuses).toEqual([]);
  });

  test("a step check stopped by the runner still ends the status", async () => {
    const run = fake(ok(), aborted);
    const { checks, statuses } = turn(new Diagnostics({ run, sandboxWorks: () => true }));
    await checks.beforeChange();
    expect(await checks.afterStep(changed)).toBeNull();
    expect(statuses).toEqual([true, false]);
  });

  test("Esc right as a check finishes: nothing is told or emitted", async () => {
    const controller = new AbortController();
    let n = 0;
    const run: RunCheck = async () => {
      if (n++ === 0) return ok();
      controller.abort();
      return failed(ERR_X);
    };
    const { checks, results } = turn(new Diagnostics({ run, sandboxWorks: () => true }), { signal: controller.signal });
    await checks.beforeChange();
    expect(await checks.afterStep(changed)).toBeNull();
    expect(results).toEqual([]);
  });

  test("callbacks that throw can't make beforeChange reject or afterStep throw", async () => {
    const run = fake(failed(ERR_X), failed(ERR_X + ERR_Y));
    const diagnostics = new Diagnostics({ run, sandboxWorks: () => true });
    const checks = diagnostics.turn({
      root,
      sandbox: true,
      signal: new AbortController().signal,
      onStatus: () => {
        throw new Error("ui");
      },
      onResult: () => {
        throw new Error("ui");
      },
    });
    await checks.beforeChange();
    expect(await checks.afterStep(changed)).toContain("found 1 new error");
  });
});

describe("checkNotice", () => {
  test("new errors: the first five, then how many more", () => {
    const added = Array.from({ length: 7 }, (_, i) => ({ file: "a.ts", line: i + 1, column: 1, code: "TS2304", message: `Cannot find name 'v${i}'.` }));
    const text = checkNotice({ status: "done", ms: 5, before: 0, errors: 7, added })!;
    expect(text.split("\n")[0]).toBe("✻ Typecheck: 7 new errors, told the model:");
    expect(text.split("\n")).toHaveLength(1 + 5 + 1);
    expect(text).toContain("… and 2 more");
  });

  test("nothing for a clean check or a failure; one line when checking is off", () => {
    expect(checkNotice({ status: "done", ms: 5, before: 0, errors: 0, added: [] })).toBeNull();
    expect(checkNotice({ status: "failed", ms: 5, reason: "timeout" })).toBeNull();
    expect(checkNotice({ status: "failed", ms: 5, reason: "unreadable" })).toBeNull();
    expect(checkNotice({ status: "failed", ms: 5, reason: "unreadable", notify: true })).toBe("✻ Marv couldn't read the typecheck's output here, so it can't tell the model about new errors.");
    expect(checkNotice({ status: "failed", ms: 5, reason: "missing-interpreter", tool: "node" })).toBeNull();
    expect(checkNotice({ status: "failed", ms: 5, reason: "missing-interpreter", tool: "node", notify: true })).toBe(
      "✻ tsc needs node, which the sandbox can't see (Marv's sandbox hides the home folder except known toolchains). Marv can't tell the model about new type errors.",
    );
    expect(checkNotice({ status: "off", reason: "no-sandbox" })).toContain("sandbox");
    expect(checkNotice({ status: "off", reason: "timeouts" })).toContain("stopped");
  });

  test("off after timeouts doesn't name a duration", () => {
    expect(checkNotice({ status: "off", reason: "timeouts" })).toBe("✻ The typecheck timed out twice, so Marv stopped running it for this session.");
  });

  test("escape sequences in an error message never reach the terminal", () => {
    const text = checkNotice({ status: "done", ms: 5, before: 0, errors: 1, added: [{ file: "a.ts", line: 1, column: 1, code: "TS2322", message: "Type '\u001b]52;c;aGk=\u0007' is wrong." }] })!;
    expect(text).not.toContain("\u001b");
  });
});
