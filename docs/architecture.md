# Architecture and safety model

SnapDog OS Installer is one Rust executable with two runtime modes:

1. The unprivileged `egui` application selects one OS image and exactly one removable target.
2. The same executable re-enters through the native administrator flow to perform the minimum
   privileged operations required to unmount, write, verify, sync, and eject that target.

There is no separately shipped sidecar. macOS, Linux, and Windows all use the same serialized job
and JSON-lines progress protocol, while target discovery, elevation, and raw-device handling remain
small platform modules.

## Image pipeline

- Stable and beta manifests are loaded without downloading image archives.
- The newest compatible version is selected by default.
- Download starts only after the user confirms the destructive operation.
- The compressed size and SHA-256 are checked while streaming to a private temporary file.
- The archive is decompressed with a bounded buffer; raw size and SHA-256 are checked before
  administrator authorization is requested.
- The privileged worker treats every serialized path as untrusted. It copies the prepared raw image
  into an unlinked staging file and hashes it again before enumerating the target.
- The target must be large enough before elevation. Written bytes are verified by default; an
  explicit skip produces a visible **Not verified** result.

Release manifest v2 supplies immutable, versioned HTTPS URLs plus compressed and raw sizes and
hashes. Schema v1 remains readable for catalog compatibility but cannot enter the destructive path
without the v2 integrity fields.

## Target safety

Common invariants:

- only one whole physical device can be selected;
- boot/system, fixed, virtual, read-only, mounted system, and otherwise ambiguous devices fail
  closed;
- every device identifier is syntax-checked and all raw paths are derived internally;
- target identity and capacity are revalidated before unmount, after unmount, and after the raw
  descriptor is opened;
- cancellation is checked between bounded reads and writes;
- every exit after destructive access best-effort syncs the open target and then attempts the
  platform cleanup path. Cleanup failures never hide the primary write, verification, or cancel
  result; on platforms/controllers without power-off support the card can remain safely unmounted
  rather than appearing as successfully ejected.

Platform identity:

- **macOS:** `diskutil list -plist physical` is intersected with I/O Registry media marked exactly
  whole, writable, removable, and ejectable. The whole-media `IORegistryEntryID` survives the
  privilege boundary and detects device-path reuse, including Apple's built-in SDXC reader.
- **Linux:** `/sys/block` must report a writable removable whole block device with a positive kernel
  `diskseq`. Device numbers, partitions, transitive holders, protected mounts, and swap are checked;
  the opened block descriptor is revalidated against sysfs before writing.
- **Windows:** `Get-Disk` must report a writable, online SD/MMC device, or a USB device whose
  physical-drive capabilities positively identify removable media. Ordinary USB HDDs and SSDs are
  excluded even when externally attached. A fingerprint binds disk number, capacity, logical and
  physical sector geometry, bus, device path, unique ID, serial number, and removable capability.
  User access paths are removed without force-closing application handles, target volumes are
  dismounted, and the physical drive is then opened exclusively with write-through semantics.
  Verification reopens the target with unbuffered, sector-aligned reads. The implementation never
  uses `Set-Disk -IsOffline`; access-path restoration is attempted only when dismount fails before
  a write begins.

## Privilege boundary

- Raw-device mode requires an exact, digest-bound worker CLI. The worker additionally proves root
  or Administrator identity. macOS uses a launcher-only environment opt-in; Windows uses a fixed
  CLI opt-in; Linux deliberately transports neither arbitrary environment nor a generic helper
  command through PolicyKit.
- Job, raw image, progress, cancel, and verification-skip paths are fixed-name members of one
  private session directory.
- Unix validates directory owner, mode, parent identity, link count, file type, size, and stable
  device/inode identity. Linux also permits only the root-owned sticky `/tmp` parent shape.
- Windows rejects reparse points and pins the session directory and opened files with native file
  identity handles across the UAC hand-off.
- Progress records and helper diagnostics are size-bounded. Monitoring failures create a durable
  cancel marker and wait for the worker so temporary state cannot disappear under a privileged
  process.
- macOS copies the signed application into a root-owned directory, verifies its Developer ID
  designated requirement, and binds the worker job to a SHA-256 digest across the administrator
  prompt. Linux invokes a constant program through a validated, root-owned `pkexec`/`bash` toolchain;
  that program copies the pre-hashed executable into a private root-owned directory, verifies the
  copy, and passes the digest-bound job to it. Windows resolves PowerShell and required modules from
  trusted System32 locations, clears inherited environment state, and invokes UAC with a constant
  program and separately quoted native arguments.

## Tests and release gates

Unit and integration tests write only to ordinary files. Platform worker modules additionally test
identity parsing, hotplug/path-reuse rejection, progress framing, and command argument construction.
A release requires strict Clippy, tests on Linux/macOS/Windows, Rust 1.88 compatibility, dependency
audit, native ARM64/x86-64 package builds, a notarized universal DMG, the exact five-asset set,
checksums, and build-provenance attestations.

A physical-media end-to-end test is intentionally manual and must use an explicitly designated
disposable SD card; the test suite never infers permission to write an attached device.
