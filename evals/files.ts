// Putting files into an eval workspace after a model has worked in it. The model could have left a symlink where a
// file goes (`__eval__.test.js` → a file of yours), and writing or copying onto it would write through the link,
// outside the workspace. So whatever is there is removed first (rm removes a link, not what it points at), and the
// write refuses to follow anything that appears in between.
import { cpSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";

/** Writes `data` to `path`, replacing whatever is there (a symlink included) rather than writing through it. */
export function writeFresh(path: string, data: string | Uint8Array): void {
  rmSync(path, { recursive: true, force: true });
  writeFileSync(path, data, { flag: "wx" });
}

/** Copies the contents of folder `from` into `dir`, each top-level entry replacing whatever the workspace has there. */
export function copyFresh(from: string, dir: string): void {
  for (const entry of readdirSync(from)) {
    rmSync(join(dir, entry), { recursive: true, force: true });
    cpSync(join(from, entry), join(dir, entry), { recursive: true, errorOnExist: true, force: false });
  }
}
