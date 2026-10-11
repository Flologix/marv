// The typecheck after a step's file changes (docs/superpowers/specs/2026-10-08-post-edit-diagnostics-design.md). A
// baseline is taken before the turn's first change; after each step that changed a file, the check runs again and
// the model is told only the errors that are new, anywhere in the project. The compiler is the project's own code,
// which a cloned repository could have planted, so it runs in the sandbox with the project read-only, and without
// the sandbox it doesn't run at all.
import { printable } from "../printable.ts";
import { sandboxAvailable } from "../sandbox.ts";
import { MAX_CAPTURE, runCommand, type CommandResult } from "../tools/bash.ts";
import type { ToolResult } from "../tools/types.ts";
import { checkCommand, detectChecker, diagnosticsNote, formatError, missingInterpreter, newErrors, parseTsc, type TsError } from "./tsc.ts";

/** Per check. TypeScript 7 checks Marv itself (24k lines) in half a second; TypeScript 5 is about ten times slower. */
export const CHECK_TIMEOUT_MS = 60_000;
/** Timeouts after which a session stops checking: a huge project would otherwise add a minute to every step. */
const MAX_TIMEOUTS = 2;
/** Errors the transcript shows (the model gets up to MAX_REPORTED). */
const NOTICE_LINES = 5;

export type CheckResult =
  /** It ran. `added`: the errors the model was told about (none: nothing was said). */
  | { status: "done"; ms: number; before: number; errors: number; added: TsError[] }
  /**
   * It couldn't tell what's new, so nothing was said. `notify`: the session's first such result, which the
   * transcript says once (otherwise this stays silent forever). `missing-interpreter`: the run died because the
   * compiler's shebang interpreter isn't in the sandbox (usually tsc's `node`, from a toolchain folder the sandbox
   * hides, e.g. mise's or asdf's); `tool` names it, so the notice can say that instead of "unreadable".
   */
  | { status: "failed"; ms: number; reason: "timeout" | "unreadable" | "missing-interpreter"; tool?: string; notify?: boolean }
  /** No checks from here on (said once per session). */
  | { status: "off"; reason: "no-sandbox" | "timeouts" };

/** Runs the check command; tests pass a stand-in. */
export type RunCheck = (command: string, opts: { root: string; signal: AbortSignal; timeoutMs: number }) => Promise<CommandResult>;

const sandboxed: RunCheck = (command, { root, signal, timeoutMs }) =>
  runCommand({ command, root, sandbox: true, network: false, projectReadOnly: true, timeoutMs, signal });

/** What the session hands the tools (beforeChange) and the loop (afterStep) for one turn. */
export interface TurnChecks {
  /** Takes the baseline before the turn's first change; later calls wait for the same one. Never rejects. */
  beforeChange(): Promise<void>;
  /** After a step: the note for the model when its file changes added errors, otherwise null. */
  afterStep(results: ToolResult[]): Promise<string | null>;
}

const NONE: TurnChecks = { beforeChange: async () => {}, afterStep: async () => null };

interface TurnOptions {
  root: string;
  /** The session's sandbox setting: checks need it on, and bwrap working. */
  sandbox: boolean;
  /**
   * Must be aborted when the turn ends, however it ends: a baseline started by a preview whose call was then
   * declined keeps running otherwise, and its orphaned timeout would count against the session.
   */
  signal: AbortSignal;
  /** True while a step's check runs ("Checking types…"), false after. */
  onStatus: (checking: boolean) => void;
  onResult: (result: CheckResult) => void;
}

/** One per session: what lasts from turn to turn. */
export class Diagnostics {
  private timeouts = 0;
  private toldNoSandbox = false;
  private toldUnreadable = false;

  /** Checking was turned off and on again: forget what made the session give up, and the notices already given. */
  reset(): void {
    this.timeouts = 0;
    this.toldNoSandbox = false;
    this.toldUnreadable = false;
  }

  constructor(private readonly opts: { run?: RunCheck; timeoutMs?: number; sandboxWorks?: () => boolean } = {}) {}

  turn({ root, sandbox, signal, onStatus, onResult }: TurnOptions): TurnChecks {
    const bin = detectChecker(root);
    if (!bin || this.timeouts >= MAX_TIMEOUTS) return NONE;
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
    // Callbacks are the UI's: whatever they do, a check never throws into the turn.
    const tell = (r: CheckResult) => {
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
    const unreadable = (ms: number): CheckResult => {
      const notify = !this.toldUnreadable;
      this.toldUnreadable = true;
      return { status: "failed", ms, reason: "unreadable", ...(notify ? { notify } : {}) };
    };
    const notifyOnce = (): { notify?: boolean } => {
      const notify = !this.toldUnreadable;
      this.toldUnreadable = true;
      return notify ? { notify } : {};
    };
    const check = async (): Promise<{ errors: TsError[]; ms: number } | null> => {
      const started = Date.now();
      let result: CommandResult;
      try {
        result = await run(checkCommand(bin), { root, signal, timeoutMs: this.opts.timeoutMs ?? CHECK_TIMEOUT_MS });
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
      const missing = missingInterpreter(result.output, result.exitCode);
      // Output at the capture limit was cut off, maybe mid-list: what's left would make errors look fixed or new.
      const errors = !missing && result.output.length < MAX_CAPTURE ? parseTsc(result.output, result.exitCode) : null;
      if (!errors) {
        unavailable = true;
        tell(missing ? { status: "failed", ms, reason: "missing-interpreter", tool: missing, ...notifyOnce() } : unreadable(ms));
        return null;
      }
      return { errors, ms };
    };
    let baseline: Promise<{ errors: TsError[]; ms: number } | null> | undefined;
    return {
      beforeChange: async () => {
        await (baseline ??= check());
      },
      afterStep: async (results) => {
        if (!baseline || !results.some((r) => r.diff)) return null;
        const before = await baseline;
        if (!before || unavailable || signal.aborted || this.timeouts >= MAX_TIMEOUTS) return null;
        // The status goes back after the result is told, so the client sees checking, the check, then running.
        try {
          setStatus(true);
          const now = await check();
          if (!now || signal.aborted) return null;
          baseline = Promise.resolve(now); // each error is told once, when it first appears
          const added = newErrors(before.errors, now.errors);
          tell({ status: "done", ms: now.ms, before: before.errors.length, errors: now.errors.length, added });
          return added.length ? diagnosticsNote(added) : null;
        } catch {
          return null; // a bug in reading the output must never end the turn: runAgent awaits this
        } finally {
          setStatus(false);
        }
      },
    };
  }
}

/** What the transcript shows for a check, or null for nothing. Error messages quote the project's code: printable. */
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
