import { expect, test } from "bun:test";
import * as store from "./src/store.ts";
import { loadSettings } from "./src/settings.ts";
import { theme } from "./src/theme.ts";
import { banner } from "./src/banner.ts";
import { debug } from "./src/log.ts";
import { settingNames } from "./src/keys.ts";
import { isDark } from "./src/css.ts";

test("loadSettings is async, and readSync is gone", async () => {
  const settings = loadSettings();
  expect(settings).toBeInstanceOf(Promise);
  expect(await settings).toEqual({ theme: "dark", verbose: true });
  expect(store.readSync).toBeUndefined();
});
test("everything that uses it still works", async () => {
  expect(await theme()).toBe("dark");
  expect(await banner()).toBe("Theme: dark");
  expect(await debug("hi")).toBe("[debug] hi");
  expect(await settingNames()).toEqual(["theme", "verbose"]);
  expect(await isDark()).toBe(true);
});
test("it typechecks", () => {
  const tsc = Bun.spawnSync(["node_modules/.bin/tsc", "--noEmit", "--pretty", "false", "-p", "tsconfig.json"]);
  expect({ code: tsc.exitCode, out: tsc.stdout.toString() + tsc.stderr.toString() }).toEqual({ code: 0, out: "" });
});
