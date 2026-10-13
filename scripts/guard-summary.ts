// Counts the guard tests (tests/guards/, issue #53) in a `bun test --reporter=junit` report, for CI: a line for the
// job summary, and with --strict a failure when none ran or any was skipped. The guards are gated on bwrap, so a
// runner without a working sandbox would skip them all and still go green; this is what makes that red.
//
//   bun scripts/guard-summary.ts test-results.xml            prints the line
//   bun scripts/guard-summary.ts test-results.xml --strict   and exits 1 unless every guard test ran
import { existsSync, readFileSync } from "node:fs";

export interface GuardCounts {
  tests: number;
  skipped: number;
  failures: number;
  files: number;
}

/** The counts of the report's top-level suites (one per test file) whose file is under tests/guards/. */
export function guardCounts(xml: string): GuardCounts {
  const counts: GuardCounts = { tests: 0, skipped: 0, failures: 0, files: 0 };
  // Bun writes one <testsuite> per file with its totals, and nested ones per describe with the same file:
  // only the per-file ones (name equal to file) are added up, or a describe would count twice.
  for (const [tag] of xml.matchAll(/<testsuite\s[^>]*>/g)) {
    const attr = (name: string) => new RegExp(`\\s${name}="([^"]*)"`).exec(tag)?.[1];
    const file = attr("file");
    if (!file?.startsWith("tests/guards/") || attr("name") !== file) continue;
    counts.files++;
    counts.tests += Number(attr("tests") ?? 0);
    counts.skipped += Number(attr("skipped") ?? 0);
    counts.failures += Number(attr("failures") ?? 0);
  }
  return counts;
}

/** The job summary's line, and whether every guard test ran. */
export function guardSummary(counts: GuardCounts): { line: string; ok: boolean } {
  const ran = counts.tests - counts.skipped;
  const ok = counts.tests > 0 && counts.skipped === 0;
  const failed = counts.failures ? `, ${counts.failures} failed` : "";
  const line = ok
    ? `guards: ${ran} guard tests ran, 0 skipped${failed} (${counts.files} files in tests/guards).`
    : counts.tests === 0
      ? "guards: **no guard tests ran** (none found in the report)."
      : `guards: ${ran} guard tests ran, **${counts.skipped} skipped**${failed} (${counts.files} files in tests/guards).`;
  return { line, ok };
}

if (import.meta.main) {
  const [file, flag] = process.argv.slice(2);
  if (!file) throw new Error("usage: bun scripts/guard-summary.ts <junit.xml> [--strict]");
  const { line, ok } = existsSync(file) ? guardSummary(guardCounts(readFileSync(file, "utf8"))) : { line: `guards: no test report (${file}): the tests didn't run.`, ok: false };
  console.log(line);
  if (flag === "--strict" && !ok) process.exit(1);
}
