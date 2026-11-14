#!/bin/sh
set -eu

# THIRD_PARTY_NOTICES.txt must match what cargo-about generates from the locked dependency graph.
# Used by the pre-push hook and by scripts/check.sh.
command -v cargo-about >/dev/null 2>&1 || {
    printf 'Notices check failed: cargo-about is missing (cargo install cargo-about).\n' >&2
    exit 1
}

cd "$(dirname "$0")/../.."
NOTICES=$(mktemp)
trap 'rm -f "$NOTICES"' EXIT
./scripts/generate-notices.sh "$NOTICES"
if ! cmp -s THIRD_PARTY_NOTICES.txt "$NOTICES"; then
    printf 'THIRD_PARTY_NOTICES.txt is stale; run ./scripts/generate-notices.sh\n' >&2
    exit 1
fi
