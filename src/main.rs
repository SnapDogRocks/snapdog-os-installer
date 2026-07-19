#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

// SPDX-License-Identifier: GPL-3.0-only

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
#[cfg(all(debug_assertions, target_os = "macos"))]
use std::sync::Mutex;

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use std::fs::{self, File};
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use std::io::Read;

use eframe::egui;
use snapdog_os_installer::SnapDogInstallerApp;
use snapdog_os_installer::pipeline::WORKER_JOB_ARGUMENT;

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use sha2::{Digest, Sha256};
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use snapdog_os_installer::worker::WORKER_JOB_SHA256_ARGUMENT;
#[cfg(target_os = "windows")]
use snapdog_os_installer::worker::{
    WINDOWS_RAW_DEVICE_OPT_IN_ARGUMENT, WINDOWS_RAW_DEVICE_OPT_IN_VALUE,
};

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
const MAX_WORKER_JOB_SIZE: u64 = 64 * 1024;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let worker_reentry =
        std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new(WORKER_JOB_ARGUMENT));
    init_tracing(worker_reentry)?;

    if let Some(job_path) = worker_job_path(std::env::args_os().skip(1))? {
        return run_worker(&job_path);
    }

    run_gui()?;
    Ok(())
}

fn init_tracing(worker_reentry: bool) -> io::Result<()> {
    let filter = if cfg!(debug_assertions) {
        // A machine-level `RUST_LOG` must not silently suppress the diagnostics this explicitly
        // instrumented build exists to collect.
        tracing_subscriber::EnvFilter::new("snapdog_os_installer=debug,info")
    } else {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
    };

    #[cfg(all(debug_assertions, target_os = "macos"))]
    if !worker_reentry {
        use std::os::unix::fs::PermissionsExt;

        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::other("HOME is unavailable for the debug log"))?;
        let directory = home.join("Library/Logs/SnapDog OS Installer");
        fs::create_dir_all(&directory)?;
        let path = directory.join("debug.log");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_target(true)
            .with_thread_ids(true)
            .with_writer(Mutex::new(file))
            .try_init()
            .map_err(|error| io::Error::other(error.to_string()))?;
        tracing::info!(log_path = %path.display(), "debug logging initialized");
        return Ok(());
    }

    let _ = worker_reentry;
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_target(true)
        .with_thread_ids(true)
        .try_init()
        .map_err(|error| io::Error::other(error.to_string()))?;
    Ok(())
}

fn run_gui() -> eframe::Result {
    let icon = load_icon();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("SnapDog OS Installer")
            .with_inner_size([1040.0, 620.0])
            .with_min_inner_size([1040.0, 620.0])
            .with_max_inner_size([1040.0, 620.0])
            .with_resizable(false)
            .with_icon(icon),
        renderer: eframe::Renderer::Wgpu,
        centered: true,
        ..Default::default()
    };

    eframe::run_native(
        "SnapDog OS Installer",
        options,
        Box::new(|context| Ok(Box::new(SnapDogInstallerApp::new(context)))),
    )
}

fn worker_job_path<I>(mut arguments: I) -> io::Result<Option<PathBuf>>
where
    I: Iterator<Item = OsString>,
{
    let Some(argument) = arguments.next() else {
        return Ok(None);
    };
    if argument != WORKER_JOB_ARGUMENT {
        return Ok(None);
    }
    let path = PathBuf::from(arguments.next().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "worker job path is missing")
    })?);
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "worker job path must be absolute",
        ));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let valid_trailing_arguments = arguments.next().as_deref()
        == Some(std::ffi::OsStr::new(WORKER_JOB_SHA256_ARGUMENT))
        && arguments
            .next()
            .is_some_and(|value| valid_sha256_os(&value))
        && arguments.next().is_none();
    #[cfg(target_os = "windows")]
    let valid_trailing_arguments = arguments.next().as_deref()
        == Some(std::ffi::OsStr::new(WORKER_JOB_SHA256_ARGUMENT))
        && arguments
            .next()
            .is_some_and(|value| valid_sha256_os(&value))
        && arguments.next().as_deref()
            == Some(std::ffi::OsStr::new(WINDOWS_RAW_DEVICE_OPT_IN_ARGUMENT))
        && arguments.next().as_deref()
            == Some(std::ffi::OsStr::new(WINDOWS_RAW_DEVICE_OPT_IN_VALUE))
        && arguments.next().is_none();
    if !valid_trailing_arguments {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unexpected arguments after worker job path",
        ));
    }
    Ok(Some(path))
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn run_worker(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    use snapdog_os_installer::worker::{RawDeviceGate, WorkerJob};

    let path_metadata = fs::symlink_metadata(path)?;
    if !worker_job_metadata_safe(&path_metadata) || path_metadata.len() > MAX_WORKER_JOB_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "worker job must be a small, non-symlink regular file",
        )
        .into());
    }

    // Read from the descriptor whose identity was checked instead of resolving the path again.
    // This makes a path replacement during privileged startup fail closed.
    let mut file = open_worker_job_file(path)?;
    let opened_metadata = file.metadata()?;
    if !opened_metadata.is_file()
        || opened_metadata.len() > MAX_WORKER_JOB_SIZE
        || !same_worker_job_file(path, &file, &path_metadata, &opened_metadata)?
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "worker job changed while it was opened",
        )
        .into());
    }
    let capacity = usize::try_from(opened_metadata.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "worker job is too large"))?;
    let mut encoded = Vec::with_capacity(capacity);
    file.by_ref()
        .take(MAX_WORKER_JOB_SIZE + 1)
        .read_to_end(&mut encoded)?;
    if encoded.len() as u64 > MAX_WORKER_JOB_SIZE {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "worker job is too large").into());
    }
    let expected = expected_worker_job_sha256()?;
    let actual = hex::encode(Sha256::digest(&encoded));
    if !actual.eq_ignore_ascii_case(&expected) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "worker job changed during interactive authorization",
        )
        .into());
    }
    let job: WorkerJob = serde_json::from_slice(&encoded)?;
    let gate = RawDeviceGate::from_environment()?;
    execute_worker_job(&job, gate)?;
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn open_worker_job_file(path: &Path) -> io::Result<File> {
    File::open(path)
}

#[cfg(target_os = "windows")]
fn open_worker_job_file(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_SHARE_READ: u32 = 0x1;
    fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(path)
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn valid_sha256_os(value: &std::ffi::OsStr) -> bool {
    let value = value.to_string_lossy();
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn expected_worker_job_sha256() -> io::Result<String> {
    let mut arguments = std::env::args_os();
    let _executable = arguments.next();
    if arguments.next().as_deref() != Some(std::ffi::OsStr::new(WORKER_JOB_ARGUMENT))
        || arguments.next().is_none()
        || arguments.next().as_deref() != Some(std::ffi::OsStr::new(WORKER_JOB_SHA256_ARGUMENT))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid worker authorization arguments",
        ));
    }
    let digest = arguments.next().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "worker job digest is missing")
    })?;
    if !valid_sha256_os(&digest) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "worker job digest is invalid",
        ));
    }
    Ok(digest.to_string_lossy().into_owned())
}

#[cfg(unix)]
fn worker_job_metadata_safe(metadata: &fs::Metadata) -> bool {
    !metadata.file_type().is_symlink() && metadata.is_file()
}

#[cfg(target_os = "windows")]
fn worker_job_metadata_safe(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.is_file() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
}

#[cfg(unix)]
fn same_worker_job_file(
    path: &Path,
    file: &File,
    path_metadata: &fs::Metadata,
    opened_metadata: &fs::Metadata,
) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;

    let current_path_metadata = fs::symlink_metadata(path)?;
    let current_opened_metadata = file.metadata()?;
    Ok(worker_job_metadata_safe(&current_path_metadata)
        && current_path_metadata.dev() == path_metadata.dev()
        && current_path_metadata.ino() == path_metadata.ino()
        && current_opened_metadata.dev() == opened_metadata.dev()
        && current_opened_metadata.ino() == opened_metadata.ino()
        && path_metadata.dev() == opened_metadata.dev()
        && path_metadata.ino() == opened_metadata.ino())
}

#[cfg(target_os = "windows")]
fn same_worker_job_file(
    path: &Path,
    file: &File,
    _path_metadata: &fs::Metadata,
    _opened_metadata: &fs::Metadata,
) -> io::Result<bool> {
    let path_handle = same_file::Handle::from_path(path)?;
    let opened_handle = same_file::Handle::from_file(file.try_clone()?)?;
    Ok(path_handle == opened_handle)
}

#[cfg(target_os = "macos")]
fn execute_worker_job(
    job: &snapdog_os_installer::worker::WorkerJob,
    gate: snapdog_os_installer::worker::RawDeviceGate,
) -> Result<snapdog_os_installer::flash::FlashReport, snapdog_os_installer::worker::WorkerError> {
    snapdog_os_installer::worker::run_macos_worker(job, gate)
}

#[cfg(target_os = "linux")]
fn execute_worker_job(
    job: &snapdog_os_installer::worker::WorkerJob,
    gate: snapdog_os_installer::worker::RawDeviceGate,
) -> Result<snapdog_os_installer::flash::FlashReport, snapdog_os_installer::worker::WorkerError> {
    snapdog_os_installer::worker::run_linux_worker(job, gate)
}

#[cfg(target_os = "windows")]
fn execute_worker_job(
    job: &snapdog_os_installer::worker::WorkerJob,
    gate: snapdog_os_installer::worker::RawDeviceGate,
) -> Result<snapdog_os_installer::flash::FlashReport, snapdog_os_installer::worker::WorkerError> {
    snapdog_os_installer::worker::run_windows_worker(job, gate)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn run_worker(_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "privileged raw-device writing is not implemented on this platform",
    )
    .into())
}

fn load_icon() -> egui::IconData {
    let image = image::load_from_memory(include_bytes!("../assets/icon.png"))
        .expect("embedded application icon must be valid")
        .into_rgba8();
    let (width, height) = image.dimensions();
    egui::IconData {
        rgba: image.into_raw(),
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn valid_worker_arguments(path: &str) -> Vec<OsString> {
        vec![
            OsString::from(WORKER_JOB_ARGUMENT),
            OsString::from(path),
            OsString::from(WORKER_JOB_SHA256_ARGUMENT),
            OsString::from("a".repeat(64)),
        ]
    }

    #[cfg(target_os = "windows")]
    fn valid_worker_arguments(path: &str) -> Vec<OsString> {
        vec![
            OsString::from(WORKER_JOB_ARGUMENT),
            OsString::from(path),
            OsString::from(WORKER_JOB_SHA256_ARGUMENT),
            OsString::from("a".repeat(64)),
            OsString::from(WINDOWS_RAW_DEVICE_OPT_IN_ARGUMENT),
            OsString::from(WINDOWS_RAW_DEVICE_OPT_IN_VALUE),
        ]
    }

    #[cfg(not(target_os = "windows"))]
    const ABSOLUTE_JOB_PATH: &str = "/tmp/job.json";
    #[cfg(target_os = "windows")]
    const ABSOLUTE_JOB_PATH: &str = r"C:\Temp\job.json";

    #[test]
    fn parses_only_exact_worker_reentry() {
        let path = worker_job_path(valid_worker_arguments(ABSOLUTE_JOB_PATH).into_iter()).unwrap();
        assert_eq!(path, Some(PathBuf::from(ABSOLUTE_JOB_PATH)));
        assert!(
            worker_job_path(std::iter::once(OsString::from("--other")))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn rejects_incomplete_or_relative_worker_reentry() {
        assert!(worker_job_path(std::iter::once(OsString::from(WORKER_JOB_ARGUMENT))).is_err());
        assert!(worker_job_path(valid_worker_arguments("relative.json").into_iter()).is_err());
    }

    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    #[test]
    fn rejects_invalid_worker_job_digest() {
        let mut invalid_digest = valid_worker_arguments(ABSOLUTE_JOB_PATH);
        invalid_digest[3] = OsString::from("not-a-sha256");
        assert!(worker_job_path(invalid_digest.into_iter()).is_err());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn rejects_invalid_windows_raw_device_opt_in() {
        let mut invalid_opt_in = valid_worker_arguments(ABSOLUTE_JOB_PATH);
        invalid_opt_in[5] = OsString::from("NO");
        assert!(worker_job_path(invalid_opt_in.into_iter()).is_err());
    }
}
