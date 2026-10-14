import { loadSettings } from "./settings.ts";

/** The message as a debug line, or null when verbose is off. */
export function debug(message: string): string | null {
  return loadSettings().verbose ? `[debug] ${message}` : null;
}
