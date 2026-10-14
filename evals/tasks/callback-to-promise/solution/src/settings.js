import { load, save } from "./store.js";

/** A setting's value, or `fallback` when it isn't set. */
export async function getSetting(path, key, fallback) {
  const settings = await load(path);
  return key in settings ? settings[key] : fallback;
}

/** Sets a setting; returns the previous value (undefined when there was none). */
export async function setSetting(path, key, value) {
  const settings = await load(path);
  const previous = settings[key];
  settings[key] = value;
  await save(path, settings);
  return previous;
}
