// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC
//! Full-page Plugin Manager surface.
use crate::state::AppState;

/// Render the full-page Plugin Manager into the central area.
pub fn render(ui: &mut egui::Ui, state: &mut AppState) {
    let palette = crate::theme::resolve(state.preferences.theme, ui.ctx());
    super::page_header(ui, "Plugins", &mut state.view);
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(crate::theme::SPACE_M);
            super::card(
                ui,
                &palette,
                "Installed plugins",
                "Sandboxed tile filters discovered in your plugins directory. Enable one, then run it on the active layer.",
                |ui| {
                    crate::panels::plugin_manager::ui(ui, state);
                },
            );
        });
}
