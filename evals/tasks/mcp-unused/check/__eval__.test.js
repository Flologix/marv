import { expect, test } from "bun:test";
import { formatDuration, parseDuration } from "./src/index.js";

test("parses what formatDuration writes", () => {
  expect(parseDuration("1h30m")).toBe(5400);
  expect(parseDuration("45s")).toBe(45);
  expect(parseDuration("2h5s")).toBe(7205);
  expect(parseDuration("  10m ")).toBe(600);
  for (const n of [0, 1, 59, 60, 3599, 3600, 3661, 86399]) expect(parseDuration(formatDuration(n))).toBe(n);
});
test("anything else throws a RangeError", () => {
  for (const bad of ["", "5", "1m1h", "1x", "h", "1h 30m"]) expect(() => parseDuration(bad)).toThrow(RangeError);
});
