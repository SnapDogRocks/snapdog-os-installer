// SPDX-License-Identifier: GPL-3.0-only

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(target_os = "macos")]
use std::fs::{self, File};
#[cfg(target_os = "macos")]
use std::io::Read;

use eframe::egui;
use snapdog_os_installer::SnapDogInstallerApp;
use snapdog_os_installer::pipeline::WORKER_JOB_ARGUMENT;

#[cfg(target_os = "macos")]
const MAX_WORKER_JOB_SIZE: u64 = 64 * 1024;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    if let Some(job_path) = worker_job_path(std::env::args_os().skip(1))? {
        return run_worker(&job_path);
    }

    run_gui()?;
    Ok(())
}

fn run_gui() -> eframe::Result {
    let icon = load_icon();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("SnapDog OS Installer")
            .with_inner_size([1040.0, 640.0])
            .with_min_inner_size([1040.0, 640.0])
            .with_max_inner_size([1040.0, 640.0])
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
    if arguments.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unexpected arguments after worker job path",
        ));
    }
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "worker job path must be absolute",
        ));
    }
    Ok(Some(path))
}

#[cfg(target_os = "macos")]
fn run_worker(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    use snapdog_os_installer::worker::{RawDeviceGate, WorkerJob, run_macos_worker};
    use std::os::unix::fs::MetadataExt;

    let path_metadata = fs::symlink_metadata(path)?;
    if path_metadata.file_type().is_symlink()
        || !path_metadata.is_file()
        || path_metadata.len() > MAX_WORKER_JOB_SIZE
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "worker job must be a small, non-symlink regular file",
        )
        .into());
    }

    // Read from the descriptor whose identity was checked instead of resolving the path again.
    // This makes a path replacement during privileged startup fail closed.
    let mut file = File::open(path)?;
    let opened_metadata = file.metadata()?;
    if !opened_metadata.is_file()
        || opened_metadata.len() > MAX_WORKER_JOB_SIZE
        || opened_metadata.dev() != path_metadata.dev()
        || opened_metadata.ino() != path_metadata.ino()
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
    let job: WorkerJob = serde_json::from_slice(&encoded)?;
    let gate = RawDeviceGate::from_environment()?;
    run_macos_worker(&job, gate)?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
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

    #[test]
    fn parses_only_exact_worker_reentry() {
        let path = worker_job_path(
            [
                OsString::from(WORKER_JOB_ARGUMENT),
                OsString::from("/tmp/job.json"),
            ]
            .into_iter(),
        )
        .unwrap();
        assert_eq!(path, Some(PathBuf::from("/tmp/job.json")));
        assert!(
            worker_job_path([OsString::from("--other")].into_iter())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn rejects_incomplete_or_relative_worker_reentry() {
        assert!(worker_job_path([OsString::from(WORKER_JOB_ARGUMENT)].into_iter()).is_err());
        assert!(
            worker_job_path(
                [
                    OsString::from(WORKER_JOB_ARGUMENT),
                    OsString::from("relative.json")
                ]
                .into_iter()
            )
            .is_err()
        );
    }
}
