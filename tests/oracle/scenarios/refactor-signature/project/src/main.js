import { createServer } from "./server.js";

export function start(env = {}) {
  return createServer(Number(env.PORT ?? 3000), env.HOST ?? "0.0.0.0");
}
