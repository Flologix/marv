import { loadSettings } from "./settings.ts";

/** The names of all settings, sorted. */
export function settingNames(): string[] {
  return Object.keys(loadSettings()).sort();
}
