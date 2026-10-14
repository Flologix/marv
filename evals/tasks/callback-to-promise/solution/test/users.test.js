import { expect, test } from "bun:test";
import { join } from "node:path";
import { addUser, findUser, listUsers } from "../src/users.js";
import { tempDir } from "./helpers.js";

test("adds and finds users", async () => {
  const path = join(tempDir(), "users.json");
  const user = await addUser(path, "bo", "2026-01-02");
  expect(user).toEqual({ name: "bo", joined: "2026-01-02" });
  expect(await findUser(path, "bo")).toEqual(user);
  expect(await listUsers(path)).toEqual(["bo"]);
});

test("a name can't be added twice", async () => {
  const path = join(tempDir(), "users.json");
  await addUser(path, "bo", "x");
  await expect(addUser(path, "bo", "y")).rejects.toThrow("already exists");
});
