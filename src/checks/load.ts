// The project's own checks: rules from its instructions, as code that runs after the model's edits (issue #41). One
// file per rule in .marv/checks/, read here; they run in the sandbox (./runner.ts) through the same machinery as the
// typecheck (src/diagnostics/turn.ts). Reading them runs nothing: a script's settings are its leading comments.
import { existsSync, readdirSync, readFileSync, statSync } from "node:fs";
import { extname, join } from "node:path";

export const CHECKS_DIR = join(".marv", "checks");

/** Declarative: a glob, and a pattern that must not (forbid) or must (require) appear in each matching file. */
export interface PatternCheck {
  kind: "pattern";
  name: string;
  /** Project-relative, e.g. `.marv/checks/imports.yaml`. */
  file: string;
  glob: string[];
  exclude: string[];
  /** A JavaScript regular expression: each match is a violation, at its line. */
  forbid?: string;
  /** A JavaScript regular expression: a matching file without one is a violation. */
  require?: string;
  /** Regular expression flags besides `g` (any of `imsu`). */
  flags: string;
  message: string;
  why?: string;
}

/** A `.ts` file run with bun from the project root; it prints violations as JSON lines `{file, line, message, why}`. */
export interface ScriptCheck {
  kind: "script";
  name: string;
  file: string;
  glob: string[];
  exclude: string[];
}

export type ProjectCheck = PatternCheck | ScriptCheck;

/** What a check reported: a rule broken somewhere, or the check itself failing (`failed`). */
export interface Violation {
  check: string;
  file?: string;
  line?: number;
  message: string;
  why?: string;
  failed?: boolean;
}

const DECLARATIVE = new Set([".yaml", ".yml", ".json"]);
/** Larger declarative files are refused (a rule is a few lines). */
const MAX_RULE_BYTES = 64 * 1024;

const strings = (value: unknown): string[] | null =>
  typeof value === "string" ? [value] : Array.isArray(value) && value.every((v) => typeof v === "string") ? value : null;

/** A declarative check's settings, or why they're wrong. */
export function parsePatternCheck(name: string, file: string, text: string): PatternCheck | string {
  let data: unknown;
  try {
    data = Bun.YAML.parse(text);
  } catch (error) {
    return `isn't valid YAML or JSON (${(error as Error).message})`;
  }
  if (!data || typeof data !== "object" || Array.isArray(data)) return "should be an object with glob, forbid or require, message and why";
  const d = data as Record<string, unknown>;
  const glob = strings(d.glob);
  if (!glob?.length) return "needs a glob (a string or a list of them)";
  const exclude = d.exclude === undefined ? [] : strings(d.exclude);
  if (!exclude) return "has an exclude that isn't a string or a list of them";
  if ((d.forbid === undefined) === (d.require === undefined)) return "needs exactly one of forbid and require";
  const pattern = d.forbid ?? d.require;
  if (typeof pattern !== "string" || !pattern) return "has a forbid or require that isn't a regular expression";
  const flags = d.flags ?? "";
  if (typeof flags !== "string" || !/^[imsu]*$/.test(flags)) return "has flags other than i, m, s and u";
  try {
    new RegExp(pattern, `g${flags}`); // compiled, never run, here: matching happens in the sandbox
  } catch (error) {
    return `has a regular expression that doesn't compile (${(error as Error).message})`;
  }
  if (typeof d.message !== "string" || !d.message.trim()) return "needs a message";
  if (d.why !== undefined && typeof d.why !== "string") return "has a why that isn't a string";
  return {
    kind: "pattern",
    name,
    file,
    glob,
    exclude,
    ...(d.forbid !== undefined ? { forbid: pattern } : { require: pattern }),
    flags,
    message: d.message.trim(),
    ...(typeof d.why === "string" && d.why.trim() ? { why: d.why.trim() } : {}),
  };
}

/** Glob patterns separated by commas or spaces; a comma inside braces (`*.{ts,tsx}`) belongs to its pattern. */
function splitPatterns(text: string): string[] {
  const patterns: string[] = [];
  let current = "";
  let depth = 0;
  for (const char of text) {
    if (char === "{") depth++;
    else if (char === "}" && depth > 0) depth--;
    else if (depth === 0 && (char === "," || /\s/.test(char))) {
      if (current) patterns.push(current);
      current = "";
      continue;
    }
    current += char;
  }
  if (current) patterns.push(current);
  return patterns;
}

/**
 * A script check's settings, from its leading `//` comments: `// glob: src/**\/*.ts, tests/**\/*.ts` (one or more
 * lines; without one it runs after any change) and `// exclude: …`.
 */
export function parseScriptCheck(name: string, file: string, text: string): ScriptCheck {
  const glob: string[] = [];
  const exclude: string[] = [];
  for (const line of text.split(/\r?\n/)) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith("#!")) continue;
    if (!trimmed.startsWith("//")) break;
    const setting = /^\/\/\s*(glob|exclude)\s*:\s*(.*)$/.exec(trimmed);
    if (!setting) continue;
    const values = splitPatterns(setting[2]!);
    (setting[1] === "glob" ? glob : exclude).push(...values);
  }
  return { kind: "script", name, file, glob: glob.length ? glob : ["**"], exclude };
}

/** The project's checks, sorted by name, and the files that couldn't be used (with why). Nothing when there's no folder. */
export function loadChecks(root: string): { checks: ProjectCheck[]; problems: string[] } {
  const dir = join(root, CHECKS_DIR);
  if (!existsSync(dir)) return { checks: [], problems: [] };
  let entries: string[];
  try {
    entries = readdirSync(dir).sort();
  } catch (error) {
    return { checks: [], problems: [`${CHECKS_DIR} couldn't be read: ${(error as Error).message}`] };
  }
  const checks: ProjectCheck[] = [];
  const problems: string[] = [];
  for (const entry of entries) {
    const ext = extname(entry);
    if (!DECLARATIVE.has(ext) && ext !== ".ts") continue;
    const name = entry.slice(0, -ext.length);
    const file = `${CHECKS_DIR.split("\\").join("/")}/${entry}`;
    if (checks.some((c) => c.name === name)) {
      problems.push(`${file}: another check is already named "${name}"`);
      continue;
    }
    let text: string;
    try {
      const stat = statSync(join(dir, entry));
      if (!stat.isFile()) continue;
      if (DECLARATIVE.has(ext) && stat.size > MAX_RULE_BYTES) {
        problems.push(`${file}: larger than ${MAX_RULE_BYTES / 1024} KB`);
        continue;
      }
      text = readFileSync(join(dir, entry), "utf8");
    } catch (error) {
      problems.push(`${file}: couldn't be read (${(error as Error).message})`);
      continue;
    }
    if (ext === ".ts") {
      checks.push(parseScriptCheck(name, file, text));
      continue;
    }
    const check = parsePatternCheck(name, file, text);
    if (typeof check === "string") problems.push(`${file}: ${check}`);
    else checks.push(check);
  }
  return { checks, problems };
}

/** Whether a change to `path` (project-relative) concerns the check: its glob matches, or it's the check's own file. */
export function concerns(check: ProjectCheck, path: string): boolean {
  if (path === check.file) return true;
  return check.glob.some((g) => new Bun.Glob(g).match(path)) && !check.exclude.some((g) => new Bun.Glob(g).match(path));
}
