import { createServer } from "../src/server.js";

export function smoke() {
  const servers = [createServer(8080), createServer(8443, "example.test")];
  return servers.map((s) => s.url);
}
