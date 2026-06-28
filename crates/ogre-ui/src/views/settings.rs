// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC
//! The Settings page: appearance, editor preferences, and keyboard shortcuts.
use super::card;
use crate::state::AppState;
use crate::theme::Theme;

/// Render the Settings page.
pub fn render(ui: &mut egui::Ui, state: &mut AppState) {
    let palette = crate::theme::resolve(state.preferences.theme, ui.ctx());
    super::page_header(ui, "Settings", &mut state.view);

    if let Some(path) = crate::prefs::Preferences::config_path() {
        ui.horizontal(|ui| {
            ui.add_space(crate::theme::SPACE_L);
            ui.label(
                egui::RichText::new(format!("Auto-saved to {}", path.display()))
                    .size(crate::theme::TEXT_CAPTION)
                    .weak(),
            );
        });
    }

    egui::ScrollArea::vertical().show(ui, |ui| {
        ui.add_space(crate::theme::SPACE_M);

        card(
            ui,
            &palette,
            "Appearance",
            "Theme variant — the GTK/Plasma host palette still drives the chrome; this picks the in-app accent.",
            |ui| {
                ui.horizontal(|ui| {
                    ui.label("Theme");
                    let mut changed = false;
                    let combo = egui::ComboBox::from_id_salt("settings_theme")
                        .width(ui.available_width())
                        .selected_text(state.preferences.theme.label())
                        .show_ui(ui, |ui| {
                            for theme in Theme::DISPLAY_ORDER {
                                if ui
                                    .selectable_value(&mut state.preferences.theme, theme, theme.label())
                                    .clicked()
                                {
                                    changed = true;
                                }
                            }
                        });
                    // egui's ComboBox has no built-in arrow nav, so scrub Up/Down
                    // through DISPLAY_ORDER (applied live) whenever the combo is the
                    // user's focus: hovered, keyboard-focused, or its list is open.
                    let active = combo.response.hovered()
                        || combo.response.has_focus()
                        || combo.inner.is_some();
                    if active {
                        let step = ui.input_mut(|i| {
                            let down = i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown);
                            let up = i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp);
                            down as i32 - up as i32
                        });
                        if step != 0 {
                            state.preferences.theme = step_theme(state.preferences.theme, step);
                            changed = true;
                        }
                    }
                    if changed {
                        if let Some(path) = crate::prefs::Preferences::config_path() {
                            let _ = state.preferences.save(&path);
                        }
                    }
                });
            },
        );

        card(
            ui,
            &palette,
            "Editor",
            "Defaults applied to new documents, and how often open work is auto-saved.",
            |ui| {
                let (mut w, mut h) = state.preferences.default_canvas;
                ui.horizontal(|ui| {
                    ui.label("Default canvas");
                    ui.add(egui::DragValue::new(&mut w).range(1..=16384));
                    ui.label("×");
                    ui.add(egui::DragValue::new(&mut h).range(1..=16384));
                });
                let mut autosave = state.preferences.autosave_interval_seconds;
                ui.horizontal(|ui| {
                    ui.label("Autosave interval (s, 0 = off)");
                    ui.add(egui::DragValue::new(&mut autosave).range(0..=3600));
                });
                if (w, h) != state.preferences.default_canvas
                    || autosave != state.preferences.autosave_interval_seconds
                {
                    state.preferences.default_canvas = (w, h);
                    state.preferences.autosave_interval_seconds = autosave;
                    if let Some(path) = crate::prefs::Preferences::config_path() {
                        let _ = state.preferences.save(&path);
                    }
                }
            },
        );

        let is_default_order = state.preferences.sidebar_order
            == crate::tools::default_sidebar_order();
        card(
            ui,
            &palette,
            "Tool sidebar",
            "Restore the original order of the tool-section headers.",
            |ui| {
                if ui
                    .add_enabled(
                        !is_default_order,
                        egui::Button::new("Reset to default order"),
                    )
                    .clicked()
                {
                    state.dragging_sidebar_section = None;
                    state.dragging_sidebar_section_anchors = None;
                    state.preferences.sidebar_order = crate::tools::default_sidebar_order();
                    if let Some(path) = crate::prefs::Preferences::config_path() {
                        let _ = state.preferences.save(&path);
                    }
                }
            },
        );

        card(
            ui,
            &palette,
            "Keyboard shortcuts",
            "Bind a chord to an action, then review or reset the current bindings.",
            |ui| {
                state.keymap.ui(
                    ui,
                    &mut state.keymap_editor_chord,
                    &mut state.keymap_editor_action,
                    &mut state.keymap_editor_feedback,
                );
            },
        );
    });
}

/// Step `current` by `step` positions through [`Theme::DISPLAY_ORDER`], wrapping
/// at both ends.
fn step_theme(current: Theme, step: i32) -> Theme {
    let order = Theme::DISPLAY_ORDER;
    let cur = order.iter().position(|&t| t == current).unwrap_or(0) as i32;
    order[(cur + step).rem_euclid(order.len() as i32) as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_theme_wraps_both_directions() {
        let order = Theme::DISPLAY_ORDER;
        let last = order.len() - 1;
        assert_eq!(step_theme(order[0], 1), order[1]);
        assert_eq!(step_theme(order[0], -1), order[last]); // wrap past the start
        assert_eq!(step_theme(order[last], 1), order[0]); // wrap past the end
        assert_eq!(step_theme(order[2], 0), order[2]); // no-op
    }
}
