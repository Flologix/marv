// Adding and removing servers in your own ~/.marv/mcp.json (/mcp add, /mcp remove). The file is yours and may
// hold more than Marv wrote: every other key and server is kept as it was, and a file that can't be read is
// never overwritten (it's yours to fix). Never the project's .mcp.json: that one is shared with the repository.
import { existsSync } from "node:fs";
import { join } from "node:path";
import { writePrivate } from "../private-file.ts";
import { checkEntry, PERSONAL_FILE } from "./config.ts";

type McpFile = Record<string, unknown> & { mcpServers?: Record<string, unknown> };

/** One change at a time: two adds right after each other must both land (each re-reads the file). */
let queue: Promise<unknown> = Promise.resolve();
const serial = <T>(work: () => Promise<T>): Promise<T> => {
  const next = queue.catch(() => {}).then(work);
  queue = next;
  return next;
};

export const personalFile = (configDir: string) => join(configDir, PERSONAL_FILE);

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
  return serial(async () => {
    checkEntry(name, entry);
    const path = personalFile(configDir);
    const file = await read(path);
    if (file.mcpServers && Object.hasOwn(file.mcpServers, name)) {
      throw new Error(`You already have a server named "${name}" in ${path}: pick another with --name, or /mcp remove ${name} first.`);
    }
    await write(path, { ...file, mcpServers: { ...file.mcpServers, [name]: entry } });
  });
}

/** Removes server `name`, keeping everything else. */
export function removePersonalServer(configDir: string, name: string): Promise<void> {
  return serial(async () => {
    const path = personalFile(configDir);
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
