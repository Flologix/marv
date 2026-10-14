// One test (or two) per ask in task.md: forgetting any one of them fails the run, and testsPassed shows how many landed.
import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { run } from "./src/cli.js";

const CSV = "name,amount\nana,1200\ncy,450\nbo,950\n";
function report(...argv) {
  const out = [];
  const err = [];
  const code = run(["sales.csv", ...argv], { readFile: () => CSV, out: (t) => out.push(t), err: (t) => err.push(t) });
  return { code, out: out.join("\n"), err: err.join("\n") };
}

test("--json prints the rows and the total as JSON", () => {
  const { code, out } = report("--json");
  expect(code).toBe(0);
  expect(JSON.parse(out)).toEqual({
    rows: [
      { name: "ana", amount: 1200 },
      { name: "bo", amount: 950 },
      { name: "cy", amount: 450 },
    ],
    total: 2600,
  });
});

test("--top N shows the N largest rows", () => {
  const { code, out, err } = report("--top", "2");
  expect(code).toBe(0);
  expect(out).toContain("ana");
  expect(out).toContain("bo");
  expect(out).not.toContain("cy");
  expect(err).toBe("");
  expect(JSON.parse(report("--json", "--top", "1").out).rows).toEqual([{ name: "ana", amount: 1200 }]);
});

test("--limit still works, with a deprecation warning on stderr that points to --top", () => {
  const { code, out, err } = report("--limit", "2");
  expect(code).toBe(0);
  expect(out).not.toContain("cy");
  expect(err).toMatch(/deprecat/i);
  expect(err).toContain("--top");
});

test("the total counts every row, the first one included", () => {
  const { out } = report();
  expect(out).toMatch(/Total\s+2600/);
});

const section = (text, heading) => {
  const start = text.search(new RegExp(`^##\\s+${heading}`, "mi"));
  if (start < 0) return "";
  const rest = text.slice(start).split("\n").slice(1).join("\n");
  const end = rest.search(/^##\s/m);
  return end < 0 ? rest : rest.slice(0, end);
};

test("the README's usage section documents --json and --top, not --limit as the option", () => {
  const usage = section(readFileSync("README.md", "utf8"), "Usage");
  expect(usage).toContain("--json");
  expect(usage).toContain("--top");
  // --limit may be mentioned as deprecated, but it isn't the option shown in the synopsis any more.
  expect(usage).not.toMatch(/report <export\.csv> \[--limit/);
});

test("CHANGELOG.md has an Unreleased entry for the changes", () => {
  const unreleased = section(readFileSync("CHANGELOG.md", "utf8"), "\\[?Unreleased");
  expect(unreleased).toContain("--json");
  expect(unreleased).toContain("--top");
  expect(readFileSync("CHANGELOG.md", "utf8")).toContain("## [1.3.0] - 2026-09-01");
});
