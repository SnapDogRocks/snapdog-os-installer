// SPDX-License-Identifier: GPL-3.0-only

//! Linux raw-device worker backend.
//!
//! A selected device is identified by both its kernel block name and Linux `diskseq`. The latter
//! changes whenever a block device is instantiated, so a hot-unplug followed by `/dev` path reuse
//! cannot silently redirect a queued flash job. All paths used by privileged commands are derived
//! from that validated kernel name and are passed as individual arguments, never through a shell.

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

use aligned_vec::{AVec, RuntimeAlign};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedFd, OwnedObjectPath, Value};

use super::{WorkerDrive, WorkerError, WorkerPlatform, WorkerTarget, compare_drive};
use crate::flash::FlashError;

const SYS_BLOCK: &str = "/sys/block";
const SYS_DEV_BLOCK: &str = "/sys/dev/block";
const PROC_MOUNTINFO: &str = "/proc/self/mountinfo";
const PROC_SWAPS: &str = "/proc/swaps";
const DEV_ROOT: &str = "/dev";
const KERNEL_SECTOR_SIZE: u64 = 512;

// Linux UAPI values accepted by UDisks2 Block.OpenDevice. O_EXCL rejects mounted media, O_SYNC
// gives completed writes, and O_DIRECT makes verification bypass the block-device page cache.
const O_EXCL: i32 = 0o200;
const O_DIRECT: i32 = 0o40_000;
const O_SYNC: i32 = 0o4_010_000;
const DIRECT_ALIGNMENT: usize = 4096;
const VERIFICATION_BUFFER_SIZE: usize = 1024 * 1024;
const UDISKS_SERVICE: &str = "org.freedesktop.UDisks2";
const UDISKS_MANAGER_PATH: &str = "/org/freedesktop/UDisks2/Manager";
const UDISKS_MANAGER_INTERFACE: &str = "org.freedesktop.UDisks2.Manager";
const UDISKS_BLOCK_INTERFACE: &str = "org.freedesktop.UDisks2.Block";
const UDISKS_FILESYSTEM_INTERFACE: &str = "org.freedesktop.UDisks2.Filesystem";
const UDISKS_DRIVE_INTERFACE: &str = "org.freedesktop.UDisks2.Drive";

#[derive(Default)]
pub(super) struct LinuxPlatform {
    connection: Option<Connection>,
}

pub(super) struct LinuxTarget {
    writer: Option<File>,
    verifier: Option<DirectVerifier>,
}

impl Read for LinuxTarget {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if let Some(verifier) = self.verifier.as_mut() {
            verifier.read(output)
        } else {
            self.writer
                .as_mut()
                .ok_or_else(|| io::Error::other("Linux target is unavailable"))?
                .read(output)
        }
    }
}

impl Write for LinuxTarget {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        self.writer
            .as_mut()
            .ok_or_else(|| io::Error::other("Linux write handle is unavailable"))?
            .write(input)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer
            .as_mut()
            .ok_or_else(|| io::Error::other("Linux write handle is unavailable"))?
            .flush()
    }
}

impl Seek for LinuxTarget {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.verifier.as_mut().map_or_else(
            || {
                self.writer
                    .as_mut()
                    .ok_or_else(|| io::Error::other("Linux target is unavailable"))?
                    .seek(position)
            },
            |verifier| verifier.seek(position),
        )
    }
}

impl super::WorkerTarget for LinuxTarget {
    fn sync_all(&self) -> io::Result<()> {
        self.writer.as_ref().map_or(Ok(()), File::sync_all)
    }
}

impl WorkerPlatform for LinuxPlatform {
    type Target = LinuxTarget;

    fn validate_staged_image(&mut self, image: &File) -> Result<(), WorkerError> {
        let metadata = image
            .metadata()
            .map_err(|error| WorkerError::Platform(error.to_string()))?;
        if !metadata.is_file()
            || metadata.uid() == 0
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() > 1
        {
            return Err(WorkerError::InvalidJob(
                "staged image is not an unlinked private file owned by the desktop user".to_owned(),
            ));
        }
        Ok(())
    }

    fn validate_target(&mut self, selected: &WorkerDrive) -> Result<WorkerDrive, WorkerError> {
        validate_target_at(
            selected,
            Path::new(SYS_BLOCK),
            Path::new(SYS_DEV_BLOCK),
            Path::new(DEV_ROOT),
            Path::new(PROC_MOUNTINFO),
            Path::new(PROC_SWAPS),
        )
    }

    fn unmount(&mut self, selected: &WorkerDrive) -> Result<(), WorkerError> {
        // Revalidate here as well: `run_job` checks immediately before this call, but keeping the
        // destructive platform method self-contained prevents accidental unsafe reuse.
        compare_drive(selected, &self.validate_target(selected)?)?;
        let (block_name, _) = split_stable_identifier(&selected.id)?;
        let device_numbers = collect_related_device_numbers(
            &Path::new(SYS_BLOCK).join(block_name),
            Path::new(SYS_DEV_BLOCK),
        )?;
        compare_drive(selected, &self.validate_target(selected)?)?;
        let mountinfo = fs::read_to_string(PROC_MOUNTINFO)
            .map_err(|error| WorkerError::Platform(error.to_string()))?;
        let mut mounts = parse_mountinfo(&mountinfo)?
            .into_iter()
            .filter(|mount| device_numbers.contains(&mount.device))
            .collect::<Vec<_>>();
        mounts.sort_by(|left, right| {
            right
                .path
                .components()
                .count()
                .cmp(&left.path.components().count())
        });
        let mut unmounted = BTreeSet::new();
        for mount in mounts {
            if !unmounted.insert(mount.device) {
                continue;
            }
            compare_drive(selected, &self.validate_target(selected)?)?;
            let path = self.block_object(mount.device)?;
            self.call_no_result(&path, UDISKS_FILESYSTEM_INTERFACE, "Unmount")?;
        }

        compare_drive(selected, &self.validate_target(selected)?)?;
        let remaining = fs::read_to_string(PROC_MOUNTINFO)
            .map_err(|error| WorkerError::Platform(error.to_string()))?;
        if !mounted_paths(&remaining, &device_numbers)?.is_empty() {
            return Err(WorkerError::Platform(
                "one or more target volumes remained mounted".to_owned(),
            ));
        }
        Ok(())
    }

    fn open_target(
        &mut self,
        selected: &WorkerDrive,
        _verify: bool,
    ) -> Result<Self::Target, WorkerError> {
        compare_drive(selected, &self.validate_target(selected)?)?;
        let (block_name, _) = split_stable_identifier(&selected.id)?;
        let expected_device =
            read_device_number(&Path::new(SYS_BLOCK).join(block_name).join("dev"))?;

        let target = self.open_device(expected_device, "rw", O_EXCL | O_SYNC)?;

        validate_opened_device(&target, expected_device)?;
        // Detect a device replacement in the interval between the sysfs lookup and raw open.
        compare_drive(selected, &self.validate_target(selected)?)?;
        let current_device =
            read_device_number(&Path::new(SYS_BLOCK).join(block_name).join("dev"))?;
        if current_device != expected_device {
            return Err(WorkerError::TargetChanged);
        }
        validate_opened_device(&target, current_device)?;
        Ok(LinuxTarget {
            writer: Some(target),
            verifier: None,
        })
    }

    fn prepare_verification(
        &mut self,
        selected: &WorkerDrive,
        target: &mut Self::Target,
    ) -> Result<(), FlashError> {
        let map_error = |error: WorkerError| FlashError::Io(io::Error::other(error.to_string()));

        // Finish synchronous writes, close that descriptor, then ask UDisks2 for a fresh O_DIRECT
        // descriptor. The aligned verifier therefore reads the physical device rather than pages
        // populated by this process' writes.
        let current = self.validate_target(selected).map_err(map_error)?;
        compare_drive(selected, &current).map_err(map_error)?;
        target.sync_all().map_err(FlashError::from)?;
        let (block_name, _) = split_stable_identifier(&selected.id).map_err(map_error)?;
        drop(target.writer.take());
        let expected_device =
            read_device_number(&Path::new(SYS_BLOCK).join(block_name).join("dev"))
                .map_err(map_error)?;
        let direct = self
            .open_device(expected_device, "r", O_EXCL | O_DIRECT)
            .map_err(map_error)?;
        target.verifier =
            Some(DirectVerifier::new(direct, selected.capacity).map_err(FlashError::from)?);
        let current = self.validate_target(selected).map_err(map_error)?;
        compare_drive(selected, &current).map_err(map_error)
    }

    fn eject(&mut self, selected: &WorkerDrive) -> Result<(), WorkerError> {
        // A desktop automounter can observe the new partition table as soon as the raw descriptor
        // is closed. Unmount once more before power-off so the success event never tells the user
        // to remove media that was remounted in that narrow interval.
        self.unmount(selected)?;
        compare_drive(selected, &self.validate_target(selected)?)?;
        let (block_name, _) = split_stable_identifier(&selected.id)?;
        let expected_device =
            read_device_number(&Path::new(SYS_BLOCK).join(block_name).join("dev"))?;
        let block_path = self.block_object(expected_device)?;
        let drive: OwnedObjectPath = self
            .proxy(&block_path, UDISKS_BLOCK_INTERFACE)?
            .get_property("Drive")
            .map_err(udisks_error)?;
        match self.call_no_result(&drive, UDISKS_DRIVE_INTERFACE, "PowerOff") {
            Ok(()) => Ok(()),
            Err(power_error) => self
                .call_no_result(&drive, UDISKS_DRIVE_INTERFACE, "Eject")
                .map_err(|eject_error| {
                    WorkerError::Platform(format!(
                        "UDisks2 could neither power off nor eject the target: {power_error}; {eject_error}"
                    ))
                }),
        }
    }
}

impl LinuxPlatform {
    fn connection(&mut self) -> Result<Connection, WorkerError> {
        if self.connection.is_none() {
            self.connection = Some(Connection::system().map_err(udisks_error)?);
        }
        Ok(self
            .connection
            .as_ref()
            .expect("connection initialized")
            .clone())
    }

    fn proxy(
        &mut self,
        path: &OwnedObjectPath,
        interface: &str,
    ) -> Result<Proxy<'static>, WorkerError> {
        Proxy::new_owned(
            self.connection()?,
            UDISKS_SERVICE.to_owned(),
            path.clone(),
            interface.to_owned(),
        )
        .map_err(udisks_error)
    }

    fn block_paths(&mut self) -> Result<Vec<OwnedObjectPath>, WorkerError> {
        let manager = Proxy::new_owned(
            self.connection()?,
            UDISKS_SERVICE.to_owned(),
            UDISKS_MANAGER_PATH.to_owned(),
            UDISKS_MANAGER_INTERFACE.to_owned(),
        )
        .map_err(udisks_error)?;
        let options = HashMap::<&str, Value<'_>>::new();
        manager
            .call("GetBlockDevices", &(options,))
            .map_err(udisks_error)
    }

    fn block_object(&mut self, expected: DeviceNumber) -> Result<OwnedObjectPath, WorkerError> {
        for path in self.block_paths()? {
            let device_number: u64 = self
                .proxy(&path, UDISKS_BLOCK_INTERFACE)?
                .get_property("DeviceNumber")
                .map_err(udisks_error)?;
            if decode_linux_device(device_number) == expected {
                return Ok(path);
            }
        }
        Err(WorkerError::TargetMissing)
    }

    fn open_device(
        &mut self,
        expected: DeviceNumber,
        mode: &str,
        flags: i32,
    ) -> Result<File, WorkerError> {
        let path = self.block_object(expected)?;
        let mut options = HashMap::new();
        options.insert("flags", Value::from(flags));
        let fd: OwnedFd = self
            .proxy(&path, UDISKS_BLOCK_INTERFACE)?
            .call("OpenDevice", &(mode, options))
            .map_err(udisks_error)?;
        let file = File::from(std::os::fd::OwnedFd::from(fd));
        validate_opened_device(&file, expected)?;
        Ok(file)
    }

    fn call_no_result(
        &mut self,
        path: &OwnedObjectPath,
        interface: &str,
        method: &str,
    ) -> Result<(), WorkerError> {
        let options = HashMap::<&str, Value<'_>>::new();
        self.proxy(path, interface)?
            .call(method, &(options,))
            .map_err(udisks_error)
    }
}

fn udisks_error(error: zbus::Error) -> WorkerError {
    let message = error.to_string();
    drop(error);
    WorkerError::Platform(format!("UDisks2 operation failed: {message}"))
}

#[derive(Debug)]
struct DirectVerifier {
    file: File,
    position: u64,
    capacity: u64,
    buffer: AVec<u8, RuntimeAlign>,
}

impl DirectVerifier {
    fn new(file: File, capacity: u64) -> io::Result<Self> {
        if capacity == 0 || !capacity.is_multiple_of(KERNEL_SECTOR_SIZE) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported Linux block-device geometry",
            ));
        }
        let mut buffer = AVec::<u8, RuntimeAlign>::with_capacity(
            DIRECT_ALIGNMENT,
            VERIFICATION_BUFFER_SIZE + DIRECT_ALIGNMENT,
        );
        buffer.resize(VERIFICATION_BUFFER_SIZE + DIRECT_ALIGNMENT, 0);
        Ok(Self {
            file,
            position: 0,
            capacity,
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
        let aligned_start = self.position - (self.position % KERNEL_SECTOR_SIZE);
        let prefix = usize::try_from(self.position - aligned_start)
            .map_err(|_| io::Error::other("verification prefix does not fit usize"))?;
        let needed = prefix
            .checked_add(available)
            .ok_or_else(|| io::Error::other("verification transfer length overflow"))?;
        let sector_size = usize::try_from(KERNEL_SECTOR_SIZE).expect("sector size fits usize");
        let aligned_length = needed
            .div_ceil(sector_size)
            .checked_mul(sector_size)
            .ok_or_else(|| io::Error::other("verification transfer length overflow"))?;
        if aligned_length > self.buffer.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unaligned Linux verification request",
            ));
        }
        self.file.seek(SeekFrom::Start(aligned_start))?;
        let count = self.file.read(&mut self.buffer[..aligned_length])?;
        if count == 0 {
            return Ok(0);
        }
        if !count.is_multiple_of(sector_size) || count <= prefix {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Linux returned an incomplete direct disk sector",
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
                "Linux verification seek is outside the physical disk",
            ));
        }
        self.position = u64::try_from(next)
            .map_err(|_| io::Error::other("verification position does not fit u64"))?;
        Ok(self.position)
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct DeviceNumber {
    major: u64,
    minor: u64,
}

#[derive(Debug, Eq, PartialEq)]
struct CurrentDrive {
    name: String,
    diskseq: u64,
    capacity: u64,
    device_number: DeviceNumber,
}

fn validate_target_at(
    selected: &WorkerDrive,
    sys_block: &Path,
    sys_dev_block: &Path,
    dev_root: &Path,
    mountinfo_path: &Path,
    swaps_path: &Path,
) -> Result<WorkerDrive, WorkerError> {
    let (block_name, _selected_diskseq) = split_stable_identifier(&selected.id)?;
    if selected.device != dev_root.join(block_name).to_string_lossy() {
        return Err(WorkerError::UnsafeTarget);
    }

    let block_root = sys_block.join(block_name);
    let current = inspect_current_drive(block_name, &block_root)?;
    let current_drive = WorkerDrive {
        id: format_stable_identifier(&current.name, current.diskseq),
        device: dev_root.join(&current.name).to_string_lossy().into_owned(),
        capacity: current.capacity,
    };
    compare_drive(selected, &current_drive)?;
    validate_device_node(&dev_root.join(block_name), current.device_number)?;

    let related = collect_related_device_numbers(&block_root, sys_dev_block)?;
    let mountinfo = fs::read_to_string(mountinfo_path)
        .map_err(|error| WorkerError::Platform(error.to_string()))?;
    reject_protected_mounts(&mountinfo, &related)?;
    let swaps =
        fs::read_to_string(swaps_path).map_err(|error| WorkerError::Platform(error.to_string()))?;
    reject_related_swap(&swaps, &related)?;
    Ok(current_drive)
}

fn inspect_current_drive(block_name: &str, root: &Path) -> Result<CurrentDrive, WorkerError> {
    if !valid_block_name(block_name) {
        return Err(WorkerError::UnsafeTarget);
    }
    if !root.exists() {
        return Err(WorkerError::TargetMissing);
    }
    if read_trimmed(&root.join("removable"))? != "1" || read_trimmed(&root.join("ro"))? != "0" {
        return Err(WorkerError::UnsafeTarget);
    }

    let diskseq = read_positive_u64(&root.join("diskseq"), "diskseq")?;
    let sectors = read_positive_u64(&root.join("size"), "device size")?;
    let capacity = sectors
        .checked_mul(KERNEL_SECTOR_SIZE)
        .ok_or_else(|| WorkerError::Platform("target capacity overflow".to_owned()))?;
    let device_number = read_device_number(&root.join("dev"))?;
    Ok(CurrentDrive {
        name: block_name.to_owned(),
        diskseq,
        capacity,
        device_number,
    })
}

fn validate_device_node(path: &Path, expected: DeviceNumber) -> Result<(), WorkerError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(WorkerError::TargetMissing);
        }
        Err(error) => return Err(WorkerError::Platform(error.to_string())),
    };
    if metadata.file_type().is_symlink() || !metadata.file_type().is_block_device() {
        return Err(WorkerError::UnsafeTarget);
    }
    if decode_linux_device(metadata.rdev()) != expected {
        return Err(WorkerError::TargetChanged);
    }
    Ok(())
}

fn validate_opened_device(file: &File, expected: DeviceNumber) -> Result<(), WorkerError> {
    let metadata = file
        .metadata()
        .map_err(|error| WorkerError::Platform(error.to_string()))?;
    if !metadata.file_type().is_block_device() {
        return Err(WorkerError::UnsafeTarget);
    }
    if decode_linux_device(metadata.rdev()) != expected {
        return Err(WorkerError::TargetChanged);
    }
    Ok(())
}

fn collect_related_device_numbers(
    block_root: &Path,
    sys_dev_block: &Path,
) -> Result<BTreeSet<DeviceNumber>, WorkerError> {
    let mut devices = BTreeSet::new();
    let whole = read_device_number(&block_root.join("dev"))?;
    devices.insert(whole);

    let entries = fs::read_dir(block_root).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            WorkerError::TargetMissing
        } else {
            WorkerError::Platform(error.to_string())
        }
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| WorkerError::Platform(error.to_string()))?;
        let path = entry.path();
        if path.join("partition").is_file() {
            devices.insert(read_device_number(&path.join("dev"))?);
        }
    }

    // Device-mapper and similar holders can contain the running system even though the selected
    // physical partition itself is not listed as a mount source. Traverse holders transitively.
    let mut pending = devices.iter().copied().collect::<Vec<_>>();
    while let Some(device) = pending.pop() {
        let holders = sys_dev_block
            .join(format_device_number(device))
            .join("holders");
        let entries = match fs::read_dir(holders) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(WorkerError::Platform(error.to_string())),
        };
        for entry in entries {
            let entry = entry.map_err(|error| WorkerError::Platform(error.to_string()))?;
            let holder = read_device_number(&entry.path().join("dev"))?;
            if devices.insert(holder) {
                pending.push(holder);
            }
        }
    }
    Ok(devices)
}

fn reject_protected_mounts(
    mountinfo: &str,
    devices: &BTreeSet<DeviceNumber>,
) -> Result<(), WorkerError> {
    for mount in parse_mountinfo(mountinfo)? {
        if devices.contains(&mount.device) && protected_mount_point(&mount.path) {
            return Err(WorkerError::UnsafeTarget);
        }
    }
    Ok(())
}

fn reject_related_swap(swaps: &str, devices: &BTreeSet<DeviceNumber>) -> Result<(), WorkerError> {
    for (index, line) in swaps.lines().enumerate() {
        if index == 0 || line.trim().is_empty() {
            continue;
        }
        let encoded_path = line.split_whitespace().next().ok_or_else(|| {
            WorkerError::Platform("kernel swap list contained an invalid record".to_owned())
        })?;
        let path = PathBuf::from(decode_mountinfo_field(encoded_path)?);
        // `/proc/swaps` is the kernel's active-source list. Any listed source that cannot be
        // inspected is ambiguous and therefore fails closed, including an unlinked swap file.
        let metadata =
            fs::metadata(path).map_err(|error| WorkerError::Platform(error.to_string()))?;
        let source_device = if metadata.file_type().is_block_device() {
            Some(decode_linux_device(metadata.rdev()))
        } else if metadata.is_file() {
            Some(decode_linux_device(metadata.dev()))
        } else {
            None
        };
        if source_device.is_some_and(|device| devices.contains(&device)) {
            return Err(WorkerError::UnsafeTarget);
        }
    }
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
struct Mount {
    device: DeviceNumber,
    path: PathBuf,
}

fn parse_mountinfo(contents: &str) -> Result<Vec<Mount>, WorkerError> {
    let mut mounts = Vec::new();
    for line in contents.lines().filter(|line| !line.trim().is_empty()) {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() < 6 || !fields.contains(&"-") {
            return Err(WorkerError::Platform(
                "kernel mountinfo contained an invalid record".to_owned(),
            ));
        }
        mounts.push(Mount {
            device: parse_device_number(fields[2])?,
            path: PathBuf::from(decode_mountinfo_field(fields[4])?),
        });
    }
    Ok(mounts)
}

fn mounted_paths(
    mountinfo: &str,
    devices: &BTreeSet<DeviceNumber>,
) -> Result<Vec<PathBuf>, WorkerError> {
    Ok(parse_mountinfo(mountinfo)?
        .into_iter()
        .filter(|mount| devices.contains(&mount.device))
        .map(|mount| mount.path)
        .collect())
}

#[cfg(test)]
fn sort_mount_points_for_unmount(paths: &mut Vec<PathBuf>) {
    paths.sort_by(|left, right| {
        right
            .components()
            .count()
            .cmp(&left.components().count())
            .then_with(|| right.as_os_str().cmp(left.as_os_str()))
    });
    paths.dedup();
}

fn protected_mount_point(path: &Path) -> bool {
    // Fail closed for every mount outside the conventional removable-media roots. Enumerating a
    // handful of system directories is insufficient: installations can place /etc, /root, /run,
    // /opt, /srv, /tmp, /nix, or application data on separate devices. A selected disk mounted
    // anywhere unexpected must require manual intervention rather than being silently unmounted.
    !["/media", "/mnt", "/run/media"]
        .into_iter()
        .map(Path::new)
        .any(|removable_root| path == removable_root || path.starts_with(removable_root))
}

fn decode_mountinfo_field(value: &str) -> Result<OsString, WorkerError> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            let escape = bytes.get(index + 1..index + 4).ok_or_else(|| {
                WorkerError::Platform("mountinfo contained a truncated escape".to_owned())
            })?;
            if !escape.iter().all(u8::is_ascii_digit) || escape.iter().any(|digit| *digit > b'7') {
                return Err(WorkerError::Platform(
                    "mountinfo contained an invalid escape".to_owned(),
                ));
            }
            let octal = (escape[0] - b'0') * 64 + (escape[1] - b'0') * 8 + escape[2] - b'0';
            decoded.push(octal);
            index += 4;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    Ok(OsString::from_vec(decoded))
}

fn split_stable_identifier(value: &str) -> Result<(&str, u64), WorkerError> {
    let (block_name, diskseq) = value.split_once('@').ok_or(WorkerError::UnsafeTarget)?;
    if !valid_block_name(block_name)
        || diskseq.is_empty()
        || !diskseq.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(WorkerError::UnsafeTarget);
    }
    let diskseq = diskseq
        .parse::<u64>()
        .ok()
        .filter(|diskseq| *diskseq > 0)
        .ok_or(WorkerError::UnsafeTarget)?;
    Ok((block_name, diskseq))
}

fn valid_block_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn format_stable_identifier(block_name: &str, diskseq: u64) -> String {
    format!("{block_name}@{diskseq}")
}

fn read_trimmed(path: &Path) -> Result<String, WorkerError> {
    fs::read_to_string(path)
        .map(|value| value.trim().to_owned())
        .map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                WorkerError::TargetMissing
            } else {
                WorkerError::Platform(error.to_string())
            }
        })
}

fn read_positive_u64(path: &Path, label: &str) -> Result<u64, WorkerError> {
    read_trimmed(path)?
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| WorkerError::Platform(format!("target {label} is invalid")))
}

fn read_device_number(path: &Path) -> Result<DeviceNumber, WorkerError> {
    parse_device_number(&read_trimmed(path)?)
}

fn parse_device_number(value: &str) -> Result<DeviceNumber, WorkerError> {
    let (major, minor) = value
        .split_once(':')
        .ok_or_else(|| WorkerError::Platform("invalid kernel device number".to_owned()))?;
    let parse = |part: &str| {
        (!part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| part.parse::<u64>().ok())
            .flatten()
            .ok_or_else(|| WorkerError::Platform("invalid kernel device number".to_owned()))
    };
    Ok(DeviceNumber {
        major: parse(major)?,
        minor: parse(minor)?,
    })
}

fn format_device_number(device: DeviceNumber) -> String {
    format!("{}:{}", device.major, device.minor)
}

// Linux's userspace dev_t layout, matching gnu_dev_major/gnu_dev_minor. This compares fstat(2)
// output with the kernel's `/sys/.../dev` identity without introducing an unsafe ioctl/FFI layer.
const fn decode_linux_device(device: u64) -> DeviceNumber {
    DeviceNumber {
        major: ((device >> 8) & 0x0fff) | ((device >> 32) & 0xffff_f000),
        minor: (device & 0x00ff) | ((device >> 12) & 0xffff_ff00),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn stable_identifier_accepts_common_devices_and_rejects_injection() {
        assert_eq!(split_stable_identifier("sdb@42").unwrap(), ("sdb", 42));
        assert_eq!(
            split_stable_identifier("mmcblk0@18446744073709551615").unwrap(),
            ("mmcblk0", u64::MAX)
        );
        for invalid in [
            "sdb",
            "sdb@0",
            "sdb@",
            "sdb@1@2",
            "../sda@1",
            "sda;reboot@1",
            "sda@nope",
        ] {
            assert!(matches!(
                split_stable_identifier(invalid),
                Err(WorkerError::UnsafeTarget)
            ));
        }
    }

    #[test]
    fn reads_removable_writable_whole_drive_identity() {
        let temporary = TempDir::new().unwrap();
        let root = temporary.path().join("sdb");
        write(&root.join("removable"), "1\n");
        write(&root.join("ro"), "0\n");
        write(&root.join("diskseq"), "42\n");
        write(&root.join("size"), "2048\n");
        write(&root.join("dev"), "8:16\n");

        assert_eq!(
            inspect_current_drive("sdb", &root).unwrap(),
            CurrentDrive {
                name: "sdb".to_owned(),
                diskseq: 42,
                capacity: 1_048_576,
                device_number: DeviceNumber {
                    major: 8,
                    minor: 16,
                },
            }
        );
    }

    #[test]
    fn missing_diskseq_and_unsafe_flags_fail_closed() {
        let temporary = TempDir::new().unwrap();
        let root = temporary.path().join("sdb");
        write(&root.join("removable"), "1");
        write(&root.join("ro"), "0");
        write(&root.join("size"), "2048");
        write(&root.join("dev"), "8:16");
        assert!(matches!(
            inspect_current_drive("sdb", &root),
            Err(WorkerError::TargetMissing)
        ));

        write(&root.join("diskseq"), "42");
        write(&root.join("removable"), "0");
        assert!(matches!(
            inspect_current_drive("sdb", &root),
            Err(WorkerError::UnsafeTarget)
        ));
        write(&root.join("removable"), "1");
        write(&root.join("ro"), "1");
        assert!(matches!(
            inspect_current_drive("sdb", &root),
            Err(WorkerError::UnsafeTarget)
        ));
    }

    #[test]
    fn collects_partitions_and_transitive_holders() {
        let temporary = TempDir::new().unwrap();
        let block = temporary.path().join("block/sdb");
        let dev_block = temporary.path().join("dev-block");
        write(&block.join("dev"), "8:16");
        write(&block.join("sdb1/partition"), "1");
        write(&block.join("sdb1/dev"), "8:17");
        write(&dev_block.join("8:17/holders/dm-0/dev"), "253:0");
        write(&dev_block.join("253:0/holders/dm-1/dev"), "253:1");

        assert_eq!(
            collect_related_device_numbers(&block, &dev_block).unwrap(),
            BTreeSet::from([
                DeviceNumber {
                    major: 8,
                    minor: 16
                },
                DeviceNumber {
                    major: 8,
                    minor: 17
                },
                DeviceNumber {
                    major: 253,
                    minor: 0,
                },
                DeviceNumber {
                    major: 253,
                    minor: 1,
                },
            ])
        );
    }

    #[test]
    fn parses_escaped_mount_points_and_unmounts_deepest_first() {
        let mountinfo = concat!(
            "40 31 8:17 / /media/SnapDog\\040OS rw,nosuid - vfat /dev/sdb1 rw\n",
            "41 40 8:17 /nested /media/SnapDog\\040OS/nested rw - vfat /dev/sdb1 rw\n",
        );
        let devices = BTreeSet::from([DeviceNumber {
            major: 8,
            minor: 17,
        }]);
        let mut paths = mounted_paths(mountinfo, &devices).unwrap();
        sort_mount_points_for_unmount(&mut paths);
        assert_eq!(
            paths,
            [
                PathBuf::from("/media/SnapDog OS/nested"),
                PathBuf::from("/media/SnapDog OS"),
            ]
        );
    }

    #[test]
    fn rejects_system_mounts_on_physical_or_holder_devices() {
        let devices = BTreeSet::from([
            DeviceNumber {
                major: 8,
                minor: 17,
            },
            DeviceNumber {
                major: 253,
                minor: 0,
            },
        ]);
        let root = "29 1 253:0 / / rw,relatime - ext4 /dev/mapper/root rw\n";
        assert!(matches!(
            reject_protected_mounts(root, &devices),
            Err(WorkerError::UnsafeTarget)
        ));
        let boot = "30 29 8:17 / /boot/efi rw - vfat /dev/sdb1 rw\n";
        assert!(matches!(
            reject_protected_mounts(boot, &devices),
            Err(WorkerError::UnsafeTarget)
        ));
        let removable = "31 29 8:17 / /media/card rw - vfat /dev/sdb1 rw\n";
        assert!(reject_protected_mounts(removable, &devices).is_ok());

        for mount_point in [
            "/etc",
            "/root",
            "/run",
            "/opt",
            "/srv",
            "/tmp",
            "/nix",
            "/var/lib/data",
        ] {
            let record = format!("32 29 8:17 / {mount_point} rw - ext4 /dev/sdb1 rw\n");
            assert!(matches!(
                reject_protected_mounts(&record, &devices),
                Err(WorkerError::UnsafeTarget)
            ));
        }

        for mount_point in ["/media/card", "/mnt/snapdog", "/run/media/user/card"] {
            let record = format!("33 29 8:17 / {mount_point} rw - vfat /dev/sdb1 rw\n");
            assert!(reject_protected_mounts(&record, &devices).is_ok());
        }
    }

    #[test]
    fn rejects_swap_file_backed_by_selected_filesystem() {
        let temporary = TempDir::new().unwrap();
        let swap = temporary.path().join("swapfile");
        write(&swap, "not actually enabled in this test");
        let filesystem_device = decode_linux_device(fs::metadata(&swap).unwrap().dev());
        let devices = BTreeSet::from([filesystem_device]);
        let swaps = format!(
            "Filename Type Size Used Priority\n{} file 1024 0 -2\n",
            swap.display()
        );
        assert!(matches!(
            reject_related_swap(&swaps, &devices),
            Err(WorkerError::UnsafeTarget)
        ));
    }

    #[test]
    fn rejects_uninspectable_active_swap_source() {
        let devices = BTreeSet::new();
        let swaps = "Filename Type Size Used Priority\n/missing/snapdog-swap file 1024 0 -2\n";
        assert!(matches!(
            reject_related_swap(swaps, &devices),
            Err(WorkerError::Platform(_))
        ));
    }

    #[test]
    fn decodes_linux_device_numbers() {
        // Values produced by Linux makedev(3) for representative block-device identities.
        assert_eq!(
            decode_linux_device(0x0810),
            DeviceNumber {
                major: 8,
                minor: 16
            }
        );
        assert_eq!(
            decode_linux_device(0xfd00),
            DeviceNumber {
                major: 253,
                minor: 0,
            }
        );
    }

    #[test]
    fn malformed_mountinfo_fails_closed() {
        assert!(matches!(
            parse_mountinfo("not mountinfo"),
            Err(WorkerError::Platform(_))
        ));
        assert!(matches!(
            decode_mountinfo_field(r"/media/bad\09x"),
            Err(WorkerError::Platform(_))
        ));
    }
}
