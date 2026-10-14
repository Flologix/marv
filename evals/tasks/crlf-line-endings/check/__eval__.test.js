import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { greet, greetAll } from "./src/greet.js";

test("greets by name, trimmed", () => {
  expect(greet("Ada")).toBe("Hello, Ada!");
  expect(greet("  Ada ")).toBe("Hello, Ada!");
});
test("a missing or blank name is a stranger", () => {
  expect(greet()).toBe("Hello, stranger!");
  expect(greet("")).toBe("Hello, stranger!");
  expect(greet("   ")).toBe("Hello, stranger!");
});
test("greetAll still works", () => {
  expect(greetAll(["A", "B"])).toBe("Hello, A!\nHello, B!");
});
test("the file keeps its Windows (CRLF) line endings", () => {
  const text = readFileSync("src/greet.js", "utf8");
  expect(text.includes("\r\n")).toBe(true);
  expect(text.replace(/\r\n/g, "").includes("\n")).toBe(false);
});
