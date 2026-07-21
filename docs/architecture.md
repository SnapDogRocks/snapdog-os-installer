# Architecture and safety model

SnapDog OS Installer is one Rust executable with two runtime modes:

1. The unprivileged `egui` application selects one OS image and exactly one removable target.
2. The same executable re-enters as an isolated worker. macOS passes it one descriptor authorized
   by `authopen`, Linux obtains scoped descriptors and operations from UDisks2, and Windows uses
   UAC for the minimum raw-device operations.

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
- The isolated worker treats every serialized path as untrusted. It copies the prepared raw image
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

- **macOS:** IOKit media marked exactly whole, writable, removable, ejectable, and physically
  backed are cross-checked against Disk Arbitration. File-backed HDI ancestry is rejected. The
  whole-media `IORegistryEntryID` survives the privilege boundary and detects device-path reuse,
  including Apple's built-in SDXC reader.
- **Linux:** `/sys/block` must report a writable removable whole block device with a positive kernel
  `diskseq`. Device numbers, partitions, transitive holders, protected mounts, and swap are checked;
  UDisks2 returns the PolicyKit-authorized raw descriptor, which is revalidated against sysfs before
  writing. Synchronous writes are followed by a newly authorized, aligned `O_DIRECT` descriptor for
  cache-bypassing verification.
- **Windows:** the Storage Management WMI provider must report a writable, online SD/MMC device, or
  a USB device whose `Win32_DiskDrive` capabilities positively identify removable media. Ordinary
  USB HDDs and SSDs are excluded even when externally attached. A fingerprint binds disk number,
  capacity, logical and physical sector geometry, bus, device path, unique ID, serial number, and
  removable capability. Native volume enumeration and disk-extent IOCTLs identify the target's
  filesystems; each volume is locked and dismounted with `FSCTL_LOCK_VOLUME` and
  `FSCTL_DISMOUNT_VOLUME`, and the physical drive is then opened exclusively with write-through
  semantics. Verification reopens the target with unbuffered, sector-aligned reads. The locked
  volume handles remain alive until cleanup, so a pre-write failure restores normal access simply
  by closing them.

## Privilege boundary

- Raw-device mode requires an exact, digest-bound worker CLI. Windows additionally proves
  Administrator identity. macOS and Linux use launcher-only accidental-write opt-ins, while the
  operating system independently authorizes the exact raw-device descriptor or operation.
- Job, raw image, progress, cancel, and verification-skip paths are fixed-name members of one
  private session directory.
- Unix validates directory owner, mode, parent identity, link count, file type, size, and stable
  device/inode identity. Linux also permits the standard root-owned sticky `/tmp` parent shape.
- Windows rejects reparse points and pins the session directory and opened files with native file
  identity handles across the UAC hand-off.
- Progress records and helper diagnostics are size-bounded. Monitoring failures create a durable
  cancel marker and wait for the worker so temporary state cannot disappear under a privileged
  process.
- macOS validates its Developer ID requirement with Security.framework, binds the job to a SHA-256
  digest, and uses the system `authopen` broker to return only the selected raw descriptor. Linux
  directly re-enters the pinned executable with a digest-bound job; UDisks2 then performs PolicyKit
  checks for `OpenDevice`, `Unmount`, and `PowerOff`/`Eject` over the system D-Bus. Windows invokes
  UAC directly through `ShellExecuteExW`, with a constant executable and separately quoted native
  arguments. No platform invokes a command shell at runtime.

## Tests and release gates

Unit and integration tests write only to ordinary files. Platform worker modules additionally test
identity parsing, hotplug/path-reuse rejection, progress framing, and native API request handling.
A release requires strict Clippy, tests on Linux/macOS/Windows, the pinned Rust toolchain, dependency
audit, native ARM64/x86-64 package builds, a notarized universal DMG, the exact five-asset set,
checksums, and build-provenance attestations.

A physical-media end-to-end test is intentionally manual and must use an explicitly designated
disposable SD card; the test suite never infers permission to write an attached device.
