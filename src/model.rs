// SPDX-License-Identifier: GPL-3.0-only

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// `SnapDog` OS release channel.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    /// Stable release images.
    #[default]
    Release,
    /// Preview images.
    Beta,
}

impl Channel {
    /// Human-facing channel name.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Release => "Stable",
            Self::Beta => "Beta",
        }
    }

    /// Manifest filename suffix.
    pub const fn manifest_name(self) -> &'static str {
        match self {
            Self::Release => "release",
            Self::Beta => "beta",
        }
    }
}

/// Raspberry Pi models supported by `SnapDog` OS.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Board {
    Pi5,
    Pi4,
    Pi3,
    Zero2W,
}

impl Board {
    pub const ALL: [Self; 4] = [Self::Pi5, Self::Pi4, Self::Pi3, Self::Zero2W];

    /// Manifest identifier.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Pi5 => "pi5",
            Self::Pi4 => "pi4",
            Self::Pi3 => "pi3",
            Self::Zero2W => "zero2w",
        }
    }

    /// Full Raspberry Pi product name.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pi5 => "Raspberry Pi 5",
            Self::Pi4 => "Raspberry Pi 4",
            Self::Pi3 => "Raspberry Pi 3",
            Self::Zero2W => "Raspberry Pi Zero 2 W",
        }
    }
}

/// Download information for one board image.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ImageInfo {
    pub image: String,
    pub sha256: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub compressed_size: Option<u64>,
    #[serde(default)]
    pub uncompressed_size: Option<u64>,
    #[serde(default)]
    pub raw_sha256: Option<String>,
}

/// Release service manifest.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Manifest {
    #[serde(default)]
    pub schema_version: Option<u32>,
    pub channel: Channel,
    pub version: String,
    pub commit: Option<String>,
    pub date: String,
    pub boards: BTreeMap<String, ImageInfo>,
}

/// Bounded, newest-first release list published for one channel.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReleaseCatalog {
    pub schema_version: u32,
    pub channel: Channel,
    pub releases: Vec<Manifest>,
}

impl Manifest {
    /// Return the selected board image, when present.
    pub fn image_for(&self, board: Board) -> Option<&ImageInfo> {
        self.boards.get(board.id())
    }
}

impl ImageInfo {
    /// Prefer the immutable v2 URL while retaining compatibility with v1 image names.
    pub fn download_reference(&self) -> &str {
        self.url.as_deref().unwrap_or(&self.image)
    }
}

/// Image choice confirmed in the first workflow step.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageSelection {
    pub board: Board,
    pub manifest: Manifest,
    pub url: String,
}

/// A removable physical drive that can be selected as the single flash target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Drive {
    pub id: String,
    pub device: String,
    pub name: String,
    pub capacity: u64,
}

impl Drive {
    /// Human-facing drive name including its approximate capacity.
    pub fn label(&self) -> String {
        format!("{} — {}", self.name, format_capacity(self.capacity))
    }
}

fn format_capacity(bytes: u64) -> String {
    const GIGABYTE: u64 = 1_000_000_000;
    const MEGABYTE: u64 = 1_000_000;
    if bytes >= GIGABYTE {
        let whole = bytes / GIGABYTE;
        let decimal = (bytes % GIGABYTE) / 100_000_000;
        format!("{whole}.{decimal} GB")
    } else {
        format!("{} MB", bytes / MEGABYTE)
    }
}

#[cfg(test)]
mod drive_tests {
    use super::*;

    #[test]
    fn formats_drive_capacity() {
        assert_eq!(format_capacity(31_900_000_000), "31.9 GB");
        assert_eq!(format_capacity(512_000_000), "512 MB");
    }

    #[test]
    fn accepts_v1_manifest_and_uses_image_name() {
        let manifest: Manifest = serde_json::from_str(
            r#"{
                "channel":"release",
                "version":"0.12.1",
                "commit":null,
                "date":"2026-07-18T12:51:25Z",
                "boards":{"pi4":{"image":"pi4.img.gz","sha256":"abcd"}}
            }"#,
        )
        .unwrap();
        let image = manifest.image_for(Board::Pi4).unwrap();
        assert_eq!(manifest.schema_version, None);
        assert_eq!(image.download_reference(), "pi4.img.gz");
    }

    #[test]
    fn prefers_immutable_v2_url() {
        let manifest: Manifest = serde_json::from_str(
            r#"{
                "schema_version":2,
                "channel":"release",
                "version":"0.13.0",
                "commit":"abc",
                "date":"2026-07-19T00:00:00Z",
                "boards":{"pi4":{
                    "image":"snapdog-os-pi4-release.img.gz",
                    "url":"https://updates.snapdog.cc/os/images/snapdog-os-pi4-0.13.0.img.gz",
                    "sha256":"abcd",
                    "compressed_size":42,
                    "uncompressed_size":84,
                    "raw_sha256":"ef01"
                }}
            }"#,
        )
        .unwrap();
        let image = manifest.image_for(Board::Pi4).unwrap();
        assert_eq!(manifest.schema_version, Some(2));
        assert_eq!(
            image.download_reference(),
            "https://updates.snapdog.cc/os/images/snapdog-os-pi4-0.13.0.img.gz"
        );
    }
}
