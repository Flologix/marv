import { expect, test } from "bun:test";
import { join } from "node:path";
import { increment } from "../src/counter.js";
import { getSetting, setSetting } from "../src/settings.js";
import { tempDir } from "./helpers.js";

test("settings fall back, then hold what was set", (done) => {
  const path = join(tempDir(), "settings.json");
  getSetting(path, "title", "Report", (err, title) => {
    expect(title).toBe("Report");
    setSetting(path, "title", "Weekly", (err2, previous) => {
      expect(previous).toBeUndefined();
      getSetting(path, "title", "Report", (err3, now) => {
        expect(now).toBe("Weekly");
        done();
      });
    });
  });
});

test("counters count", (done) => {
  const path = join(tempDir(), "counters.json");
  increment(path, "visits", (err, one) => {
    expect(one).toBe(1);
    increment(path, "visits", 5, (err2, six) => {
      expect(six).toBe(6);
      done();
    });
  });
});
