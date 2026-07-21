#!/bin/bash
set -euo pipefail

APPIMAGE=${1:-}
case "$APPIMAGE" in
  /*) ;;
  *) APPIMAGE="$PWD/$APPIMAGE" ;;
esac
test -s "$APPIMAGE"
test -x "$APPIMAGE"

WORK=$(mktemp -d)
SESSION=
cleanup() {
  rm -rf "$WORK"
  if [[ -n ${SESSION:-} ]]; then
    rm -rf "$SESSION"
  fi
}
trap cleanup EXIT

require_text() {
  local expected
  local path
  local description
  expected=$1
  path=$2
  description=$3
  if ! grep -F "$expected" "$path" >/dev/null; then
    echo "$description" >&2
    sed -n '1,160p' "$path" >&2
    exit 1
  fi
}

(
  cd "$WORK"
  "$APPIMAGE" --appimage-extract >/dev/null
)

APPDIR="$WORK/squashfs-root"
test -x "$APPDIR/AppRun"
test -x "$APPDIR/usr/bin/snapdog-os-installer"
test -s "$APPDIR/usr/share/licenses/snapdog-os-installer/LICENSE"
test -s "$APPDIR/usr/share/licenses/snapdog-os-installer/THIRD_PARTY_NOTICES.txt"

for soname in \
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
  test -f "$APPDIR/usr/lib/$soname" || {
    echo "AppImage is missing dlopen runtime library: $soname" >&2
    exit 1
  }
done

if ldd "$APPDIR/usr/bin/snapdog-os-installer" 2>&1 | grep -F 'not found'; then
  echo "AppImage executable has unresolved shared libraries" >&2
  exit 1
fi

max_glibc=0
while IFS= read -r -d '' candidate; do
  if readelf -h "$candidate" >/dev/null 2>&1; then
    while IFS= read -r version; do
      if [[ "$(printf '%s\n%s\n' "$max_glibc" "$version" | sort -V | tail -1)" == "$version" ]]; then
        max_glibc=$version
      fi
    done < <(
      readelf --version-info "$candidate" 2>/dev/null |
        sed -n 's/.*Name: GLIBC_\([0-9][0-9.]*\).*/\1/p'
    )
  fi
done < <(find "$APPDIR" -type f -print0)

if [[ "$(printf '%s\n%s\n' 2.28 "$max_glibc" | sort -V | tail -1)" != 2.28 ]]; then
  echo "AppImage requires GLIBC_$max_glibc; maximum permitted is GLIBC_2.28" >&2
  exit 1
fi

set +e
timeout --signal=TERM 8s xvfb-run -a env \
  APPIMAGE_EXTRACT_AND_RUN=1 \
  LIBGL_ALWAYS_SOFTWARE=1 \
  WGPU_BACKEND=gl \
  "$APPIMAGE" >"$WORK/launch.log" 2>&1
status=$?
set -e

case "$status" in
  124 | 143)
    ;;
  *)
    echo "AppImage did not remain alive during the GUI smoke test (exit $status)" >&2
    sed -n '1,160p' "$WORK/launch.log" >&2
    exit 1
    ;;
esac

# Exercise the outer AppImage's privileged-worker entry point as root without ever inspecting or
# opening a target. The pre-existing cancel marker makes the worker stop before target validation.
SESSION=$(mktemp -d /tmp/snapdog-worker-reentry.XXXXXXXXXX)
chmod 0700 "$SESSION"
chown 1000:1000 "$SESSION"
printf snap >"$SESSION/snapdog-os.img"
: >"$SESSION/worker-progress.jsonl"
: >"$SESSION/cancel"
raw_sha256=$(sha256sum "$SESSION/snapdog-os.img" | cut -d' ' -f1)
printf '%s' \
  "{\"schema_version\":1,\"drive\":{\"id\":\"sdz@4242\",\"device\":\"/dev/sdz\",\"capacity\":4096},\"raw_path\":\"$SESSION/snapdog-os.img\",\"raw_size\":4,\"verify\":true,\"expected_raw_sha256\":\"$raw_sha256\",\"progress\":{\"kind\":\"file\",\"path\":\"$SESSION/worker-progress.jsonl\"},\"cancel_path\":\"$SESSION/cancel\",\"skip_verification_path\":\"$SESSION/skip-verification\"}" \
  >"$SESSION/worker-job.json"
chown 1000:1000 "$SESSION"/*
chmod 0600 "$SESSION"/*
job_sha256=$(sha256sum "$SESSION/worker-job.json" | cut -d' ' -f1)

set +e
APPIMAGE_EXTRACT_AND_RUN=1 "$APPIMAGE" \
  --worker-job "$SESSION/worker-job.json" \
  --worker-job-sha256 "$(printf '0%.0s' {1..64})" \
  >"$WORK/bad-digest.stdout" 2>"$WORK/bad-digest.stderr"
bad_digest_status=$?
set -e
test "$bad_digest_status" -eq 1
test ! -s "$SESSION/worker-progress.jsonl"
require_text \
  'worker job changed during interactive authorization' \
  "$WORK/bad-digest.stderr" \
  'worker rejected the invalid digest without the expected diagnostic'

set +e
SNAPDOG_INSTALLER_ALLOW_UDISKS_WRITE=YES-I-UNDERSTAND \
  APPIMAGE_EXTRACT_AND_RUN=1 "$APPIMAGE" \
  --worker-job "$SESSION/worker-job.json" \
  --worker-job-sha256 "$job_sha256" \
  >"$WORK/worker.stdout" 2>"$WORK/worker.stderr"
worker_status=$?
set -e
test "$worker_status" -eq 1
require_text \
  '"phase":"cancelled"' \
  "$SESSION/worker-progress.jsonl" \
  'worker did not report cancellation through the progress channel'
require_text \
  'Error: Cancelled' \
  "$WORK/worker.stderr" \
  'worker did not return the expected cancellation diagnostic'

echo "AppImage smoke test passed (maximum required GLIBC_$max_glibc)"
