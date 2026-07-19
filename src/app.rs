// SPDX-License-Identifier: GPL-3.0-only

use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use eframe::egui::{self, Color32, FontId, RichText, Stroke, Vec2};

use crate::catalog::CatalogClient;
use crate::drives;
use crate::model::{Board, Channel, Drive, ImageSelection, Manifest};

const BACKGROUND: Color32 = Color32::from_rgb(28, 25, 23);
const SURFACE: Color32 = Color32::from_rgb(36, 33, 31);
const ORANGE: Color32 = Color32::from_rgb(225, 136, 46);
const BRIGHT_ORANGE: Color32 = Color32::from_rgb(255, 159, 10);
const TEXT: Color32 = Color32::from_rgb(242, 242, 242);
const MUTED: Color32 = Color32::from_rgb(132, 128, 125);

enum CatalogEvent {
    Loaded(Channel, Manifest),
    Failed(String),
}

/// `SnapDog`'s guided three-step installer interface.
pub struct SnapDogInstallerApp {
    catalog_rx: Receiver<CatalogEvent>,
    stable: Option<Manifest>,
    beta: Option<Manifest>,
    catalog_error: Option<String>,
    board: Option<Board>,
    channel: Channel,
    confirmed: Option<ImageSelection>,
    drives: Vec<Drive>,
    selected_drive: Option<Drive>,
    drive_status: Option<String>,
}

impl SnapDogInstallerApp {
    /// Initialize image loaders, theme, and asynchronous catalog loading.
    pub fn new(context: &eframe::CreationContext<'_>) -> Self {
        egui_extras::install_image_loaders(&context.egui_ctx);
        configure_style(&context.egui_ctx);

        let (catalog_tx, catalog_rx) = mpsc::channel();
        thread::spawn(move || match CatalogClient::new() {
            Ok(client) => {
                for channel in [Channel::Release, Channel::Beta] {
                    let event = match client.fetch_latest(channel) {
                        Ok(manifest) => CatalogEvent::Loaded(channel, manifest),
                        Err(error) => CatalogEvent::Failed(error.to_string()),
                    };
                    if catalog_tx.send(event).is_err() {
                        return;
                    }
                }
            }
            Err(error) => {
                let _ = catalog_tx.send(CatalogEvent::Failed(error.to_string()));
            }
        });

        Self {
            catalog_rx,
            stable: None,
            beta: None,
            catalog_error: None,
            board: None,
            channel: Channel::Release,
            confirmed: None,
            drives: Vec::new(),
            selected_drive: None,
            drive_status: None,
        }
    }

    fn receive_catalog(&mut self) {
        while let Ok(event) = self.catalog_rx.try_recv() {
            match event {
                CatalogEvent::Loaded(Channel::Release, manifest) => self.stable = Some(manifest),
                CatalogEvent::Loaded(Channel::Beta, manifest) => self.beta = Some(manifest),
                CatalogEvent::Failed(error) => self.catalog_error = Some(error),
            }
        }
    }

    const fn selected_manifest(&self) -> Option<&Manifest> {
        match self.channel {
            Channel::Release => self.stable.as_ref(),
            Channel::Beta => self.beta.as_ref(),
        }
    }

    fn source_step(&mut self, ui: &mut egui::Ui) {
        ui.set_width(390.0);
        ui.vertical_centered(|ui| {
            step_title(ui, "1. Choose image");
            ui.add_space(18.0);
            ui.label(
                RichText::new("Choose your Raspberry Pi")
                    .size(17.0)
                    .strong(),
            );
            ui.add_space(14.0);

            egui::Grid::new("board-grid")
                .num_columns(2)
                .spacing([18.0, 12.0])
                .show(ui, |ui| {
                    for (index, board) in Board::ALL.into_iter().enumerate() {
                        let selected = self.board == Some(board);
                        if board_button(ui, board, selected).clicked() {
                            self.board = Some(board);
                            self.confirmed = None;
                        }
                        if index % 2 == 1 {
                            ui.end_row();
                        }
                    }
                });

            ui.add_space(12.0);
            ui.label(RichText::new("Choose your version").size(17.0).strong());
            ui.add_space(8.0);

            ui.add_enabled_ui(self.board.is_some(), |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    for channel in [Channel::Release, Channel::Beta] {
                        let selected = self.channel == channel;
                        let button = egui::Button::new(
                            RichText::new(channel.label())
                                .color(if selected { Color32::BLACK } else { MUTED })
                                .strong(),
                        )
                        .fill(if selected { Color32::WHITE } else { SURFACE })
                        .stroke(Stroke::new(1.0, Color32::from_gray(82)))
                        .corner_radius(17.0)
                        .min_size(Vec2::new(96.0, 34.0));
                        if ui.add(button).clicked() {
                            self.channel = channel;
                            self.confirmed = None;
                        }
                    }
                });
            });
            ui.add_space(8.0);

            let version = self.selected_manifest().map_or_else(
                || "Loading releases…".to_owned(),
                |manifest| manifest.version.clone(),
            );
            ui.add_enabled_ui(
                self.board.is_some() && self.selected_manifest().is_some(),
                |ui| {
                    egui::ComboBox::from_id_salt("version")
                        .width(200.0)
                        .selected_text(format!("{version} — Latest"))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut (), (), format!("{version} — Latest"));
                        });
                },
            );
            ui.add_space(8.0);

            let can_confirm = self.board.is_some() && self.selected_manifest().is_some();
            let label = self.confirmed.as_ref().map_or_else(
                || format!("Use SnapDog OS {version}"),
                |selection| format!("SnapDog OS {} selected ✓", selection.manifest.version),
            );
            let button = egui::Button::new(RichText::new(label).color(Color32::BLACK).strong())
                .fill(ORANGE)
                .corner_radius(18.0)
                .min_size(Vec2::new(220.0, 38.0));
            if ui.add_enabled(can_confirm, button).clicked() {
                self.confirm_selection();
            }

            if self.channel == Channel::Beta {
                ui.add_space(4.0);
                ui.label(
                    RichText::new("Preview build — expect rough edges.")
                        .color(ORANGE)
                        .small(),
                );
            }
            if let Some(error) = &self.catalog_error {
                ui.add_space(4.0);
                ui.label(RichText::new(error).color(Color32::LIGHT_RED).small());
            }
        });
    }

    fn confirm_selection(&mut self) {
        let (Some(board), Some(manifest)) = (self.board, self.selected_manifest().cloned()) else {
            return;
        };
        let Some(image) = manifest.image_for(board) else {
            self.catalog_error = Some(format!(
                "SnapDog OS {} has no image for {}",
                manifest.version,
                board.label()
            ));
            return;
        };
        match CatalogClient::image_url(&image.image) {
            Ok(url) => {
                self.confirmed = Some(ImageSelection {
                    board,
                    manifest,
                    url,
                });
                self.catalog_error = None;
            }
            Err(error) => self.catalog_error = Some(error.to_string()),
        }
    }

    fn target_step(&mut self, ui: &mut egui::Ui) {
        ui.set_width(220.0);
        ui.vertical_centered(|ui| {
            step_title(ui, "2. Target");
            ui.add_space(134.0);
            ui.label(RichText::new("▰").size(48.0).color(MUTED));
            ui.add_space(24.0);
            let ready = self.confirmed.is_some();
            let label = self
                .selected_drive
                .as_ref()
                .map_or("Select target", |_| "Change target");
            let button = egui::Button::new(RichText::new(label).strong())
                .fill(if ready {
                    ORANGE
                } else {
                    Color32::from_rgb(72, 68, 65)
                })
                .corner_radius(22.0)
                .min_size(Vec2::new(200.0, 46.0));
            if ui.add_enabled(ready, button).clicked() {
                self.refresh_drives();
            }
            ui.add_space(8.0);
            if !self.drives.is_empty() {
                let selected = self
                    .selected_drive
                    .as_ref()
                    .map_or_else(|| "Choose a removable drive".to_owned(), Drive::label);
                egui::ComboBox::from_id_salt("target-drive")
                    .width(200.0)
                    .selected_text(selected)
                    .show_ui(ui, |ui| {
                        for drive in &self.drives {
                            ui.selectable_value(
                                &mut self.selected_drive,
                                Some(drive.clone()),
                                drive.label(),
                            );
                        }
                    });
            }
            let status = if ready {
                self.drive_status.as_deref().unwrap_or(
                    "Only external physical drives are shown. System drives are excluded.",
                )
            } else {
                "Choose an image first"
            };
            ui.label(RichText::new(status).color(MUTED).small());
        });
    }

    fn refresh_drives(&mut self) {
        match drives::removable_drives() {
            Ok(found) => {
                self.drives = found;
                if self.selected_drive.as_ref().is_some_and(|selected| {
                    !self.drives.iter().any(|drive| drive.id == selected.id)
                }) {
                    self.selected_drive = None;
                }
                self.drive_status = Some(if self.drives.is_empty() {
                    "No removable physical drive found. Insert an SD card and try again.".to_owned()
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

    fn flash_step(&self, ui: &mut egui::Ui) {
        ui.set_width(220.0);
        ui.vertical_centered(|ui| {
            step_title(ui, "3. Flash");
            ui.add_space(130.0);
            ui.label(RichText::new("ϟ").size(58.0).color(MUTED));
            ui.add_space(20.0);
            let ready = self.selected_drive.is_some();
            let button = egui::Button::new(RichText::new("Flash!").strong())
                .fill(if ready {
                    ORANGE
                } else {
                    Color32::from_rgb(72, 68, 65)
                })
                .corner_radius(22.0)
                .min_size(Vec2::new(200.0, 46.0));
            ui.add_enabled(false, button);
            ui.add_space(8.0);
            ui.label(
                RichText::new(if ready {
                    "Privileged writer is intentionally disabled in this local milestone"
                } else {
                    "Select a target first"
                })
                .color(MUTED)
                .small(),
            );
        });
    }
}

impl eframe::App for SnapDogInstallerApp {
    fn update(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.receive_catalog();
        if self.stable.is_none() || self.beta.is_none() {
            context.request_repaint_after(Duration::from_millis(100));
        }

        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(BACKGROUND)
                    .inner_margin(egui::Margin::same(24)),
            )
            .show(context, |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(ui.available_width() - 92.0);
                    ui.label(RichText::new("⚙").size(24.0).color(TEXT));
                    if ui
                        .add(egui::Button::new(RichText::new("?").size(19.0).strong()).frame(false))
                        .on_hover_text("Open SnapDog help")
                        .clicked()
                    {
                        context.open_url(egui::OpenUrl::new_tab("https://snapdog.cc"));
                    }
                });
                ui.add_space(12.0);

                ui.horizontal_top(|ui| {
                    self.source_step(ui);
                    connector(ui, self.confirmed.is_some());
                    self.target_step(ui);
                    connector(ui, self.selected_drive.is_some());
                    self.flash_step(ui);
                });

                let logo_size = Vec2::new(250.0, 120.0);
                let logo_x = ui.available_width() - 410.0;
                ui.horizontal(|ui| {
                    ui.add_space(logo_x.max(0.0));
                    let response = ui.add(
                        egui::Image::new(egui::include_image!("../assets/snapdog-logo.svg"))
                            .fit_to_exact_size(logo_size)
                            .sense(egui::Sense::click()),
                    );
                    if response.on_hover_text("Open snapdog.cc").clicked() {
                        context.open_url(egui::OpenUrl::new_tab("https://snapdog.cc"));
                    }
                });
            });
    }
}

fn configure_style(context: &egui::Context) {
    let mut style = (*context.style()).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.panel_fill = BACKGROUND;
    style.visuals.window_fill = SURFACE;
    style.visuals.selection.bg_fill = ORANGE;
    style.visuals.widgets.active.bg_fill = ORANGE;
    style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(240, 154, 63);
    style.visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    style.spacing.button_padding = Vec2::new(12.0, 7.0);
    context.set_style(style);
}

fn step_title(ui: &mut egui::Ui, title: &str) {
    ui.label(RichText::new(title).size(17.0).strong().color(TEXT));
}

fn connector(ui: &mut egui::Ui, enabled: bool) {
    ui.set_width(55.0);
    ui.add_space(188.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(55.0, 12.0), egui::Sense::hover());
    let color = if enabled {
        Color32::from_gray(130)
    } else {
        Color32::from_gray(55)
    };
    ui.painter().line_segment(
        [
            rect.left_center(),
            rect.right_center() - Vec2::new(6.0, 0.0),
        ],
        Stroke::new(1.0, color),
    );
    ui.painter().text(
        rect.right_center(),
        egui::Align2::RIGHT_CENTER,
        "›",
        FontId::proportional(19.0),
        color,
    );
}

fn board_button(ui: &mut egui::Ui, board: Board, selected: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(150.0, 142.0), egui::Sense::click());
    let center = rect.center_top() + Vec2::new(0.0, 57.0);
    let radius = 55.0;
    let fill = Color32::from_rgb(255, 240, 212);
    ui.painter().circle_filled(center, radius, fill);
    ui.painter().circle_stroke(
        center,
        radius,
        Stroke::new(
            if selected { 5.0 } else { 2.0 },
            if selected {
                BRIGHT_ORANGE
            } else {
                Color32::from_gray(214)
            },
        ),
    );

    let image_rect = egui::Rect::from_center_size(center, Vec2::splat(94.0));
    egui::Image::new(board_image(board))
        .fit_to_exact_size(image_rect.size())
        .paint_at(ui, image_rect);

    if selected {
        let badge = center + Vec2::new(43.0, -42.0);
        ui.painter().circle_filled(badge, 16.0, BRIGHT_ORANGE);
        ui.painter()
            .circle_stroke(badge, 16.0, Stroke::new(3.0, BACKGROUND));
        ui.painter().text(
            badge,
            egui::Align2::CENTER_CENTER,
            "✓",
            FontId::proportional(17.0),
            Color32::BLACK,
        );
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
