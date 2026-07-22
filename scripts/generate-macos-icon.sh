#!/bin/bash
set -euo pipefail
umask 077

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

if [[ $(uname -s) != Darwin ]]; then
  echo "generate-macos-icon.sh requires macOS and Xcode 26 or newer" >&2
  exit 1
fi

DEVELOPER_DIR=$(xcode-select -p)
XCODE_CONTENTS=$(dirname "$DEVELOPER_DIR")
ICTOOL="$XCODE_CONTENTS/Applications/Icon Composer.app/Contents/Executables/ictool"
if [[ ! -x "$ICTOOL" ]]; then
  echo "Icon Composer is unavailable; install Xcode 26 or newer" >&2
  exit 1
fi

WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/snapdog-macos-icon.XXXXXX")
cleanup() {
  rm -rf "$WORK_DIR"
}
trap cleanup EXIT
mkdir -p "$WORK_DIR/output"

xcrun actool \
  --compile "$WORK_DIR/output" \
  --platform macosx \
  --minimum-deployment-target 12.0 \
  --target-device mac \
  --app-icon AppIcon \
  --output-partial-info-plist "$WORK_DIR/partial.plist" \
  --output-format human-readable-text \
  --warnings \
  --errors \
  --notices \
  assets/AppIcon.icon

[[ $(/usr/libexec/PlistBuddy -c 'Print :CFBundleIconFile' "$WORK_DIR/partial.plist") == AppIcon ]]
[[ $(/usr/libexec/PlistBuddy -c 'Print :CFBundleIconName' "$WORK_DIR/partial.plist") == AppIcon ]]

"$ICTOOL" assets/AppIcon.icon \
  --export-image \
  --output-file "$WORK_DIR/icon-macos-16bit.png" \
  --platform macOS \
  --rendition Default \
  --width 1024 \
  --height 1024 \
  --scale 1
xcrun pngcrush -q -bit_depth 8 \
  "$WORK_DIR/icon-macos-16bit.png" "$WORK_DIR/icon-macos.png"

ICONSET="$WORK_DIR/AppIcon.iconset"
mkdir -p "$ICONSET"
while read -r filename pixels; do
  sips -z "$pixels" "$pixels" "$WORK_DIR/icon-macos.png" \
    --out "$WORK_DIR/$filename" >/dev/null
  xcrun pngcrush -q "$WORK_DIR/$filename" "$ICONSET/$filename"
done <<'SIZES'
icon_16x16.png 16
icon_16x16@2x.png 32
icon_32x32.png 32
icon_32x32@2x.png 64
icon_128x128.png 128
icon_128x128@2x.png 256
icon_256x256.png 256
icon_256x256@2x.png 512
icon_512x512.png 512
icon_512x512@2x.png 1024
SIZES
iconutil --convert icns --output "$WORK_DIR/icon.icns" "$ICONSET"

cp "$WORK_DIR/icon.icns" assets/icon.icns
cp "$WORK_DIR/output/Assets.car" assets/Assets.car
cp "$WORK_DIR/icon-macos.png" assets/icon-macos.png

echo "Regenerated Apple Icon Composer resources"
