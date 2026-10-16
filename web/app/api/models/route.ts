import { DEFAULT_MODEL } from "@/lib/marv";

export interface ModelInfo {
  id: string;
  name: string;
  context: number;
  /** Dollars per million tokens. */
  input: number;
  output: number;
}

let cache: { at: number; models: ModelInfo[] } | undefined;
const HOUR = 60 * 60 * 1000;

/** OpenRouter's models that can call tools (Marv needs them), cheapest first. */
export async function GET() {
  if (!cache || Date.now() - cache.at > HOUR) {
    try {
      const response = await fetch("https://openrouter.ai/api/v1/models");
      const { data } = await response.json();
      const models: ModelInfo[] = (data ?? [])
        .filter((m: { supported_parameters?: string[] }) => m.supported_parameters?.includes("tools"))
        .map((m: { id: string; name: string; context_length: number; pricing: { prompt: string; completion: string } }) => ({
          id: m.id,
          name: m.name,
          context: m.context_length,
          input: Number(m.pricing.prompt) * 1e6,
          output: Number(m.pricing.completion) * 1e6,
        }))
        // Routers price per request routed (-1 here): no fixed price to show.
        .filter((m: ModelInfo) => m.input >= 0 && m.output >= 0)
        .sort((a: ModelInfo, b: ModelInfo) => a.input - b.input);
      cache = { at: Date.now(), models };
    } catch {
      // The picker still takes a typed id.
      return Response.json({ default: DEFAULT_MODEL, models: cache?.models ?? [] });
    }
  }
  return Response.json({ default: DEFAULT_MODEL, models: cache.models });
}
