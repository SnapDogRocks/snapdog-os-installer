// SPDX-License-Identifier: GPL-3.0-only

//! Native Windows UAC elevation and progress monitoring for the privileged worker.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use super::{
    WORKER_JOB_ARGUMENT, WorkerLaunch, WorkerRunner, WorkerRunnerError, touch_marker,
    validate_progress,
};
use crate::worker::WorkerProgress;
use crate::worker::{
    WINDOWS_JOB_SHA256_ARGUMENT, WINDOWS_RAW_DEVICE_OPT_IN_ARGUMENT,
    WINDOWS_RAW_DEVICE_OPT_IN_VALUE, WorkerJob,
};
use sha2::{Digest, Sha256};

const POLL_INTERVAL: Duration = Duration::from_millis(40);
const MAX_PROGRESS_READ: u64 = 1024 * 1024;
const MAX_PROGRESS_RECORD: usize = 64 * 1024;
const MAX_WORKER_JOB_SIZE: u64 = 64 * 1024;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const FILE_SHARE_READ: u32 = 0x1;

/// Launches this executable as an administrator through the Windows UAC prompt.
#[derive(Clone, Debug)]
pub struct WindowsWorkerRunner {
    executable: PathBuf,
}

impl WindowsWorkerRunner {
    /// Use the currently running executable for elevated worker re-entry.
    pub fn current() -> Result<Self, WorkerRunnerError> {
        let executable = std::env::current_exe()?;
        if !executable.is_absolute()
            || !executable.is_file()
            || executable
                .extension()
                .is_none_or(|extension| !extension.eq_ignore_ascii_case("exe"))
        {
            return Err(WorkerRunnerError::Io(io::Error::other(
                "raw-device writing requires the packaged SnapDog Windows executable",
            )));
        }
        Ok(Self { executable })
    }

    /// Use an explicit executable path, primarily for packaged integration tests.
    pub const fn with_executable(executable: PathBuf) -> Self {
        Self { executable }
    }
}

impl WorkerRunner for WindowsWorkerRunner {
    fn run(
        &self,
        launch: WorkerLaunch<'_>,
        progress: &mut dyn FnMut(WorkerProgress),
    ) -> Result<(), WorkerRunnerError> {
        let mut tail = ProgressTail::open(launch.progress_path)?;
        let mut process = ElevatedProcess::spawn(&self.executable, launch.job_path)?;
        let mut monitor_error = None;
        let mut cancellation_sent = false;

        loop {
            if monitor_error.is_none()
                && let Err(error) = tail.drain(progress)
            {
                let _ = touch_marker(launch.cancel_path);
                monitor_error = Some(error);
            }

            if launch.cancelled.load(Ordering::Acquire) && !cancellation_sent {
                if let Err(error) = touch_marker(launch.cancel_path)
                    && monitor_error.is_none()
                {
                    monitor_error = Some(WorkerRunnerError::Io(error));
                }
                // Never terminate the elevated process: it is outside this process' security
                // token. The durable marker makes an accepted worker stop at its first safe
                // boundary; a pending UAC prompt must be accepted or denied by the user.
                cancellation_sent = true;
            }

            let exited = match process.try_wait() {
                Ok(status) => status.is_some(),
                Err(error) => {
                    let _ = touch_marker(launch.cancel_path);
                    let status = process.finish()?;
                    return Err(WorkerRunnerError::Failed {
                        status: format!("exit code {status}"),
                        message: error.to_string(),
                    });
                }
            };
            if exited {
                if monitor_error.is_none()
                    && let Err(error) = tail.finish(progress)
                {
                    monitor_error = Some(error);
                }
                let status = process.finish()?;
                if let Some(error) = monitor_error {
                    return Err(error);
                }
                if status == 0 {
                    return Ok(());
                }
                return Err(WorkerRunnerError::Failed {
                    status: format!("exit code {status}"),
                    message: "the native elevated worker reported a failure".to_owned(),
                });
            }

            thread::sleep(POLL_INTERVAL);
        }
    }
}

struct ElevatedProcess {
    child: Option<crate::windows_native::ElevatedChild>,
    _pins: PinnedLaunchFiles,
}

impl ElevatedProcess {
    fn spawn(executable: &Path, job_path: &Path) -> Result<Self, WorkerRunnerError> {
        let pins = PinnedLaunchFiles::open(executable, job_path)?;
        let elevated_arguments = native_command_line(&[
            OsStr::new(WORKER_JOB_ARGUMENT),
            job_path.as_os_str(),
            OsStr::new(WINDOWS_JOB_SHA256_ARGUMENT),
            OsStr::new(&pins.job_sha256),
            OsStr::new(WINDOWS_RAW_DEVICE_OPT_IN_ARGUMENT),
            OsStr::new(WINDOWS_RAW_DEVICE_OPT_IN_VALUE),
        ]);
        let child = crate::windows_native::launch_elevated(executable, &elevated_arguments)?;
        Ok(Self {
            child: Some(child),
            _pins: pins,
        })
    }

    fn try_wait(&mut self) -> Result<Option<u32>, WorkerRunnerError> {
        self.child
            .as_mut()
            .ok_or_else(missing_child)?
            .try_wait()
            .map_err(WorkerRunnerError::Io)
    }

    fn finish(mut self) -> Result<u32, WorkerRunnerError> {
        let status = self.child.as_ref().ok_or_else(missing_child)?.wait()?;
        self.child = None;
        Ok(status)
    }
}

struct PinnedLaunchFiles {
    _executable: File,
    _job: File,
    _raw_image: File,
    job_sha256: String,
}

impl PinnedLaunchFiles {
    fn open(executable: &Path, job_path: &Path) -> Result<Self, WorkerRunnerError> {
        let executable_file = pin_regular_file(executable, None)?;
        let mut job_file = pin_regular_file(job_path, None)?;
        let job_size = job_file.metadata()?.len();
        if job_size == 0 || job_size > MAX_WORKER_JOB_SIZE {
            return Err(WorkerRunnerError::Io(io::Error::other(
                "worker job has an unsafe size",
            )));
        }
        let capacity =
            usize::try_from(job_size).map_err(|_| io::Error::other("worker job is too large"))?;
        let mut encoded = Vec::with_capacity(capacity);
        job_file
            .by_ref()
            .take(MAX_WORKER_JOB_SIZE + 1)
            .read_to_end(&mut encoded)?;
        if encoded.len() as u64 != job_size {
            return Err(WorkerRunnerError::Io(io::Error::other(
                "worker job changed while it was pinned",
            )));
        }
        let job: WorkerJob = serde_json::from_slice(&encoded).map_err(|error| {
            WorkerRunnerError::Io(io::Error::new(io::ErrorKind::InvalidData, error))
        })?;
        let raw_image = pin_regular_file(&job.raw_path, Some(job.raw_size))?;
        let job_sha256 = hex::encode(Sha256::digest(&encoded));
        Ok(Self {
            _executable: executable_file,
            _job: job_file,
            _raw_image: raw_image,
            job_sha256,
        })
    }
}

fn pin_regular_file(path: &Path, expected_size: Option<u64>) -> io::Result<File> {
    let before = fs::symlink_metadata(path)?;
    if !before.is_file()
        || before.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || expected_size.is_some_and(|size| before.len() != size)
    {
        return Err(io::Error::other("refusing to pin an unsafe launch file"));
    }
    let file = OpenOptions::new()
        .read(true)
        // Keep read sharing so the Windows image loader and elevated worker can open the file,
        // but deny new write and delete handles for the full UAC hand-off.
        .share_mode(FILE_SHARE_READ)
        .open(path)?;
    let opened = file.metadata()?;
    let path_handle = same_file::Handle::from_path(path)?;
    let file_handle = same_file::Handle::from_file(file.try_clone()?)?;
    if path_handle != file_handle
        || !opened.is_file()
        || opened.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || expected_size.is_some_and(|size| opened.len() != size)
    {
        return Err(io::Error::other("launch file changed while it was pinned"));
    }
    Ok(file)
}

impl Drop for ElevatedProcess {
    fn drop(&mut self) {
        drop(self.child.take());
    }
}

fn missing_child() -> WorkerRunnerError {
    WorkerRunnerError::Io(io::Error::other(
        "authorization helper process handle is unavailable",
    ))
}

fn native_command_line(arguments: &[&OsStr]) -> OsString {
    let mut encoded = Vec::new();
    for (index, argument) in arguments.iter().enumerate() {
        if index > 0 {
            encoded.push(u16::from(b' '));
        }
        encoded.extend(quote_native_argument(argument).encode_wide());
    }
    OsString::from_wide(&encoded)
}

/// Inverse of Windows' `CommandLineToArgvW` rules for one arbitrary native argument.
fn quote_native_argument(value: &OsStr) -> OsString {
    let mut quoted = Vec::new();
    quoted.push(u16::from(b'"'));
    let mut backslashes = 0_usize;
    for character in value.encode_wide() {
        if character == u16::from(b'\\') {
            backslashes += 1;
            continue;
        }
        if character == u16::from(b'"') {
            quoted.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes * 2 + 1));
        } else {
            quoted.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes));
        }
        quoted.push(character);
        backslashes = 0;
    }
    quoted.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes * 2));
    quoted.push(u16::from(b'"'));
    OsString::from_wide(&quoted)
}

struct ProgressTail {
    file: File,
    pending: Vec<u8>,
}

impl ProgressTail {
    fn open(path: &Path) -> Result<Self, WorkerRunnerError> {
        Ok(Self {
            file: File::open(path)?,
            pending: Vec::new(),
        })
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

    use tempfile::NamedTempFile;

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
    fn native_argument_quoting_handles_spaces_quotes_and_trailing_slashes() {
        assert_eq!(
            quote_native_argument(OsStr::new(r"C:\safe path\job.json")),
            OsString::from(r#""C:\safe path\job.json""#)
        );
        assert_eq!(
            quote_native_argument(OsStr::new("a\\\"b\\")),
            OsString::from("\"a\\\\\\\"b\\\\\"")
        );
        assert_eq!(
            native_command_line(&[
                OsStr::new(WORKER_JOB_ARGUMENT),
                OsStr::new(r"C:\safe path\job.json")
            ]),
            OsString::from(r#""--worker-job" "C:\safe path\job.json""#)
        );
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
    fn progress_tail_rejects_schema_mismatch_and_partial_records() {
        let mut invalid = NamedTempFile::new().unwrap();
        let mut progress = event(WorkerPhase::Writing);
        progress.schema_version += 1;
        serde_json::to_writer(&mut invalid, &progress).unwrap();
        invalid.write_all(b"\n").unwrap();
        invalid.flush().unwrap();
        assert!(matches!(
            ProgressTail::open(invalid.path())
                .unwrap()
                .finish(&mut |_| {}),
            Err(WorkerRunnerError::InvalidProgress(_))
        ));

        let mut partial = NamedTempFile::new().unwrap();
        serde_json::to_writer(&mut partial, &event(WorkerPhase::Writing)).unwrap();
        partial.flush().unwrap();
        assert!(matches!(
            ProgressTail::open(partial.path())
                .unwrap()
                .finish(&mut |_| {}),
            Err(WorkerRunnerError::InvalidProgress(_))
        ));
    }
}
