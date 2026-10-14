import { expect, test } from "bun:test";
import { makeUser } from "./src/factory.ts";
import { seed } from "./src/seed.ts";
import { greet } from "./src/greet.ts";
import { welcomeGuest, welcomeOwner } from "./src/guest.ts";
import { importUsers } from "./src/csv.ts";

test("makeUser takes the email", () => {
  expect(makeUser("lin", "Lin", "lin@x.io")).toEqual({ id: "lin", name: "Lin", email: "lin@x.io" });
});
test("greet shows it", () => {
  expect(greet({ id: "ada", name: "Ada Lovelace", email: "ada@example.com" })).toBe("Hi Ada Lovelace <ada@example.com>");
});
test("every user gets one", () => {
  for (const user of seed) expect(user.email).toBe(`${user.id}@example.com`);
  expect(importUsers("a,A\nb,B").map((user) => user.email)).toEqual(["a@example.com", "b@example.com"]);
  expect(welcomeGuest()).toBe("Hi Guest <guest@example.com>");
  expect(welcomeOwner()).toBe("Hi Root <root@example.com>");
});
test("it typechecks", () => {
  const tsc = Bun.spawnSync(["node_modules/.bin/tsc", "--noEmit", "--pretty", "false", "-p", "tsconfig.eval.json"]);
  expect({ code: tsc.exitCode, out: tsc.stdout.toString() + tsc.stderr.toString() }).toEqual({ code: 0, out: "" });
});
