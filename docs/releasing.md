# Release configuration

Release Please watches conventional commits on `main` and maintains one release pull request. That
pull request updates `Cargo.toml`, `Cargo.lock`, `.release-please-manifest.json`, and `CHANGELOG.md`.
It is deliberately never auto-merged: merge it only after the disposable-media checklist and the
public manifest preflight are ready for the proposed version.

Merging the release pull request creates an exact `v<version>` tag and a private draft GitHub
release. Release Please then dispatches `release.yml` against that immutable tag. The release
workflow has no tag trigger: Release Please is the sole production tag authority. A mismatched tag,
or a production tag whose commit is not contained in `origin/main`, fails before package jobs start.

Release automation uses a dedicated GitHub App. `RELEASE_PLEASE_CLIENT_ID` is a non-secret variable
and `RELEASE_PLEASE_APP_PRIVATE_KEY` is a secret in the protected `release` environment. Each job
mints a narrowly scoped, short-lived installation token; no long-lived personal access token is
used. All third-party Actions are pinned to immutable commit SHAs.

The same gate fetches both public SnapDog OS channel manifests and requires schema v2 metadata for
all four supported boards: immutable versioned HTTPS URLs, compressed and raw sizes, and both
SHA-256 digests. This is a semantic JSON check, so harmless page/content changes do not affect it;
an installer release cannot publish while either live channel would be unflashable.

## Required release environment configuration

- Secret `RELEASE_PLEASE_APP_PRIVATE_KEY`: GitHub App private key
- Variable `RELEASE_PLEASE_CLIENT_ID`: GitHub App client ID
- Secret `APPLE_CERT_P12_BASE64`: base64-encoded Developer ID Application PKCS#12 content
- Secret `APPLE_CERT_PASSWORD`: PKCS#12 password
- Secret `APPLE_API_KEY_CONTENT`: App Store Connect API `.p8` content
- Variable `APPLE_API_KEY`: App Store Connect key ID
- Variable `APPLE_API_ISSUER`: App Store Connect issuer ID

The packager's legacy local names (`MACOS_CERT_P12`, `MACOS_CERT_PASSWORD`,
`APPLE_API_KEY_CONTENT`, `APPLE_API_KEY`, and `APPLE_API_ISSUER`) may be supplied in `~/.env_vars`
for a local macOS package build. The
workflow builds both Rust targets, creates one universal binary, signs the hardened application and
DMG, waits for notarization, staples the ticket, and validates it with both `stapler` and Gatekeeper.
Apple credentials are exposed only to that packaging step. The local script imports only these five
named values and removes them from the exported environment before Cargo, build scripts, and DMG
tooling run.

The GitHub `release` environment stores the release and Apple signing secrets and accepts only
deployments from `main` or `v*` tags. It intentionally has no required-reviewer rule, so release
automation, signing, staging, and promotion proceed without a manual approval pause.

Windows Azure configuration is documented in [windows-signing.md](windows-signing.md).

## Published asset contract

For version `X.Y.Z`, publication requires exactly:

- `snapdog-os-installer-X.Y.Z-linux-x86_64.AppImage`
- `snapdog-os-installer-X.Y.Z-linux-aarch64.AppImage`
- `snapdog-os-installer-X.Y.Z-windows-x86_64.exe`
- `snapdog-os-installer-X.Y.Z-windows-aarch64.exe`
- `snapdog-os-installer-X.Y.Z-macos-universal.dmg`

Each payload additionally requires an adjacent `.spdx.json` SBOM and `.sigstore.json` keyless
signature bundle. `SHA256SUMS` covers all fifteen payload and sidecar files, and GitHub
build-provenance attestations cover all sixteen published files. Failed, missing, empty, duplicated,
or unexpected artifacts stop the release.

The complete candidate is uploaded to a draft and exposed as a prerelease. The workflow downloads
the public bytes into a clean directory, enforces the exact asset contract, compares them byte for
byte with the candidate, verifies all checksums, Sigstore identities, and GitHub attestations, and
only then promotes a production tag unchanged to stable and latest. Test tags use
`v<current-version>-<suffix>` and permanently remain prereleases; they never update release
channels or downstream consumers.

## Website release handoff

After the GitHub release has passed every package, signing, notarization, SBOM, signature, checksum,
attestation, public-download, and asset-contract gate and has been promoted, the final step dispatches
`update-installer-release.yml` on `SnapDogRocks/snapdog-web`. That workflow validates the published
asset set, updates the shared download metadata and version number, verifies the website, and opens
an auto-merge pull request. The installer workflow waits for that downstream workflow and then reads
the metadata back from `snapdog-web` main. Draft, prerelease-test, or failed installer releases never
trigger the website update.

The release GitHub App must be installed on both repositories with permission to dispatch workflows
in `SnapDogRocks/snapdog-web`. The source workflow mints a target-repository token only after stable
promotion; it never exposes that token to build jobs.

Windows ARM64 is cross-compiled by the native MSVC ARM64 toolchain on an x86-64 GitHub runner. Its
compile, lint, static-runtime, and PE dependency gates are automated, but the resulting application
must still pass the native Windows-on-ARM disposable-media checklist before the first public tag.
