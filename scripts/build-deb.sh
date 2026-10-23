#!/bin/sh
# Packages target/release/floki as dist/Floki-linux-x64.deb. Usage: scripts/build-deb.sh <version>
set -e
cd "$(dirname "$0")/.."
VERSION=${1#v}
: "${VERSION:?usage: build-deb.sh <version>}"
ROOT=dist/deb
rm -rf "$ROOT"
install -Dm755 target/release/floki "$ROOT/usr/bin/floki"
for size in 16 24 32 48 64 128 256 512; do
    install -Dm644 "assets/app-icon/floki-$size.png" "$ROOT/usr/share/icons/hicolor/${size}x${size}/apps/floki.png"
done
install -Dm644 assets/floki.svg "$ROOT/usr/share/icons/hicolor/scalable/apps/floki.svg"
install -Dm644 assets/fonts/OFL-Inter.txt "$ROOT/usr/share/doc/floki/OFL-Inter.txt"
install -Dm644 assets/fonts/OFL-IBMPlexMono.txt "$ROOT/usr/share/doc/floki/OFL-IBMPlexMono.txt"
install -Dm644 /dev/stdin "$ROOT/usr/share/applications/floki.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=Floki
Comment=Native API client
Exec=floki
Icon=floki
Terminal=false
Categories=Development;
StartupWMClass=floki
DESKTOP
install -Dm644 /dev/stdin "$ROOT/DEBIAN/control" <<CONTROL
Package: floki
Version: $VERSION
Architecture: amd64
Maintainer: Floki <adityakamal303@gmail.com>
Installed-Size: $(du -sk "$ROOT/usr" | cut -f1)
Depends: libc6, libxcb1, libxkbcommon0, libxkbcommon-x11-0, libfontconfig1, libfreetype6, libwayland-client0, libvulkan1
Section: devel
Priority: optional
Homepage: https://github.com/Adityakumar-blip/floki
Description: Fast, local-first API client
 Sends HTTP, WebSocket, Socket.IO and gRPC requests and keeps collections on your machine.
CONTROL
dpkg-deb --root-owner-group --build "$ROOT" dist/Floki-linux-x64.deb
