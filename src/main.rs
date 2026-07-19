// SPDX-License-Identifier: GPL-3.0-only

use eframe::egui;
use snapdog_os_installer::SnapDogInstallerApp;

fn main() -> eframe::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

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
