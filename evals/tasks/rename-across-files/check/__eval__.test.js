import { expect, test } from "bun:test";
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import * as lib from "./src/index.js";

const client = { get: async (url) => ({ data: { name: "Ada", email: "ada@x.io", team: "core", url } }) };

test("loadUser is exported and works", async () => {
  expect(typeof lib.loadUser).toBe("function");
  expect((await lib.loadUser(1, { client })).url).toBe("/users/1");
  expect(await lib.profileCard(1, { client })).toBe("Ada <ada@x.io>");
  expect((await lib.compareUsers(1, 2, { client })).sameTeam).toBe(true);
});
test("its error message uses the new name", async () => {
  await expect(lib.loadUser(undefined, { client })).rejects.toThrow("loadUser");
});
test("the old name is gone everywhere", () => {
  const files = (dir) => readdirSync(dir).flatMap((name) => {
    if (name === ".git" || name === "node_modules" || name.startsWith("__eval__")) return [];
    const path = join(dir, name);
    return statSync(path).isDirectory() ? files(path) : [path];
  });
  expect(files(".").filter((f) => readFileSync(f, "utf8").includes("fetchUserData"))).toEqual([]);
});
