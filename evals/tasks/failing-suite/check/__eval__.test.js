import { expect, test } from "bun:test";
import { countByLevel, errors, parseLog, summarize } from "./src/logscan.js";

// A different log than the repo's own tests use, so passing them isn't enough.
const log = Array.from({ length: 400 }, (_, i) => {
  const minute = String(Math.floor(i / 60) % 3).padStart(2, "0");
  const second = String(i % 60).padStart(2, "0");
  const level = i % 20 === 19 ? "ERROR" : i % 4 === 3 ? "WARN" : "INFO";
  return `2026-10-12T09:${minute}:${second}Z ${level} request ${i} took ${5 + (i % 25)}ms`;
}).join("\n");

const entries = parseLog(log);

test("every entry is summarized", () => {
  const lines = summarize(entries);
  expect(lines).toHaveLength(400);
  expect(lines[0]).toBe("09:00:00Z INFO request 0 took 5ms");
  expect(lines[1]).toBe("09:00:01Z INFO request 1 took 6ms");
  expect(lines.at(-1)).toBe("09:00:39Z ERROR request 399 took 29ms");
});

test("no entry is dropped, in the middle either", () => {
  expect(linesAt(summarize(entries), 118, 120)).toEqual([
    "09:01:58Z INFO request 118 took 23ms",
    "09:01:59Z ERROR request 119 took 24ms",
  ]);
  expect(linesAt(summarize(entries), 0, 2)).toEqual(["09:00:00Z INFO request 0 took 5ms", "09:00:01Z INFO request 1 took 6ms"]);
});

test("levels are counted over every entry", () => {
  expect(countByLevel(entries)).toEqual({ INFO: 300, WARN: 80, ERROR: 20 });
  expect(errors(entries)).toHaveLength(20);
  expect(errors(entries)[0]).toEqual({ time: "2026-10-12T09:00:19Z", level: "ERROR", message: "request 19 took 24ms" });
});

test("a round trip keeps the message", () => {
  const again = parseLog(summarize(entries).map((line) => `2026-10-12T${line}`).join("\n"));
  expect(again.map((entry) => entry.message)).toEqual(entries.map((entry) => entry.message));
});

/** summarize()'s lines for entries `from` (inclusive) to `to` (exclusive), found by their request number. */
function linesAt(lines, from, to) {
  return lines.filter((line) => {
    const n = Number(/request (\d+)/.exec(line)?.[1]);
    return n >= from && n < to;
  });
}
