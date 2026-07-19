// SPDX-License-Identifier: GPL-3.0-only

use std::fs::{File, OpenOptions};
use std::process::{Command, Output};

use serde::Deserialize;

use super::{WorkerDrive, WorkerError, WorkerPlatform, compare_drive, disk_identifier};
use crate::drives;

const DISKUTIL: &str = "/usr/sbin/diskutil";

#[derive(Default)]
pub(super) struct MacOsPlatform {
    stable_identity: Option<StableMediaIdentity>,
}

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
        let (selected_disk_id, selected_registry_entry_id) =
            drives::macos_stable_disk_id(&selected.id).ok_or(WorkerError::UnsafeTarget)?;

        // This call itself is constrained to `external physical`, independently of the GUI's
        // earlier discovery. Matching every captured field detects path reuse and media swaps.
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

        let output = diskutil(["info", "-plist", selected_disk_id])?;
        let info: DiskInfo = plist::from_bytes(&output.stdout)
            .map_err(|error| WorkerError::Platform(error.to_string()))?;
        let capacity = info.size.or(info.total_size).unwrap_or_default();
        let physical = info.virtual_or_physical.as_deref() == Some("Physical");
        if info.device_identifier != selected_disk_id
            || info.device_node != selected.device
            || info.whole_disk != Some(true)
            || info.internal != Some(false)
            || info.external != Some(true)
            || info.writable != Some(true)
            || !physical
            || capacity != selected.capacity
        {
            return Err(WorkerError::UnsafeTarget);
        }
        let stable_identity =
            StableMediaIdentity::from_disk_info(&info, selected_registry_entry_id)?;
        if let Some(expected) = &self.stable_identity {
            if expected != &stable_identity {
                return Err(WorkerError::TargetChanged);
            }
        } else {
            self.stable_identity = Some(stable_identity);
        }
        Ok(current)
    }

    fn unmount(&mut self, selected: &WorkerDrive) -> Result<(), WorkerError> {
        diskutil(["unmountDisk", selected.device.as_str()]).map(|_| ())
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
        diskutil(["eject", selected.device.as_str()]).map(|_| ())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StableMediaIdentity {
    device_identifier: String,
    registry_entry_id: u64,
    device_tree_path: Option<String>,
    media_uuid: Option<String>,
}

impl StableMediaIdentity {
    fn from_disk_info(info: &DiskInfo, registry_entry_id: u64) -> Result<Self, WorkerError> {
        let device_tree_path = nonempty(info.device_tree_path.as_deref());
        let media_uuid = nonempty(info.media_uuid.as_deref());
        if device_tree_path.is_none() && media_uuid.is_none() {
            return Err(WorkerError::UnsafeTarget);
        }
        Ok(Self {
            device_identifier: info.device_identifier.clone(),
            registry_entry_id,
            device_tree_path,
            media_uuid,
        })
    }
}

fn nonempty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

#[derive(Debug, Deserialize)]
struct DiskInfo {
    #[serde(rename = "DeviceIdentifier")]
    device_identifier: String,
    #[serde(rename = "DeviceNode")]
    device_node: String,
    #[serde(rename = "WholeDisk", default)]
    whole_disk: Option<bool>,
    #[serde(rename = "Internal", default)]
    internal: Option<bool>,
    #[serde(rename = "RemovableMediaOrExternalDevice", default)]
    external: Option<bool>,
    #[serde(rename = "Writable", default)]
    writable: Option<bool>,
    #[serde(rename = "VirtualOrPhysical")]
    virtual_or_physical: Option<String>,
    #[serde(rename = "Size")]
    size: Option<u64>,
    #[serde(rename = "TotalSize")]
    total_size: Option<u64>,
    #[serde(rename = "DeviceTreePath")]
    device_tree_path: Option<String>,
    #[serde(rename = "MediaUUID")]
    media_uuid: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_safe_external_whole_disk_info() {
        let document = br#"<?xml version="1.0" encoding="UTF-8"?>
        <plist version="1.0"><dict>
        <key>DeviceIdentifier</key><string>disk7</string>
        <key>DeviceNode</key><string>/dev/disk7</string>
        <key>WholeDisk</key><true/>
        <key>Internal</key><false/>
        <key>RemovableMediaOrExternalDevice</key><true/>
        <key>Writable</key><true/>
        <key>VirtualOrPhysical</key><string>Physical</string>
        <key>Size</key><integer>31900000000</integer>
        <key>DeviceTreePath</key><string>IODeviceTree:/arm-io/usb/sd-reader</string>
        <key>MediaUUID</key><string>7BD1717B-7229-4D31-A6B8-E3D4FAE2EE91</string>
        </dict></plist>"#;
        let info: DiskInfo = plist::from_bytes(document).unwrap();
        assert_eq!(info.whole_disk, Some(true));
        assert_eq!(info.internal, Some(false));
        assert_eq!(info.external, Some(true));
        assert_eq!(info.writable, Some(true));
        assert_eq!(info.virtual_or_physical.as_deref(), Some("Physical"));
        assert_eq!(info.size, Some(31_900_000_000));
        let identity = StableMediaIdentity::from_disk_info(&info, 4_242).unwrap();
        assert_eq!(identity.device_identifier, "disk7");
        assert_eq!(identity.registry_entry_id, 4_242);
        assert_eq!(
            identity.device_tree_path.as_deref(),
            Some("IODeviceTree:/arm-io/usb/sd-reader")
        );
        assert_eq!(
            identity.media_uuid.as_deref(),
            Some("7BD1717B-7229-4D31-A6B8-E3D4FAE2EE91")
        );
    }

    #[test]
    fn omitted_internal_flag_defaults_to_unsafe() {
        let document = br#"<?xml version="1.0" encoding="UTF-8"?>
        <plist version="1.0"><dict>
        <key>DeviceIdentifier</key><string>disk7</string>
        <key>DeviceNode</key><string>/dev/disk7</string>
        </dict></plist>"#;
        let info: DiskInfo = plist::from_bytes(document).unwrap();
        assert_eq!(info.internal, None);
        assert_eq!(info.whole_disk, None);
        assert_eq!(info.external, None);
        assert_eq!(info.writable, None);
        assert!(matches!(
            StableMediaIdentity::from_disk_info(&info, 4_242),
            Err(WorkerError::UnsafeTarget)
        ));
    }

    #[test]
    fn media_swap_changes_stable_identity() {
        let first = DiskInfo {
            device_identifier: "disk7".to_owned(),
            device_node: "/dev/disk7".to_owned(),
            whole_disk: Some(true),
            internal: Some(false),
            external: Some(true),
            writable: Some(true),
            virtual_or_physical: Some("Physical".to_owned()),
            size: Some(31_900_000_000),
            total_size: None,
            device_tree_path: Some("IODeviceTree:/arm-io/usb/sd-reader".to_owned()),
            media_uuid: Some("FIRST".to_owned()),
        };
        let second = DiskInfo {
            media_uuid: Some("SECOND".to_owned()),
            ..first
        };

        let second_identity = StableMediaIdentity::from_disk_info(&second, 4_243).unwrap();
        let first_identity = StableMediaIdentity::from_disk_info(
            &DiskInfo {
                media_uuid: Some("FIRST".to_owned()),
                ..second
            },
            4_242,
        )
        .unwrap();
        assert_ne!(second_identity, first_identity);
    }
}
