import { load, save } from "./store.js";

/** Adds `by` (default 1) to the named counter and returns its new value. */
export async function increment(path, name, by = 1) {
  const counters = await load(path);
  counters[name] = (counters[name] ?? 0) + by;
  await save(path, counters);
  return counters[name];
}
