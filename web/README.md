# Marv on the web

A minimal browser UI for Marv: a text box, the model's reply as it streams, each tool call (expand it for its output), and settings for the model, the step limit and what Marv may do without asking. It's a first step toward a hosted Marv; for now it's for showing people how the harness works.

## How it works

There is no second agent here. Each message is one `marv -p - --output stream-json` run, and the server streams Marv's JSON events to the page:

- **One workspace per chat.** A chat runs in its own folder (`.data/workspaces/<chat id>`, a fresh git repo). Later messages pass `-c`, so Marv continues that chat's saved session.
- **Its own HOME.** Marv runs with `HOME=.data/home`, so it never sees the server user's `~/.marv` (their skills, memory, MCP servers, sessions) or `gh` login. The child gets only `PATH`, `HOME`, `LANG`, `TERM`, `NODE_ENV` and the OpenRouter key, never the rest of the server's environment.
- **Approvals.** There's no one to click "yes", so Marv's headless rules apply: sandboxed commands and edits in the workspace run, anything else is refused unless a scope is ticked in settings (`--approve web`, `network`, `git_write`, `process`, `files`, `memory`). `github`, mesh receptacles and the tainted/protected opt-ins aren't offered.
- **Stop** aborts the request, and the server interrupts Marv (SIGINT, then SIGKILL after 3 s).

| File | What it does |
|---|---|
| `lib/marv.ts` | Spawns Marv for a turn and turns its stdout into the response stream |
| `app/api/chat/route.ts` | `POST {chatId, prompt, model, approve, maxSteps, resume}` → NDJSON events |
| `app/api/models/route.ts` | OpenRouter's tool-capable models with prices (cached for an hour) |
| `app/chat.tsx` | The page |

## Run it

Needs `marv` on PATH (or `MARV_BIN`), bubblewrap, git and Node.

```sh
cd web
npm install
OPENROUTER_API_KEY=sk-or-… npm run dev -- -H 127.0.0.1
```

Without `OPENROUTER_API_KEY`, the server uses the key saved in your own `~/.marv/config.json` (handy locally).

| Env var | Default |
|---|---|
| `OPENROUTER_API_KEY` | the key in `~/.marv/config.json` |
| `MARV_BIN` | `marv` |
| `MARV_WEB_MODEL` | `z-ai/glm-5.3-flash` (the picker's default) |
| `MARV_WEB_DATA` | `./.data` |

## Not hosted yet

**Keep it on 127.0.0.1.** There's no login, so anyone who can reach the page runs an agent on your key and your machine. Before it goes on the internet it needs:
- accounts and a per-user key or budget;
- workspaces in a container or VM per user rather than folders on the host (Marv's bwrap sandbox protects the host from a command, not from a stranger who controls the prompt);
- a cap on concurrent runs;
- workspace cleanup;
- a way to see and download a chat's files.
