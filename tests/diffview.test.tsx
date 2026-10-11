import { expect, test } from "bun:test";
import { render } from "ink-testing-library";
import { diffText } from "../src/tools/diff.ts";
import { DiffView } from "../src/ui/DiffView.tsx";

const file = (prefix: string, n: number) => Array.from({ length: n }, (_, i) => `${prefix} ${i + 1}`).join("\n") + "\n";

test("a collapsed full rewrite shows the start of the new file, not only removed lines", () => {
  const { lines } = diffText(file("old", 30), file("new", 30));
  const { lastFrame } = render(<DiffView lines={lines} max={15} hint="ctrl+o" />);
  const frame = lastFrame()!;
  expect(frame).toContain("- old 1");
  expect(frame).not.toContain("old 4");
  expect(frame).toContain("+ new 1");
  expect(frame).toContain("+ new 11");
  expect(frame).not.toContain("new 12");
  // 60 lines, 3 removed and 11 added shown.
  expect(frame).toContain("… 46 more lines (ctrl+o)");
});

test("expanded, a full rewrite shows every line in diff order", () => {
  const { lines } = diffText(file("old", 30), file("new", 30));
  const frame = render(<DiffView lines={lines} />).lastFrame()!;
  expect(frame).toContain("old 30");
  expect(frame.indexOf("old 30")).toBeLessThan(frame.indexOf("new 1"));
  expect(frame).not.toContain("more line");
});

test("a small change fits as it is", () => {
  const { lines } = diffText(file("line", 10), file("line", 10).replace("line 5\n", "line five\n"));
  const frame = render(<DiffView lines={lines} max={15} />).lastFrame()!;
  expect(frame).toContain("- line 5");
  expect(frame).toContain("+ line five");
  expect(frame).not.toContain("more line");
});
