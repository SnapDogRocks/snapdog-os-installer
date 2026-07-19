# Architecture and safety model

SnapDog OS Installer is one Rust executable with two runtime modes:

1. The unprivileged `egui` application selects an OS image and exactly one removable target.
2. The same executable re-launches in a narrowly scoped privileged worker mode for unmounting
   and writing that selected target. There is no separately shipped sidecar.

The macOS worker boundary is connected for local testing. The automated pipeline remains fully
file-backed and never opens raw devices; a real device is reachable only through the explicit UI
selection, erase confirmation, native administrator prompt, root gate, and final worker-side target
revalidation.

## Image pipeline

- Load stable and beta release manifests without downloading image archives.
- Select the newest available version by default.
- Download only after the user presses **Flash**.
- Verify compressed size and SHA-256 before opening the target.
- Decompress to a private temporary raw file with a bounded buffer; verify its size and SHA-256.
- In the privileged worker, copy the prepared image into an unlinked, root-owned staging file and
  verify it again before enumerating the target. The worker writes from that same staged descriptor,
  preventing path replacement or same-inode mutation from changing bytes after validation.
- Refuse images larger than the reported target capacity before requesting privileges.
- Verify written bytes by default; an explicit skip produces a visible **Not verified** result.

Release manifest v2 supplies immutable versioned image URLs, compressed and uncompressed sizes,
and hashes for both representations while retaining the v1 fields used by existing clients.

## Target safety

- Exactly one whole physical drive can be selected.
- macOS intersects `diskutil list -plist physical` results with I/O Registry media marked exactly
  whole, writable, removable, and ejectable. This includes Apple's built-in SDXC reader while
  excluding internal fixed disks and virtual disk images.
- Linux queries removable whole devices in `/sys/block`.
- Windows queries USB/SD disks and rejects `IsBoot` and `IsSystem` disks.
- Device identifiers are syntax-checked before they can cross the worker boundary.
- macOS carries the whole-media `IORegistryEntryID` captured at selection into the worker, then
  repeats the independent `diskutil`/I/O Registry intersection immediately before writing and after
  opening the raw descriptor. Device path, capacity, I/O Registry identity, and every removable
  physical-media safety flag must match.
- Cancellation is checked between every buffered read/write operation.
- Once a target has been unmounted, all failure and cancellation paths attempt a safe eject.
- Closing the GUI during an operation requests a safe stop and waits for the worker to finish.

## Privilege boundary

- Raw-device mode requires an explicit root-only environment gate and the exact worker CLI shape.
- The elevated launcher copies the signed `.app` to a root-owned temporary directory and verifies
  the SnapDog Developer ID designated requirement before executing its embedded worker binary.
- Job, image, progress, cancel, and skip paths must be fixed-name members of one private session
  directory with expected ownership, permissions, link count, file size, and stable directory
  identity. The progress descriptor is revalidated before root truncates it.
- Monitoring errors request cancellation and still wait for the authorization/worker process to
  exit, so temporary cancellation state is not removed while a root worker could remain active.

## Packaging

- macOS: one universal application (`arm64` + `x86_64`) in a signed, hardened, notarized DMG.
- Windows: separate ARM64 and x86-64 executables, prepared for Azure Artifact Signing through
  GitHub OIDC.
- Linux: separate ARM64 and x86-64 AppImages.

Release automation remains disabled until local removable-media write and verification tests have
passed on every platform.
