// SPDX-License-Identifier: GPL-3.0-only

//! Isolated flash-worker boundary.
//!
//! Production entry points require a platform-native authorization flow and a [`RawDeviceGate`]. The
//! actual orchestration is backend-driven so all destructive behaviour can be exercised in tests
//! with ordinary files, without ever opening a real device.

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows", test))]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::path::Component;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::flash::{
    FlashError, FlashProgress, FlashReport, FlashStage, FlashWriteOptions,
    write_raw_from_with_prepare,
};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

/// Current serialized worker-job schema.
pub const WORKER_JOB_SCHEMA_VERSION: u32 = 1;
/// Current JSON-lines progress schema.
pub const WORKER_PROGRESS_SCHEMA_VERSION: u32 = 1;

#[cfg(target_os = "macos")]
const RAW_DEVICE_OPT_IN: &str = "SNAPDOG_INSTALLER_ALLOW_RAW_DEVICE_WRITE";
#[cfg(target_os = "macos")]
const RAW_DEVICE_OPT_IN_VALUE: &str = "YES-I-UNDERSTAND";

/// Worker argument carrying the pinned job-file digest across interactive authorization.
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
pub const WORKER_JOB_SHA256_ARGUMENT: &str = "--worker-job-sha256";
/// Windows-compatible name retained for the UAC-specific launch boundary.
#[cfg(target_os = "windows")]
pub const WINDOWS_JOB_SHA256_ARGUMENT: &str = WORKER_JOB_SHA256_ARGUMENT;
/// Windows worker argument proving intentional raw-device re-entry.
#[cfg(target_os = "windows")]
pub const WINDOWS_RAW_DEVICE_OPT_IN_ARGUMENT: &str = "--raw-device-opt-in";
/// Exact Windows worker opt-in value.
#[cfg(target_os = "windows")]
pub const WINDOWS_RAW_DEVICE_OPT_IN_VALUE: &str = "YES-I-UNDERSTAND";

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
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
    /// Private Unix socket used to pass an `authopen`-authorized target descriptor on macOS.
    #[serde(default)]
    pub macos_target_socket: Option<PathBuf>,
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
    #[error("the privileged worker must run with administrator privileges")]
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
/// On macOS it can only be obtained by the explicitly opted-in isolated worker; the worker still
/// cannot open a raw device by path and must receive the exact `authopen` descriptor from the GUI.
/// Linux and Windows retain their elevated-worker checks. Tests use a file-backed platform.
#[derive(Debug)]
pub struct RawDeviceGate {
    _private: (),
}

impl RawDeviceGate {
    /// Validate the explicit opt-in for the isolated macOS worker.
    #[cfg(target_os = "macos")]
    pub fn from_environment() -> Result<Self, WorkerError> {
        if std::env::var_os(RAW_DEVICE_OPT_IN).as_deref()
            != Some(std::ffi::OsStr::new(RAW_DEVICE_OPT_IN_VALUE))
        {
            return Err(WorkerError::RawDeviceDisabled);
        }

        Ok(Self { _private: () })
    }

    /// Validate the root identity supplied by `PolicyKit`.
    ///
    /// Linux deliberately does not transport the accidental-write environment opt-in through a
    /// generic elevated `env` process. The exact worker CLI, private session contract, root token,
    /// and worker-side target revalidation form the authorization boundary instead.
    #[cfg(target_os = "linux")]
    pub fn from_environment() -> Result<Self, WorkerError> {
        Self::from_root_worker()
    }

    #[cfg(target_os = "linux")]
    fn from_root_worker() -> Result<Self, WorkerError> {
        let output = ["/usr/bin/id", "/bin/id"]
            .into_iter()
            .find_map(|program| std::process::Command::new(program).arg("-u").output().ok())
            .ok_or_else(|| {
                WorkerError::Platform("could not determine the worker user ID".to_owned())
            })?;
        if !output.status.success() || output.stdout != b"0\n" {
            return Err(WorkerError::NotRoot);
        }
        Ok(Self { _private: () })
    }

    /// Validate the explicit opt-in and an elevated Windows administrator token.
    #[cfg(target_os = "windows")]
    pub fn from_environment() -> Result<Self, WorkerError> {
        if !windows_worker_arguments_are_authorized() {
            return Err(WorkerError::RawDeviceDisabled);
        }
        if !crate::windows_native::is_elevated()
            .map_err(|error| WorkerError::Platform(error.to_string()))?
        {
            return Err(WorkerError::NotRoot);
        }
        Ok(Self { _private: () })
    }
}

#[cfg(target_os = "windows")]
fn windows_worker_arguments_are_authorized() -> bool {
    let mut arguments = std::env::args_os();
    let _executable = arguments.next();
    arguments.next().as_deref() == Some(std::ffi::OsStr::new(WORKER_JOB_ARGUMENT))
        && arguments.next().is_some()
        && arguments.next().as_deref() == Some(std::ffi::OsStr::new(WINDOWS_JOB_SHA256_ARGUMENT))
        && arguments
            .next()
            .is_some_and(|value| valid_sha256(&value.to_string_lossy()))
        && arguments.next().as_deref()
            == Some(std::ffi::OsStr::new(WINDOWS_RAW_DEVICE_OPT_IN_ARGUMENT))
        && arguments.next().as_deref()
            == Some(std::ffi::OsStr::new(WINDOWS_RAW_DEVICE_OPT_IN_VALUE))
        && arguments.next().is_none()
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
    let socket = job.macos_target_socket.clone().ok_or_else(|| {
        WorkerError::InvalidJob(
            "macOS raw-device writing requires an authorized target socket".to_owned(),
        )
    })?;
    run_unix_worker(job, &mut macos::MacOsPlatform::new(socket))
}

/// Run one raw-device job on Linux after native privilege elevation.
#[cfg(target_os = "linux")]
pub fn run_linux_worker(job: &WorkerJob, _gate: RawDeviceGate) -> Result<FlashReport, WorkerError> {
    run_unix_worker(job, &mut linux::LinuxPlatform)
}

#[cfg(unix)]
fn run_unix_worker<P>(job: &WorkerJob, platform: &mut P) -> Result<FlashReport, WorkerError>
where
    P: WorkerPlatform,
{
    let session = PrivilegedSession::validate(job, &current_worker_job_path()?)?;
    validate_job(job)?;
    let writer: Box<dyn Write> = match &job.progress {
        ProgressDestination::Stdout => Box::new(io::stdout()),
        ProgressDestination::File { path } => Box::new(open_progress_file(path, &session)?),
    };
    let mut progress = JsonLineProgress::new(writer);
    run_and_report(job, platform, &mut progress)
}

/// Run one raw-device job on Windows after UAC elevation.
#[cfg(target_os = "windows")]
pub fn run_windows_worker(
    job: &WorkerJob,
    _gate: RawDeviceGate,
) -> Result<FlashReport, WorkerError> {
    let session = WindowsPrivilegedSession::validate(job, &current_worker_job_path()?)?;
    validate_job(job)?;
    let writer: Box<dyn Write> = match &job.progress {
        ProgressDestination::Stdout => Box::new(io::stdout()),
        ProgressDestination::File { path } => Box::new(open_windows_progress_file(path, &session)?),
    };
    let mut progress = JsonLineProgress::new(writer);
    run_and_report(job, &mut windows::WindowsPlatform::default(), &mut progress)
}

#[cfg(unix)]
#[derive(Debug)]
struct PrivilegedSession {
    directory: PathBuf,
    owner_uid: u32,
    directory_device: u64,
    directory_inode: u64,
    parent_owner_uid: u32,
    parent_mode: u32,
    parent_device: u64,
    parent_inode: u64,
}

#[cfg(unix)]
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
            || metadata.mode() & 0o7777 != 0o700
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
        if !valid_session_parent(&parent_metadata, metadata.uid()) {
            return Err(invalid_session(
                "session directory must live inside an approved private or sticky temporary directory",
            ));
        }

        let session = Self {
            directory,
            owner_uid: metadata.uid(),
            directory_device: metadata.dev(),
            directory_inode: metadata.ino(),
            parent_owner_uid: parent_metadata.uid(),
            parent_mode: parent_metadata.mode(),
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
        #[cfg(target_os = "macos")]
        if let Some(path) = job.macos_target_socket.as_deref() {
            session.validate_socket(path, "authorized-target.sock")?;
        }
        Ok(session)
    }

    #[cfg(target_os = "macos")]
    fn validate_socket(&self, path: &Path, expected_name: &str) -> Result<(), WorkerError> {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};

        self.validate_member_path(path, expected_name)?;
        self.ensure_directory_unchanged()?;
        let metadata = fs::symlink_metadata(path).map_err(|error| {
            invalid_session(format!("authorized target socket is unavailable: {error}"))
        })?;
        if !metadata.file_type().is_socket()
            || metadata.uid() != self.owner_uid
            || metadata.nlink() != 1
            || metadata.mode() & 0o077 != 0
        {
            return Err(invalid_session(
                "authorized target socket has unsafe ownership, permissions, links, or type",
            ));
        }
        self.ensure_directory_unchanged()
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
        self.validate_file_metadata(&metadata, expected_name, expected_size, require_empty)?;
        self.ensure_directory_unchanged()
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
            || metadata.mode() & 0o7777 != 0o700
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
            || parent_metadata.uid() != self.parent_owner_uid
            || parent_metadata.mode() != self.parent_mode
            || parent_metadata.dev() != self.parent_device
            || parent_metadata.ino() != self.parent_inode
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
fn valid_session_parent(metadata: &fs::Metadata, owner_uid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;

    !metadata.file_type().is_symlink()
        && metadata.is_dir()
        && metadata.uid() == owner_uid
        && metadata.mode() & 0o7777 == 0o700
}

#[cfg(target_os = "linux")]
fn valid_session_parent(metadata: &fs::Metadata, owner_uid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;

    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return false;
    }
    let private_owner = metadata.uid() == owner_uid && metadata.mode() & 0o7777 == 0o700;
    let root_sticky_temp = metadata.uid() == 0 && metadata.mode() & 0o7777 == 0o1777;
    private_owner || root_sticky_temp
}

#[cfg(target_os = "windows")]
#[derive(Debug)]
struct WindowsPrivilegedSession {
    directory: PathBuf,
    directory_handle: same_file::Handle,
}

#[cfg(target_os = "windows")]
impl WindowsPrivilegedSession {
    fn validate(job: &WorkerJob, job_path: &Path) -> Result<Self, WorkerError> {
        let directory = job
            .raw_path
            .parent()
            .ok_or_else(|| invalid_session("raw image has no parent directory"))?
            .to_path_buf();
        if !is_clean_windows_absolute_path(&directory) {
            return Err(invalid_session("session directory path is not canonical"));
        }
        validate_windows_local_fixed_disk(&directory)?;
        let metadata = fs::symlink_metadata(&directory).map_err(|error| {
            invalid_session(format!("session directory is unavailable: {error}"))
        })?;
        validate_windows_metadata(&metadata, true, None, false, "session directory")?;
        let directory_handle = same_file::Handle::from_path(&directory).map_err(|error| {
            invalid_session(format!("could not pin session directory: {error}"))
        })?;
        let session = Self {
            directory,
            directory_handle,
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
        validate_windows_metadata(
            &metadata,
            false,
            expected_size,
            require_empty,
            expected_name,
        )?;
        self.ensure_directory_unchanged()
    }

    fn validate_marker(&self, path: Option<&Path>, expected_name: &str) -> Result<(), WorkerError> {
        let path = path.ok_or_else(|| {
            invalid_session(format!(
                "session marker {expected_name} is missing from the job"
            ))
        })?;
        self.validate_member_path(path, expected_name)?;
        self.ensure_directory_unchanged()?;
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                validate_windows_metadata(&metadata, false, Some(0), true, expected_name)?;
                self.ensure_directory_unchanged()
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(invalid_session(format!(
                "session marker {expected_name} is unavailable: {error}"
            ))),
        }
    }

    fn validate_member_path(&self, path: &Path, expected_name: &str) -> Result<(), WorkerError> {
        if !is_clean_windows_absolute_path(path)
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
        let metadata = fs::symlink_metadata(&self.directory)
            .map_err(|error| invalid_session(format!("session directory changed: {error}")))?;
        validate_windows_metadata(&metadata, true, None, false, "session directory")?;
        let current = same_file::Handle::from_path(&self.directory)
            .map_err(|error| invalid_session(format!("session directory changed: {error}")))?;
        if current != self.directory_handle {
            return Err(invalid_session(
                "session directory changed during authorization",
            ));
        }
        Ok(())
    }
}

#[cfg(target_os = "windows")]
fn validate_windows_metadata(
    metadata: &fs::Metadata,
    directory: bool,
    expected_size: Option<u64>,
    require_empty: bool,
    name: &str,
) -> Result<(), WorkerError> {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    let expected_type = if directory {
        metadata.is_dir()
    } else {
        metadata.is_file()
    };
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || !expected_type
        || expected_size.is_some_and(|size| metadata.len() != size)
        || (require_empty && metadata.len() != 0)
    {
        return Err(invalid_session(format!(
            "{name} has an unsafe type, reparse point, or size"
        )));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn is_clean_windows_absolute_path(path: &Path) -> bool {
    windows_local_disk(path).is_some()
}

#[cfg(target_os = "windows")]
fn windows_local_disk(path: &Path) -> Option<u8> {
    use std::path::{Component, Prefix};

    let mut components = path.components();
    let Component::Prefix(prefix) = components.next()? else {
        return None;
    };
    let letter = match prefix.kind() {
        Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => letter,
        Prefix::Verbatim(_)
        | Prefix::VerbatimUNC(_, _)
        | Prefix::DeviceNS(_)
        | Prefix::UNC(_, _) => return None,
    };
    if !letter.is_ascii_alphabetic()
        || !matches!(components.next(), Some(Component::RootDir))
        || !components.all(|component| matches!(component, Component::Normal(_)))
    {
        return None;
    }
    Some(letter.to_ascii_uppercase())
}

#[cfg(target_os = "windows")]
fn validate_windows_local_fixed_disk(path: &Path) -> Result<(), WorkerError> {
    if windows_local_disk(path).is_none() {
        return Err(invalid_session(
            "session paths must use a local drive-letter root",
        ));
    }
    if !crate::windows_native::is_fixed_drive_path(path) {
        return Err(invalid_session(
            "worker session must be stored on a ready local fixed disk",
        ));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn open_windows_progress_file(
    path: &Path,
    session: &WindowsPrivilegedSession,
) -> Result<File, WorkerError> {
    session.validate_existing_file(path, "worker-progress.jsonl", Some(0), true)?;
    let path_handle = same_file::Handle::from_path(path).map_err(WorkerError::Progress)?;
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(WorkerError::Progress)?;
    let opened_metadata = file.metadata().map_err(WorkerError::Progress)?;
    validate_windows_metadata(
        &opened_metadata,
        false,
        Some(0),
        true,
        "worker-progress.jsonl",
    )?;
    let opened_handle =
        same_file::Handle::from_file(file.try_clone().map_err(WorkerError::Progress)?)
            .map_err(WorkerError::Progress)?;
    if path_handle != opened_handle {
        return Err(invalid_session("progress path changed while it was opened"));
    }
    session.ensure_directory_unchanged()?;
    file.set_len(0).map_err(WorkerError::Progress)?;
    Ok(file)
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
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

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let valid_trailing_arguments = arguments.next().as_deref()
        == Some(std::ffi::OsStr::new(WORKER_JOB_SHA256_ARGUMENT))
        && arguments
            .next()
            .is_some_and(|value| valid_sha256(&value.to_string_lossy()))
        && arguments.next().is_none();
    #[cfg(target_os = "windows")]
    let valid_trailing_arguments = arguments.next().as_deref()
        == Some(std::ffi::OsStr::new(WINDOWS_JOB_SHA256_ARGUMENT))
        && arguments
            .next()
            .is_some_and(|value| valid_sha256(&value.to_string_lossy()))
        && arguments.next().as_deref()
            == Some(std::ffi::OsStr::new(WINDOWS_RAW_DEVICE_OPT_IN_ARGUMENT))
        && arguments.next().as_deref()
            == Some(std::ffi::OsStr::new(WINDOWS_RAW_DEVICE_OPT_IN_VALUE))
        && arguments.next().is_none();
    if !valid_trailing_arguments {
        return Err(invalid_session("worker re-entry has unexpected arguments"));
    }
    Ok(path)
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn invalid_session(message: impl Into<String>) -> WorkerError {
    WorkerError::InvalidJob(message.into())
}

#[cfg(unix)]
fn is_clean_absolute_path(path: &Path) -> bool {
    let mut components = path.components();
    matches!(components.next(), Some(Component::RootDir))
        && components.all(|component| matches!(component, Component::Normal(_)))
}

#[cfg(unix)]
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
    fn prepare_verification(
        &mut self,
        _selected: &WorkerDrive,
        target: &mut Self::Target,
    ) -> Result<(), FlashError> {
        target.sync_all().map_err(FlashError::from)
    }
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
    if let Err(error) = platform.unmount(&job.drive) {
        // Platform preparation can fail after one or more volumes were already detached. Always
        // give the backend a chance to finish its safe cleanup, while preserving the preparation
        // error that explains why writing never started.
        let _ = platform.eject(&job.drive);
        return Err(error);
    }

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
    let flash_result = write_raw_from_with_prepare(
        raw_image,
        &mut target,
        FlashWriteOptions {
            target_capacity: job.drive.capacity,
            verify: job.verify,
            cancelled: signals.cancelled_flag(),
            skip_verification: signals.skip_verification_flag(),
        },
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
        |target| platform.prepare_verification(&job.drive, target),
    );

    if let Some(error) = callback_error {
        let _ = target.sync_all();
        drop(target);
        return Err(error);
    }

    match signals.is_cancelled() {
        Ok(true) => {
            let _ = target.sync_all();
            drop(target);
            return Err(WorkerError::Cancelled);
        }
        Ok(false) => {}
        Err(error) => {
            let _ = target.sync_all();
            drop(target);
            return Err(error);
        }
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
            let _ = target.sync_all();
            drop(target);
            return Err(FlashError::Verification.into());
        }
        Err(error) => {
            let _ = target.sync_all();
            drop(target);
            return Err(error.into());
        }
    };

    if let Err(error) = progress.emit(&WorkerProgress::phase(WorkerPhase::Syncing)) {
        let _ = target.sync_all();
        drop(target);
        return Err(error);
    }
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
    if !valid_worker_drive(&job.drive) {
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
        || job
            .macos_target_socket
            .as_ref()
            .is_some_and(|path| !path.is_absolute())
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

    let file = open_raw_image_file(&job.raw_path)
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

    #[cfg(target_os = "windows")]
    {
        // UAC introduces a comparatively long hand-off window. Pin both the current path and the
        // opened descriptor, then compare their Windows file identities before any bytes are
        // staged. Re-checking the path metadata also rejects a reparse point installed between
        // the first metadata lookup and File::open.
        let current_path_metadata = fs::symlink_metadata(&job.raw_path).map_err(|error| {
            WorkerError::InvalidJob(format!("raw image is not readable: {error}"))
        })?;
        validate_windows_metadata(
            &current_path_metadata,
            false,
            Some(job.raw_size),
            false,
            "snapdog-os.img",
        )?;
        validate_windows_metadata(
            &opened_metadata,
            false,
            Some(job.raw_size),
            false,
            "snapdog-os.img",
        )?;
        let path_handle = same_file::Handle::from_path(&job.raw_path).map_err(|error| {
            WorkerError::InvalidJob(format!("raw image identity is unavailable: {error}"))
        })?;
        let opened_handle = same_file::Handle::from_file(file.try_clone().map_err(|error| {
            WorkerError::InvalidJob(format!("raw image identity is unavailable: {error}"))
        })?)
        .map_err(|error| {
            WorkerError::InvalidJob(format!("raw image identity is unavailable: {error}"))
        })?;
        if path_handle != opened_handle {
            return Err(WorkerError::InvalidJob(
                "raw image changed while it was opened".to_owned(),
            ));
        }
    }
    Ok(file)
}

#[cfg(not(target_os = "windows"))]
fn open_raw_image_file(path: &Path) -> io::Result<File> {
    File::open(path)
}

#[cfg(target_os = "windows")]
fn open_raw_image_file(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_SHARE_READ: u32 = 0x1;
    OpenOptions::new()
        .read(true)
        // The unelevated runner already pins this file with the same sharing contract. Keep that
        // protection active while hashing so no new writer or path replacement can race UAC.
        .share_mode(FILE_SHARE_READ)
        .open(path)
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

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
fn valid_worker_drive(drive: &WorkerDrive) -> bool {
    let Some((disk_id, _registry_entry_id)) = crate::drives::macos_stable_disk_id(&drive.id) else {
        return false;
    };
    drive.capacity > 0 && drive.device == format!("/dev/{disk_id}")
}

#[cfg(target_os = "linux")]
fn valid_worker_drive(drive: &WorkerDrive) -> bool {
    let Some((block_name, diskseq)) = drive.id.split_once('@') else {
        return false;
    };
    !block_name.is_empty()
        && block_name.len() <= 128
        && block_name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        && !diskseq.is_empty()
        && diskseq.bytes().all(|byte| byte.is_ascii_digit())
        && diskseq.parse::<u64>().is_ok_and(|value| value > 0)
        && drive.device == format!("/dev/{block_name}")
        && drive.capacity > 0
}

#[cfg(target_os = "windows")]
fn valid_worker_drive(drive: &WorkerDrive) -> bool {
    const PREFIX: &str = r"\\.\PHYSICALDRIVE";

    let Some((device, fingerprint)) = drive.id.rsplit_once('@') else {
        return false;
    };
    let Some(number) = device.strip_prefix(PREFIX) else {
        return false;
    };
    !number.is_empty()
        && number.bytes().all(|byte| byte.is_ascii_digit())
        && number
            .parse::<u32>()
            .is_ok_and(|value| device == format!("{PREFIX}{value}"))
        && fingerprint.len() == 64
        && fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit())
        && drive.device == device
        && drive.capacity > 0
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
    use std::cell::Cell;
    use std::rc::Rc;

    use sha2::{Digest, Sha256};
    #[cfg(unix)]
    use tempfile::NamedTempFile;
    use tempfile::TempDir;

    use super::*;

    struct FilePlatform {
        current: WorkerDrive,
        target_path: PathBuf,
        sync_count: Rc<Cell<usize>>,
        mutate_raw_after_staging: Option<(PathBuf, Vec<u8>)>,
        validations: usize,
        failure: FailureMode,
        unmounted: bool,
        ejected: bool,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FailureMode {
        None,
        SecondValidation,
        Unmount,
        VerificationBarrier,
    }

    struct TrackingFile {
        file: File,
        sync_count: Rc<Cell<usize>>,
    }

    impl Read for TrackingFile {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.file.read(buffer)
        }
    }

    impl Write for TrackingFile {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.file.write(buffer)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.file.flush()
        }
    }

    impl Seek for TrackingFile {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            self.file.seek(position)
        }
    }

    impl WorkerTarget for TrackingFile {
        fn sync_all(&self) -> io::Result<()> {
            self.sync_count.set(self.sync_count.get() + 1);
            self.file.sync_all()
        }
    }

    impl WorkerPlatform for FilePlatform {
        type Target = TrackingFile;

        fn validate_target(&mut self, _selected: &WorkerDrive) -> Result<WorkerDrive, WorkerError> {
            self.validations += 1;
            if self.validations == 1
                && let Some((path, replacement)) = self.mutate_raw_after_staging.take()
            {
                fs::write(path, replacement)
                    .map_err(|error| WorkerError::Platform(error.to_string()))?;
            }
            if self.failure == FailureMode::SecondValidation && self.validations == 2 {
                return Err(WorkerError::TargetMissing);
            }
            Ok(self.current.clone())
        }

        fn unmount(&mut self, _selected: &WorkerDrive) -> Result<(), WorkerError> {
            self.unmounted = true;
            if self.failure == FailureMode::Unmount {
                Err(WorkerError::Platform("partial unmount failed".to_owned()))
            } else {
                Ok(())
            }
        }

        fn open_target(
            &mut self,
            _selected: &WorkerDrive,
            verify: bool,
        ) -> Result<Self::Target, WorkerError> {
            let file = OpenOptions::new()
                .read(verify)
                .write(true)
                .open(&self.target_path)
                .map_err(|error| WorkerError::Platform(error.to_string()))?;
            Ok(TrackingFile {
                file,
                sync_count: Rc::clone(&self.sync_count),
            })
        }

        fn prepare_verification(
            &mut self,
            _selected: &WorkerDrive,
            target: &mut Self::Target,
        ) -> Result<(), FlashError> {
            target.sync_all().map_err(FlashError::from)?;
            if self.failure == FailureMode::VerificationBarrier {
                Err(FlashError::Io(io::Error::other(
                    "injected verification barrier failure",
                )))
            } else {
                Ok(())
            }
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

    struct CancellingEvents {
        cancel_path: PathBuf,
        events: Vec<WorkerProgress>,
    }

    impl ProgressSink for CancellingEvents {
        fn emit(&mut self, progress: &WorkerProgress) -> Result<(), WorkerError> {
            self.events.push(progress.clone());
            if progress.phase == WorkerPhase::Writing && !self.cancel_path.exists() {
                fs::write(&self.cancel_path, []).map_err(WorkerError::Progress)?;
            }
            Ok(())
        }
    }

    #[cfg(target_os = "macos")]
    fn test_worker_drive(capacity: u64) -> WorkerDrive {
        WorkerDrive {
            id: "disk42@4242".to_owned(),
            device: "/dev/disk42".to_owned(),
            capacity,
        }
    }

    #[cfg(target_os = "linux")]
    fn test_worker_drive(capacity: u64) -> WorkerDrive {
        WorkerDrive {
            id: "sdz@4242".to_owned(),
            device: "/dev/sdz".to_owned(),
            capacity,
        }
    }

    #[cfg(target_os = "windows")]
    fn test_worker_drive(capacity: u64) -> WorkerDrive {
        let device = r"\\.\PHYSICALDRIVE42".to_owned();
        WorkerDrive {
            id: format!("{device}@{}", "a".repeat(64)),
            device,
            capacity,
        }
    }

    fn fixture() -> (TempDir, WorkerJob, FilePlatform, Vec<u8>) {
        let directory = TempDir::new().unwrap();
        let payload = b"snapdog worker image".repeat(131_072);
        let raw_path = directory.path().join("image.img");
        fs::write(&raw_path, &payload).unwrap();

        let target_path = directory.path().join("target.img");
        let target = File::create(&target_path).unwrap();
        target.set_len(payload.len() as u64 + 4096).unwrap();
        drop(target);

        let drive = test_worker_drive(payload.len() as u64 + 4096);
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
            macos_target_socket: None,
        };
        let platform = FilePlatform {
            current: drive,
            target_path,
            sync_count: Rc::new(Cell::new(0)),
            mutate_raw_after_staging: None,
            validations: 0,
            failure: FailureMode::None,
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
        platform.failure = FailureMode::SecondValidation;
        let mut events = Events::default();

        let result = run_and_report(&job, &mut platform, &mut events);

        assert!(matches!(result, Err(WorkerError::TargetMissing)));
        assert!(platform.unmounted);
        assert!(platform.ejected);
        assert_eq!(platform.validations, 2);
    }

    #[test]
    fn partial_unmount_failure_attempts_eject() {
        let (_directory, job, mut platform, _payload) = fixture();
        platform.failure = FailureMode::Unmount;
        let mut events = Events::default();

        let result = run_and_report(&job, &mut platform, &mut events);

        assert!(matches!(result, Err(WorkerError::Platform(_))));
        assert!(platform.unmounted);
        assert!(platform.ejected);
        assert_eq!(platform.validations, 1);
    }

    #[test]
    fn verification_failure_syncs_target_before_cleanup() {
        let (_directory, job, mut platform, _payload) = fixture();
        platform.failure = FailureMode::VerificationBarrier;
        let mut events = Events::default();

        let result = run_and_report(&job, &mut platform, &mut events);

        assert!(matches!(result, Err(WorkerError::Flash(_))));
        // The platform barrier performs the first durable flush. The error path must make a
        // second best-effort sync before dropping the target and attempting cleanup.
        assert!(platform.sync_count.get() >= 2);
        assert!(platform.ejected);
    }

    #[test]
    fn cancellation_after_writing_starts_syncs_target_before_cleanup() {
        let (directory, mut job, mut platform, _payload) = fixture();
        let cancel_path = directory.path().join("cancel-during-write");
        job.cancel_path = Some(cancel_path.clone());
        let mut events = CancellingEvents {
            cancel_path,
            events: Vec::new(),
        };

        let result = run_and_report(&job, &mut platform, &mut events);

        assert!(matches!(result, Err(WorkerError::Cancelled)));
        assert!(platform.sync_count.get() >= 1);
        assert!(platform.ejected);
        assert!(
            events
                .events
                .iter()
                .any(|event| event.phase == WorkerPhase::Writing)
        );
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
        job.drive.id = "invalid/partition".to_owned();
        job.drive.device = "invalid/partition".to_owned();
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
    fn privileged_session_fixture() -> (TempDir, WorkerJob, PathBuf) {
        use std::os::unix::fs::PermissionsExt;

        let directory = TempDir::new().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let payload = b"session image";
        let raw_path = directory.path().join("snapdog-os.img");
        fs::write(&raw_path, payload).unwrap();
        let progress_path = directory.path().join("worker-progress.jsonl");
        fs::write(&progress_path, []).unwrap();
        let job_path = directory.path().join("worker-job.json");
        fs::write(&job_path, b"{}").unwrap();
        let job = WorkerJob {
            schema_version: WORKER_JOB_SCHEMA_VERSION,
            drive: test_worker_drive(4096),
            raw_path,
            raw_size: payload.len() as u64,
            verify: true,
            expected_raw_sha256: hex::encode(Sha256::digest(payload)),
            progress: ProgressDestination::File {
                path: progress_path,
            },
            cancel_path: Some(directory.path().join("cancel")),
            skip_verification_path: Some(directory.path().join("skip-verification")),
            macos_target_socket: None,
        };
        (directory, job, job_path)
    }
}
