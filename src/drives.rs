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
pub(crate) fn macos_stable_disk_id(value: &str) -> Option<(&str, u64)> {
    platform::split_stable_identifier(value)
}

#[cfg(target_os = "macos")]
mod platform {
    use std::collections::BTreeMap;
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

    #[derive(Deserialize)]
    struct IoMedia {
        #[serde(rename = "BSD Name")]
        bsd_name: Option<String>,
        #[serde(rename = "IORegistryEntryName")]
        name: Option<String>,
        #[serde(rename = "IORegistryEntryID")]
        registry_entry_id: Option<u64>,
        #[serde(rename = "Size")]
        size: Option<u64>,
        #[serde(rename = "Whole")]
        whole: Option<bool>,
        #[serde(rename = "Writable")]
        writable: Option<bool>,
        #[serde(rename = "Removable")]
        removable: Option<bool>,
        #[serde(rename = "Ejectable")]
        ejectable: Option<bool>,
    }

    #[derive(Debug, Eq, PartialEq)]
    struct SafeMedia {
        registry_entry_id: u64,
        name: Option<String>,
        size: u64,
    }

    pub(super) fn removable_drives() -> Result<Vec<Drive>, DriveError> {
        let output = Command::new("/usr/sbin/diskutil")
            .args(["list", "-plist", "physical"])
            .output()?;
        if !output.status.success() {
            return Err(DriveError::InvalidData(
                String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            ));
        }
        let registry_entries = media_registry_entries()?;
        parse_diskutil(&output.stdout, &registry_entries)
    }

    fn media_registry_entries() -> Result<BTreeMap<String, SafeMedia>, DriveError> {
        let output = Command::new("/usr/sbin/ioreg")
            .args(["-r", "-c", "IOMedia", "-a"])
            .output()?;
        if !output.status.success() {
            return Err(DriveError::InvalidData(
                String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            ));
        }
        parse_ioreg(&output.stdout)
    }

    fn parse_ioreg(bytes: &[u8]) -> Result<BTreeMap<String, SafeMedia>, DriveError> {
        let entries: Vec<IoMedia> =
            plist::from_bytes(bytes).map_err(|error| DriveError::InvalidData(error.to_string()))?;
        let mut identities = BTreeMap::new();
        let mut registry_ids = std::collections::BTreeSet::new();
        for entry in entries {
            let (Some(bsd_name), Some(registry_entry_id), Some(size)) =
                (entry.bsd_name, entry.registry_entry_id, entry.size)
            else {
                continue;
            };
            if entry.whole != Some(true)
                || entry.writable != Some(true)
                || entry.removable != Some(true)
                || entry.ejectable != Some(true)
                || !valid_identifier(&bsd_name)
                || registry_entry_id == 0
                || size == 0
            {
                continue;
            }
            if !registry_ids.insert(registry_entry_id) {
                return Err(DriveError::InvalidData(format!(
                    "duplicate I/O Registry entry ID {registry_entry_id}"
                )));
            }
            let media = SafeMedia {
                registry_entry_id,
                name: entry.name.filter(|name| !name.trim().is_empty()),
                size,
            };
            if identities.insert(bsd_name.clone(), media).is_some() {
                return Err(DriveError::InvalidData(format!(
                    "duplicate I/O Registry identity for {bsd_name}"
                )));
            }
        }
        Ok(identities)
    }

    fn parse_diskutil(
        bytes: &[u8],
        registry_entries: &BTreeMap<String, SafeMedia>,
    ) -> Result<Vec<Drive>, DriveError> {
        let list: DiskList =
            plist::from_bytes(bytes).map_err(|error| DriveError::InvalidData(error.to_string()))?;
        let mut drives = Vec::new();
        for disk in list.disks {
            let Some(size) = disk.size.filter(|size| *size > 0) else {
                continue;
            };
            if !valid_identifier(&disk.id) {
                continue;
            }
            let Some(media) = registry_entries.get(&disk.id) else {
                continue;
            };
            if media.size != size {
                return Err(DriveError::InvalidData(format!(
                    "size mismatch for {} between diskutil ({size}) and I/O Registry ({})",
                    disk.id, media.size
                )));
            }
            let name = disk
                .name
                .filter(|name| !name.trim().is_empty())
                .or_else(|| media.name.clone())
                .unwrap_or_else(|| "Removable drive".to_owned());
            drives.push(Drive {
                device: format!("/dev/{}", disk.id),
                id: format_stable_identifier(&disk.id, media.registry_entry_id),
                name,
                capacity: size,
            });
        }
        Ok(drives)
    }

    fn valid_identifier(id: &str) -> bool {
        id.strip_prefix("disk").is_some_and(|suffix| {
            !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
        })
    }

    fn format_stable_identifier(disk_id: &str, registry_entry_id: u64) -> String {
        format!("{disk_id}@{registry_entry_id}")
    }

    pub(super) fn split_stable_identifier(value: &str) -> Option<(&str, u64)> {
        let (disk_id, registry_entry_id) = value.split_once('@')?;
        if !valid_identifier(disk_id)
            || registry_entry_id.is_empty()
            || !registry_entry_id.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        let registry_entry_id = registry_entry_id.parse().ok()?;
        (registry_entry_id != 0).then_some((disk_id, registry_entry_id))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn discovers_builtin_sd_and_excludes_fixed_and_virtual_media() {
            let registry_document = br#"<?xml version="1.0" encoding="UTF-8"?>
            <plist version="1.0"><array>
            <dict><key>BSD Name</key><string>disk0</string>
            <key>IORegistryEntryName</key><string>Internal SSD</string>
            <key>IORegistryEntryID</key><integer>4000</integer>
            <key>Size</key><integer>4000000000000</integer>
            <key>Whole</key><true/><key>Writable</key><true/>
            <key>Removable</key><false/><key>Ejectable</key><false/></dict>
            <dict><key>BSD Name</key><string>disk21</string>
            <key>IORegistryEntryName</key><string>Apple SDXC Reader Media</string>
            <key>IORegistryEntryID</key><integer>4242</integer>
            <key>Size</key><integer>63864569856</integer>
            <key>Whole</key><true/><key>Writable</key><true/>
            <key>Removable</key><true/><key>Ejectable</key><true/></dict>
            <dict><key>BSD Name</key><string>disk21s1</string>
            <key>IORegistryEntryID</key><integer>4243</integer>
            <key>Size</key><integer>268435456</integer>
            <key>Whole</key><false/><key>Writable</key><true/>
            <key>Removable</key><true/><key>Ejectable</key><true/></dict>
            <dict><key>BSD Name</key><string>disk4</string>
            <key>IORegistryEntryName</key><string>Writable disk image</string>
            <key>IORegistryEntryID</key><integer>4444</integer>
            <key>Size</key><integer>1000000000</integer>
            <key>Whole</key><true/><key>Writable</key><true/>
            <key>Removable</key><true/><key>Ejectable</key><true/></dict>
            </array></plist>"#;
            let registry_entries = parse_ioreg(registry_document).unwrap();
            let diskutil_document = br#"<?xml version="1.0" encoding="UTF-8"?>
            <plist version="1.0"><dict><key>AllDisksAndPartitions</key><array>
            <dict><key>DeviceIdentifier</key><string>disk0</string>
            <key>Size</key><integer>4000000000000</integer></dict>
            <dict><key>DeviceIdentifier</key><string>disk21</string>
            <key>Size</key><integer>63864569856</integer></dict>
            </array></dict></plist>"#;
            let drives = parse_diskutil(diskutil_document, &registry_entries).unwrap();
            assert_eq!(drives.len(), 1);
            assert_eq!(drives[0].device, "/dev/disk21");
            assert_eq!(drives[0].id, "disk21@4242");
            assert_eq!(drives[0].name, "Apple SDXC Reader Media");
            assert_eq!(drives[0].capacity, 63_864_569_856);
        }

        #[test]
        fn missing_media_safety_flag_fails_closed() {
            let document = br#"<?xml version="1.0" encoding="UTF-8"?>
            <plist version="1.0"><array>
            <dict><key>BSD Name</key><string>disk7</string>
            <key>IORegistryEntryID</key><integer>4242</integer>
            <key>Size</key><integer>31900000000</integer>
            <key>Whole</key><true/><key>Writable</key><true/>
            <key>Removable</key><true/></dict>
            </array></plist>"#;
            let identities = parse_ioreg(document).unwrap();
            assert!(identities.is_empty());
            assert_eq!(
                split_stable_identifier("disk7@4242"),
                Some(("disk7", 4_242))
            );
            assert_eq!(split_stable_identifier("disk7s1@4242"), None);
        }

        #[test]
        fn rejects_diskutil_and_registry_size_mismatch() {
            let registry_entries = BTreeMap::from([(
                "disk7".to_owned(),
                SafeMedia {
                    registry_entry_id: 4_242,
                    name: None,
                    size: 31_900_000_000,
                },
            )]);
            let document = br#"<?xml version="1.0" encoding="UTF-8"?>
            <plist version="1.0"><dict><key>AllDisksAndPartitions</key><array><dict>
            <key>DeviceIdentifier</key><string>disk7</string>
            <key>Size</key><integer>31900000001</integer>
            </dict></array></dict></plist>"#;
            assert!(matches!(
                parse_diskutil(document, &registry_entries),
                Err(DriveError::InvalidData(message)) if message.contains("size mismatch")
            ));
        }

        #[test]
        fn duplicate_registry_entry_id_is_rejected() {
            let document = br#"<?xml version="1.0" encoding="UTF-8"?>
            <plist version="1.0"><array>
            <dict><key>BSD Name</key><string>disk7</string>
            <key>IORegistryEntryID</key><integer>4242</integer>
            <key>Size</key><integer>31900000000</integer>
            <key>Whole</key><true/><key>Writable</key><true/>
            <key>Removable</key><true/><key>Ejectable</key><true/></dict>
            <dict><key>BSD Name</key><string>disk8</string>
            <key>IORegistryEntryID</key><integer>4242</integer>
            <key>Size</key><integer>31900000000</integer>
            <key>Whole</key><true/><key>Writable</key><true/>
            <key>Removable</key><true/><key>Ejectable</key><true/></dict>
            </array></plist>"#;
            assert!(matches!(
                parse_ioreg(document),
                Err(DriveError::InvalidData(message)) if message.contains("duplicate I/O Registry entry ID")
            ));
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
