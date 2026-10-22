#!/bin/sh
# Builds Floki and installs it for the current user (~/.local).
set -e
cd "$(dirname "$0")/.."
cargo build --release --locked
install -Dm755 target/release/floki "$HOME/.local/bin/floki"
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
echo "Installed to $HOME/.local/bin/floki"
