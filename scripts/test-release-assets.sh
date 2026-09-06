#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
test_dir=$(mktemp -d "${TMPDIR:-/tmp}/snapdog-release-assets.XXXXXX")
trap 'rm -rf "$test_dir"' EXIT
version=9.8.7

while IFS= read -r asset; do
  printf 'payload:%s\n' "$asset" > "$test_dir/$asset"
  printf '{"sbom":"%s"}\n' "$asset" > "$test_dir/$asset.spdx.json"
  printf '{"signature":"%s"}\n' "$asset" > "$test_dir/$asset.sigstore.json"
done < <("$root/scripts/release-assets.sh" list-payloads "$version")

checksum_file=$(mktemp "${TMPDIR:-/tmp}/snapdog-release-checksums.XXXXXX")
(
  cd "$test_dir"
  find . -mindepth 1 -maxdepth 1 -type f -exec basename {} \; |
    LC_ALL=C sort |
    xargs sha256sum --
) > "$checksum_file"
mv "$checksum_file" "$test_dir/SHA256SUMS"
"$root/scripts/release-assets.sh" verify "$test_dir" "$version" complete

missing="$test_dir/snapdog-os-installer-$version-linux-x86_64.AppImage.sigstore.json"
mv "$missing" "$missing.saved"
if "$root/scripts/release-assets.sh" verify "$test_dir" "$version" complete >/dev/null 2>&1; then
  echo 'asset verifier accepted a missing signature bundle' >&2
  exit 1
fi
mv "$missing.saved" "$missing"

printf 'unexpected\n' > "$test_dir/unexpected.txt"
if "$root/scripts/release-assets.sh" verify "$test_dir" "$version" complete >/dev/null 2>&1; then
  echo 'asset verifier accepted an unexpected file' >&2
  exit 1
fi
rm "$test_dir/unexpected.txt"

printf 'tampered\n' >> "$test_dir/snapdog-os-installer-$version-linux-aarch64.AppImage"
if "$root/scripts/release-assets.sh" verify "$test_dir" "$version" complete >/dev/null 2>&1; then
  echo 'asset verifier accepted a checksum mismatch' >&2
  exit 1
fi

echo 'release asset contract self-test passed'
