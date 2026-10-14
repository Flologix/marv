// A stand-in for the GitHub CLI (#47): tests put it on PATH as `gh` (installFakeGh), so the github tool runs it
// exactly as it would run gh, and nothing reaches GitHub. It logs every argv (and a few environment variables) as a
// JSON line to $FAKE_GH_LOG, answers `auth status` per $FAKE_GH_LOGGED_IN ("1": logged in; $FAKE_GH_HANG: never
// answers), `repo view --json nameWithOwner` with o/r ($FAKE_GH_NO_REPO=1: fails), and anything else with $FAKE_GH_OUTPUT (default: its argv),
// $FAKE_GH_STDERR on stderr, and exit code $FAKE_GH_EXIT.
import { appendFileSync } from "node:fs";
import { chmod, mkdtemp, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

/** The environment variables the log keeps: what tests check gh got (or didn't). */
const LOGGED_ENV = ["GH_HOST", "GH_ENTERPRISE_TOKEN", "OPENROUTER_API_KEY", "MARV_SECRET", "GH_PROMPT_DISABLED", "GH_PAGER", "NO_COLOR", "GH_DEBUG", "GIT_DIR", "GIT_CONFIG_COUNT", "GIT_CONFIG_KEY_0", "GIT_CONFIG_VALUE_0", "GIT_CONFIG_KEY_1", "GIT_CONFIG_VALUE_1"];

export interface FakeGhCall {
  args: string[];
  cwd: string;
  env: Record<string, string | null>;
}

/** A folder with an executable `gh` that runs this file, to put first on PATH. */
export async function installFakeGh(): Promise<string> {
  const bin = await mkdtemp(join(tmpdir(), "marv-fake-gh-"));
  const gh = join(bin, "gh");
  await writeFile(gh, `#!/bin/sh\nexec '${process.execPath}' '${import.meta.path}' "$@"\n`);
  await chmod(gh, 0o755);
  return bin;
}

/** The calls the fake logged, in order. */
export async function fakeGhCalls(log: string): Promise<FakeGhCall[]> {
  const file = Bun.file(log);
  if (!(await file.exists())) return [];
  return (await file.text())
    .split("\n")
    .filter(Boolean)
    .map((line) => JSON.parse(line) as FakeGhCall);
}

if (import.meta.main) {
  const args = process.argv.slice(2);
  const log = process.env.FAKE_GH_LOG;
  const env = Object.fromEntries(LOGGED_ENV.map((name) => [name, process.env[name] ?? null]));
  if (log) appendFileSync(log, `${JSON.stringify({ args, cwd: process.cwd(), env })}\n`);
  if (args[0] === "auth" && args[1] === "status") {
    if (process.env.FAKE_GH_HANG) await Bun.sleep(30_000);
    process.exit(process.env.FAKE_GH_LOGGED_IN === "1" ? 0 : 1);
  }
  if (args[0] === "repo" && args[1] === "view" && args.includes("nameWithOwner")) {
    if (process.env.FAKE_GH_NO_REPO === "1") process.exit(1);
    console.log("o/r");
    process.exit(0);
  }
  process.stdout.write(`${process.env.FAKE_GH_OUTPUT ?? `fake gh ${args.join(" ")}`}\n`);
  if (process.env.FAKE_GH_STDERR) process.stderr.write(`${process.env.FAKE_GH_STDERR}\n`);
  process.exit(Number(process.env.FAKE_GH_EXIT ?? 0));
}
