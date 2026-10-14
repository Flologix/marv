// A pretend server: just enough to show where it would listen.
export function createServer(port, host = "localhost") {
  if (!Number.isInteger(port)) throw new TypeError("port must be an integer");
  return { port, host, url: `http://${host}:${port}` };
}
