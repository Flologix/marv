# README screenshots

How `docs/images/screenshot.png`, `approval.png` and `desktop.png` are made, so they can be made again for each release. Everything runs headless (no window opens), against a stub model: no API key, no cost, and no real paths (the demo `HOME` is a temp folder, so the project shows as `~/demo/todo-api`).

Needs `tmux`, `python3`, `git`, `chromium` (or set `CHROMIUM` to another Chromium-based browser, e.g. `CHROMIUM=google-chrome`), ImageMagick 7 (`magick`), bubblewrap, and app/'s `node_modules` (`cd app && npm ci`).

```sh
cargo build -p marv-cli
scripts/screenshots/capture.sh target/debug/marv     # screenshot.png and approval.png
scripts/screenshots/capture-desktop.sh                # desktop.png
```

The welcome screen shows the version the binary was built with: bump `version` in `Cargo.toml` first (or build from the release tag).

- **`stub_provider.py`** is an OpenAI-compatible server on `127.0.0.1` that streams a scripted turn: a pinned plan, two reads, three edits (one replacing a line), the tests, then a `git commit`, which asks. It picks each reply by the step of the turn, so side requests can't shift it.
- **`demo_project.sh`** writes the demo project, a tiny todo API in Python with stdlib tests.
- **`capture.sh`** runs Marv in a detached tmux session (100x24 for the welcome screen, 120x46 for the turn) with a clean environment and a throwaway `HOME`, config and runtime folder, captures the pane with `tmux capture-pane -e`, renders it with **`ansi2html.py`** (ANSI colors to HTML, box lines drawn in CSS, JetBrains Mono if installed) and screenshots that with Chromium at 2x; ImageMagick cuts it to 64 colors.
- **`capture-desktop.sh`** builds `app/` with Vite and serves it with **`desktop_mock.js`** injected, a stand-in for the Tauri backend with demo folders and chats, then screenshots it at 1280x960 (2x, scaled to 1.5x).

To change what the turn shows, edit `STEPS` in `stub_provider.py` (and the demo project to match); for the desktop, the transcript in `desktop_mock.js`.
