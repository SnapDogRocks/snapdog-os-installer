#!/usr/bin/env python3
"""Fail closed unless both public SnapDog OS channels are installer-ready."""

from __future__ import annotations

import hashlib
import json
import re
import sys
import urllib.error
import urllib.parse
import urllib.request


BASE_URL = "https://updates.snapdog.cc/os/images/"
CHANNELS = ("release", "beta")
BOARDS = ("pi5", "pi4", "pi3", "zero2w")
SHA256 = re.compile(r"[0-9a-fA-F]{64}\Z")
MAX_MANIFEST_SIZE = 1024 * 1024
MAX_TEXT_FIELD_SIZE = 1024
U64_MAX = 2**64 - 1


def fail(message: str) -> None:
    raise ValueError(message)


def positive_integer(value: object, field: str) -> None:
    if (
        isinstance(value, bool)
        or not isinstance(value, int)
        or value <= 0
        or value > U64_MAX
    ):
        fail(f"{field} must be a positive 64-bit integer")


def digest(value: object, field: str) -> None:
    if not isinstance(value, str) or SHA256.fullmatch(value) is None:
        fail(f"{field} must be a 64-character SHA-256 value")


def bounded_text(value: object, field: str) -> str:
    if not isinstance(value, str) or not value or len(value) > MAX_TEXT_FIELD_SIZE:
        fail(f"{field} must be a non-empty bounded string")
    return value


def semver(value: object) -> str:
    version = bounded_text(value, "version")
    if version.count("+") > 1:
        fail("version is not valid SemVer")
    core_and_pre, separator, build = version.partition("+")
    if separator:
        validate_identifiers(build, allow_leading_zero=True)
    core, separator, prerelease = core_and_pre.partition("-")
    if separator:
        validate_identifiers(prerelease, allow_leading_zero=False)
    numbers = core.split(".")
    if len(numbers) != 3 or any(
        not number.isascii()
        or not number.isdigit()
        or (len(number) > 1 and number.startswith("0"))
        or (number.isdigit() and int(number) > U64_MAX)
        for number in numbers
    ):
        fail("version is not valid SemVer")
    return version


def validate_identifiers(value: str, *, allow_leading_zero: bool) -> None:
    identifiers = value.split(".")
    if any(
        not identifier
        or any(
            not character.isascii() or not (character.isalnum() or character == "-")
            for character in identifier
        )
        or (
            not allow_leading_zero
            and identifier.isdigit()
            and len(identifier) > 1
            and identifier.startswith("0")
        )
        for identifier in identifiers
    ):
        fail("version is not valid SemVer")


def immutable_image_url(value: object, board: str, version: str) -> None:
    if not isinstance(value, str):
        fail(f"{board}.url must be a string")
    parsed = urllib.parse.urlsplit(value)
    if (
        parsed.scheme != "https"
        or not parsed.hostname
        or parsed.username is not None
        or parsed.password is not None
        or parsed.query
        or parsed.fragment
    ):
        fail(f"{board}.url must be an unadorned absolute HTTPS URL")
    filename = urllib.parse.unquote(parsed.path.rsplit("/", maxsplit=1)[-1])
    expected = f"snapdog-os-{board}-{version}.img.gz"
    if filename != expected:
        fail(f"{board}.url must end in {expected!r}")


def validate_manifest(channel: str, manifest: object) -> None:
    if not isinstance(manifest, dict):
        fail("top-level JSON must be an object")
    if manifest.get("schema_version") != 2:
        fail("schema_version must be exactly 2")
    if manifest.get("channel") != channel:
        fail(f"channel must be exactly {channel!r}")
    version = semver(manifest.get("version"))
    bounded_text(manifest.get("date"), "date")
    boards = manifest.get("boards")
    if not isinstance(boards, dict):
        fail("boards must be an object")
    for board in BOARDS:
        image = boards.get(board)
        if not isinstance(image, dict):
            fail(f"missing image metadata for {board}")
        bounded_text(image.get("image"), f"{board}.image")
        immutable_image_url(image.get("url"), board, version)
        digest(image.get("sha256"), f"{board}.sha256")
        digest(image.get("raw_sha256"), f"{board}.raw_sha256")
        positive_integer(image.get("compressed_size"), f"{board}.compressed_size")
        positive_integer(image.get("uncompressed_size"), f"{board}.uncompressed_size")


def fetch(channel: str) -> object:
    url = f"{BASE_URL}latest-{channel}.json"
    request = urllib.request.Request(
        url,
        headers={
            "Accept": "application/json",
            "Cache-Control": "no-cache",
            "User-Agent": "snapdog-os-installer-release-preflight",
        },
    )
    with urllib.request.urlopen(request, timeout=20) as response:
        if response.status != 200:
            fail(f"HTTP {response.status} from {url}")
        final_url = urllib.parse.urlsplit(response.geturl())
        if final_url.scheme != "https" or not final_url.hostname:
            fail(f"manifest request was redirected away from HTTPS: {response.geturl()}")
        payload = response.read(MAX_MANIFEST_SIZE + 1)
    if len(payload) > MAX_MANIFEST_SIZE:
        fail(f"manifest exceeds {MAX_MANIFEST_SIZE} bytes")
    # Record a digest in the log without echoing the complete service response.
    print(f"{channel}: fetched {len(payload)} bytes ({hashlib.sha256(payload).hexdigest()})")
    return json.loads(payload)


def self_test() -> None:
    version = "1.2.3-beta.4+build.5"
    manifest: dict[str, object] = {
        "schema_version": 2,
        "channel": "release",
        "version": version,
        "commit": None,
        "date": "2026-07-19T00:00:00Z",
        "boards": {
            board: {
                "image": f"snapdog-os-{board}-release.img.gz",
                "url": f"{BASE_URL}snapdog-os-{board}-1.2.3-beta.4%2Bbuild.5.img.gz",
                "sha256": "a" * 64,
                "compressed_size": 42,
                "uncompressed_size": 84,
                "raw_sha256": "b" * 64,
            }
            for board in BOARDS
        },
    }
    validate_manifest("release", manifest)

    invalid_versions = (
        "1",
        "01.2.3",
        "1.2.3-01",
        "1.2.3+",
        "1.2.3_foo",
        f"{U64_MAX + 1}.0.0",
    )
    for invalid_version in invalid_versions:
        try:
            semver(invalid_version)
        except ValueError:
            continue
        fail(f"self-test accepted invalid SemVer {invalid_version!r}")

    for missing_field in ("date",):
        broken = dict(manifest)
        del broken[missing_field]
        try:
            validate_manifest("release", broken)
        except ValueError:
            continue
        fail(f"self-test accepted missing field {missing_field!r}")

    broken = json.loads(json.dumps(manifest))
    del broken["boards"]["pi4"]["image"]
    try:
        validate_manifest("release", broken)
    except ValueError:
        pass
    else:
        fail("self-test accepted a board without the required image field")

    broken = json.loads(json.dumps(manifest))
    broken["boards"]["pi4"]["compressed_size"] = U64_MAX + 1
    try:
        validate_manifest("release", broken)
    except ValueError:
        pass
    else:
        fail("self-test accepted an image size larger than u64")


def main() -> int:
    try:
        self_test()
        if sys.argv[1:] == ["--self-test"]:
            print("release manifest preflight self-test passed")
            return 0
        if sys.argv[1:]:
            fail("usage: check-live-manifests.py [--self-test]")
        for channel in CHANNELS:
            validate_manifest(channel, fetch(channel))
            print(f"{channel}: installer metadata is ready")
    except (OSError, ValueError, json.JSONDecodeError, urllib.error.URLError) as error:
        print(f"release manifest preflight failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
