// SPDX-License-Identifier: GPL-3.0-only

//! Read-only removable-drive discovery. Writing and elevation live in a separate worker layer.

use std::io;

use thiserror::Error;

use crate::model::Drive;

/// Failures while querying removable physical drives.
#[derive(Debug, Error)]
pub enum DriveError {
    #[error("could not query removable drives: {0}")]
    Io(#[from] io::Error),
    #[error("the operating system returned invalid drive information: {0}")]
    InvalidData(String),
    #[error("drive discovery is not implemented on this platform")]
    Unsupported,
}

/// Enumerate removable, non-system physical drives without modifying them.
pub fn removable_drives() -> Result<Vec<Drive>, DriveError> {
    platform::removable_drives()
}

#[cfg(target_os = "macos")]
mod platform {
    use std::process::Command;

    use serde::Deserialize;

    use super::{Drive, DriveError};

    #[derive(Deserialize)]
    struct DiskList {
        #[serde(rename = "AllDisksAndPartitions", default)]
        disks: Vec<Disk>,
    }

    #[derive(Deserialize)]
    struct Disk {
        #[serde(rename = "DeviceIdentifier")]
        id: String,
        #[serde(rename = "MediaName")]
        name: Option<String>,
        #[serde(rename = "Size")]
        size: Option<u64>,
    }

    pub(super) fn removable_drives() -> Result<Vec<Drive>, DriveError> {
        let output = Command::new("/usr/sbin/diskutil")
            .args(["list", "-plist", "external", "physical"])
            .output()?;
        if !output.status.success() {
            return Err(DriveError::InvalidData(
                String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            ));
        }
        parse_diskutil(&output.stdout)
    }

    fn parse_diskutil(bytes: &[u8]) -> Result<Vec<Drive>, DriveError> {
        let list: DiskList =
            plist::from_bytes(bytes).map_err(|error| DriveError::InvalidData(error.to_string()))?;
        Ok(list
            .disks
            .into_iter()
            .filter_map(|disk| {
                let size = disk.size.filter(|size| *size > 0)?;
                if !valid_identifier(&disk.id) {
                    return None;
                }
                let name = disk
                    .name
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or_else(|| "Removable drive".to_owned());
                Some(Drive {
                    device: format!("/dev/{}", disk.id),
                    id: disk.id,
                    name,
                    capacity: size,
                })
            })
            .collect())
    }

    fn valid_identifier(id: &str) -> bool {
        id.strip_prefix("disk").is_some_and(|suffix| {
            !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parses_external_physical_disk() {
            let document = br#"<?xml version="1.0" encoding="UTF-8"?>
            <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
            <plist version="1.0"><dict><key>AllDisksAndPartitions</key><array><dict>
            <key>DeviceIdentifier</key><string>disk7</string>
            <key>MediaName</key><string>SD Card Reader</string>
            <key>Size</key><integer>31900000000</integer>
            </dict></array></dict></plist>"#;
            let drives = parse_diskutil(document).unwrap();
            assert_eq!(drives.len(), 1);
            assert_eq!(drives[0].device, "/dev/disk7");
            assert_eq!(drives[0].name, "SD Card Reader");
        }

        #[test]
        fn rejects_partition_or_injected_identifier() {
            assert!(!valid_identifier("disk7s1"));
            assert!(!valid_identifier("disk7;rm"));
            assert!(valid_identifier("disk12"));
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::fs;
    use std::path::Path;

    use super::{Drive, DriveError};

    const SECTOR_SIZE: u64 = 512;

    pub(super) fn removable_drives() -> Result<Vec<Drive>, DriveError> {
        let mut drives = Vec::new();
        for entry in fs::read_dir("/sys/block")? {
            let entry = entry?;
            let id = entry.file_name().to_string_lossy().into_owned();
            let root = entry.path();
            if !is_safe_id(&id) || read_trimmed(&root.join("removable"))? != "1" {
                continue;
            }
            let sectors = read_trimmed(&root.join("size"))?
                .parse::<u64>()
                .map_err(|error| DriveError::InvalidData(error.to_string()))?;
            let capacity = sectors
                .checked_mul(SECTOR_SIZE)
                .ok_or_else(|| DriveError::InvalidData("drive capacity overflow".to_owned()))?;
            if capacity == 0 {
                continue;
            }
            let vendor = read_optional(&root.join("device/vendor"));
            let model = read_optional(&root.join("device/model"));
            let name = format!("{vendor} {model}").trim().to_owned();
            drives.push(Drive {
                device: format!("/dev/{id}"),
                id,
                name: if name.is_empty() {
                    "Removable drive".to_owned()
                } else {
                    name
                },
                capacity,
            });
        }
        Ok(drives)
    }

    fn read_trimmed(path: &Path) -> Result<String, DriveError> {
        Ok(fs::read_to_string(path)?.trim().to_owned())
    }

    fn read_optional(path: &Path) -> String {
        fs::read_to_string(path).map_or_else(|_| String::new(), |value| value.trim().to_owned())
    }

    fn is_safe_id(id: &str) -> bool {
        !id.is_empty()
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use std::process::Command;

    use serde::Deserialize;

    use super::{Drive, DriveError};

    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct WindowsDisk {
        friendly_name: String,
        device_id: String,
        size: u64,
        is_boot: bool,
        is_system: bool,
    }

    pub(super) fn removable_drives() -> Result<Vec<Drive>, DriveError> {
        let script = "Get-Disk | Where-Object { -not $_.IsBoot -and -not $_.IsSystem -and ($_.BusType -eq 'USB' -or $_.BusType -eq 'SD') } | Select-Object FriendlyName,@{N='DeviceId';E={'\\\\.\\PHYSICALDRIVE'+$_.Number}},Size,IsBoot,IsSystem | ConvertTo-Json -Compress";
        let output = Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .output()?;
        if !output.status.success() {
            return Err(DriveError::InvalidData(
                String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            ));
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }
        let values: serde_json::Value = serde_json::from_str(trimmed)
            .map_err(|error| DriveError::InvalidData(error.to_string()))?;
        let disks = if values.is_array() {
            serde_json::from_value::<Vec<WindowsDisk>>(values)
        } else {
            serde_json::from_value::<WindowsDisk>(values).map(|disk| vec![disk])
        }
        .map_err(|error| DriveError::InvalidData(error.to_string()))?;
        Ok(disks
            .into_iter()
            .filter(|disk| {
                !disk.is_boot
                    && !disk.is_system
                    && disk.size > 0
                    && valid_device_id(&disk.device_id)
            })
            .map(|disk| Drive {
                id: disk.device_id.clone(),
                device: disk.device_id,
                name: disk.friendly_name,
                capacity: disk.size,
            })
            .collect())
    }

    fn valid_device_id(id: &str) -> bool {
        id.strip_prefix(r"\\.\PHYSICALDRIVE").is_some_and(|suffix| {
            !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
        })
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod platform {
    use super::{Drive, DriveError};

    pub(super) fn removable_drives() -> Result<Vec<Drive>, DriveError> {
        Err(DriveError::Unsupported)
    }
}
