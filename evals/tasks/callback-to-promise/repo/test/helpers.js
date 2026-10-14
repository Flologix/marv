import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

export const tempDir = () => mkdtempSync(join(tmpdir(), "tiny-store-"));
