#!/bin/bash
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

if [[ -f "${HOME}/.env_vars" ]]; then
  set -a
  # shellcheck disable=SC1091
  source "${HOME}/.env_vars"
  set +a
fi

: "${MACOS_CERT_P12:?MACOS_CERT_P12 is required}"
: "${MACOS_CERT_PASSWORD:?MACOS_CERT_PASSWORD is required}"
: "${APPLE_API_KEY_CONTENT:?APPLE_API_KEY_CONTENT is required}"
: "${APPLE_API_KEY:?APPLE_API_KEY is required}"
: "${APPLE_API_ISSUER:?APPLE_API_ISSUER is required}"

VERSION=$(cargo metadata --no-deps --format-version=1 | sed -n 's/.*"version":"\([^"]*\)".*/\1/p')
APP_NAME="SnapDog OS Installer"
APP="$ROOT/dist/${APP_NAME}.app"
DMG="$ROOT/dist/snapdog-os-installer-${VERSION}-macos-universal.dmg"
RUNNER_TEMP=${RUNNER_TEMP:-${TMPDIR%/}}
KEYCHAIN="$RUNNER_TEMP/snapdog-installer-signing.keychain-db"
KEY_FILE="$RUNNER_TEMP/AuthKey_${APPLE_API_KEY}.p8"
CERT_FILE="$RUNNER_TEMP/snapdog-installer-signing.p12"
STAGING=""

cleanup() {
  if [[ -n "$STAGING" ]]; then
    rm -rf "$STAGING"
  fi
  rm -f "$KEY_FILE" "$CERT_FILE"
  if [[ -f "$KEYCHAIN" ]]; then
    security delete-keychain "$KEYCHAIN" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

mkdir -p "$RUNNER_TEMP" "$ROOT/dist"
rm -rf "$APP" "$DMG" "$KEYCHAIN"

rustup target add aarch64-apple-darwin x86_64-apple-darwin
cargo build --locked --release --target aarch64-apple-darwin
cargo build --locked --release --target x86_64-apple-darwin

mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
lipo -create \
  target/aarch64-apple-darwin/release/snapdog-os-installer \
  target/x86_64-apple-darwin/release/snapdog-os-installer \
  -output "$APP/Contents/MacOS/snapdog-os-installer"
sed "s/__VERSION__/${VERSION}/g" packaging/macos/Info.plist > "$APP/Contents/Info.plist"
cp assets/icon.icns "$APP/Contents/Resources/icon.icns"

KEYCHAIN_PASSWORD=$(openssl rand -hex 24)
security create-keychain -p "$KEYCHAIN_PASSWORD" "$KEYCHAIN"
security set-keychain-settings -lut 21600 "$KEYCHAIN"
security unlock-keychain -p "$KEYCHAIN_PASSWORD" "$KEYCHAIN"
if [[ -f "$MACOS_CERT_P12" ]]; then
  cp "$MACOS_CERT_P12" "$CERT_FILE"
else
  printf '%s' "$MACOS_CERT_P12" | openssl base64 -d -A -out "$CERT_FILE"
fi
security import "$CERT_FILE" -k "$KEYCHAIN" -P "$MACOS_CERT_PASSWORD" -T /usr/bin/codesign >/dev/null
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$KEYCHAIN_PASSWORD" "$KEYCHAIN" >/dev/null
SIGN_IDENTITY=$(security find-identity -v -p codesigning "$KEYCHAIN" | sed -n 's/.*"\(Developer ID Application:[^"]*\)".*/\1/p' | head -1)
test -n "$SIGN_IDENTITY"

codesign --force --options runtime --timestamp \
  --entitlements packaging/macos/entitlements.plist \
  --sign "$SIGN_IDENTITY" "$APP"
codesign --verify --deep --strict --verbose=2 "$APP"

STAGING=$(mktemp -d)
cp -R "$APP" "$STAGING/"
create-dmg \
  --volname "$APP_NAME" \
  --volicon assets/icon.icns \
  --background assets/dmg/background.png \
  --window-pos 180 140 \
  --window-size 600 400 \
  --icon-size 112 \
  --icon "${APP_NAME}.app" 160 190 \
  --hide-extension "${APP_NAME}.app" \
  --app-drop-link 440 190 \
  --no-internet-enable \
  --overwrite \
  "$DMG" "$STAGING"
codesign --force --timestamp --sign "$SIGN_IDENTITY" "$DMG"

printf '%s' "$APPLE_API_KEY_CONTENT" > "$KEY_FILE"
chmod 600 "$KEY_FILE"
xcrun notarytool submit "$DMG" \
  --key "$KEY_FILE" \
  --key-id "$APPLE_API_KEY" \
  --issuer "$APPLE_API_ISSUER" \
  --wait
xcrun stapler staple "$DMG"
xcrun stapler validate "$DMG"
spctl --assess --type open --context context:primary-signature --verbose=2 "$DMG"

echo "$DMG"
