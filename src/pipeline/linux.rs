// SPDX-License-Identifier: GPL-3.0-only

//! Linux progress monitoring for the UDisks2-authorized worker.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::Ordering;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::{
    WORKER_JOB_ARGUMENT, WorkerLaunch, WorkerRunner, WorkerRunnerError, touch_marker,
    validate_progress,
};
use crate::worker::{WORKER_JOB_SHA256_ARGUMENT, WorkerProgress};
use sha2::{Digest, Sha256};

const POLL_INTERVAL: Duration = Duration::from_millis(40);
const MAX_PROGRESS_READ: u64 = 1024 * 1024;
const MAX_PROGRESS_RECORD: usize = 64 * 1024;
const MAX_HELPER_STDERR: u64 = 256 * 1024;
const MAX_WORKER_JOB_SIZE: u64 = 64 * 1024;
const O_NOFOLLOW: i32 = 0o400_000;
const RAW_DEVICE_OPT_IN: &str = "SNAPDOG_INSTALLER_ALLOW_UDISKS_WRITE";
const RAW_DEVICE_OPT_IN_VALUE: &str = "YES-I-UNDERSTAND";

/// Re-enters this executable as an unprivileged worker. `UDisks2` performs scoped authorization.
#[derive(Clone, Debug)]
pub struct LinuxWorkerRunner {
    executable: PathBuf,
}

impl LinuxWorkerRunner {
    /// Use the currently running executable for privileged worker re-entry.
    pub fn current() -> Result<Self, WorkerRunnerError> {
        let executable = worker_executable()?;
        validate_executable(&executable)?;
        Ok(Self { executable })
    }

    /// Use an explicit executable path. The second argument is retained for API compatibility.
    ///
    /// This constructor is intended for packaged-launch plumbing and integration tests. Both paths
    /// are still validated before every launch.
    pub fn with_paths(executable: PathBuf, _authorization_helper: PathBuf) -> Self {
        Self { executable }
    }
}

fn worker_executable() -> Result<PathBuf, WorkerRunnerError> {
    std::env::current_exe().map_err(WorkerRunnerError::Io)
}

impl WorkerRunner for LinuxWorkerRunner {
    fn run(
        &self,
        launch: WorkerLaunch<'_>,
        progress: &mut dyn FnMut(WorkerProgress),
    ) -> Result<(), WorkerRunnerError> {
        validate_executable(&self.executable)?;
        let pinned = PinnedLaunchFiles::open(&self.executable, launch.job_path)?;
        let launch_identity = pinned.identity;
        let mut tail = ProgressTail::open(launch.progress_path)?;
        let mut process = ElevatedProcess::spawn(&self.executable, launch.job_path, pinned)?;
        let mut monitor_error = None;
        let mut cancellation_sent = false;

        loop {
            if monitor_error.is_none()
                && !tail.saw_event()
                && let Err(error) =
                    launch_identity.ensure_unchanged(&self.executable, launch.job_path)
            {
                let _ = touch_marker(launch.cancel_path);
                let _ = process.request_stop();
                monitor_error = Some(error);
            }

            if monitor_error.is_none()
                && let Err(error) = tail.drain(progress)
            {
                let _ = touch_marker(launch.cancel_path);
                if !tail.saw_event() {
                    let _ = process.request_stop();
                }
                monitor_error = Some(error);
            }

            if launch.cancelled.load(Ordering::Acquire) && !cancellation_sent {
                if let Err(error) = touch_marker(launch.cancel_path)
                    && monitor_error.is_none()
                {
                    monitor_error = Some(WorkerRunnerError::Io(error));
                }
                // Before the first worker event, UDisks2 may be waiting for PolicyKit. Stop the
                // worker to dismiss that prompt; otherwise the marker stops at the next boundary.
                if !tail.saw_event()
                    && let Err(error) = process.request_stop()
                    && monitor_error.is_none()
                {
                    monitor_error = Some(error);
                }
                cancellation_sent = true;
            }

            let exited = match process.try_wait() {
                Ok(status) => status.is_some(),
                Err(error) => {
                    let _ = touch_marker(launch.cancel_path);
                    let (status, stderr) = process.finish()?;
                    return Err(WorkerRunnerError::Failed {
                        status: status.to_string(),
                        message: format!("{error}; {}", stderr_message(&stderr)),
                    });
                }
            };
            if exited {
                if monitor_error.is_none()
                    && let Err(error) = tail.finish(progress)
                {
                    monitor_error = Some(error);
                }
                let (status, stderr) = process.finish()?;
                if let Some(error) = monitor_error {
                    return Err(error);
                }
                if status.success() {
                    return Ok(());
                }
                return Err(WorkerRunnerError::Failed {
                    status: status.to_string(),
                    message: stderr_message(&stderr),
                });
            }

            thread::sleep(POLL_INTERVAL);
        }
    }
}

struct PinnedLaunchFiles {
    _executable: File,
    _job: File,
    identity: LaunchIdentity,
    job_sha256: String,
}

impl PinnedLaunchFiles {
    fn open(executable: &Path, job_path: &Path) -> Result<Self, WorkerRunnerError> {
        let executable = pin_and_hash(executable, None, true)?;
        let job = pin_and_hash(job_path, Some(MAX_WORKER_JOB_SIZE), false)?;
        Ok(Self {
            _executable: executable.file,
            _job: job.file,
            identity: LaunchIdentity {
                executable: executable.identity,
                job: job.identity,
            },
            job_sha256: job.sha256,
        })
    }
}

struct PinnedFile {
    file: File,
    identity: FileIdentity,
    sha256: String,
}

fn pin_and_hash(
    path: &Path,
    maximum_size: Option<u64>,
    require_executable: bool,
) -> Result<PinnedFile, WorkerRunnerError> {
    if !path.is_absolute() {
        return Err(WorkerRunnerError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "launch file path must be absolute",
        )));
    }
    let before = fs::symlink_metadata(path)?;
    validate_launch_file_metadata(&before, maximum_size, require_executable)?;
    let before_identity = FileIdentity::from_metadata(&before);

    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW)
        .open(path)?;
    let opened = file.metadata()?;
    validate_launch_file_metadata(&opened, maximum_size, require_executable)?;
    if FileIdentity::from_metadata(&opened) != before_identity {
        return Err(WorkerRunnerError::Io(io::Error::other(
            "launch file changed while it was opened",
        )));
    }

    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    let opened_after = file.metadata()?;
    let path_after = fs::symlink_metadata(path)?;
    validate_launch_file_metadata(&opened_after, maximum_size, require_executable)?;
    validate_launch_file_metadata(&path_after, maximum_size, require_executable)?;
    if FileIdentity::from_metadata(&opened_after) != before_identity
        || FileIdentity::from_metadata(&path_after) != before_identity
    {
        return Err(WorkerRunnerError::Io(io::Error::other(
            "launch file changed while it was hashed",
        )));
    }

    Ok(PinnedFile {
        file,
        identity: before_identity,
        sha256: hex::encode(hasher.finalize()),
    })
}

fn validate_launch_file_metadata(
    metadata: &fs::Metadata,
    maximum_size: Option<u64>,
    require_executable: bool,
) -> Result<(), WorkerRunnerError> {
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() == 0
        || maximum_size.is_some_and(|maximum| metadata.len() > maximum)
        || (require_executable && metadata.mode() & 0o111 == 0)
        || metadata.mode() & 0o022 != 0
    {
        return Err(WorkerRunnerError::Io(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "launch file has unsafe type, size, ownership mode, or executable mode",
        )));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LaunchIdentity {
    executable: FileIdentity,
    job: FileIdentity,
}

impl LaunchIdentity {
    fn ensure_unchanged(self, executable: &Path, job_path: &Path) -> Result<(), WorkerRunnerError> {
        self.executable.ensure_unchanged(executable)?;
        self.job.ensure_unchanged(job_path)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

impl FileIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        }
    }

    fn read(path: &Path) -> Result<Self, WorkerRunnerError> {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(WorkerRunnerError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "privileged executable must be a non-symlink regular file",
            )));
        }
        Ok(Self::from_metadata(&metadata))
    }

    fn ensure_unchanged(self, path: &Path) -> Result<(), WorkerRunnerError> {
        if Self::read(path)? == self {
            Ok(())
        } else {
            Err(WorkerRunnerError::Io(io::Error::other(
                "pinned launch file changed during worker launch",
            )))
        }
    }
}

fn validate_executable(path: &Path) -> Result<(), WorkerRunnerError> {
    if !path.is_absolute() {
        return Err(WorkerRunnerError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "worker executable path must be absolute",
        )));
    }
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() == 0
        || metadata.mode() & 0o111 == 0
        || metadata.mode() & 0o022 != 0
    {
        return Err(WorkerRunnerError::Io(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "worker executable must be a non-symlink executable that is not group- or world-writable",
        )));
    }
    Ok(())
}

fn stderr_message(stderr: &[u8]) -> String {
    let message = String::from_utf8_lossy(stderr).trim().to_owned();
    if message.is_empty() {
        "UDisks2 authorization was denied or the worker returned no details".to_owned()
    } else {
        message
    }
}

struct ElevatedProcess {
    child: Option<Child>,
    stderr_reader: Option<JoinHandle<io::Result<Vec<u8>>>>,
    _pins: PinnedLaunchFiles,
}

impl ElevatedProcess {
    fn spawn(
        executable: &Path,
        job_path: &Path,
        pins: PinnedLaunchFiles,
    ) -> Result<Self, WorkerRunnerError> {
        let mut child = Command::new(executable)
            .arg(WORKER_JOB_ARGUMENT)
            .arg(job_path)
            .arg(WORKER_JOB_SHA256_ARGUMENT)
            .arg(&pins.job_sha256)
            .env(RAW_DEVICE_OPT_IN, RAW_DEVICE_OPT_IN_VALUE)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        if let Err(error) = pins.identity.ensure_unchanged(executable, job_path) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        let Some(mut stderr) = child.stderr.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(WorkerRunnerError::Io(io::Error::other(
                "failed to capture worker stderr",
            )));
        };
        let stderr_reader = thread::spawn(move || {
            let mut bytes = Vec::new();
            stderr
                .by_ref()
                .take(MAX_HELPER_STDERR + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 > MAX_HELPER_STDERR {
                return Err(io::Error::other(
                    "authorization helper error output is too large",
                ));
            }
            Ok(bytes)
        });
        Ok(Self {
            child: Some(child),
            stderr_reader: Some(stderr_reader),
            _pins: pins,
        })
    }

    fn try_wait(&mut self) -> Result<Option<ExitStatus>, WorkerRunnerError> {
        self.child
            .as_mut()
            .ok_or_else(missing_child)?
            .try_wait()
            .map_err(WorkerRunnerError::Io)
    }

    fn request_stop(&mut self) -> Result<(), WorkerRunnerError> {
        match self.child.as_mut().ok_or_else(missing_child)?.kill() {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => Ok(()),
            Err(error) => Err(WorkerRunnerError::Io(error)),
        }
    }

    fn finish(mut self) -> Result<(ExitStatus, Vec<u8>), WorkerRunnerError> {
        let status = self.child.as_mut().ok_or_else(missing_child)?.wait()?;
        self.child = None;
        let stderr = self
            .stderr_reader
            .take()
            .ok_or_else(missing_stderr_reader)?
            .join()
            .map_err(|_| missing_stderr_reader())??;
        Ok((status, stderr))
    }
}

impl Drop for ElevatedProcess {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(reader) = self.stderr_reader.take() {
            let _ = reader.join();
        }
    }
}

fn missing_child() -> WorkerRunnerError {
    WorkerRunnerError::Io(io::Error::other("worker process handle is unavailable"))
}

fn missing_stderr_reader() -> WorkerRunnerError {
    WorkerRunnerError::Io(io::Error::other("worker stderr monitor is unavailable"))
}

struct ProgressTail {
    file: File,
    pending: Vec<u8>,
    saw_event: bool,
}

impl ProgressTail {
    fn open(path: &Path) -> Result<Self, WorkerRunnerError> {
        Ok(Self {
            file: File::open(path)?,
            pending: Vec::new(),
            saw_event: false,
        })
    }

    const fn saw_event(&self) -> bool {
        self.saw_event
    }

    fn drain(&mut self, progress: &mut dyn FnMut(WorkerProgress)) -> Result<(), WorkerRunnerError> {
        let mut chunk = Vec::new();
        self.file
            .by_ref()
            .take(MAX_PROGRESS_READ + 1)
            .read_to_end(&mut chunk)?;
        if chunk.len() as u64 > MAX_PROGRESS_READ {
            return Err(WorkerRunnerError::InvalidProgress(
                "worker progress grew too quickly".to_owned(),
            ));
        }
        self.pending.extend_from_slice(&chunk);
        if self.pending.len() > MAX_PROGRESS_RECORD && !self.pending.contains(&b'\n') {
            return Err(WorkerRunnerError::InvalidProgress(
                "worker progress record is too large".to_owned(),
            ));
        }
        let mut consumed = 0;
        for newline in self
            .pending
            .iter()
            .enumerate()
            .filter_map(|(index, byte)| (*byte == b'\n').then_some(index))
        {
            let line = &self.pending[consumed..newline];
            consumed = newline + 1;
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            if line.len() > MAX_PROGRESS_RECORD {
                return Err(WorkerRunnerError::InvalidProgress(
                    "worker progress record is too large".to_owned(),
                ));
            }
            let event: WorkerProgress = serde_json::from_slice(line)
                .map_err(|error| WorkerRunnerError::InvalidProgress(error.to_string()))?;
            validate_progress(&event)?;
            self.saw_event = true;
            progress(event);
        }
        self.pending.drain(..consumed);
        if self.pending.len() > MAX_PROGRESS_RECORD {
            return Err(WorkerRunnerError::InvalidProgress(
                "worker progress record is too large".to_owned(),
            ));
        }
        Ok(())
    }

    fn finish(
        &mut self,
        progress: &mut dyn FnMut(WorkerProgress),
    ) -> Result<(), WorkerRunnerError> {
        self.drain(progress)?;
        if self.pending.iter().all(u8::is_ascii_whitespace) {
            Ok(())
        } else {
            Err(WorkerRunnerError::InvalidProgress(
                "worker exited with an incomplete JSON-lines record".to_owned(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    use tempfile::{NamedTempFile, TempDir};

    use super::*;
    use crate::worker::{WORKER_PROGRESS_SCHEMA_VERSION, WorkerPhase};

    fn event(phase: WorkerPhase) -> WorkerProgress {
        WorkerProgress {
            schema_version: WORKER_PROGRESS_SCHEMA_VERSION,
            phase,
            bytes_processed: None,
            total_bytes: None,
            raw_sha256: None,
            verified: (phase == WorkerPhase::Completed).then_some(true),
            message: None,
        }
    }

    #[test]
    fn explicit_paths_are_retained_without_command_construction() {
        let runner = LinuxWorkerRunner::with_paths(
            PathBuf::from("/opt/snapdog/snapdog-os-installer"),
            PathBuf::from("/unused/compatibility-argument"),
        );
        assert_eq!(
            runner.executable,
            PathBuf::from("/opt/snapdog/snapdog-os-installer")
        );
    }

    #[test]
    fn executable_validation_rejects_relative_and_symlink_paths() {
        assert!(validate_executable(Path::new("relative-worker")).is_err());
        let directory = TempDir::new().unwrap();
        let executable = directory.path().join("worker");
        fs::write(&executable, b"binary").unwrap();
        let link = directory.path().join("worker-link");
        std::os::unix::fs::symlink(&executable, &link).unwrap();
        assert!(validate_executable(&link).is_err());
    }

    #[test]
    fn file_identity_detects_replacement() {
        let directory = TempDir::new().unwrap();
        let executable = directory.path().join("worker");
        fs::write(&executable, b"first").unwrap();
        let identity = FileIdentity::read(&executable).unwrap();
        fs::remove_file(&executable).unwrap();
        fs::write(&executable, b"second").unwrap();
        assert!(identity.ensure_unchanged(&executable).is_err());
    }

    #[test]
    fn launch_files_are_hashed_from_pinned_descriptors() {
        let directory = TempDir::new().unwrap();
        let executable = directory.path().join("installer");
        let job = directory.path().join("worker-job.json");
        fs::write(&executable, b"trusted executable").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(&job, b"{\"schema_version\":1}").unwrap();
        fs::set_permissions(&job, fs::Permissions::from_mode(0o600)).unwrap();

        let pinned = PinnedLaunchFiles::open(&executable, &job).unwrap();
        assert_eq!(
            pinned.job_sha256,
            hex::encode(Sha256::digest(b"{\"schema_version\":1}"))
        );

        fs::write(&executable, b"replacement").unwrap();
        assert!(pinned.identity.ensure_unchanged(&executable, &job).is_err());
    }

    #[test]
    fn progress_tail_emits_complete_json_lines() {
        let mut file = NamedTempFile::new().unwrap();
        serde_json::to_writer(&mut file, &event(WorkerPhase::Writing)).unwrap();
        file.write_all(b"\n").unwrap();
        serde_json::to_writer(&mut file, &event(WorkerPhase::Completed)).unwrap();
        file.write_all(b"\n").unwrap();
        file.flush().unwrap();

        let mut phases = Vec::new();
        let mut tail = ProgressTail::open(file.path()).unwrap();
        tail.finish(&mut |progress| phases.push(progress.phase))
            .unwrap();

        assert_eq!(phases, [WorkerPhase::Writing, WorkerPhase::Completed]);
    }

    #[test]
    fn progress_tail_rejects_schema_mismatch_and_incomplete_record() {
        let mut invalid_schema = NamedTempFile::new().unwrap();
        let mut progress = event(WorkerPhase::Writing);
        progress.schema_version += 1;
        serde_json::to_writer(&mut invalid_schema, &progress).unwrap();
        invalid_schema.write_all(b"\n").unwrap();
        invalid_schema.flush().unwrap();
        let mut tail = ProgressTail::open(invalid_schema.path()).unwrap();
        assert!(matches!(
            tail.finish(&mut |_| {}),
            Err(WorkerRunnerError::InvalidProgress(_))
        ));

        let mut incomplete = NamedTempFile::new().unwrap();
        serde_json::to_writer(&mut incomplete, &event(WorkerPhase::Writing)).unwrap();
        incomplete.flush().unwrap();
        let mut tail = ProgressTail::open(incomplete.path()).unwrap();
        assert!(matches!(
            tail.finish(&mut |_| {}),
            Err(WorkerRunnerError::InvalidProgress(_))
        ));
    }
}
