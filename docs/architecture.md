# Architecture and safety model

SnapDog OS Installer is one Rust executable with two runtime modes:

1. The unprivileged `egui` application selects an OS image and exactly one removable target.
2. The same executable will re-launch in a narrowly scoped privileged worker mode for unmounting
   and writing that selected target. There is no separately shipped sidecar.

The worker boundary is deliberately not connected in the first local milestone. The current flash
pipeline is exercised only against ordinary temporary files.

## Image pipeline

- Load stable and beta release manifests without downloading image archives.
- Select the newest available version by default.
- Download only after the user presses **Flash**.
- Verify the compressed archive SHA-256 before opening the target.
- Decompress with a bounded buffer and refuse writes beyond the reported target capacity.
- Verify written bytes by default; an explicit skip produces a visible **Not verified** result.

The release manifest should grow immutable versioned image URLs, compressed and uncompressed
sizes, and a raw-image SHA-256 before the privileged writer is enabled.

## Target safety

- Exactly one whole physical drive can be selected.
- macOS queries only `diskutil list -plist external physical` results.
- Linux queries removable whole devices in `/sys/block`.
- Windows queries USB/SD disks and rejects `IsBoot` and `IsSystem` disks.
- Device identifiers are syntax-checked before they can cross the worker boundary.
- The worker must re-enumerate the target immediately before writing and compare its stable
  identity and capacity with the user's selection.
- Cancellation is checked between every buffered read/write operation.

## Packaging

- macOS: one universal application (`arm64` + `x86_64`) in a signed, hardened, notarized DMG.
- Windows: separate ARM64 and x86-64 executables, prepared for Azure Artifact Signing through
  GitHub OIDC.
- Linux: separate ARM64 and x86-64 AppImages.

Release automation remains disabled until local removable-media write and verification tests have
passed on every platform.
