// A tiny store: one JSON file per collection, read and written whole.
import { readFile, writeFile } from "node:fs/promises";

/** The file's data; a file that doesn't exist yet is an empty collection. */
export async function load(path) {
  let text;
  try {
    text = await readFile(path, "utf8");
  } catch (err) {
    if (err.code === "ENOENT") return {};
    throw err;
  }
  try {
    return JSON.parse(text);
  } catch (parseError) {
    throw new Error(`${path} isn't valid JSON: ${parseError.message}`);
  }
}

/** Writes the whole collection. */
export async function save(path, data) {
  await writeFile(path, `${JSON.stringify(data, null, 2)}\n`);
}

/** The old callback API, for the plugin that still uses it. */
export function loadCallback(path, cb) {
  load(path).then((data) => cb(null, data), cb);
}
