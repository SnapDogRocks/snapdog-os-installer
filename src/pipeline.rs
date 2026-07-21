// SPDX-License-Identifier: GPL-3.0-only

//! Unprivileged image preparation and isolated same-executable writer orchestration.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tempfile::TempDir;
use thiserror::Error;

use crate::download::{
    DownloadClient, DownloadError, DownloadProgress, DownloadReport, DownloadRequest,
};
use crate::flash::{FlashError, FlashProgress, PreparedImage, prepare_gzip};
use crate::worker::{
    ProgressDestination, WORKER_JOB_SCHEMA_VERSION, WORKER_PROGRESS_SCHEMA_VERSION, WorkerDrive,
    WorkerJob, WorkerPhase, WorkerProgress,
};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::MacOsWorkerRunner;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::LinuxWorkerRunner;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::WindowsWorkerRunner;

/// Command-line switch used to re-enter this executable as the isolated writer.
pub const WORKER_JOB_ARGUMENT: &str = "--worker-job";

/// Everything required to download, prepare, and flash one image.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PipelineRequest {
    pub image_url: String,
    pub expected_compressed_sha256: Option<String>,
    pub expected_compressed_size: Option<u64>,
    pub expected_raw_size: Option<u64>,
    pub expected_raw_sha256: Option<String>,
    pub drive: WorkerDrive,
    pub verify: bool,
}

/// Progress surfaced to the unprivileged UI.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PipelineEvent {
    Download(DownloadProgress),
    Preparing(FlashProgress),
    AwaitingAuthorization,
    Worker(WorkerProgress),
}

/// Stable metadata retained after temporary pipeline files have been removed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PipelineReport {
    pub bytes_downloaded: u64,
    pub compressed_sha256: String,
    pub raw_size: u64,
    pub raw_sha256: String,
    pub verified: bool,
}

/// Failures while creating a cancel or skip marker.
#[derive(Debug, Error)]
pub enum PipelineControlError {
    #[error("pipeline control is unavailable")]
    Unavailable,
    #[error("could not write a pipeline control marker: {0}")]
    Io(#[from] io::Error),
}

/// End-to-end pipeline failures.
#[derive(Debug, Error)]
pub enum PipelineError {
    #[error("this pipeline control is already running an operation")]
    AlreadyRunning,
    #[error("the image pipeline was cancelled")]
    Cancelled,
    #[error("the prepared image requires {image_size} bytes but the target has {target_size}")]
    TargetTooSmall { image_size: u64, target_size: u64 },
    #[error(transparent)]
    Download(#[from] DownloadError),
    #[error(transparent)]
    Preparation(#[from] FlashError),
    #[error(transparent)]
    Control(#[from] PipelineControlError),
    #[error("could not serialize the isolated writer job: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("pipeline file operation failed: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Runner(#[from] WorkerRunnerError),
    #[error("the isolated writer failed: {0}")]
    WorkerFailed(String),
    #[error("the isolated writer exited without a terminal progress event")]
    MissingTerminalEvent,
}

/// Failures while launching or monitoring the isolated same-executable writer.
#[derive(Debug, Error)]
pub enum WorkerRunnerError {
    #[error("administrator authorization was cancelled or denied")]
    AuthorizationDenied,
    #[error("worker process I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("worker emitted invalid JSON-lines progress: {0}")]
    InvalidProgress(String),
    #[error("worker process failed with status {status}: {message}")]
    Failed { status: String, message: String },
    #[error("an isolated writer runner is not implemented on this platform")]
    Unsupported,
}

/// Paths supplied to a worker runner for one operation.
#[derive(Clone, Copy, Debug)]
pub struct WorkerLaunch<'a> {
    pub job_path: &'a Path,
    pub progress_path: &'a Path,
    pub cancel_path: &'a Path,
    pub skip_verification_path: &'a Path,
    pub cancelled: &'a AtomicBool,
    pub target_device: &'a str,
    pub macos_target_socket: Option<&'a Path>,
}

/// Injectable download boundary used by the real client and file-backed tests.
pub trait ImageDownloader: Send + Sync {
    fn download(
        &self,
        request: &DownloadRequest<'_>,
        cancelled: &AtomicBool,
        progress: &mut dyn FnMut(DownloadProgress),
    ) -> Result<DownloadReport, DownloadError>;
}

impl ImageDownloader for DownloadClient {
    fn download(
        &self,
        request: &DownloadRequest<'_>,
        cancelled: &AtomicBool,
        progress: &mut dyn FnMut(DownloadProgress),
    ) -> Result<DownloadReport, DownloadError> {
        Self::download(self, request, cancelled, progress)
    }
}

/// Injectable privileged process boundary. Test implementations must remain file-backed.
pub trait WorkerRunner: Send + Sync {
    fn run(
        &self,
        launch: WorkerLaunch<'_>,
        progress: &mut dyn FnMut(WorkerProgress),
    ) -> Result<(), WorkerRunnerError>;
}

/// Thread-safe, one-operation control handle for cancellation and verification skipping.
#[derive(Clone, Default)]
pub struct PipelineControl {
    inner: Arc<ControlInner>,
}

impl PipelineControl {
    /// Request cancellation. During worker execution this atomically creates its marker file.
    pub fn cancel(&self) -> Result<(), PipelineControlError> {
        self.inner.cancelled.store(true, Ordering::Release);
        if let Some(path) = self.marker_paths()?.map(|paths| paths.cancel) {
            touch_marker(&path)?;
        }
        Ok(())
    }

    /// Skip read-back verification once the worker reaches that phase.
    pub fn skip_verification(&self) -> Result<(), PipelineControlError> {
        self.inner.skip_verification.store(true, Ordering::Release);
        if let Some(path) = self.marker_paths()?.map(|paths| paths.skip_verification) {
            touch_marker(&path)?;
        }
        Ok(())
    }

    /// Whether cancellation has been requested for this one-shot control.
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    /// Whether verification skipping has been requested for this one-shot control.
    pub fn is_verification_skipped(&self) -> bool {
        self.inner.skip_verification.load(Ordering::Acquire)
    }

    fn begin(&self) -> Result<RunGuard<'_>, PipelineError> {
        self.inner
            .active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| PipelineError::AlreadyRunning)?;
        Ok(RunGuard { control: self })
    }

    fn cancelled_flag(&self) -> &AtomicBool {
        &self.inner.cancelled
    }

    fn marker_paths(&self) -> Result<Option<MarkerPaths>, PipelineControlError> {
        self.inner
            .markers
            .lock()
            .map(|paths| paths.clone())
            .map_err(|_| PipelineControlError::Unavailable)
    }
}

#[derive(Default)]
struct ControlInner {
    active: AtomicBool,
    cancelled: AtomicBool,
    skip_verification: AtomicBool,
    markers: Mutex<Option<MarkerPaths>>,
}

#[derive(Clone)]
struct MarkerPaths {
    cancel: PathBuf,
    skip_verification: PathBuf,
}

struct RunGuard<'a> {
    control: &'a PipelineControl,
}

impl RunGuard<'_> {
    fn register_markers(
        &self,
        cancel: &Path,
        skip_verification: &Path,
    ) -> Result<(), PipelineControlError> {
        let markers = MarkerPaths {
            cancel: cancel.to_path_buf(),
            skip_verification: skip_verification.to_path_buf(),
        };
        *self
            .control
            .inner
            .markers
            .lock()
            .map_err(|_| PipelineControlError::Unavailable)? = Some(markers.clone());

        if self.control.is_cancelled() {
            touch_marker(&markers.cancel)?;
        }
        if self.control.is_verification_skipped() {
            touch_marker(&markers.skip_verification)?;
        }
        Ok(())
    }
}

impl Drop for RunGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut paths) = self.control.inner.markers.lock() {
            *paths = None;
        }
        self.control.inner.active.store(false, Ordering::Release);
    }
}

/// Run the complete blocking pipeline. Call this from a background thread, never the UI thread.
pub fn run_pipeline<F>(
    request: &PipelineRequest,
    downloader: &dyn ImageDownloader,
    runner: &dyn WorkerRunner,
    control: &PipelineControl,
    mut emit: F,
) -> Result<PipelineReport, PipelineError>
where
    F: FnMut(PipelineEvent),
{
    let run = control.begin()?;
    ensure_not_cancelled(control)?;
    let temporary = private_temporary_directory()?;
    let archive_path = temporary.path().join("snapdog-os.img.gz");
    let raw_path = temporary.path().join("snapdog-os.img");
    let job_path = temporary.path().join("worker-job.json");
    let progress_path = temporary.path().join("worker-progress.jsonl");
    let cancel_path = temporary.path().join("cancel");
    let skip_path = temporary.path().join("skip-verification");
    let macos_target_socket =
        cfg!(target_os = "macos").then(|| temporary.path().join("authorized-target.sock"));

    let download_request = DownloadRequest {
        url: &request.image_url,
        destination: &archive_path,
        expected_sha256: request.expected_compressed_sha256.as_deref(),
        expected_size: request.expected_compressed_size,
    };
    let mut download_progress = |progress| emit(PipelineEvent::Download(progress));
    let download_report = downloader
        .download(
            &download_request,
            control.cancelled_flag(),
            &mut download_progress,
        )
        .map_err(map_download_error)?;
    ensure_not_cancelled(control)?;

    let mut prepare_progress = |progress| emit(PipelineEvent::Preparing(progress));
    let prepared = prepare_gzip(
        &archive_path,
        &raw_path,
        request.expected_raw_size,
        request.expected_raw_sha256.as_deref(),
        control.cancelled_flag(),
        &mut prepare_progress,
    )
    .map_err(map_preparation_error)?;
    ensure_not_cancelled(control)?;
    ensure_target_capacity(&prepared, &request.drive)?;
    sync_private_file(&raw_path)?;

    create_empty_file(&progress_path)?;
    let worker_job = build_worker_job(
        request,
        &prepared,
        raw_path,
        &progress_path,
        &cancel_path,
        &skip_path,
        macos_target_socket,
    );
    write_json(&job_path, &worker_job)?;
    run.register_markers(&cancel_path, &skip_path)?;
    ensure_not_cancelled(control)?;

    let launch = WorkerLaunch {
        job_path: &job_path,
        progress_path: &progress_path,
        cancel_path: &cancel_path,
        skip_verification_path: &skip_path,
        cancelled: control.cancelled_flag(),
        target_device: &worker_job.drive.device,
        macos_target_socket: worker_job.macos_target_socket.as_deref(),
    };
    emit(PipelineEvent::AwaitingAuthorization);
    let mut terminal = None;
    let mut worker_progress = |progress: WorkerProgress| {
        if matches!(
            progress.phase,
            WorkerPhase::Completed | WorkerPhase::Cancelled | WorkerPhase::Failed
        ) {
            terminal = Some(progress.clone());
        }
        emit(PipelineEvent::Worker(progress));
    };
    let worker_result = runner.run(launch, &mut worker_progress);
    if control.is_cancelled() {
        return Err(PipelineError::Cancelled);
    }
    worker_result?;
    let verified = match terminal {
        Some(progress) if progress.phase == WorkerPhase::Completed => progress
            .verified
            .ok_or(PipelineError::MissingTerminalEvent)?,
        Some(progress) if progress.phase == WorkerPhase::Cancelled => {
            return Err(PipelineError::Cancelled);
        }
        Some(progress) if progress.phase == WorkerPhase::Failed => {
            return Err(PipelineError::WorkerFailed(
                progress
                    .message
                    .unwrap_or_else(|| "unknown worker failure".to_owned()),
            ));
        }
        Some(_) | None => return Err(PipelineError::MissingTerminalEvent),
    };

    Ok(PipelineReport {
        bytes_downloaded: download_report.bytes_downloaded,
        compressed_sha256: download_report.sha256,
        raw_size: prepared.bytes,
        raw_sha256: prepared.raw_sha256,
        verified,
    })
}

fn build_worker_job(
    request: &PipelineRequest,
    prepared: &PreparedImage,
    raw_path: PathBuf,
    progress_path: &Path,
    cancel_path: &Path,
    skip_path: &Path,
    macos_target_socket: Option<PathBuf>,
) -> WorkerJob {
    WorkerJob {
        schema_version: WORKER_JOB_SCHEMA_VERSION,
        drive: request.drive.clone(),
        raw_path,
        raw_size: prepared.bytes,
        verify: request.verify,
        expected_raw_sha256: prepared.raw_sha256.clone(),
        progress: ProgressDestination::File {
            path: progress_path.to_path_buf(),
        },
        cancel_path: Some(cancel_path.to_path_buf()),
        skip_verification_path: Some(skip_path.to_path_buf()),
        macos_target_socket,
    }
}

fn ensure_not_cancelled(control: &PipelineControl) -> Result<(), PipelineError> {
    if control.is_cancelled() {
        Err(PipelineError::Cancelled)
    } else {
        Ok(())
    }
}

fn private_temporary_directory() -> Result<TempDir, io::Error> {
    let directory = TempDir::new()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        // `TempDir::new` follows the process umask and can therefore create mode 0755. The
        // privileged worker deliberately rejects such a session before opening any image or
        // control file. Tighten the empty directory before those files are created.
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(directory)
}

fn map_download_error(error: DownloadError) -> PipelineError {
    if matches!(error, DownloadError::Cancelled) {
        PipelineError::Cancelled
    } else {
        PipelineError::Download(error)
    }
}

fn map_preparation_error(error: FlashError) -> PipelineError {
    if matches!(error, FlashError::Cancelled) {
        PipelineError::Cancelled
    } else {
        PipelineError::Preparation(error)
    }
}

const fn ensure_target_capacity(
    prepared: &PreparedImage,
    drive: &WorkerDrive,
) -> Result<(), PipelineError> {
    if prepared.bytes > drive.capacity {
        Err(PipelineError::TargetTooSmall {
            image_size: prepared.bytes,
            target_size: drive.capacity,
        })
    } else {
        Ok(())
    }
}

fn create_empty_file(path: &Path) -> Result<(), io::Error> {
    let file = OpenOptions::new().write(true).create_new(true).open(path)?;
    set_private_file_permissions(&file)?;
    file.sync_all()
}

fn sync_private_file(path: &Path) -> Result<(), io::Error> {
    let file = OpenOptions::new().write(true).open(path)?;
    set_private_file_permissions(&file)?;
    file.sync_all()
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), PipelineError> {
    let encoded = serde_json::to_vec(value)?;
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    set_private_file_permissions(&file)?;
    file.write_all(&encoded)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

fn touch_marker(path: &Path) -> Result<(), io::Error> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => {
            set_private_file_permissions(&file)?;
            file.sync_all()
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn set_private_file_permissions(file: &std::fs::File) -> Result<(), io::Error> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the no-op implementation preserves one fallible cross-platform call contract"
)]
const fn set_private_file_permissions(_file: &std::fs::File) -> Result<(), io::Error> {
    Ok(())
}

pub(crate) fn validate_progress(progress: &WorkerProgress) -> Result<(), WorkerRunnerError> {
    if progress.schema_version == WORKER_PROGRESS_SCHEMA_VERSION {
        Ok(())
    } else {
        Err(WorkerRunnerError::InvalidProgress(format!(
            "unsupported worker progress schema {}",
            progress.schema_version
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Mutex;

    use flate2::{Compression, write::GzEncoder};
    use sha2::{Digest, Sha256};
    use tempfile::tempdir;

    use super::*;
    use crate::flash::FlashStage;

    struct FileDownloader {
        archive: Vec<u8>,
        calls: AtomicBool,
    }

    impl ImageDownloader for FileDownloader {
        fn download(
            &self,
            request: &DownloadRequest<'_>,
            cancelled: &AtomicBool,
            progress: &mut dyn FnMut(DownloadProgress),
        ) -> Result<DownloadReport, DownloadError> {
            self.calls.store(true, Ordering::Release);
            if cancelled.load(Ordering::Acquire) {
                return Err(DownloadError::Cancelled);
            }
            progress(DownloadProgress {
                downloaded: 0,
                total: Some(self.archive.len() as u64),
            });
            fs::write(request.destination, &self.archive)?;
            progress(DownloadProgress {
                downloaded: self.archive.len() as u64,
                total: Some(self.archive.len() as u64),
            });
            Ok(DownloadReport {
                destination: request.destination.to_path_buf(),
                bytes_downloaded: self.archive.len() as u64,
                sha256: hex::encode(Sha256::digest(&self.archive)),
            })
        }
    }

    #[derive(Default)]
    struct FakeRunner {
        job: Mutex<Option<WorkerJob>>,
        temporary_root: Mutex<Option<PathBuf>>,
        saw_skip: AtomicBool,
    }

    impl WorkerRunner for FakeRunner {
        fn run(
            &self,
            launch: WorkerLaunch<'_>,
            progress: &mut dyn FnMut(WorkerProgress),
        ) -> Result<(), WorkerRunnerError> {
            let job: WorkerJob = serde_json::from_slice(&fs::read(launch.job_path)?)
                .map_err(|error| WorkerRunnerError::InvalidProgress(error.to_string()))?;
            *self.job.lock().expect("job mutex") = Some(job.clone());
            *self.temporary_root.lock().expect("root mutex") =
                launch.job_path.parent().map(Path::to_path_buf);
            assert!(job.raw_path.is_file());
            assert_eq!(fs::metadata(&job.raw_path)?.len(), job.raw_size);
            assert!(launch.progress_path.is_file());
            assert_eq!(job.cancel_path.as_deref(), Some(launch.cancel_path));
            assert_eq!(
                job.skip_verification_path.as_deref(),
                Some(launch.skip_verification_path)
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;

                let session = launch.job_path.parent().expect("session directory");
                assert_eq!(fs::metadata(session)?.permissions().mode() & 0o7777, 0o700);
                for path in [
                    launch.job_path,
                    launch.progress_path,
                    job.raw_path.as_path(),
                ] {
                    assert_eq!(fs::metadata(path)?.permissions().mode() & 0o7777, 0o600);
                }
            }

            progress(worker_progress(WorkerPhase::Writing, None));
            progress(worker_progress(WorkerPhase::Verifying, None));
            self.saw_skip
                .store(launch.skip_verification_path.exists(), Ordering::Release);
            progress(worker_progress(WorkerPhase::Completed, None));
            Ok(())
        }
    }

    struct FileTargetRunner {
        target_path: PathBuf,
    }

    impl WorkerRunner for FileTargetRunner {
        fn run(
            &self,
            launch: WorkerLaunch<'_>,
            progress: &mut dyn FnMut(WorkerProgress),
        ) -> Result<(), WorkerRunnerError> {
            let job: WorkerJob = serde_json::from_slice(&fs::read(launch.job_path)?)
                .map_err(|error| WorkerRunnerError::InvalidProgress(error.to_string()))?;
            let mut target = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&self.target_path)?;
            target.set_len(job.drive.capacity)?;
            let cancelled = AtomicBool::new(false);
            let skip_verification = AtomicBool::new(false);
            let report = crate::flash::write_raw(
                &job.raw_path,
                &mut target,
                job.drive.capacity,
                job.verify,
                &cancelled,
                &skip_verification,
                |update| {
                    let phase = match update.stage {
                        FlashStage::Decompressing => WorkerPhase::ValidatingImage,
                        FlashStage::Writing => WorkerPhase::Writing,
                        FlashStage::Verifying => WorkerPhase::Verifying,
                    };
                    progress(WorkerProgress {
                        schema_version: WORKER_PROGRESS_SCHEMA_VERSION,
                        phase,
                        bytes_processed: Some(update.processed),
                        total_bytes: update.total,
                        raw_sha256: None,
                        verified: None,
                        message: None,
                    });
                    if launch.cancel_path.exists() {
                        cancelled.store(true, Ordering::Release);
                    }
                    if launch.skip_verification_path.exists() {
                        skip_verification.store(true, Ordering::Release);
                    }
                },
            )
            .map_err(|error| WorkerRunnerError::Failed {
                status: "file-backed worker".to_owned(),
                message: error.to_string(),
            })?;
            target.sync_all()?;
            progress(WorkerProgress {
                schema_version: WORKER_PROGRESS_SCHEMA_VERSION,
                phase: WorkerPhase::Completed,
                bytes_processed: Some(report.bytes_written),
                total_bytes: Some(report.bytes_written),
                raw_sha256: Some(report.raw_sha256),
                verified: Some(report.verified),
                message: None,
            });
            Ok(())
        }
    }

    fn worker_progress(phase: WorkerPhase, message: Option<String>) -> WorkerProgress {
        WorkerProgress {
            schema_version: WORKER_PROGRESS_SCHEMA_VERSION,
            phase,
            bytes_processed: None,
            total_bytes: None,
            raw_sha256: None,
            verified: (phase == WorkerPhase::Completed).then_some(true),
            message,
        }
    }

    fn fixture() -> (Vec<u8>, Vec<u8>, PipelineRequest) {
        let raw = b"snapdog pipeline image".repeat(16_384);
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&raw).unwrap();
        let archive = encoder.finish().unwrap();
        let request = PipelineRequest {
            image_url: "https://example.invalid/snapdog.img.gz".to_owned(),
            expected_compressed_sha256: Some(hex::encode(Sha256::digest(&archive))),
            expected_compressed_size: Some(archive.len() as u64),
            expected_raw_size: Some(raw.len() as u64),
            expected_raw_sha256: Some(hex::encode(Sha256::digest(&raw))),
            drive: WorkerDrive {
                id: "disk42".to_owned(),
                device: "/dev/disk42".to_owned(),
                capacity: raw.len() as u64 + 4096,
            },
            verify: true,
        };
        (archive, raw, request)
    }

    #[test]
    fn orchestrates_files_and_removes_temporary_directory() {
        let (archive, raw, request) = fixture();
        let downloader = FileDownloader {
            archive,
            calls: AtomicBool::new(false),
        };
        let runner = FakeRunner::default();
        let control = PipelineControl::default();
        let mut events = Vec::new();

        let report = run_pipeline(&request, &downloader, &runner, &control, |event| {
            events.push(event);
        })
        .unwrap();

        assert!(downloader.calls.load(Ordering::Acquire));
        assert_eq!(report.raw_size, raw.len() as u64);
        assert_eq!(report.raw_sha256, hex::encode(Sha256::digest(&raw)));
        let job = runner.job.lock().unwrap().clone().unwrap();
        assert_eq!(job.raw_size, raw.len() as u64);
        assert_eq!(job.expected_raw_sha256, report.raw_sha256);
        let temporary_root = runner.temporary_root.lock().unwrap().clone().unwrap();
        assert!(!temporary_root.exists());
        assert!(matches!(events.first(), Some(PipelineEvent::Download(_))));
        assert!(events.iter().any(|event| matches!(
            event,
            PipelineEvent::Preparing(FlashProgress { processed, .. }) if *processed > 0
        )));
        assert!(matches!(
            events.last(),
            Some(PipelineEvent::Worker(WorkerProgress {
                phase: WorkerPhase::Completed,
                ..
            }))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn pipeline_session_directory_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let directory = private_temporary_directory().unwrap();
        assert_eq!(
            fs::metadata(directory.path()).unwrap().permissions().mode() & 0o7777,
            0o700
        );
    }

    #[test]
    fn end_to_end_pipeline_writes_and_verifies_a_file_target() {
        let (archive, raw, request) = fixture();
        let downloader = FileDownloader {
            archive,
            calls: AtomicBool::new(false),
        };
        let directory = tempdir().unwrap();
        let target_path = directory.path().join("fake-sd-card.img");
        let runner = FileTargetRunner {
            target_path: target_path.clone(),
        };

        let report = run_pipeline(
            &request,
            &downloader,
            &runner,
            &PipelineControl::default(),
            |_| {},
        )
        .unwrap();

        assert!(report.verified);
        assert_eq!(&fs::read(target_path).unwrap()[..raw.len()], raw);
    }

    #[test]
    fn cancel_before_start_never_calls_downloader_or_runner() {
        let (archive, _raw, request) = fixture();
        let downloader = FileDownloader {
            archive,
            calls: AtomicBool::new(false),
        };
        let runner = FakeRunner::default();
        let control = PipelineControl::default();
        control.cancel().unwrap();

        let result = run_pipeline(&request, &downloader, &runner, &control, |_| {});

        assert!(matches!(result, Err(PipelineError::Cancelled)));
        assert!(!downloader.calls.load(Ordering::Acquire));
        assert!(runner.job.lock().unwrap().is_none());
    }

    #[test]
    fn skip_control_creates_worker_marker() {
        let (archive, _raw, request) = fixture();
        let downloader = FileDownloader {
            archive,
            calls: AtomicBool::new(false),
        };
        let runner = FakeRunner::default();
        let control = PipelineControl::default();

        run_pipeline(&request, &downloader, &runner, &control, |event| {
            if matches!(
                event,
                PipelineEvent::Worker(WorkerProgress {
                    phase: WorkerPhase::Verifying,
                    ..
                })
            ) {
                control.skip_verification().unwrap();
            }
        })
        .unwrap();

        assert!(runner.saw_skip.load(Ordering::Acquire));
    }

    #[test]
    fn target_size_is_checked_before_worker_launch() {
        let (archive, raw, mut request) = fixture();
        request.drive.capacity = raw.len() as u64 - 1;
        let downloader = FileDownloader {
            archive,
            calls: AtomicBool::new(false),
        };
        let runner = FakeRunner::default();

        let result = run_pipeline(
            &request,
            &downloader,
            &runner,
            &PipelineControl::default(),
            |_| {},
        );

        assert!(matches!(result, Err(PipelineError::TargetTooSmall { .. })));
        assert!(runner.job.lock().unwrap().is_none());
    }

    #[test]
    fn control_marker_creation_is_idempotent() {
        let directory = tempdir().unwrap();
        let marker = directory.path().join("cancel");
        touch_marker(&marker).unwrap();
        touch_marker(&marker).unwrap();
        assert!(marker.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            assert_eq!(
                fs::metadata(marker).unwrap().permissions().mode() & 0o7777,
                0o600
            );
        }
    }
}
