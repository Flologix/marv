// Marv's own git calls (worktrees, the file listing behind glob and grep) run
// outside the sandbox, without approval, in folders an agent could have
// tampered with. So they never run a repository's programs (hooks, fsmonitor),
// never wait for input, and never wait forever (`spawnSync` blocks the whole
// UI, ctrl+c included, until git exits).

/** Hooks and fsmonitor are how a repository makes git run a program; Marv's own git calls turn both off. */
export const NO_PROGRAMS = ["-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false"];

/** The environment without inherited GIT_* variables (a parent git or hook could have set GIT_DIR, GIT_INDEX_FILE…), plus `pinned`. */
export function gitEnvironment(pinned: Record<string, string> = {}): Record<string, string> {
  const env: Record<string, string> = {};
  for (const [key, value] of Object.entries(process.env)) if (value !== undefined && !key.startsWith("GIT_")) env[key] = value;
  return { ...env, ...pinned };
}

export const GIT_TIMEOUT_MS = 60_000;

export interface GitResult {
  ok: boolean;
  /** stdout when it succeeded, stderr when it failed; not trimmed. */
  out: string;
  timedOut: boolean;
}

export function runGit(
  cwd: string,
  args: string[],
  { env, timeoutMs = GIT_TIMEOUT_MS }: { env?: Record<string, string>; timeoutMs?: number } = {},
): GitResult {
  const result = Bun.spawnSync(["git", ...NO_PROGRAMS, ...args], {
    cwd,
    env: gitEnvironment(env),
    stdin: "ignore", // nothing may wait for input: there's no one to type it
    stdout: "pipe",
    stderr: "pipe",
    timeout: timeoutMs,
  });
  if (result.exitedDueToTimeout) return { ok: false, out: "", timedOut: true };
  const ok = result.exitCode === 0;
  return { ok, out: (ok ? result.stdout : result.stderr).toString(), timedOut: false };
}

export interface GitAsyncResult extends GitResult {
  /** stdout as bytes (a blob from `cat-file`), whatever the exit code. */
  bytes: Uint8Array;
}

/**
 * runGit without blocking the event loop: a snapshot of a big tree (checkpoints) takes seconds, and the UI must keep
 * drawing meanwhile. The same rules (no hooks or fsmonitor, no inherited GIT_*, no stdin, a timeout), and an abort
 * kills it too. A git that's killed isn't waited for: a child it started (an alias, a filter) may hold its pipes open.
 */
export async function runGitAsync(
  cwd: string,
  args: string[],
  { env, timeoutMs = GIT_TIMEOUT_MS, signal }: { env?: Record<string, string>; timeoutMs?: number; signal?: AbortSignal } = {},
): Promise<GitAsyncResult> {
  const killed: GitAsyncResult = { ok: false, out: "", bytes: new Uint8Array(), timedOut: false };
  if (signal?.aborted) return killed;
  let proc: ReturnType<typeof Bun.spawn<"ignore", "pipe", "pipe">>;
  try {
    proc = Bun.spawn(["git", ...NO_PROGRAMS, ...args], { cwd, env: gitEnvironment(env), stdin: "ignore", stdout: "pipe", stderr: "pipe" });
  } catch (error) {
    return { ...killed, out: (error as Error).message };
  }
  let timer: ReturnType<typeof setTimeout> | undefined;
  let onAbort: (() => void) | undefined;
  const stopped = new Promise<"timeout" | "abort">((resolve) => {
    timer = setTimeout(() => resolve("timeout"), timeoutMs);
    onAbort = () => resolve("abort");
    signal?.addEventListener("abort", onAbort, { once: true });
  });
  const finished = Promise.all([new Response(proc.stdout).bytes(), new Response(proc.stderr).text(), proc.exited]);
  try {
    const outcome = await Promise.race([finished, stopped]);
    if (outcome === "timeout" || outcome === "abort") {
      proc.kill("SIGKILL");
      finished.catch(() => {}); // its pipes close whenever the last child lets go
      return { ...killed, timedOut: outcome === "timeout" };
    }
    const [stdout, stderr, code] = outcome;
    const ok = code === 0;
    return { ok, out: ok ? new TextDecoder().decode(stdout) : stderr, bytes: stdout, timedOut: false };
  } finally {
    clearTimeout(timer);
    if (onAbort) signal?.removeEventListener("abort", onAbort);
  }
}
