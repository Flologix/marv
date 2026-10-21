#!/usr/bin/env bash
# Makes docs/images/desktop.png: Marv desktop's page (app/) in headless Chromium, with desktop_mock.js standing in
# for the Tauri backend (demo folders and chats, one approval waiting). Nothing runs behind it.
#
#   scripts/screenshots/capture-desktop.sh [out-dir]
#
# Needs: npm (app/'s node_modules installed: `cd app && npm ci`), python3, chromium, magick (ImageMagick 7).
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
app=$here/../../app
out=$(realpath "${1:-$here/../../docs/images}")
port=${SHOT_PORT:-8766}
tmp=$(mktemp -d /tmp/marv-desktop-shot.XXXXXX)
server_pid=
cleanup() {
  [ -n "$server_pid" ] && kill "$server_pid" 2>/dev/null || true
  rm -rf "$tmp"
}
trap cleanup EXIT

(cd "$app" && npx vite build --outDir "$tmp/dist" --emptyOutDir --logLevel error)
cp "$here/desktop_mock.js" "$tmp/dist/"
sed -i 's|<head>|<head><script src="/desktop_mock.js"></script>|' "$tmp/dist/index.html"
python3 -m http.server "$port" --bind 127.0.0.1 --directory "$tmp/dist" >/dev/null 2>&1 &
server_pid=$!
sleep 0.5

"${CHROMIUM:-chromium}" --headless --disable-gpu --hide-scrollbars --force-device-scale-factor=2 --window-size=1280,960 \
  --virtual-time-budget=4000 --screenshot="$tmp/desktop.raw.png" "http://127.0.0.1:$port/" 2>/dev/null
magick "$tmp/desktop.raw.png" -resize 75% -dither None -colors 128 -strip -define png:compression-level=9 "$out/desktop.png"
echo "$out/desktop.png: $(du -k "$out/desktop.png" | cut -f1) KB"
