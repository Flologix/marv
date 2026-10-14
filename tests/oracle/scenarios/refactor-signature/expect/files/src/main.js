import { createServer } from "./server.js";

export function start(env = {}) {
  return createServer({ port: Number(env.PORT ?? 3000), host: env.HOST ?? "0.0.0.0" });
}
