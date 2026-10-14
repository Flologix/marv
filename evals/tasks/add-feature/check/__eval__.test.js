import { expect, test } from "bun:test";
import { slugify } from "./src/index.js";

test("basics", () => {
  expect(slugify("Hello, World!")).toBe("hello-world");
  expect(slugify("  Many   spaces -- and dashes  ")).toBe("many-spaces-and-dashes");
  expect(slugify("Version 2.0 Release")).toBe("version-2-0-release");
  expect(slugify("")).toBe("");
});
test("accents are removed", () => {
  expect(slugify("Crème brûlée")).toBe("creme-brulee");
});
test("separator", () => {
  expect(slugify("Hello World", { separator: "_" })).toBe("hello_world");
});
test("maxLength cuts at a word boundary", () => {
  expect(slugify("hello wonderful world", { maxLength: 10 })).toBe("hello");
  expect(slugify("hello wonderful world", { maxLength: 15 })).toBe("hello-wonderful");
  expect(slugify("a b c", { maxLength: 100 })).toBe("a-b-c");
});
