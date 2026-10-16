#!/bin/sh
# Builds Marv desktop and installs it for this user: the binary in ~/.local/bin, its icon, and a launcher entry, so
# it shows up in the app launcher (Omarchy: Super+Space). Run it again to update; --uninstall removes all three.
set -eu
cd "$(dirname "$0")"
BIN="$HOME/.local/bin/marv-desktop"
ICON="$HOME/.local/share/icons/hicolor/128x128/apps/marv-desktop.png"
ENTRY="$HOME/.local/share/applications/marv-desktop.desktop"

if [ "${1:-}" = "--uninstall" ]; then
  rm -f "$BIN" "$ICON" "$ENTRY"
  echo "Removed Marv desktop."
  exit 0
fi

npm install --silent
# The frontend, then the binary with it built in (no .deb or AppImage here: just the app).
npx tauri build --no-bundle
mkdir -p "$(dirname "$BIN")" "$(dirname "$ICON")" "$(dirname "$ENTRY")"
install -m 755 ../target/release/marv-desktop "$BIN"
install -m 644 src-tauri/icons/128x128.png "$ICON"
cat > "$ENTRY" <<DESKTOP
[Desktop Entry]
Type=Application
Name=Marv
Comment=Marv, the coding agent, on your folders
Exec=$BIN
Icon=marv-desktop
Terminal=false
Categories=Development;
StartupWMClass=marv-desktop
DESKTOP
update-desktop-database "$(dirname "$ENTRY")" 2>/dev/null || true
echo "Installed Marv desktop: $BIN. Open it from the app launcher (\"Marv\")."
