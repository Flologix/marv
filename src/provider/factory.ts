// How a session gets its providers. It needs more than one Provider object: a subagent type can name another
// model, and on OpenRouter the provider is remade once the model list says whether the model's reasoning can be
// turned off (/think). A factory makes them, and says what to record in saved sessions and trajectories.
import { resolveConfig, type Config } from "../config/config.ts";
import { SDK_HINTS } from "./errors.ts";
import { createProvider } from "./index.ts";
import { listModels, type ModelInfo } from "./models.ts";
import type { Provider } from "./types.ts";

/**
 * Thinking (/think) is part of the factory: the TUI makes a new factory when it changes, and `configure({ thinking })`
 * only rebuilds from a ProviderOption.
 */
export interface ProviderFactory {
  /** Recorded with sessions and trajectories: "openrouter", "ollama", or "custom". */
  id: string;
  model: string;
  /** Runs on this machine (Ollama): no cost. */
  local?: boolean;
  /** The session's provider; `model` for a subagent type that names another one; `info` once the model list was read. */
  make(model?: string, info?: Pick<ModelInfo, "reasoning">): Provider;
  /** What the provider's model list says about the model (context window, prices, reasoning), when it has a list. */
  lookup?(): Promise<ModelInfo | undefined>;
}

/**
 * What the SDK takes: a built-in provider by name, or any Provider of your own. A Provider of your own is used for
 * everything, subagents included (a subagent type naming another model gets the same provider), and what's recorded
 * as the model in sessions and trajectories is its `name`.
 */
export type ProviderOption =
  | { kind: "openrouter"; apiKey: string; model?: string; baseUrl?: string }
  | { kind: "ollama"; model: string; host?: string; contextLength?: number }
  | Provider;

// A factory has `make`; a Provider has `stream`.
export const isFactory = (value: ProviderFactory | ProviderOption): value is ProviderFactory =>
  typeof (value as ProviderFactory).make === "function";

/** The TUI's way: from its resolved Config, with the provider maker and model list it was given (tests swap both). */
export function configFactory(
  config: Config,
  make: (config: Config, info?: Pick<ModelInfo, "reasoning">) => Provider = createProvider,
  load: (config: Pick<Config, "provider" | "baseUrl">) => Promise<ModelInfo[]> = listModels,
): ProviderFactory {
  return {
    id: config.provider,
    model: config.model,
    local: config.provider === "ollama",
    make: (model, info) => make(model ? { ...config, model } : config, info),
    // OpenRouter's list has every model's context window, prices and reasoning; Ollama's knows none of that.
    lookup: config.provider === "openrouter" ? () => load(config).then((models) => models.find((m) => m.id === config.model)) : undefined,
  };
}

/** The SDK's way. Environment variables (OPENROUTER_API_KEY, MARV_MODEL…) are ignored: a library uses what it's given. */
export function providerFactory(option: ProviderOption, thinking = false): ProviderFactory {
  // A Provider has `stream` (a built-in option never does).
  if ("stream" in option) return { id: "custom", model: option.name, make: () => option };
  // Check both kinds by name: a typo (or "OpenRouter" from plain JS) must not quietly become Ollama on localhost.
  if (option.kind !== "openrouter" && option.kind !== "ollama") {
    throw new Error(`Unknown provider kind "${(option as { kind?: unknown }).kind}": use "openrouter" or "ollama", or pass a Provider.`);
  }
  const file =
    option.kind === "openrouter"
      ? { provider: "openrouter" as const, apiKey: option.apiKey, model: option.model, baseUrl: option.baseUrl }
      : { provider: "ollama" as const, model: option.model, contextLength: option.contextLength };
  const env = option.kind === "ollama" && option.host ? { OLLAMA_HOST: option.host } : {};
  // Errors name the option to change: an SDK user has no /setup or /model.
  return configFactory({ ...resolveConfig(file, env), thinking }, (config, info) => createProvider(config, info, SDK_HINTS));
}
