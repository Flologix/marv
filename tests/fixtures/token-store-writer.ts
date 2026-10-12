// A second Marv process for tests/mcp-oauth-store.test.ts: `bun token-store-writer.ts <store path> <prefix> <count>`
// updates <count> keys of its own in the shared token store, one after the other, then exits.
import { McpTokenStore } from "../../src/mcp/oauth-store.ts";

const [path, prefix, count] = process.argv.slice(2);
const store = new McpTokenStore(path!);
for (let i = 0; i < Number(count); i++) {
  await store.update(`${prefix} https://x.test/${i}`, (e) => ({ ...e, server: `${prefix}${i}`, savedAt: i }));
}
