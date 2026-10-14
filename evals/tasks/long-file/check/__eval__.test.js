import { expect, test } from "bun:test";
import { formatDuration, formatKilobytes, formatMilesShort } from "./src/format.js";

test("formatDuration", () => {
  expect(formatDuration(3725)).toBe("1h 2m 5s");
  expect(formatDuration(7200)).toBe("2h 0m 0s");
  expect(formatDuration(125)).toBe("2m 5s");
  expect(formatDuration(9)).toBe("9s");
  expect(formatDuration(-1)).toBe("—");
});
test("the other formatters are untouched", () => {
  expect(formatKilobytes(2048)).toBe("2.00 KB");
  expect(formatMilesShort(3218.688)).toBe("2mi");
});
