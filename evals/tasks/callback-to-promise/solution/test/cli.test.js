import { expect, test } from "bun:test";
import { main } from "../src/cli.js";
import { tempDir } from "./helpers.js";

test("the commands, then the report", async () => {
  const dir = tempDir();
  expect(await main(dir, ["add-user", "ana"])).toBe("added ana");
  await main(dir, ["visit"]);
  await main(dir, ["set", "title", "Weekly"]);
  expect(await main(dir, ["report"])).toBe("Weekly: 1 users (ana); 1 visits");
});
