// SPDX-License-Identifier: GPL-3.0-only

//! Bounded, cancellable image downloads with atomic publication.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use reqwest::blocking::Client;
use reqwest::redirect::Policy;
use reqwest::{Url, header};
use sha2::{Digest, Sha256};
use thiserror::Error;

const BUFFER_SIZE: usize = 128 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const IO_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REDIRECTS: usize = 10;

/// One image download request.
#[derive(Clone, Copy, Debug)]
pub struct DownloadRequest<'a> {
    pub url: &'a str,
    pub destination: &'a Path,
    pub expected_sha256: Option<&'a str>,
    pub expected_size: Option<u64>,
}

impl<'a> DownloadRequest<'a> {
    /// Create a request without optional integrity metadata.
    pub const fn new(url: &'a str, destination: &'a Path) -> Self {
        Self {
            url,
            destination,
            expected_sha256: None,
            expected_size: None,
        }
    }
}

/// A progress snapshot emitted after each persisted chunk.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DownloadProgress {
    pub downloaded: u64,
    pub total: Option<u64>,
}

/// Metadata for a successfully published download.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DownloadReport {
    pub destination: PathBuf,
    pub bytes_downloaded: u64,
    pub sha256: String,
}

/// Download failures that never leave a partially published destination.
#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("invalid image URL: {0}")]
    InvalidUrl(String),
    #[error("image downloads require HTTPS: {0}")]
    InsecureUrl(String),
    #[error("invalid expected SHA-256 digest: {0}")]
    InvalidChecksum(String),
    #[error("download destination has no file name: {0}")]
    InvalidDestination(PathBuf),
    #[error("download destination already exists: {0}")]
    DestinationExists(PathBuf),
    #[error("another download is already using the partial file: {0}")]
    PartialExists(PathBuf),
    #[error("image request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("download I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("download size mismatch: expected {expected} bytes, received {actual} bytes")]
    SizeMismatch { expected: u64, actual: u64 },
    #[error("download size exceeds the supported range")]
    SizeOverflow,
    #[error("download checksum mismatch")]
    ChecksumMismatch,
    #[error("download was cancelled")]
    Cancelled,
}

/// Reusable HTTPS client with bounded connection and I/O timeouts.
#[derive(Clone)]
pub struct DownloadClient {
    client: Client,
}

impl DownloadClient {
    /// Build a downloader. No network request is made until [`Self::download`] is called.
    pub fn new() -> Result<Self, DownloadError> {
        let redirects = Policy::custom(|attempt| {
            if attempt.url().scheme() != "https" {
                attempt.error("redirected to a non-HTTPS image URL")
            } else if attempt.previous().len() >= MAX_REDIRECTS {
                attempt.error("too many image download redirects")
            } else {
                attempt.follow()
            }
        });
        let client = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            // Blocking reqwest reapplies this timeout to every response read. It is an
            // idle/per-operation bound, not a deadline for the complete image download.
            .timeout(IO_TIMEOUT)
            .https_only(true)
            .redirect(redirects)
            .user_agent(concat!("snapdog-os-installer/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self { client })
    }

    /// Download an image to a sibling `.part` file and publish it only after validation.
    ///
    /// Cancellation is observed before the request and between all blocking reads and writes.
    /// The configured I/O timeout bounds time spent inside a blocking network operation.
    pub fn download<F>(
        &self,
        request: &DownloadRequest<'_>,
        cancelled: &AtomicBool,
        progress: F,
    ) -> Result<DownloadReport, DownloadError>
    where
        F: FnMut(DownloadProgress),
    {
        let url = validate_url(request.url)?;
        let expected_digest = request.expected_sha256.map(parse_sha256).transpose()?;
        check_cancelled(cancelled)?;
        ensure_destination_available(request.destination)?;

        let mut response = self
            .client
            .get(url)
            .header(header::ACCEPT_ENCODING, "identity")
            .send()?
            .error_for_status()?;
        let response_size = response.content_length();
        if let (Some(expected), Some(actual)) = (request.expected_size, response_size)
            && expected != actual
        {
            return Err(DownloadError::SizeMismatch { expected, actual });
        }

        download_from_reader(
            &mut response,
            request.destination,
            expected_digest,
            request.expected_size,
            request.expected_size.or(response_size),
            cancelled,
            progress,
        )
    }
}

fn validate_url(value: &str) -> Result<Url, DownloadError> {
    let url = Url::parse(value).map_err(|_| DownloadError::InvalidUrl(value.to_owned()))?;
    if url.scheme() != "https" {
        return Err(DownloadError::InsecureUrl(value.to_owned()));
    }
    if url.host_str().is_none() {
        return Err(DownloadError::InvalidUrl(value.to_owned()));
    }
    Ok(url)
}

fn parse_sha256(value: &str) -> Result<[u8; 32], DownloadError> {
    let decoded =
        hex::decode(value).map_err(|_| DownloadError::InvalidChecksum(value.to_owned()))?;
    decoded
        .try_into()
        .map_err(|_| DownloadError::InvalidChecksum(value.to_owned()))
}

fn ensure_destination_available(destination: &Path) -> Result<(), DownloadError> {
    if destination.file_name().is_none() {
        return Err(DownloadError::InvalidDestination(destination.to_path_buf()));
    }
    if destination.try_exists()? {
        return Err(DownloadError::DestinationExists(destination.to_path_buf()));
    }
    Ok(())
}

fn partial_path(destination: &Path) -> Result<PathBuf, DownloadError> {
    let Some(file_name) = destination.file_name() else {
        return Err(DownloadError::InvalidDestination(destination.to_path_buf()));
    };
    let mut partial_name = OsString::from(file_name);
    partial_name.push(".part");
    Ok(destination.with_file_name(partial_name))
}

fn download_from_reader<R, F>(
    reader: &mut R,
    destination: &Path,
    expected_digest: Option<[u8; 32]>,
    expected_size: Option<u64>,
    progress_total: Option<u64>,
    cancelled: &AtomicBool,
    mut progress: F,
) -> Result<DownloadReport, DownloadError>
where
    R: Read,
    F: FnMut(DownloadProgress),
{
    check_cancelled(cancelled)?;
    ensure_destination_available(destination)?;
    let partial_path = partial_path(destination)?;
    let mut partial = PartialDownload::create(partial_path)?;
    let mut buffer = vec![0_u8; BUFFER_SIZE];
    let mut hasher = Sha256::new();
    let mut downloaded = 0_u64;

    progress(DownloadProgress {
        downloaded,
        total: progress_total,
    });
    loop {
        check_cancelled(cancelled)?;
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        check_cancelled(cancelled)?;
        let count = u64::try_from(count).expect("download buffer length fits u64");
        let next = downloaded
            .checked_add(count)
            .ok_or(DownloadError::SizeOverflow)?;
        if let Some(expected) = expected_size
            && next > expected
        {
            return Err(DownloadError::SizeMismatch {
                expected,
                actual: next,
            });
        }
        let count = usize::try_from(count).expect("download buffer length fits usize");
        partial.file_mut().write_all(&buffer[..count])?;
        hasher.update(&buffer[..count]);
        downloaded = next;
        progress(DownloadProgress {
            downloaded,
            total: progress_total,
        });
    }

    if let Some(expected) = expected_size
        && downloaded != expected
    {
        return Err(DownloadError::SizeMismatch {
            expected,
            actual: downloaded,
        });
    }
    let digest: [u8; 32] = hasher.finalize().into();
    if expected_digest.is_some_and(|expected| expected != digest) {
        return Err(DownloadError::ChecksumMismatch);
    }

    partial.publish(destination)?;
    Ok(DownloadReport {
        destination: destination.to_path_buf(),
        bytes_downloaded: downloaded,
        sha256: hex::encode(digest),
    })
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<(), DownloadError> {
    if cancelled.load(Ordering::Relaxed) {
        Err(DownloadError::Cancelled)
    } else {
        Ok(())
    }
}

struct PartialDownload {
    path: PathBuf,
    file: Option<File>,
    published: bool,
}

impl PartialDownload {
    fn create(path: PathBuf) -> Result<Self, DownloadError> {
        if path.try_exists()? {
            return Err(DownloadError::PartialExists(path));
        }
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        Ok(Self {
            path,
            file: Some(file),
            published: false,
        })
    }

    const fn file_mut(&mut self) -> &mut File {
        self.file.as_mut().expect("partial file remains open")
    }

    fn publish(mut self, destination: &Path) -> Result<(), DownloadError> {
        let file = self.file.take().expect("partial file remains open");
        file.sync_all()?;
        drop(file);
        ensure_destination_available(destination)?;
        fs::rename(&self.path, destination)?;
        self.published = true;
        Ok(())
    }
}

impl Drop for PartialDownload {
    fn drop(&mut self) {
        drop(self.file.take());
        if !self.published
            && let Err(error) = fs::remove_file(&self.path)
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %self.path.display(),
                %error,
                "could not remove partial image download"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use tempfile::tempdir;

    use super::*;

    fn digest(payload: &[u8]) -> String {
        hex::encode(Sha256::digest(payload))
    }

    fn request_path(directory: &Path) -> PathBuf {
        directory.join("snapdog-os.img.gz")
    }

    #[test]
    fn streams_validated_file_and_publishes_atomically() {
        let directory = tempdir().unwrap();
        let destination = request_path(directory.path());
        let payload = vec![0x5a; BUFFER_SIZE * 2 + 17];
        let expected_digest = parse_sha256(&digest(&payload)).unwrap();
        let mut reader = Cursor::new(&payload);
        let cancelled = AtomicBool::new(false);
        let mut snapshots = Vec::new();
        let partial = partial_path(&destination).unwrap();

        let report = download_from_reader(
            &mut reader,
            &destination,
            Some(expected_digest),
            Some(payload.len() as u64),
            Some(payload.len() as u64),
            &cancelled,
            |snapshot| {
                assert!(!destination.exists());
                assert!(partial.exists());
                snapshots.push(snapshot);
            },
        )
        .unwrap();

        assert_eq!(fs::read(&destination).unwrap(), payload);
        assert!(!partial.exists());
        assert_eq!(report.destination, destination);
        assert_eq!(report.bytes_downloaded, payload.len() as u64);
        assert_eq!(report.sha256, digest(&payload));
        assert_eq!(snapshots.first().unwrap().downloaded, 0);
        assert_eq!(snapshots.last().unwrap().downloaded, payload.len() as u64);
        assert!(
            snapshots
                .windows(2)
                .all(|pair| pair[0].downloaded <= pair[1].downloaded)
        );
    }

    #[test]
    fn removes_partial_file_after_checksum_failure() {
        let directory = tempdir().unwrap();
        let destination = request_path(directory.path());
        let payload = b"corrupt image";
        let mut reader = Cursor::new(payload);
        let result = download_from_reader(
            &mut reader,
            &destination,
            Some([0_u8; 32]),
            Some(payload.len() as u64),
            None,
            &AtomicBool::new(false),
            |_| {},
        );

        assert!(matches!(result, Err(DownloadError::ChecksumMismatch)));
        assert!(!destination.exists());
        assert!(!partial_path(&destination).unwrap().exists());
    }

    #[test]
    fn removes_partial_file_after_size_failure() {
        let directory = tempdir().unwrap();
        let destination = request_path(directory.path());
        let payload = b"image is too long";
        let mut reader = Cursor::new(payload);
        let result = download_from_reader(
            &mut reader,
            &destination,
            None,
            Some(5),
            None,
            &AtomicBool::new(false),
            |_| {},
        );

        assert!(matches!(
            result,
            Err(DownloadError::SizeMismatch {
                expected: 5,
                actual: _
            })
        ));
        assert!(!destination.exists());
        assert!(!partial_path(&destination).unwrap().exists());
    }

    #[test]
    fn cancellation_removes_partial_file() {
        let directory = tempdir().unwrap();
        let destination = request_path(directory.path());
        let payload = vec![0x42; BUFFER_SIZE * 2];
        let mut reader = Cursor::new(payload);
        let cancelled = AtomicBool::new(false);
        let result = download_from_reader(
            &mut reader,
            &destination,
            None,
            None,
            None,
            &cancelled,
            |snapshot| {
                if snapshot.downloaded > 0 {
                    cancelled.store(true, Ordering::Relaxed);
                }
            },
        );

        assert!(matches!(result, Err(DownloadError::Cancelled)));
        assert!(!destination.exists());
        assert!(!partial_path(&destination).unwrap().exists());
    }

    #[test]
    fn reader_error_removes_partial_file() {
        struct FailingReader {
            first_read: bool,
        }

        impl Read for FailingReader {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                if self.first_read {
                    return Err(io::Error::new(io::ErrorKind::ConnectionReset, "test"));
                }
                self.first_read = true;
                buffer[..4].copy_from_slice(b"data");
                Ok(4)
            }
        }

        let directory = tempdir().unwrap();
        let destination = request_path(directory.path());
        let mut reader = FailingReader { first_read: false };
        let result = download_from_reader(
            &mut reader,
            &destination,
            None,
            None,
            None,
            &AtomicBool::new(false),
            |_| {},
        );

        assert!(matches!(result, Err(DownloadError::Io(_))));
        assert!(!destination.exists());
        assert!(!partial_path(&destination).unwrap().exists());
    }

    #[test]
    fn rejects_http_without_starting_a_request() {
        let directory = tempdir().unwrap();
        let destination = request_path(directory.path());
        let client = DownloadClient::new().unwrap();
        let request = DownloadRequest::new("http://127.0.0.1/image.img.gz", &destination);

        let result = client.download(&request, &AtomicBool::new(false), |_| {});

        assert!(matches!(result, Err(DownloadError::InsecureUrl(_))));
        assert!(!destination.exists());
        assert!(!partial_path(&destination).unwrap().exists());
    }

    #[test]
    fn existing_destination_is_not_replaced() {
        let directory = tempdir().unwrap();
        let destination = request_path(directory.path());
        fs::write(&destination, b"keep me").unwrap();
        let client = DownloadClient::new().unwrap();
        let request = DownloadRequest::new("https://example.invalid/image.img.gz", &destination);

        let result = client.download(&request, &AtomicBool::new(false), |_| {});

        assert!(matches!(result, Err(DownloadError::DestinationExists(_))));
        assert_eq!(fs::read(&destination).unwrap(), b"keep me");
    }
}
