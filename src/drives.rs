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
pub(crate) fn macos_removable_drive_by_stable_id(value: &str) -> Result<Option<Drive>, DriveError> {
    platform::removable_drive_by_stable_id(value)
}

#[cfg(target_os = "macos")]
mod platform {
    use std::collections::BTreeMap;

    use super::{Drive, DriveError};
    use crate::macos_native;

    #[derive(Debug, Eq, PartialEq)]
    struct SafeMedia {
        registry_entry_id: u64,
        name: Option<String>,
        size: u64,
    }

    pub(super) fn removable_drives() -> Result<Vec<Drive>, DriveError> {
        let registry_entries = media_registry_entries()?;
        Ok(registry_entries
            .keys()
            .filter_map(|disk_id| {
                registry_drive(
                    &format_stable_identifier(disk_id, registry_entries[disk_id].registry_entry_id),
                    &registry_entries,
                )
            })
            .collect())
    }

    fn media_registry_entries() -> Result<BTreeMap<String, SafeMedia>, DriveError> {
        let records = macos_native::query_removable_media()?;
        safe_media_records(records)
    }

    fn safe_media_records(
        records: Vec<macos_native::DiskRecord>,
    ) -> Result<BTreeMap<String, SafeMedia>, DriveError> {
        let mut identities = BTreeMap::new();
        let mut registry_ids = std::collections::BTreeSet::new();
        for entry in records {
            tracing::debug!(
                bsd_name = entry.bsd_name,
                name = entry.name.as_deref().unwrap_or("<none>"),
                registry_entry_id = entry.registry_entry_id,
                size = entry.size,
                whole = entry.whole,
                writable = entry.writable,
                removable = entry.removable,
                ejectable = entry.ejectable,
                physical = entry.physical,
                "observed native macOS IOMedia entry"
            );
            if !entry.whole
                || !entry.writable
                || !entry.removable
                || !entry.ejectable
                || !entry.physical
                || !valid_identifier(&entry.bsd_name)
                || entry.registry_entry_id == 0
                || entry.size == 0
            {
                continue;
            }
            if !registry_ids.insert(entry.registry_entry_id) {
                return Err(DriveError::InvalidData(format!(
                    "duplicate I/O Registry entry ID {}",
                    entry.registry_entry_id
                )));
            }
            let bsd_name = entry.bsd_name;
            let media = SafeMedia {
                registry_entry_id: entry.registry_entry_id,
                name: entry.name.filter(|name| !name.trim().is_empty()),
                size: entry.size,
            };
            if identities.insert(bsd_name.clone(), media).is_some() {
                return Err(DriveError::InvalidData(format!(
                    "duplicate I/O Registry identity for {bsd_name}"
                )));
            }
        }
        Ok(identities)
    }

    pub(super) fn removable_drive_by_stable_id(value: &str) -> Result<Option<Drive>, DriveError> {
        let registry_entries = media_registry_entries()?;
        let drive = registry_drive(value, &registry_entries);
        tracing::debug!(
            stable_id = value,
            safe_media_count = registry_entries.len(),
            found = drive.is_some(),
            candidates = ?registry_entries.keys().collect::<Vec<_>>(),
            "looked up removable medium by stable I/O Registry identity"
        );
        Ok(drive)
    }

    fn registry_drive(
        value: &str,
        registry_entries: &BTreeMap<String, SafeMedia>,
    ) -> Option<Drive> {
        let (disk_id, registry_entry_id) = split_stable_identifier(value)?;
        let media = registry_entries.get(disk_id)?;
        if media.registry_entry_id != registry_entry_id {
            return None;
        }
        Some(Drive {
            device: format!("/dev/{disk_id}"),
            id: format_stable_identifier(disk_id, registry_entry_id),
            name: media
                .name
                .clone()
                .unwrap_or_else(|| "Removable drive".to_owned()),
            capacity: media.size,
        })
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

        fn record(
            bsd_name: &str,
            registry_entry_id: u64,
            name: Option<&str>,
        ) -> macos_native::DiskRecord {
            macos_native::DiskRecord {
                bsd_name: bsd_name.to_owned(),
                name: name.map(str::to_owned),
                registry_entry_id,
                size: 63_864_569_856,
                whole: true,
                writable: true,
                removable: true,
                ejectable: true,
                physical: true,
            }
        }

        #[test]
        fn discovers_builtin_sd_and_excludes_fixed_and_virtual_media() {
            let mut internal = record("disk0", 4_000, Some("Internal SSD"));
            internal.removable = false;
            internal.ejectable = false;
            let mut partition = record("disk21s1", 4_243, None);
            partition.whole = false;
            let mut virtual_media = record("disk4", 4_444, Some("Writable disk image"));
            virtual_media.physical = false;

            let media = safe_media_records(vec![
                internal,
                record("disk21", 4_242, Some("Apple SDXC Reader Media")),
                partition,
                virtual_media,
            ])
            .unwrap();
            assert_eq!(media.len(), 1);
            let drive = registry_drive("disk21@4242", &media).unwrap();
            assert_eq!(drive.device, "/dev/disk21");
            assert_eq!(drive.id, "disk21@4242");
            assert_eq!(drive.name, "Apple SDXC Reader Media");
            assert_eq!(drive.capacity, 63_864_569_856);
        }

        #[test]
        fn missing_media_safety_flag_fails_closed() {
            let mut incomplete = record("disk7", 4_242, None);
            incomplete.ejectable = false;
            let identities = safe_media_records(vec![incomplete]).unwrap();
            assert!(identities.is_empty());
            assert_eq!(
                split_stable_identifier("disk7@4242"),
                Some(("disk7", 4_242))
            );
            assert_eq!(split_stable_identifier("disk7s1@4242"), None);
        }

        #[test]
        fn stable_registry_identity_survives_unmount() {
            let identities = safe_media_records(vec![record(
                "disk21",
                4_242,
                Some("Apple SDXC Reader Media"),
            )])
            .unwrap();
            let drive = registry_drive("disk21@4242", &identities).unwrap();

            assert_eq!(drive.device, "/dev/disk21");
            assert_eq!(drive.id, "disk21@4242");
            assert_eq!(drive.capacity, 63_864_569_856);
            assert_eq!(drive.name, "Apple SDXC Reader Media");
            assert!(registry_drive("disk21@4243", &identities).is_none());
        }

        #[test]
        fn duplicate_registry_entry_id_is_rejected() {
            assert!(matches!(
                safe_media_records(vec![record("disk7", 4_242, None), record("disk8", 4_242, None)]),
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
            if !is_safe_id(&id)
                || read_trimmed(&root.join("removable"))? != "1"
                || read_trimmed(&root.join("ro"))? != "0"
            {
                continue;
            }
            let diskseq_text = read_trimmed(&root.join("diskseq")).map_err(|error| {
                DriveError::InvalidData(format!(
                    "Linux kernel 5.15 or newer is required for safe removable-drive identity: {error}"
                ))
            })?;
            let diskseq = diskseq_text.parse::<u64>().map_err(|error| {
                DriveError::InvalidData(format!("invalid kernel disk sequence number: {error}"))
            })?;
            if diskseq == 0 {
                return Err(DriveError::InvalidData(
                    "kernel returned an unsafe zero disk sequence number".to_owned(),
                ));
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
                id: format!("{id}@{diskseq}"),
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
            && id.len() <= 128
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use sha2::{Digest, Sha256};

    use super::{Drive, DriveError};
    use crate::windows_native::DiskRecord;

    const STABLE_ID_VERSION: &str = "windows-disk-v2";
    const PHYSICAL_DRIVE_PREFIX: &str = r"\\.\PHYSICALDRIVE";
    #[derive(Clone, Copy)]
    struct BoolFlag(bool);

    impl BoolFlag {
        const fn is_set(self) -> bool {
            self.0
        }
    }

    struct WindowsDisk {
        number: u32,
        friendly_name: String,
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

    impl From<DiskRecord> for WindowsDisk {
        fn from(disk: DiskRecord) -> Self {
            Self {
                number: disk.number,
                friendly_name: disk.friendly_name,
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
            }
        }
    }

    impl WindowsDisk {
        fn normalize(&mut self) {
            self.friendly_name = self.friendly_name.trim().to_owned();
            self.path = self.path.trim().to_owned();
            self.unique_id = self.unique_id.trim().to_owned();
            self.serial_number = self.serial_number.trim().to_owned();
            self.bus_type = self.bus_type.trim().to_ascii_uppercase();
        }

        fn safe_removable(&self) -> bool {
            !self.is_boot.is_set()
                && !self.is_system.is_set()
                && !self.is_offline.is_set()
                && !self.is_read_only.is_set()
                && self.size > 0
                && valid_sector_geometry(
                    self.size,
                    self.logical_sector_size,
                    self.physical_sector_size,
                )
                && !self.path.is_empty()
                && (!self.unique_id.is_empty() || !self.serial_number.is_empty())
                && match self.bus_type.as_str() {
                    // The storage bus itself is an unambiguous removable-card discriminator for
                    // built-in SD/MMC readers. USB also carries ordinary external HDDs/SSDs, so
                    // it requires Win32_DiskDrive capability 7 (supports removable media).
                    "SD" | "MMC" => true,
                    "USB" => self.supports_removable_media.is_set(),
                    _ => false,
                }
        }

        fn fingerprint(&self) -> Option<String> {
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
                let length = u64::try_from(field.len()).ok()?;
                digest.update(length.to_le_bytes());
                digest.update(field);
            }
            Some(hex::encode(digest.finalize()))
        }
    }

    pub(super) fn removable_drives() -> Result<Vec<Drive>, DriveError> {
        let mut disks: Vec<WindowsDisk> = crate::windows_native::query_disks()?
            .into_iter()
            .map(WindowsDisk::from)
            .collect();
        let mut drives = Vec::new();
        for disk in &mut disks {
            disk.normalize();
            if !disk.safe_removable() {
                continue;
            }
            let Some(fingerprint) = disk.fingerprint() else {
                continue;
            };
            let device = format!("{PHYSICAL_DRIVE_PREFIX}{}", disk.number);
            drives.push(Drive {
                id: format!("{device}@{fingerprint}"),
                device,
                name: if disk.friendly_name.is_empty() {
                    "Removable drive".to_owned()
                } else {
                    disk.friendly_name.clone()
                },
                capacity: disk.size,
            });
        }
        Ok(drives)
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

        fn disk(bus_type: &str, supports_removable_media: bool) -> WindowsDisk {
            WindowsDisk {
                number: 7,
                friendly_name: "SnapDog test card".to_owned(),
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
                bus_type: bus_type.to_owned(),
                supports_removable_media: BoolFlag(supports_removable_media),
            }
        }

        #[test]
        fn excludes_ordinary_usb_fixed_media() {
            assert!(!disk("USB", false).safe_removable());
            assert!(disk("USB", true).safe_removable());
        }

        #[test]
        fn preserves_native_sd_and_mmc_readers() {
            assert!(disk("SD", false).safe_removable());
            assert!(disk("MMC", false).safe_removable());
        }

        #[test]
        fn rejects_ambiguous_or_unsupported_sector_geometry() {
            let mut candidate = disk("SD", false);
            candidate.logical_sector_size = 0;
            assert!(!candidate.safe_removable());

            let mut candidate = disk("SD", false);
            candidate.physical_sector_size = 131_072;
            assert!(!candidate.safe_removable());

            let mut candidate = disk("SD", false);
            candidate.size += 1;
            assert!(!candidate.safe_removable());
        }

        #[test]
        fn native_records_preserve_safety_fields() {
            let record = DiskRecord {
                number: 7,
                friendly_name: "SnapDog test card".to_owned(),
                path: "native-path".to_owned(),
                unique_id: "MEDIA-1234".to_owned(),
                serial_number: "SERIAL-5678".to_owned(),
                size: 32_000_000_000,
                logical_sector_size: 512,
                physical_sector_size: 4_096,
                is_boot: false,
                is_system: false,
                is_offline: false,
                is_read_only: false,
                bus_type: "SD".to_owned(),
                supports_removable_media: false,
            };
            assert!(WindowsDisk::from(record).safe_removable());
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod platform {
    use super::{Drive, DriveError};

    pub(super) fn removable_drives() -> Result<Vec<Drive>, DriveError> {
        Err(DriveError::Unsupported)
    }
}
