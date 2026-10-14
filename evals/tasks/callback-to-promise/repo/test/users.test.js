import { expect, test } from "bun:test";
import { join } from "node:path";
import { addUser, findUser, listUsers } from "../src/users.js";
import { tempDir } from "./helpers.js";

test("adds and finds users", (done) => {
  const path = join(tempDir(), "users.json");
  addUser(path, "bo", "2026-01-02", (err, user) => {
    expect(err).toBeNull();
    expect(user).toEqual({ name: "bo", joined: "2026-01-02" });
    findUser(path, "bo", (err2, found) => {
      expect(found).toEqual(user);
      listUsers(path, (err3, names) => {
        expect(names).toEqual(["bo"]);
        done();
      });
    });
  });
});

test("a name can't be added twice", (done) => {
  const path = join(tempDir(), "users.json");
  addUser(path, "bo", "x", () => {
    addUser(path, "bo", "y", (err) => {
      expect(err.message).toContain("already exists");
      done();
    });
  });
});
