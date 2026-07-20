#!/bin/sh
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
OUTPUT=${1:-"$ROOT/THIRD_PARTY_NOTICES.txt"}
RAW=$(mktemp)
CLEAN=$(mktemp "${OUTPUT}.tmp.XXXXXX")
trap 'rm -f "$RAW" "$CLEAN"' EXIT

cd "$ROOT"
cargo about generate --locked --fail -o "$RAW" about.hbs

# License texts occasionally contain CRLF endings, trailing spaces, or an
# extra blank line. Canonicalize generated output so Git and CI agree on all
# platforms and `git diff --check` remains useful.
LC_ALL=C awk '
  {
    sub(/\r$/, "")
    sub(/[ \t]+$/, "")
    lines[NR] = $0
    if ($0 != "") {
      last = NR
    }
  }
  END {
    for (line = 1; line <= last; line++) {
      print lines[line]
    }
  }
' "$RAW" >"$CLEAN"

chmod 0644 "$CLEAN"
mv "$CLEAN" "$OUTPUT"
