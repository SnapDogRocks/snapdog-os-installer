#!/bin/bash
set -euo pipefail
umask 077

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

load_local_secret() {
  local variable=$1
  if [[ -f "${HOME}/.env_vars" ]]; then
    local value
    # shellcheck disable=SC2016
    value=$(
      /usr/bin/env -i \
        HOME="$HOME" \
        PATH=/usr/bin:/bin:/usr/sbin:/sbin \
        /bin/bash --noprofile --norc -c '
          # The local file is user-controlled configuration. Source it only in this isolated
          # process, then return exactly the one requested value to the packager.
          source "$1"
          variable=$2
          printf "%s" "${!variable-}"
        ' -- "${HOME}/.env_vars" "$variable"
    )
    printf -v "$variable" '%s' "$value"
  fi
  # A local .env_vars file is authoritative because login environments can truncate multiline PEM
  # values. GitHub runners have no such file and use the secrets passed through the step
  # environment. Keep either source in this shell for the explicit signing commands, but prevent
  # Cargo, build scripts, and DMG tooling from inheriting private signing material.
  export -n "${variable?}"
}

for variable in \
  MACOS_CERT_P12 \
  MACOS_CERT_PASSWORD \
  APPLE_API_KEY_CONTENT \
  APPLE_API_KEY \
  APPLE_API_ISSUER
do
  load_local_secret "$variable"
done

: "${MACOS_CERT_P12:?MACOS_CERT_P12 is required}"
: "${MACOS_CERT_PASSWORD:?MACOS_CERT_PASSWORD is required}"
: "${APPLE_API_KEY_CONTENT:?APPLE_API_KEY_CONTENT is required}"
: "${APPLE_API_KEY:?APPLE_API_KEY is required}"
: "${APPLE_API_ISSUER:?APPLE_API_ISSUER is required}"

VERSION=$(python3 -c 'import tomllib; print(tomllib.load(open("Cargo.toml", "rb"))["package"]["version"])')
test -n "$VERSION"
cargo metadata --locked --no-deps --format-version=1 >/dev/null
APP_NAME="SnapDog OS Installer"
WORKER_REQUIREMENT='identifier "cc.snapdog.os-installer" and anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] exists and certificate leaf[field.1.2.840.113635.100.6.1.13] exists and certificate leaf[subject.OU] = "898G35U5LW"'
RUNNER_TEMP=${RUNNER_TEMP:-${TMPDIR:-/tmp}}
RUNNER_TEMP=${RUNNER_TEMP%/}
WORK_DIR=""
OUTPUT_DIR=""
KEYCHAIN=""
ORIGINAL_DEFAULT_KEYCHAIN=""
ORIGINAL_KEYCHAINS=()

cleanup() {
  if [[ ${#ORIGINAL_KEYCHAINS[@]} -gt 0 ]]; then
    security list-keychains -d user -s "${ORIGINAL_KEYCHAINS[@]}" >/dev/null 2>&1 || true
  fi
  if [[ -n "$ORIGINAL_DEFAULT_KEYCHAIN" ]]; then
    security default-keychain -d user -s "$ORIGINAL_DEFAULT_KEYCHAIN" >/dev/null 2>&1 || true
  fi
  if [[ -n "$KEYCHAIN" && -f "$KEYCHAIN" ]]; then
    security delete-keychain "$KEYCHAIN" >/dev/null 2>&1 || true
  fi
  if [[ -n "$WORK_DIR" ]]; then
    rm -rf "$WORK_DIR"
  fi
  if [[ -n "$OUTPUT_DIR" ]]; then
    rm -rf "$OUTPUT_DIR"
  fi
}
trap cleanup EXIT

while IFS= read -r original_keychain; do
  original_keychain=${original_keychain#*\"}
  original_keychain=${original_keychain%\"*}
  if [[ -n "$original_keychain" ]]; then
    ORIGINAL_KEYCHAINS+=("$original_keychain")
  fi
done < <(security list-keychains -d user)
ORIGINAL_DEFAULT_KEYCHAIN=$(security default-keychain -d user | sed 's/^[[:space:]]*"//; s/"[[:space:]]*$//')

mkdir -p "$RUNNER_TEMP" "$ROOT/dist"
WORK_DIR=$(mktemp -d "$RUNNER_TEMP/snapdog-installer-package.XXXXXX")
OUTPUT_DIR=$(mktemp -d "$ROOT/dist/.snapdog-installer-package.XXXXXX")
APP="$WORK_DIR/${APP_NAME}.app"
UNVALIDATED_DMG="$OUTPUT_DIR/snapdog-os-installer-${VERSION}-macos-universal.dmg"
DMG="$ROOT/dist/snapdog-os-installer-${VERSION}-macos-universal.dmg"
KEYCHAIN="$WORK_DIR/signing.keychain-db"
KEY_FILE="$WORK_DIR/AuthKey_${APPLE_API_KEY}.p8"
CERT_FILE="$WORK_DIR/signing.p12"
STAGING="$WORK_DIR/staging"

command -v create-dmg >/dev/null 2>&1 || {
  echo "create-dmg is required (CI installs the pinned v1.2.2 source archive)" >&2
  exit 1
}

rustup target add aarch64-apple-darwin x86_64-apple-darwin
cargo build --locked --release --target aarch64-apple-darwin
cargo build --locked --release --target x86_64-apple-darwin

mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
lipo -create \
  target/aarch64-apple-darwin/release/snapdog-os-installer \
  target/x86_64-apple-darwin/release/snapdog-os-installer \
  -output "$APP/Contents/MacOS/$APP_NAME"
ARCHS=$(lipo -archs "$APP/Contents/MacOS/$APP_NAME")
if [[ "$ARCHS" != "arm64 x86_64" && "$ARCHS" != "x86_64 arm64" ]]; then
  echo "universal binary has unexpected architectures: $ARCHS" >&2
  exit 1
fi
sed "s/__VERSION__/${VERSION}/g" packaging/macos/Info.plist > "$APP/Contents/Info.plist"
cp assets/icon.icns "$APP/Contents/Resources/icon.icns"
cp LICENSE "$APP/Contents/Resources/LICENSE"
cp THIRD_PARTY_NOTICES.txt "$APP/Contents/Resources/THIRD_PARTY_NOTICES.txt"
chmod -R u+rwX,go+rX "$APP"

echo "Creating isolated signing keychain"
KEYCHAIN_PASSWORD=$(openssl rand -hex 24)
security create-keychain -p "$KEYCHAIN_PASSWORD" "$KEYCHAIN"
security set-keychain-settings -lut 21600 "$KEYCHAIN"
security unlock-keychain -p "$KEYCHAIN_PASSWORD" "$KEYCHAIN"
security list-keychains -d user -s "$KEYCHAIN" "${ORIGINAL_KEYCHAINS[@]}"
security default-keychain -d user -s "$KEYCHAIN"
if [[ -f "$MACOS_CERT_P12" ]]; then
  cp "$MACOS_CERT_P12" "$CERT_FILE"
else
  printf '%s' "$MACOS_CERT_P12" | openssl base64 -d -A -out "$CERT_FILE"
fi
echo "Importing Developer ID identity"
security import "$CERT_FILE" -k "$KEYCHAIN" -P "$MACOS_CERT_PASSWORD" -T /usr/bin/codesign >/dev/null
SIGN_IDENTITY=$(security find-identity -v -p codesigning "$KEYCHAIN" | sed -n 's/.*"\(Developer ID Application:[^"]*\)".*/\1/p' | head -1)
if [[ -z "$SIGN_IDENTITY" ]]; then
  echo "the PKCS#12 did not provide a usable Developer ID Application identity and private key" >&2
  security find-identity -v -p codesigning "$KEYCHAIN" >&2 || true
  exit 1
fi
echo "Configuring signing-key access"
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$KEYCHAIN_PASSWORD" "$KEYCHAIN" >/dev/null

echo "Signing macOS application bundle"
codesign --force --options runtime --timestamp \
  --keychain "$KEYCHAIN" \
  --entitlements packaging/macos/entitlements.plist \
  --sign "$SIGN_IDENTITY" "$APP"
codesign --verify --deep --strict --verbose=2 "-R=$WORKER_REQUIREMENT" "$APP"

mkdir -p "$STAGING"
/usr/bin/ditto "$APP" "$STAGING/${APP_NAME}.app"
create-dmg \
  --volname "$APP_NAME" \
  --volicon assets/icon.icns \
  --background assets/dmg/background.png \
  --window-pos 180 140 \
  --window-size 600 400 \
  --text-size 12 \
  --icon-size 88 \
  --icon "${APP_NAME}.app" 150 200 \
  --hide-extension "${APP_NAME}.app" \
  --app-drop-link 450 200 \
  --no-internet-enable \
  --overwrite \
  "$UNVALIDATED_DMG" "$STAGING"
codesign --force --timestamp --keychain "$KEYCHAIN" --sign "$SIGN_IDENTITY" "$UNVALIDATED_DMG"

# `notarytool` rejects an otherwise valid PKCS#8 PEM without the conventional final newline.
printf '%s\n' "$APPLE_API_KEY_CONTENT" > "$KEY_FILE"
chmod 600 "$KEY_FILE"
openssl pkey -in "$KEY_FILE" -check -noout >/dev/null

submit_for_notarization() {
  xcrun notarytool submit "$UNVALIDATED_DMG" \
    --key "$KEY_FILE" \
    --key-id "$APPLE_API_KEY" \
    --issuer "$APPLE_API_ISSUER" \
    --wait
}

# Apple's notary client has occasionally returned `invalidPEMDocument` once for a key which both
# OpenSSL and a subsequent notary request accept. Retry only that pre-upload parsing failure; all
# other failures remain fail-closed, and an accepted upload is never submitted twice.
if ! NOTARY_OUTPUT=$(submit_for_notarization 2>&1); then
  printf '%s\n' "$NOTARY_OUTPUT" >&2
  if [[ "$NOTARY_OUTPUT" != *invalidPEMDocument* || "$NOTARY_OUTPUT" == *"Submission ID received"* ]]; then
    exit 1
  fi
  sleep 2
  submit_for_notarization
else
  printf '%s\n' "$NOTARY_OUTPUT"
fi
xcrun stapler staple "$UNVALIDATED_DMG"
xcrun stapler validate "$UNVALIDATED_DMG"
spctl --assess --type open --context context:primary-signature --verbose=2 "$UNVALIDATED_DMG"

# Publish only after every signature, notarization, staple, and Gatekeeper check has passed.
mv -f "$UNVALIDATED_DMG" "$DMG"
chmod 0644 "$DMG"

echo "$DMG"
