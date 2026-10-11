import { afterEach, describe, expect, test } from "bun:test";
import { resolveConfig } from "../src/config/config.ts";
import { configFactory, isFactory, providerFactory, type ProviderOption } from "../src/provider/factory.ts";
import { OllamaProvider } from "../src/provider/ollama.ts";
import { OpenAICompatProvider } from "../src/provider/openai-compat.ts";
import type { Provider } from "../src/provider/types.ts";
import { ScriptedProvider } from "./fake-provider.ts";

test("an OpenRouter option makes an OpenRouter provider, and can look the model up", () => {
  const factory = providerFactory({ kind: "openrouter", apiKey: "sk-test", model: "x/y" });
  expect(factory).toMatchObject({ id: "openrouter", model: "x/y", local: false });
  const provider = factory.make();
  expect(provider).toBeInstanceOf(OpenAICompatProvider);
  expect(provider.name).toBe("openrouter · x/y");
  expect(factory.lookup).toBeDefined();
});

test("an Ollama option is local, with its host and context window, and nothing to look up", () => {
  const factory = providerFactory({ kind: "ollama", model: "qwen3.5:9b", host: "gpu-box:11434", contextLength: 65536 });
  expect(factory).toMatchObject({ id: "ollama", model: "qwen3.5:9b", local: true });
  const provider = factory.make();
  expect(provider).toBeInstanceOf(OllamaProvider);
  expect(provider.contextLength).toBe(65536);
  expect(factory.lookup).toBeUndefined();
});

test("a Provider of your own is used as is, also for subagents", () => {
  const own = new ScriptedProvider([]);
  const factory = providerFactory(own);
  expect(factory).toMatchObject({ id: "custom", model: "scripted" });
  expect(factory.make()).toBe(own);
  expect(factory.make("another/model")).toBe(own);
});

test("configFactory: the TUI's config, with the maker and model list it was given", async () => {
  const config = { ...resolveConfig({ provider: "openrouter", model: "a/b", apiKey: "k" }, {}), thinking: true };
  const made: unknown[] = [];
  const factory = configFactory(
    config,
    (c, info) => (made.push([c.model, c.thinking, info]), new ScriptedProvider([])),
    async () => [{ id: "a/b", context: 1000 }, { id: "c/d" }],
  );
  factory.make();
  factory.make("c/d");
  factory.make(undefined, { reasoning: "optional" });
  expect(made).toEqual([
    ["a/b", true, undefined],
    ["c/d", true, undefined],
    ["a/b", true, { reasoning: "optional" }],
  ]);
  expect(await factory.lookup!()).toEqual({ id: "a/b", context: 1000 });
  expect(isFactory(factory)).toBe(true);
  expect(isFactory(new ScriptedProvider([]))).toBe(false);
});

test("an unknown kind is an error, not a silent Ollama on localhost", () => {
  const bad = { kind: "OpenRouter", apiKey: "k" } as unknown as ProviderOption;
  expect(() => providerFactory(bad)).toThrow('Unknown provider kind "OpenRouter": use "openrouter" or "ollama", or pass a Provider.');
});

describe("error hints name what to change: the TUI's commands, the SDK's options", () => {
  const servers: ReturnType<typeof Bun.serve>[] = [];
  afterEach(() => servers.splice(0).forEach((server) => server.stop(true)));
  // Every request is answered with this status, as OpenRouter and Ollama would.
  const serve = (status: number) => {
    const server = Bun.serve({ port: 0, fetch: () => Response.json({ error: { message: "nope" } }, { status }) });
    servers.push(server);
    return `http://localhost:${server.port}`;
  };
  const errorOf = async (provider: Provider) => {
    for await (const event of provider.stream([{ role: "user", text: "hi" }])) if (event.type === "error") return event.message;
  };
  const tui = (baseUrl: string) => configFactory(resolveConfig({ provider: "openrouter", model: "x/y", apiKey: "k", baseUrl }, {}));
  const sdk = (baseUrl: string) => providerFactory({ kind: "openrouter", apiKey: "k", model: "x/y", baseUrl });

  test("a rejected key", async () => {
    expect(await errorOf(tui(`${serve(401)}/v1`).make())).toStartWith("OpenRouter rejected the API key. Run /setup to enter a new one.");
    const message = await errorOf(sdk(`${serve(401)}/v1`).make());
    expect(message).toStartWith("OpenRouter rejected the API key. Pass another provider.apiKey to createSession() or configure().");
    expect(message).not.toContain("/setup");
  });

  test("an unknown model, on OpenRouter and on Ollama", async () => {
    expect(await errorOf(tui(`${serve(404)}/v1`).make())).toStartWith('Model "x/y" wasn\'t found on OpenRouter. Pick another with /model.');
    expect(await errorOf(sdk(`${serve(404)}/v1`).make())).toStartWith(
      'Model "x/y" wasn\'t found on OpenRouter. Pass another provider.model to createSession() or configure().',
    );
    const message = await errorOf(providerFactory({ kind: "ollama", model: "nope:1b", host: serve(404) }).make());
    expect(message).toStartWith('Model "nope:1b" wasn\'t found on Ollama. Pass another provider.model to createSession() or configure().');
  });
});
