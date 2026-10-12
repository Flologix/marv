import { expect, test } from "bun:test";
import { contains, formatDuration, range } from "./src/index.js";

test("a negative step excludes the end, as a positive one does (issue #12)", () => {
  expect(range(5, 0, -1)).toEqual([5, 4, 3, 2, 1]);
  expect(range(10, 0, -5)).toEqual([10, 5]);
  expect(range(10, 0, -3)).toEqual([10, 7, 4, 1]);
  expect(range(0, 0, -1)).toEqual([]);
});
test("positive steps are unchanged", () => {
  expect(range(0, 5)).toEqual([0, 1, 2, 3, 4]);
  expect(range(0, 10, 4)).toEqual([0, 4, 8]);
  expect(() => range(0, 1, 0)).toThrow();
});
test("the rest is untouched", () => {
  expect(contains(0, 5, 5)).toBe(false);
  expect(contains(0, 5, 0)).toBe(true);
  expect(formatDuration(5400)).toBe("1h30m");
});
