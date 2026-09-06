#!/bin/sh
set -eu

max_bytes=${MAX_STAGED_BYTES:-5242880}
status=0
fail() { printf 'Commit rejected: %s\n' "$1" >&2; status=1; }

branch=$(git symbolic-ref --quiet --short HEAD 2>/dev/null || echo '')
default=$(git symbolic-ref --quiet --short refs/remotes/origin/HEAD 2>/dev/null | sed 's#^origin/##')
default=${default:-main}
if [ "$branch" = "$default" ] && [ "${ALLOW_COMMIT_ON_DEFAULT:-}" != '1' ]; then
    fail "Direct commit to '$default'. Create a branch."
fi

git -c core.quotePath=false diff --cached --name-only --diff-filter=AM |
while IFS= read -r staged_file; do
    [ -f "$staged_file" ] || continue
    size=$(wc -c < "$staged_file" | tr -d ' ')
    if [ "$size" -gt "$max_bytes" ]; then
        printf 'Commit rejected: %s is %s bytes (limit %s).\n' \
            "$staged_file" "$size" "$max_bytes" >&2
        exit 1
    fi
done || status=1

if git -c core.quotePath=false diff --cached --name-only |
    grep -Eq '(^|/)(node_modules|target|dist|build|\.next|coverage)/'; then
    fail "A build or dependency directory is staged."
fi

exit "$status"
