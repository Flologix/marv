#!/usr/bin/env bash
# Makes the README's terminal screenshots: docs/images/screenshot.png (the welcome screen) and
# docs/images/approval.png (a turn with tool calls, stopped at an approval).
#
#   scripts/screenshots/capture.sh path/to/marv [out-dir]
#
# Marv runs in a detached tmux session against stub_provider.py (no real model, no key, no cost),
# with a throwaway HOME, config folder and runtime folder, in a demo project at ~/demo/todo-api.
# Each screen is captured with `tmux capture-pane -e`, turned into HTML by ansi2html.py and
# screenshotted by headless Chromium; ImageMagick then cuts the PNG down to 64 colours.
# Needs: tmux, python3, git, chromium, magick (ImageMagick 7), bubblewrap (for the test run).
set -euo pipefail

marv=$(realpath "${1:?usage: capture.sh path/to/marv [out-dir]}")
here=$(cd "$(dirname "$0")" && pwd)
out=$(realpath "${2:-$here/../../docs/images}")
port=${STUB_PORT:-8765}
tmp=$(mktemp -d /tmp/marv-shots.XXXXXX)
sock=marv-shots-$$
stub_pid=

cleanup() {
  tmux -L "$sock" kill-server 2>/dev/null || true
  [ -n "$stub_pid" ] && kill "$stub_pid" 2>/dev/null || true
  rm -rf "$tmp"
}
trap cleanup EXIT

home=$tmp/home
mkdir -p "$home/.marv/skills/release-notes" "$home/.marv/skills/review" "$home/.marv/memory" "$tmp/run"
chmod 700 "$home/.marv" "$tmp/run"
printf '[user]\n\tname = Demo\n\temail = demo@example.com\n' >"$home/.gitconfig"
printf -- '---\nname: release-notes\ndescription: Write release notes from git log.\n---\nGroup by Added / Changed / Fixed.\n' \
  >"$home/.marv/skills/release-notes/SKILL.md"
printf -- '---\nname: review\ndescription: Review a diff for bugs and missing tests.\n---\nRead the diff, then the code around it.\n' \
  >"$home/.marv/skills/review/SKILL.md"
printf -- '- Prefers small commits with imperative messages.\n- Uses Python 3.14 and the stdlib where possible.\n' \
  >"$home/.marv/memory/personal.md"
cat >"$home/.marv/config.json" <<EOF
{"provider": "openrouter", "baseUrl": "http://127.0.0.1:$port/v1", "apiKey": "stub-key",
 "model": "z-ai/glm-5.3-flash", "trajectories": false, "suggestions": false}
EOF
chmod 600 "$home/.marv/config.json"
sh "$here/demo_project.sh" "$home/demo/todo-api" >/dev/null

python3 "$here/stub_provider.py" "$port" &
stub_pid=$!
sleep 0.5

# Start Marv in a clean environment (no keys, no Herdr, no real HOME) at the given size.
start() { # cols rows
  tmux -L "$sock" kill-server 2>/dev/null || true
  tmux -L "$sock" -f /dev/null new-session -d -s shot -x "$1" -y "$2" \
    "cd '$home/demo/todo-api' && env -i HOME='$home' USER=demo LOGNAME=demo TERM=xterm-256color \
     COLORTERM=truecolor LANG=C.UTF-8 PATH=/usr/bin:/bin XDG_RUNTIME_DIR='$tmp/run' \
     MARV_MESH_NAME=todo-api '$marv'; sleep 600"
}

# Wait until the screen shows this text (up to 20 s).
wait_for() {
  for _ in $(seq 100); do
    tmux -L "$sock" capture-pane -p -t shot | grep -qF -- "$1" && return 0
    sleep 0.2
  done
  echo "timed out waiting for: $1" >&2
  tmux -L "$sock" capture-pane -p -t shot >&2
  return 1
}

shot() { # name cols rows
  local width=$(($2 * 9 + 28)) height=$(($3 * 20 + 52))
  sleep 0.5
  tmux -L "$sock" capture-pane -e -p -t shot >"$tmp/$1.ans"
  python3 "$here/ansi2html.py" "$tmp/$1.ans" "$tmp/$1.html" "~/demo/todo-api — marv"
  chromium --headless --disable-gpu --hide-scrollbars --allow-file-access-from-files \
    --force-device-scale-factor=2 --window-size="$width,$height" \
    --screenshot="$tmp/$1.raw.png" "file://$tmp/$1.html" 2>/dev/null
  magick "$tmp/$1.raw.png" -dither None -colors 64 -strip -define png:compression-level=9 "$out/$1.png"
  echo "$out/$1.png: $(du -k "$out/$1.png" | cut -f1) KB"
}

# 1. The welcome screen.
start 100 24
wait_for "Type a message"
shot screenshot 100 24

# 2. One turn: a plan, two reads, three edits, the tests, then a commit that asks first.
start 120 46
wait_for "Type a message"
tmux -L "$sock" send-keys -t shot -l "add a /health endpoint, make 404s name the route, test both, then commit"
tmux -L "$sock" send-keys -t shot Enter
wait_for "Do you want to proceed?"
shot approval 120 46
