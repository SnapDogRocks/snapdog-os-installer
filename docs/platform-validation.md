# Physical platform validation

Automated tests never open a block device. Before the first public release, run this checklist on a
clearly labeled disposable SD card whose contents may be destroyed. Record installer version,
package SHA-256, host OS version, architecture, card reader, card identity/capacity, and result.

Never infer a disposable target from its size or position in a device list. The tester must name and
confirm the exact medium before each destructive run.

## Every platform and architecture

1. Confirm the internal system disk, virtual disks, fixed external disks, and mounted system media
   never appear as selectable targets.
2. Select the disposable card, remove and reinsert it before confirmation, and verify that the stale
   selection is rejected.
3. Complete a stable-channel flash with verification and boot the matching Raspberry Pi.
4. Complete a beta-channel flash with verification skipped; verify the result is visibly marked
   **Not verified**.
5. Cancel during download/preparation and confirm the target was untouched.
6. Cancel during write and confirm the app waits for a safe boundary and reports that the card is
   incomplete.
7. Keep a file on the target open and confirm the operation fails safely instead of forcing a
   dismount.
8. Disconnect the target during validation, writing, verification, and final ejection in separate
   runs; each must fail without redirecting writes to another device.
9. Exercise retry, Flash another, close-during-operation, offline catalog, corrupt archive, raw hash
   mismatch, too-small target, and automatic-eject failure paths.

## macOS

- Test the notarized universal DMG natively on Apple Silicon and Intel.
- Confirm Gatekeeper accepts the app and the administrator dialog names the signed SnapDog app.
- Test both a built-in SDXC reader and a USB reader where available.
- Confirm the card disappears from Disk Utility/Finder after completion and can be removed safely.

## Linux

- Confirm the host runs Linux kernel 5.15 or newer and exposes a positive
  `/sys/block/<device>/diskseq`; older kernels are deliberately unsupported because they cannot
  provide the required hot-plug identity.
- Confirm the packaged executable needs no symbol newer than GLIBC 2.28. Test each native AppImage
  on both an older GLIBC 2.28 userland running a supported kernel and a current desktop.
- Confirm the outer AppImage successfully re-enters through PolicyKit; the private FUSE mount must
  not cause an executable-permission failure.
- Test common `/media/...` and `/run/media/...` automount layouts and verify unexpected mounts such
  as `/home`, `/var`, `/root`, `/nix`, or `/srv` are refused.
- Test systems with and without `udisksctl`/`eject`; a synced and unmounted card remains a successful
  write even when automatic power-off is unavailable.
- Confirm `blockdev --flushbufs` is available from util-linux and that verified writes perform a
  true post-flush readback rather than accepting cached write pages.

## Windows

- Test both native x86-64 and ARM64 packages on supported Windows 11 hardware.
- Confirm UAC names SnapDog OS Installer (and the verified publisher once Azure signing is enabled),
  not PowerShell.
- Test native SD/MMC readers and USB readers whose physical device reports removable-media
  capability. Ordinary USB HDDs/SSDs and readers exposed only as fixed media must not appear.
- Confirm every target volume dismounts without `Set-Disk -IsOffline`, busy volumes fail closed, and
  the exclusive write-through PhysicalDrive handle succeeds on a normal removable SD card.
- Confirm verification uses sector-aligned, unbuffered reads and detects deliberate post-write
  corruption, including an image whose byte length is not a whole physical sector.
- Confirm `dumpbin /dependents` reports no dynamic VC/UCRT redistributable dependency for either
  release executable.
- Confirm no console windows flash during discovery, authorization, writing, or ejection.

The first release tag should be created only after the matrix has an explicit recorded pass or a
documented, user-visible limitation accepted for that release.
