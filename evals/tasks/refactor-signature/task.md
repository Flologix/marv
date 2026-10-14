Change createServer in src/server.js to take one options object, createServer({ port, host }), with host still defaulting to "localhost", and update every caller.
