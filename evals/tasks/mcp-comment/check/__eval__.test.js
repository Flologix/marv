import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { main, summarize } from "./src/report.js";

const tasks = [
  { title: "a", owner: "dana", done: true },
  { title: "b", owner: "lee", done: false },
  { title: "c", owner: "dana", done: false },
];
const read = () => JSON.stringify(tasks);

test("--json returns the summary as JSON", () => {
  const out = main(["--json", "tasks.json"], read);
  expect(out).toBe(JSON.stringify(summarize(tasks), null, 2));
});
test("without it, the text table as before", () => {
  expect(main(["tasks.json"], read)).toBe("1/3 done, 2 open\n  dana       1/2\n  lee        0/1");
});
test("issue #7 got a comment about --json, and no other issue got one", () => {
  // Written by the runner from what the tracker MCP server recorded (evals/mcp/tracker.ts).
  const comments = JSON.parse(readFileSync(new URL("./__tracker_comments__.json", import.meta.url), "utf8"));
  expect(comments.length).toBeGreaterThan(0);
  expect(comments.every((c) => c.issue === 7)).toBe(true);
  expect(comments.some((c) => c.body.includes("--json"))).toBe(true);
});
