// Runs the project's checks inside the sandbox (src/checks/kind.ts builds the command). `runChecks` is sent as source
// text to `bun -e`, since the sandbox can't see Marv's own files (they're under the hidden home folder): it must be
// self-contained, using only its argument and globals, with no imports and nothing from this module.
import type { ProjectCheck } from "./load.ts";

export interface RunnerSpec {
  checks: ProjectCheck[];
  /** Per script check. */
  timeoutMs: number;
}

/**
 * Prints one JSON line per violation (`{check, file?, line?, message, why?}`), `{check, error, detail?}` for a check that
 * failed, and `{done: true}` last, so output that stops early (a crash, the capture limit) can be told apart.
 */
export async function runChecks(spec: RunnerSpec): Promise<void> {
  const MAX_PER_CHECK = 500;
  const MAX_FILE_BYTES = 2_000_000;
  /** A script's stdout; past it the script is killed and the check failed. */
  const MAX_OUTPUT_BYTES = 1_000_000;
  const MAX_STDERR_CHARS = 16_384;
  const SKIP = /(^|\/)(node_modules|\.git)(\/|$)/;
  const print = (record: Record<string, unknown>) => console.log(JSON.stringify(record));
  const matches = (globs: string[], path: string) => globs.some((g) => new Bun.Glob(g).match(path));
  const files = (glob: string[], exclude: string[]): string[] => {
    const found = new Set<string>();
    for (const pattern of glob) {
      for (const path of new Bun.Glob(pattern).scanSync({ cwd: ".", dot: true, onlyFiles: true, followSymlinks: false })) {
        if (!SKIP.test(path) && !matches(exclude, path)) found.add(path);
      }
    }
    return [...found].sort();
  };

  for (const check of spec.checks) {
    try {
      let found = 0;
      const report = (v: { file?: string; line?: number; message: string; why?: string }) => {
        if (++found > MAX_PER_CHECK) return;
        print({ check: check.name, ...v });
      };
      if (check.kind === "pattern") {
        for (const file of files(check.glob, check.exclude)) {
          const handle = Bun.file(file);
          if (handle.size > MAX_FILE_BYTES) continue;
          const text = await handle.text();
          const why = check.why ? { why: check.why } : {};
          if (check.require !== undefined) {
            if (!new RegExp(check.require, check.flags).test(text)) report({ file, message: check.message, ...why });
            continue;
          }
          let line = 1;
          let at = 0;
          for (const match of text.matchAll(new RegExp(check.forbid!, `g${check.flags}`))) {
            for (; at < match.index!; at++) if (text.charCodeAt(at) === 10) line++;
            report({ file, line, message: check.message, ...why });
          }
        }
      } else {
        const proc = Bun.spawn([process.execPath, check.file], { stdin: "ignore", stdout: "pipe", stderr: "pipe" });
        let timedOut = false;
        const timer = setTimeout(() => {
          timedOut = true;
          proc.kill("SIGKILL");
        }, spec.timeoutMs);
        // Read as it arrives, never more than MAX_OUTPUT_BYTES: a script printing in a loop for its 30 s would
        // otherwise fill memory. Only the violations that will be printed are kept.
        let tooMuch = false;
        let notViolation: string | undefined;
        const violations: { file?: string; line?: number; message: string; why?: string }[] = [];
        const take = (raw: string) => {
          if (!raw.trim() || notViolation !== undefined) return;
          let v: Record<string, unknown> | null = null;
          try {
            v = JSON.parse(raw);
          } catch {}
          const ok =
            v &&
            typeof v === "object" &&
            typeof v.message === "string" &&
            (v.file === undefined || typeof v.file === "string") &&
            (v.line === undefined || typeof v.line === "number") &&
            (v.why === undefined || typeof v.why === "string");
          if (!ok) {
            notViolation = raw.slice(0, 200);
            return;
          }
          if (violations.length >= MAX_PER_CHECK) return;
          violations.push({
            message: v!.message as string,
            ...(v!.file !== undefined ? { file: v!.file as string } : {}),
            ...(v!.line !== undefined ? { line: v!.line as number } : {}),
            ...(v!.why !== undefined ? { why: v!.why as string } : {}),
          });
        };
        const readStdout = async () => {
          const decoder = new TextDecoder();
          let bytes = 0;
          let pending = "";
          for await (const chunk of proc.stdout) {
            bytes += chunk.byteLength;
            if (bytes > MAX_OUTPUT_BYTES) {
              tooMuch = true;
              proc.kill("SIGKILL");
              return;
            }
            const lines = (pending + decoder.decode(chunk, { stream: true })).split("\n");
            pending = lines.pop()!;
            for (const raw of lines) take(raw);
          }
          take(pending + decoder.decode());
        };
        // Only stderr's start is kept (where bun says what went wrong); the rest is read and dropped.
        const readStderr = async () => {
          const decoder = new TextDecoder();
          let text = "";
          for await (const chunk of proc.stderr) if (text.length < MAX_STDERR_CHARS) text += decoder.decode(chunk, { stream: true });
          return text.slice(0, MAX_STDERR_CHARS);
        };
        const [, stderr] = await Promise.all([readStdout(), readStderr()]);
        const code = await proc.exited;
        clearTimeout(timer);
        // The line that says what went wrong: the first that mentions an error (bun ends a crash with its version).
        const lastLine = (text: string) => {
          const lines = text.trim().split("\n").filter((l) => l.trim());
          return (lines.find((l) => /error/i.test(l)) ?? lines.at(-1) ?? "").slice(0, 200);
        };
        if (timedOut) throw new Error(`timed out after ${spec.timeoutMs / 1000} s`);
        if (tooMuch) throw new Error(`printed more than ${MAX_OUTPUT_BYTES / 1_000_000} MB`);
        // The message stays the same from run to run (it's part of what makes a problem "the same one"); what stderr
        // said, which can vary, goes in `detail`.
        if (code !== 0) throw Object.assign(new Error(`exited with code ${code}`), stderr.trim() ? { detail: lastLine(stderr) } : {});
        if (notViolation !== undefined) throw new Error(`printed a line that isn't a violation ({file, line, message, why}): ${notViolation}`);
        for (const v of violations) report(v);
      }
    } catch (error) {
      const detail = (error as { detail?: unknown })?.detail;
      print({ check: check.name, error: String((error as Error)?.message ?? error), ...(typeof detail === "string" ? { detail } : {}) });
    }
  }
  print({ done: true });
}
