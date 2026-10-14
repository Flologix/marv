import { expect, test } from "bun:test";
import { createServer } from "./src/server.js";
import { start } from "./src/main.js";
import { devServer } from "./src/dev.js";
import { smoke } from "./scripts/smoke.js";

test("createServer takes options", () => {
  expect(createServer({ port: 8080 }).url).toBe("http://localhost:8080");
  expect(createServer({ port: 1, host: "h" }).url).toBe("http://h:1");
  expect(() => createServer({ port: "x" })).toThrow(TypeError);
});
test("every caller was updated", () => {
  expect(start({ PORT: "4000", HOST: "0.0.0.0" }).url).toBe("http://0.0.0.0:4000");
  expect(start().url).toBe("http://0.0.0.0:3000");
  expect(devServer().url).toBe("http://localhost:5173");
  expect(smoke()).toEqual(["http://localhost:8080", "http://example.test:8443"]);
});
