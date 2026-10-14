import { expect, test } from "bun:test";
import { join } from "node:path";
import { increment } from "../src/counter.js";
import { getSetting, setSetting } from "../src/settings.js";
import { tempDir } from "./helpers.js";

test("settings fall back, then hold what was set", async () => {
  const path = join(tempDir(), "settings.json");
  expect(await getSetting(path, "title", "Report")).toBe("Report");
  expect(await setSetting(path, "title", "Weekly")).toBeUndefined();
  expect(await getSetting(path, "title", "Report")).toBe("Weekly");
});

test("counters count", async () => {
  const path = join(tempDir(), "counters.json");
  expect(await increment(path, "visits")).toBe(1);
  expect(await increment(path, "visits", 5)).toBe(6);
});
