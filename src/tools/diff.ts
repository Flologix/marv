// A change as diff lines, for the approval prompt and the transcript.
import { structuredPatch } from "diff";
import type { DiffLine, DiffShown } from "./types.ts";

export interface Diff {
  lines: DiffLine[];
  added: number;
  removed: number;
}

/** Changed lines with a little context; a "gap" marks skipped unchanged lines between hunks. */
export function diffText(before: string, after: string, context = 3): Diff {
  const { hunks } = structuredPatch("a", "b", before, after, "", "", { context });
  const lines: DiffLine[] = [];
  let added = 0;
  let removed = 0;
  hunks.forEach((hunk, i) => {
    if (i > 0) lines.push({ kind: "gap", text: "…" });
    // Each hunk says where it starts in the old and the new file; count from there.
    let oldLine = hunk.oldStart;
    let newLine = hunk.newStart;
    for (const line of hunk.lines) {
      if (line.startsWith("\\")) continue; // "\ No newline at end of file"
      const text = line.slice(1);
      if (line[0] === "+") {
        added++;
        lines.push({ kind: "add", text, newLine: newLine++ });
      } else if (line[0] === "-") {
        removed++;
        lines.push({ kind: "del", text, oldLine: oldLine++ });
      } else lines.push({ kind: "ctx", text, oldLine: oldLine++, newLine: newLine++ });
    }
  });
  return { lines, added, removed };
}

/** "+3 −1" (a real minus sign, which lines up with the plus). */
export const changeSummary = ({ added, removed }: Diff) => `+${added} −${removed}`;

/** A new file as a diff: every line added, numbered from 1. */
export function addedLines(content: string): DiffLine[] {
  const text = content.replace(/\n$/, "");
  return text === "" ? [] : text.split("\n").map((line, i) => ({ kind: "add" as const, text: line, newLine: i + 1 }));
}

/**
 * At most `max` lines of a diff, for a collapsed view. In order, except that a change (removed lines, then added ones)
 * that doesn't fit keeps only its first few removals (a fifth of `max`) and a "gap" before its additions: a full rewrite
 * lists every old line first, and would otherwise show no line of the new file.
 */
export function previewLines(lines: DiffLine[], max: number): DiffLine[] {
  const keep = Math.max(1, Math.floor(max / 5));
  const out: DiffLine[] = [];
  let i = 0;
  while (i < lines.length && out.length < max) {
    if (lines[i]!.kind !== "del") {
      out.push(lines[i++]!);
      continue;
    }
    let end = i;
    while (end < lines.length && lines[end]!.kind === "del") end++;
    // A gap right after removals is one of ours (a hunk ends in context), from a kept diff that was already cut.
    const adds = lines[end]?.kind === "gap" ? end + 1 : end;
    let next = adds;
    while (next < lines.length && lines[next]!.kind === "add") next++;
    if (end - i > keep && next > adds && next - i > max - out.length) {
      out.push(...lines.slice(i, i + keep), { kind: "gap", text: "…" });
      i = adds;
    } else {
      out.push(...lines.slice(i, Math.min(end, i + max - out.length)));
      i = end;
    }
  }
  return out.slice(0, max);
}

/** Lines of the file, not counting "…" gap markers. */
export const fileLines = (lines: DiffLine[]) => lines.filter((l) => l.kind !== "gap").length;

/** The most lines of one change a tool result keeps for the transcript (saved sessions hold them too). */
export const MAX_SHOWN_DIFF = 400;

/**
 * The longest line it keeps: the screen truncates at the terminal's width anyway, and an edit to minified code would
 * otherwise store a huge line (twice: removed and added) in every save of the session.
 */
export const MAX_SHOWN_LINE_CHARS = 500;

/** What a result keeps: `previewLines`, so a big rewrite keeps the new file's start; `more` counts the file lines left out. */
export function shownDiff(lines: DiffLine[]): DiffShown {
  const kept = previewLines(lines, MAX_SHOWN_DIFF);
  return {
    lines: kept.map((l) => (l.text.length > MAX_SHOWN_LINE_CHARS ? { ...l, text: l.text.slice(0, MAX_SHOWN_LINE_CHARS) } : l)),
    more: fileLines(lines) - fileLines(kept),
  };
}
