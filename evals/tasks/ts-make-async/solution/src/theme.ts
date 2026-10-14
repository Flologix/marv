import { loadSettings, type Settings } from "./settings.ts";

export async function theme(): Promise<Settings["theme"]> {
  return (await loadSettings()).theme;
}
