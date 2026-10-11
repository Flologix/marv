// Reading TypeScript's own checker, for the typecheck Marv runs after a step's file changes (./turn.ts). Pure: no
// processes, and no files beyond seeing whether the project has a compiler.
import { existsSync } from "node:fs";
import { join } from "node:path";

export interface TsError {
  /** As tsc prints it (relative to the project); absent for errors about the command line or the run as a whole (tsconfig option errors are located: `tsconfig.json(1,21)`). */
  file?: string;
  line?: number;
  column?: number;
  /** "TS2345". */
  code: string;
  /** With its continuation lines (tsc's indented detail), joined by newlines. */
  message: string;
}

/** Errors the model is told per check; the rest are counted. */
export const MAX_REPORTED = 20;

/** The project's own compiler (tsgo first: TypeScript's native preview), or null when there's nothing to check with. */
export function detectChecker(root: string): string | null {
  if (!existsSync(join(root, "tsconfig.json"))) return null;
  for (const name of ["tsgo", "tsc"]) {
    const bin = join("node_modules", ".bin", name);
    if (existsSync(join(root, bin))) return bin;
  }
  return null;
}

/**
 * No output files, one plain line per error. The .tsbuildinfo goes to the sandbox's throwaway /tmp, since the project
 * is read-only (`--incremental false` would make every composite project fail with only TS6379).
 */
export const checkCommand = (bin: string) => `${bin} --noEmit --pretty false --tsBuildInfoFile /tmp/marv.tsbuildinfo -p tsconfig.json`;

const LOCATED = /^(.+)\((\d+),(\d+)\): error (TS\d+): (.*)$/;
const UNLOCATED = /^error (TS\d+): (.*)$/;

/**
 * The errors in tsc's `--pretty false` output. Null when a failed run printed none: a crash, or something else
 * answering to the name. Then nobody can tell what's new, and nothing is reported.
 */
export function parseTsc(output: string, exitCode: number | null): TsError[] | null {
  if (exitCode === 0) return [];
  const errors: TsError[] = [];
  // Only a line right after an error (or its continuation) can continue it.
  let open = false;
  const lines = output.split(/\r?\n/);
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i]!;
    // Node's own runtime warnings (stderr is merged into the output) aren't tsc's report; the second line follows the
    // first. One in the middle of an error's continuation makes the next indented line unreadable, deliberately
    // (conservative: the report can't be trusted to be whole).
    if (/^\(node:\d+\) /.test(line)) {
      if (lines[i + 1]?.startsWith("(Use `node --trace-warnings")) i++;
      open = false;
      continue;
    }
    const located = LOCATED.exec(line);
    const unlocated = located ? null : UNLOCATED.exec(line);
    if (located) errors.push({ file: located[1]!, line: Number(located[2]), column: Number(located[3]), code: located[4]!, message: located[5]! });
    else if (unlocated) errors.push({ code: unlocated[1]!, message: unlocated[2]! });
    else if (open && line.startsWith("  ") && line.trim()) {
      // Strip only tsc's own two-space indent: deeper levels of the elaboration keep theirs.
      errors.at(-1)!.message += `\n${line.slice(2).trimEnd()}`;
      continue;
    } else if (line.trim()) {
      // A panic, a stack trace, anything that isn't tsc's report: the run can't be trusted.
      return null;
    }
    open = !!(located || unlocated);
  }
  return errors.length ? errors : null;
}

const key = (e: TsError) => `${e.file ?? ""}\u0000${e.code}\u0000${e.message}`;

/**
 * The errors in `now` that `before` didn't have. Matched by file, code and message, not line: edits move lines.
 * Counted, not a set, so a second copy of an old error is new.
 */
export function newErrors(before: TsError[], now: TsError[]): TsError[] {
  const left = new Map<string, number>();
  for (const e of before) left.set(key(e), (left.get(key(e)) ?? 0) + 1);
  return now.filter((e) => {
    const n = left.get(key(e)) ?? 0;
    if (n > 0) left.set(key(e), n - 1);
    return n === 0;
  });
}

/** `src/a.ts:4:2 TS2322 message`, continuation lines indented. */
export function formatError(e: TsError): string {
  const where = e.file ? `${e.file}:${e.line}:${e.column} ` : "";
  return `${where}${e.code} ${e.message.replaceAll("\n", "\n  ")}`;
}

/** What `env` prints when the shebang interpreter isn't there (`env: ‘node’: No such file or directory`). */
const MISSING = /^env: [‘'](.+?)[’']: No such file or directory$/m;

/**
 * The interpreter a failed check run couldn't find, when that's what went wrong (a `#!/usr/bin/env node` launcher
 * whose `node` the sandbox can't see — it's under the hidden home, e.g. mise's). Its name, or undefined when the
 * failure was something else (or the run succeeded). Exit 127 is "command not found"; 126 would be "found but
 * can't run", which is a different problem.
 */
export function missingInterpreter(output: string, exitCode: number | null): string | undefined {
  if (exitCode !== 127) return undefined;
  return MISSING.exec(output)?.[1];
}

/** What the model is told: a message from Marv, like the cut-off and empty-reply notes. */
export function diagnosticsNote(added: TsError[]): string {
  const shown = added.slice(0, MAX_REPORTED).map(formatError);
  const more = added.length - shown.length;
  return [
    `Marv: the typecheck (tsc) after your changes found ${added.length} new error${added.length === 1 ? "" : "s"}:`,
    ...shown,
    ...(more ? [`and ${more} more.`] : []),
  ].join("\n");
}
