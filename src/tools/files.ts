// Shared file helpers for the tools: confining paths to the project, and
// listing the project's files.
import { existsSync, lstatSync, realpathSync, statSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { runGit } from "../git.ts";
import { ToolError } from "./types.ts";

/** Skipped when the project isn't a git repo (inside one, .gitignore decides). */
const IGNORED_DIRS = new Set([".git", "node_modules", "dist", "build", "out", "coverage", ".next", ".cache", "target", ".venv", "__pycache__"]);

const isInside = (root: string, path: string) => {
  const rel = relative(root, path);
  return rel === "" || (!rel.startsWith(`..${sep}`) && rel !== ".." && !isAbsolute(rel));
};

/**
 * Resolves a path the model gave us (relative to the project, absolute, or
 * starting with `~/`) and refuses anything outside the project, including via
 * symlinks. `~` is the home folder, as in a shell: taken literally, `~/.config/x`
 * became `<project>/~/.config/x`, and a model told to create it would make a
 * stray `~` folder.
 */
export function resolveInProject(root: string, path: string, home = homedir()): string {
  const expanded = path === "~" || path.startsWith("~/") ? join(home, path.slice(1)) : path;
  const absolute = isAbsolute(expanded) ? resolve(expanded) : resolve(root, expanded);
  const outside = new ToolError(
    `"${path}" is outside the project (${root}). Use a path relative to the project root, like "src/app.ts".`,
  );
  if (!isInside(root, absolute)) throw outside;
  // A symlink inside the project can still point outside it. For a path that
  // doesn't exist yet (a new file), check the nearest folder that does: a new
  // file in a symlinked folder would land wherever that folder points.
  let existing = absolute;
  while (!existsSync(existing) && existing !== dirname(existing)) existing = dirname(existing);
  if (!isInside(realpathSync(root), realpathSync(existing))) throw outside;
  return absolute;
}

/**
 * Whether a project path is (or is inside) the project's .git, also through a
 * symlink: git runs its hooks and config outside any sandbox, so yolo mode
 * never changes it without asking. Like resolveInProject, a path that doesn't
 * exist yet is judged by the nearest folder that does.
 */
function insideGitDir(root: string, absolute: string): boolean {
  let existing = absolute;
  while (!existsSync(existing) && existing !== dirname(existing)) existing = dirname(existing);
  const real = join(realpathSync(existing), relative(existing, absolute));
  return relative(realpathSync(root), real).split(sep)[0] === ".git";
}

/** For `autoSafe`: whether a path the model gave leads into .git (a path outside the project counts too: it asks, and fails). */
export function touchesGit(root: string, path: string): boolean {
  try {
    return insideGitDir(root, resolveInProject(root, path));
  } catch {
    return true;
  }
}

/**
 * Checked again when a change that runs without asking is applied: a command
 * running meanwhile (a parallel subagent) could have pointed a symlink into .git.
 */
export function refuseGit(root: string, absolute: string): void {
  if (insideGitDir(root, absolute)) {
    throw new ToolError(`"${projectPath(root, absolute)}" leads into .git, which changes only with the user's approval. Try again; they'll be asked.`);
  }
}

/** "src/app.ts" for display and for the model; "." for the root itself. */
export const projectPath = (root: string, absolute: string) => relative(root, absolute).split(sep).join("/") || ".";

/** Long enough for a big repository, short enough that a blocked git (e.g. a FIFO named .gitignore) gives up. */
export const LIST_TIMEOUT_MS = 20_000;

/**
 * Every file in the project, as project-relative paths, sorted. Inside a git
 * repo this is `git ls-files` (tracked + untracked, minus .gitignore'd), so
 * build output and dependencies stay out of the model's way. It runs outside
 * the sandbox without approval, so it never runs the repository's programs
 * (hooks, fsmonitor), and `gitEnv` pins where git looks.
 */
export async function listProjectFiles(
  root: string,
  gitEnv?: Record<string, string>,
  timeoutMs = LIST_TIMEOUT_MS,
): Promise<string[]> {
  const git = runGit(root, ["ls-files", "-z", "--cached", "--others", "--exclude-standard"], { env: gitEnv, timeoutMs });
  // No fallback to the walk: it would return node_modules and other ignored folders unfiltered.
  if (git.timedOut) {
    throw new ToolError("Listing the project's files timed out (a special file such as a FIFO may be blocking git).");
  }
  let paths: string[];
  if (git.ok) {
    paths = git.out
      .split("\0")
      .filter((p) => p && existsSync(join(root, p))); // --cached includes deleted-but-staged files
  } else {
    paths = [];
    for await (const path of new Bun.Glob("**/*").scan({ cwd: root, dot: true, onlyFiles: true })) {
      if (path.split(sep).some((part) => IGNORED_DIRS.has(part)) || !isRegularFile(join(root, path))) continue;
      paths.push(path.split(sep).join("/"));
    }
  }
  return paths.sort();
}

/** Files under `dir` (a project-relative directory, "." for everything). */
export async function filesUnder(root: string, dir: string, gitEnv?: Record<string, string>): Promise<string[]> {
  const all = await listProjectFiles(root, gitEnv);
  if (dir === ".") return all;
  return all.filter((p) => p.startsWith(`${dir}/`) || p === dir);
}

/** Reading a FIFO or a device would block forever (and Esc can't cancel it), so tools read regular files only. */
export function isRegularFile(path: string): boolean {
  try {
    return lstatSync(path).isFile() || statSync(path).isFile();
  } catch {
    return false;
  }
}

export function requireRegularFile(path: string, shown: string): void {
  if (!existsSync(path)) throw new ToolError(`"${shown}" doesn't exist.`);
  if (!isRegularFile(path)) throw new ToolError(`"${shown}" is not a regular file (a pipe, socket or device), so it can't be read.`);
}

export function isDirectory(path: string): boolean {
  return existsSync(path) && statSync(path).isDirectory();
}

/** Crude but standard: a NUL byte in the first 8 KB means binary. */
export const looksBinary = (bytes: Uint8Array) => bytes.subarray(0, 8192).includes(0);
