#!/bin/sh
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
OUTPUT=${1:-"$ROOT/THIRD_PARTY_NOTICES.txt"}
RAW=$(mktemp)
VERSION_NEUTRAL=$(mktemp)
CLEAN=$(mktemp "${OUTPUT}.tmp.XXXXXX")
trap 'rm -f "$RAW" "$VERSION_NEUTRAL" "$CLEAN"' EXIT

cd "$ROOT"
cargo about generate --locked --fail -o "$RAW" about.hbs

# Keep the application's GPL entry visible in acknowledgements without tying
# the generated file to the application version. Dependency versions remain
# intact because they are part of the locked third-party dependency graph.
LC_ALL=C awk -v root_crate="snapdog-os-installer" '
  $0 == "Used by:" {
    in_used_by = 1
    print
    next
  }

  in_used_by && $0 == "" {
    in_used_by = 0
    print
    next
  }

  in_used_by && index($0, "- " root_crate " ") == 1 {
    print "- " root_crate " (application)"
    next
  }

  {
    print
  }
' "$RAW" >"$VERSION_NEUTRAL"

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
' "$VERSION_NEUTRAL" >"$CLEAN"

chmod 0644 "$CLEAN"
mv "$CLEAN" "$OUTPUT"
