import { read } from "./store.ts";

export interface Settings {
  theme: "light" | "dark";
  verbose: boolean;
}

const DEFAULTS: Settings = { theme: "light", verbose: false };

export async function loadSettings(): Promise<Settings> {
  const raw = await read("settings");
  return { ...DEFAULTS, ...(raw ? (JSON.parse(raw) as Partial<Settings>) : {}) };
}
