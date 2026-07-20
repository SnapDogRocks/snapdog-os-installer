// SPDX-License-Identifier: GPL-3.0-only

//! Windows raw-disk implementation for the privileged worker.
//!
//! Elevation and validation of the worker-job/session files are deliberately handled by the
//! parent worker integration. This module owns only physical-disk identity validation and the
//! destructive disk operations through native Windows handles and storage control codes.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::windows::fs::OpenOptionsExt;

use aligned_vec::{AVec, RuntimeAlign};
use sha2::{Digest, Sha256};

use super::{WorkerDrive, WorkerError, WorkerPlatform, WorkerTarget, compare_drive};

const PHYSICAL_DRIVE_PREFIX: &str = r"\\.\PHYSICALDRIVE";
const STABLE_ID_VERSION: &str = "windows-disk-v2";
const FILE_FLAG_WRITE_THROUGH: u32 = 0x8000_0000;
const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
const FILE_SHARE_READ: u32 = 0x1;
const FILE_SHARE_WRITE: u32 = 0x2;
const VERIFICATION_BUFFER_SIZE: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BoolFlag(bool);

impl BoolFlag {
    const fn is_set(self) -> bool {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DiskSnapshot {
    number: u32,
    path: String,
    unique_id: String,
    serial_number: String,
    size: u64,
    logical_sector_size: u32,
    physical_sector_size: u32,
    is_boot: BoolFlag,
    is_system: BoolFlag,
    is_offline: BoolFlag,
    is_read_only: BoolFlag,
    bus_type: String,
    supports_removable_media: BoolFlag,
}

impl DiskSnapshot {
    fn fingerprint(&self) -> Result<String, WorkerError> {
        if self.path.is_empty()
            || (self.unique_id.is_empty() && self.serial_number.is_empty())
            || self.size == 0
            || !valid_sector_geometry(
                self.size,
                self.logical_sector_size,
                self.physical_sector_size,
            )
        {
            return Err(WorkerError::UnsafeTarget);
        }
        let number = self.number.to_string();
        let size = self.size.to_string();
        let logical_sector_size = self.logical_sector_size.to_string();
        let physical_sector_size = self.physical_sector_size.to_string();
        let supports_removable_media = self.supports_removable_media.is_set().to_string();
        let mut digest = Sha256::new();
        for field in [
            STABLE_ID_VERSION.as_bytes(),
            number.as_bytes(),
            size.as_bytes(),
            logical_sector_size.as_bytes(),
            physical_sector_size.as_bytes(),
            self.bus_type.as_bytes(),
            self.path.as_bytes(),
            self.unique_id.as_bytes(),
            self.serial_number.as_bytes(),
            supports_removable_media.as_bytes(),
        ] {
            let length = u64::try_from(field.len()).map_err(|_| WorkerError::UnsafeTarget)?;
            digest.update(length.to_le_bytes());
            digest.update(field);
        }
        Ok(hex::encode(digest.finalize()))
    }

    fn is_card_target(&self) -> bool {
        match self.bus_type.as_str() {
            "SD" | "MMC" => true,
            "USB" => self.supports_removable_media.is_set(),
            _ => false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SelectedDisk {
    number: u32,
    device: String,
    fingerprint: String,
}

impl SelectedDisk {
    fn parse(selected: &WorkerDrive) -> Result<Self, WorkerError> {
        let (device, fingerprint) = selected
            .id
            .rsplit_once('@')
            .ok_or(WorkerError::UnsafeTarget)?;
        let suffix = device
            .strip_prefix(PHYSICAL_DRIVE_PREFIX)
            .ok_or(WorkerError::UnsafeTarget)?;
        if suffix.is_empty()
            || !suffix.bytes().all(|byte| byte.is_ascii_digit())
            || fingerprint.len() != 64
            || !fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit())
            || selected.device != device
            || selected.capacity == 0
        {
            return Err(WorkerError::UnsafeTarget);
        }
        let number = suffix.parse().map_err(|_| WorkerError::UnsafeTarget)?;
        let canonical_device = format!(r"\\.\PHYSICALDRIVE{number}");
        if canonical_device != device {
            return Err(WorkerError::UnsafeTarget);
        }
        Ok(Self {
            number,
            device: canonical_device,
            fingerprint: fingerprint.to_ascii_lowercase(),
        })
    }
}

/// Raw-disk handle that switches from a flushed buffered writer to an unbuffered reader before
/// verification. `FILE_FLAG_WRITE_THROUGH` makes writes durable; the separate
/// `FILE_FLAG_NO_BUFFERING` handle guarantees readback bypasses the Windows system cache.
#[derive(Debug)]
pub struct WindowsTarget {
    writer: Option<File>,
    verifier: Option<UnbufferedVerifier>,
}

impl WindowsTarget {
    const fn new(writer: File) -> Self {
        Self {
            writer: Some(writer),
            verifier: None,
        }
    }

    fn begin_verification(
        &mut self,
        device: &str,
        capacity: u64,
        logical_sector_size: u32,
        physical_sector_size: u32,
        expected_identity: &crate::windows_native::OpenDiskIdentity,
    ) -> io::Result<()> {
        self.sync_all()?;
        drop(self.writer.take());
        let file = open_unbuffered_target(device)?;
        let reopened_identity = crate::windows_native::query_open_disk(&file)?;
        validate_open_identity(expected_identity, &reopened_identity)
            .map_err(|error| io::Error::other(error.to_string()))?;
        self.verifier = Some(UnbufferedVerifier::new(
            file,
            capacity,
            logical_sector_size,
            physical_sector_size,
        )?);
        Ok(())
    }

    fn open_identity(&self) -> io::Result<crate::windows_native::OpenDiskIdentity> {
        match (&self.writer, &self.verifier) {
            (Some(writer), _) => crate::windows_native::query_open_disk(writer),
            (None, Some(verifier)) => crate::windows_native::query_open_disk(&verifier.file),
            (None, None) => Err(io::Error::other("Windows target handle is unavailable")),
        }
    }

    fn try_clone_active_handle(&self) -> io::Result<File> {
        match (&self.writer, &self.verifier) {
            (Some(writer), _) => writer.try_clone(),
            (None, Some(verifier)) => verifier.file.try_clone(),
            (None, None) => Err(io::Error::other("Windows target handle is unavailable")),
        }
    }
}

impl Read for WindowsTarget {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if let Some(verifier) = &mut self.verifier {
            verifier.read(buffer)
        } else {
            self.writer
                .as_mut()
                .ok_or_else(|| io::Error::other("Windows target handle is unavailable"))?
                .read(buffer)
        }
    }
}

impl Write for WindowsTarget {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.verifier.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "cannot write while unbuffered verification is active",
            ));
        }
        self.writer
            .as_mut()
            .ok_or_else(|| io::Error::other("Windows target handle is unavailable"))?
            .write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.as_mut().map_or(Ok(()), std::io::Write::flush)
    }
}

impl Seek for WindowsTarget {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        if let Some(verifier) = &mut self.verifier {
            verifier.seek(position)
        } else {
            self.writer
                .as_mut()
                .ok_or_else(|| io::Error::other("Windows target handle is unavailable"))?
                .seek(position)
        }
    }
}

impl WorkerTarget for WindowsTarget {
    fn sync_all(&self) -> io::Result<()> {
        // The write handle was synchronously flushed before the cache-bypassing verifier was
        // opened. A read-only unbuffered handle has no dirty state left to flush.
        self.writer.as_ref().map_or(Ok(()), File::sync_all)
    }
}

#[derive(Debug)]
struct UnbufferedVerifier {
    file: File,
    position: u64,
    capacity: u64,
    transfer_size: usize,
    buffer: AVec<u8, RuntimeAlign>,
}

impl UnbufferedVerifier {
    fn new(
        file: File,
        capacity: u64,
        logical_sector_size: u32,
        physical_sector_size: u32,
    ) -> io::Result<Self> {
        if !valid_sector_geometry(capacity, logical_sector_size, physical_sector_size) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported Windows disk sector geometry",
            ));
        }
        let transfer_size = usize::try_from(logical_sector_size)
            .map_err(|_| io::Error::other("logical sector size does not fit usize"))?;
        let alignment = usize::try_from(physical_sector_size)
            .map_err(|_| io::Error::other("physical sector size does not fit usize"))?;
        let mut buffer = AVec::<u8, RuntimeAlign>::with_capacity(
            alignment,
            VERIFICATION_BUFFER_SIZE + alignment,
        );
        buffer.resize(VERIFICATION_BUFFER_SIZE + alignment, 0);
        Ok(Self {
            file,
            position: 0,
            capacity,
            transfer_size,
            buffer,
        })
    }

    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() || self.position >= self.capacity {
            return Ok(0);
        }
        let requested = output.len().min(VERIFICATION_BUFFER_SIZE);
        let requested_u64 = u64::try_from(requested)
            .map_err(|_| io::Error::other("verification read size does not fit u64"))?;
        let available = usize::try_from((self.capacity - self.position).min(requested_u64))
            .map_err(|_| io::Error::other("verification read size does not fit usize"))?;
        let transfer_size_u64 = u64::try_from(self.transfer_size)
            .map_err(|_| io::Error::other("logical sector size does not fit u64"))?;
        let aligned_start = self.position - (self.position % transfer_size_u64);
        let prefix = usize::try_from(self.position - aligned_start)
            .map_err(|_| io::Error::other("verification prefix does not fit usize"))?;
        let needed = prefix
            .checked_add(available)
            .ok_or_else(|| io::Error::other("verification transfer length overflow"))?;
        let aligned_length = needed
            .div_ceil(self.transfer_size)
            .checked_mul(self.transfer_size)
            .ok_or_else(|| io::Error::other("verification transfer length overflow"))?;
        let aligned_length_u64 = u64::try_from(aligned_length)
            .map_err(|_| io::Error::other("verification transfer length does not fit u64"))?;
        if aligned_length > self.buffer.len()
            || aligned_start
                .checked_add(aligned_length_u64)
                .is_none_or(|end| end > self.capacity)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unaligned Windows verification request",
            ));
        }
        self.file.seek(SeekFrom::Start(aligned_start))?;
        let count = self.file.read(&mut self.buffer[..aligned_length])?;
        if count == 0 {
            return Ok(0);
        }
        if !count.is_multiple_of(self.transfer_size) || count <= prefix {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Windows returned an incomplete unbuffered disk sector",
            ));
        }
        let copied = available.min(count - prefix);
        output[..copied].copy_from_slice(&self.buffer[prefix..prefix + copied]);
        self.position += u64::try_from(copied)
            .map_err(|_| io::Error::other("verification result length does not fit u64"))?;
        Ok(copied)
    }

    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let next = match position {
            SeekFrom::Start(position) => i128::from(position),
            SeekFrom::Current(offset) => i128::from(self.position) + i128::from(offset),
            SeekFrom::End(offset) => i128::from(self.capacity) + i128::from(offset),
        };
        if next < 0 || next > i128::from(self.capacity) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Windows verification seek is outside the physical disk",
            ));
        }
        self.position = u64::try_from(next)
            .map_err(|_| io::Error::other("verification position does not fit u64"))?;
        Ok(self.position)
    }
}

/// Windows physical-disk backend. The surrounding worker must already be elevated.
#[derive(Debug, Default)]
pub struct WindowsPlatform {
    volumes_dismounted: bool,
    write_may_have_started: bool,
    prepared_target: Option<File>,
    post_write_pin: Option<File>,
    open_identity: Option<crate::windows_native::OpenDiskIdentity>,
    rollback_snapshot: Option<DiskSnapshot>,
    locked_volumes: Option<crate::windows_native::LockedVolumes>,
}

impl WindowsPlatform {
    fn rollback_unmount(&mut self, selected: &WorkerDrive) -> Result<(), WorkerError> {
        if self.write_may_have_started {
            return Err(WorkerError::UnsafeTarget);
        }
        drop(self.prepared_target.take());
        drop(self.post_write_pin.take());
        drop(self.locked_volumes.take());
        let expected = self
            .rollback_snapshot
            .as_ref()
            .ok_or(WorkerError::UnsafeTarget)?;
        let identity = SelectedDisk::parse(selected)?;
        let current = query_disk(identity.number)?;
        validate_snapshot(selected, &identity, &current, Some(false))?;
        if current != *expected {
            return Err(WorkerError::TargetChanged);
        }
        self.rollback_snapshot = None;
        self.open_identity = None;
        self.volumes_dismounted = false;
        Ok(())
    }
}

impl WorkerPlatform for WindowsPlatform {
    type Target = WindowsTarget;

    fn validate_target(&mut self, selected: &WorkerDrive) -> Result<WorkerDrive, WorkerError> {
        let identity = SelectedDisk::parse(selected)?;
        tracing::info!(
            disk_number = identity.number,
            device = identity.device,
            "validating Windows target"
        );
        let snapshot = query_disk(identity.number)?;
        validate_snapshot(selected, &identity, &snapshot, Some(false))?;
        tracing::info!(
            disk_number = identity.number,
            "Windows target validation succeeded"
        );
        Ok(selected.clone())
    }

    fn unmount(&mut self, selected: &WorkerDrive) -> Result<(), WorkerError> {
        if self.prepared_target.is_some()
            || self.volumes_dismounted
            || self.write_may_have_started
            || self.locked_volumes.is_some()
        {
            return Err(WorkerError::UnsafeTarget);
        }
        let identity = SelectedDisk::parse(selected)?;
        tracing::info!(
            disk_number = identity.number,
            device = identity.device,
            "locking Windows target volumes"
        );
        let snapshot = query_disk(identity.number)?;
        validate_snapshot(selected, &identity, &snapshot, Some(false))?;

        // A shared read handle pins the selected device object while the storage stack identifies
        // and locks each of its volumes. It permits Windows' own dismount handles but prevents a
        // hot-unplug/number-reuse race from silently changing the object under validation.
        let device_pin = open_device_pin(&identity.device)?;
        let pinned_snapshot = query_disk(identity.number)?;
        validate_snapshot(selected, &identity, &pinned_snapshot, Some(false))?;
        let pinned_identity = query_open_identity(&device_pin)?;
        validate_open_identity_against_snapshot(&pinned_snapshot, &pinned_identity)?;
        tracing::info!(
            disk_number = pinned_identity.number,
            size = pinned_identity.size,
            logical_sector_size = pinned_identity.logical_sector_size,
            physical_sector_size = pinned_identity.physical_sector_size,
            bus_type = pinned_identity.bus_type,
            "pinned native Windows disk identity"
        );
        self.rollback_snapshot = Some(pinned_snapshot);
        let locked_volumes = match crate::windows_native::lock_and_dismount_volumes(identity.number)
        {
            Ok(handles) => handles,
            Err(error) => {
                self.open_identity = None;
                self.rollback_snapshot = None;
                return Err(WorkerError::Platform(error.to_string()));
            }
        };
        self.locked_volumes = Some(locked_volumes);
        self.volumes_dismounted = true;
        tracing::info!(
            disk_number = identity.number,
            "Windows target volumes locked and dismounted"
        );
        drop(device_pin);

        let prepared = (|| {
            // Acquiring this zero-share handle is the decisive proof that no mounted filesystem or
            // competing process still owns the target. It is retained across the final identity
            // check and handed directly to the writer, so the path is never reopened for writing.
            let target = open_exclusive_target(&identity.device)?;
            let exclusive_identity = query_open_identity(&target)?;
            validate_open_identity(&pinned_identity, &exclusive_identity)?;
            self.prepared_target = Some(target);
            self.open_identity = Some(exclusive_identity);
            tracing::info!(
                disk_number = identity.number,
                "exclusive Windows target handle prepared"
            );
            Ok(())
        })();
        if let Err(error) = prepared {
            return match self.rollback_unmount(selected) {
                Ok(()) => Err(error),
                Err(rollback) => Err(WorkerError::Platform(format!(
                    "{error}; Windows volume-lock cleanup also failed: {rollback}"
                ))),
            };
        }
        Ok(())
    }

    fn open_target(
        &mut self,
        selected: &WorkerDrive,
        _verify: bool,
    ) -> Result<Self::Target, WorkerError> {
        if !self.volumes_dismounted {
            return Err(WorkerError::UnsafeTarget);
        }
        let _identity = SelectedDisk::parse(selected)?;
        let target = self
            .prepared_target
            .take()
            .ok_or(WorkerError::UnsafeTarget)?;
        let current = query_open_identity(&target)?;
        let expected = self
            .open_identity
            .as_ref()
            .ok_or(WorkerError::UnsafeTarget)?;
        validate_open_identity(expected, &current)?;
        tracing::info!(
            disk_number = current.number,
            "handing native Windows target handle to writer"
        );
        self.post_write_pin = Some(
            target
                .try_clone()
                .map_err(|error| WorkerError::Platform(error.to_string()))?,
        );
        self.write_may_have_started = true;
        self.rollback_snapshot = None;
        Ok(WindowsTarget::new(target))
    }

    fn prepare_verification(
        &mut self,
        selected: &WorkerDrive,
        target: &mut Self::Target,
    ) -> Result<(), crate::flash::FlashError> {
        let identity = SelectedDisk::parse(selected).map_err(|error| worker_as_io(&error))?;
        let expected = self
            .open_identity
            .as_ref()
            .ok_or(WorkerError::UnsafeTarget)
            .map_err(|error| worker_as_io(&error))?;
        validate_open_identity(expected, &target.open_identity()?)
            .map_err(|error| worker_as_io(&error))?;
        tracing::info!(
            disk_number = expected.number,
            "reopening Windows target for unbuffered verification"
        );

        // Both clones refer to the zero-share write-through handle. Flush first, then close every
        // clone so Windows will grant a new zero-share, NO_BUFFERING read handle.
        target.sync_all()?;
        drop(self.post_write_pin.take());
        target.begin_verification(
            &identity.device,
            expected.size,
            expected.logical_sector_size,
            expected.physical_sector_size,
            expected,
        )?;
        // `begin_verification` must close every buffered writer clone before Windows will grant
        // the cache-bypassing zero-share reader. Pin that newly opened verifier instead, so the
        // physical device object cannot be replaced between verification and final cleanup.
        self.post_write_pin = Some(target.try_clone_active_handle()?);
        Ok(())
    }

    fn eject(&mut self, selected: &WorkerDrive) -> Result<(), WorkerError> {
        if self.volumes_dismounted && !self.write_may_have_started {
            return self.rollback_unmount(selected);
        }
        if !self.volumes_dismounted && !self.write_may_have_started {
            return Ok(());
        }
        // A failure can occur after the exclusive handle was prepared but before it was handed to
        // the writer. Release it before asking the volume manager for a final dismount.
        drop(self.prepared_target.take());
        let identity = SelectedDisk::parse(selected)?;
        // `open_target` duplicates the same exclusive kernel handle before handing it to the
        // writer. Retain that pin across the first post-write identity query so the device number
        // cannot be recycled between the write and cleanup.
        let expected = self
            .open_identity
            .as_ref()
            .ok_or(WorkerError::UnsafeTarget)?;
        let pin = self
            .post_write_pin
            .as_ref()
            .ok_or(WorkerError::UnsafeTarget)?;
        validate_open_identity(expected, &query_open_identity(pin)?)?;
        drop(self.post_write_pin.take());

        if self.locked_volumes.is_none() {
            return Err(WorkerError::UnsafeTarget);
        }
        // Reacquiring an exclusive handle proves that the freshly written partition table did not
        // race an automatic mount. Closing that handle after WRITE_THROUGH flushing leaves the
        // removable medium in the safe-removal state even on controllers without inbox eject.
        let proof = open_exclusive_target(&identity.device)?;
        validate_open_identity(expected, &query_open_identity(&proof)?)?;
        proof
            .sync_all()
            .map_err(|error| WorkerError::Platform(error.to_string()))?;
        drop(proof);
        drop(self.locked_volumes.take());
        tracing::info!(
            disk_number = expected.number,
            "Windows target cleanup completed"
        );
        self.volumes_dismounted = true;
        self.write_may_have_started = true;
        Ok(())
    }
}

fn query_disk(number: u32) -> Result<DiskSnapshot, WorkerError> {
    let disk = crate::windows_native::query_disk(number).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            WorkerError::TargetMissing
        } else {
            WorkerError::Platform(error.to_string())
        }
    })?;
    Ok(DiskSnapshot {
        number: disk.number,
        path: disk.path,
        unique_id: disk.unique_id,
        serial_number: disk.serial_number,
        size: disk.size,
        logical_sector_size: disk.logical_sector_size,
        physical_sector_size: disk.physical_sector_size,
        is_boot: BoolFlag(disk.is_boot),
        is_system: BoolFlag(disk.is_system),
        is_offline: BoolFlag(disk.is_offline),
        is_read_only: BoolFlag(disk.is_read_only),
        bus_type: disk.bus_type,
        supports_removable_media: BoolFlag(disk.supports_removable_media),
    })
}

fn open_device_pin(device: &str) -> Result<File, WorkerError> {
    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(device)
        .map_err(|error| device_open_error(&error))
}

fn open_exclusive_target(device: &str) -> Result<File, WorkerError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .custom_flags(FILE_FLAG_WRITE_THROUGH)
        .open(device)
        .map_err(|error| device_open_error(&error))
}

fn open_unbuffered_target(device: &str) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .share_mode(0)
        .custom_flags(FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH)
        .open(device)
}

fn worker_as_io(error: &WorkerError) -> crate::flash::FlashError {
    crate::flash::FlashError::Io(io::Error::other(error.to_string()))
}

fn device_open_error(error: &io::Error) -> WorkerError {
    WorkerError::Platform(format!(
        "could not lock the physical disk; close Explorer and other applications using it, then retry: {error}"
    ))
}

fn validate_snapshot(
    selected: &WorkerDrive,
    identity: &SelectedDisk,
    snapshot: &DiskSnapshot,
    expected_offline: Option<bool>,
) -> Result<(), WorkerError> {
    validate_common_safety(selected, identity, snapshot)?;
    if expected_offline.is_some_and(|expected| snapshot.is_offline.is_set() != expected)
        || snapshot.fingerprint()? != identity.fingerprint
    {
        return Err(WorkerError::TargetChanged);
    }
    let current = WorkerDrive {
        id: format!("{}@{}", identity.device, snapshot.fingerprint()?),
        device: identity.device.clone(),
        capacity: snapshot.size,
    };
    compare_drive(selected, &current)
}

fn query_open_identity(
    file: &File,
) -> Result<crate::windows_native::OpenDiskIdentity, WorkerError> {
    crate::windows_native::query_open_disk(file)
        .map_err(|error| WorkerError::Platform(error.to_string()))
}

fn validate_open_identity_against_snapshot(
    snapshot: &DiskSnapshot,
    current: &crate::windows_native::OpenDiskIdentity,
) -> Result<(), WorkerError> {
    if current.number != snapshot.number
        || current.size != snapshot.size
        || current.logical_sector_size != snapshot.logical_sector_size
        || current.physical_sector_size != snapshot.physical_sector_size
        || current.bus_type != snapshot.bus_type
        || current.device_guid.iter().all(|byte| *byte == 0)
    {
        return Err(WorkerError::TargetChanged);
    }
    Ok(())
}

fn validate_open_identity(
    expected: &crate::windows_native::OpenDiskIdentity,
    current: &crate::windows_native::OpenDiskIdentity,
) -> Result<(), WorkerError> {
    if current != expected {
        return Err(WorkerError::TargetChanged);
    }
    Ok(())
}

fn validate_common_safety(
    selected: &WorkerDrive,
    identity: &SelectedDisk,
    snapshot: &DiskSnapshot,
) -> Result<(), WorkerError> {
    if snapshot.number != identity.number {
        return Err(WorkerError::UnsafeTarget);
    }
    if snapshot.size != selected.capacity {
        return Err(WorkerError::UnsafeTarget);
    }
    let unsafe_status = snapshot.is_boot.is_set()
        || snapshot.is_system.is_set()
        || snapshot.is_offline.is_set()
        || snapshot.is_read_only.is_set()
        || !snapshot.is_card_target()
        || !valid_sector_geometry(
            snapshot.size,
            snapshot.logical_sector_size,
            snapshot.physical_sector_size,
        )
        || snapshot.path.is_empty()
        || (snapshot.unique_id.is_empty() && snapshot.serial_number.is_empty());
    if unsafe_status {
        return Err(WorkerError::UnsafeTarget);
    }
    Ok(())
}

fn valid_sector_geometry(size: u64, logical: u32, physical: u32) -> bool {
    (512..=65_536).contains(&logical)
        && logical.is_power_of_two()
        && (logical..=65_536).contains(&physical)
        && physical.is_power_of_two()
        && size.is_multiple_of(u64::from(logical))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> DiskSnapshot {
        DiskSnapshot {
            number: 7,
            path: r"\\?\usbstor#disk&ven_snapdog&prod_test".to_owned(),
            unique_id: "MEDIA-1234".to_owned(),
            serial_number: "SERIAL-5678".to_owned(),
            size: 32_000_000_000,
            logical_sector_size: 512,
            physical_sector_size: 4_096,
            is_boot: BoolFlag(false),
            is_system: BoolFlag(false),
            is_offline: BoolFlag(false),
            is_read_only: BoolFlag(false),
            bus_type: "USB".to_owned(),
            supports_removable_media: BoolFlag(true),
        }
    }

    fn selected(snapshot: &DiskSnapshot) -> WorkerDrive {
        let device = format!(r"\\.\PHYSICALDRIVE{}", snapshot.number);
        WorkerDrive {
            id: format!("{device}@{}", snapshot.fingerprint().unwrap()),
            device,
            capacity: snapshot.size,
        }
    }

    fn open_identity(snapshot: &DiskSnapshot) -> crate::windows_native::OpenDiskIdentity {
        crate::windows_native::OpenDiskIdentity {
            number: snapshot.number,
            device_guid: [7; 16],
            size: snapshot.size,
            logical_sector_size: snapshot.logical_sector_size,
            physical_sector_size: snapshot.physical_sector_size,
            bus_type: snapshot.bus_type.clone(),
        }
    }

    #[test]
    fn stable_identity_round_trips() {
        let snapshot = snapshot();
        let selected = selected(&snapshot);
        let identity = SelectedDisk::parse(&selected).unwrap();
        assert_eq!(identity.number, 7);
        assert_eq!(identity.device, r"\\.\PHYSICALDRIVE7");
        assert!(validate_snapshot(&selected, &identity, &snapshot, Some(false)).is_ok());
    }

    #[test]
    fn native_handle_identity_survives_dismount_without_wmi() {
        let snapshot = snapshot();
        let pinned = open_identity(&snapshot);
        assert!(validate_open_identity_against_snapshot(&snapshot, &pinned).is_ok());
        assert!(validate_open_identity(&pinned, &pinned).is_ok());

        let mut replacement = pinned.clone();
        replacement.device_guid[0] ^= 0xff;
        assert!(matches!(
            validate_open_identity(&pinned, &replacement),
            Err(WorkerError::TargetChanged)
        ));
    }

    #[test]
    fn identity_rejects_partitions_missing_fingerprint_and_aliases() {
        let snapshot = snapshot();
        let mut drive = selected(&snapshot);
        drive.id = r"\\.\PHYSICALDRIVE7".to_owned();
        assert!(SelectedDisk::parse(&drive).is_err());

        drive.id = format!(r"\\.\PHYSICALDRIVE07@{}", snapshot.fingerprint().unwrap());
        drive.device = r"\\.\PHYSICALDRIVE07".to_owned();
        assert!(SelectedDisk::parse(&drive).is_err());

        drive.id = format!(
            r"\\.\PHYSICALDRIVE7\Partition0@{}",
            snapshot.fingerprint().unwrap()
        );
        drive.device = r"\\.\PHYSICALDRIVE7\Partition0".to_owned();
        assert!(SelectedDisk::parse(&drive).is_err());
    }

    #[test]
    fn rejects_boot_system_read_only_and_non_removable_disks() {
        for mutate in [
            |disk: &mut DiskSnapshot| disk.is_boot = BoolFlag(true),
            |disk: &mut DiskSnapshot| disk.is_system = BoolFlag(true),
            |disk: &mut DiskSnapshot| disk.is_read_only = BoolFlag(true),
            |disk: &mut DiskSnapshot| disk.bus_type = "NVME".to_owned(),
            |disk: &mut DiskSnapshot| disk.supports_removable_media = BoolFlag(false),
        ] {
            let original = snapshot();
            let selected = selected(&original);
            let identity = SelectedDisk::parse(&selected).unwrap();
            let mut changed = original;
            mutate(&mut changed);
            assert!(matches!(
                validate_snapshot(&selected, &identity, &changed, None),
                Err(WorkerError::UnsafeTarget)
            ));
        }
    }

    #[test]
    fn built_in_sd_and_mmc_do_not_depend_on_usb_removable_capability() {
        for bus_type in ["SD", "MMC"] {
            let mut card = snapshot();
            card.bus_type = bus_type.to_owned();
            card.supports_removable_media = BoolFlag(false);
            let selected = selected(&card);
            let identity = SelectedDisk::parse(&selected).unwrap();
            assert!(validate_snapshot(&selected, &identity, &card, Some(false)).is_ok());
        }
    }

    #[test]
    fn rejects_unsafe_sector_geometry() {
        for mutate in [
            |disk: &mut DiskSnapshot| disk.logical_sector_size = 0,
            |disk: &mut DiskSnapshot| disk.logical_sector_size = 1_000,
            |disk: &mut DiskSnapshot| disk.physical_sector_size = 128,
            |disk: &mut DiskSnapshot| disk.physical_sector_size = 131_072,
            |disk: &mut DiskSnapshot| disk.size += 1,
        ] {
            let original = snapshot();
            let selected = selected(&original);
            let identity = SelectedDisk::parse(&selected).unwrap();
            let mut changed = original;
            mutate(&mut changed);
            assert!(matches!(
                validate_snapshot(&selected, &identity, &changed, None),
                Err(WorkerError::UnsafeTarget)
            ));
        }
    }

    #[test]
    fn aligned_verifier_handles_full_and_unaligned_logical_reads() {
        let mut file = tempfile::tempfile().unwrap();
        let bytes = (0_u16..16_384)
            .map(|value| u8::try_from(value % 251).unwrap())
            .collect::<Vec<_>>();
        file.write_all(&bytes).unwrap();
        file.sync_all().unwrap();

        let mut verifier = UnbufferedVerifier::new(file, 16_384, 512, 4_096).unwrap();
        let mut first = vec![0_u8; 6_000];
        assert_eq!(verifier.read(&mut first).unwrap(), first.len());
        assert_eq!(first, bytes[..first.len()]);

        verifier.seek(SeekFrom::Start(123)).unwrap();
        let mut unaligned = vec![0_u8; 1_001];
        assert_eq!(verifier.read(&mut unaligned).unwrap(), unaligned.len());
        assert_eq!(unaligned, bytes[123..123 + unaligned.len()]);
    }

    #[test]
    fn detects_media_swap_and_offline_state_mismatch() {
        let original = snapshot();
        let selected = selected(&original);
        let identity = SelectedDisk::parse(&selected).unwrap();

        let mut swapped = original.clone();
        swapped.unique_id = "DIFFERENT-MEDIA".to_owned();
        assert!(matches!(
            validate_snapshot(&selected, &identity, &swapped, Some(false)),
            Err(WorkerError::TargetChanged)
        ));

        let mut offline = original;
        offline.is_offline = BoolFlag(true);
        assert!(matches!(
            validate_snapshot(&selected, &identity, &offline, Some(false)),
            Err(WorkerError::UnsafeTarget)
        ));
    }

    #[test]
    fn native_backend_keeps_unbuffered_verification() {
        assert_eq!(FILE_FLAG_NO_BUFFERING, 0x2000_0000);
        assert_eq!(FILE_FLAG_WRITE_THROUGH, 0x8000_0000);
    }
}
