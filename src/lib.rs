// SPDX-License-Identifier: GPL-3.0-only

//! `SnapDog` OS installer core and desktop application.

pub mod app;
pub mod catalog;
pub mod download;
pub mod drives;
pub mod flash;
pub mod model;
pub mod pipeline;
pub mod worker;

// All direct Win32 FFI is kept behind this one audited, Windows-only safe wrapper. No other
// module is permitted to contain unsafe code.
#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
mod windows_native;

pub use app::SnapDogInstallerApp;
