import { readSync } from "./store.ts";

export interface Settings {
  theme: "light" | "dark";
  verbose: boolean;
}

const DEFAULTS: Settings = { theme: "light", verbose: false };

export function loadSettings(): Settings {
  const raw = readSync("settings");
  return { ...DEFAULTS, ...(raw ? (JSON.parse(raw) as Partial<Settings>) : {}) };
}
