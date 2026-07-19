// SPDX-License-Identifier: GPL-3.0-only

use std::time::Duration;

use reqwest::{StatusCode, blocking::Client};
use semver::Version;
use thiserror::Error;

use crate::model::{Board, Channel, Manifest, ReleaseCatalog};

pub const IMAGE_BASE_URL: &str = "https://updates.snapdog.cc/os/images/";

/// Catalog loading errors presented by the application.
#[derive(Debug, Error)]
pub enum CatalogError {
    #[error("could not contact the SnapDog release service: {0}")]
    Request(#[from] reqwest::Error),
    #[error("the release service returned an invalid image URL: {0}")]
    InvalidImageUrl(String),
    #[error("the release service returned an invalid manifest: {0}")]
    InvalidManifest(String),
}

/// HTTP-backed `SnapDog` OS release catalog.
#[derive(Clone)]
pub struct CatalogClient {
    client: Client,
}

impl CatalogClient {
    /// Create a catalog client with bounded network timeouts.
    pub fn new() -> Result<Self, CatalogError> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            .https_only(true)
            .user_agent(concat!("snapdog-os-installer/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self { client })
    }

    /// Fetch the latest image manifest for one channel.
    pub fn fetch_latest(&self, channel: Channel) -> Result<Manifest, CatalogError> {
        let url = format!("{IMAGE_BASE_URL}latest-{}.json", channel.manifest_name());
        let manifest: Manifest = self
            .client
            .get(url)
            .header(reqwest::header::CACHE_CONTROL, "no-cache")
            .send()?
            .error_for_status()?
            .json()?;
        validate_download_manifest(&manifest)?;
        Ok(manifest)
    }

    /// Fetch all safely installable releases for a channel.
    ///
    /// Older servers that do not publish catalogs yet fall back to the latest
    /// manifest. Invalid catalogs never fall back silently.
    pub fn fetch_catalog(&self, channel: Channel) -> Result<Vec<Manifest>, CatalogError> {
        let url = format!("{IMAGE_BASE_URL}catalog-{}.json", channel.manifest_name());
        let response = self
            .client
            .get(url)
            .header(reqwest::header::CACHE_CONTROL, "no-cache")
            .send()?;
        if response.status() == StatusCode::NOT_FOUND {
            return self.fetch_latest(channel).map(|manifest| vec![manifest]);
        }
        let catalog: ReleaseCatalog = response.error_for_status()?.json()?;
        validate_catalog(&catalog, channel)?;
        Ok(catalog.releases)
    }

    /// Resolve an image name or absolute URL without accepting other schemes.
    pub fn image_url(image: &str) -> Result<String, CatalogError> {
        let base = reqwest::Url::parse(IMAGE_BASE_URL)
            .map_err(|_| CatalogError::InvalidImageUrl(image.to_owned()))?;
        let url = base
            .join(image)
            .map_err(|_| CatalogError::InvalidImageUrl(image.to_owned()))?;
        if url.scheme() != "https" {
            return Err(CatalogError::InvalidImageUrl(image.to_owned()));
        }
        Ok(url.into())
    }
}

fn validate_catalog(catalog: &ReleaseCatalog, channel: Channel) -> Result<(), CatalogError> {
    if catalog.schema_version != 1 {
        return Err(CatalogError::InvalidManifest(format!(
            "unsupported catalog schema version {}",
            catalog.schema_version
        )));
    }
    if catalog.channel != channel {
        return Err(CatalogError::InvalidManifest(format!(
            "catalog channel {:?} does not match {:?}",
            catalog.channel, channel
        )));
    }
    if catalog.releases.is_empty() || catalog.releases.len() > 20 {
        return Err(CatalogError::InvalidManifest(
            "catalog must contain between 1 and 20 releases".to_owned(),
        ));
    }

    let mut previous: Option<Version> = None;
    let mut versions = std::collections::BTreeSet::new();
    for manifest in &catalog.releases {
        validate_download_manifest(manifest)?;
        if manifest.channel != channel {
            return Err(CatalogError::InvalidManifest(format!(
                "release {} belongs to the wrong channel",
                manifest.version
            )));
        }
        let version = Version::parse(&manifest.version).map_err(|_| {
            CatalogError::InvalidManifest(format!(
                "invalid SnapDog OS version {:?}",
                manifest.version
            ))
        })?;
        if !versions.insert(manifest.version.clone()) {
            return Err(CatalogError::InvalidManifest(format!(
                "duplicate release version {}",
                manifest.version
            )));
        }
        if previous.as_ref().is_some_and(|newer| newer < &version) {
            return Err(CatalogError::InvalidManifest(
                "catalog releases are not sorted newest first".to_owned(),
            ));
        }
        previous = Some(version);
    }
    Ok(())
}

fn validate_download_manifest(manifest: &Manifest) -> Result<(), CatalogError> {
    match manifest.schema_version {
        // Schema v1 remains readable for compatibility. The UI's destructive
        // pipeline separately requires v2 integrity metadata before flashing.
        None => return Ok(()),
        Some(2) => {}
        Some(version) => {
            return Err(CatalogError::InvalidManifest(format!(
                "unsupported schema version {version}"
            )));
        }
    }

    Version::parse(&manifest.version).map_err(|_| {
        CatalogError::InvalidManifest(format!("invalid SnapDog OS version {:?}", manifest.version))
    })?;

    for board in Board::ALL {
        let image = manifest.image_for(board).ok_or_else(|| {
            CatalogError::InvalidManifest(format!("missing image for {}", board.id()))
        })?;
        let value = image.url.as_deref().ok_or_else(|| {
            CatalogError::InvalidManifest(format!("missing immutable image URL for {}", board.id()))
        })?;
        validate_immutable_url(value, board, &manifest.version)?;
    }
    Ok(())
}

fn validate_immutable_url(value: &str, board: Board, version: &str) -> Result<(), CatalogError> {
    let url = reqwest::Url::parse(value).map_err(|_| {
        CatalogError::InvalidManifest(format!("{} image URL must be absolute HTTPS", board.id()))
    })?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(CatalogError::InvalidManifest(format!(
            "{} image URL must be an unadorned absolute HTTPS URL",
            board.id()
        )));
    }

    let encoded_name = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .filter(|segment| !segment.is_empty())
        .ok_or_else(|| {
            CatalogError::InvalidManifest(format!("{} image URL is missing a filename", board.id()))
        })?;
    let name = percent_decode(encoded_name).ok_or_else(|| {
        CatalogError::InvalidManifest(format!(
            "{} image URL contains an invalid filename",
            board.id()
        ))
    })?;
    let expected = format!("snapdog-os-{}-{version}.img.gz", board.id());
    if name != expected {
        return Err(CatalogError::InvalidManifest(format!(
            "{} image URL must end in {expected:?}",
            board.id()
        )));
    }
    Ok(())
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = *bytes.get(index + 1)?;
            let low = *bytes.get(index + 2)?;
            decoded.push(
                hex_digit(high)?
                    .checked_mul(16)?
                    .checked_add(hex_digit(low)?)?,
            );
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

const fn hex_digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::model::{ImageInfo, ReleaseCatalog};

    fn manifest(schema_version: Option<u32>, version: &str) -> Manifest {
        let boards = Board::ALL
            .into_iter()
            .map(|board| {
                let filename = format!("snapdog-os-{}-{version}.img.gz", board.id());
                (
                    board.id().to_owned(),
                    ImageInfo {
                        image: format!("snapdog-os-{}-release.img.gz", board.id()),
                        sha256: Some("a".repeat(64)),
                        url: Some(format!("{IMAGE_BASE_URL}{filename}")),
                        compressed_size: Some(42),
                        uncompressed_size: Some(84),
                        raw_sha256: Some("b".repeat(64)),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        Manifest {
            schema_version,
            channel: Channel::Release,
            version: version.to_owned(),
            commit: None,
            date: "2026-07-19T00:00:00Z".to_owned(),
            boards,
        }
    }

    #[test]
    fn resolves_relative_image_name() {
        let url = CatalogClient::image_url("snapdog-os-pi4-release.img.gz").unwrap();
        assert_eq!(
            url,
            "https://updates.snapdog.cc/os/images/snapdog-os-pi4-release.img.gz"
        );
    }

    #[test]
    fn keeps_versioned_https_url() {
        let url = CatalogClient::image_url(
            "https://github.com/SnapDogRocks/snapdog-os/releases/download/v0.12.1/image.img.gz",
        )
        .unwrap();
        assert_eq!(
            url,
            "https://github.com/SnapDogRocks/snapdog-os/releases/download/v0.12.1/image.img.gz"
        );
    }

    #[test]
    fn rejects_non_https_url() {
        assert!(CatalogClient::image_url("http://example.com/image.img.gz").is_err());
    }

    #[test]
    fn accepts_v1_without_v2_urls() {
        let mut manifest = manifest(None, "not-validated-for-v1");
        for image in manifest.boards.values_mut() {
            image.url = None;
        }

        assert!(validate_download_manifest(&manifest).is_ok());
    }

    #[test]
    fn accepts_absolute_board_and_version_bound_v2_urls() {
        let mut manifest = manifest(Some(2), "1.2.3-beta.4+build.5");
        manifest.boards.get_mut("pi4").unwrap().url = Some(format!(
            "{IMAGE_BASE_URL}snapdog-os-pi4-1.2.3-beta.4%2Bbuild.5.img.gz"
        ));

        assert!(validate_download_manifest(&manifest).is_ok());
    }

    #[test]
    fn rejects_non_immutable_v2_urls() {
        let invalid = [
            "snapdog-os-pi4-1.2.3.img.gz",
            "http://updates.snapdog.cc/os/images/snapdog-os-pi4-1.2.3.img.gz",
            "https://updates.snapdog.cc/os/images/snapdog-os-pi4-release.img.gz",
            "https://updates.snapdog.cc/os/images/snapdog-os-pi5-1.2.3.img.gz",
            "https://updates.snapdog.cc/os/images/snapdog-os-pi4-1.2.2.img.gz",
            "https://updates.snapdog.cc/os/images/snapdog-os-pi4-1.2.3.img.gz?mutable=1",
            "https://updates.snapdog.cc/os/images/snapdog-os-pi4-1.2.3.img.gz/",
        ];
        for value in invalid {
            let mut manifest = manifest(Some(2), "1.2.3");
            manifest.boards.get_mut("pi4").unwrap().url = Some(value.to_owned());

            assert!(
                validate_download_manifest(&manifest).is_err(),
                "accepted invalid URL {value}"
            );
        }
    }

    #[test]
    fn rejects_unknown_manifest_schema() {
        assert!(validate_download_manifest(&manifest(Some(3), "1.2.3")).is_err());
    }

    #[test]
    fn accepts_newest_first_catalog() {
        let catalog = ReleaseCatalog {
            schema_version: 1,
            channel: Channel::Release,
            releases: vec![manifest(Some(2), "1.2.3"), manifest(Some(2), "1.2.2")],
        };

        assert!(validate_catalog(&catalog, Channel::Release).is_ok());
    }

    #[test]
    fn rejects_unsafe_catalog_shapes() {
        let valid = ReleaseCatalog {
            schema_version: 1,
            channel: Channel::Release,
            releases: vec![manifest(Some(2), "1.2.3"), manifest(Some(2), "1.2.2")],
        };

        let mut unsorted = valid.clone();
        unsorted.releases.reverse();
        assert!(validate_catalog(&unsorted, Channel::Release).is_err());

        let mut duplicate = valid.clone();
        duplicate.releases[1] = duplicate.releases[0].clone();
        assert!(validate_catalog(&duplicate, Channel::Release).is_err());

        let mut wrong_channel = valid.clone();
        wrong_channel.channel = Channel::Beta;
        assert!(validate_catalog(&wrong_channel, Channel::Release).is_err());

        let mut unknown_schema = valid;
        unknown_schema.schema_version = 2;
        assert!(validate_catalog(&unknown_schema, Channel::Release).is_err());
    }
}
