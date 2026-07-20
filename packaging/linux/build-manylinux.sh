#!/bin/bash
set -euo pipefail

TARGET=${1:-}
# The default path always runs target-native Clippy and tests. Package-only is an explicit local
# retry mode for environments such as QEMU after those checks have already completed successfully.
MODE=${2:-}
case "$TARGET" in
  x86_64-unknown-linux-gnu)
    EXPECTED_MACHINE=x86_64
    RUSTUP_SHA256=20a06e644b0d9bd2fbdbfd52d42540bdde820ea7df86e92e533c073da0cdd43c
    ;;
  aarch64-unknown-linux-gnu)
    EXPECTED_MACHINE=aarch64
    RUSTUP_SHA256=e3853c5a252fca15252d07cb23a1bdd9377a8c6f3efa01531109281ae47f841c
    ;;
  *)
    echo "usage: $0 {x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu}" >&2
    exit 64
    ;;
esac
case "$MODE" in
  ""|--package-only) ;;
  *)
    echo "usage: $0 {x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu} [--package-only]" >&2
    exit 64
    ;;
esac

test "$(uname -m)" = "$EXPECTED_MACHINE"
test "$(getconf GNU_LIBC_VERSION)" = "glibc 2.28"

restore_ownership() {
  if [[ ${HOST_UID:-} =~ ^[0-9]+$ && ${HOST_GID:-} =~ ^[0-9]+$ ]]; then
    chown -R "$HOST_UID:$HOST_GID" /workspace/dist /workspace/target 2>/dev/null || true
  fi
}
trap restore_ownership EXIT

dnf install --assumeyes --setopt=install_weak_deps=False \
  alsa-lib-devel \
  binutils \
  curl \
  file \
  gcc \
  gcc-c++ \
  git \
  libX11-devel \
  libXcursor-devel \
  libXi-devel \
  libXinerama-devel \
  libXrandr-devel \
  libxkbcommon-devel \
  libxkbcommon-x11-devel \
  mesa-libEGL-devel \
  pkgconf-pkg-config \
  systemd-devel \
  wayland-devel

export CARGO_HOME=${CARGO_HOME:-/opt/snapdog-cargo}
export RUSTUP_HOME=${RUSTUP_HOME:-/opt/snapdog-rustup}
export PATH="$CARGO_HOME/bin:$PATH"
RUSTUP_INIT="$CARGO_HOME/bin/rustup-init"
mkdir -p "$CARGO_HOME/bin" "$RUSTUP_HOME"

if ! command -v rustup >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 --location --fail --silent --show-error \
    "https://static.rust-lang.org/rustup/archive/1.28.2/$TARGET/rustup-init" \
    --output "$RUSTUP_INIT"
  printf '%s  %s\n' "$RUSTUP_SHA256" "$RUSTUP_INIT" | sha256sum --check --status
  chmod 0755 "$RUSTUP_INIT"
  "$RUSTUP_INIT" -y --profile minimal --default-toolchain 1.88.0
fi

rustup toolchain install 1.88.0 --profile minimal --no-self-update \
  --component clippy --component rustfmt
rustup default 1.88.0

BUILD_UID=${HOST_UID:-1000}
BUILD_GID=${HOST_GID:-1000}
if [[ "$BUILD_UID" == 0 ]]; then
  BUILD_UID=1000
fi
if [[ "$BUILD_GID" == 0 ]]; then
  BUILD_GID=1000
fi

if getent group "$BUILD_GID" >/dev/null; then
  BUILD_GROUP=$(getent group "$BUILD_GID" | cut -d: -f1)
else
  BUILD_GROUP=snapdog-build
  groupadd --gid "$BUILD_GID" "$BUILD_GROUP"
fi
if getent passwd "$BUILD_UID" >/dev/null; then
  BUILD_USER=$(getent passwd "$BUILD_UID" | cut -d: -f1)
else
  BUILD_USER=snapdog-build
  useradd --uid "$BUILD_UID" --gid "$BUILD_GROUP" --no-create-home \
    --home-dir /tmp/snapdog-build-home --shell /bin/bash "$BUILD_USER"
fi

mkdir -p /tmp/snapdog-build-home /workspace/dist /workspace/target
chown -R "$BUILD_UID:$BUILD_GID" \
  /tmp/snapdog-build-home "$CARGO_HOME" "$RUSTUP_HOME" /workspace/dist /workspace/target

# Never run the test suite as root: the worker/session tests intentionally
# reject root-owned GUI session state as part of the raw-device safety model.
# TARGET is expanded by the unprivileged inner shell.
# shellcheck disable=SC2016
runuser --user "$BUILD_USER" -- env \
  CARGO_HOME="$CARGO_HOME" \
  HOME=/tmp/snapdog-build-home \
  PATH="$PATH" \
  RUSTFLAGS=-Dwarnings \
  RUSTUP_HOME="$RUSTUP_HOME" \
  MODE="$MODE" \
  TARGET="$TARGET" \
  bash -c '
    set -euo pipefail
    if [[ "$MODE" != --package-only ]]; then
      cargo clippy --locked --target "$TARGET" --all-targets --all-features -- -D warnings
      cargo test --locked --target "$TARGET" --all-features
    fi
    ./scripts/package-linux.sh "$TARGET"
  '
