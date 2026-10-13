import { load, save } from "./store.js";

/** A setting's value, or `fallback` when it isn't set. */
export function getSetting(path, key, fallback, cb) {
  load(path, (err, settings) => {
    if (err) return cb(err);
    cb(null, key in settings ? settings[key] : fallback);
  });
}

/** Sets a setting; returns the previous value (undefined when there was none). */
export function setSetting(path, key, value, cb) {
  load(path, (err, settings) => {
    if (err) return cb(err);
    const previous = settings[key];
    settings[key] = value;
    save(path, settings, (err2) => (err2 ? cb(err2) : cb(null, previous)));
  });
}
