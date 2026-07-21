// SPDX-License-Identifier: GPL-3.0-only

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use super::{
    WorkerDrive, WorkerError, WorkerPlatform, WorkerTarget, compare_drive, disk_identifier,
};
use crate::drives;
use crate::macos_native;
use unix_ancillary::UnixStreamExt;

const TARGET_SETTLE_ATTEMPTS: usize = 25;
const TARGET_SETTLE_DELAY: Duration = Duration::from_millis(100);
const TARGET_FD_REQUEST: &[u8] = b"SNAPDOG_TARGET_FD_REQUEST_V1\n";
const TARGET_FD_RESPONSE: &[u8] = b"SNAPDOG_TARGET_FD_RESPONSE_V1\n";

pub(super) struct MacOsPlatform {
    target_socket: PathBuf,
}

pub(super) struct MacOsTarget {
    file: File,
}

impl Read for MacOsTarget {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.file.read(buffer)
    }
}

impl Write for MacOsTarget {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.file.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Seek for MacOsTarget {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.file.seek(position)
    }
}

impl WorkerTarget for MacOsTarget {
    fn sync_all(&self) -> io::Result<()> {
        // `/dev/rdiskN` is a character device and rejects Rust's macOS full-file sync with ENOTTY.
        // The GUI deliberately obtains this descriptor from authopen with O_SYNC, so every write
        // already has file-integrity completion semantics before it returns to the worker.
        Ok(())
    }
}

impl MacOsPlatform {
    pub(super) const fn new(target_socket: PathBuf) -> Self {
        Self { target_socket }
    }
}

impl WorkerPlatform for MacOsPlatform {
    type Target = MacOsTarget;

    fn validate_staged_image(&mut self, image: &File) -> Result<(), WorkerError> {
        use std::os::unix::fs::MetadataExt;

        let metadata = image
            .metadata()
            .map_err(|error| WorkerError::Platform(error.to_string()))?;
        if !metadata.is_file()
            || metadata.uid() == 0
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() > 1
        {
            return Err(WorkerError::InvalidJob(
                "staged image is not a private file owned by the signed-in user".to_owned(),
            ));
        }
        Ok(())
    }

    fn validate_target(&mut self, selected: &WorkerDrive) -> Result<WorkerDrive, WorkerError> {
        let (selected_disk_id, _) =
            drives::macos_stable_disk_id(&selected.id).ok_or(WorkerError::UnsafeTarget)?;
        tracing::info!(
            stable_id = selected.id,
            device = selected.device,
            capacity = selected.capacity,
            "validating selected macOS target"
        );

        // Disk Arbitration can briefly remove Apple's built-in SDXC reader's corresponding IOMedia
        // entry while an unmount settles
        // while it settles the now-unmounted medium. Require the exact same stable registry ID and
        // safety fields, but tolerate that short transition instead of treating its first snapshot
        // as a hot-unplug. A genuinely removed or replaced card still fails closed after 2.5 s.
        let current = retry_target_lookup(|| {
            drives::macos_removable_drive_by_stable_id(&selected.id)
                .map_err(|error| WorkerError::Platform(error.to_string()))
                .map(|drive| {
                    drive.map(|drive| WorkerDrive {
                        id: drive.id,
                        device: drive.device,
                        capacity: drive.capacity,
                    })
                })
        })?
        .ok_or(WorkerError::TargetMissing)?;
        compare_drive(selected, &current)?;
        if selected.device != format!("/dev/{selected_disk_id}") {
            return Err(WorkerError::UnsafeTarget);
        }
        tracing::info!(
            stable_id = current.id,
            device = current.device,
            capacity = current.capacity,
            "validated selected macOS target"
        );
        Ok(current)
    }

    fn unmount(&mut self, selected: &WorkerDrive) -> Result<(), WorkerError> {
        compare_drive(selected, &self.validate_target(selected)?)?;
        let disk_id = disk_identifier(&selected.id).ok_or(WorkerError::UnsafeTarget)?;
        tracing::info!(device = selected.device, "unmounting all target volumes");
        macos_native::unmount_whole_disk(disk_id)
            .map_err(|error| WorkerError::Platform(error.to_string()))?;
        tracing::info!(
            device = selected.device,
            "Disk Arbitration reported successful unmount"
        );
        compare_drive(selected, &self.validate_target(selected)?)
    }

    fn open_target(
        &mut self,
        selected: &WorkerDrive,
        verify: bool,
    ) -> Result<Self::Target, WorkerError> {
        let selected_disk_id = disk_identifier(&selected.id).ok_or(WorkerError::UnsafeTarget)?;
        let raw_path = format!("/dev/r{selected_disk_id}");
        tracing::info!(raw_path, verify, "opening raw macOS target");
        let file = receive_authorized_target(&self.target_socket, Path::new(&raw_path))?;

        // Re-query after opening. If `/dev/rdiskN` was reused between the preceding validation and
        // this open, the stable media identity changes and no bytes are written through this fd.
        compare_drive(selected, &self.validate_target(selected)?)?;
        Ok(MacOsTarget { file })
    }

    fn eject(&mut self, selected: &WorkerDrive) -> Result<(), WorkerError> {
        // Cleanup can run after a hot-unplug failure. Never eject a new medium
        // that inherited the selected BSD path after the original card disappeared.
        compare_drive(selected, &self.validate_target(selected)?)?;
        let disk_id = disk_identifier(&selected.id).ok_or(WorkerError::UnsafeTarget)?;
        macos_native::eject_disk(disk_id).map_err(|error| WorkerError::Platform(error.to_string()))
    }
}

fn receive_authorized_target(socket: &Path, raw_path: &Path) -> Result<File, WorkerError> {
    let mut stream = UnixStream::connect(socket).map_err(|error| {
        WorkerError::Platform(format!(
            "connecting to authorized target socket failed: {error}"
        ))
    })?;
    stream
        .write_all(TARGET_FD_REQUEST)
        .and_then(|()| stream.flush())
        .map_err(|error| {
            WorkerError::Platform(format!("requesting authorized target failed: {error}"))
        })?;
    let mut received = stream.recv_fds::<1>().map_err(|error| {
        WorkerError::Platform(format!("receiving authorized target failed: {error}"))
    })?;
    if received.data != TARGET_FD_RESPONSE || received.fds.len() != 1 {
        return Err(WorkerError::Platform(
            "authorized target response was invalid".to_owned(),
        ));
    }
    let target = File::from(received.fds.remove(0));
    validate_authorized_descriptor(&target, raw_path)?;
    Ok(target)
}

fn validate_authorized_descriptor(target: &File, raw_path: &Path) -> Result<(), WorkerError> {
    let opened = target.metadata().map_err(|error| {
        WorkerError::Platform(format!("inspecting authorized target failed: {error}"))
    })?;
    let path = fs::symlink_metadata(raw_path).map_err(|error| {
        WorkerError::Platform(format!("inspecting raw target path failed: {error}"))
    })?;
    if !opened.file_type().is_char_device()
        || !path.file_type().is_char_device()
        || opened.dev() != path.dev()
        || opened.ino() != path.ino()
        || opened.rdev() != path.rdev()
    {
        return Err(WorkerError::UnsafeTarget);
    }
    Ok(())
}

fn retry_target_lookup<F>(mut lookup: F) -> Result<Option<WorkerDrive>, WorkerError>
where
    F: FnMut() -> Result<Option<WorkerDrive>, WorkerError>,
{
    for attempt in 0..TARGET_SETTLE_ATTEMPTS {
        match lookup()? {
            Some(drive) => {
                tracing::debug!(attempt = attempt + 1, "target lookup succeeded");
                return Ok(Some(drive));
            }
            None if attempt + 1 < TARGET_SETTLE_ATTEMPTS => {
                tracing::debug!(
                    attempt = attempt + 1,
                    "target lookup returned no matching medium"
                );
                thread::sleep(TARGET_SETTLE_DELAY);
            }
            None => {}
        }
    }
    tracing::warn!(
        attempts = TARGET_SETTLE_ATTEMPTS,
        "target lookup exhausted its settling window"
    );
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_lookup_tolerates_a_transient_registry_gap() {
        let expected = WorkerDrive {
            id: "disk21@4242".to_owned(),
            device: "/dev/disk21".to_owned(),
            capacity: 63_864_569_856,
        };
        let mut attempts = 0;

        let found = retry_target_lookup(|| {
            attempts += 1;
            Ok((attempts == 3).then(|| expected.clone()))
        })
        .unwrap();

        assert_eq!(found, Some(expected));
        assert_eq!(attempts, 3);
    }
}
