import { expect, test } from "bun:test";
import { match } from "./src/router.js";

test("the new route", () => {
  expect(match("GET", "/users/4/posts")).toEqual({ handler: "userPosts", params: { id: "4" } });
  expect(match("POST", "/users/4/posts")).toBeNull();
});
test("users can be deleted", () => {
  expect(match("DELETE", "/users/4")).toEqual({ handler: "user", params: { id: "4" } });
  expect(match("GET", "/users/4")).toEqual({ handler: "user", params: { id: "4" } });
});
test("the rest is unchanged", () => {
  expect(match("GET", "/")).toEqual({ handler: "home", params: {} });
  expect(match("POST", "/login")?.handler).toBe("login");
  expect(match("DELETE", "/login")).toBeNull();
});
