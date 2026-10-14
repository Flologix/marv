import { expect, test } from "bun:test";
import { parseCsv } from "../src/csv.js";
import { topRows } from "../src/report.js";

test("parseCsv skips the header and blank lines", () => {
  expect(parseCsv("name,amount\nana,10\n\nbo,5\n")).toEqual([
    { name: "ana", amount: 10 },
    { name: "bo", amount: 5 },
  ]);
});

test("topRows sorts and limits", () => {
  const rows = [{ name: "a", amount: 1 }, { name: "b", amount: 3 }, { name: "c", amount: 2 }];
  expect(topRows(rows, 2).map((r) => r.name)).toEqual(["b", "c"]);
});
