// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC
//! Full-page surfaces (Settings / Licenses / Credits / Plugins / Bird's Eye) shown in place of the editor.
pub mod birds_eye;
pub mod credits;
pub mod licenses;
pub mod plugins;
pub mod settings;

use crate::state::{AppState, AppView};

/// Render the active non-editor page into the central area.
pub fn render(ui: &mut egui::Ui, state: &mut AppState) {
    match state.view {
        AppView::Settings => settings::render(ui, state),
        AppView::Licenses => licenses::render(ui, state),
        AppView::Credits => credits::render(ui, state),
        AppView::Plugins => plugins::render(ui, state),
        AppView::BirdsEye => birds_eye::render(ui, state),
        AppView::Editor => {}
    }
}

/// Page header: a large title on the left and a "Close" button pinned to the
/// far right that returns to the editor.
pub fn page_header(ui: &mut egui::Ui, title: &str, view: &mut AppView) {
    ui.horizontal(|ui| {
        // Indent the title to line up with the card titles (card inner margin).
        ui.add_space(crate::theme::SPACE_L);
        ui.heading(title);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("Close").clicked() {
                *view = AppView::Editor;
            }
        });
    });
    ui.separator();
}

/// A titled, bordered card with a muted description line — the Grexa settings
/// look. The caller controls width by constraining `ui` first (e.g.
/// `ui.set_max_width(..)`); otherwise the card fills the available width.
pub fn card(
    ui: &mut egui::Ui,
    palette: &crate::theme::Palette,
    title: &str,
    description: &str,
    contents: impl FnOnce(&mut egui::Ui),
) {
    egui::Frame::new()
        .fill(palette.secondary)
        .stroke(egui::Stroke::new(1.0, palette.separator_strong))
        .corner_radius(egui::CornerRadius::same(crate::theme::RADIUS_CARD))
        .inner_margin(egui::Margin::same(crate::theme::SPACE_L as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                egui::RichText::new(title)
                    .size(crate::theme::TEXT_SUBHEADING)
                    .strong(),
            );
            ui.label(egui::RichText::new(description).weak());
            ui.add_space(crate::theme::SPACE_M);
            contents(ui);
        });
    ui.add_space(crate::theme::SPACE_L);
}
