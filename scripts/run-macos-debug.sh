#!/bin/bash
set -euo pipefail
umask 077

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

if [[ $(uname -s) != Darwin ]]; then
  echo "run-macos-debug.sh is available only on macOS" >&2
  exit 1
fi

VERSION=$(python3 -c 'import tomllib; print(tomllib.load(open("Cargo.toml", "rb"))["package"]["version"])')
APP="$ROOT/target/diagnostic/SnapDog OS Installer.app"
LOG="$HOME/Library/Logs/SnapDog OS Installer/debug.log"

cargo build --locked --profile diagnostic
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/diagnostic/snapdog-os-installer "$APP/Contents/MacOS/SnapDog OS Installer"
sed "s/__VERSION__/${VERSION}/g" packaging/macos/Info.plist > "$APP/Contents/Info.plist"
cp assets/icon.icns "$APP/Contents/Resources/icon.icns"
cp LICENSE "$APP/Contents/Resources/LICENSE"
cp THIRD_PARTY_NOTICES.txt "$APP/Contents/Resources/THIRD_PARTY_NOTICES.txt"
chmod -R u+rwX,go+rX "$APP"
codesign --force --deep --sign - "$APP"

echo "Launching ad-hoc-signed DEBUG bundle: $APP"
echo "Debug log: $LOG"
open -n "$APP"
