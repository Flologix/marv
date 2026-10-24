import { createServer } from "../src/server.js";

export function smoke() {
  const servers = [createServer({ port: 8080 }), createServer({ port: 8443, host: "example.test" })];
  return servers.map((s) => s.url);
}
