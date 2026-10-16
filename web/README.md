# Marv on the web

A browser UI for Marv:
- a prompt box;
- the reply as it streams;
- every tool call, which expands to its output or diff;
- **live approvals**: when Marv wants to do something the sandbox can't contain (a git commit, network access, a protected file), the page shows the exact command or diff and waits for **Allow**, **Allow … this session** or **Decline**.

The page is styled with the FloLogix AI design system v1.0: Jost, IBM Plex Sans and Mono, Flow Cyan and the Signal Yellow tilt, in light and dark.

## How it works

```
browser ──WebSocket──▶ marv-web (Rust, crates/marv-web) ──▶ marv-session (the same engine as the marv CLI)
   ▲                                                          │
   └────────── events, approval requests ◀───────────────────┘
Next.js (this folder) serves the page and /api/models
```

- **One WebSocket is one chat.** The page sends `hello` (the chat id, model, step limit, yolo). `marv-web` then builds a session with `SessionBuilder` in that chat's own workspace (`<data>/workspaces/<chat id>`, a fresh git repo) and keeps it open between messages. A reconnect resumes the chat's saved session.
- **Approvals.** `marv-web` gives the session an approver that sends each request to the page and waits for the answer:
  - "Always" is remembered for the session.
  - Requests Marv says a person must answer every time (tainted egress, protected paths) get Allow and Decline only.
  - Stop, or closing the tab, answers whatever is still open with no.
- **Isolation.**
  - The session loads no sources: none of the server user's skills, memory, MCP servers or instructions.
  - Marv's own files (sessions, checkpoints) live in `<data>/marv`.
  - The `process` tool is off, since its ports would open on the server, not the viewer's machine.
- **Origin check.** WebSockets are accepted only from the origins in `MARV_WEB_ORIGINS`. Without it, any page the user visits could drive their agent.

### The protocol

| Page → server | |
|---|---|
| `{type:"hello", chatId, model, maxSteps, yolo}` | first message |
| `{type:"send", text}` | a user message (one turn at a time) |
| `{type:"interrupt"}` | stop the turn |
| `{type:"approval", id, decision:"yes"\|"always"\|"no"}` | answer a request |
| `{type:"configure", model?, maxSteps?, yolo?}` | from the next turn |

**Server → page:**
- `ready` and `fatal`;
- `turn_start`, `text_delta`, `thinking_delta` and `assistant`;
- `tool_start` and `tool_end` (with `id`, `result`, `summary`, `diff` and `approval`), `tool_skipped`;
- `approval` (with `title`, `command`, `diff`, `note`, `warning`, `scope` and `once`), `approval_done`;
- `status`, `compaction`, `tainted`, `notice`, `error` and `turn_end`;
- `usage` (this turn's tokens and cost, plus the context used).

## Run it

You need Rust (see the repo README), Node, bubblewrap and git.

```sh
# 1. The server (the key: OPENROUTER_API_KEY, else the one marv's setup saved)
cargo run -p marv-web

# 2. The page
cd web && npm install && npm run dev -- -H 127.0.0.1
```

Open http://127.0.0.1:3000. Using another port? Add its origin to `MARV_WEB_ORIGINS`.

| Server env var | Default |
|---|---|
| `MARV_WEB_ADDR` | `127.0.0.1:8787` |
| `MARV_WEB_DATA` | `.marv-web` |
| `MARV_WEB_ORIGINS` | `http://localhost:3000,http://127.0.0.1:3000` |
| `MARV_WEB_MODEL` | `z-ai/glm-5.3-flash` (also read by the page's `/api/models`) |
| `OPENROUTER_API_KEY` | the key in `~/.marv/config.json` |

The page connects to `ws://<its host>:8787/ws`; set `NEXT_PUBLIC_MARV_WS` to point it elsewhere.

## Not hosted yet

**Keep it on 127.0.0.1.** There's no login, so anyone who reaches the page runs an agent on your key and your machine. To host it, it still needs:
- accounts, with the WebSocket authenticated (a session cookie checked on upgrade);
- a key or budget per user;
- workspaces in a container or VM per user, not folders on the host (bubblewrap protects the host from a command, not from a stranger who writes the prompts);
- a cap on sessions;
- workspace cleanup;
- a file browser and download.
