// SPDX-License-Identifier: GPL-3.0-only

//! macOS authorization and progress monitoring for the isolated worker.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
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
use unix_ancillary::UnixStreamExt;

const POLL_INTERVAL: Duration = Duration::from_millis(40);
const MAX_PROGRESS_READ: u64 = 1024 * 1024;
const MAX_PROGRESS_RECORD: usize = 64 * 1024;
const MAX_HELPER_STDERR: u64 = 256 * 1024;
const MAX_WORKER_JOB_SIZE: u64 = 64 * 1024;
#[cfg(not(debug_assertions))]
const WORKER_REQUIREMENT: &str = r#"identifier "cc.snapdog.os-installer" and anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] exists and certificate leaf[field.1.2.840.113635.100.6.1.13] exists and certificate leaf[subject.OU] = "898G35U5LW""#;
const WORKER_RELATIVE_PATH: &str = "Contents/MacOS/SnapDog OS Installer";
const RAW_DEVICE_OPT_IN: &str = "SNAPDOG_INSTALLER_ALLOW_RAW_DEVICE_WRITE";
const RAW_DEVICE_OPT_IN_VALUE: &str = "YES-I-UNDERSTAND";
const AUTHOPEN: &str = "/usr/libexec/authopen";
// Darwin O_RDWR | O_SYNC. Raw disks reject File::sync_all/F_FULLFSYNC with ENOTTY, so the
// descriptor itself must provide synchronous file-integrity writes.
const AUTHOPEN_RAW_FLAGS: &str = "130";
const TARGET_FD_REQUEST: &[u8] = b"SNAPDOG_TARGET_FD_REQUEST_V1\n";
const TARGET_FD_RESPONSE: &[u8] = b"SNAPDOG_TARGET_FD_RESPONSE_V1\n";

/// Launches the current application binary as an isolated, unprivileged worker.
#[derive(Clone, Debug)]
pub struct MacOsWorkerRunner {
    bundle: PathBuf,
}

impl MacOsWorkerRunner {
    /// Use the currently running executable for the isolated worker re-entry.
    pub fn current() -> Result<Self, WorkerRunnerError> {
        let executable = std::env::current_exe()?;
        let bundle = bundle_for_executable(&executable)?;
        #[cfg(not(debug_assertions))]
        verify_signed_bundle(&bundle)?;
        #[cfg(debug_assertions)]
        tracing::warn!(
            bundle = %bundle.display(),
            "DEBUG BUILD: signed bundle verification is disabled"
        );
        Ok(Self { bundle })
    }

    /// Use an explicit signed application bundle.
    ///
    /// This is primarily useful for integration tests and packaged-app launch plumbing.
    pub const fn with_bundle(bundle: PathBuf) -> Self {
        Self { bundle }
    }

    /// Ask macOS for scoped removable-volume access before launching the worker.
    ///
    /// TCC attributes access to the responsible GUI process. A non-destructive `open(2)` attempt
    /// from that process triggers the native Removable Volumes prompt; the subsequently spawned
    /// worker then inherits the decision. The open itself is expected to fail for an unprivileged
    /// process, and the descriptor is immediately closed if it happens to succeed. Authorization
    /// to write is granted later for the exact raw device through `authopen`.
    pub fn prime_removable_volume_access(&self, device: &str) -> Result<(), WorkerRunnerError> {
        let raw_path = raw_device_path(device)?;
        match OpenOptions::new().read(true).write(true).open(&raw_path) {
            Ok(file) => {
                tracing::info!(
                    raw_path = %raw_path.display(),
                    "primed macOS removable-volume access"
                );
                drop(file);
            }
            Err(error) => {
                // EPERM/EACCES is the normal result after TCC has presented its prompt because the
                // GUI remains unprivileged. EBUSY is also normal while volumes are still mounted.
                // `authopen` performs the authoritative open after unmounting.
                tracing::info!(
                    raw_path = %raw_path.display(),
                    error = %error,
                    "macOS removable-volume access primer completed"
                );
            }
        }
        Ok(())
    }
}

fn raw_device_path(device: &str) -> Result<PathBuf, WorkerRunnerError> {
    let Some(identifier) = device.strip_prefix("/dev/disk") else {
        return Err(WorkerRunnerError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "macOS target must be a whole-disk device",
        )));
    };
    if identifier.is_empty() || !identifier.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(WorkerRunnerError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "macOS target must be a whole-disk device",
        )));
    }
    Ok(PathBuf::from(format!("/dev/rdisk{identifier}")))
}

impl WorkerRunner for MacOsWorkerRunner {
    fn run(
        &self,
        launch: WorkerLaunch<'_>,
        progress: &mut dyn FnMut(WorkerProgress),
    ) -> Result<(), WorkerRunnerError> {
        let executable = self.bundle.join(WORKER_RELATIVE_PATH);
        let pinned = PinnedLaunchFiles::open(&executable, launch.job_path)?;
        let launch_identity = pinned.identity;
        let mut tail = ProgressTail::open(launch.progress_path)?;
        let mut target_listener =
            AuthorizedTargetListener::bind(launch.macos_target_socket.ok_or_else(|| {
                WorkerRunnerError::Io(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "macOS authorized target socket is missing",
                ))
            })?)?;
        let mut process = WorkerProcess::spawn(&executable, launch.job_path, pinned)?;
        let mut monitor_error = None;
        let mut cancellation_sent = false;

        loop {
            if monitor_error.is_none()
                && !tail.saw_event()
                && let Err(error) = launch_identity.ensure_unchanged(&executable, launch.job_path)
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
                // If startup raced, the already-created marker makes the worker stop at its first
                // safe boundary while this runner still waits for process completion.
                if !tail.saw_event()
                    && let Err(error) = process.request_stop()
                    && monitor_error.is_none()
                {
                    monitor_error = Some(error);
                }
                cancellation_sent = true;
            }

            if monitor_error.is_none()
                && let Err(error) = target_listener.try_serve(launch.target_device)
            {
                let _ = touch_marker(launch.cancel_path);
                let _ = process.request_stop();
                monitor_error = Some(error);
            }

            let exited = match process.try_wait() {
                Ok(status) => status.is_some(),
                Err(error) => {
                    let _ = touch_marker(launch.cancel_path);
                    let (status, stderr) = process.finish()?;
                    log_helper_stderr(&stderr);
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
                log_helper_stderr(&stderr);
                if let Some(error) = monitor_error {
                    return Err(error);
                }
                if status.success() {
                    return Ok(());
                }
                let message = stderr_message(&stderr);
                return Err(WorkerRunnerError::Failed {
                    status: status.to_string(),
                    message,
                });
            }

            thread::sleep(POLL_INTERVAL);
        }
    }
}

struct AuthorizedTargetListener {
    listener: UnixListener,
    path: PathBuf,
    served: bool,
}

impl AuthorizedTargetListener {
    fn bind(path: &Path) -> Result<Self, WorkerRunnerError> {
        if !path.is_absolute() || path.exists() {
            return Err(WorkerRunnerError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "macOS authorized target socket path is invalid",
            )));
        }
        let listener = UnixListener::bind(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            path: path.to_path_buf(),
            served: false,
        })
    }

    fn try_serve(&mut self, device: &str) -> Result<(), WorkerRunnerError> {
        if self.served {
            return Ok(());
        }
        let (mut stream, _) = match self.listener.accept() {
            Ok(connection) => connection,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        stream.set_nonblocking(false)?;
        let mut request = [0_u8; TARGET_FD_REQUEST.len()];
        stream.read_exact(&mut request)?;
        if request != TARGET_FD_REQUEST {
            return Err(WorkerRunnerError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "worker sent an invalid target descriptor request",
            )));
        }

        let target = authorized_raw_target(device)?;
        let sent = stream.send_fds(TARGET_FD_RESPONSE, &[&target])?;
        if sent != TARGET_FD_RESPONSE.len() {
            return Err(WorkerRunnerError::Io(io::Error::new(
                io::ErrorKind::WriteZero,
                "authorized target descriptor response was incomplete",
            )));
        }
        stream.flush()?;
        self.served = true;
        tracing::info!(device, "passed authopen target descriptor to macOS worker");
        Ok(())
    }
}

impl Drop for AuthorizedTargetListener {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn authorized_raw_target(device: &str) -> Result<File, WorkerRunnerError> {
    let raw_path = raw_device_path(device)?;
    let (auth_socket, child_socket) = UnixStream::pair()?;
    let child_socket_fd = OwnedFd::from(child_socket);
    tracing::info!(raw_path = %raw_path.display(), "requesting authorized raw target descriptor");
    let child = Command::new(AUTHOPEN)
        .arg("-stdoutpipe")
        .arg("-o")
        .arg(AUTHOPEN_RAW_FLAGS)
        .arg(&raw_path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(child_socket_fd))
        .stderr(Stdio::piped())
        .spawn()?;
    let descriptor_result = auth_socket.recv_fds::<1>();
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(WorkerRunnerError::Failed {
            status: output.status.to_string(),
            message: stderr_message(&output.stderr),
        });
    }
    let mut descriptor_message = descriptor_result?;
    if descriptor_message.data.is_empty() || descriptor_message.fds.len() != 1 {
        return Err(WorkerRunnerError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "authopen did not return exactly one target descriptor",
        )));
    }
    Ok(File::from(descriptor_message.fds.remove(0)))
}

fn stderr_message(stderr: &[u8]) -> String {
    let message = String::from_utf8_lossy(stderr).trim().to_owned();
    if message.is_empty() {
        "the macOS authorization helper returned no error details".to_owned()
    } else {
        message
    }
}

fn log_helper_stderr(stderr: &[u8]) {
    for line in String::from_utf8_lossy(stderr).lines() {
        if !line.trim().is_empty() {
            tracing::debug!(worker = line, "macOS worker diagnostic");
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

    let mut file = File::open(path)?;
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
        || require_executable && metadata.mode() & 0o111 == 0
        || metadata.mode() & 0o022 != 0
    {
        return Err(WorkerRunnerError::Io(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "launch file has unsafe type, size, or mode",
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
                "launch file must remain a non-symlink regular file",
            )));
        }
        Ok(Self::from_metadata(&metadata))
    }

    fn ensure_unchanged(self, path: &Path) -> Result<(), WorkerRunnerError> {
        if Self::read(path)? == self {
            Ok(())
        } else {
            Err(WorkerRunnerError::Io(io::Error::other(
                "launch file changed during worker startup",
            )))
        }
    }
}

struct WorkerProcess {
    child: Option<Child>,
    stderr_reader: Option<JoinHandle<io::Result<Vec<u8>>>>,
    _pins: PinnedLaunchFiles,
}

impl WorkerProcess {
    fn spawn(
        executable: &Path,
        job_path: &Path,
        pins: PinnedLaunchFiles,
    ) -> Result<Self, WorkerRunnerError> {
        tracing::info!(
            executable = %executable.display(),
            job_path = %job_path.display(),
            debug_unsigned = cfg!(debug_assertions),
            "launching isolated macOS worker"
        );
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

impl Drop for WorkerProcess {
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
        Read::by_ref(&mut self.file)
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

fn bundle_for_executable(executable: &Path) -> Result<PathBuf, WorkerRunnerError> {
    let macos = executable.parent().ok_or_else(|| {
        WorkerRunnerError::Io(io::Error::other("application executable has no parent"))
    })?;
    let contents = macos.parent().ok_or_else(|| {
        WorkerRunnerError::Io(io::Error::other(
            "application executable is not in a bundle",
        ))
    })?;
    let bundle = contents.parent().ok_or_else(|| {
        WorkerRunnerError::Io(io::Error::other(
            "application executable is not in a bundle",
        ))
    })?;
    if macos.file_name().is_none_or(|name| name != "MacOS")
        || bundle
            .extension()
            .is_none_or(|extension| extension != "app")
        || !bundle.join(WORKER_RELATIVE_PATH).is_file()
    {
        return Err(WorkerRunnerError::Io(io::Error::other(
            "raw-device writing requires the signed SnapDog application bundle",
        )));
    }
    Ok(bundle.to_path_buf())
}

#[cfg(not(debug_assertions))]
fn verify_signed_bundle(bundle: &Path) -> Result<(), WorkerRunnerError> {
    let output = Command::new("/usr/bin/codesign")
        .arg("--verify")
        .arg("--deep")
        .arg("--strict")
        .arg(format!("-R={WORKER_REQUIREMENT}"))
        .arg(bundle)
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(WorkerRunnerError::Failed {
            status: output.status.to_string(),
            message: stderr_message(&output.stderr),
        })
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
    fn removable_volume_primer_accepts_only_whole_disk_devices() {
        assert_eq!(
            raw_device_path("/dev/disk21").unwrap(),
            PathBuf::from("/dev/rdisk21")
        );
        for invalid in [
            "/dev/disk21s1",
            "/dev/rdisk21",
            "/tmp/disk21",
            "/dev/disk21;touch /tmp/injected",
        ] {
            assert!(raw_device_path(invalid).is_err());
        }
    }

    #[test]
    fn direct_worker_contract_uses_an_explicit_opt_in() {
        assert_eq!(
            RAW_DEVICE_OPT_IN,
            "SNAPDOG_INSTALLER_ALLOW_RAW_DEVICE_WRITE"
        );
        assert_eq!(RAW_DEVICE_OPT_IN_VALUE, "YES-I-UNDERSTAND");
        assert_eq!(WORKER_RELATIVE_PATH, "Contents/MacOS/SnapDog OS Installer");
        assert_eq!(AUTHOPEN_RAW_FLAGS, "130");
    }

    #[test]
    fn pinned_launch_file_is_hashed_and_detects_mutation() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(b"pinned SnapDog job").unwrap();
        file.flush().unwrap();
        let pinned = pin_and_hash(file.path(), Some(MAX_WORKER_JOB_SIZE), false).unwrap();

        assert_eq!(
            pinned.sha256,
            hex::encode(Sha256::digest(b"pinned SnapDog job"))
        );
        fs::write(file.path(), b"replaced SnapDog job").unwrap();
        assert!(pinned.identity.ensure_unchanged(file.path()).is_err());
    }

    #[test]
    fn codesign_requirement_flag_rejects_a_false_designated_requirement() {
        let status = Command::new("/usr/bin/codesign")
            .args([
                "--verify",
                "--strict",
                r#"-R=identifier "cc.snapdog.this-is-deliberately-false""#,
                "/bin/ls",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();

        assert!(!status.success());
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
    fn progress_tail_rejects_schema_mismatch() {
        let mut file = NamedTempFile::new().unwrap();
        let mut progress = event(WorkerPhase::Writing);
        progress.schema_version += 1;
        serde_json::to_writer(&mut file, &progress).unwrap();
        file.write_all(b"\n").unwrap();
        file.flush().unwrap();

        let mut tail = ProgressTail::open(file.path()).unwrap();
        let error = tail.finish(&mut |_| {}).unwrap_err();

        assert!(matches!(error, WorkerRunnerError::InvalidProgress(_)));
    }

    #[test]
    fn progress_tail_rejects_incomplete_final_record() {
        let mut file = NamedTempFile::new().unwrap();
        serde_json::to_writer(&mut file, &event(WorkerPhase::Writing)).unwrap();
        file.flush().unwrap();

        let mut tail = ProgressTail::open(file.path()).unwrap();
        let error = tail.finish(&mut |_| {}).unwrap_err();

        assert!(matches!(error, WorkerRunnerError::InvalidProgress(_)));
    }
}
