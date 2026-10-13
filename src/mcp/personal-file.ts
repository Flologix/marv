// Adding and removing servers in your own ~/.marv/mcp.json (/mcp add, /mcp remove). The file is yours and may
// hold more than Marv wrote: every other key and server is kept as it was, and a file that can't be read is
// never overwritten (it's yours to fix). Never the project's .mcp.json: that one is shared with the repository.
import { existsSync } from "node:fs";
import { join } from "node:path";
import { withFileLock } from "../file-lock.ts";
import { writePrivate } from "../private-file.ts";
import { checkEntry, PERSONAL_FILE } from "./config.ts";

type McpFile = Record<string, unknown> & { mcpServers?: Record<string, unknown> };

export const personalFile = (configDir: string) => join(configDir, PERSONAL_FILE);

/**
 * One change at a time, across Marv processes too: the whole read-change-write holds `mcp.json.lock`
 * (src/file-lock.ts), so two terminals adding servers at once both land, and a remove can't undo an add.
 */
const locked = <T>(configDir: string, work: (path: string) => Promise<T>): Promise<T> => {
  const path = personalFile(configDir);
  return withFileLock(path, () => work(path));
};

/** The file as it is right now; missing is empty. Anything Marv can't safely change throws. */
async function read(path: string): Promise<McpFile> {
  if (!existsSync(path)) return {};
  let data: unknown;
  try {
    data = await Bun.file(path).json();
  } catch {
    throw new Error(`Couldn't read ${path} (it isn't valid JSON): fix it by hand first, Marv won't overwrite it.`);
  }
  const isObject = (v: unknown): v is Record<string, unknown> => typeof v === "object" && v !== null && !Array.isArray(v);
  if (!isObject(data) || (data.mcpServers !== undefined && !isObject(data.mcpServers))) {
    throw new Error(`Couldn't read ${path} (it should be {"mcpServers": {…}}): fix it by hand first, Marv won't overwrite it.`);
  }
  return data as McpFile;
}

const write = (path: string, file: McpFile) => writePrivate(path, `${JSON.stringify(file, null, 2)}\n`);

/**
 * Adds `entry` as server `name`, exactly as given (it's what the user approved). Refuses a name that's taken and
 * an entry Marv couldn't load. Written 0600: it may hold header references.
 */
export function addPersonalServer(configDir: string, name: string, entry: unknown): Promise<void> {
  return locked(configDir, async (path) => {
    checkEntry(name, entry);
    const file = await read(path);
    if (file.mcpServers && Object.hasOwn(file.mcpServers, name)) {
      throw new Error(`You already have a server named "${name}" in ${path}: pick another with --name, or /mcp remove ${name} first.`);
    }
    await write(path, { ...file, mcpServers: { ...file.mcpServers, [name]: entry } });
  });
}

/** Removes server `name`, keeping everything else. */
export function removePersonalServer(configDir: string, name: string): Promise<void> {
  return locked(configDir, async (path) => {
    const file = await read(path);
    if (!file.mcpServers || !Object.hasOwn(file.mcpServers, name)) throw new Error(`There's no server "${name}" in ${path}.`);
    const { [name]: _, ...rest } = file.mcpServers;
    await write(path, { ...file, mcpServers: rest });
  });
}

/** The names in your mcp.json, or none when it can't be read (for /mcp remove's check). */
export async function personalServerNames(configDir: string): Promise<string[]> {
  try {
    return Object.keys((await read(personalFile(configDir))).mcpServers ?? {});
  } catch {
    return [];
  }
}
