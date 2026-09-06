#!/bin/sh
set -eu

msg_file=${1:?path to the commit message is missing}
msg=$(cat "$msg_file")
body=$(printf '%s\n' "$msg" | sed -e '/^#/d' -e '/^diff --git /,$d')
subject=$(printf '%s\n' "$body" | sed -e '/^[[:space:]]*$/d' -e 1q)

fail() {
    printf 'Commit rejected: %s\n' "$1" >&2
    exit 1
}

case "$subject" in
    "Merge "*|"Revert \""*|"fixup!"*|"squash!"*|"amend!"*) exit 0 ;;
esac

types='feat|fix|docs|style|refactor|perf|test|build|ci|chore|revert'
printf '%s' "$subject" | grep -Eq "^($types)(\([a-z0-9._/-]+\))?!?: .+" ||
    fail "The subject line does not follow Conventional Commits."
[ "${#subject}" -le 100 ] || fail "The subject line exceeds 100 characters."

if printf '%s\n' "$body" |
    grep -Eiq '^[[:space:]]*co-authored-by:.*(claude|anthropic|codex|openai|chatgpt)'; then
    fail "The message contains an AI attribution trailer."
fi
if printf '%s\n' "$body" |
    grep -Eiq 'generated with .*claude code|generated with .*codex|ai-generated'; then
    fail "The message contains an AI generation line."
fi
