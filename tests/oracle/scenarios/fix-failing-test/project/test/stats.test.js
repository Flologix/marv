import { expect, test } from "bun:test";
import { mean, median } from "../src/stats.js";

test("mean", () => expect(mean([1, 2, 3, 4])).toBe(2.5));
test("median of an odd-length list", () => expect(median([3, 1, 2])).toBe(2));
test("median of an even-length list", () => expect(median([4, 1, 3, 2])).toBe(2.5));
