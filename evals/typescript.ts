// A TypeScript task needs the compiler in its repo: its check runs `tsc --noEmit`, and the agent (and later Marv's
// post-edit check) finds it at node_modules/.bin/tsc, as in a real project. The runs have no network, so it's copied
// from Marv's own node_modules. Not symlinked: the sandbox hides the home folder such a link would point into (and
// `dereference` copies a package that is itself a link, as with Bun's isolated linker, as files). The launcher is
// `#!/usr/bin/env node`, so the sandbox needs a node on PATH inside it: the system one (/usr/bin/node here) or one
// from a home toolchain it mounts (mise, asdf); another one would be reported by the startup notice, not silent.
import { cpSync, existsSync, mkdirSync, rmSync, symlinkSync } from "node:fs";
import { join } from "node:path";

const OWN = join(import.meta.dir, "..", "node_modules");

/** Copies TypeScript into `dir/node_modules` when `dir` has a tsconfig.json; says whether it did. */
export function addTypeScript(dir: string, from = OWN): boolean {
  if (!existsSync(join(dir, "tsconfig.json"))) return false;
  // Fail loudly: without the compiler every TypeScript run would look like the model's failure.
  if (!existsSync(join(from, "typescript"))) throw new Error(`No TypeScript in ${from}: run bun install`);
  const modules = join(dir, "node_modules");
  // `typescript` is the launcher; from TypeScript 7 the compiler itself is a platform package under @typescript.
  for (const pkg of ["typescript", "@typescript"]) {
    if (existsSync(join(from, pkg))) cpSync(join(from, pkg), join(modules, pkg), { recursive: true, dereference: true });
  }
  mkdirSync(join(modules, ".bin"), { recursive: true });
  const link = join(modules, ".bin", "tsc");
  // Replaced, not added: a second call, or a task repo shipping its own link, would otherwise fail with EEXIST.
  rmSync(link, { force: true });
  symlinkSync("../typescript/bin/tsc", link);
  return true;
}

/** Whether a bash call's arguments (the raw JSON text) run a typecheck: a guess from the command's text. */
export function ranTypecheck(args: string): boolean {
  let command: unknown;
  try {
    // Parsed, because in the raw JSON a newline before `tsc` is the two characters `\n`, which hides the word boundary.
    command = (JSON.parse(args) as { command?: unknown } | null)?.command;
  } catch {
    return false;
  }
  // `run typecheck`: the TS task repos have that script (npm run, bun run), and models use it.
  return typeof command === "string" && /\b(tsc|tsgo)\b|\brun typecheck\b/.test(command);
}
