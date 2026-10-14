import { expect, test } from "bun:test";
import { mean, median } from "./src/stats.js";

test("median", () => {
  expect(median([3, 1, 2])).toBe(2);
  expect(median([4, 1, 3, 2])).toBe(2.5);
  expect(median([10, -5, 0, 7, 3])).toBe(3);
  expect(median([7])).toBe(7);
  expect(median([])).toBeNaN();
});
test("mean", () => {
  expect(mean([1, 2, 3, 4])).toBe(2.5);
  expect(mean([])).toBeNaN();
});
