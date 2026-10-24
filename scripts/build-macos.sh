#!/bin/sh
# Builds dist/Floki-macos-universal.dmg (Apple Silicon + Intel) and dist/floki-cli-macos-universal on macOS.
# Signs with $MACOS_SIGNING_IDENTITY ("Developer ID Application: …") when set, otherwise ad-hoc.
# Usage: scripts/build-macos.sh <version>
set -e
cd "$(dirname "$0")/.."
VERSION=${1#v}
: "${VERSION:?usage: build-macos.sh <version>}"

for target in aarch64-apple-darwin x86_64-apple-darwin; do
    cargo build --release --locked --target "$target" --bins
done
mkdir -p dist
for bin in floki floki-cli; do
    lipo -create -output "dist/$bin-universal" \
        "target/aarch64-apple-darwin/release/$bin" "target/x86_64-apple-darwin/release/$bin"
done

APP=dist/dmg/Floki.app
rm -rf dist/dmg dist/Floki.iconset
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" dist/Floki.iconset
cp dist/floki-universal "$APP/Contents/MacOS/Floki"
sed "s/VERSION/$VERSION/g" assets/macos/Info.plist > "$APP/Contents/Info.plist"

# iconutil wants each size at 1x and 2x.
for size in 16 32 128 256 512; do
    sips -z "$size" "$size" assets/app-icon/floki-512.png --out "dist/Floki.iconset/icon_${size}x${size}.png" >/dev/null
    double=$((size * 2))
    sips -z "$double" "$double" assets/app-icon/floki-512.png --out "dist/Floki.iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns dist/Floki.iconset -o "$APP/Contents/Resources/Floki.icns"

IDENTITY=${MACOS_SIGNING_IDENTITY:--}
# Hardened runtime and a timestamp are what notarization requires; ad-hoc signing skips them.
sign() {
    if [ "$IDENTITY" = "-" ]; then
        codesign --force --sign - "$@"
    else
        codesign --force --options runtime --timestamp --sign "$IDENTITY" "$@"
    fi
}
sign dist/floki-cli-universal
sign "$APP"
codesign --verify --strict "$APP"
cp dist/floki-cli-universal dist/floki-cli-macos-universal

ln -s /Applications dist/dmg/Applications
rm -f dist/Floki-macos-universal.dmg
# hdiutil's own size estimate is often too small for the app ("No space left on device"), so give it room.
SIZE_MB=$(( $(du -sm dist/dmg/Floki.app | cut -f1) + 64 ))
hdiutil create -volname Floki -srcfolder dist/dmg -size "${SIZE_MB}m" -fs HFS+ -ov -format UDZO dist/Floki-macos-universal.dmg
if [ "$IDENTITY" != "-" ]; then
    sign dist/Floki-macos-universal.dmg
fi
echo "Built dist/Floki-macos-universal.dmg"
