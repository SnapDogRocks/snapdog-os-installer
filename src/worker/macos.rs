// SPDX-License-Identifier: GPL-3.0-only

use std::fs::{File, OpenOptions};
use std::process::{Command, Output};

use super::{WorkerDrive, WorkerError, WorkerPlatform, compare_drive, disk_identifier};
use crate::drives;

const DISKUTIL: &str = "/usr/sbin/diskutil";

pub(super) struct MacOsPlatform;

impl WorkerPlatform for MacOsPlatform {
    type Target = File;

    fn validate_staged_image(&mut self, image: &File) -> Result<(), WorkerError> {
        use std::os::unix::fs::MetadataExt;

        let metadata = image
            .metadata()
            .map_err(|error| WorkerError::Platform(error.to_string()))?;
        if !metadata.is_file()
            || metadata.uid() != 0
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() > 1
        {
            return Err(WorkerError::InvalidJob(
                "staged image is not an unlinked, root-owned private file".to_owned(),
            ));
        }
        Ok(())
    }

    fn validate_target(&mut self, selected: &WorkerDrive) -> Result<WorkerDrive, WorkerError> {
        let (selected_disk_id, _) =
            drives::macos_stable_disk_id(&selected.id).ok_or(WorkerError::UnsafeTarget)?;

        // Discovery independently intersects diskutil's physical disks with whole, writable,
        // removable, ejectable IOMedia entries. Matching the captured I/O Registry entry ID,
        // device path, and capacity detects path reuse and media swaps without relying on
        // `diskutil info`, which can block indefinitely for Apple's built-in SDXC reader.
        let current = drives::removable_drives()
            .map_err(|error| WorkerError::Platform(error.to_string()))?
            .into_iter()
            .find(|drive| drive.id == selected.id)
            .map(|drive| WorkerDrive {
                id: drive.id,
                device: drive.device,
                capacity: drive.capacity,
            })
            .ok_or(WorkerError::TargetMissing)?;
        compare_drive(selected, &current)?;
        if selected.device != format!("/dev/{selected_disk_id}") {
            return Err(WorkerError::UnsafeTarget);
        }
        Ok(current)
    }

    fn unmount(&mut self, selected: &WorkerDrive) -> Result<(), WorkerError> {
        compare_drive(selected, &self.validate_target(selected)?)?;
        diskutil(["unmountDisk", selected.device.as_str()])?;
        compare_drive(selected, &self.validate_target(selected)?)
    }

    fn open_target(
        &mut self,
        selected: &WorkerDrive,
        verify: bool,
    ) -> Result<Self::Target, WorkerError> {
        let selected_disk_id = disk_identifier(&selected.id).ok_or(WorkerError::UnsafeTarget)?;
        let raw_path = format!("/dev/r{selected_disk_id}");
        let target = OpenOptions::new()
            .read(verify)
            .write(true)
            .open(raw_path)
            .map_err(|error| WorkerError::Platform(error.to_string()))?;

        // Re-query after opening. If `/dev/rdiskN` was reused between the preceding validation and
        // this open, the stable media identity changes and no bytes are written through this fd.
        compare_drive(selected, &self.validate_target(selected)?)?;
        Ok(target)
    }

    fn eject(&mut self, selected: &WorkerDrive) -> Result<(), WorkerError> {
        // Cleanup can run after a hot-unplug failure. Never apply `diskutil eject` to a new medium
        // that inherited the selected BSD path after the original card disappeared.
        compare_drive(selected, &self.validate_target(selected)?)?;
        diskutil(["eject", selected.device.as_str()]).map(|_| ())
    }
}

fn diskutil<const N: usize>(arguments: [&str; N]) -> Result<Output, WorkerError> {
    let output = Command::new(DISKUTIL)
        .args(arguments)
        .output()
        .map_err(|error| WorkerError::Platform(error.to_string()))?;
    if output.status.success() {
        Ok(output)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(WorkerError::Platform(if stderr.is_empty() {
            format!("diskutil failed with status {}", output.status)
        } else {
            stderr
        }))
    }
}
