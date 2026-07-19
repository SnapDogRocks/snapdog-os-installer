// SPDX-License-Identifier: GPL-3.0-only

//! `SnapDog` OS installer core and desktop application.

pub mod app;
pub mod catalog;
pub mod drives;
pub mod flash;
pub mod model;

pub use app::SnapDogInstallerApp;
