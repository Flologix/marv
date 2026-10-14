import { loadSettings } from "./settings.ts";

/** The names of all settings, sorted. */
export async function settingNames(): Promise<string[]> {
  return Object.keys(await loadSettings()).sort();
}
