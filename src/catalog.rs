// SPDX-License-Identifier: GPL-3.0-only

use std::time::Duration;

use reqwest::blocking::Client;
use thiserror::Error;

use crate::model::{Channel, Manifest};

pub const IMAGE_BASE_URL: &str = "https://updates.snapdog.cc/os/images/";

/// Catalog loading errors presented by the application.
#[derive(Debug, Error)]
pub enum CatalogError {
    #[error("could not contact the SnapDog release service: {0}")]
    Request(#[from] reqwest::Error),
    #[error("the release service returned an invalid image URL: {0}")]
    InvalidImageUrl(String),
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
            .user_agent(concat!("snapdog-os-installer/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self { client })
    }

    /// Fetch the latest image manifest for one channel.
    pub fn fetch_latest(&self, channel: Channel) -> Result<Manifest, CatalogError> {
        let url = format!("{IMAGE_BASE_URL}latest-{}.json", channel.manifest_name());
        let manifest = self
            .client
            .get(url)
            .header(reqwest::header::CACHE_CONTROL, "no-cache")
            .send()?
            .error_for_status()?
            .json()?;
        Ok(manifest)
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
