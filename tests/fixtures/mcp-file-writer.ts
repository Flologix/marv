// A second Marv process for tests/mcp-personal-file.test.ts: `bun mcp-file-writer.ts <config dir> <prefix> <count>`
// adds <count> servers of its own to the shared ~/.marv/mcp.json, one after the other, then exits.
import { addPersonalServer } from "../../src/mcp/personal-file.ts";

const [dir, prefix, count] = process.argv.slice(2);
for (let i = 0; i < Number(count); i++) await addPersonalServer(dir!, `${prefix}${i}`, { command: "npx", args: [`${prefix}${i}`] });
