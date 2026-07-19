// SPDX-License-Identifier: GPL-3.0-only

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use thiserror::Error;

const BUFFER_SIZE: usize = 1024 * 1024;
const BUFFER_SIZE_U64: u64 = 1024 * 1024;

/// Result of a file-backed flash operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FlashReport {
    pub bytes_written: u64,
    pub raw_sha256: String,
    pub verified: bool,
}

/// A prepared uncompressed image that is safe to hand to a privileged writer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedImage {
    pub bytes: u64,
    pub raw_sha256: String,
}

/// The currently active phase of the image pipeline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlashStage {
    Decompressing,
    Writing,
    Verifying,
}

/// Monotonic byte progress for one pipeline phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FlashProgress {
    pub stage: FlashStage,
    pub processed: u64,
    pub total: Option<u64>,
}

/// Safe core flash errors.
#[derive(Debug, Error)]
pub enum FlashError {
    #[error("I/O error while flashing: {0}")]
    Io(#[from] io::Error),
    #[error("the downloaded image checksum does not match")]
    CompressedChecksum,
    #[error("the image is larger than the selected target")]
    TargetTooSmall,
    #[error("the uncompressed image size does not match the release manifest")]
    RawSize,
    #[error("the flash operation was cancelled")]
    Cancelled,
    #[error("the written data could not be verified")]
    Verification,
}

/// Decompress an archive to an ordinary file before privileged target access begins.
pub fn prepare_gzip<F>(
    archive: &Path,
    raw_path: &Path,
    expected_size: Option<u64>,
    expected_raw_sha256: Option<&str>,
    cancelled: &AtomicBool,
    mut progress: F,
) -> Result<PreparedImage, FlashError>
where
    F: FnMut(FlashProgress),
{
    let result = (|| {
        let input = BufReader::new(File::open(archive)?);
        let mut decoder = GzDecoder::new(input);
        let output = File::create(raw_path)?;
        let mut output = BufWriter::new(output);
        let mut hasher = Sha256::new();
        let mut buffer = vec![0_u8; BUFFER_SIZE];
        let mut written = 0_u64;

        loop {
            ensure_not_cancelled(cancelled)?;
            let count = decoder.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            let count_u64 = u64::try_from(count).expect("buffer length fits u64");
            written = written.checked_add(count_u64).ok_or(FlashError::RawSize)?;
            if expected_size.is_some_and(|size| written > size) {
                return Err(FlashError::RawSize);
            }
            output.write_all(&buffer[..count])?;
            hasher.update(&buffer[..count]);
            progress(FlashProgress {
                stage: FlashStage::Decompressing,
                processed: written,
                total: expected_size,
            });
        }
        output.flush()?;

        if expected_size.is_some_and(|size| written != size) {
            return Err(FlashError::RawSize);
        }
        let raw_sha256 = hex::encode(hasher.finalize());
        if expected_raw_sha256.is_some_and(|expected| !raw_sha256.eq_ignore_ascii_case(expected)) {
            return Err(FlashError::Verification);
        }
        Ok(PreparedImage {
            bytes: written,
            raw_sha256,
        })
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(raw_path);
    }
    result
}

/// Write a prepared raw image and optionally read it back for byte-for-byte verification.
pub fn write_raw<T, F>(
    raw_path: &Path,
    target: &mut T,
    target_capacity: u64,
    verify: bool,
    cancelled: &AtomicBool,
    skip_verification: &AtomicBool,
    progress: F,
) -> Result<FlashReport, FlashError>
where
    T: Read + Write + Seek,
    F: FnMut(FlashProgress),
{
    let mut input = File::open(raw_path)?;
    write_raw_from(
        &mut input,
        target,
        target_capacity,
        verify,
        cancelled,
        skip_verification,
        progress,
    )
}

/// Write from an already-open, seekable raw image.
///
/// The privileged worker uses this entry point so the file it validates is exactly the file it
/// later writes, even if the path is replaced concurrently.
pub(crate) fn write_raw_from<T, F>(
    input: &mut File,
    target: &mut T,
    target_capacity: u64,
    verify: bool,
    cancelled: &AtomicBool,
    skip_verification: &AtomicBool,
    mut progress: F,
) -> Result<FlashReport, FlashError>
where
    T: Read + Write + Seek,
    F: FnMut(FlashProgress),
{
    let total = input.metadata()?.len();
    if total > target_capacity {
        return Err(FlashError::TargetTooSmall);
    }

    input.seek(SeekFrom::Start(0))?;
    target.seek(SeekFrom::Start(0))?;
    let mut input = BufReader::new(input);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; BUFFER_SIZE];
    let mut written = 0_u64;
    loop {
        ensure_not_cancelled(cancelled)?;
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        target.write_all(&buffer[..count])?;
        hasher.update(&buffer[..count]);
        written += u64::try_from(count).expect("buffer length fits u64");
        progress(FlashProgress {
            stage: FlashStage::Writing,
            processed: written,
            total: Some(total),
        });
    }
    target.flush()?;

    let raw_digest = hasher.finalize();
    let raw_sha256 = hex::encode(raw_digest);
    let verified = if verify && !skip_verification.load(Ordering::Relaxed) {
        target.seek(SeekFrom::Start(0))?;
        let mut read = 0_u64;
        let mut verify_hasher = Sha256::new();
        while read < written {
            ensure_not_cancelled(cancelled)?;
            if skip_verification.load(Ordering::Relaxed) {
                return Ok(FlashReport {
                    bytes_written: written,
                    raw_sha256,
                    verified: false,
                });
            }
            let limit = usize::try_from((written - read).min(BUFFER_SIZE_U64))
                .expect("bounded verification buffer length fits usize");
            let count = target.read(&mut buffer[..limit])?;
            if count == 0 {
                return Err(FlashError::Verification);
            }
            verify_hasher.update(&buffer[..count]);
            read += u64::try_from(count).expect("buffer length fits u64");
            progress(FlashProgress {
                stage: FlashStage::Verifying,
                processed: read,
                total: Some(written),
            });
        }
        if verify_hasher.finalize() != raw_digest {
            return Err(FlashError::Verification);
        }
        true
    } else {
        false
    };

    Ok(FlashReport {
        bytes_written: written,
        raw_sha256,
        verified,
    })
}

fn ensure_not_cancelled(cancelled: &AtomicBool) -> Result<(), FlashError> {
    if cancelled.load(Ordering::Relaxed) {
        Err(FlashError::Cancelled)
    } else {
        Ok(())
    }
}

/// Hash a downloaded archive before any target is touched.
pub fn verify_archive(path: &Path, expected_sha256: &str) -> Result<(), FlashError> {
    verify_file_sha256(path, expected_sha256)
}

/// Hash any regular file and compare it with an expected SHA-256 digest.
pub fn verify_file_sha256(path: &Path, expected_sha256: &str) -> Result<(), FlashError> {
    let mut input = File::open(path)?;
    verify_open_file_sha256(&mut input, expected_sha256)
}

/// Hash an already-open file and rewind it for a subsequent read.
pub(crate) fn verify_open_file_sha256(
    input: &mut File,
    expected_sha256: &str,
) -> Result<(), FlashError> {
    input.seek(SeekFrom::Start(0))?;
    let mut input = BufReader::new(input);
    let mut hasher = Sha256::new();
    io::copy(&mut input, &mut HashWriter(&mut hasher))?;
    let actual = hex::encode(hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        return Err(FlashError::CompressedChecksum);
    }
    input.seek(SeekFrom::Start(0))?;
    Ok(())
}

/// Decompress a previously verified archive into a seekable target and optionally verify it.
///
/// Production platform backends will provide a locked raw device. Tests use an ordinary file,
/// keeping destructive device access completely outside the core pipeline.
pub fn flash_gzip<T>(
    archive: &Path,
    target: &mut T,
    target_capacity: u64,
    expected_raw_sha256: Option<&str>,
    verify: bool,
    cancelled: &AtomicBool,
) -> Result<FlashReport, FlashError>
where
    T: Read + Write + Seek,
{
    target.seek(SeekFrom::Start(0))?;
    let input = BufReader::new(File::open(archive)?);
    let mut decoder = GzDecoder::new(input);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; BUFFER_SIZE];
    let mut written = 0_u64;

    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err(FlashError::Cancelled);
        }
        let count = decoder.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let next = written
            .checked_add(u64::try_from(count).expect("buffer length fits u64"))
            .ok_or(FlashError::TargetTooSmall)?;
        if next > target_capacity {
            return Err(FlashError::TargetTooSmall);
        }
        target.write_all(&buffer[..count])?;
        hasher.update(&buffer[..count]);
        written = next;
    }
    target.flush()?;

    let raw_digest = hasher.finalize();
    let raw_sha256 = hex::encode(raw_digest);
    if expected_raw_sha256.is_some_and(|expected| !raw_sha256.eq_ignore_ascii_case(expected)) {
        return Err(FlashError::Verification);
    }

    if verify {
        target.seek(SeekFrom::Start(0))?;
        let mut remaining = written;
        let mut verify_hasher = Sha256::new();
        while remaining > 0 {
            if cancelled.load(Ordering::Relaxed) {
                return Err(FlashError::Cancelled);
            }
            let limit = usize::try_from(remaining.min(BUFFER_SIZE_U64))
                .expect("bounded verification buffer length fits usize");
            let count = target.read(&mut buffer[..limit])?;
            if count == 0 {
                return Err(FlashError::Verification);
            }
            verify_hasher.update(&buffer[..count]);
            remaining -= u64::try_from(count).expect("buffer length fits u64");
        }
        if verify_hasher.finalize() != raw_digest {
            return Err(FlashError::Verification);
        }
    }

    Ok(FlashReport {
        bytes_written: written,
        raw_sha256,
        verified: verify,
    })
}

struct HashWriter<'a>(&'a mut Sha256);

impl Write for HashWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.update(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use flate2::{Compression, write::GzEncoder};
    use tempfile::{NamedTempFile, tempdir};

    use super::*;

    fn archive(payload: &[u8]) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(payload).unwrap();
        file.write_all(&encoder.finish().unwrap()).unwrap();
        file.flush().unwrap();
        file
    }

    #[test]
    fn writes_and_verifies_file_backed_target() {
        let payload = b"snapdog image".repeat(4096);
        let archive = archive(&payload);
        let mut target = Cursor::new(vec![0_u8; payload.len()]);
        let report = flash_gzip(
            archive.path(),
            &mut target,
            payload.len() as u64,
            None,
            true,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(report.verified);
        assert_eq!(report.bytes_written, payload.len() as u64);
        assert_eq!(&target.into_inner()[..payload.len()], payload);
    }

    #[test]
    fn refuses_target_that_is_too_small() {
        let payload = b"too large".repeat(1024);
        let archive = archive(&payload);
        let mut target = Cursor::new(vec![0_u8; payload.len()]);
        let result = flash_gzip(
            archive.path(),
            &mut target,
            100,
            None,
            false,
            &AtomicBool::new(false),
        );
        assert!(matches!(result, Err(FlashError::TargetTooSmall)));
    }

    #[test]
    fn cancellation_happens_before_writing() {
        let archive = archive(b"payload");
        let mut target = Cursor::new(vec![0_u8; 64]);
        let result = flash_gzip(
            archive.path(),
            &mut target,
            64,
            None,
            true,
            &AtomicBool::new(true),
        );
        assert!(matches!(result, Err(FlashError::Cancelled)));
        assert_eq!(target.position(), 0);
    }

    #[test]
    fn prepares_and_hashes_raw_image_before_target_access() {
        let payload = b"prepared snapdog image".repeat(2048);
        let archive = archive(&payload);
        let directory = tempdir().unwrap();
        let raw_path = directory.path().join("prepared.img");
        let expected_hash = hex::encode(Sha256::digest(&payload));
        let mut updates = Vec::new();

        let prepared = prepare_gzip(
            archive.path(),
            &raw_path,
            Some(payload.len() as u64),
            Some(&expected_hash),
            &AtomicBool::new(false),
            |update| updates.push(update),
        )
        .unwrap();

        assert_eq!(prepared.bytes, payload.len() as u64);
        assert_eq!(prepared.raw_sha256, expected_hash);
        assert_eq!(std::fs::read(raw_path).unwrap(), payload);
        assert_eq!(updates.last().unwrap().processed, payload.len() as u64);
    }

    #[test]
    fn removes_prepared_image_after_hash_mismatch() {
        let archive = archive(b"payload");
        let directory = tempdir().unwrap();
        let raw_path = directory.path().join("bad.img");

        let result = prepare_gzip(
            archive.path(),
            &raw_path,
            None,
            Some("00"),
            &AtomicBool::new(false),
            |_| {},
        );

        assert!(matches!(result, Err(FlashError::Verification)));
        assert!(!raw_path.exists());
    }

    #[test]
    fn writes_prepared_image_and_verifies_target() {
        let payload = b"raw snapdog image".repeat(2048);
        let raw = NamedTempFile::new().unwrap();
        std::fs::write(raw.path(), &payload).unwrap();
        let mut target = Cursor::new(vec![0_u8; payload.len()]);
        let mut updates = Vec::new();

        let report = write_raw(
            raw.path(),
            &mut target,
            payload.len() as u64,
            true,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
            |update| updates.push(update),
        )
        .unwrap();

        assert!(report.verified);
        assert!(
            updates
                .iter()
                .any(|update| update.stage == FlashStage::Writing)
        );
        assert!(
            updates
                .iter()
                .any(|update| update.stage == FlashStage::Verifying)
        );
        assert_eq!(&target.into_inner()[..payload.len()], payload);
    }

    #[test]
    fn verification_can_be_skipped_without_rewriting() {
        let payload = b"raw snapdog image";
        let raw = NamedTempFile::new().unwrap();
        std::fs::write(raw.path(), payload).unwrap();
        let mut target = Cursor::new(vec![0_u8; payload.len()]);

        let report = write_raw(
            raw.path(),
            &mut target,
            payload.len() as u64,
            true,
            &AtomicBool::new(false),
            &AtomicBool::new(true),
            |_| {},
        )
        .unwrap();

        assert!(!report.verified);
        assert_eq!(report.bytes_written, payload.len() as u64);
    }

    #[test]
    fn opened_raw_image_cannot_be_replaced_before_writing() {
        let original = b"validated snapdog image";
        let replacement = b"malicious replacement data";
        let directory = tempdir().unwrap();
        let raw_path = directory.path().join("image.img");
        std::fs::write(&raw_path, original).unwrap();
        let mut opened = File::open(&raw_path).unwrap();
        std::fs::remove_file(&raw_path).unwrap();
        std::fs::write(&raw_path, replacement).unwrap();
        let mut target = Cursor::new(vec![0_u8; original.len()]);

        let report = write_raw_from(
            &mut opened,
            &mut target,
            original.len() as u64,
            false,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();

        assert_eq!(report.bytes_written, original.len() as u64);
        assert_eq!(&target.into_inner()[..original.len()], original);
    }
}
