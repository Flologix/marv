// The project's checks as a kind of check for src/diagnostics/turn.ts: which ones a step's changes concern, the
// sandboxed command that runs them (./runner.ts), and how to read and tell what they found.
import { printable } from "../printable.ts";
import { Checker, type CheckKind, type CheckOutcome, type RunCheck } from "../diagnostics/turn.ts";
import { CHECKS_DIR, concerns, loadChecks, type ProjectCheck, type Violation } from "./load.ts";
import { runChecks, type RunnerSpec } from "./runner.ts";

/** Per script check, inside the whole run's CHECK_TIMEOUT_MS. */
export const SCRIPT_TIMEOUT_MS = 30_000;
/** Violations the model is told per check pass; the rest are counted. */
export const MAX_REPORTED_VIOLATIONS = 20;
const NOTICE_LINES = 5;

export type ProjectCheckResult = CheckOutcome<Violation>;

const quote = (s: string) => `'${s.replaceAll("'", `'\\''`)}'`;

/** The sandboxed command: `bun -e` with the runner's source and the checks to run. */
export function runnerCommand(checks: ProjectCheck[], timeoutMs = SCRIPT_TIMEOUT_MS): string {
  const spec: RunnerSpec = { checks, timeoutMs };
  return `bun -e ${quote(`(${runChecks.toString()})(${JSON.stringify(spec)})`)}`;
}

/** The violations in the runner's output; null when it isn't the runner's report (cut off, a crash, anything else). */
export function parseRunner(output: string, exitCode: number | null): Violation[] | null {
  if (exitCode !== 0) return null;
  const lines = output.split("\n").filter((l) => l.trim());
  if (lines.length === 0) return null;
  const found: Violation[] = [];
  for (const [i, line] of lines.entries()) {
    let record: Record<string, unknown>;
    try {
      record = JSON.parse(line);
    } catch {
      return null;
    }
    if (!record || typeof record !== "object") return null;
    if (record.done === true) return i === lines.length - 1 ? found : null;
    if (typeof record.check !== "string") return null;
    if (typeof record.error === "string") {
      const why = typeof record.detail === "string" ? { why: `its stderr ended: ${record.detail}` } : {};
      found.push({ check: record.check, message: `the check failed: ${record.error}`, ...why, failed: true });
      continue;
    }
    if (typeof record.message !== "string") return null;
    found.push({
      check: record.check,
      message: record.message,
      ...(typeof record.file === "string" ? { file: record.file } : {}),
      ...(typeof record.line === "number" ? { line: record.line } : {}),
      ...(typeof record.why === "string" ? { why: record.why } : {}),
    });
  }
  return null; // no `done`: it stopped early
}

/** What makes two violations the same one: the check, the file and the message, not the line (edits move lines). */
export const violationKey = (v: Violation) => `${v.check}\u0000${v.file ?? ""}\u0000${v.message}`;

/** `src/a.ts:3 [imports] message`. */
export function formatViolation(v: Violation): string {
  const where = v.file ? `${v.file}${v.line ? `:${v.line}` : ""} ` : "";
  return `${where}[${v.check}] ${v.message}`;
}

/** What the model is told: each new violation, then each check's why, once. A message from Marv, like the typecheck's. */
export function checksNote(added: Violation[]): string {
  const shown = added.slice(0, MAX_REPORTED_VIOLATIONS);
  const more = added.length - shown.length;
  const whys = new Map<string, string>();
  for (const v of shown) if (v.why) whys.set(`${v.check}\u0000${v.why}`, `Why [${v.check}]: ${v.why}`);
  return [
    `Marv: the project's checks (${CHECKS_DIR}/) after your changes found ${added.length} new problem${added.length === 1 ? "" : "s"}:`,
    ...shown.map(formatViolation),
    ...(more ? [`and ${more} more.`] : []),
    ...whys.values(),
  ].join("\n");
}

/** The project's checks as a kind of check: loaded again each turn, so a check added mid-session counts. */
export const projectChecks: CheckKind<Violation> = {
  prepare(root) {
    let { checks } = loadChecks(root);
    if (checks.length === 0) return null;
    return {
      pass(changed) {
        // A check edited (or added) during the turn runs as it now reads; the baseline from before the edit stays
        // what its new violations are compared with.
        if (changed === null || changed.some((path) => path.startsWith(`${CHECKS_DIR}/`))) checks = loadChecks(root).checks;
        const run = changed === null ? checks : checks.filter((c) => changed.some((path) => concerns(c, path)));
        if (run.length === 0) return null;
        const names = new Set(run.map((c) => c.name));
        return { command: runnerCommand(run), covers: (v) => names.has(v.check) };
      },
    };
  },
  parse: parseRunner,
  // bun itself isn't in the sandbox (installed under the hidden home, somewhere it doesn't show).
  missing: (output, exitCode) => (exitCode === 127 && /\bbun: (command )?not found/.test(output) ? "bun" : undefined),
  key: violationKey,
  note: checksNote,
};

/** The project checks' session state (one per session). */
export class ProjectChecks extends Checker<Violation> {
  constructor(opts: { run?: RunCheck; timeoutMs?: number; sandboxWorks?: () => boolean } = {}) {
    super(projectChecks, opts);
  }
}

/** What the transcript shows for a check pass, or null for nothing. Messages quote the project: printable. */
export function projectCheckNotice(result: ProjectCheckResult): string | null {
  if (result.status === "off") {
    return result.reason === "no-sandbox"
      ? `✻ Marv runs the project's checks (${CHECKS_DIR}/) only in the sandbox (they're the project's own code), so it won't here.`
      : "✻ The project's checks timed out twice, so Marv stopped running them for this session.";
  }
  if (result.status === "failed") {
    if (!result.notify) return null;
    return result.reason === "missing-interpreter"
      ? `✻ The project's checks need ${result.tool}, which the sandbox can't see, so Marv can't run them.`
      : "✻ Marv couldn't read the project checks' output here, so it can't tell the model about new problems.";
  }
  if (result.added.length === 0) return null;
  const n = result.added.length;
  const shown = result.added.slice(0, NOTICE_LINES).map((v) => printable(formatViolation(v)));
  return [`✻ Checks: ${n} new problem${n === 1 ? "" : "s"}, told the model:`, ...shown, ...(n > shown.length ? [`… and ${n - shown.length} more`] : [])].join("\n");
}
