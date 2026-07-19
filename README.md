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

The current local milestone provides the branded image chooser, live stable/beta catalog loading,
read-only discovery of removable physical drives, and the complete macOS flash path. The image is
downloaded only after confirmation, validated and decompressed before authorization, written by a
narrow same-binary root worker, verified by default, synchronized, and safely ejected. Boot and
system drives are excluded, and the insertion-lifetime I/O Registry identity is revalidated before
and after opening the raw device.

`cargo run` is suitable for UI and non-destructive development. Raw-device writing requires the
Developer ID-signed application bundle produced by `scripts/package-macos.sh`; the privileged
launcher copies that bundle into a root-owned directory and verifies its designated requirement
before executing worker mode.

The automated suite uses ordinary temporary files and never opens a real device. Public release
automation remains disabled until the destructive path has also passed an explicit local test on a
disposable SD card.

See [Architecture](docs/architecture.md) for the safety model and remaining local milestones.

## Distribution plan

- macOS: signed and notarized universal DMG
- Windows: standalone x86-64 and ARM64 executables, prepared for Azure Artifact Signing
- Linux: x86-64 and ARM64 AppImages

No release workflow is enabled while destructive-device handling is still under local test.
macOS is the first complete platform implementation; Windows and Linux privileged workers remain
local roadmap items before their packages can be published.

## License

[GNU General Public License v3.0](LICENSE)
