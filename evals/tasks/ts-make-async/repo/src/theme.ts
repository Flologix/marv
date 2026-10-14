import { loadSettings, type Settings } from "./settings.ts";

export function theme(): Settings["theme"] {
  return loadSettings().theme;
}
