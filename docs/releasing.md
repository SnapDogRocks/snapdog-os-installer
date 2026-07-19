# Release configuration

Release Please watches conventional commits on `main` and maintains one release pull request. That
pull request updates `Cargo.toml`, `Cargo.lock`, `.release-please-manifest.json`, and `CHANGELOG.md`.
It is deliberately never auto-merged: merge it only after the disposable-media checklist and the
public manifest preflight are ready for the proposed version.

Merging the release pull request creates an exact `v<version>` tag and a private draft GitHub
release. The tag starts the package workflow; the draft is published only after all five packages,
checksums, attestations, signing, and notarization succeed. A mismatched tag, or a tag whose commit
is not contained in `origin/main`, fails before package jobs start. Manually pushing an exact tag
remains a recovery path and creates the release if no Release Please draft exists.

`TAP_TOKEN` is a repository Actions secret containing a GitHub token with access to this repository
and permission to create pull requests, tags, and releases. A separate token is required because
events created with the workflow's built-in `GITHUB_TOKEN` do not start subsequent workflows. The
Release Please action is commit-pinned, and its token is not exposed to build or packaging jobs.

The same gate fetches both public SnapDog OS channel manifests and requires schema v2 metadata for
all four supported boards: immutable versioned HTTPS URLs, compressed and raw sizes, and both
SHA-256 digests. This is a semantic JSON check, so harmless page/content changes do not affect it;
an installer release cannot publish while either live channel would be unflashable.

## Required macOS secrets

- `MACOS_CERT_P12`: base64-encoded Developer ID Application PKCS#12 content
- `MACOS_CERT_PASSWORD`: PKCS#12 password
- `APPLE_API_KEY_CONTENT`: App Store Connect API `.p8` content
- `APPLE_API_KEY`: App Store Connect key ID
- `APPLE_API_ISSUER`: App Store Connect issuer ID

The same variable names may be supplied in `~/.env_vars` for a local macOS package build. The
workflow builds both Rust targets, creates one universal binary, signs the hardened application and
DMG, waits for notarization, staples the ticket, and validates it with both `stapler` and Gatekeeper.
Apple credentials are exposed only to that packaging step. The local script imports only these five
named values and removes them from the exported environment before Cargo, build scripts, and DMG
tooling run.

The GitHub `release` environment requires approval by its configured reviewer and accepts only
deployments originating from `v*` tags. Keep those protections enabled; they are part of the signing
boundary, not optional repository decoration.

Windows Azure configuration is documented in [windows-signing.md](windows-signing.md).

## Published asset contract

For version `X.Y.Z`, publication requires exactly:

- `snapdog-os-installer-X.Y.Z-linux-x86_64.AppImage`
- `snapdog-os-installer-X.Y.Z-linux-aarch64.AppImage`
- `snapdog-os-installer-X.Y.Z-windows-x86_64.exe`
- `snapdog-os-installer-X.Y.Z-windows-aarch64.exe`
- `snapdog-os-installer-X.Y.Z-macos-universal.dmg`

The publish job then adds `SHA256SUMS`, creates GitHub build-provenance attestations, and publishes
generated release notes. Failed, missing, empty, duplicated, or unexpected package artifacts stop
the release.

Windows ARM64 is cross-compiled by the native MSVC ARM64 toolchain on an x86-64 GitHub runner. Its
compile, lint, static-runtime, and PE dependency gates are automated, but the resulting application
must still pass the native Windows-on-ARM disposable-media checklist before the first public tag.
