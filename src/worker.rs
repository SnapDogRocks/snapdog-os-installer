// SPDX-License-Identifier: GPL-3.0-only

//! Privileged flash-worker boundary.
//!
//! The public production entry point is deliberately available only on macOS and requires a
//! [`RawDeviceGate`]. The actual orchestration is backend-driven so all destructive behaviour can
//! be exercised in tests with ordinary files, without ever opening a real device.

#[cfg(any(target_os = "macos", test))]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
#[cfg(target_os = "macos")]
use std::path::Component;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::flash::{FlashError, FlashProgress, FlashReport, FlashStage, write_raw_from};

#[cfg(target_os = "macos")]
mod macos;

/// Current serialized worker-job schema.
pub const WORKER_JOB_SCHEMA_VERSION: u32 = 1;
/// Current JSON-lines progress schema.
pub const WORKER_PROGRESS_SCHEMA_VERSION: u32 = 1;

#[cfg(target_os = "macos")]
const RAW_DEVICE_OPT_IN: &str = "SNAPDOG_INSTALLER_ALLOW_RAW_DEVICE_WRITE";
#[cfg(target_os = "macos")]
const RAW_DEVICE_OPT_IN_VALUE: &str = "YES-I-UNDERSTAND";

#[cfg(target_os = "macos")]
const WORKER_JOB_ARGUMENT: &str = "--worker-job";
const STAGING_BUFFER_SIZE: usize = 1024 * 1024;

/// Target identity captured while the user selected a drive.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkerDrive {
    pub id: String,
    pub device: String,
    pub capacity: u64,
}

/// Where the worker writes machine-readable progress events.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProgressDestination {
    /// Write one JSON object per line to standard output.
    #[default]
    Stdout,
    /// Write one JSON object per line to a GUI-provided file.
    File { path: PathBuf },
}

/// Serializable request accepted by the privileged worker.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkerJob {
    pub schema_version: u32,
    pub drive: WorkerDrive,
    pub raw_path: PathBuf,
    pub raw_size: u64,
    pub verify: bool,
    pub expected_raw_sha256: String,
    #[serde(default)]
    pub progress: ProgressDestination,
    #[serde(default)]
    pub cancel_path: Option<PathBuf>,
    #[serde(default)]
    pub skip_verification_path: Option<PathBuf>,
}

/// Worker state emitted as JSON lines.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerPhase {
    ValidatingImage,
    ValidatingTarget,
    Unmounting,
    Writing,
    Verifying,
    Syncing,
    Ejecting,
    Completed,
    Cancelled,
    Failed,
}

/// One versioned JSON-lines progress event.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkerProgress {
    pub schema_version: u32,
    pub phase: WorkerPhase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes_processed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl WorkerProgress {
    const fn phase(phase: WorkerPhase) -> Self {
        Self {
            schema_version: WORKER_PROGRESS_SCHEMA_VERSION,
            phase,
            bytes_processed: None,
            total_bytes: None,
            raw_sha256: None,
            verified: None,
            message: None,
        }
    }

    fn completed(report: &FlashReport) -> Self {
        Self {
            bytes_processed: Some(report.bytes_written),
            total_bytes: Some(report.bytes_written),
            raw_sha256: Some(report.raw_sha256.clone()),
            verified: Some(report.verified),
            ..Self::phase(WorkerPhase::Completed)
        }
    }

    fn failed(error: &WorkerError) -> Self {
        Self {
            message: Some(error.to_string()),
            ..Self::phase(if matches!(error, WorkerError::Cancelled) {
                WorkerPhase::Cancelled
            } else {
                WorkerPhase::Failed
            })
        }
    }
}

/// Failures at the privileged worker boundary.
#[derive(Debug, Error)]
pub enum WorkerError {
    #[error("invalid worker job: {0}")]
    InvalidJob(String),
    #[error("raw-device execution is not explicitly enabled")]
    RawDeviceDisabled,
    #[error("the privileged worker must run as root")]
    NotRoot,
    #[error("the selected target is no longer available")]
    TargetMissing,
    #[error("the selected target changed after selection")]
    TargetChanged,
    #[error("the selected target is not a writable, whole removable physical disk")]
    UnsafeTarget,
    #[error("the flash operation was cancelled")]
    Cancelled,
    #[error("progress output failed: {0}")]
    Progress(io::Error),
    #[error("platform operation failed: {0}")]
    Platform(String),
    #[error(transparent)]
    Flash(#[from] FlashError),
}

/// Capability required before production code can open a raw device.
///
/// It can only be obtained by an explicitly opted-in, root worker process. Tests never construct
/// this value and use a file-backed platform implementation instead.
#[derive(Debug)]
pub struct RawDeviceGate {
    _private: (),
}

impl RawDeviceGate {
    /// Validate the explicit environment opt-in and effective worker identity.
    #[cfg(target_os = "macos")]
    pub fn from_environment() -> Result<Self, WorkerError> {
        if std::env::var_os(RAW_DEVICE_OPT_IN).as_deref()
            != Some(std::ffi::OsStr::new(RAW_DEVICE_OPT_IN_VALUE))
        {
            return Err(WorkerError::RawDeviceDisabled);
        }

        let output = std::process::Command::new("/usr/bin/id")
            .arg("-u")
            .output()
            .map_err(|error| WorkerError::Platform(error.to_string()))?;
        if !output.status.success() || output.stdout != b"0\n" {
            return Err(WorkerError::NotRoot);
        }
        Ok(Self { _private: () })
    }
}

/// Emit progress events to an arbitrary writer using JSON Lines.
pub struct JsonLineProgress<W: Write> {
    writer: BufWriter<W>,
}

impl<W: Write> JsonLineProgress<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer: BufWriter::new(writer),
        }
    }
}

trait ProgressSink {
    fn emit(&mut self, progress: &WorkerProgress) -> Result<(), WorkerError>;
}

impl<W: Write> ProgressSink for JsonLineProgress<W> {
    fn emit(&mut self, progress: &WorkerProgress) -> Result<(), WorkerError> {
        serde_json::to_writer(&mut self.writer, progress)
            .map_err(|error| WorkerError::Progress(io::Error::other(error)))?;
        self.writer
            .write_all(b"\n")
            .map_err(WorkerError::Progress)?;
        self.writer.flush().map_err(WorkerError::Progress)
    }
}

/// Run one raw-device job on macOS.
///
/// The GUI is expected to serialize the job before elevation and obtain `gate` only inside the
/// elevated worker process. No raw path supplied by the job is opened directly.
#[cfg(target_os = "macos")]
pub fn run_macos_worker(job: &WorkerJob, _gate: RawDeviceGate) -> Result<FlashReport, WorkerError> {
    let session = PrivilegedSession::validate(job, &current_worker_job_path()?)?;
    validate_job(job)?;
    let writer: Box<dyn Write> = match &job.progress {
        ProgressDestination::Stdout => Box::new(io::stdout()),
        ProgressDestination::File { path } => Box::new(open_progress_file(path, &session)?),
    };
    let mut progress = JsonLineProgress::new(writer);
    run_and_report(job, &mut macos::MacOsPlatform, &mut progress)
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
struct PrivilegedSession {
    directory: PathBuf,
    owner_uid: u32,
    directory_device: u64,
    directory_inode: u64,
    parent_device: u64,
    parent_inode: u64,
}

#[cfg(target_os = "macos")]
impl PrivilegedSession {
    fn validate(job: &WorkerJob, job_path: &Path) -> Result<Self, WorkerError> {
        use std::os::unix::fs::MetadataExt;

        let directory = job
            .raw_path
            .parent()
            .ok_or_else(|| invalid_session("raw image has no parent directory"))?
            .to_path_buf();
        if !is_clean_absolute_path(&directory) {
            return Err(invalid_session("session directory path is not canonical"));
        }

        let metadata = fs::symlink_metadata(&directory).map_err(|error| {
            invalid_session(format!("session directory is unavailable: {error}"))
        })?;
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || metadata.uid() == 0
            || metadata.mode() & 0o700 != 0o700
            || metadata.mode() & 0o022 != 0
        {
            return Err(invalid_session(format!(
                "session directory must be private and non-root-owned (uid {}, mode {:o})",
                metadata.uid(),
                metadata.mode() & 0o777
            )));
        }
        let parent = directory
            .parent()
            .ok_or_else(|| invalid_session("session directory has no private parent"))?;
        let parent_metadata = fs::symlink_metadata(parent).map_err(|error| {
            invalid_session(format!("session parent directory is unavailable: {error}"))
        })?;
        if parent_metadata.file_type().is_symlink()
            || !parent_metadata.is_dir()
            || parent_metadata.uid() != metadata.uid()
            || parent_metadata.mode() & 0o700 != 0o700
            || parent_metadata.mode() & 0o077 != 0
        {
            return Err(invalid_session(
                "session directory must live directly inside a private owner-only directory",
            ));
        }

        let session = Self {
            directory,
            owner_uid: metadata.uid(),
            directory_device: metadata.dev(),
            directory_inode: metadata.ino(),
            parent_device: parent_metadata.dev(),
            parent_inode: parent_metadata.ino(),
        };
        session.validate_existing_file(job_path, "worker-job.json", None, false)?;
        session.validate_existing_file(
            &job.raw_path,
            "snapdog-os.img",
            Some(job.raw_size),
            false,
        )?;

        let progress_path = match &job.progress {
            ProgressDestination::File { path } => path,
            ProgressDestination::Stdout => {
                return Err(invalid_session(
                    "privileged jobs require a session progress file",
                ));
            }
        };
        session.validate_existing_file(progress_path, "worker-progress.jsonl", Some(0), true)?;
        session.validate_marker(job.cancel_path.as_deref(), "cancel")?;
        session.validate_marker(job.skip_verification_path.as_deref(), "skip-verification")?;
        Ok(session)
    }

    fn validate_existing_file(
        &self,
        path: &Path,
        expected_name: &str,
        expected_size: Option<u64>,
        require_empty: bool,
    ) -> Result<(), WorkerError> {
        self.validate_member_path(path, expected_name)?;
        self.ensure_directory_unchanged()?;
        let metadata = fs::symlink_metadata(path)
            .map_err(|error| invalid_session(format!("{expected_name} is unavailable: {error}")))?;
        self.validate_file_metadata(&metadata, expected_name, expected_size, require_empty)
    }

    fn validate_marker(&self, path: Option<&Path>, expected_name: &str) -> Result<(), WorkerError> {
        let path = path.ok_or_else(|| {
            invalid_session(format!(
                "session marker {expected_name} is missing from the job"
            ))
        })?;
        self.validate_member_path(path, expected_name)?;
        match fs::symlink_metadata(path) {
            Ok(metadata) => self.validate_file_metadata(&metadata, expected_name, Some(0), true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(invalid_session(format!(
                "session marker {expected_name} is unavailable: {error}"
            ))),
        }
    }

    fn validate_member_path(&self, path: &Path, expected_name: &str) -> Result<(), WorkerError> {
        if !is_clean_absolute_path(path)
            || path.parent() != Some(self.directory.as_path())
            || path.file_name() != Some(std::ffi::OsStr::new(expected_name))
        {
            return Err(invalid_session(format!(
                "{expected_name} must be a direct member of the private session directory"
            )));
        }
        Ok(())
    }

    fn ensure_directory_unchanged(&self) -> Result<(), WorkerError> {
        use std::os::unix::fs::MetadataExt;

        let metadata = fs::symlink_metadata(&self.directory)
            .map_err(|error| invalid_session(format!("session directory changed: {error}")))?;
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || metadata.uid() != self.owner_uid
            || metadata.dev() != self.directory_device
            || metadata.ino() != self.directory_inode
            || metadata.mode() & 0o700 != 0o700
            || metadata.mode() & 0o022 != 0
        {
            return Err(invalid_session(
                "session directory changed during authorization",
            ));
        }
        let parent = self
            .directory
            .parent()
            .ok_or_else(|| invalid_session("session directory has no private parent"))?;
        let parent_metadata = fs::symlink_metadata(parent)
            .map_err(|error| invalid_session(format!("session parent changed: {error}")))?;
        if parent_metadata.file_type().is_symlink()
            || !parent_metadata.is_dir()
            || parent_metadata.uid() != self.owner_uid
            || parent_metadata.dev() != self.parent_device
            || parent_metadata.ino() != self.parent_inode
            || parent_metadata.mode() & 0o700 != 0o700
            || parent_metadata.mode() & 0o077 != 0
        {
            return Err(invalid_session(
                "session parent directory changed during authorization",
            ));
        }
        Ok(())
    }

    fn validate_file_metadata(
        &self,
        metadata: &fs::Metadata,
        name: &str,
        expected_size: Option<u64>,
        require_empty: bool,
    ) -> Result<(), WorkerError> {
        use std::os::unix::fs::MetadataExt;

        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.uid() != self.owner_uid
            || metadata.nlink() != 1
            || metadata.mode() & 0o022 != 0
            || expected_size.is_some_and(|size| metadata.len() != size)
            || (require_empty && metadata.len() != 0)
        {
            return Err(invalid_session(format!(
                "{name} has unsafe ownership, permissions, links, type, or size"
            )));
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn current_worker_job_path() -> Result<PathBuf, WorkerError> {
    let mut arguments = std::env::args_os();
    let _executable = arguments.next();
    if arguments.next().as_deref() != Some(std::ffi::OsStr::new(WORKER_JOB_ARGUMENT)) {
        return Err(invalid_session("worker re-entry argument is missing"));
    }
    let path = arguments
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| invalid_session("worker job path is missing"))?;
    if arguments.next().is_some() {
        return Err(invalid_session("worker re-entry has unexpected arguments"));
    }
    Ok(path)
}

#[cfg(target_os = "macos")]
fn invalid_session(message: impl Into<String>) -> WorkerError {
    WorkerError::InvalidJob(message.into())
}

#[cfg(target_os = "macos")]
fn is_clean_absolute_path(path: &Path) -> bool {
    let mut components = path.components();
    matches!(components.next(), Some(Component::RootDir))
        && components.all(|component| matches!(component, Component::Normal(_)))
}

#[cfg(target_os = "macos")]
fn open_progress_file(path: &Path, session: &PrivilegedSession) -> Result<File, WorkerError> {
    session.validate_existing_file(path, "worker-progress.jsonl", Some(0), true)?;
    let path_metadata = fs::symlink_metadata(path).map_err(WorkerError::Progress)?;
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(WorkerError::Progress)?;
    let opened_metadata = file.metadata().map_err(WorkerError::Progress)?;
    session.validate_file_metadata(&opened_metadata, "worker-progress.jsonl", Some(0), true)?;

    {
        use std::os::unix::fs::MetadataExt;

        if path_metadata.dev() != opened_metadata.dev()
            || path_metadata.ino() != opened_metadata.ino()
        {
            return Err(WorkerError::InvalidJob(
                "progress path changed while it was opened".to_owned(),
            ));
        }
    }
    session.ensure_directory_unchanged()?;

    // Only truncate after proving that the opened descriptor still represents the file checked
    // above. This avoids letting a privileged worker follow a path swapped to a symlink.
    file.set_len(0).map_err(WorkerError::Progress)?;
    Ok(file)
}

trait WorkerTarget: Read + Write + Seek {
    fn sync_all(&self) -> io::Result<()>;
}

impl WorkerTarget for File {
    fn sync_all(&self) -> io::Result<()> {
        Self::sync_all(self)
    }
}

trait WorkerPlatform {
    type Target: WorkerTarget;

    fn validate_staged_image(&mut self, _image: &File) -> Result<(), WorkerError> {
        Ok(())
    }
    fn validate_target(&mut self, selected: &WorkerDrive) -> Result<WorkerDrive, WorkerError>;
    fn unmount(&mut self, selected: &WorkerDrive) -> Result<(), WorkerError>;
    fn open_target(
        &mut self,
        selected: &WorkerDrive,
        verify: bool,
    ) -> Result<Self::Target, WorkerError>;
    fn eject(&mut self, selected: &WorkerDrive) -> Result<(), WorkerError>;
}

fn run_and_report<P, S>(
    job: &WorkerJob,
    platform: &mut P,
    progress: &mut S,
) -> Result<FlashReport, WorkerError>
where
    P: WorkerPlatform,
    S: ProgressSink,
{
    let result = run_job(job, platform, progress);
    if let Err(error) = &result {
        progress.emit(&WorkerProgress::failed(error))?;
    }
    result
}

fn run_job<P, S>(
    job: &WorkerJob,
    platform: &mut P,
    progress: &mut S,
) -> Result<FlashReport, WorkerError>
where
    P: WorkerPlatform,
    S: ProgressSink,
{
    validate_job(job)?;
    let signals = WorkerSignals::new(
        job.cancel_path.as_deref(),
        job.skip_verification_path.as_deref(),
    );
    signals.refresh()?;
    signals.check_cancelled()?;

    // The unprivileged process prepared this raw file, but the worker treats all serialized input
    // as mutable and untrusted. Copy it into an unlinked private staging file, then validate that
    // immutable copy before even enumerating the target.
    progress.emit(&WorkerProgress::phase(WorkerPhase::ValidatingImage))?;
    let mut raw_image = stage_raw_image(job, &signals, progress)?;
    platform.validate_staged_image(&raw_image)?;
    signals.refresh()?;
    signals.check_cancelled()?;

    progress.emit(&WorkerProgress::phase(WorkerPhase::ValidatingTarget))?;
    compare_drive(&job.drive, &platform.validate_target(&job.drive)?)?;
    signals.refresh()?;
    signals.check_cancelled()?;

    progress.emit(&WorkerProgress::phase(WorkerPhase::Unmounting))?;
    platform.unmount(&job.drive)?;

    let result = run_after_unmount(job, &mut raw_image, platform, progress, &signals);
    if result.is_err() {
        // Once the volumes are unmounted, every failure path attempts to leave the selected media
        // safely ejected. Preserve the primary error if cleanup also fails.
        let _ = platform.eject(&job.drive);
    }
    result
}

fn stage_raw_image<S>(
    job: &WorkerJob,
    signals: &WorkerSignals,
    progress: &mut S,
) -> Result<File, WorkerError>
where
    S: ProgressSink,
{
    let mut source = open_raw_image(job)?;
    let mut staged = tempfile::tempfile().map_err(FlashError::from)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; STAGING_BUFFER_SIZE];
    let mut copied = 0_u64;

    loop {
        signals.refresh()?;
        signals.check_cancelled()?;
        let count = source.read(&mut buffer).map_err(FlashError::from)?;
        if count == 0 {
            break;
        }
        copied = copied
            .checked_add(u64::try_from(count).expect("staging buffer length fits u64"))
            .ok_or(FlashError::Verification)?;
        if copied > job.raw_size {
            return Err(FlashError::Verification.into());
        }
        staged
            .write_all(&buffer[..count])
            .map_err(FlashError::from)?;
        hasher.update(&buffer[..count]);
        progress.emit(&WorkerProgress {
            schema_version: WORKER_PROGRESS_SCHEMA_VERSION,
            phase: WorkerPhase::ValidatingImage,
            bytes_processed: Some(copied),
            total_bytes: Some(job.raw_size),
            raw_sha256: None,
            verified: None,
            message: None,
        })?;
    }

    let actual_sha256 = hex::encode(hasher.finalize());
    if copied != job.raw_size || !actual_sha256.eq_ignore_ascii_case(&job.expected_raw_sha256) {
        return Err(FlashError::Verification.into());
    }
    staged.sync_all().map_err(FlashError::from)?;
    staged.seek(SeekFrom::Start(0)).map_err(FlashError::from)?;
    Ok(staged)
}

fn run_after_unmount<P, S>(
    job: &WorkerJob,
    raw_image: &mut File,
    platform: &mut P,
    progress: &mut S,
    signals: &WorkerSignals,
) -> Result<FlashReport, WorkerError>
where
    P: WorkerPlatform,
    S: ProgressSink,
{
    signals.refresh()?;
    signals.check_cancelled()?;

    // Device identifiers can be reused after hot-unplug. Validate all identity fields once more
    // after unmount and immediately before opening the internally derived raw-device path.
    progress.emit(&WorkerProgress::phase(WorkerPhase::ValidatingTarget))?;
    compare_drive(&job.drive, &platform.validate_target(&job.drive)?)?;

    let mut target = platform.open_target(&job.drive, job.verify)?;
    let mut callback_error = None;
    let flash_result = write_raw_from(
        raw_image,
        &mut target,
        job.drive.capacity,
        job.verify,
        signals.cancelled_flag(),
        signals.skip_verification_flag(),
        |update| {
            if callback_error.is_some() {
                return;
            }
            if let Err(error) = signals.refresh() {
                signals.request_cancel();
                callback_error = Some(error);
                return;
            }
            if let Err(error) = progress.emit(&progress_from_flash(update)) {
                signals.request_cancel();
                callback_error = Some(error);
            }
        },
    );

    if let Some(error) = callback_error {
        drop(target);
        return Err(error);
    }

    if signals.is_cancelled()? {
        drop(target);
        return Err(WorkerError::Cancelled);
    }

    let report = match flash_result {
        Ok(report)
            if report.bytes_written == job.raw_size
                && report
                    .raw_sha256
                    .eq_ignore_ascii_case(&job.expected_raw_sha256) =>
        {
            report
        }
        Ok(_) => {
            drop(target);
            return Err(FlashError::Verification.into());
        }
        Err(error) => {
            drop(target);
            return Err(error.into());
        }
    };

    progress.emit(&WorkerProgress::phase(WorkerPhase::Syncing))?;
    target.sync_all().map_err(FlashError::from)?;
    drop(target);

    progress.emit(&WorkerProgress::phase(WorkerPhase::Ejecting))?;
    platform.eject(&job.drive)?;
    progress.emit(&WorkerProgress::completed(&report))?;
    Ok(report)
}

fn validate_job(job: &WorkerJob) -> Result<(), WorkerError> {
    if job.schema_version != WORKER_JOB_SCHEMA_VERSION {
        return Err(WorkerError::InvalidJob(format!(
            "unsupported schema version {}",
            job.schema_version
        )));
    }
    let Some(disk_identifier) = disk_identifier(&job.drive.id) else {
        return Err(WorkerError::InvalidJob(
            "invalid whole-disk identity".to_owned(),
        ));
    };
    if job.drive.device != format!("/dev/{disk_identifier}") || job.drive.capacity == 0 {
        return Err(WorkerError::InvalidJob(
            "invalid whole-disk identity".to_owned(),
        ));
    }
    if !valid_sha256(&job.expected_raw_sha256) {
        return Err(WorkerError::InvalidJob(
            "expected raw SHA-256 must contain 64 hexadecimal characters".to_owned(),
        ));
    }
    if !job.raw_path.is_absolute()
        || job
            .cancel_path
            .as_ref()
            .is_some_and(|path| !path.is_absolute())
        || job
            .skip_verification_path
            .as_ref()
            .is_some_and(|path| !path.is_absolute())
        || matches!(
            &job.progress,
            ProgressDestination::File { path } if !path.is_absolute()
        )
    {
        return Err(WorkerError::InvalidJob(
            "worker file paths must be absolute".to_owned(),
        ));
    }
    if job.raw_size == 0 || job.raw_size > job.drive.capacity {
        return Err(WorkerError::InvalidJob(
            "raw image size must fit the selected target".to_owned(),
        ));
    }
    let raw = fs::symlink_metadata(&job.raw_path)
        .map_err(|error| WorkerError::InvalidJob(format!("raw image is not readable: {error}")))?;
    if raw.file_type().is_symlink() || !raw.is_file() {
        return Err(WorkerError::InvalidJob(
            "raw image path must be a non-symlink regular file".to_owned(),
        ));
    }
    if raw.len() != job.raw_size {
        return Err(WorkerError::InvalidJob(
            "raw image size changed after preparation".to_owned(),
        ));
    }
    Ok(())
}

fn open_raw_image(job: &WorkerJob) -> Result<File, WorkerError> {
    let path_metadata = fs::symlink_metadata(&job.raw_path)
        .map_err(|error| WorkerError::InvalidJob(format!("raw image is not readable: {error}")))?;
    if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
        return Err(WorkerError::InvalidJob(
            "raw image path must be a non-symlink regular file".to_owned(),
        ));
    }

    let file = File::open(&job.raw_path)
        .map_err(|error| WorkerError::InvalidJob(format!("raw image is not readable: {error}")))?;
    let opened_metadata = file
        .metadata()
        .map_err(|error| WorkerError::InvalidJob(format!("raw image is not readable: {error}")))?;
    if !opened_metadata.is_file() || opened_metadata.len() != job.raw_size {
        return Err(WorkerError::InvalidJob(
            "raw image changed after preparation".to_owned(),
        ));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        if path_metadata.dev() != opened_metadata.dev()
            || path_metadata.ino() != opened_metadata.ino()
        {
            return Err(WorkerError::InvalidJob(
                "raw image changed while it was opened".to_owned(),
            ));
        }
    }
    Ok(file)
}

const fn progress_from_flash(progress: FlashProgress) -> WorkerProgress {
    let phase = match progress.stage {
        FlashStage::Decompressing => WorkerPhase::ValidatingImage,
        FlashStage::Writing => WorkerPhase::Writing,
        FlashStage::Verifying => WorkerPhase::Verifying,
    };
    WorkerProgress {
        schema_version: WORKER_PROGRESS_SCHEMA_VERSION,
        phase,
        bytes_processed: Some(progress.processed),
        total_bytes: progress.total,
        raw_sha256: None,
        verified: None,
        message: None,
    }
}

fn disk_identifier(id: &str) -> Option<&str> {
    let (disk_id, stable_suffix) = id
        .split_once('@')
        .map_or((id, None), |(disk_id, suffix)| (disk_id, Some(suffix)));
    let suffix = disk_id.strip_prefix("disk")?;
    if suffix.is_empty()
        || !suffix.bytes().all(|byte| byte.is_ascii_digit())
        || stable_suffix.is_some_and(|value| {
            value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return None;
    }
    Some(disk_id)
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn compare_drive(selected: &WorkerDrive, current: &WorkerDrive) -> Result<(), WorkerError> {
    if selected == current {
        Ok(())
    } else {
        Err(WorkerError::TargetChanged)
    }
}

struct WorkerSignals {
    cancelled: AtomicBool,
    skip_verification: AtomicBool,
    cancel_path: Option<PathBuf>,
    skip_verification_path: Option<PathBuf>,
}

impl WorkerSignals {
    fn new(cancel_path: Option<&Path>, skip_verification_path: Option<&Path>) -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            skip_verification: AtomicBool::new(false),
            cancel_path: cancel_path.map(Path::to_path_buf),
            skip_verification_path: skip_verification_path.map(Path::to_path_buf),
        }
    }

    const fn cancelled_flag(&self) -> &AtomicBool {
        &self.cancelled
    }

    const fn skip_verification_flag(&self) -> &AtomicBool {
        &self.skip_verification
    }

    fn request_cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    fn check_cancelled(&self) -> Result<(), WorkerError> {
        if self.cancelled.load(Ordering::Relaxed) {
            Err(WorkerError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn is_cancelled(&self) -> Result<bool, WorkerError> {
        self.refresh()?;
        Ok(self.cancelled.load(Ordering::Relaxed))
    }

    fn refresh(&self) -> Result<(), WorkerError> {
        if marker_exists(self.cancel_path.as_deref())? {
            self.cancelled.store(true, Ordering::Relaxed);
        }
        if marker_exists(self.skip_verification_path.as_deref())? {
            self.skip_verification.store(true, Ordering::Relaxed);
        }
        Ok(())
    }
}

fn marker_exists(path: Option<&Path>) -> Result<bool, WorkerError> {
    let Some(path) = path else {
        return Ok(false);
    };
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(WorkerError::Platform(format!(
            "could not inspect worker marker path: {error}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};
    #[cfg(target_os = "macos")]
    use tempfile::NamedTempFile;
    use tempfile::TempDir;

    use super::*;

    struct FilePlatform {
        current: WorkerDrive,
        target_path: PathBuf,
        mutate_raw_after_staging: Option<(PathBuf, Vec<u8>)>,
        validations: usize,
        fail_second_validation: bool,
        unmounted: bool,
        ejected: bool,
    }

    impl WorkerPlatform for FilePlatform {
        type Target = File;

        fn validate_target(&mut self, _selected: &WorkerDrive) -> Result<WorkerDrive, WorkerError> {
            self.validations += 1;
            if self.validations == 1
                && let Some((path, replacement)) = self.mutate_raw_after_staging.take()
            {
                fs::write(path, replacement)
                    .map_err(|error| WorkerError::Platform(error.to_string()))?;
            }
            if self.fail_second_validation && self.validations == 2 {
                return Err(WorkerError::TargetMissing);
            }
            Ok(self.current.clone())
        }

        fn unmount(&mut self, _selected: &WorkerDrive) -> Result<(), WorkerError> {
            self.unmounted = true;
            Ok(())
        }

        fn open_target(
            &mut self,
            _selected: &WorkerDrive,
            verify: bool,
        ) -> Result<Self::Target, WorkerError> {
            OpenOptions::new()
                .read(verify)
                .write(true)
                .open(&self.target_path)
                .map_err(|error| WorkerError::Platform(error.to_string()))
        }

        fn eject(&mut self, _selected: &WorkerDrive) -> Result<(), WorkerError> {
            self.ejected = true;
            Ok(())
        }
    }

    #[derive(Default)]
    struct Events(Vec<WorkerProgress>);

    impl ProgressSink for Events {
        fn emit(&mut self, progress: &WorkerProgress) -> Result<(), WorkerError> {
            self.0.push(progress.clone());
            Ok(())
        }
    }

    fn fixture() -> (TempDir, WorkerJob, FilePlatform, Vec<u8>) {
        let directory = TempDir::new().unwrap();
        let payload = b"snapdog worker image".repeat(32_768);
        let raw_path = directory.path().join("image.img");
        fs::write(&raw_path, &payload).unwrap();

        let target_path = directory.path().join("target.img");
        let target = File::create(&target_path).unwrap();
        target.set_len(payload.len() as u64 + 4096).unwrap();
        drop(target);

        let drive = WorkerDrive {
            id: "disk42".to_owned(),
            device: "/dev/disk42".to_owned(),
            capacity: payload.len() as u64 + 4096,
        };
        let job = WorkerJob {
            schema_version: WORKER_JOB_SCHEMA_VERSION,
            drive: drive.clone(),
            raw_path,
            raw_size: payload.len() as u64,
            verify: true,
            expected_raw_sha256: hex::encode(Sha256::digest(&payload)),
            progress: ProgressDestination::Stdout,
            cancel_path: None,
            skip_verification_path: None,
        };
        let platform = FilePlatform {
            current: drive,
            target_path,
            mutate_raw_after_staging: None,
            validations: 0,
            fail_second_validation: false,
            unmounted: false,
            ejected: false,
        };
        (directory, job, platform, payload)
    }

    #[test]
    fn job_round_trips_through_json() {
        let (_directory, job, _platform, _payload) = fixture();
        let json = serde_json::to_string(&job).unwrap();
        assert_eq!(serde_json::from_str::<WorkerJob>(&json).unwrap(), job);
    }

    #[test]
    fn file_backend_runs_full_flash_and_verification() {
        let (_directory, job, mut platform, payload) = fixture();
        let target_path = platform.target_path.clone();
        let mut events = Events::default();

        let report = run_and_report(&job, &mut platform, &mut events).unwrap();

        assert!(report.verified);
        assert_eq!(report.bytes_written, payload.len() as u64);
        assert_eq!(&fs::read(target_path).unwrap()[..payload.len()], payload);
        assert_eq!(platform.validations, 2);
        assert!(platform.unmounted);
        assert!(platform.ejected);
        assert_eq!(events.0.last().unwrap().phase, WorkerPhase::Completed);
        assert!(
            events
                .0
                .iter()
                .any(|event| event.phase == WorkerPhase::Writing)
        );
        assert!(
            events
                .0
                .iter()
                .any(|event| event.phase == WorkerPhase::Verifying)
        );
    }

    #[test]
    fn changed_drive_is_rejected_before_unmount() {
        let (_directory, job, mut platform, _payload) = fixture();
        platform.current.capacity += 1;
        let mut events = Events::default();

        let result = run_and_report(&job, &mut platform, &mut events);

        assert!(matches!(result, Err(WorkerError::TargetChanged)));
        assert!(!platform.unmounted);
        assert!(!platform.ejected);
        assert_eq!(events.0.last().unwrap().phase, WorkerPhase::Failed);
    }

    #[test]
    fn failure_after_unmount_attempts_eject() {
        let (_directory, job, mut platform, _payload) = fixture();
        platform.fail_second_validation = true;
        let mut events = Events::default();

        let result = run_and_report(&job, &mut platform, &mut events);

        assert!(matches!(result, Err(WorkerError::TargetMissing)));
        assert!(platform.unmounted);
        assert!(platform.ejected);
        assert_eq!(platform.validations, 2);
    }

    #[test]
    fn existing_cancel_path_prevents_target_validation() {
        let (directory, mut job, mut platform, _payload) = fixture();
        let cancel = directory.path().join("cancel");
        fs::write(&cancel, []).unwrap();
        job.cancel_path = Some(cancel);
        let mut events = Events::default();

        let result = run_and_report(&job, &mut platform, &mut events);

        assert!(matches!(result, Err(WorkerError::Cancelled)));
        assert_eq!(platform.validations, 0);
        assert_eq!(events.0.last().unwrap().phase, WorkerPhase::Cancelled);
    }

    #[test]
    fn rejects_partition_identifier_and_bad_hash() {
        let (_directory, mut job, mut platform, _payload) = fixture();
        let mut events = Events::default();
        job.drive.id = "disk42s1".to_owned();
        job.drive.device = "/dev/disk42s1".to_owned();
        assert!(matches!(
            run_and_report(&job, &mut platform, &mut events),
            Err(WorkerError::InvalidJob(_))
        ));

        let (_directory, mut job, mut platform, _payload) = fixture();
        let mut events = Events::default();
        job.expected_raw_sha256 = "not-a-hash".to_owned();
        assert!(matches!(
            run_and_report(&job, &mut platform, &mut events),
            Err(WorkerError::InvalidJob(_))
        ));
    }

    #[test]
    fn json_line_sink_emits_parseable_single_line_events() {
        let mut bytes = Vec::new();
        {
            let mut sink = JsonLineProgress::new(&mut bytes);
            sink.emit(&WorkerProgress::phase(WorkerPhase::Syncing))
                .unwrap();
        }
        assert!(bytes.ends_with(b"\n"));
        assert!(!bytes[..bytes.len() - 1].contains(&b'\n'));
        let event: WorkerProgress = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(event.phase, WorkerPhase::Syncing);
    }

    #[test]
    fn existing_skip_path_skips_readback_only() {
        let (directory, mut job, mut platform, payload) = fixture();
        let skip = directory.path().join("skip");
        fs::write(&skip, []).unwrap();
        job.skip_verification_path = Some(skip);
        let target_path = platform.target_path.clone();
        let mut events = Events::default();

        let report = run_and_report(&job, &mut platform, &mut events).unwrap();

        assert!(!report.verified);
        assert_eq!(&fs::read(target_path).unwrap()[..payload.len()], payload);
        assert!(
            !events
                .0
                .iter()
                .any(|event| event.phase == WorkerPhase::Verifying)
        );
    }

    #[test]
    fn raw_hash_is_rechecked_before_target_validation() {
        let (_directory, mut job, mut platform, _payload) = fixture();
        fs::write(&job.raw_path, b"changed after preparation").unwrap();
        job.raw_size = fs::metadata(&job.raw_path).unwrap().len();
        let mut events = Events::default();

        let result = run_and_report(&job, &mut platform, &mut events);

        assert!(matches!(result, Err(WorkerError::Flash(_))));
        assert_eq!(platform.validations, 0);
        assert!(!platform.unmounted);
    }

    #[test]
    fn staged_image_cannot_be_changed_through_the_original_path() {
        let (_directory, job, mut platform, payload) = fixture();
        let target_path = platform.target_path.clone();
        let replacement = vec![b'X'; payload.len()];
        platform.mutate_raw_after_staging = Some((job.raw_path.clone(), replacement.clone()));
        let mut events = Events::default();

        let report = run_and_report(&job, &mut platform, &mut events).unwrap();

        assert!(report.verified);
        assert_eq!(fs::read(&job.raw_path).unwrap(), replacement);
        assert_eq!(&fs::read(target_path).unwrap()[..payload.len()], payload);
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn raw_image_symlink_is_rejected_before_target_validation() {
        use std::os::unix::fs::symlink;

        let (directory, mut job, mut platform, _payload) = fixture();
        let link = directory.path().join("linked.img");
        symlink(&job.raw_path, &link).unwrap();
        job.raw_path = link;
        let mut events = Events::default();

        let result = run_and_report(&job, &mut platform, &mut events);

        assert!(matches!(result, Err(WorkerError::InvalidJob(_))));
        assert_eq!(platform.validations, 0);
    }

    #[test]
    fn oversized_raw_image_is_rejected_before_target_validation() {
        let (_directory, mut job, mut platform, _payload) = fixture();
        job.drive.capacity = job.raw_size - 1;
        platform.current.capacity = job.drive.capacity;
        let mut events = Events::default();

        let result = run_and_report(&job, &mut platform, &mut events);

        assert!(matches!(result, Err(WorkerError::InvalidJob(_))));
        assert_eq!(platform.validations, 0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn progress_file_must_already_be_regular_file() {
        let (_directory, job, job_path) = privileged_session_fixture();
        let session = PrivilegedSession::validate(&job, &job_path).unwrap();
        let ProgressDestination::File { path } = &job.progress else {
            panic!("fixture must use file progress");
        };
        fs::remove_file(path).unwrap();
        assert!(open_progress_file(path, &session).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn privileged_session_rejects_progress_outside_its_private_directory() {
        let (_directory, mut job, job_path) = privileged_session_fixture();
        let outside = NamedTempFile::new().unwrap();
        job.progress = ProgressDestination::File {
            path: outside.path().to_path_buf(),
        };

        assert!(matches!(
            PrivilegedSession::validate(&job, &job_path),
            Err(WorkerError::InvalidJob(_))
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn privileged_session_rejects_multiply_linked_progress_file() {
        let (_directory, job, job_path) = privileged_session_fixture();
        let ProgressDestination::File { path } = &job.progress else {
            panic!("fixture must use file progress");
        };
        let outside = NamedTempFile::new().unwrap();
        fs::remove_file(path).unwrap();
        fs::hard_link(outside.path(), path).unwrap();

        assert!(matches!(
            PrivilegedSession::validate(&job, &job_path),
            Err(WorkerError::InvalidJob(_))
        ));
    }

    #[cfg(target_os = "macos")]
    fn privileged_session_fixture() -> (TempDir, WorkerJob, PathBuf) {
        let directory = TempDir::new().unwrap();
        let payload = b"session image";
        let raw_path = directory.path().join("snapdog-os.img");
        fs::write(&raw_path, payload).unwrap();
        let progress_path = directory.path().join("worker-progress.jsonl");
        fs::write(&progress_path, []).unwrap();
        let job_path = directory.path().join("worker-job.json");
        fs::write(&job_path, b"{}").unwrap();
        let job = WorkerJob {
            schema_version: WORKER_JOB_SCHEMA_VERSION,
            drive: WorkerDrive {
                id: "disk42".to_owned(),
                device: "/dev/disk42".to_owned(),
                capacity: 4096,
            },
            raw_path,
            raw_size: payload.len() as u64,
            verify: true,
            expected_raw_sha256: hex::encode(Sha256::digest(payload)),
            progress: ProgressDestination::File {
                path: progress_path,
            },
            cancel_path: Some(directory.path().join("cancel")),
            skip_verification_path: Some(directory.path().join("skip-verification")),
        };
        (directory, job, job_path)
    }
}
