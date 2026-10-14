import { load, save } from "./store.js";

/** Adds `by` (default 1) to the named counter and returns its new value. */
export function increment(path, name, by, cb) {
  if (typeof by === "function") {
    cb = by;
    by = 1;
  }
  load(path, (err, counters) => {
    if (err) return cb(err);
    counters[name] = (counters[name] ?? 0) + by;
    save(path, counters, (err2) => (err2 ? cb(err2) : cb(null, counters[name])));
  });
}
