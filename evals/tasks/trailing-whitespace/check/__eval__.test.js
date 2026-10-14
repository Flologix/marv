import { expect, test } from "bun:test";
import { defaults, getConfig } from "./src/config.js";

test("new defaults", () => {
  expect(defaults.timeout).toBe(60);
  expect(defaults.retries).toBe(3);
  expect(getConfig().retries).toBe(3);
});
test("the rest is unchanged", () => {
  expect(defaults.baseUrl).toBe("https://api.example.com");
  expect(getConfig({ timeout: 5 }).timeout).toBe(5);
  expect(getConfig({ headers: { a: "b" } }).headers).toEqual({ "user-agent": "example-client/1.0", a: "b" });
});
