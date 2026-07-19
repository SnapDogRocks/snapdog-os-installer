# SnapDog OS Installer

Pure Rust desktop application for downloading and writing the correct
[SnapDog OS](https://github.com/SnapDogRocks/snapdog-os) image to an SD card.

The project targets macOS, Windows, and Linux on ARM64 and x86-64. It is currently in a
local-only development phase: no public installer artifacts are produced yet.

## Development

Rust 1.88 or newer is required.

```bash
cargo run
./scripts/check.sh
```

The current local milestone provides the complete branded image chooser, live stable/beta catalog
loading, and read-only discovery of removable physical drives. Boot and system drives are excluded
by the platform backends. The destructive privileged worker is intentionally not enabled yet; core
flash tests use ordinary temporary files and never write real disks.

See [Architecture](docs/architecture.md) for the safety model and remaining local milestones.

## Distribution plan

- macOS: signed and notarized universal DMG
- Windows: standalone x86-64 and ARM64 executables, prepared for Azure Artifact Signing
- Linux: x86-64 and ARM64 AppImages

No release workflow is enabled while destructive-device handling is still under local test.

## License

[GNU General Public License v3.0](LICENSE)
