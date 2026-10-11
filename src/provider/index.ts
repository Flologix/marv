import { PRESETS, type Config } from "../config/config.ts";
import type { ErrorHints } from "./errors.ts";
import type { ModelInfo } from "./models.ts";
import { OllamaProvider } from "./ollama.ts";
import { OpenAICompatProvider } from "./openai-compat.ts";
import type { Provider } from "./types.ts";

/**
 * /think on OpenRouter: its unified `reasoning` switch. "Off" goes only to models whose reasoning is optional:
 * for one that always reasons, OpenRouter rejects the request (and thinking is off by default, so that would be
 * every request). For a model Marv knows nothing about yet (the model list hasn't loaded), nothing is sent.
 */
function reasoningFor(thinking: boolean, reasoning: ModelInfo["reasoning"]) {
  if (!reasoning) return undefined;
  if (thinking) return { enabled: true };
  return reasoning === "optional" ? { enabled: false } : undefined;
}

// The one place that maps a config to a concrete Provider. `model`: what the model list says about it. `hints`: what
// a bad key or an unknown model tells the user to change (the TUI's commands by default; the SDK passes its options).
export function createProvider(config: Config, model?: Pick<ModelInfo, "reasoning">, hints?: ErrorHints): Provider {
  switch (config.provider) {
    case "openrouter": {
      const reasoning = reasoningFor(config.thinking, model?.reasoning);
      return new OpenAICompatProvider({
        name: `openrouter · ${config.model}`,
        label: PRESETS.openrouter.label,
        baseUrl: config.baseUrl,
        model: config.model,
        apiKey: config.apiKey,
        // Optional attribution: shows "Marv" in your OpenRouter activity log.
        headers: { "X-Title": "Marv" },
        hints,
        ...(reasoning ? { body: { reasoning } } : {}),
      });
    }
    case "ollama":
      // Ollama's native API, so we can set the context window (its default is 4096 tokens).
      return new OllamaProvider({
        baseUrl: config.baseUrl,
        model: config.model,
        contextLength: config.contextLength,
        thinking: config.thinking,
        hints,
      });
  }
}
