// SPDX-License-Identifier: GPL-3.0-only

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
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

/// Safe core flash errors.
#[derive(Debug, Error)]
pub enum FlashError {
    #[error("I/O error while flashing: {0}")]
    Io(#[from] io::Error),
    #[error("the downloaded image checksum does not match")]
    CompressedChecksum,
    #[error("the image is larger than the selected target")]
    TargetTooSmall,
    #[error("the flash operation was cancelled")]
    Cancelled,
    #[error("the written data could not be verified")]
    Verification,
}

/// Hash a downloaded archive before any target is touched.
pub fn verify_archive(path: &Path, expected_sha256: &str) -> Result<(), FlashError> {
    let mut input = BufReader::new(File::open(path)?);
    let mut hasher = Sha256::new();
    io::copy(&mut input, &mut HashWriter(&mut hasher))?;
    let actual = hex::encode(hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        return Err(FlashError::CompressedChecksum);
    }
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
    use tempfile::NamedTempFile;

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
}
