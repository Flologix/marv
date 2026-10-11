// Repairs for malformed tool calls, issue #10: fixes so obvious that doing
// them beats bouncing the call back to the model, which then burns a step
// re-sending what it meant. Only unambiguous cases are repaired; anything
// else stays an error the model can recover from. Every repair says what it
// did (a "Marv:" note on the result), so the model learns the right form.
//
// Two kinds, applied in runTool (src/tools/index.ts):
//   - repairJsonArgs: the arguments as a whole (raw control characters
//     inside JSON strings, which JSON.parse rejects out of hand), and
//   - a tool's repair(input, ctx): a field whose meaning the tool knows
//     (bash's timeout given in milliseconds, a project path written with a
//     leading slash), applied before the input is validated.
import { existsSync } from "node:fs";
import { resolve, sep } from "node:path";
import type { ToolContext } from "./types.ts";

/** A repaired call's explanation, added to the result the model reads. */
export type RepairNote = string | null;

export interface JsonRepair {
  /** The parsed arguments, as JSON.parse would have given them. */
  value: unknown;
  note: RepairNote;
}

/**
 * What a raw control character (U+0000–U+001F) inside a JSON string looks
 * like to JSON.parse: an invalid escape. JSON allows exactly ", \, /, b, f,
 * n, r, t and uXXXX after a backslash; these characters are never valid
 * there, so finding one inside a string is unambiguous. Outside strings they
 * are legal whitespace (a pretty-printed call has newlines and tabs between
 * the tokens), so they don't count.
 */
const CONTROL = /[\u0000-\u001f]/;

/** The escapes JSON.stringify writes for them; \b and \f keep their short forms. */
const ESCAPES: Record<string, string> = {
  "\b": "\\b",
  "\t": "\\t",
  "\n": "\\n",
  "\f": "\\f",
  "\r": "\\r",
};

/** The label for a control character in the note, for the model to avoid it. */
function describeCode(code: number): string {
  const named: Record<number, string> = { 9: "tab", 10: "newline", 13: "carriage return" };
  return named[code] ?? `U+${code.toString(16).toUpperCase().padStart(2, "0")}`;
}

/**
 * Escapes raw control characters inside JSON string literals, so a call that
 * put a literal tab or newline in a string parses. Nothing else is touched:
 * broken JSON of any other kind comes back (null), and the normal "not valid
 * JSON" error follows.
 */
export function repairJsonArgs(args: string): JsonRepair | null {
  try {
    JSON.parse(args);
    return null; // The arguments are valid JSON: nothing to repair.
  } catch {
    // Try the repair only when a raw control character sits inside a string;
    // anything else (truncated output, a missing brace) is not ours to guess.
    let inString = false;
    let escaped = false;
    const found: number[] = [];
    for (let i = 0; i < args.length; i++) {
      const code = args.charCodeAt(i);
      if (inString) {
        if (escaped) escaped = false;
        else if (code === 0x5c) escaped = true; // backslash
        else if (code === 0x22) inString = false; // quote
        else if (code < 0x20) found.push(code);
        continue;
      }
      if (code === 0x22) inString = true;
      // Outside strings every character is fine as it is: control characters
      // there are whitespace, and JSON.parse says what else is wrong.
    }
    if (found.length === 0) return null;

    let out = "";
    inString = false;
    escaped = false;
    for (const ch of args) {
      if (inString) {
        if (escaped) {
          escaped = false;
          out += ch;
          continue;
        }
        if (ch === "\\") {
          escaped = true;
          out += ch;
          continue;
        }
        if (ch === '"') {
          inString = false;
          out += ch;
          continue;
        }
        // The short escapes where JSON has one; any other control character (a vertical tab, NUL…) as \u00XX.
        out += ESCAPES[ch] ?? (ch.charCodeAt(0) < 0x20 ? `\\u${ch.charCodeAt(0).toString(16).padStart(4, "0")}` : ch);
        continue;
      }
      if (ch === '"') inString = true;
      out += ch;
    }
    try {
      const names = [...new Set(found.map(describeCode))].join(", ");
      return {
        value: JSON.parse(out),
        note:
          `Marv: the arguments contained ${found.length} raw control character${found.length === 1 ? "" : "s"} inside JSON strings (${names}), ` +
          `which JSON doesn't allow; they were escaped and the call ran. Quote them as \\t, \\n or \\r next time.`,
      };
    } catch {
      return null; // Escaping didn't make it valid: leave the honest error.
    }
  }
}

/**
 * A path a tool was given, as the project means it: a path written with a
 * leading slash that doesn't exist there but does beside the project's files
 * (`/src/pagination.js` while the file is `src/pagination.js`) is read as
 * that file. Absolute paths that exist (a real file, or /etc/hosts) and
 * anything that doesn't are left alone — that's an error the model can act
 * on, not a guess.
 */
export function repairProjectPath(root: string, path: string): { path: string; note: RepairNote } {
  // A leading "/" is the repair's shape; an empty name, "//", "//x" (POSIX's
  // implementation-defined root), a climb out of the project and NUL are not
  // readable as a project path.
  const relativePath = path.slice(1);
  if (
    !path.startsWith("/") ||
    !relativePath ||
    relativePath.startsWith("/") ||
    relativePath.split("/").includes("..") ||
    relativePath.includes("\0")
  ) {
    return { path, note: null };
  }
  if (existsSync(resolve(path))) return { path, note: null }; // It exists where it was written: meant as written.
  const candidate = resolve(root, relativePath);
  if (!existsSync(candidate)) return { path, note: null };
  const shown = relativePath.split(sep).join("/");
  return {
    path: shown,
    note: `Marv: the path was written with a leading slash (${path}): used ${shown}, which is where the file is in this project.`,
  };
}

/**
 * A timeout far above the tool's maximum, when it is exactly a whole number
 * of milliseconds (and at least one second's worth), was meant in
 * milliseconds — the maximum is 600 seconds, and no model means a fifth of a
 * second of it. Divided and clamped to the maximum; anything else (750, 0,
 * 60) could still be meant in seconds, so it fails with the ordinary error.
 */
export function repairTimeoutSeconds(timeout: number, maxSeconds: number): { seconds: number; note: RepairNote } {
  if (Number.isInteger(timeout) && timeout >= 1000 && timeout % 1000 === 0 && timeout > maxSeconds) {
    const seconds = Math.min(timeout / 1000, maxSeconds);
    return {
      seconds,
      note: `Marv: the timeout was given in milliseconds (${timeout}): used ${seconds} s. Give timeout in seconds.`,
    };
  }
  return { seconds: timeout, note: null };
}

/** Appends a repair's note to the tool's output (both are read by the model). */
export function withNote(output: string, note: RepairNote): string {
  return note ? `${output}\n\n${note}` : output;
}

/** What `repair(input, ctx)` returns: the input to use instead, and what to say about it. */
export interface Repaired<S> {
  input: S;
  note: RepairNote;
}

/**
 * The `repair(raw, ctx)` of the tools that take a project `path` (read_file,
 * glob, grep, edit_file, write_file): a path written with a leading slash that
 * only exists relative to the project (see repairProjectPath) is read as that
 * file. No object, or no string path (glob and grep's path is optional), is
 * nothing to repair.
 */
export function repairPathField(raw: unknown, root: string): Repaired<unknown> | null {
  if (typeof raw !== "object" || raw === null) return null;
  const { path } = raw as { path?: unknown };
  if (typeof path !== "string") return null;
  const fixed = repairProjectPath(root, path);
  if (!fixed.note) return null;
  return { input: { ...raw, path: fixed.path }, note: fixed.note };
}

/** The context a repair may use: just the project root (kept narrow on purpose). */
export type RepairContext = Pick<ToolContext, "root">;
