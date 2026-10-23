# Marv desktop

Marv in a window. You pick a **Marv folder** once (for example `~/Projects` or a new `~/Marv`). The left pane shows its tree, where you can make, rename and trash folders. A chat runs Marv on whichever folder is selected: Marv reads and changes everything below that folder, runs sandboxed commands and edits on its own, and asks you, in the window, before anything leaves the sandbox. Every chat belongs to a folder and picks up where it left off when you reopen it.

The look is Flexoki light (paper, thin lines) with the FloLogix AI typefaces: Jost, IBM Plex Sans and IBM Plex Mono. Marv the martian appears on an empty chat and bobs while it works. The FLAI mark, from `FlologixAI/flologixai-site/public/Logo.png` and cropped, sits in the sidebar and on the first-run screen.

## Commands and skills

Type `/` in the prompt for the menu (↑/↓, then Tab or Enter):
- **Commands Marv runs itself:** `/help`, `/skills`, `/compact`, `/cost`, `/memory` and `/model <id>`.
- **Your skills:** `/<skill> <request>` runs one. Skills come from `~/.marv/skills` and the folder's `.marv/skills`.

The terminal UI's other commands (`/undo`, `/mcp`, `/resume`…) aren't in the desktop yet.

## How it fits together

```
app/ (Vite + React)  ── Tauri commands + a channel ──▶  app/src-tauri (marv-desktop)
                                                          │  folder tree, chats, settings
                                                          ▼
                                                   marv_web::host::run_chat  ──▶  marv-session (Marv's engine)
                                                          ▲
browser (same app/)  ── WebSocket ──▶  crates/marv-web ───┘
```

- **One frontend, two backends.** The page in `app/` works in both:
  - Inside Tauri it calls the desktop's commands.
  - In a plain browser it talks to `marv-web` over a WebSocket.
- **One protocol.** A chat gets the same messages either way, and returns the same events: `send`, `interrupt`, `approval`, `configure` in; text, tool calls, approvals and usage out (see `crates/marv-web/src/protocol.rs`). That's the contract a hosted Marv, or a desktop linked to one, builds on.
- **One engine.** `marv_web::host::run_chat` drives the session for both. The desktop runs it in-process, with no server and no port.

### What the desktop keeps

| Where | What |
|---|---|
| `~/.config/ai.flologix.marv/settings.json` (0600) | The Marv folder, and the OpenRouter key if you pasted one |
| `~/.local/share/ai.flologix.marv/chats.json` | Each chat's folder, title and Marv session id |
| `~/.marv/` (the CLI's own) | Saved sessions and undo checkpoints, next to your memory, skills and MCP servers |
| The window's local storage | Transcripts as shown, and the model, step limit and yolo setting |

### Safety

- **The Marv folder can't be your home folder or anything above it.** Marv changes files below its folder without asking, so that would include your dotfiles.
- **Tree paths are checked against the root.** Every path the page sends must be relative to the root: no absolute paths, no `..`, and no symlinks that lead out.
- **Deleting goes to the trash.** Nothing is removed outright.
- **Marv loads what the CLI loads.** On your own machine it gets your `~/.marv` skills, memory, MCP servers and instructions, and the folder's own. `marv-web` loads none of these, since it isn't your machine.
- **The key is found in this order:**
  1. the one you pasted;
  2. `OPENROUTER_API_KEY`;
  3. the one `marv` setup saved.

## Install it (Omarchy and other Linux desktops)

```sh
app/install-desktop.sh            # build, then install for you
app/install-desktop.sh --uninstall
```

It builds the release binary into `~/.local/bin/marv-desktop`, and adds an icon and a launcher entry. Open **Marv** from the app launcher (Omarchy: Super+Space), or run `marv-desktop`. Run it again after pulling changes to update.

**Opening a specific folder:** `marv-desktop <dir>`, for example `marv-desktop .` or `marv-desktop ~/Projects/site`.
- A folder inside your Marv folder is selected in the tree.
- Any other folder becomes the Marv folder.

`setsid -f marv-desktop .` gives your terminal back. In the app:
- click a folder to work there;
- double-click a folder to go into it, and **↑** (next to **+**) to come back out (like a file explorer);
- **⇄** picks any folder.

Your home folder and anything above it are refused.

## Develop

You need Rust, Node and bubblewrap. On Linux you also need WebKitGTK 4.1 (`webkit2gtk-4.1`).

```sh
cd app
npm install
npx tauri dev        # the app, with hot reload
npx tauri build      # .deb and .AppImage in target/release/bundle
npm test             # the transcript's tests (plain Node)
```

**Linux note:** WebKitGTK's DMA-BUF renderer crashes on some Wayland + NVIDIA setups ("Error 71 (Protocol error)"). Marv desktop sets `WEBKIT_DISABLE_DMABUF_RENDERER=1` for itself. To keep the renderer, set the variable to `0`.

**In a browser:**
1. Run `cargo run -p marv-web`.
2. In `app/`, run `npm run dev` and open http://127.0.0.1:1420.
3. Add that origin to `MARV_WEB_ORIGINS` (see `crates/marv-web`).

This mode is one chat in a fresh workspace on the server, with no folder tree.

## Next

- Accounts, and a hosted `marv-web` with a workspace per user in a container.
- Code moves between desktop and hosted through git, not file sync.
- Link a signed-in desktop so the hosted page can reach it.
