// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Plugin manager panel.
//!
//! Renders a window listing discovered plugins and lets the user enable/disable
//! them or run Lua/WASM plugins against the active document.

use crate::plugin_worker::PluginRequest;
use crate::shell::active_raster_layer_id;
use crate::state::AppState;
use ogre_plugins::{PluginFilter, PluginInfo, PluginKind};

/// Render the plugin manager inline into `ui`.
pub fn ui(ui: &mut egui::Ui, state: &mut AppState) {
    ui.horizontal(|ui| {
        if ui.button("Rescan").clicked() {
            state.plugin_manager.discover();
            state.plugin_manager_feedback.clear();
        }
        ui.label(
            state
                .plugin_manager
                .plugins_dir()
                .to_string_lossy()
                .as_ref(),
        );
    });

    ui.separator();

    let plugins: Vec<PluginInfo> = state.plugin_manager.plugins().to_vec();
    if plugins.is_empty() {
        ui.label("No plugins discovered.");
    } else {
        egui::ScrollArea::vertical().max_height(220.0).show_rows(
            ui,
            ui.text_style_height(&egui::TextStyle::Body),
            plugins.len(),
            |ui, range| {
                for plugin in &plugins[range] {
                    render_plugin_row(ui, state, plugin);
                }
            },
        );
    }

    if state.plugin_busy {
        ui.separator();
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Running plugin…");
        });
    }

    if !state.plugin_manager_feedback.is_empty() {
        ui.separator();
        ui.colored_label(egui::Color32::LIGHT_RED, &state.plugin_manager_feedback);
    }
}

fn render_plugin_row(ui: &mut egui::Ui, state: &mut AppState, plugin: &PluginInfo) {
    ui.horizontal(|ui| {
        ui.label(&plugin.name);
        ui.label(format!("v{}", plugin.version));
        ui.label(format!("{:?}", plugin.kind));

        if plugin.valid {
            let mut enabled = plugin.enabled;
            if ui.checkbox(&mut enabled, "Enabled").changed() {
                // Toggle by unique directory, not by (possibly duplicated) name.
                state.plugin_manager.set_enabled_dir(&plugin.dir, enabled);
            }

            let run_btn = ui.add_enabled(enabled && !state.plugin_busy, egui::Button::new("Run"));
            if run_btn.clicked() {
                state.plugin_manager_feedback.clear();
                if let Err(e) = run_plugin(state, plugin) {
                    state.plugin_manager_feedback = e;
                }
            }
        } else {
            ui.colored_label(egui::Color32::LIGHT_RED, "invalid");
        }
    });

    if let Some(err) = &plugin.error {
        ui.colored_label(egui::Color32::LIGHT_RED, err);
    }
}

fn run_plugin(state: &mut AppState, plugin: &PluginInfo) -> Result<(), String> {
    let id = state.plugin_worker.next_id();
    state.plugin_expected_id = Some(id);
    match plugin.kind {
        PluginKind::Lua => {
            let source = std::fs::read_to_string(&plugin.entry)
                .map_err(|e| format!("Could not read {}: {e}", plugin.entry.display()))?;
            state.plugin_worker.request(PluginRequest::Lua {
                id,
                source,
                doc: state.doc().clone(),
            });
            state.plugin_busy = true;
            Ok(())
        }
        PluginKind::Wasm => {
            let wasm = std::fs::read(&plugin.entry)
                .map_err(|e| format!("Could not read {}: {e}", plugin.entry.display()))?;
            let layer_id = active_raster_layer_id(state)
                .ok_or_else(|| "No active raster layer".to_string())?;
            let filter = Box::new(PluginFilter::new(&plugin.name, wasm, Vec::new()));
            state.plugin_worker.request(PluginRequest::Wasm {
                id,
                cmd: Box::new(ogre_gpu::ApplyFilterCmd::new(layer_id, filter)),
                doc: state.doc().clone(),
            });
            state.plugin_busy = true;
            Ok(())
        }
    }
}
