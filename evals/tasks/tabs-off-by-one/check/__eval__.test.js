import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { pageNumbers, paginate } from "./src/pagination.js";

const items = Array.from({ length: 25 }, (_, i) => i + 1);

test("page 1 is the first perPage items", () => {
  expect(paginate(items, 1, 10).items).toEqual([1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
});
test("the last page holds the rest", () => {
  expect(paginate(items, 3, 10).items).toEqual([21, 22, 23, 24, 25]);
});
test("totalPages counts a partly filled page", () => {
  expect(paginate(items, 1, 10).totalPages).toBe(3);
  expect(paginate(items.slice(0, 20), 1, 10).totalPages).toBe(2);
});
test("still validates and numbers pages", () => {
  expect(() => paginate("x", 1)).toThrow(TypeError);
  expect(pageNumbers(3)).toEqual([1, 2, 3]);
});
test("the file is still indented with tabs", () => {
  const lines = readFileSync("src/pagination.js", "utf8").split("\n");
  expect(lines.filter((l) => /^ +\S/.test(l) && !/^ \*/.test(l))).toEqual([]);
});
