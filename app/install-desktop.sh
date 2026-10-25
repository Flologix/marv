#!/bin/sh
# Builds Marv desktop and installs it for this user: on Linux the binary in ~/.local/bin, its icon, and a launcher
# entry, so it shows up in the app launcher (Omarchy: Super+Space); on macOS Marv.app in Applications. Run it again to
# update; --uninstall removes what it installed.
set -eu
cd "$(dirname "$0")"
BIN="$HOME/.local/bin/marv-desktop"
ICON="$HOME/.local/share/icons/hicolor/128x128/apps/marv-desktop.png"
ENTRY="$HOME/.local/share/applications/marv-desktop.desktop"

# macOS has no launcher entry to write: the app bundle in Applications is it (Launchpad, Spotlight, the Dock). This
# builds Marv.app and puts it in /Applications (or ~/Applications when /Applications isn't writable). A build made
# here isn't quarantined, so Gatekeeper doesn't ask, unlike the downloaded .dmg.
if [ "$(uname -s)" = Darwin ]; then
  if [ -w /Applications ]; then APPS=/Applications; else APPS="$HOME/Applications"; fi
  if [ "${1:-}" = "--uninstall" ]; then
    # Wherever an earlier run put it.
    for dir in /Applications "$HOME/Applications"; do
      if [ -d "${dir:?}/Marv.app" ]; then
        rm -rf "${dir:?}/Marv.app"
        echo "Removed Marv desktop from $dir."
      fi
    done
    exit 0
  fi
  npm install --silent
  npx tauri build --bundles app
  mkdir -p "$APPS"
  rm -rf "${APPS:?}/Marv.app"
  ditto ../target/release/bundle/macos/Marv.app "$APPS/Marv.app"
  echo "Installed Marv desktop: $APPS/Marv.app. Open it from Launchpad or Spotlight (\"Marv\")."
  exit 0
fi

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
