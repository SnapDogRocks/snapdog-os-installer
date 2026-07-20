#!/bin/sh
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

TARGET=${1:-}
if [ "$TARGET" = "--container" ]; then
  TARGET=${2:-}
  case "$TARGET" in
    x86_64-unknown-linux-gnu)
      CONTAINER_PLATFORM=linux/amd64
      CONTAINER_ARCH=x86_64
      BASELINE_IMAGE='quay.io/pypa/manylinux_2_28_x86_64@sha256:a61875a2f84cab7df8de222ff12cabc08ff86eb4ad402ac90ba7bdaed9600cca'
      ;;
    aarch64-unknown-linux-gnu)
      CONTAINER_PLATFORM=linux/arm64
      CONTAINER_ARCH=aarch64
      BASELINE_IMAGE='quay.io/pypa/manylinux_2_28_aarch64@sha256:162c81dfd3efc710732a571717d3c916a6945ebf279e879ddee3243af96fe46f'
      ;;
    *)
      echo "usage: $0 --container {x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu}" >&2
      exit 64
      ;;
  esac

  command -v docker >/dev/null 2>&1 || {
    echo "Docker is required for a portable Linux package build" >&2
    exit 1
  }
  CARGO_VOLUME="snapdog-installer-manylinux28-cargo-$CONTAINER_ARCH"
  RUSTUP_VOLUME="snapdog-installer-manylinux28-rustup-$CONTAINER_ARCH"
  docker volume create "$CARGO_VOLUME" >/dev/null
  docker volume create "$RUSTUP_VOLUME" >/dev/null
  exec docker run --rm \
    --platform "$CONTAINER_PLATFORM" \
    --env CARGO_HOME=/cargo \
    --env HOST_GID="$(id -g)" \
    --env HOST_UID="$(id -u)" \
    --env RUNNER_TEMP=/cargo/package-tools \
    --env SNAPDOG_PACKAGE_WORK_DIR=/cargo/package-work \
    --env RUSTUP_HOME=/rustup \
    --volume "$CARGO_VOLUME:/cargo" \
    --volume "$RUSTUP_VOLUME:/rustup" \
    --volume "$ROOT:/workspace" \
    --workdir /workspace \
    "$BASELINE_IMAGE" \
    ./packaging/linux/build-manylinux.sh "$TARGET"
fi

case "$TARGET" in
  x86_64-unknown-linux-gnu)
    APPIMAGE_ARCH=x86_64
    EXPECTED_HOST=x86_64
    LINUXDEPLOY_SHA256=c20cd71e3a4e3b80c3483cef793cda3f4e990aca14014d23c544ca3ce1270b4d
    RUNTIME_SHA256=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d
    ;;
  aarch64-unknown-linux-gnu)
    APPIMAGE_ARCH=aarch64
    EXPECTED_HOST=aarch64
    LINUXDEPLOY_SHA256=620095110d693282b8ebeb244a95b5e911cf8f65f76c88b4b47d16ae6346fcff
    RUNTIME_SHA256=00cbdfcf917cc6c0ff6d3347d59e0ca1f7f45a6df1a428a0d6d8a78664d87444
    ;;
  *)
    echo "usage: $0 {x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu}" >&2
    exit 64
    ;;
esac

HOST_ARCH=$(uname -m)
if [ "$HOST_ARCH" != "$EXPECTED_HOST" ]; then
  echo "AppImages must be assembled natively: target $TARGET requires a $EXPECTED_HOST host, got $HOST_ARCH" >&2
  exit 1
fi

GLIBC_VERSION=$(getconf GNU_LIBC_VERSION | sed 's/^glibc //')
if [ "$(printf '%s\n%s\n' 2.28 "$GLIBC_VERSION" | sort -V | tail -1)" != 2.28 ]; then
  echo "refusing a non-portable GLIBC_$GLIBC_VERSION build; use '$0 --container $TARGET'" >&2
  exit 1
fi

VERSION=$(cargo metadata --locked --no-deps --format-version=1 |
  sed -n 's/.*"name":"snapdog-os-installer","version":"\([^"]*\)".*/\1/p')
if [ -z "$VERSION" ]; then
  echo "could not determine package version" >&2
  exit 1
fi

DIST="$ROOT/dist"
case ${CARGO_TARGET_DIR:-} in
  "") BUILD_TARGET_DIR="$ROOT/target" ;;
  /*) BUILD_TARGET_DIR=$CARGO_TARGET_DIR ;;
  *) BUILD_TARGET_DIR="$ROOT/$CARGO_TARGET_DIR" ;;
esac
PACKAGE_WORK_DIR=${SNAPDOG_PACKAGE_WORK_DIR:-"$BUILD_TARGET_DIR/package"}
APPDIR="$PACKAGE_WORK_DIR/$TARGET/SnapDog_OS_Installer.AppDir"
OUTPUT="$DIST/snapdog-os-installer-$VERSION-linux-$APPIMAGE_ARCH.AppImage"
ASSEMBLED_OUTPUT="$PACKAGE_WORK_DIR/output/snapdog-os-installer-$VERSION-linux-$APPIMAGE_ARCH.AppImage"
TOOL_DIR=${RUNNER_TEMP:-"$ROOT/target/package/tools"}
LINUXDEPLOY="$TOOL_DIR/linuxdeploy-$APPIMAGE_ARCH.AppImage"
LINUXDEPLOY_VERSION=1-alpha-20251107-1
LINUXDEPLOY_URL="https://github.com/linuxdeploy/linuxdeploy/releases/download/$LINUXDEPLOY_VERSION/linuxdeploy-$APPIMAGE_ARCH.AppImage"
RUNTIME="$TOOL_DIR/runtime-$APPIMAGE_ARCH"
RUNTIME_VERSION=20251108
RUNTIME_URL="https://github.com/AppImage/type2-runtime/releases/download/$RUNTIME_VERSION/runtime-$APPIMAGE_ARCH"

mkdir -p "$DIST" "$TOOL_DIR" "$(dirname "$ASSEMBLED_OUTPUT")"
rm -rf "$APPDIR" "$ASSEMBLED_OUTPUT" "$OUTPUT"

cargo build --locked --release --target "$TARGET"

BINARY="$BUILD_TARGET_DIR/$TARGET/release/snapdog-os-installer"
MAX_GLIBC=$(objdump -T "$BINARY" 2>/dev/null |
  sed -n 's/.*GLIBC_\([0-9][0-9.]*\).*/\1/p' |
  sort -V |
  tail -1)
if [ -n "$MAX_GLIBC" ] && \
  [ "$(printf '%s\n%s\n' 2.28 "$MAX_GLIBC" | sort -V | tail -1)" != 2.28 ]; then
  echo "binary requires GLIBC_$MAX_GLIBC; maximum permitted is GLIBC_2.28" >&2
  exit 1
fi

mkdir -p \
  "$APPDIR/usr/bin" \
  "$APPDIR/usr/share/applications" \
  "$APPDIR/usr/share/icons/hicolor/512x512/apps" \
  "$APPDIR/usr/share/licenses/snapdog-os-installer" \
  "$APPDIR/usr/share/metainfo"
install -m 0755 \
  "$BINARY" \
  "$APPDIR/usr/bin/snapdog-os-installer"
install -m 0644 \
  "$ROOT/packaging/linux/cc.snapdog.os-installer.desktop" \
  "$APPDIR/usr/share/applications/cc.snapdog.os-installer.desktop"
install -m 0644 \
  "$ROOT/packaging/linux/cc.snapdog.os-installer.appdata.xml" \
  "$APPDIR/usr/share/metainfo/cc.snapdog.os-installer.appdata.xml"
install -m 0644 \
  "$ROOT/packaging/linux/cc.snapdog.os-installer.png" \
  "$APPDIR/usr/share/icons/hicolor/512x512/apps/cc.snapdog.os-installer.png"
install -m 0644 \
  "$ROOT/LICENSE" \
  "$APPDIR/usr/share/licenses/snapdog-os-installer/LICENSE"
install -m 0644 \
  "$ROOT/THIRD_PARTY_NOTICES.txt" \
  "$APPDIR/usr/share/licenses/snapdog-os-installer/THIRD_PARTY_NOTICES.txt"

curl --proto '=https' --tlsv1.2 --location --fail --silent --show-error \
  "$LINUXDEPLOY_URL" --output "$LINUXDEPLOY"
printf '%s  %s\n' "$LINUXDEPLOY_SHA256" "$LINUXDEPLOY" | sha256sum --check --status
chmod 0755 "$LINUXDEPLOY"
curl --proto '=https' --tlsv1.2 --location --fail --silent --show-error \
  "$RUNTIME_URL" --output "$RUNTIME"
printf '%s  %s\n' "$RUNTIME_SHA256" "$RUNTIME" | sha256sum --check --status
chmod 0755 "$RUNTIME"

# winit and the Wayland/X11 support crates intentionally dlopen these libraries,
# so they are invisible to linuxdeploy's normal ELF dependency walk. Resolve
# the exact baseline-container SONAMEs and deploy them explicitly; linuxdeploy
# then follows their transitive dependencies as usual.
set --
for SONAME in \
  libX11.so.6 \
  libX11-xcb.so.1 \
  libXcursor.so.1 \
  libXi.so.6 \
  libXinerama.so.1 \
  libXrandr.so.2 \
  libxkbcommon.so.0 \
  libxkbcommon-x11.so.0 \
  libwayland-client.so.0 \
  libwayland-cursor.so.0 \
  libwayland-egl.so.1
do
  LIBRARY=$(ldconfig -p | awk -v soname="$SONAME" '$1 == soname { print $NF; exit }')
  if [ -z "$LIBRARY" ] || [ ! -f "$LIBRARY" ]; then
    echo "required runtime library was not found: $SONAME" >&2
    exit 1
  fi
  set -- "$@" --library "$LIBRARY"
done

# The GitHub runners do not expose FUSE. linuxdeploy can extract and execute
# itself in that environment while still producing a regular type-2 AppImage.
APPIMAGE_EXTRACT_AND_RUN=1 \
ARCH="$APPIMAGE_ARCH" \
LDAI_OUTPUT="$ASSEMBLED_OUTPUT" \
LDAI_RUNTIME_FILE="$RUNTIME" \
"$LINUXDEPLOY" \
  --appdir "$APPDIR" \
  --executable "$APPDIR/usr/bin/snapdog-os-installer" \
  --desktop-file "$APPDIR/usr/share/applications/cc.snapdog.os-installer.desktop" \
  --icon-file "$APPDIR/usr/share/icons/hicolor/512x512/apps/cc.snapdog.os-installer.png" \
  "$@" \
  --output appimage

test -s "$ASSEMBLED_OUTPUT"
install -m 0755 "$ASSEMBLED_OUTPUT" "$OUTPUT"
test -s "$OUTPUT"
echo "$OUTPUT"
