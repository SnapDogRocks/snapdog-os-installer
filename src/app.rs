// SPDX-License-Identifier: GPL-3.0-only

use std::sync::{
    LazyLock,
    mpsc::{self, Receiver},
};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Color32, FontId, Layout, RichText, Stroke, Vec2};

use crate::catalog::CatalogClient;
use crate::download::{DownloadClient, DownloadProgress};
use crate::drives;
use crate::flash::{FlashProgress, FlashStage};
use crate::model::{Board, Channel, Drive, ImageSelection, Manifest};
use crate::pipeline::{
    PipelineControl, PipelineError, PipelineEvent, PipelineReport, PipelineRequest,
    WorkerRunnerError, run_pipeline,
};
use crate::worker::{WorkerDrive, WorkerPhase, WorkerProgress};

#[cfg(target_os = "linux")]
use crate::pipeline::LinuxWorkerRunner;
#[cfg(target_os = "macos")]
use crate::pipeline::MacOsWorkerRunner;
#[cfg(target_os = "windows")]
use crate::pipeline::WindowsWorkerRunner;

const BACKGROUND: Color32 = Color32::from_rgb(28, 25, 23);
const SURFACE: Color32 = Color32::from_rgb(38, 34, 31);
const ELEVATED: Color32 = Color32::from_rgb(48, 43, 39);
const ORANGE: Color32 = Color32::from_rgb(225, 136, 46);
const BRIGHT_ORANGE: Color32 = Color32::from_rgb(255, 159, 10);
const GREEN: Color32 = Color32::from_rgb(48, 209, 88);
const TEXT: Color32 = Color32::from_rgb(242, 242, 242);
const MUTED: Color32 = Color32::from_rgb(170, 166, 162);
const SUBTLE: Color32 = Color32::from_rgb(118, 114, 111);
const DANGER: Color32 = Color32::from_rgb(255, 105, 97);
const VERSION_CONTROL_WIDTH: f32 = 220.0;
const WORKFLOW_CONNECTOR_WIDTH: f32 = 62.0;
const STEP_ICON_CENTER_Y: f32 = 151.0;
const SETUP_SOURCE_WIDTH: f32 = 380.0;
const SETUP_STEP_WIDTH: f32 = 220.0;
const SETUP_RIGHT_WIDTH: f32 = SETUP_STEP_WIDTH * 2.0 + WORKFLOW_CONNECTOR_WIDTH;
const SETUP_CONTENT_WIDTH: f32 = SETUP_SOURCE_WIDTH + WORKFLOW_CONNECTOR_WIDTH + SETUP_RIGHT_WIDTH;
const BOARD_GRID_WIDTH: f32 = 302.0;
const BOARD_GRID_HEIGHT: f32 = 274.0;
const WORKFLOW_OPTICAL_LEFT_SHIFT: f32 = 15.0;
// assets/snapdog-logo.svg has a square 595.3 × 595.3 viewBox.
const SNAPDOG_LOGO_ASPECT_RATIO: f32 = 1.0;
const THIRD_PARTY_NOTICES: &str = include_str!("../THIRD_PARTY_NOTICES.txt");
const GPL_3_URL: &str = "https://opensource.org/license/GPL-3.0";
static THIRD_PARTY_LICENSES: LazyLock<Vec<ThirdPartyLicenseGroup>> =
    LazyLock::new(|| third_party_license_groups(THIRD_PARTY_NOTICES));
const PENDING_RELEASE_METADATA: &str =
    "Secure installer metadata is still publishing for this release.";

enum CatalogEvent {
    Loaded(Channel, Vec<Manifest>),
    Failed(Channel, String),
}

#[derive(Debug)]
struct ThirdPartyLicenseGroup {
    title: String,
    packages: Vec<String>,
    notices: Vec<String>,
}

fn fetch_catalog_async() -> Receiver<CatalogEvent> {
    let (catalog_tx, catalog_rx) = mpsc::channel();
    thread::spawn(move || match CatalogClient::new() {
        Ok(client) => {
            for channel in [Channel::Release, Channel::Beta] {
                let event = match client.fetch_catalog(channel) {
                    Ok(manifests) => CatalogEvent::Loaded(channel, manifests),
                    Err(error) => CatalogEvent::Failed(channel, error.to_string()),
                };
                if catalog_tx.send(event).is_err() {
                    return;
                }
            }
        }
        Err(error) => {
            for channel in [Channel::Release, Channel::Beta] {
                let _ = catalog_tx.send(CatalogEvent::Failed(channel, error.to_string()));
            }
        }
    });
    catalog_rx
}

enum OperationMessage {
    Event(PipelineEvent),
    Finished(Result<PipelineReport, OperationFailure>),
}

#[derive(Clone, Debug)]
struct OperationFailure {
    message: String,
    cancelled: bool,
    authorization_denied: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OperationPhase {
    Downloading,
    Decompressing,
    Authorizing,
    ValidatingImage,
    ValidatingTarget,
    Unmounting,
    Writing,
    Verifying,
    Syncing,
    Ejecting,
}

impl OperationPhase {
    const fn title(self) -> &'static str {
        match self {
            Self::Downloading => "Downloading SnapDog OS…",
            Self::Decompressing => "Preparing the image…",
            Self::Authorizing => "Waiting for approval…",
            Self::ValidatingImage => "Checking the image…",
            Self::ValidatingTarget => "Checking the target…",
            Self::Unmounting => "Preparing the target…",
            Self::Writing => "Writing SnapDog OS…",
            Self::Verifying => "Verifying the SD card…",
            Self::Syncing => "Finishing the write…",
            Self::Ejecting => "Safely ejecting…",
        }
    }

    const fn detail(self) -> &'static str {
        match self {
            Self::Downloading => "The image is downloaded only now, after your confirmation.",
            Self::Decompressing => {
                "Checking size and both release checksums before touching the SD card."
            }
            Self::Authorizing => "Approve the system dialog so the selected drive can be written.",
            Self::ValidatingImage => {
                "Rechecking the prepared image before the selected drive is accessed."
            }
            Self::ValidatingTarget => {
                "Confirming that the same removable physical drive is still connected."
            }
            Self::Unmounting => "Closing mounted volumes before raw-device access.",
            Self::Writing => "Do not remove the SD card. Cancelling now can leave it incomplete.",
            Self::Verifying => "Reading the image back to detect faulty media or write errors.",
            Self::Syncing => "Flushing all buffered data to the SD card.",
            Self::Ejecting => "Waiting until the system reports that the card is safe to remove.",
        }
    }

    const fn step(self) -> usize {
        match self {
            Self::Downloading => 0,
            Self::Decompressing
            | Self::Authorizing
            | Self::ValidatingImage
            | Self::ValidatingTarget
            | Self::Unmounting => 1,
            Self::Writing => 2,
            Self::Verifying => 3,
            Self::Syncing | Self::Ejecting => 4,
        }
    }

    const fn can_cancel(self) -> bool {
        !matches!(self, Self::Syncing | Self::Ejecting)
    }
}

struct RunningOperation {
    receiver: Receiver<OperationMessage>,
    control: PipelineControl,
    selection: ImageSelection,
    drive: Drive,
    phase: OperationPhase,
    processed: Option<u64>,
    total: Option<u64>,
    started: Instant,
    verification_enabled: bool,
    cancel_requested: bool,
    skip_requested: bool,
}

struct SuccessState {
    report: PipelineReport,
    selection: ImageSelection,
    drive: Drive,
    elapsed: Duration,
}

struct FailureState {
    failure: OperationFailure,
    selection: ImageSelection,
    drive: Drive,
    phase: OperationPhase,
}

enum OperationState {
    Idle,
    Running(RunningOperation),
    Succeeded(SuccessState),
    Failed(FailureState),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Overlay {
    TargetPicker,
    EraseConfirmation,
    SkipConfirmation,
    Settings,
    ThirdPartyNotices,
    CloseConfirmation,
}

/// `SnapDog`'s guided three-step installer interface.
pub struct SnapDogInstallerApp {
    catalog_rx: Receiver<CatalogEvent>,
    stable: Option<Vec<Manifest>>,
    beta: Option<Vec<Manifest>>,
    catalog_error: Option<String>,
    board: Option<Board>,
    channel: Channel,
    selected_version: Option<String>,
    confirmed: Option<ImageSelection>,
    drives: Vec<Drive>,
    selected_drive: Option<Drive>,
    drive_status: Option<String>,
    operation: OperationState,
    overlay: Option<Overlay>,
    verify_after_write: bool,
    quit_after_operation: bool,
}

impl SnapDogInstallerApp {
    /// Initialize image loaders, theme, and asynchronous catalog loading.
    pub fn new(context: &eframe::CreationContext<'_>) -> Self {
        egui_extras::install_image_loaders(&context.egui_ctx);
        configure_style(&context.egui_ctx);

        let catalog_rx = fetch_catalog_async();

        Self {
            catalog_rx,
            stable: None,
            beta: None,
            catalog_error: None,
            board: None,
            channel: Channel::Release,
            selected_version: None,
            confirmed: None,
            drives: Vec::new(),
            selected_drive: None,
            drive_status: None,
            operation: OperationState::Idle,
            overlay: None,
            verify_after_write: true,
            quit_after_operation: false,
        }
    }

    fn receive_catalog(&mut self) {
        while let Ok(event) = self.catalog_rx.try_recv() {
            match event {
                CatalogEvent::Loaded(channel, manifests) => {
                    if self.channel == channel && self.selected_version.is_none() {
                        self.selected_version =
                            manifests.first().map(|manifest| manifest.version.clone());
                    }
                    match channel {
                        Channel::Release => self.stable = Some(manifests),
                        Channel::Beta => self.beta = Some(manifests),
                    }
                    if self.channel == channel {
                        self.catalog_error = None;
                    }
                }
                CatalogEvent::Failed(channel, error) if self.channel == channel => {
                    self.catalog_error = Some(error);
                }
                CatalogEvent::Failed(_, _) => {}
            }
        }
    }

    fn reload_catalog(&mut self) {
        self.catalog_rx = fetch_catalog_async();
        self.stable = None;
        self.beta = None;
        self.selected_version = None;
        self.catalog_error = None;
        self.clear_image_dependants();
    }

    fn receive_operation(&mut self, context: &egui::Context) {
        let transition = if let OperationState::Running(running) = &mut self.operation {
            let mut finished = None;
            while let Ok(message) = running.receiver.try_recv() {
                match message {
                    OperationMessage::Event(event) => apply_pipeline_event(running, event),
                    OperationMessage::Finished(result) => finished = Some(result),
                }
            }
            finished.map(|result| {
                (
                    result,
                    running.selection.clone(),
                    running.drive.clone(),
                    running.phase,
                    running.started.elapsed(),
                )
            })
        } else {
            None
        };

        if let Some((result, selection, drive, phase, elapsed)) = transition {
            match &result {
                Ok(report) => tracing::info!(
                    device = drive.device,
                    stable_id = drive.id,
                    bytes = report.raw_size,
                    verified = report.verified,
                    elapsed_ms = elapsed.as_millis(),
                    "flash operation completed"
                ),
                Err(failure) => tracing::error!(
                    device = drive.device,
                    stable_id = drive.id,
                    phase = ?phase,
                    error = failure.message,
                    elapsed_ms = elapsed.as_millis(),
                    "flash operation failed"
                ),
            }
            self.operation = match result {
                Ok(report) => OperationState::Succeeded(SuccessState {
                    report,
                    selection,
                    drive,
                    elapsed,
                }),
                Err(failure) => OperationState::Failed(FailureState {
                    failure,
                    selection,
                    drive,
                    phase,
                }),
            };
            self.overlay = None;
            if self.quit_after_operation {
                context.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn selected_catalog(&self) -> Option<&[Manifest]> {
        match self.channel {
            Channel::Release => self.stable.as_deref(),
            Channel::Beta => self.beta.as_deref(),
        }
    }

    fn selected_manifest(&self) -> Option<&Manifest> {
        let catalog = self.selected_catalog()?;
        self.selected_version
            .as_deref()
            .and_then(|selected| catalog.iter().find(|manifest| manifest.version == selected))
            .or_else(|| catalog.first())
    }

    fn source_step(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            step_title(ui, "1. Choose image", true);
            ui.add_space(12.0);
            ui.label(
                RichText::new("Choose your Raspberry Pi")
                    .size(18.0)
                    .strong(),
            );
            ui.add_space(8.0);

            ui.allocate_ui_with_layout(
                Vec2::new(BOARD_GRID_WIDTH, BOARD_GRID_HEIGHT),
                Layout::top_down(Align::Center),
                |ui| {
                    egui::Grid::new("board-grid")
                        .num_columns(2)
                        .spacing([14.0, 6.0])
                        .show(ui, |ui| {
                            for (index, board) in Board::ALL.into_iter().enumerate() {
                                let selected = self.board == Some(board);
                                if board_button(ui, board, selected).clicked() {
                                    self.board = Some(board);
                                    self.clear_image_dependants();
                                }
                                if index % 2 == 1 {
                                    ui.end_row();
                                }
                            }
                        });
                },
            );

            self.version_section(ui);
        });
    }

    fn version_section(&mut self, ui: &mut egui::Ui) {
        ui.add_space(7.0);
        ui.label(RichText::new("Choose your version").size(18.0).strong());
        ui.add_space(6.0);
        self.channel_picker(ui);
        ui.add_space(6.0);

        let version = self.selected_manifest().map_or_else(
            || "Loading releases…".to_owned(),
            |manifest| manifest.version.clone(),
        );
        self.version_picker(ui, &version);
        ui.add_space(6.0);
        self.release_confirmation(ui, &version);

        if self.channel == Channel::Beta {
            ui.add_space(3.0);
            ui.label(
                RichText::new("Preview build — expect rough edges.")
                    .color(ORANGE)
                    .small(),
            );
        }
        if let Some(error) = &self.catalog_error {
            ui.add_space(3.0);
            ui.label(RichText::new(error).color(DANGER).small());
        }
    }

    fn channel_picker(&mut self, ui: &mut egui::Ui) {
        ui.allocate_ui_with_layout(
            Vec2::new(VERSION_CONTROL_WIDTH, 34.0),
            Layout::top_down(Align::Center),
            |ui| {
                ui.add_enabled_ui(self.board.is_some(), |ui| {
                    egui::Frame::new()
                        .fill(ELEVATED)
                        .stroke(Stroke::new(1.0_f32, Color32::from_gray(76)))
                        .corner_radius(18.0)
                        .inner_margin(egui::Margin::same(2))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 0.0;
                                for channel in [Channel::Release, Channel::Beta] {
                                    let selected = self.channel == channel;
                                    let button = egui::Button::new(
                                        RichText::new(channel.label())
                                            .color(if selected { Color32::BLACK } else { MUTED })
                                            .strong(),
                                    )
                                    .fill(if selected {
                                        Color32::from_rgb(244, 242, 240)
                                    } else {
                                        Color32::TRANSPARENT
                                    })
                                    .stroke(Stroke::NONE)
                                    .corner_radius(15.0)
                                    .min_size(Vec2::new(106.0, 30.0));
                                    if ui.add(button).clicked() {
                                        self.channel = channel;
                                        self.selected_version = self
                                            .selected_catalog()
                                            .and_then(|catalog| catalog.first())
                                            .map(|manifest| manifest.version.clone());
                                        self.clear_image_dependants();
                                        self.catalog_error = None;
                                    }
                                }
                            });
                        });
                });
            },
        );
    }

    fn version_picker(&mut self, ui: &mut egui::Ui, version: &str) {
        let releases = self.selected_catalog().map_or_else(Vec::new, |catalog| {
            catalog
                .iter()
                .map(|manifest| manifest.version.clone())
                .collect::<Vec<_>>()
        });
        let previous = self.selected_version.clone();
        ui.allocate_ui_with_layout(
            Vec2::new(VERSION_CONTROL_WIDTH, 40.0),
            Layout::top_down(Align::Center),
            |ui| {
                ui.add_enabled_ui(self.board.is_some() && !releases.is_empty(), |ui| {
                    let controls = &mut ui.style_mut().visuals.widgets;
                    for visuals in [
                        &mut controls.inactive,
                        &mut controls.hovered,
                        &mut controls.active,
                        &mut controls.open,
                    ] {
                        visuals.bg_fill = Color32::from_rgb(245, 243, 241);
                        visuals.weak_bg_fill = Color32::from_rgb(245, 243, 241);
                        visuals.fg_stroke = Stroke::new(1.0_f32, Color32::BLACK);
                        visuals.bg_stroke = Stroke::new(1.0_f32, Color32::from_gray(198));
                        visuals.corner_radius = 10.0.into();
                    }
                    egui::ComboBox::from_id_salt("release-version")
                        .width(VERSION_CONTROL_WIDTH)
                        .selected_text(RichText::new(version).color(Color32::BLACK))
                        .show_ui(ui, |ui| {
                            for (index, release) in releases.iter().enumerate() {
                                let label = if index == 0 {
                                    format!("{release} — Latest")
                                } else {
                                    release.clone()
                                };
                                ui.selectable_value(
                                    &mut self.selected_version,
                                    Some(release.clone()),
                                    label,
                                );
                            }
                        });
                });
            },
        );
        if self.selected_version != previous {
            self.clear_image_dependants();
        }
    }

    fn release_confirmation(&mut self, ui: &mut egui::Ui, version: &str) {
        let release_error = self.board.and_then(|board| {
            self.selected_manifest()
                .and_then(|manifest| validate_release_image(manifest, board).err())
        });
        let can_confirm =
            self.board.is_some() && self.selected_manifest().is_some() && release_error.is_none();
        let label = self.confirmed.as_ref().map_or_else(
            || format!("Use SnapDog OS {version}"),
            |selection| format!("SnapDog OS {} selected", selection.manifest.version),
        );
        if ui
            .add_enabled(
                can_confirm,
                setup_action_button(&label, Vec2::new(220.0, 40.0), can_confirm),
            )
            .clicked()
        {
            self.confirm_selection();
        }

        if let Some(error) = release_error {
            ui.add_space(5.0);
            if error == PENDING_RELEASE_METADATA {
                let mut refresh = false;
                egui::Frame::new()
                    .fill(SURFACE)
                    .stroke(Stroke::new(1.0_f32, Color32::from_rgb(114, 76, 40)))
                    .corner_radius(12.0)
                    .inner_margin(egui::Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        ui.set_width(280.0);
                        ui.horizontal(|ui| {
                            ui.add(egui::Spinner::new().size(14.0));
                            ui.label(
                                RichText::new("Waiting for secure release metadata")
                                    .color(MUTED)
                                    .small(),
                            );
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let button =
                                    egui::Button::new(RichText::new("Retry").color(TEXT).strong())
                                        .fill(ELEVATED)
                                        .stroke(Stroke::new(1.0_f32, ORANGE))
                                        .corner_radius(8.0)
                                        .min_size(Vec2::new(68.0, 28.0));
                                refresh = ui.add(button).clicked();
                            });
                        });
                    });
                if refresh {
                    self.reload_catalog();
                }
            } else {
                ui.label(RichText::new(error).color(ORANGE).small());
            }
        }
    }

    fn clear_image_dependants(&mut self) {
        self.confirmed = None;
        self.selected_drive = None;
        self.drives.clear();
        self.drive_status = None;
    }

    fn confirm_selection(&mut self) {
        let (Some(board), Some(manifest)) = (self.board, self.selected_manifest().cloned()) else {
            return;
        };
        if let Err(error) = validate_release_image(&manifest, board) {
            self.catalog_error = Some(error);
            return;
        }
        let Some(image) = manifest.image_for(board) else {
            self.catalog_error = Some(format!(
                "SnapDog OS {} has no image for {}",
                manifest.version,
                board.label()
            ));
            return;
        };
        match CatalogClient::image_url(image.download_reference()) {
            Ok(url) => {
                self.confirmed = Some(ImageSelection {
                    board,
                    manifest,
                    url,
                });
                self.selected_drive = None;
                self.drives.clear();
                self.catalog_error = None;
            }
            Err(error) => self.catalog_error = Some(error.to_string()),
        }
    }

    fn target_step(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            step_title(ui, "2. Target", self.confirmed.is_some());
            ui.add_space(102.0);
            let ready = self.confirmed.is_some();
            drive_icon(ui, if ready { ORANGE } else { SUBTLE }, 58.0);
            ui.add_space(18.0);
            let label = self
                .selected_drive
                .as_ref()
                .map_or("Select target", |_| "Change target");
            if ui
                .add_enabled(
                    ready,
                    setup_action_button(label, Vec2::new(200.0, 46.0), ready),
                )
                .clicked()
            {
                self.refresh_drives();
                self.overlay = Some(Overlay::TargetPicker);
            }
            ui.add_space(8.0);
            if let Some(drive) = &self.selected_drive {
                ui.label(RichText::new(drive.label()).color(TEXT).strong().small());
                ui.label(RichText::new(&drive.device).color(MUTED).small());
            } else {
                let status = if ready {
                    self.drive_status.as_deref().unwrap_or(
                        "Only removable physical drives are shown. System drives are excluded.",
                    )
                } else {
                    "Confirm the image selection first"
                };
                ui.label(RichText::new(status).color(MUTED).small());
            }
        });
    }

    fn refresh_drives(&mut self) {
        match drives::removable_drives() {
            Ok(found) => {
                self.drives = found;
                if self.selected_drive.as_ref().is_some_and(|selected| {
                    !self.drives.iter().any(|drive| {
                        drive.id == selected.id
                            && drive.device == selected.device
                            && drive.capacity == selected.capacity
                    })
                }) {
                    self.selected_drive = None;
                }
                self.drive_status = Some(if self.drives.is_empty() {
                    "No removable physical drive found. Insert an SD card and refresh.".to_owned()
                } else {
                    "Choose exactly one target. Its contents will be erased.".to_owned()
                });
            }
            Err(error) => {
                self.drives.clear();
                self.selected_drive = None;
                self.drive_status = Some(error.to_string());
            }
        }
    }

    fn flash_step(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            let request = self.pipeline_request();
            let ready = request.is_ok();
            step_title(ui, "3. Flash", ready);
            ui.add_space(99.0);
            lightning_icon(ui, if ready { ORANGE } else { SUBTLE }, 62.0);
            ui.add_space(17.0);
            if ui
                .add_enabled(
                    ready,
                    setup_action_button("Flash!", Vec2::new(200.0, 46.0), ready),
                )
                .clicked()
            {
                self.overlay = Some(Overlay::EraseConfirmation);
            }
            ui.add_space(8.0);
            let status = if ready {
                "Ready to download and flash"
            } else if self.confirmed.is_none() {
                "Complete the image selection first"
            } else if self.selected_drive.is_none() {
                "Select a target first"
            } else {
                request
                    .as_ref()
                    .err()
                    .map_or("Review the previous steps", String::as_str)
            };
            ui.label(
                RichText::new(status)
                    .color(if ready { TEXT } else { MUTED })
                    .small(),
            );
        });
    }

    fn pipeline_request(&self) -> Result<PipelineRequest, String> {
        let selection = self
            .confirmed
            .as_ref()
            .ok_or_else(|| "Choose an image first".to_owned())?;
        let drive = self
            .selected_drive
            .as_ref()
            .ok_or_else(|| "Select a target first".to_owned())?;
        if !cfg!(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "windows"
        )) {
            return Err("Flashing is not available on this platform yet".to_owned());
        }
        let image = selection
            .manifest
            .image_for(selection.board)
            .ok_or_else(|| "Selected release has no image for this Raspberry Pi".to_owned())?;
        validate_release_image(&selection.manifest, selection.board)?;
        let compressed_sha256 = required_hash(image.sha256.as_deref(), "compressed image")?;
        let raw_sha256 = required_hash(image.raw_sha256.as_deref(), "raw image")?;
        let compressed_size = required_size(image.compressed_size, "compressed image")?;
        let raw_size = required_size(image.uncompressed_size, "raw image")?;
        if raw_size > drive.capacity {
            return Err("The selected target is too small for this image".to_owned());
        }

        Ok(PipelineRequest {
            image_url: selection.url.clone(),
            expected_compressed_sha256: Some(compressed_sha256),
            expected_compressed_size: Some(compressed_size),
            expected_raw_size: Some(raw_size),
            expected_raw_sha256: Some(raw_sha256),
            drive: WorkerDrive {
                id: drive.id.clone(),
                device: drive.device.clone(),
                capacity: drive.capacity,
            },
            verify: self.verify_after_write,
        })
    }

    fn start_operation(&mut self) {
        let Ok(request) = self.pipeline_request() else {
            return;
        };
        let (Some(selection), Some(drive)) = (self.confirmed.clone(), self.selected_drive.clone())
        else {
            return;
        };
        let (sender, receiver) = mpsc::channel();
        let verification_enabled = request.verify;
        let control = PipelineControl::default();
        let background_control = control.clone();
        thread::spawn(move || {
            let result = execute_pipeline(&request, &background_control, |event| {
                let _ = sender.send(OperationMessage::Event(event));
            });
            let outcome = result.map_err(|error| operation_failure(&error));
            let _ = sender.send(OperationMessage::Finished(outcome));
        });
        self.operation = OperationState::Running(RunningOperation {
            receiver,
            control,
            selection,
            drive,
            phase: OperationPhase::Downloading,
            processed: Some(0),
            total: None,
            started: Instant::now(),
            verification_enabled,
            cancel_requested: false,
            skip_requested: false,
        });
        self.overlay = None;
    }

    fn reset_for_another(&mut self) {
        self.operation = OperationState::Idle;
        self.board = None;
        self.channel = Channel::Release;
        self.selected_version = self
            .stable
            .as_deref()
            .and_then(|catalog| catalog.first())
            .map(|manifest| manifest.version.clone());
        self.confirmed = None;
        self.selected_drive = None;
        self.drives.clear();
        self.drive_status = None;
        self.catalog_error = None;
        self.overlay = None;
    }

    fn return_to_setup(&mut self) {
        self.operation = OperationState::Idle;
        self.selected_drive = None;
        self.drives.clear();
        self.drive_status = Some("Select the target again before retrying.".to_owned());
        self.overlay = None;
    }

    fn setup_screen(&mut self, ui: &mut egui::Ui) {
        const STEP_AREA_HEIGHT: f32 = 316.0;

        let content_height = ui.available_height();
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.add_space(setup_content_left_inset(ui.available_width()));
            ui.allocate_ui_with_layout(
                Vec2::new(SETUP_SOURCE_WIDTH, content_height),
                Layout::top_down(Align::Center),
                |ui| self.source_step(ui),
            );
            connector(ui, self.confirmed.is_some(), content_height);
            ui.allocate_ui_with_layout(
                Vec2::new(SETUP_RIGHT_WIDTH, content_height),
                Layout::top_down(Align::Center),
                |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.allocate_ui_with_layout(
                        Vec2::new(SETUP_RIGHT_WIDTH, STEP_AREA_HEIGHT),
                        Layout::left_to_right(Align::Min),
                        |ui| {
                            ui.spacing_mut().item_spacing.x = 0.0;
                            ui.allocate_ui_with_layout(
                                Vec2::new(SETUP_STEP_WIDTH, STEP_AREA_HEIGHT),
                                Layout::top_down(Align::Center),
                                |ui| self.target_step(ui),
                            );
                            connector(ui, self.selected_drive.is_some(), STEP_AREA_HEIGHT);
                            ui.allocate_ui_with_layout(
                                Vec2::new(SETUP_STEP_WIDTH, STEP_AREA_HEIGHT),
                                Layout::top_down(Align::Center),
                                |ui| self.flash_step(ui),
                            );
                        },
                    );
                },
            );
        });
    }

    fn show_target_picker(&mut self, context: &egui::Context) {
        let mut close = false;
        let response = branded_modal(context, "target-picker", |ui| {
            ui.set_width(470.0);
            ui.heading("Choose the SD card");
            ui.add_space(4.0);
            ui.label(
                RichText::new("Only removable physical drives are listed. The selected drive will be completely erased.")
                    .color(MUTED),
            );
            ui.add_space(14.0);
            if self.drives.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(14.0);
                    drive_icon(ui, MUTED, 46.0);
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(
                            self.drive_status
                                .as_deref()
                                .unwrap_or("No removable physical drive found."),
                        )
                        .color(MUTED),
                    );
                    ui.add_space(14.0);
                });
            } else {
                egui::ScrollArea::vertical()
                    .max_height(272.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for drive in self.drives.clone() {
                            let selected = self.selected_drive.as_ref() == Some(&drive);
                            let text = format!("{}\n{}", drive.label(), drive.device);
                            let button =
                                egui::Button::new(RichText::new(text).color(TEXT).strong())
                                    .fill(if selected { ELEVATED } else { SURFACE })
                                    .stroke(Stroke::new(
                                        if selected { 2.0_f32 } else { 1.0_f32 },
                                        if selected {
                                            ORANGE
                                        } else {
                                            Color32::from_gray(78)
                                        },
                                    ))
                                    .corner_radius(12.0)
                                    .min_size(Vec2::new(454.0, 62.0));
                            if ui.add(button).clicked() {
                                self.selected_drive = Some(drive);
                                close = true;
                            }
                            ui.add_space(6.0);
                        }
                    });
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui
                    .add(secondary_button("Cancel", Vec2::new(146.0, 42.0)))
                    .clicked()
                {
                    close = true;
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add(primary_button("Refresh", Vec2::new(146.0, 42.0)))
                        .clicked()
                    {
                        self.refresh_drives();
                    }
                });
            });
        });
        if close || response.should_close() {
            self.overlay = None;
        }
    }

    fn show_erase_confirmation(&mut self, context: &egui::Context) {
        let (Some(selection), Some(drive)) =
            (self.confirmed.as_ref(), self.selected_drive.as_ref())
        else {
            self.overlay = None;
            return;
        };
        let mut action = None;
        let response = branded_modal(context, "erase-confirmation", |ui| {
            ui.set_width(470.0);
            ui.heading("Erase this drive?");
            ui.add_space(6.0);
            ui.label(
                RichText::new("This cannot be undone. Every partition and file on the selected drive will be replaced.")
                    .color(DANGER),
            );
            ui.add_space(14.0);
            summary_row(ui, "Target", &drive.label());
            summary_row(ui, "Device", &drive.device);
            summary_row(
                ui,
                "Image",
                &format!("SnapDog OS {}", selection.manifest.version),
            );
            summary_row(ui, "Model", selection.board.label());
            summary_row(
                ui,
                "Verification",
                if self.verify_after_write {
                    "Enabled"
                } else {
                    "Skipped"
                },
            );
            ui.add_space(18.0);
            ui.horizontal(|ui| {
                if ui
                    .add(secondary_button("Cancel", Vec2::new(220.0, 44.0)))
                    .clicked()
                {
                    action = Some(false);
                }
                if ui
                    .add(primary_button("Erase & Flash", Vec2::new(220.0, 44.0)))
                    .clicked()
                {
                    action = Some(true);
                }
            });
        });
        if action == Some(true) {
            self.start_operation();
        } else if action == Some(false) || response.should_close() {
            self.overlay = None;
        }
    }

    fn show_skip_confirmation(&mut self, context: &egui::Context) {
        if !matches!(
            &self.operation,
            OperationState::Running(RunningOperation {
                phase: OperationPhase::Verifying,
                skip_requested: false,
                ..
            })
        ) {
            self.overlay = None;
            return;
        }

        let mut action = None;
        let response = branded_modal(context, "skip-confirmation", |ui| {
            ui.set_width(430.0);
            ui.heading("Skip verification?");
            ui.add_space(6.0);
            ui.label(
                RichText::new("Verification detects faulty SD cards and incomplete writes. The card will be marked as not verified.")
                    .color(MUTED),
            );
            ui.add_space(18.0);
            ui.horizontal(|ui| {
                if ui
                    .add(secondary_button("Keep verifying", Vec2::new(200.0, 44.0)))
                    .clicked()
                {
                    action = Some(false);
                }
                if ui
                    .add(primary_button("Skip", Vec2::new(200.0, 44.0)))
                    .clicked()
                {
                    action = Some(true);
                }
            });
        });
        if action == Some(true) {
            if let OperationState::Running(running) = &mut self.operation
                && running.phase == OperationPhase::Verifying
                && !running.skip_requested
                && running.control.skip_verification().is_ok()
            {
                running.skip_requested = true;
            }
            self.overlay = None;
        } else if action == Some(false) || response.should_close() {
            self.overlay = None;
        }
    }

    fn show_settings(&mut self, context: &egui::Context) {
        let mut show_notices = false;
        let response = branded_modal(context, "settings", |ui| {
            ui.set_width(420.0);
            ui.heading("Settings");
            ui.add_space(12.0);
            egui::Frame::new()
                .fill(ELEVATED)
                .corner_radius(12.0)
                .inner_margin(egui::Margin::same(14))
                .show(ui, |ui| {
                    let mut switch_rect = egui::Rect::NOTHING;
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(RichText::new("Verify after writing").strong());
                            ui.label(
                                RichText::new("Recommended — reads the complete image back.")
                                    .color(MUTED)
                                    .small(),
                            );
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            (switch_rect, _) =
                                ui.allocate_exact_size(Vec2::new(44.0, 26.0), egui::Sense::hover());
                        });
                    });
                    let mut response = ui.interact(
                        ui.min_rect(),
                        ui.id().with("verify-after-writing"),
                        egui::Sense::click(),
                    );
                    let keyboard_activation = response.has_focus()
                        && ui.input(|input| {
                            input.key_pressed(egui::Key::Space)
                                || input.key_pressed(egui::Key::Enter)
                        });
                    if response.clicked() || keyboard_activation {
                        self.verify_after_write = !self.verify_after_write;
                        response.mark_changed();
                    }
                    paint_apple_switch(ui, switch_rect, &response, self.verify_after_write);
                    response.on_hover_cursor(egui::CursorIcon::PointingHand);
                });
            ui.add_space(14.0);
            ui.vertical_centered(|ui| {
                ui.hyperlink_to(
                    "Source code",
                    "https://github.com/SnapDogRocks/snapdog-os-installer",
                );
                if ui.link("Third-party acknowledgements").clicked() {
                    show_notices = true;
                }
                ui.add_space(8.0);
                ui.label(RichText::new("(C) 2026 Fabian Schmieder").color(MUTED));
                ui.hyperlink_to("Licensed under GPL-3", GPL_3_URL);
            });
            ui.add_space(14.0);
            ui.vertical_centered(|ui| {
                if ui
                    .add(primary_button("Done", Vec2::new(180.0, 42.0)))
                    .clicked()
                {
                    ui.close();
                }
            });
        });
        if show_notices {
            self.overlay = Some(Overlay::ThirdPartyNotices);
        } else if response.should_close() {
            self.overlay = None;
        }
    }

    fn show_third_party_notices(&mut self, context: &egui::Context) {
        let mut back = false;
        let response = branded_modal(context, "third-party-notices", |ui| {
            ui.set_width(640.0);
            ui.heading("Open-source acknowledgements");
            ui.add_space(6.0);
            ui.label(
                RichText::new(
                    "SnapDog OS Installer is built with these open-source libraries. Thank you to every maintainer and contributor.",
                )
                    .color(MUTED),
            );
            ui.add_space(12.0);
            ui.label(RichText::new("Licenses and libraries").strong());
            ui.add_space(6.0);
            egui::ScrollArea::vertical()
                .id_salt("third-party-acknowledgements-v2-scroll")
                .max_height(330.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.set_width(640.0);
                    for (index, license) in THIRD_PARTY_LICENSES.iter().enumerate() {
                        third_party_license_card(ui, index, license);
                        ui.add_space(7.0);
                    }
                });
            ui.add_space(12.0);
            ui.vertical_centered(|ui| {
                if ui
                    .add(primary_button("Back to Settings", Vec2::new(200.0, 42.0)))
                    .clicked()
                {
                    back = true;
                }
            });
        });
        if back || response.should_close() {
            self.overlay = Some(Overlay::Settings);
        }
    }

    fn show_close_confirmation(&mut self, context: &egui::Context) {
        let mut action = None;
        let response = branded_modal(context, "close-confirmation", |ui| {
            ui.set_width(440.0);
            ui.heading("Installer is still working");
            ui.add_space(6.0);
            ui.label(
                RichText::new("SnapDog will stop at a safe boundary, attempt to eject the SD card, and then quit.")
                    .color(MUTED),
            );
            ui.add_space(18.0);
            ui.horizontal(|ui| {
                if ui
                    .add(secondary_button("Keep working", Vec2::new(205.0, 44.0)))
                    .clicked()
                {
                    action = Some(false);
                }
                if ui
                    .add(primary_button("Stop & Quit", Vec2::new(205.0, 44.0)))
                    .clicked()
                {
                    action = Some(true);
                }
            });
        });
        if action == Some(true) {
            self.quit_after_operation = true;
            if let OperationState::Running(running) = &mut self.operation
                && running.phase.can_cancel()
            {
                let _ = running.control.cancel();
                running.cancel_requested = true;
            }
            self.overlay = None;
        } else if action == Some(false) || response.should_close() {
            self.overlay = None;
        }
    }
}

impl eframe::App for SnapDogInstallerApp {
    fn update(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.receive_catalog();
        self.receive_operation(context);

        let running = matches!(self.operation, OperationState::Running(_));
        if running || self.stable.is_none() || self.beta.is_none() {
            context.request_repaint_after(Duration::from_millis(80));
        }
        if running && context.input(|input| input.viewport().close_requested()) {
            context.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if !self.quit_after_operation {
                self.overlay = Some(Overlay::CloseConfirmation);
            }
        }

        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(BACKGROUND)
                    .inner_margin(egui::Margin::same(24)),
            )
            .show(context, |ui| {
                let setup = matches!(self.operation, OperationState::Idle);
                if setup {
                    self.setup_top_bar(ui);
                } else {
                    self.top_bar(ui);
                    ui.add_space(10.0);
                }
                let action = match &mut self.operation {
                    OperationState::Idle => {
                        self.setup_screen(ui);
                        ScreenAction::None
                    }
                    OperationState::Running(operation) => running_screen(ui, operation),
                    OperationState::Succeeded(success) => success_screen(ui, success),
                    OperationState::Failed(failure) => failure_screen(ui, failure),
                };
                if setup {
                    setup_logo(ui);
                } else {
                    operation_logo(ui);
                }
                match action {
                    ScreenAction::None => {}
                    ScreenAction::Cancel => {
                        if let OperationState::Running(running) = &mut self.operation
                            && running.control.cancel().is_ok()
                        {
                            running.cancel_requested = true;
                        }
                    }
                    ScreenAction::Skip => self.overlay = Some(Overlay::SkipConfirmation),
                    ScreenAction::Reset | ScreenAction::Done => self.reset_for_another(),
                    ScreenAction::Back => self.return_to_setup(),
                    ScreenAction::Retry => self.start_operation(),
                }
            });

        match self.overlay {
            Some(Overlay::TargetPicker) => self.show_target_picker(context),
            Some(Overlay::EraseConfirmation) => self.show_erase_confirmation(context),
            Some(Overlay::SkipConfirmation) => self.show_skip_confirmation(context),
            Some(Overlay::Settings) => self.show_settings(context),
            Some(Overlay::ThirdPartyNotices) => self.show_third_party_notices(context),
            Some(Overlay::CloseConfirmation) => self.show_close_confirmation(context),
            None => {}
        }
    }
}

impl SnapDogInstallerApp {
    fn setup_top_bar(&mut self, ui: &egui::Ui) {
        // The icon boxes are taller than the step labels. Moving their boxes up by
        // eight points aligns their optical centres without consuming layout space.
        let top = ui.cursor().top() - 8.0;
        let right = ui.max_rect().right();
        let settings_rect =
            egui::Rect::from_min_size(egui::pos2(right - 40.0, top), Vec2::splat(40.0));
        if toolbar_icon_at(
            ui,
            settings_rect,
            "setup-settings",
            ToolbarIcon::Settings,
            true,
        )
        .on_hover_text("Installer settings")
        .clicked()
        {
            self.overlay = Some(Overlay::Settings);
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.allocate_ui_with_layout(
            Vec2::new(ui.available_width(), 40.0),
            Layout::right_to_left(Align::Center),
            |ui| {
                self.top_bar_controls(ui);
            },
        );
    }

    fn top_bar_controls(&mut self, ui: &mut egui::Ui) {
        ui.spacing_mut().item_spacing.x = 4.0;
        let settings_enabled = matches!(self.operation, OperationState::Idle);
        if toolbar_icon_button(ui, ToolbarIcon::Settings, settings_enabled)
            .on_hover_text("Installer settings")
            .clicked()
            && settings_enabled
        {
            self.overlay = Some(Overlay::Settings);
        }
    }
}

fn paint_apple_switch(ui: &egui::Ui, rect: egui::Rect, response: &egui::Response, selected: bool) {
    let track = egui::Rect::from_center_size(rect.center(), Vec2::new(42.0, 24.0));
    let position = ui.ctx().animate_bool(response.id, selected);
    let track_color = if selected {
        ORANGE
    } else if response.hovered() {
        Color32::from_rgb(104, 100, 97)
    } else {
        Color32::from_rgb(84, 80, 77)
    };
    ui.painter().rect_filled(track, 12.0, track_color);
    ui.painter().rect_stroke(
        track,
        12.0,
        Stroke::new(
            1.0_f32,
            if selected {
                Color32::from_rgb(242, 154, 65)
            } else {
                Color32::from_rgb(130, 126, 122)
            },
        ),
        egui::StrokeKind::Inside,
    );

    let thumb_x = egui::lerp((track.left() + 12.0)..=(track.right() - 12.0), position);
    let thumb_center = egui::pos2(thumb_x, track.center().y);
    ui.painter().circle_filled(
        thumb_center + Vec2::new(0.0, 0.8),
        9.3,
        Color32::from_black_alpha(70),
    );
    ui.painter().circle_filled(thumb_center, 9.3, TEXT);

    if response.has_focus() {
        ui.painter().rect_stroke(
            track.expand(3.0),
            15.0,
            Stroke::new(2.0_f32, BRIGHT_ORANGE),
            egui::StrokeKind::Outside,
        );
    }
}

#[derive(Clone, Copy)]
enum ToolbarIcon {
    Settings,
}

fn toolbar_icon_button(ui: &mut egui::Ui, icon: ToolbarIcon, enabled: bool) -> egui::Response {
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(40.0), sense);
    paint_toolbar_icon(ui, rect, &response, icon, enabled);
    response.on_hover_cursor(if enabled {
        egui::CursorIcon::PointingHand
    } else {
        egui::CursorIcon::Default
    })
}

fn toolbar_icon_at(
    ui: &egui::Ui,
    rect: egui::Rect,
    id: &'static str,
    icon: ToolbarIcon,
    enabled: bool,
) -> egui::Response {
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let response = ui.interact(rect, ui.make_persistent_id(id), sense);
    paint_toolbar_icon(ui, rect, &response, icon, enabled);
    response.on_hover_cursor(if enabled {
        egui::CursorIcon::PointingHand
    } else {
        egui::CursorIcon::Default
    })
}

fn paint_toolbar_icon(
    ui: &egui::Ui,
    rect: egui::Rect,
    response: &egui::Response,
    icon: ToolbarIcon,
    enabled: bool,
) {
    if response.hovered() && enabled {
        ui.painter().circle_filled(rect.center(), 19.0, ELEVATED);
    }
    let color = if enabled { TEXT } else { SUBTLE };
    match icon {
        ToolbarIcon::Settings => {
            let center = rect.center();
            ui.painter().circle_filled(center, 10.2, color);
            for index in 0_u16..8 {
                let angle = f32::from(index) * std::f32::consts::TAU / 8.0;
                let tangent = Vec2::new(-angle.sin(), angle.cos());
                let radial = Vec2::new(angle.cos(), angle.sin());
                let tooth = vec![
                    center + radial * 8.6 + tangent * 2.6,
                    center + radial * 13.0 + tangent * 2.1,
                    center + radial * 13.0 - tangent * 2.1,
                    center + radial * 8.6 - tangent * 2.6,
                ];
                ui.painter()
                    .add(egui::Shape::convex_polygon(tooth, color, Stroke::NONE));
            }
            let cutout = if response.hovered() && enabled {
                ELEVATED
            } else {
                BACKGROUND
            };
            ui.painter().circle_filled(center, 4.2, cutout);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScreenAction {
    None,
    Cancel,
    Skip,
    Reset,
    Back,
    Retry,
    Done,
}

fn running_screen(ui: &mut egui::Ui, running: &RunningOperation) -> ScreenAction {
    let mut action = ScreenAction::None;
    ui.add_space(54.0);
    ui.vertical_centered(|ui| {
        ui.set_width(600.0);
        phase_strip(
            ui,
            running.phase.step(),
            running.verification_enabled && !running.skip_requested,
        );
        ui.add_space(34.0);
        ui.add(
            egui::Image::new(egui::include_image!("../assets/icon.png"))
                .fit_to_exact_size(Vec2::splat(76.0)),
        );
        ui.add_space(18.0);
        ui.label(RichText::new(running.phase.title()).size(27.0).strong());
        ui.add_space(7.0);
        ui.label(RichText::new(running.phase.detail()).color(MUTED));
        ui.add_space(24.0);

        if let Some(fraction) = progress_fraction(running.processed, running.total) {
            let fill = if running.phase == OperationPhase::Verifying {
                GREEN
            } else {
                ORANGE
            };
            ui.add(
                egui::ProgressBar::new(fraction)
                    .desired_width(520.0)
                    .desired_height(12.0)
                    .fill(fill)
                    .corner_radius(6.0),
            );
            ui.add_space(8.0);
            ui.label(
                RichText::new(progress_text(running.processed, running.total))
                    .color(MUTED)
                    .monospace(),
            );
        } else {
            ui.spinner();
        }

        ui.add_space(28.0);
        if running.cancel_requested {
            ui.label(RichText::new("Stopping safely…").color(ORANGE).strong());
        } else if running.skip_requested {
            ui.label(
                RichText::new("Finishing without verification…")
                    .color(ORANGE)
                    .strong(),
            );
        } else {
            ui.horizontal(|ui| {
                let can_cancel = running.phase.can_cancel();
                if ui
                    .add_enabled(
                        can_cancel,
                        secondary_button("Cancel", Vec2::new(190.0, 44.0)),
                    )
                    .clicked()
                {
                    action = ScreenAction::Cancel;
                }
                if running.phase == OperationPhase::Verifying
                    && ui
                        .add(secondary_button(
                            "Skip verification",
                            Vec2::new(190.0, 44.0),
                        ))
                        .clicked()
                {
                    action = ScreenAction::Skip;
                }
            });
        }
    });
    action
}

fn success_screen(ui: &mut egui::Ui, success: &SuccessState) -> ScreenAction {
    let mut reset = false;
    ui.add_space(54.0);
    ui.vertical_centered(|ui| {
        ui.set_width(600.0);
        phase_strip(ui, 5, success.report.verified);
        ui.add_space(30.0);
        success_icon(ui, 62.0);
        ui.label(RichText::new("Flash completed").size(29.0).strong());
        ui.add_space(8.0);
        ui.label(
            RichText::new(if success.report.verified {
                "Verified — the card matches the SnapDog OS release image."
            } else {
                "Verification skipped — the image was written but not read back."
            })
            .color(if success.report.verified {
                GREEN
            } else {
                ORANGE
            }),
        );
        ui.add_space(20.0);
        egui::Frame::new()
            .fill(SURFACE)
            .corner_radius(14.0)
            .inner_margin(egui::Margin::symmetric(20, 14))
            .show(ui, |ui| {
                ui.set_width(480.0);
                summary_row(
                    ui,
                    "Image",
                    &format!("SnapDog OS {}", success.selection.manifest.version),
                );
                summary_row(ui, "Target", &success.drive.label());
                summary_row(ui, "Written", &format_bytes(success.report.raw_size));
                summary_row(ui, "Elapsed", &format_duration(success.elapsed));
            });
        ui.add_space(22.0);
        if ui
            .add(primary_button("Flash another", Vec2::new(260.0, 46.0)))
            .clicked()
        {
            reset = true;
        }
    });
    if reset {
        ScreenAction::Reset
    } else {
        ScreenAction::None
    }
}

fn failure_screen(ui: &mut egui::Ui, failure: &FailureState) -> ScreenAction {
    let mut action = ScreenAction::None;
    let requires_signed_bundle = failure
        .failure
        .message
        .contains("signed SnapDog application bundle");
    let can_retry = !failure.failure.cancelled
        && !requires_signed_bundle
        && matches!(
            failure.phase,
            OperationPhase::Downloading
                | OperationPhase::Decompressing
                | OperationPhase::Authorizing
                | OperationPhase::ValidatingImage
        );
    let eject_only = !failure.failure.cancelled && failure.phase == OperationPhase::Ejecting;
    let (title, guidance) = failure_copy(failure, requires_signed_bundle, eject_only);

    ui.add_space(12.0);
    ui.vertical_centered(|ui| {
        ui.set_width(560.0);
        failure_icon(
            ui,
            if failure.failure.cancelled {
                ORANGE
            } else {
                DANGER
            },
            failure.failure.cancelled,
        );
        ui.add_space(10.0);
        ui.label(RichText::new(title).size(26.0).strong());
        ui.add_space(6.0);
        ui.label(RichText::new(guidance).color(MUTED).size(14.0));
        ui.add_space(16.0);
        egui::Frame::new()
            .fill(SURFACE)
            .stroke(Stroke::new(1.0_f32, Color32::from_gray(60)))
            .corner_radius(14.0)
            .inner_margin(egui::Margin::symmetric(20, 12))
            .show(ui, |ui| {
                ui.set_width(500.0);
                let stage = if failure.phase == OperationPhase::Authorizing {
                    if failure.failure.authorization_denied {
                        "Authorization cancelled"
                    } else {
                        "Starting secure writer"
                    }
                } else {
                    failure.phase.title().trim_end_matches('…')
                };
                summary_row(ui, "Stage", stage);
                summary_row(
                    ui,
                    "Image",
                    &format!(
                        "SnapDog OS {} · {}",
                        failure.selection.manifest.version,
                        failure.selection.board.label()
                    ),
                );
                summary_row(ui, "Target", &failure.drive.label());
                ui.add_space(2.0);
                egui::CollapsingHeader::new(
                    RichText::new("Technical details").color(MUTED).small(),
                )
                .id_salt("operation-failure-details")
                .default_open(
                    failure.phase == OperationPhase::Authorizing
                        && !failure.failure.authorization_denied,
                )
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(&failure.failure.message)
                            .color(SUBTLE)
                            .small()
                            .monospace(),
                    );
                });
            });
        ui.add_space(14.0);
        action = failure_actions(ui, can_retry, eject_only);
    });
    action
}

fn failure_copy(
    failure: &FailureState,
    requires_signed_bundle: bool,
    eject_only: bool,
) -> (&'static str, &'static str) {
    if failure.failure.cancelled {
        let guidance = if matches!(
            failure.phase,
            OperationPhase::Writing
                | OperationPhase::Verifying
                | OperationPhase::Syncing
                | OperationPhase::Ejecting
        ) {
            "The SD card may be incomplete. Flash it again before using it."
        } else {
            "No further changes will be made to the selected SD card."
        };
        return ("Flash cancelled", guidance);
    }
    if requires_signed_bundle {
        return (
            "Open the signed installer app",
            "Raw-device access is available only from the signed SnapDog OS Installer in Applications.",
        );
    }
    if eject_only {
        return (
            "The SD card is ready",
            "The write completed, but the card could not be ejected automatically. Eject it in the operating system before removal.",
        );
    }
    match failure.phase {
        OperationPhase::Downloading => (
            "Couldn’t download SnapDog OS",
            "Check the network connection, then try the download again.",
        ),
        OperationPhase::Decompressing | OperationPhase::ValidatingImage => (
            "The image could not be prepared",
            "The downloaded release did not pass all safety checks. The SD card was not written.",
        ),
        OperationPhase::Authorizing if failure.failure.authorization_denied => (
            "Permission was not granted",
            "Approve the system prompt so the installer can write to the selected SD card.",
        ),
        OperationPhase::Authorizing => (
            "Couldn’t start the secure writer",
            "The protected writing process could not start. No data was written to the SD card.",
        ),
        OperationPhase::ValidatingTarget | OperationPhase::Unmounting => (
            "The target is no longer available",
            "Reconnect the SD card, return to setup, and select it again.",
        ),
        OperationPhase::Writing | OperationPhase::Verifying | OperationPhase::Syncing => (
            "Couldn’t complete the flash",
            "The SD card may be incomplete. Return to setup and flash it again.",
        ),
        OperationPhase::Ejecting => unreachable!("eject failures are handled above"),
    }
}

fn operation_failure(error: &PipelineError) -> OperationFailure {
    let cancelled = matches!(&error, PipelineError::Cancelled);
    let authorization_denied = matches!(
        &error,
        PipelineError::Runner(WorkerRunnerError::AuthorizationDenied)
    );
    OperationFailure {
        message: error.to_string(),
        cancelled,
        authorization_denied,
    }
}

fn failure_icon(ui: &mut egui::Ui, color: Color32, cancelled: bool) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(50.0), egui::Sense::hover());
    let painter = ui.painter();
    painter.circle_stroke(rect.center(), 22.0, Stroke::new(2.5_f32, color));
    if cancelled {
        painter.line_segment(
            [
                rect.center() + Vec2::new(-7.0, -7.0),
                rect.center() + Vec2::new(7.0, 7.0),
            ],
            Stroke::new(2.8_f32, color),
        );
        painter.line_segment(
            [
                rect.center() + Vec2::new(7.0, -7.0),
                rect.center() + Vec2::new(-7.0, 7.0),
            ],
            Stroke::new(2.8_f32, color),
        );
    } else {
        painter.line_segment(
            [
                rect.center() + Vec2::new(0.0, -10.0),
                rect.center() + Vec2::new(0.0, 5.0),
            ],
            Stroke::new(3.0_f32, color),
        );
        painter.circle_filled(rect.center() + Vec2::new(0.0, 12.0), 2.0, color);
    }
}

fn failure_actions(ui: &mut egui::Ui, can_retry: bool, eject_only: bool) -> ScreenAction {
    if eject_only {
        if ui
            .add(primary_button("Done", Vec2::new(260.0, 46.0)))
            .clicked()
        {
            return ScreenAction::Done;
        }
    } else if can_retry {
        let mut action = ScreenAction::None;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            let actions_width = 214.0_f32.mul_add(2.0, ui.spacing().item_spacing.x);
            ui.add_space(((ui.available_width() - actions_width) / 2.0).max(0.0));
            if ui
                .add(secondary_button("Back to setup", Vec2::new(214.0, 44.0)))
                .clicked()
            {
                action = ScreenAction::Back;
            }
            if ui
                .add(primary_button("Retry", Vec2::new(214.0, 44.0)))
                .clicked()
            {
                action = ScreenAction::Retry;
            }
        });
        return action;
    } else if ui
        .add(primary_button("Back to setup", Vec2::new(260.0, 46.0)))
        .clicked()
    {
        return ScreenAction::Back;
    }
    ScreenAction::None
}

fn apply_pipeline_event(running: &mut RunningOperation, event: PipelineEvent) {
    match event {
        PipelineEvent::Download(progress) => {
            running.phase = OperationPhase::Downloading;
            apply_download_progress(running, progress);
        }
        PipelineEvent::Preparing(progress) => apply_flash_progress(running, progress),
        PipelineEvent::AwaitingAuthorization => {
            running.phase = OperationPhase::Authorizing;
            running.processed = None;
            running.total = None;
        }
        PipelineEvent::Worker(progress) => {
            // Byte-level progress can arrive for every MiB of a multi-gigabyte image. The UI still
            // consumes every update, but the diagnostic log records phase transitions and messages
            // rather than turning image preparation into thousands of synchronous file writes.
            if progress.bytes_processed.is_none() || progress.message.is_some() {
                tracing::debug!(
                    phase = ?progress.phase,
                    processed = ?progress.bytes_processed,
                    total = ?progress.total_bytes,
                    message = progress.message.as_deref().unwrap_or(""),
                    "received isolated writer progress"
                );
            }
            apply_worker_progress(running, &progress);
        }
    }
}

const fn apply_download_progress(running: &mut RunningOperation, progress: DownloadProgress) {
    running.processed = Some(progress.downloaded);
    running.total = progress.total;
}

const fn apply_flash_progress(running: &mut RunningOperation, progress: FlashProgress) {
    running.phase = match progress.stage {
        FlashStage::Decompressing => OperationPhase::Decompressing,
        FlashStage::Writing => OperationPhase::Writing,
        FlashStage::Verifying => OperationPhase::Verifying,
    };
    running.processed = Some(progress.processed);
    running.total = progress.total;
}

const fn apply_worker_progress(running: &mut RunningOperation, progress: &WorkerProgress) {
    running.phase = match progress.phase {
        WorkerPhase::ValidatingImage => OperationPhase::ValidatingImage,
        WorkerPhase::ValidatingTarget => OperationPhase::ValidatingTarget,
        WorkerPhase::Unmounting => OperationPhase::Unmounting,
        WorkerPhase::Writing => OperationPhase::Writing,
        WorkerPhase::Verifying => OperationPhase::Verifying,
        WorkerPhase::Syncing => OperationPhase::Syncing,
        WorkerPhase::Ejecting | WorkerPhase::Completed => OperationPhase::Ejecting,
        WorkerPhase::Cancelled | WorkerPhase::Failed => running.phase,
    };
    running.processed = progress.bytes_processed;
    running.total = progress.total_bytes;
}

#[cfg(target_os = "macos")]
fn execute_pipeline<F>(
    request: &PipelineRequest,
    control: &PipelineControl,
    emit: F,
) -> Result<PipelineReport, PipelineError>
where
    F: FnMut(PipelineEvent),
{
    let runner = MacOsWorkerRunner::current()?;
    runner.prime_removable_volume_access(&request.drive.device)?;
    let downloader = DownloadClient::new()?;
    run_pipeline(request, &downloader, &runner, control, emit)
}

#[cfg(target_os = "linux")]
fn execute_pipeline<F>(
    request: &PipelineRequest,
    control: &PipelineControl,
    emit: F,
) -> Result<PipelineReport, PipelineError>
where
    F: FnMut(PipelineEvent),
{
    let downloader = DownloadClient::new()?;
    let runner = LinuxWorkerRunner::current()?;
    run_pipeline(request, &downloader, &runner, control, emit)
}

#[cfg(target_os = "windows")]
fn execute_pipeline<F>(
    request: &PipelineRequest,
    control: &PipelineControl,
    emit: F,
) -> Result<PipelineReport, PipelineError>
where
    F: FnMut(PipelineEvent),
{
    let downloader = DownloadClient::new()?;
    let runner = WindowsWorkerRunner::current()?;
    run_pipeline(request, &downloader, &runner, control, emit)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn execute_pipeline<F>(
    _request: &PipelineRequest,
    _control: &PipelineControl,
    _emit: F,
) -> Result<PipelineReport, PipelineError>
where
    F: FnMut(PipelineEvent),
{
    Err(crate::pipeline::WorkerRunnerError::Unsupported.into())
}

const fn supports_manifest_schema(schema_version: Option<u32>) -> bool {
    matches!(schema_version, Some(2))
}

fn validate_release_image(manifest: &Manifest, board: Board) -> Result<(), String> {
    if !supports_manifest_schema(manifest.schema_version) {
        return Err(PENDING_RELEASE_METADATA.to_owned());
    }
    let image = manifest
        .image_for(board)
        .ok_or_else(|| "This release has no image for the selected Raspberry Pi".to_owned())?;
    if image.url.is_none() {
        return Err("Release metadata is missing the immutable image URL".to_owned());
    }
    required_hash(image.sha256.as_deref(), "compressed image")?;
    required_hash(image.raw_sha256.as_deref(), "raw image")?;
    required_size(image.compressed_size, "compressed image")?;
    required_size(image.uncompressed_size, "raw image")?;
    Ok(())
}

fn required_hash(value: Option<&str>, name: &str) -> Result<String, String> {
    let value = value.ok_or_else(|| format!("Release metadata is missing the {name} checksum"))?;
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("Release metadata has an invalid {name} checksum"));
    }
    Ok(value.to_owned())
}

fn required_size(value: Option<u64>, name: &str) -> Result<u64, String> {
    value
        .filter(|size| *size > 0)
        .ok_or_else(|| format!("Release metadata is missing the {name} size"))
}

fn configure_style(context: &egui::Context) {
    let mut style = (*context.style()).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.panel_fill = BACKGROUND;
    style.visuals.window_fill = SURFACE;
    style.visuals.selection.bg_fill = ORANGE;
    style.visuals.widgets.active.bg_fill = ORANGE;
    style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(240, 154, 63);
    style.visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    style.spacing.button_padding = Vec2::new(12.0, 7.0);
    context.set_style(style);
}

fn step_title(ui: &mut egui::Ui, title: &str, active: bool) {
    ui.label(
        RichText::new(title)
            .size(17.0)
            .strong()
            .color(if active { TEXT } else { MUTED }),
    );
}

fn connector(ui: &mut egui::Ui, enabled: bool, height: f32) {
    ui.allocate_ui_with_layout(
        Vec2::new(WORKFLOW_CONNECTOR_WIDTH, height),
        Layout::top_down(Align::Center),
        |ui| {
            ui.add_space(STEP_ICON_CENTER_Y - 6.0);
            let (rect, _) = ui.allocate_exact_size(
                Vec2::new(WORKFLOW_CONNECTOR_WIDTH, 12.0),
                egui::Sense::hover(),
            );
            let color = if enabled {
                ORANGE
            } else {
                Color32::from_gray(66)
            };
            ui.painter().line_segment(
                [
                    rect.left_center(),
                    rect.right_center() - Vec2::new(8.0, 0.0),
                ],
                Stroke::new(1.5_f32, color),
            );
            let tip = rect.right_center();
            ui.painter().line_segment(
                [tip - Vec2::new(8.0, 5.0), tip],
                Stroke::new(1.5_f32, color),
            );
            ui.painter().line_segment(
                [tip, tip - Vec2::new(8.0, -5.0)],
                Stroke::new(1.5_f32, color),
            );
        },
    );
}

fn board_button(ui: &mut egui::Ui, board: Board, selected: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(144.0, 134.0), egui::Sense::click());
    let center = rect.center_top() + Vec2::new(0.0, 53.0);
    let radius = 51.0;
    let fill = Color32::from_rgb(255, 240, 212);
    if selected {
        ui.painter().circle_filled(
            center,
            radius + 6.0,
            Color32::from_rgba_premultiplied(255, 159, 10, 32),
        );
    }
    ui.painter().circle_filled(center, radius, fill);
    ui.painter().circle_stroke(
        center,
        radius,
        Stroke::new(
            if selected { 3.2_f32 } else { 2.0_f32 },
            if selected {
                BRIGHT_ORANGE
            } else {
                Color32::from_gray(214)
            },
        ),
    );

    let image_rect = egui::Rect::from_center_size(center, Vec2::splat(88.0));
    egui::Image::new(board_image(board))
        .fit_to_exact_size(image_rect.size())
        .paint_at(ui, image_rect);

    if selected {
        let badge = center + Vec2::new(40.0, -39.0);
        ui.painter().circle_filled(badge, 15.0, BRIGHT_ORANGE);
        ui.painter()
            .circle_stroke(badge, 15.0, Stroke::new(3.0_f32, BACKGROUND));
        paint_checkmark(ui.painter(), badge, 15.0, Color32::BLACK, 2.4);
    }

    ui.painter().text(
        egui::pos2(rect.center().x, rect.bottom() - 7.0),
        egui::Align2::CENTER_BOTTOM,
        board.label(),
        FontId::proportional(12.0),
        TEXT,
    );

    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

const fn board_image(board: Board) -> egui::ImageSource<'static> {
    match board {
        Board::Pi5 => egui::include_image!("../assets/rpi/pi5.png"),
        Board::Pi4 => egui::include_image!("../assets/rpi/pi4.png"),
        Board::Pi3 => egui::include_image!("../assets/rpi/pi3.png"),
        Board::Zero2W => egui::include_image!("../assets/rpi/zero2w.png"),
    }
}

fn paint_checkmark(
    painter: &egui::Painter,
    center: egui::Pos2,
    size: f32,
    color: Color32,
    width: f32,
) {
    painter.line_segment(
        [
            center + Vec2::new(-size * 0.34, 0.0),
            center + Vec2::new(-size * 0.08, size * 0.25),
        ],
        Stroke::new(width, color),
    );
    painter.line_segment(
        [
            center + Vec2::new(-size * 0.08, size * 0.25),
            center + Vec2::new(size * 0.38, -size * 0.30),
        ],
        Stroke::new(width, color),
    );
}

fn success_icon(ui: &mut egui::Ui, size: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::hover());
    ui.painter()
        .circle_filled(rect.center(), size * 0.36, GREEN);
    paint_checkmark(
        ui.painter(),
        rect.center(),
        size * 0.34,
        Color32::BLACK,
        3.2,
    );
}

fn drive_icon(ui: &mut egui::Ui, color: Color32, size: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::hover());
    let center = rect.center();
    let body = egui::Rect::from_center_size(
        center + Vec2::new(0.0, -1.0),
        Vec2::new(size * 0.58, size * 0.68),
    );
    ui.painter().rect_filled(body, 6.0, ELEVATED);
    ui.painter().rect_stroke(
        body,
        6.0,
        Stroke::new(2.6_f32, color),
        egui::StrokeKind::Inside,
    );
    let baseline_y = size.mul_add(-0.15, body.bottom());
    ui.painter().line_segment(
        [
            egui::pos2(size.mul_add(0.10, body.left()), baseline_y),
            egui::pos2(size.mul_add(-0.10, body.right()), baseline_y),
        ],
        Stroke::new(2.0_f32, color),
    );
    ui.painter().circle_filled(
        egui::pos2(
            size.mul_add(0.13, body.left()),
            size.mul_add(-0.08, body.bottom()),
        ),
        size * 0.025,
        color,
    );
}

fn lightning_icon(ui: &mut egui::Ui, color: Color32, size: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::hover());
    let center = rect.center();
    let points = [
        center + Vec2::new(size * 0.03, -size * 0.43),
        center + Vec2::new(-size * 0.28, size * 0.02),
        center + Vec2::new(-size * 0.04, size * 0.02),
        center + Vec2::new(-size * 0.12, size * 0.43),
        center + Vec2::new(size * 0.30, -size * 0.12),
        center + Vec2::new(size * 0.06, -size * 0.12),
    ];
    ui.painter().add(egui::Shape::convex_polygon(
        points.to_vec(),
        color,
        Stroke::NONE,
    ));
}

fn setup_action_button(label: &str, size: Vec2, enabled: bool) -> egui::Button<'_> {
    egui::Button::new(
        RichText::new(label)
            .color(if enabled { Color32::BLACK } else { TEXT })
            .strong(),
    )
    .fill(if enabled { ORANGE } else { ELEVATED })
    .stroke(Stroke::new(
        1.0_f32,
        if enabled {
            Color32::from_rgb(242, 154, 65)
        } else {
            Color32::from_gray(70)
        },
    ))
    .corner_radius(size.y / 2.0)
    .min_size(size)
}

fn phase_strip(ui: &mut egui::Ui, current: usize, verification_expected: bool) {
    const PHASES: [&str; 5] = ["Download", "Prepare", "Write", "Verify", "Finish"];
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        for (index, label) in PHASES.into_iter().enumerate() {
            if index > 0 {
                ui.label(RichText::new("—").color(if index <= current { ORANGE } else { MUTED }));
            }
            let verification_skipped = index == 3 && !verification_expected;
            let color = if verification_skipped {
                MUTED
            } else {
                match index.cmp(&current) {
                    std::cmp::Ordering::Less => GREEN,
                    std::cmp::Ordering::Equal => ORANGE,
                    std::cmp::Ordering::Greater => MUTED,
                }
            };
            let label = if verification_skipped {
                "Verify skipped"
            } else {
                label
            };
            ui.label(RichText::new(label).color(color).strong());
        }
    });
}

fn primary_button(label: &str, size: Vec2) -> egui::Button<'_> {
    egui::Button::new(RichText::new(label).color(Color32::BLACK).strong())
        .fill(ORANGE)
        .corner_radius(size.y / 2.0)
        .min_size(size)
}

fn secondary_button(label: &str, size: Vec2) -> egui::Button<'_> {
    egui::Button::new(RichText::new(label).color(TEXT).strong())
        .fill(SURFACE)
        .stroke(Stroke::new(1.0_f32, Color32::from_gray(100)))
        .corner_radius(size.y / 2.0)
        .min_size(size)
}

fn branded_modal<T>(
    context: &egui::Context,
    id: &str,
    body: impl FnOnce(&mut egui::Ui) -> T,
) -> egui::ModalResponse<T> {
    egui::Modal::new(egui::Id::new(id))
        .backdrop_color(Color32::from_black_alpha(170))
        .frame(
            egui::Frame::new()
                .fill(SURFACE)
                .stroke(Stroke::new(1.0_f32, Color32::from_gray(76)))
                .corner_radius(18.0)
                .inner_margin(egui::Margin::same(24)),
        )
        .show(context, body)
}

fn summary_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).color(MUTED));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(value).color(TEXT).strong());
        });
    });
    ui.add_space(5.0);
}

fn third_party_license_card(ui: &mut egui::Ui, index: usize, license: &ThirdPartyLicenseGroup) {
    egui::Frame::new()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0_f32, Color32::from_gray(58)))
        .corner_radius(12.0)
        .inner_margin(egui::Margin::symmetric(14, 11))
        .show(ui, |ui| {
            ui.set_width(610.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(&license.title).strong());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let count = license.packages.len();
                    ui.label(
                        RichText::new(format!(
                            "{count} {}",
                            if count == 1 { "library" } else { "libraries" }
                        ))
                        .color(MUTED)
                        .small(),
                    );
                });
            });
            ui.add_space(4.0);
            ui.label(
                RichText::new(license.packages.join(", "))
                    .color(MUTED)
                    .small(),
            );
            ui.add_space(4.0);
            egui::CollapsingHeader::new(
                RichText::new("License text and notices")
                    .color(ORANGE)
                    .small(),
            )
            .id_salt(("third-party-license", index))
            .default_open(false)
            .show(ui, |ui| {
                ui.add_space(4.0);
                for (notice_index, notice) in license.notices.iter().enumerate() {
                    if license.notices.len() > 1 {
                        ui.label(
                            RichText::new(format!("Notice {}", notice_index + 1))
                                .color(TEXT)
                                .strong()
                                .small(),
                        );
                        ui.add_space(3.0);
                    }
                    ui.add(
                        egui::Label::new(RichText::new(notice).color(MUTED).size(11.0))
                            .wrap()
                            .selectable(true),
                    );
                    if notice_index + 1 < license.notices.len() {
                        ui.separator();
                    }
                }
            });
        });
}

fn third_party_license_groups(notices: &str) -> Vec<ThirdPartyLicenseGroup> {
    let notices = notices.replace("\r\n", "\n");
    let mut groups: Vec<ThirdPartyLicenseGroup> = Vec::new();
    for section in notices
        .split("================================================================================")
    {
        let section = section.trim();
        let Some((title, body)) = section.split_once("\n\nUsed by:\n") else {
            continue;
        };
        let Some((packages, notice)) = body.split_once("\n\n") else {
            continue;
        };
        let title = decode_notice_title(title.trim());
        let packages = packages
            .lines()
            .filter_map(|line| line.trim().strip_prefix("- "))
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if packages.is_empty() || notice.trim().is_empty() {
            continue;
        }
        let notice = notice.trim().to_owned();
        if let Some(group) = groups.iter_mut().find(|group| group.title == title) {
            group.packages.extend(packages);
            if !group.notices.contains(&notice) {
                group.notices.push(notice);
            }
        } else {
            groups.push(ThirdPartyLicenseGroup {
                title,
                packages,
                notices: vec![notice],
            });
        }
    }
    for group in &mut groups {
        group.packages.sort();
        group.packages.dedup();
    }
    groups
}

fn decode_notice_title(title: &str) -> String {
    title
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
        .replace("&#x27;", "'")
}

fn setup_logo(ui: &egui::Ui) {
    let bounds = ui.max_rect();
    let size = snapdog_logo_size(210.0);
    let content_left = bounds.left() + setup_content_left_inset(bounds.width());
    let second_connector_center_x = content_left
        + SETUP_SOURCE_WIDTH
        + WORKFLOW_CONNECTOR_WIDTH
        + SETUP_STEP_WIDTH
        + WORKFLOW_CONNECTOR_WIDTH / 2.0;
    let center = egui::pos2(
        second_connector_center_x,
        bounds.bottom() - 64.0 - size.y / 2.0,
    );
    snapdog_logo_at(ui, egui::Rect::from_center_size(center, size), "setup-logo");
}

fn setup_content_left_inset(available_width: f32) -> f32 {
    ((available_width - SETUP_CONTENT_WIDTH) / 2.0 - WORKFLOW_OPTICAL_LEFT_SHIFT).max(0.0)
}

fn operation_logo(ui: &egui::Ui) {
    let bounds = ui.max_rect();
    let size = snapdog_logo_size(96.0);
    let center = egui::pos2(
        bounds.right() - 24.0 - size.x / 2.0,
        bounds.bottom() - 18.0 - size.y / 2.0,
    );
    snapdog_logo_at(
        ui,
        egui::Rect::from_center_size(center, size),
        "operation-logo",
    );
}

fn snapdog_logo_size(width: f32) -> Vec2 {
    Vec2::new(width, width / SNAPDOG_LOGO_ASPECT_RATIO)
}

fn snapdog_logo_at(ui: &egui::Ui, rect: egui::Rect, id: &str) {
    let response = ui.interact(rect, ui.make_persistent_id(id), egui::Sense::click());
    egui::Image::new(egui::include_image!("../assets/snapdog-logo.svg"))
        .fit_to_exact_size(rect.size())
        .paint_at(ui, rect);
    if response.on_hover_text("Open snapdog.cc").clicked() {
        ui.ctx()
            .open_url(egui::OpenUrl::new_tab("https://snapdog.cc"));
    }
}

fn progress_fraction(processed: Option<u64>, total: Option<u64>) -> Option<f32> {
    match (processed, total) {
        (Some(processed), Some(total)) if total > 0 => {
            let permille = (u128::from(processed) * 1_000 / u128::from(total)).min(1_000);
            let permille = u16::try_from(permille).expect("bounded progress fits u16");
            Some(f32::from(permille) / 1_000.0)
        }
        _ => None,
    }
}

fn progress_text(processed: Option<u64>, total: Option<u64>) -> String {
    match (processed, total) {
        (Some(processed), Some(total)) => {
            let percent = u128::from(processed)
                .saturating_mul(100)
                .checked_div(u128::from(total))
                .unwrap_or(0);
            format!(
                "{} / {}  •  {percent}%",
                format_bytes(processed),
                format_bytes(total)
            )
        }
        (Some(processed), None) => format_bytes(processed),
        _ => String::new(),
    }
}

fn format_bytes(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    if bytes >= GIB {
        let whole = bytes / GIB;
        let hundredths = (bytes % GIB) * 100 / GIB;
        format!("{whole}.{hundredths:02} GiB")
    } else {
        let whole = bytes / MIB;
        let tenths = (bytes % MIB) * 10 / MIB;
        format!("{whole}.{tenths} MiB")
    }
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::model::ImageInfo;

    fn release_manifest(schema_version: Option<u32>) -> Manifest {
        Manifest {
            schema_version,
            channel: Channel::Release,
            version: "0.13.0".to_owned(),
            commit: Some("abc123".to_owned()),
            date: "2026-07-19T00:00:00Z".to_owned(),
            boards: BTreeMap::from([(
                Board::Pi4.id().to_owned(),
                ImageInfo {
                    image: "snapdog-os-pi4-release.img.gz".to_owned(),
                    sha256: Some("a".repeat(64)),
                    url: Some(
                        "https://updates.snapdog.cc/os/images/snapdog-os-pi4-0.13.0.img.gz"
                            .to_owned(),
                    ),
                    compressed_size: Some(42),
                    uncompressed_size: Some(84),
                    raw_sha256: Some("b".repeat(64)),
                },
            )]),
        }
    }

    #[test]
    fn validates_integrity_metadata_helpers() {
        assert!(supports_manifest_schema(Some(2)));
        assert!(!supports_manifest_schema(None));
        assert!(!supports_manifest_schema(Some(1)));
        assert!(!supports_manifest_schema(Some(3)));
        assert!(required_hash(Some(&"a".repeat(64)), "image").is_ok());
        assert!(required_hash(Some("nope"), "image").is_err());
        assert_eq!(required_size(Some(42), "image").unwrap(), 42);
        assert!(required_size(Some(0), "image").is_err());
    }

    #[test]
    fn release_selection_fails_closed_until_manifest_v2_is_complete() {
        assert!(validate_release_image(&release_manifest(Some(2)), Board::Pi4).is_ok());

        let legacy = release_manifest(None);
        assert!(validate_release_image(&legacy, Board::Pi4).is_err());

        let mut incomplete = release_manifest(Some(2));
        incomplete
            .boards
            .get_mut(Board::Pi4.id())
            .unwrap()
            .raw_sha256 = None;
        assert!(validate_release_image(&incomplete, Board::Pi4).is_err());
    }

    #[test]
    fn progress_is_bounded() {
        assert_eq!(progress_fraction(Some(50), Some(100)), Some(0.5));
        assert_eq!(progress_fraction(Some(200), Some(100)), Some(1.0));
        assert_eq!(progress_fraction(Some(1), None), None);
    }

    #[test]
    fn distinguishes_denied_authorization_from_an_early_helper_failure() {
        let denied = operation_failure(&PipelineError::Runner(
            WorkerRunnerError::AuthorizationDenied,
        ));
        assert!(denied.authorization_denied);
        assert!(!denied.cancelled);

        let helper_failure = operation_failure(&PipelineError::Runner(WorkerRunnerError::Failed {
            status: "exit status: 1".to_owned(),
            message: "signed helper failed before its first event".to_owned(),
        }));
        assert!(!helper_failure.authorization_denied);
        assert!(!helper_failure.cancelled);
    }

    #[test]
    fn acknowledgements_group_dependencies_and_embed_all_license_texts() {
        let licenses = third_party_license_groups(THIRD_PARTY_NOTICES);
        assert!(!licenses.is_empty());
        let gpl = licenses
            .iter()
            .find(|license| license.title.contains("GPL-3.0-only"))
            .expect("the application GPL entry remains visible");
        assert!(!gpl.notices.is_empty());
        assert!(
            gpl.notices
                .iter()
                .any(|notice| notice.contains("GNU GENERAL PUBLIC LICENSE"))
        );
        assert!(
            gpl.packages
                .iter()
                .any(|package| package.starts_with("snapdog-os-installer "))
        );
        assert!(
            licenses
                .iter()
                .any(|license| license.title.contains("MIT") && !license.packages.is_empty())
        );
        assert_eq!(decode_notice_title("BSD &quot;New&quot;"), "BSD \"New\"");
    }

    #[test]
    fn acknowledgements_accept_windows_line_endings() {
        let normalized_notices = THIRD_PARTY_NOTICES.replace("\r\n", "\n");
        let windows_notices = normalized_notices.replace('\n', "\r\n");
        assert_eq!(
            third_party_license_groups(&windows_notices).len(),
            third_party_license_groups(&normalized_notices).len()
        );
    }
}
