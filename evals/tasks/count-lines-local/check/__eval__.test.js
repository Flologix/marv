// #81: the vendored copy is on disk; its README points at an upstream the sandbox can't reach. The exact count is
// 1,318 lines (wc -l over vendor/tinyparse/src/*.js); an estimate from sizes or a guess fails. And it gets there in
// at most 8 requests with the sandbox on (no network approved), as #81 asks: opencode took 6 on the original prompt,
// Marv 20, most of them a clone the sandbox couldn't make.
import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";

const reply = readFileSync("./__eval_reply__.md", "utf8");
// bench.py writes the run's request count here after the run (RUN_FILE).
const run = JSON.parse(readFileSync("./__eval_run__.json", "utf8"));

test("the reply gives the exact line count", () => {
  // 1318 or 1,318 on its own: not part of a larger number (1,318,000, 11,318, 1318.5).
  const found = [...reply.matchAll(/(?<![\d.,])1,?318(?![\d]|[.,]\d)/g)];
  expect(found.length).toBeGreaterThan(0);
  // Given as the count, not hedged: "about 1,318", "~1,318", "over 1,318 lines".
  for (const m of found) {
    const before = reply.slice(Math.max(0, m.index - 30), m.index).toLowerCase();
    expect({ before, hedged: /(about|around|approx\w*|roughly|nearly|almost|over|under|some|~|≈)[\s*_]*$/.test(before) })
      .toEqual({ before, hedged: false });
  }
});

test("it takes at most 8 requests", () => {
  expect(typeof run.requests).toBe("number");
  expect(run.requests).toBeLessThanOrEqual(8);
});
