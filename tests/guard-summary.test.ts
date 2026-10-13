// scripts/guard-summary.ts: CI counts the guard tests (tests/guards, #53) in bun's JUnit report and fails when any
// was skipped. The reports here have Bun's shape: one <testsuite> per file, nested ones per describe.
import { describe, expect, test } from "bun:test";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { guardCounts, guardSummary } from "../scripts/guard-summary.ts";

const report = (suites: string) => `<?xml version="1.0" encoding="UTF-8"?>\n<testsuites name="bun test" tests="9" skipped="0">\n${suites}\n</testsuites>`;
const file = (path: string, tests: number, skipped = 0, failures = 0) =>
  `  <testsuite name="${path}" file="${path}" tests="${tests}" assertions="1" failures="${failures}" skipped="${skipped}" time="0">\n` +
  `    <testsuite name="a describe" file="${path}" line="3" tests="${tests}" assertions="1" failures="${failures}" skipped="${skipped}" time="0">\n` +
  `      <testcase name="x" classname="a describe" time="0" file="${path}" line="4" assertions="1" />\n    </testsuite>\n  </testsuite>`;

describe("guardCounts", () => {
  test("adds up the guard files only, each once (not again for its describes)", () => {
    const xml = report([file("tests/guards/sandbox.test.ts", 10), file("tests/guards/yolo.test.ts", 15, 0, 1), file("tests/bash.test.ts", 40, 3)].join("\n"));
    expect(guardCounts(xml)).toEqual({ tests: 25, skipped: 0, failures: 1, files: 2 });
  });

  test("counts skipped guards (no bwrap)", () => {
    expect(guardCounts(report(file("tests/guards/sandbox.test.ts", 10, 10)))).toEqual({ tests: 10, skipped: 10, failures: 0, files: 1 });
  });
});

describe("guardSummary", () => {
  test("ok only when guards ran and none was skipped", () => {
    expect(guardSummary({ tests: 25, skipped: 0, failures: 0, files: 2 })).toEqual({ ok: true, line: "guards: 25 guard tests ran, 0 skipped (2 files in tests/guards)." });
    expect(guardSummary({ tests: 25, skipped: 4, failures: 0, files: 2 })).toEqual({ ok: false, line: "guards: 21 guard tests ran, **4 skipped** (2 files in tests/guards)." });
    expect(guardSummary({ tests: 0, skipped: 0, failures: 0, files: 0 }).ok).toBe(false);
  });

  test("the real suite, run here, is counted", async () => {
    // Not the whole suite: one guard file is enough to see the script read Bun's actual report.
    const dir = await mkdtemp(join(tmpdir(), "marv-guard-summary-"));
    const out = join(dir, "report.xml");
    const proc = Bun.spawn([process.execPath, "test", "tests/guards/yolo.test.ts", "--reporter=junit", `--reporter-outfile=${out}`], { cwd: `${import.meta.dir}/..`, stdout: "ignore", stderr: "ignore" });
    expect(await proc.exited).toBe(0);
    const counts = guardCounts(await Bun.file(out).text());
    expect(counts.files).toBe(1);
    expect(counts.tests).toBeGreaterThan(0);
    await rm(dir, { recursive: true, force: true });
  }, 30_000);
});
