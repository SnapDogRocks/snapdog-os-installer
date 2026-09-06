#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: $0 list-payloads VERSION | verify DIR VERSION [payloads|complete]" >&2
  exit 2
}

payloads() {
  local version=$1
  printf '%s\n' \
    "snapdog-os-installer-$version-linux-x86_64.AppImage" \
    "snapdog-os-installer-$version-linux-aarch64.AppImage" \
    "snapdog-os-installer-$version-windows-x86_64.exe" \
    "snapdog-os-installer-$version-windows-aarch64.exe" \
    "snapdog-os-installer-$version-macos-universal.dmg"
}

validate_version() {
  if [[ ! $1 =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "invalid release version: $1" >&2
    exit 1
  fi
}

case ${1:-} in
  list-payloads)
    [[ $# -eq 2 ]] || usage
    validate_version "$2"
    payloads "$2"
    ;;
  verify)
    [[ $# -ge 3 && $# -le 4 ]] || usage
    directory=$2
    version=$3
    mode=${4:-complete}
    validate_version "$version"
    [[ -d $directory ]] || {
      echo "release asset directory does not exist: $directory" >&2
      exit 1
    }
    [[ $mode == payloads || $mode == complete ]] || usage

    expected=()
    while IFS= read -r payload; do
      expected+=("$payload")
      if [[ $mode == complete ]]; then
        expected+=("$payload.spdx.json" "$payload.sigstore.json")
      fi
    done < <(payloads "$version")
    if [[ $mode == complete ]]; then
      expected+=(SHA256SUMS)
    fi

    actual=()
    while IFS= read -r file; do
      actual+=("${file#"$directory"/}")
    done < <(find "$directory" -mindepth 1 -maxdepth 1 -type f -print | LC_ALL=C sort)

    diff -u \
      <(printf '%s\n' "${expected[@]}" | LC_ALL=C sort) \
      <(printf '%s\n' "${actual[@]}" | LC_ALL=C sort)
    for asset in "${expected[@]}"; do
      [[ -s "$directory/$asset" ]] || {
        echo "release asset is missing or empty: $asset" >&2
        exit 1
      }
    done

    if [[ $mode == complete ]]; then
      checksum_entries=()
      while IFS= read -r entry; do
        checksum_entries+=("$entry")
      done < <(awk '{ name=$2; sub(/^\*/, "", name); print name }' "$directory/SHA256SUMS" | LC_ALL=C sort)
      expected_checksum_entries=()
      for asset in "${expected[@]}"; do
        [[ $asset == SHA256SUMS ]] || expected_checksum_entries+=("$asset")
      done
      diff -u \
        <(printf '%s\n' "${expected_checksum_entries[@]}" | LC_ALL=C sort) \
        <(printf '%s\n' "${checksum_entries[@]}")
      (cd "$directory" && sha256sum --check --strict SHA256SUMS)
    fi
    ;;
  *) usage ;;
esac
