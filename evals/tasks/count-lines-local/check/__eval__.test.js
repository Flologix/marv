// #81: the vendored copy is on disk; its README points at an upstream the sandbox can't reach. The exact count is
// 1,318 lines (wc -l over vendor/tinyparse/src/*.js); an estimate from sizes or a guess fails.
import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";

const reply = readFileSync("./__eval_reply__.md", "utf8");

test("the reply gives the exact line count", () => {
  expect(reply).toMatch(/\b1,?318\b/);
});
