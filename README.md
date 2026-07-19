# SnapDog OS Installer

[![CI](https://github.com/SnapDogRocks/snapdog-os-installer/actions/workflows/ci.yml/badge.svg)](https://github.com/SnapDogRocks/snapdog-os-installer/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/SnapDogRocks/snapdog-os-installer?display_name=tag)](https://github.com/SnapDogRocks/snapdog-os-installer/releases)
[![License: GPL-3.0](https://img.shields.io/badge/license-GPL--3.0-blue.svg)](LICENSE)

A focused, native desktop application for installing
[SnapDog OS](https://github.com/SnapDogRocks/snapdog-os) on a Raspberry Pi SD card.

Pick a Raspberry Pi model and a stable or beta SnapDog OS release, select exactly one removable
target, and flash it. The image is downloaded only after confirmation, checked against the release
metadata, written by a narrowly scoped administrator worker, verified by default, synced,
and safely ejected or left unmounted.

## Downloads

Each tagged release is built as one Rust executable per platform package:

- macOS: signed and notarized universal DMG (`arm64` + `x86_64`)
- Windows: standalone `arm64` and `x86_64` executables
- Linux: standalone `arm64` and `x86_64` AppImages

Release assets include SHA-256 checksums and GitHub build-provenance attestations. Windows release
signing is ready for Azure Artifact Signing and can be enabled once the SnapDog signing account is
provisioned; until then the Windows executables are clearly published unsigned.
The GPL license and generated third-party notices are embedded in every package and available from
the application's Settings screen; the Windows executables retain the one-file distribution model.

## Safety model

The graphical application never writes a raw device directly. The same binary is re-entered through
the platform's native authorization mechanism, receives a fixed-shape job, and independently
revalidates the selected whole physical device immediately before destructive access. Stable media
identity prevents a remove/reinsert race from redirecting a queued job.

- macOS uses a Developer ID-verified privileged copy, `diskutil`, and I/O Registry media identity.
- Linux uses PolicyKit, kernel `diskseq`, sysfs device identity, and mount/swap/holder protection.
  Linux kernel 5.15 or newer is required so path reuse can be detected without an unsafe fallback.
- Windows uses trusted System32 PowerShell modules plus UAC, requires positive SD/MMC or removable
  USB identity, and verifies through sector-aligned unbuffered reads after an exclusive
  write-through physical-disk write. Boot, system, read-only, fixed, and ambiguous USB disks are
  rejected.

The automated tests use ordinary temporary files and never open a real device. See
[Architecture and safety model](docs/architecture.md) for the complete boundary and failure model,
and [Physical platform validation](docs/platform-validation.md) for the mandatory disposable-media
release checklist.

## Development

Rust 1.88 or newer is required.

```bash
cargo run
./scripts/check.sh
```

`scripts/check.sh` enforces formatting, all/pedantic/nursery Clippy lints, warnings-as-errors,
tests, the Rust 1.88 minimum version, and mandatory license, advisory, and dependency auditing.

Platform packages are produced with:

```bash
./scripts/package-macos.sh
./scripts/package-linux.sh x86_64-unknown-linux-gnu
pwsh ./scripts/package-windows.ps1 -Target x86_64-pc-windows-msvc
```

The macOS script requires the Developer ID and App Store Connect API values documented in the
release workflow. Linux AppImages must be assembled natively on the matching architecture. Windows
packages must be built from an MSVC developer shell.

## Releasing

The package version in `Cargo.toml` is authoritative. Pushing an exact matching tag such as `v0.1.0`
builds all five packages, signs and notarizes macOS, optionally signs Windows through GitHub OIDC,
checks the complete asset set, creates checksums and attestations, and publishes one GitHub release.
See [Release configuration](docs/releasing.md).

## License

[GNU General Public License v3.0](LICENSE)
