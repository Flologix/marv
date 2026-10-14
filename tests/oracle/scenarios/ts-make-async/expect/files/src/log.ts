import { loadSettings } from "./settings.ts";

/** The message as a debug line, or null when verbose is off. */
export async function debug(message: string): Promise<string | null> {
  return (await loadSettings()).verbose ? `[debug] ${message}` : null;
}
