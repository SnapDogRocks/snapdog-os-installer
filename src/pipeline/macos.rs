// SPDX-License-Identifier: GPL-3.0-only

//! macOS elevation and progress monitoring for the privileged worker.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::Ordering;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::{
    WORKER_JOB_ARGUMENT, WorkerLaunch, WorkerRunner, WorkerRunnerError, touch_marker,
    validate_progress,
};
use crate::worker::WorkerProgress;

const POLL_INTERVAL: Duration = Duration::from_millis(40);
const MAX_PROGRESS_READ: u64 = 1024 * 1024;
const MAX_PROGRESS_RECORD: usize = 64 * 1024;
const MAX_HELPER_STDERR: u64 = 256 * 1024;
const WORKER_REQUIREMENT: &str = r#"identifier "cc.snapdog.os-installer" and anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] exists and certificate leaf[field.1.2.840.113635.100.6.1.13] exists and certificate leaf[subject.OU] = "898G35U5LW""#;
const WORKER_RELATIVE_PATH: &str = "Contents/MacOS/snapdog-os-installer";
const ELEVATED_SHELL_PROGRAM: &str = r#"set -eu
source_bundle=$1
worker_argument=$2
job_path=$3
requirement=$4
worker_root=$(/usr/bin/mktemp -d /private/tmp/snapdog-installer-worker.XXXXXX)
trap '/bin/rm -rf "$worker_root"' EXIT HUP INT TERM
worker_bundle="$worker_root/worker.app"
/usr/bin/ditto --noqtn "$source_bundle" "$worker_bundle"
/usr/sbin/chown -R root:wheel "$worker_bundle"
/usr/bin/codesign --verify --deep --strict --requirement "$requirement" "$worker_bundle"
/usr/bin/env SNAPDOG_INSTALLER_ALLOW_RAW_DEVICE_WRITE=YES-I-UNDERSTAND "$worker_bundle/Contents/MacOS/snapdog-os-installer" "$worker_argument" "$job_path"
"#;
// Values originating outside this source file are passed in argv and quoted by AppleScript.
// Never interpolate a path into this program text.
const ELEVATION_SCRIPT: &str = r#"
on run argv
    if (count of argv) is not 5 then error "Invalid worker launch arguments"
    set bundlePath to item 1 of argv
    set workerArgument to item 2 of argv
    set jobPath to item 3 of argv
    set shellProgram to item 4 of argv
    set requirement to item 5 of argv
    set shellCommand to "/bin/sh -c " & quoted form of shellProgram & " -- " & quoted form of bundlePath & " " & quoted form of workerArgument & " " & quoted form of jobPath & " " & quoted form of requirement
    do shell script shellCommand with administrator privileges
end run
"#;

/// Launches the current application binary as a root worker through the native macOS prompt.
#[derive(Clone, Debug)]
pub struct MacOsWorkerRunner {
    bundle: PathBuf,
}

impl MacOsWorkerRunner {
    /// Use the currently running executable for the privileged worker re-entry.
    pub fn current() -> Result<Self, WorkerRunnerError> {
        let executable = std::env::current_exe()?;
        let bundle = bundle_for_executable(&executable)?;
        verify_signed_bundle(&bundle)?;
        Ok(Self { bundle })
    }

    /// Use an explicit signed application bundle.
    ///
    /// This is primarily useful for integration tests and packaged-app launch plumbing.
    pub const fn with_bundle(bundle: PathBuf) -> Self {
        Self { bundle }
    }
}

impl WorkerRunner for MacOsWorkerRunner {
    fn run(
        &self,
        launch: WorkerLaunch<'_>,
        progress: &mut dyn FnMut(WorkerProgress),
    ) -> Result<(), WorkerRunnerError> {
        let mut tail = ProgressTail::open(launch.progress_path)?;
        let mut process = ElevatedProcess::spawn(&self.bundle, launch.job_path)?;
        let mut monitor_error = None;
        let mut cancellation_sent = false;

        loop {
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
                // Before the worker emits its first event, the only process we can be waiting on
                // is normally the macOS authorization dialog. Killing osascript dismisses that
                // dialog. If startup raced, the already-created marker makes the worker stop at
                // its first safe boundary while this runner still waits for process completion.
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

fn stderr_message(stderr: &[u8]) -> String {
    let message = String::from_utf8_lossy(stderr).trim().to_owned();
    if message.is_empty() {
        "the macOS authorization helper returned no error details".to_owned()
    } else {
        message
    }
}

struct ElevatedProcess {
    child: Option<Child>,
    stderr_reader: Option<JoinHandle<io::Result<Vec<u8>>>>,
}

impl ElevatedProcess {
    fn spawn(bundle: &Path, job_path: &Path) -> Result<Self, WorkerRunnerError> {
        let mut child = Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(ELEVATION_SCRIPT)
            .arg("--")
            .arg(bundle)
            .arg(WORKER_JOB_ARGUMENT)
            .arg(job_path)
            .arg(ELEVATED_SHELL_PROGRAM)
            .arg(WORKER_REQUIREMENT)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stderr = child.stderr.take().ok_or_else(|| {
            WorkerRunnerError::Io(io::Error::other("failed to capture osascript stderr"))
        })?;
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

fn verify_signed_bundle(bundle: &Path) -> Result<(), WorkerRunnerError> {
    let output = Command::new("/usr/bin/codesign")
        .arg("--verify")
        .arg("--deep")
        .arg("--strict")
        .arg("--requirement")
        .arg(WORKER_REQUIREMENT)
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

    use tempfile::{NamedTempFile, tempdir};

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
    fn elevation_script_quotes_every_dynamic_argument() {
        assert!(ELEVATION_SCRIPT.contains("on run argv"));
        assert!(ELEVATION_SCRIPT.contains("quoted form of bundlePath"));
        assert!(ELEVATION_SCRIPT.contains("quoted form of workerArgument"));
        assert!(ELEVATION_SCRIPT.contains("quoted form of jobPath"));
        assert!(ELEVATION_SCRIPT.contains("quoted form of shellProgram"));
        assert!(ELEVATION_SCRIPT.contains("quoted form of requirement"));
        assert!(ELEVATION_SCRIPT.contains("with administrator privileges"));
        assert!(
            ELEVATED_SHELL_PROGRAM
                .contains("SNAPDOG_INSTALLER_ALLOW_RAW_DEVICE_WRITE=YES-I-UNDERSTAND")
        );
        assert!(ELEVATED_SHELL_PROGRAM.contains("codesign --verify --deep --strict"));
        assert!(!ELEVATION_SCRIPT.contains("$(touch /tmp/injected)"));
    }

    #[test]
    fn elevation_programs_parse_without_execution() {
        assert!(
            Command::new("/bin/sh")
                .args(["-n", "-c", ELEVATED_SHELL_PROGRAM])
                .status()
                .unwrap()
                .success()
        );
        let directory = tempdir().unwrap();
        let compiled = directory.path().join("elevation.scpt");
        let output = Command::new("/usr/bin/osacompile")
            .arg("-e")
            .arg(ELEVATION_SCRIPT)
            .arg("-o")
            .arg(compiled)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
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
