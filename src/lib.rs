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

pub use app::SnapDogInstallerApp;
