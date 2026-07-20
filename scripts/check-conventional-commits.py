#!/usr/bin/env python3
"""Validate commit subjects against the Conventional Commits specification."""

from __future__ import annotations

import argparse
import re
import subprocess
import sys


TYPES = (
    "build",
    "chore",
    "ci",
    "docs",
    "feat",
    "fix",
    "perf",
    "refactor",
    "revert",
    "style",
    "test",
)
SUBJECT = re.compile(
    rf"^(?:{'|'.join(TYPES)})(?:\([a-z0-9][a-z0-9._/-]*\))?!?: \S.*$"
)


def is_conventional(subject: str) -> bool:
    return bool(SUBJECT.fullmatch(subject))


def commit_subjects(base: str, head: str) -> list[str]:
    result = subprocess.run(
        ["git", "log", "--format=%s", f"{base}..{head}"],
        check=True,
        capture_output=True,
        text=True,
    )
    return result.stdout.splitlines()


def self_test() -> None:
    valid = (
        "feat: add image verification",
        "fix(macos): preserve icon transparency",
        "refactor(worker/native): simplify disk lookup",
        "feat!: remove legacy catalog",
        "fix(api)!: reject unsigned catalogs",
    )
    invalid = (
        "Add image verification",
        "feature: add image verification",
        "fix(): add image verification",
        "fix: ",
        "Merge main into feature",
    )
    assert all(is_conventional(subject) for subject in valid)
    assert not any(is_conventional(subject) for subject in invalid)


def main() -> int:
    parser = argparse.ArgumentParser()
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--message", action="append", help="subject to validate")
    source.add_argument("--range", nargs=2, metavar=("BASE", "HEAD"))
    source.add_argument("--self-test", action="store_true")
    args = parser.parse_args()

    if args.self_test:
        self_test()
        return 0

    subjects = args.message or commit_subjects(*args.range)
    if not subjects:
        print("No commit messages found in the requested range", file=sys.stderr)
        return 1

    invalid = [subject for subject in subjects if not is_conventional(subject)]
    if invalid:
        print("Invalid Conventional Commit subject(s):", file=sys.stderr)
        for subject in invalid:
            print(f"  - {subject}", file=sys.stderr)
        print(
            "Expected: <type>[optional scope][!]: <description>",
            file=sys.stderr,
        )
        print(f"Allowed types: {', '.join(TYPES)}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
