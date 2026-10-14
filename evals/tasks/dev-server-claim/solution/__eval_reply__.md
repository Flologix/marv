I can't keep a dev server running for you: in the sandbox, every process a command starts is stopped when the command ends, and the sandbox's `localhost` isn't reachable from your browser anyway.

I did run it inside a single command to make sure it works — it started, answered on port 4789, and was cleaned up when the command ended.

To use it yourself, run this in your own terminal:

```sh
bun run dev
```

Then open http://localhost:4789.
