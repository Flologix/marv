// A tiny store: one JSON file per collection, read and written whole.
import { readFile, writeFile } from "node:fs";

/** The file's data; a file that doesn't exist yet is an empty collection. */
export function load(path, cb) {
  readFile(path, "utf8", (err, text) => {
    if (err) return err.code === "ENOENT" ? cb(null, {}) : cb(err);
    let data;
    try {
      data = JSON.parse(text);
    } catch (parseError) {
      return cb(new Error(`${path} isn't valid JSON: ${parseError.message}`));
    }
    cb(null, data);
  });
}

/** Writes the whole collection. */
export function save(path, data, cb) {
  writeFile(path, `${JSON.stringify(data, null, 2)}\n`, cb);
}
