import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, readFile, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { ConfigError, ConfigStore, maskKey, migrateLegacyConfig, needsSetup, PRESETS, resolveConfig } from "../src/config/config.ts";

let dir: string;
let store: ConfigStore;

beforeEach(async () => {
  dir = await mkdtemp(join(tmpdir(), "marv-config-"));
  store = new ConfigStore(join(dir, "nested"));
});
afterEach(() => rm(dir, { recursive: true, force: true }));

describe("ConfigStore", () => {
  test("load returns null on first run", async () => {
    expect(await store.load()).toBeNull();
  });

  test("save then load round-trips, and the file is private (0600)", async () => {
    const config = { provider: "openrouter" as const, model: "openai/gpt-5.6-sol", apiKey: "sk-or-test" };
    await store.save(config);
    expect(await store.load()).toEqual(config);
    expect((await stat(store.path)).mode & 0o777).toBe(0o600);
  });

  test("rejects invalid JSON", async () => {
    await store.save({ provider: "ollama" });
    await writeFile(store.path, "{not json");
    await expect(store.load()).rejects.toBeInstanceOf(ConfigError);
  });

  test("drops a provider Marv no longer has (like the old echo), keeping the rest", async () => {
    await store.save({ provider: "ollama" });
    await writeFile(store.path, JSON.stringify({ provider: "echo", apiKey: "sk-or-keep" }));
    const file = await store.load();
    expect(file).toEqual({ apiKey: "sk-or-keep" });
    expect(needsSetup(file, resolveConfig(file, {}))).toBe(true); // so setup opens instead of a crash
  });
});

describe("resolveConfig", () => {
  const file = { provider: "openrouter" as const, model: "z-ai/glm-5.3", apiKey: "sk-from-file" };

  test("uses the file and the preset endpoint when no env vars are set", () => {
    expect(resolveConfig(file, {})).toEqual({ ...file, baseUrl: PRESETS.openrouter.baseUrl, apiKeySource: "file", thinking: false, contextLength: 32768, sandbox: true, yolo: true, trajectories: true, diagnostics: true, maxSteps: 25 });
  });

  test("diagnostics: on unless the file turns it off", () => {
    expect(resolveConfig(file, {}).diagnostics).toBe(true);
    expect(resolveConfig({ ...file, diagnostics: false }, {}).diagnostics).toBe(false);
  });

  test("maxSteps: the default is 25, the file sets the interval, 0 and \"off\" never ask", () => {
    expect(resolveConfig(file, {}).maxSteps).toBe(25);
    expect(resolveConfig({ ...file, maxSteps: 40 }, {}).maxSteps).toBe(40);
    expect(resolveConfig({ ...file, maxSteps: 0 }, {}).maxSteps).toBe(0);
    expect(resolveConfig({ ...file, maxSteps: "off" }, {}).maxSteps).toBe(0);
  });

  test("a maxSteps that isn't 0, off, or a whole number is rejected", async () => {
    await mkdir(join(dir, "nested"), { recursive: true });
    for (const maxSteps of [-1, 2.5, "sometimes", 1]) {
      await writeFile(store.path, JSON.stringify({ provider: "openrouter", maxSteps }));
      await expect(store.load()).rejects.toBeInstanceOf(ConfigError);
    }
  });

  test("MARV_MAX_STEPS overrides the file, like MARV_MODEL; an unusable value is ignored", () => {
    expect(resolveConfig(file, { MARV_MAX_STEPS: "50" }).maxSteps).toBe(50);
    expect(resolveConfig({ ...file, maxSteps: 40 }, { MARV_MAX_STEPS: "0" }).maxSteps).toBe(0);
    expect(resolveConfig(file, { MARV_MAX_STEPS: "OFF" }).maxSteps).toBe(0);
    for (const junk of ["junk", "-5", "2.5", "1", ""]) {
      expect(resolveConfig({ ...file, maxSteps: 40 }, { MARV_MAX_STEPS: junk }).maxSteps).toBe(40);
    }
  });

  test("yolo mode is on unless the file turns it off", () => {
    expect(resolveConfig(file, {}).yolo).toBe(true);
    expect(resolveConfig({ ...file, yolo: false }, {}).yolo).toBe(false);
  });

  test("env vars override the file", () => {
    const config = resolveConfig(file, { OPENROUTER_API_KEY: "sk-from-env", MARV_MODEL: "openai/gpt-5.6-luna" });
    expect(config).toMatchObject({ apiKey: "sk-from-env", apiKeySource: "env", model: "openai/gpt-5.6-luna" });
  });

  test("defaults to OpenRouter and its default model with no file at all", () => {
    expect(resolveConfig(null, {})).toMatchObject({ provider: "openrouter", model: PRESETS.openrouter.defaultModel });
  });

  test("Ollama needs no key, even if one is saved or set", () => {
    const config = resolveConfig({ provider: "ollama", model: "qwen3.5:9b", apiKey: "sk-or" }, { OPENROUTER_API_KEY: "k" });
    expect(config).toMatchObject({ baseUrl: "http://localhost:11434", apiKey: undefined, apiKeySource: undefined });
  });

  test("Ollama honors OLLAMA_HOST, with or without a scheme", () => {
    const ollama = { provider: "ollama" as const, model: "m" };
    expect(resolveConfig(ollama, { OLLAMA_HOST: "10.0.0.5:11434" }).baseUrl).toBe("http://10.0.0.5:11434");
    expect(resolveConfig(ollama, { OLLAMA_HOST: "https://box.lan/" }).baseUrl).toBe("https://box.lan");
  });

  test("a baseUrl in the file wins over the preset", () => {
    expect(resolveConfig({ provider: "ollama", model: "m", baseUrl: "http://gpu:11434" }, {}).baseUrl).toBe("http://gpu:11434");
  });
});

describe("needsSetup", () => {
  test("on first run", () => {
    expect(needsSetup(null, resolveConfig(null, { OPENROUTER_API_KEY: "k" }))).toBe(true);
  });
  test("when OpenRouter has no key", () => {
    const file = { provider: "openrouter" as const, model: "z-ai/glm-5.3" };
    expect(needsSetup(file, resolveConfig(file, {}))).toBe(true);
    expect(needsSetup(file, resolveConfig(file, { OPENROUTER_API_KEY: "k" }))).toBe(false);
  });
  test("when Ollama has no model picked yet", () => {
    expect(needsSetup({ provider: "ollama" }, resolveConfig({ provider: "ollama" }, {}))).toBe(true);
    const file = { provider: "ollama" as const, model: "qwen3.5:9b" };
    expect(needsSetup(file, resolveConfig(file, {}))).toBe(false);
  });
  test("not when a provider and model are set and no key is needed", () => {
    const file = { provider: "ollama" as const, model: "qwen3.5:9b" };
    expect(needsSetup(file, resolveConfig(file, {}))).toBe(false);
  });
});

test("maskKey hides the middle of a key", () => {
  expect(maskKey("sk-or-v1-abcdefghijklmnop-wxyz")).toBe("sk-or-v1-a…wxyz");
  expect(maskKey("short")).toBe("****");
});

describe("migrateLegacyConfig (Marv used to be called Ekko)", () => {
  let home: string;
  beforeEach(async () => {
    home = await mkdtemp(join(tmpdir(), "marv-home-"));
  });
  afterEach(() => rm(home, { recursive: true, force: true }));

  test("copies ~/.ekko/config.json to ~/.marv/, privately, and leaves the old one", async () => {
    await mkdir(join(home, ".ekko"));
    await writeFile(join(home, ".ekko", "config.json"), '{"provider":"openrouter","apiKey":"sk-or-old"}');
    expect(await migrateLegacyConfig(home)).toBe(true);
    expect(await readFile(join(home, ".marv", "config.json"), "utf8")).toBe('{"provider":"openrouter","apiKey":"sk-or-old"}');
    expect((await stat(join(home, ".marv", "config.json"))).mode & 0o777).toBe(0o600);
    expect(await readFile(join(home, ".ekko", "config.json"), "utf8")).toContain("sk-or-old");
  });

  test("never overwrites an existing ~/.marv config, and does nothing without an old one", async () => {
    expect(await migrateLegacyConfig(home)).toBe(false);
    await mkdir(join(home, ".ekko"));
    await writeFile(join(home, ".ekko", "config.json"), '{"provider":"openrouter"}');
    await mkdir(join(home, ".marv"));
    await writeFile(join(home, ".marv", "config.json"), '{"provider":"ollama"}');
    expect(await migrateLegacyConfig(home)).toBe(false);
    expect(await readFile(join(home, ".marv", "config.json"), "utf8")).toBe('{"provider":"ollama"}');
  });
});
