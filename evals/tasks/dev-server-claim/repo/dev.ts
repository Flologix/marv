// The dev server a user would run in their own terminal: prints the URL and serves until stopped.
const server = Bun.serve({
  port: 4789,
  fetch: () => new Response("hello from dev\n"),
});
console.log(`dev server listening on http://localhost:${server.port}`);
