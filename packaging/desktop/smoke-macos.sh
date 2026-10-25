#!/bin/sh
# Launches a built Marv.app's binary and waits for it to say it's up (#208): the window opened, the bundled page ran
# and asked the backend for the launch folder (`MARV_DESKTOP_SMOKE_TEST=1`, see take_launch in app/src-tauri/src/lib.rs).
# Fails if it doesn't say so within the time limit, or exits without it.
#
#   packaging/desktop/smoke-macos.sh path/to/Marv.app
set -eu
app=$1
limit=${MARV_DESKTOP_SMOKE_SECONDS:-120}
bin=$(find "$app/Contents/MacOS" -type f -perm -u+x | head -n 1)
[ -n "$bin" ] || { echo "no binary in $app/Contents/MacOS"; exit 1; }
work=$(mktemp -d "${TMPDIR:-/tmp}/marv-desktop-smoke.XXXXXX")
trap 'rm -rf "${work:?}"' EXIT
mkdir -p "$work/home" "$work/folder"
log="$work/log"

# A HOME of its own, so the app's settings and chats land in the temp folder; the launch folder is outside it (the
# Marv folder can't be HOME or above it).
HOME="$work/home" MARV_DESKTOP_SMOKE_TEST=1 "$bin" "$work/folder" >"$log" 2>&1 &
pid=$!
waited=0
while kill -0 "$pid" 2>/dev/null && [ "$waited" -lt "$limit" ]; do
  grep -q '^marv-desktop: ready' "$log" && break
  sleep 1
  waited=$((waited + 1))
done
if kill -0 "$pid" 2>/dev/null; then
  # Asked to quit after printing: give it a moment, then stop it.
  sleep 5
  kill "$pid" 2>/dev/null || true
fi
wait "$pid" 2>/dev/null || true
echo "== $bin output"
cat "$log"
grep -q '^marv-desktop: ready' "$log" || { echo "Marv desktop didn't reach ready within ${limit}s"; exit 1; }
echo "Marv desktop reached ready in ${waited}s"
