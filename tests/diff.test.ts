import { expect, test } from "bun:test";
import { addedLines, diffText, MAX_SHOWN_DIFF, MAX_SHOWN_LINE_CHARS, previewLines, shownDiff } from "../src/tools/diff.ts";

test("diff lines carry line numbers: new for added and unchanged lines, old for removed ones, across hunks", () => {
  const before = Array.from({ length: 20 }, (_, i) => `line ${i + 1}`).join("\n") + "\n";
  const after = before.replace("line 2\n", "line two\n").replace("line 18\n", "line 18\nline 18b\n");
  const { lines } = diffText(before, after, 1);
  expect(lines).toEqual([
    { kind: "ctx", text: "line 1", oldLine: 1, newLine: 1 },
    { kind: "del", text: "line 2", oldLine: 2 },
    { kind: "add", text: "line two", newLine: 2 },
    { kind: "ctx", text: "line 3", oldLine: 3, newLine: 3 },
    { kind: "gap", text: "…" },
    { kind: "ctx", text: "line 18", oldLine: 18, newLine: 18 },
    { kind: "add", text: "line 18b", newLine: 19 },
    { kind: "ctx", text: "line 19", oldLine: 19, newLine: 20 },
  ]);
});

test("a new file is all additions, numbered from 1", () => {
  expect(addedLines("a\nb\n")).toEqual([
    { kind: "add", text: "a", newLine: 1 },
    { kind: "add", text: "b", newLine: 2 },
  ]);
  expect(addedLines("")).toEqual([]);
});

test("what a result keeps is capped, and says how much it left out", () => {
  const many = addedLines(Array.from({ length: MAX_SHOWN_DIFF + 7 }, (_, i) => `l${i}`).join("\n"));
  expect(shownDiff(many)).toEqual({ lines: many.slice(0, MAX_SHOWN_DIFF), more: 7 });
  expect(shownDiff(many.slice(0, 3))).toEqual({ lines: many.slice(0, 3), more: 0 });
});

test("what a result keeps cuts each line short: a minified file's one huge line isn't stored whole", () => {
  const long = "x".repeat(MAX_SHOWN_LINE_CHARS * 10);
  const { lines } = shownDiff([{ kind: "add", text: long, newLine: 1 }, { kind: "ctx", text: "short", oldLine: 1, newLine: 2 }]);
  expect(lines[0]).toEqual({ kind: "add", text: long.slice(0, MAX_SHOWN_LINE_CHARS), newLine: 1 });
  expect(lines[1]).toEqual({ kind: "ctx", text: "short", oldLine: 1, newLine: 2 });
});

test("a collapsed preview keeps a change's additions in view: a long removal is cut to its first lines and a gap", () => {
  const { lines } = diffText("a\nb\nc\nd\ne\nf\n", "x\ny\n");
  expect(previewLines(lines, 5).map((l) => (l.kind === "gap" ? "…" : `${l.kind} ${l.text}`))).toEqual(["del a", "…", "add x", "add y"]);
  // What fits is shown as it is, and nothing is ever longer than asked.
  expect(previewLines(lines, 8)).toEqual(lines);
  expect(previewLines(lines, 2)).toHaveLength(2);
});

test("a big rewrite keeps the new file's start too, and counts what it left out", () => {
  const file = (prefix: string) => Array.from({ length: 500 }, (_, i) => `${prefix} ${i + 1}`).join("\n") + "\n";
  const all = diffText(file("old"), file("new")).lines;
  const { lines, more } = shownDiff(all);
  expect(lines).toHaveLength(MAX_SHOWN_DIFF);
  expect(lines.filter((l) => l.kind === "del")).toEqual(all.slice(0, MAX_SHOWN_DIFF / 5));
  expect(lines[MAX_SHOWN_DIFF / 5]).toEqual({ kind: "gap", text: "…" });
  expect(lines[MAX_SHOWN_DIFF / 5 + 1]).toEqual({ kind: "add", text: "new 1", newLine: 1 });
  expect(more).toBe(1000 - (MAX_SHOWN_DIFF - 1));
  // Collapsed again on screen, the kept diff still shows the new file's start, with one gap.
  const preview = previewLines(lines, 15);
  expect(preview.map((l) => l.kind)).toEqual(["del", "del", "del", "gap", ...Array(11).fill("add")]);
  expect(preview[4]).toEqual({ kind: "add", text: "new 1", newLine: 1 });
});
