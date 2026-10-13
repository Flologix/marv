import { createServer } from "./server.js";

// The dev server always runs locally.
export const devServer = () => createServer({ port: 5173 });
