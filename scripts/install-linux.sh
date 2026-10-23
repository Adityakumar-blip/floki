#!/bin/sh
# Installs Floki for the current user (~/.local). Run from a release download, or from the repo to build first.
set -e
cd "$(dirname "$0")"
[ -d assets ] || cd ..
if [ -x floki ]; then
    BIN=floki
else
    cargo build --release --locked
    BIN=target/release/floki
fi
install -Dm755 "$BIN" "$HOME/.local/bin/floki"
for size in 16 24 32 48 64 128 256 512; do
    install -Dm644 "assets/app-icon/floki-$size.png" "$HOME/.local/share/icons/hicolor/${size}x${size}/apps/floki.png"
done
install -Dm644 assets/floki.svg "$HOME/.local/share/icons/hicolor/scalable/apps/floki.svg"
install -Dm644 /dev/stdin "$HOME/.local/share/applications/floki.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=Floki
Comment=Native API client
Exec=$HOME/.local/bin/floki
Icon=floki
Terminal=false
Categories=Development;
StartupWMClass=floki
DESKTOP
gtk-update-icon-cache -q "$HOME/.local/share/icons/hicolor" 2>/dev/null || true
echo "Installed. Floki is now in your app launcher."
