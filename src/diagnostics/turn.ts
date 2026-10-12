// Checks after a step's file changes (docs/superpowers/specs/2026-10-08-post-edit-diagnostics-design.md): the
// typecheck (./tsc.ts) and the project's own checks (src/checks). A baseline is taken before the turn's first change;
// after each step that changed a file, the check runs again and the model is told only the problems that are new,
// anywhere in the project. What runs is the project's own code (its compiler, its check scripts), which a cloned
// repository could have planted, so it runs in the sandbox with the project read-only, and without the sandbox it
// doesn't run at all. `Checker` is that machinery; a `CheckKind` says what to run and how to read it.
import { printable } from "../printable.ts";
import { sandboxAvailable } from "../sandbox.ts";
import { MAX_CAPTURE, runCommand, worktreeGit, type CommandResult } from "../tools/bash.ts";
import type { ToolResult } from "../tools/types.ts";
import { checkCommand, detectChecker, diagnosticsNote, formatError, missingInterpreter, parseTsc, tsErrorKey, type TsError } from "./tsc.ts";

/** Per check. TypeScript 7 checks Marv itself (24k lines) in half a second; TypeScript 5 is about ten times slower. */
export const CHECK_TIMEOUT_MS = 60_000;
/** Timeouts after which a session stops checking: a huge project would otherwise add a minute to every step. */
const MAX_TIMEOUTS = 2;
/** Errors the transcript shows (the model gets up to MAX_REPORTED). */
const NOTICE_LINES = 5;

/** How a check went, for one kind of problem `E` (the typecheck's TsError, a project check's Violation). */
export type CheckOutcome<E> =
  /** It ran. `added`: the problems the model was told about (none: nothing was said). */
  | { status: "done"; ms: number; before: number; errors: number; added: E[] }
  /**
   * It couldn't tell what's new, so nothing was said. `notify`: the session's first such result, which the
   * transcript says once (otherwise this stays silent forever). `missing-interpreter`: the run died because the
   * checker's interpreter isn't in the sandbox (usually tsc's `node`, from a toolchain folder the sandbox hides,
   * e.g. mise's or asdf's); `tool` names it, so the notice can say that instead of "unreadable".
   */
  | { status: "failed"; ms: number; reason: "timeout" | "unreadable" | "missing-interpreter"; tool?: string; notify?: boolean }
  /** No checks from here on (said once per session). */
  | { status: "off"; reason: "no-sandbox" | "timeouts" };

/** The typecheck's result. */
export type CheckResult = CheckOutcome<TsError>;

/** Runs the check command; tests pass a stand-in. */
export type RunCheck = (command: string, opts: { root: string; signal: AbortSignal; timeoutMs: number }) => Promise<CommandResult>;

// The repository's .git (a worktree's common one too) read-only, so a check can ask git what changed.
const sandboxed: RunCheck = (command, { root, signal, timeoutMs }) =>
  runCommand({ command, root, sandbox: true, network: false, projectReadOnly: true, readOnly: worktreeGit(root), timeoutMs, signal });

/** One pass of a check: the command, and which problems it can report (all, when absent). */
export interface CheckPass<E> {
  command: string;
  /** The problems this pass re-checks: the rest of the baseline stands. */
  covers?: (problem: E) => boolean;
}

/** What one kind of check is: what to run, and how to read what it printed. */
export interface CheckKind<E> {
  /**
   * What can run in this project this turn, or null (nothing to check: no compiler, no checks). `pass(null)` is
   * the baseline (everything); `pass(files)` a step that changed those files (project-relative), or null when
   * nothing it checks could have changed, which then costs nothing.
   */
  prepare(root: string): { pass(changed: string[] | null): CheckPass<E> | null } | null;
  /** The problems in a run's output, or null when it can't be read (a crash, cut-off output). */
  parse(output: string, exitCode: number | null): E[] | null;
  /** The interpreter a failed run couldn't find in the sandbox, if that's what happened. */
  missing(output: string, exitCode: number | null): string | undefined;
  /** What makes two problems the same one (not the line: edits move lines). */
  key(problem: E): string;
  /** What the model is told about new problems. */
  note(added: E[]): string;
}

/** The problems in `now` that `before` didn't have, matched by `key`; counted, so a second copy of an old one is new. */
export function newProblems<E>(before: E[], now: E[], key: (e: E) => string): E[] {
  const left = new Map<string, number>();
  for (const e of before) left.set(key(e), (left.get(key(e)) ?? 0) + 1);
  return now.filter((e) => {
    const n = left.get(key(e)) ?? 0;
    if (n > 0) left.set(key(e), n - 1);
    return n === 0;
  });
}

/** What the session hands the tools (beforeChange) and the loop (afterStep) for one turn. */
export interface TurnChecks {
  /** Takes the baseline before the turn's first change; later calls wait for the same one. Never rejects. */
  beforeChange(): Promise<void>;
  /** After a step: the note for the model when its file changes added problems, otherwise null. */
  afterStep(results: ToolResult[]): Promise<string | null>;
}

export const NO_CHECKS: TurnChecks = { beforeChange: async () => {}, afterStep: async () => null };

interface TurnOptions<E> {
  root: string;
  /** The session's sandbox setting: checks need it on, and bwrap working. */
  sandbox: boolean;
  /**
   * Must be aborted when the turn ends, however it ends: a baseline started by a preview whose call was then
   * declined keeps running otherwise, and its orphaned timeout would count against the session.
   */
  signal: AbortSignal;
  /** True while a step's check runs ("Checking…"), false after. */
  onStatus: (checking: boolean) => void;
  onResult: (result: CheckOutcome<E>) => void;
}

/** The files a step's results changed, or null when one of them doesn't say which (then everything is checked). */
function changedFiles(results: ToolResult[]): string[] | null {
  const files: string[] = [];
  for (const r of results.filter((r) => r.diff)) {
    if (r.changed === undefined) return null;
    files.push(r.changed);
  }
  return files;
}

/** One per kind of check and session: what lasts from turn to turn. */
export class Checker<E> {
  private timeouts = 0;
  private toldNoSandbox = false;
  private toldUnreadable = false;

  constructor(
    private readonly kind: CheckKind<E>,
    private readonly opts: { run?: RunCheck; timeoutMs?: number; sandboxWorks?: () => boolean } = {},
  ) {}

  /** Checking was turned off and on again: forget what made the session give up, and the notices already given. */
  reset(): void {
    this.timeouts = 0;
    this.toldNoSandbox = false;
    this.toldUnreadable = false;
  }

  turn({ root, sandbox, signal, onStatus, onResult }: TurnOptions<E>): TurnChecks {
    if (this.timeouts >= MAX_TIMEOUTS) return NO_CHECKS;
    const plan = this.kind.prepare(root);
    if (!plan) return NO_CHECKS;
    if (!sandbox || !(this.opts.sandboxWorks ?? sandboxAvailable)()) {
      return {
        // Said when it would have mattered (the first change), not on every turn of a project nobody edits.
        beforeChange: async () => {
          if (this.toldNoSandbox) return;
          this.toldNoSandbox = true;
          try {
            onResult({ status: "off", reason: "no-sandbox" });
          } catch {}
        },
        afterStep: async () => null,
      };
    }
    const kind = this.kind;
    // Callbacks are the UI's: whatever they do, a check never throws into the turn.
    const tell = (r: CheckOutcome<E>) => {
      try {
        onResult(r);
      } catch {}
    };
    const setStatus = (checking: boolean) => {
      try {
        onStatus(checking);
      } catch {}
    };
    const run = this.opts.run ?? sandboxed;
    // A check that failed (not one Esc stopped) makes checking unavailable for the rest of this turn.
    let unavailable = false;
    // The first unreadable-or-worse result of the session asks for a notice; later ones don't repeat it.
    const notifyOnce = (): { notify?: boolean } => {
      const notify = !this.toldUnreadable;
      this.toldUnreadable = true;
      return notify ? { notify } : {};
    };
    const unreadable = (ms: number): CheckOutcome<E> => ({ status: "failed", ms, reason: "unreadable", ...notifyOnce() });
    const check = async (command: string): Promise<{ errors: E[]; ms: number } | null> => {
      const started = Date.now();
      let result: CommandResult;
      try {
        result = await run(command, { root, signal, timeoutMs: this.opts.timeoutMs ?? CHECK_TIMEOUT_MS });
      } catch {
        if (signal.aborted) return null;
        unavailable = true;
        tell(unreadable(Date.now() - started));
        return null;
      }
      const ms = Date.now() - started;
      if (result.aborted || signal.aborted) return null; // Esc: the turn is ending, nothing to say
      if (result.timedOut) {
        this.timeouts++;
        unavailable = true;
        tell({ status: "failed", ms, reason: "timeout" });
        if (this.timeouts === MAX_TIMEOUTS) tell({ status: "off", reason: "timeouts" });
        return null;
      }
      // A `#!/usr/bin/env node` launcher whose node the sandbox can't see (hidden home, e.g. mise's): that's worth
      // saying plainly, not as an unreadable output.
      const missing = kind.missing(result.output, result.exitCode);
      // Output at the capture limit was cut off, maybe mid-list: what's left would make problems look fixed or new.
      const errors = !missing && result.output.length < MAX_CAPTURE ? kind.parse(result.output, result.exitCode) : null;
      if (!errors) {
        unavailable = true;
        tell(missing ? { status: "failed", ms, reason: "missing-interpreter", tool: missing, ...notifyOnce() } : unreadable(ms));
        return null;
      }
      return { errors, ms };
    };
    let baseline: Promise<{ errors: E[]; ms: number } | null> | undefined;
    return {
      beforeChange: async () => {
        await (baseline ??= (async () => {
          const pass = plan.pass(null);
          return pass ? check(pass.command) : { errors: [], ms: 0 };
        })());
      },
      afterStep: async (results) => {
        if (!baseline || !results.some((r) => r.diff)) return null;
        const before = await baseline;
        if (!before || unavailable || signal.aborted || this.timeouts >= MAX_TIMEOUTS) return null;
        // The status goes back after the result is told, so the client sees checking, the check, then running.
        let checking = false;
        try {
          const pass = plan.pass(changedFiles(results));
          if (!pass) return null; // nothing this kind checks could have changed
          const covers = pass.covers ?? (() => true);
          checking = true;
          setStatus(true);
          const now = await check(pass.command);
          if (!now || signal.aborted) return null;
          // Each problem is told once, when it first appears; what this pass didn't re-check stands.
          const next = [...before.errors.filter((e) => !covers(e)), ...now.errors];
          baseline = Promise.resolve({ errors: next, ms: now.ms });
          const added = newProblems(before.errors.filter(covers), now.errors, kind.key);
          tell({ status: "done", ms: now.ms, before: before.errors.length, errors: next.length, added });
          return added.length ? kind.note(added) : null;
        } catch {
          return null; // a bug in reading the output must never end the turn: runAgent awaits this
        } finally {
          if (checking) setStatus(false);
        }
      },
    };
  }
}

/** The typecheck: the project's own tsgo or tsc. */
export const typecheck: CheckKind<TsError> = {
  prepare(root) {
    const bin = detectChecker(root);
    return bin ? { pass: () => ({ command: checkCommand(bin) }) } : null;
  },
  parse: parseTsc,
  missing: missingInterpreter,
  key: tsErrorKey,
  note: diagnosticsNote,
};

/** The typecheck's session state (one per session, so it survives clear() and resume()). */
export class Diagnostics extends Checker<TsError> {
  constructor(opts: { run?: RunCheck; timeoutMs?: number; sandboxWorks?: () => boolean } = {}) {
    super(typecheck, opts);
  }
}

/**
 * Several kinds as one: their baselines are taken together, their checks after a step run side by side, and their
 * notes go to the model as one message. The status is "checking" while any of them runs.
 */
export function combineChecks(parts: TurnChecks[]): TurnChecks {
  if (parts.length === 0) return NO_CHECKS;
  if (parts.length === 1) return parts[0]!;
  return {
    beforeChange: async () => {
      await Promise.all(parts.map((p) => p.beforeChange()));
    },
    afterStep: async (results) => {
      const notes = (await Promise.all(parts.map((p) => p.afterStep(results)))).filter((n): n is string => !!n);
      return notes.length ? notes.join("\n\n") : null;
    },
  };
}

/** What the transcript shows for a typecheck, or null for nothing. Error messages quote the project's code: printable. */
export function checkNotice(result: CheckResult): string | null {
  if (result.status === "off") {
    return result.reason === "no-sandbox"
      ? "✻ Marv typechecks after the model's edits only in the sandbox (it runs the project's own compiler), so it won't here."
      : "✻ The typecheck timed out twice, so Marv stopped running it for this session.";
  }
  if (result.status === "failed") {
    if (result.reason === "missing-interpreter") {
      return result.notify
        ? `✻ tsc needs ${result.tool}, which the sandbox can't see (Marv's sandbox hides the home folder except known toolchains). Marv can't tell the model about new type errors.`
        : null;
    }
    return result.reason === "unreadable" && result.notify ? "✻ Marv couldn't read the typecheck's output here, so it can't tell the model about new errors." : null;
  }
  if (result.status !== "done" || result.added.length === 0) return null;
  const n = result.added.length;
  const shown = result.added.slice(0, NOTICE_LINES).map((e) => printable(formatError(e)));
  return [`✻ Typecheck: ${n} new error${n === 1 ? "" : "s"}, told the model:`, ...shown, ...(n > shown.length ? [`… and ${n - shown.length} more`] : [])].join("\n");
}
