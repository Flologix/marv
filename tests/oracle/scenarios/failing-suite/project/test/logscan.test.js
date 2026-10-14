import { expect, test } from "bun:test";
import { countByLevel, errors, parseLog, summarize } from "../src/logscan.js";

const log = Array.from({ length: 500 }, (_, i) => {
  const second = String(i % 60).padStart(2, "0");
  const level = i % 25 === 24 ? "ERROR" : i % 5 === 4 ? "WARN" : "INFO";
  return `2026-10-12T09:00:${second}Z ${level} request ${i} handled in ${10 + (i % 40)}ms`;
}).join("\n");

const entries = parseLog(log);

test("every entry is summarized", () => {
  const lines = summarize(entries);
  expect(lines).toHaveLength(500);
  expect(lines[0]).toBe("09:00:00Z INFO request 0 handled in 10ms");
  expect(lines[499]).toBe("09:00:19Z ERROR request 499 handled in 29ms");
});

test("levels are counted over every entry", () => {
  expect(countByLevel(entries)).toEqual({ INFO: 400, WARN: 80, ERROR: 20 });
  expect(errors(entries)).toHaveLength(20);
});

test("a round trip keeps the message", () => {
  const again = parseLog(summarize(entries).map((line) => `2026-10-12T${line}`).join("\n"));
  expect(again.map((entry) => entry.message)).toEqual(entries.map((entry) => entry.message));
});
