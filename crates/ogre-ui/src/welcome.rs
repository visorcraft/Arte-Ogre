// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC
//! The startup welcome card: open an image, start a blank canvas.
use crate::state::AppState;

/// Full-body Ogre artwork shown on the welcome screen.
const OGRE_BODY_PNG: &[u8] = include_bytes!("../../../assets/ogre.png");

/// Sized, cached full-body ogre image for the landing screen.
fn ogre_body_image() -> egui::Image<'static> {
    egui::Image::from_bytes("bytes://ogre/body/ogre.png", OGRE_BODY_PNG)
}

/// Render the welcome card into the central area.
pub fn render(ui: &mut egui::Ui, state: &mut AppState) {
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() * 0.12);

        let palette = crate::theme::resolve(state.preferences.theme, ui.ctx());
        let busy = state.plugin_busy || state.io_busy;

        // A single clickable card that contains both the ogre image and the
        // "Click to open an Image" prompt. Hovering the whole card highlights it
        // and turns the text accent-colored, just like PDF Panda's landing card.
        let card_response = clickable_open_card(ui, palette);
        if card_response.hovered() || ui.rect_contains_pointer(card_response.rect) {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if card_response.clicked() && !busy {
            crate::open_flow::show_open_dialog(state);
        }

        ui.add_space(16.0);

        if ui
            .add_enabled(!busy, egui::Button::new("New blank canvas"))
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .clicked()
        {
            let (w, h) = state.preferences.default_canvas;
            state.new_blank_document((w, h));
        }

        if !state.file_dialog_feedback.is_empty() {
            ui.colored_label(ui.visuals().error_fg_color, &state.file_dialog_feedback);
        }
    });
}

/// A centered, rounded card with the ogre image and open prompt.
///
/// The returned [`egui::Response`] covers the whole card, so callers can react
/// to clicks and hover uniformly for both the image and the label.
fn clickable_open_card(ui: &mut egui::Ui, palette: crate::theme::Palette) -> egui::Response {
    let max_width = ui.available_width().min(360.0);
    let padding = 16.0;
    let gap = 24.0;
    let text_height = 20.0;
    let image_size = max_width.min(320.0) - padding * 2.0;

    let desired_size = egui::vec2(max_width, image_size + gap + text_height + padding * 2.0);
    // Reserve the card rect for layout only; the top-level interact below is
    // what actually captures clicks, so clicking the label works the same as
    // clicking the image.
    let (rect, _size_response) = ui.allocate_exact_size(desired_size, egui::Sense::hover());

    let hovered = ui.rect_contains_pointer(rect);
    let fill = if hovered {
        ui.visuals().widgets.hovered.bg_fill
    } else {
        egui::Color32::TRANSPARENT
    };
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::same(12), fill);

    let text_color = if hovered {
        palette.accent
    } else {
        ui.visuals().text_color()
    };

    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(rect.shrink(padding))
            .layout(*ui.layout()),
        |ui| {
            ui.vertical_centered(|ui| {
                ui.add_sized(egui::vec2(image_size, image_size), ogre_body_image());
                ui.add_space(gap);
                ui.label(
                    egui::RichText::new("Click to open an Image")
                        .size(16.0)
                        .color(text_color),
                );
            });
        },
    );

    // This interact is issued after the children are drawn so it sits on top
    // and receives clicks anywhere in the card, including over the label.
    ui.interact(rect, ui.id().with("open_card"), egui::Sense::click())
}

/// Render the crash-recovery prompt when [`AppState::recovery_prompt`] holds a
/// document salvaged from a previous session that ended unexpectedly.
///
/// **Recover** adopts the snapshot as the active document; **Discard** deletes
/// the recovery files. Dismissing the window (✕) defers the choice — the files
/// stay, so the prompt returns on the next launch. Call once per frame.
pub fn render_recovery_prompt(ctx: &egui::Context, state: &mut AppState) {
    if state.recovery_prompt.is_none() {
        return;
    }
    let mut recover = false;
    let mut discard = false;
    let mut open = true;
    let mut escape = false;
    egui::Window::new("Recover unsaved work")
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .collapsible(false)
        .resizable(false)
        .open(&mut open)
        .show(ctx, |ui| {
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                escape = true;
            }
            ui.label("Arte Ogre found unsaved work from a session that ended unexpectedly.");
            ui.add_space(crate::theme::SPACE_S);
            ui.horizontal(|ui| {
                if ui.button("Recover").clicked() {
                    recover = true;
                }
                if ui.button("Discard").clicked() {
                    discard = true;
                }
            });
        });
    if recover {
        if let Some(doc) = state.recovery_prompt.take() {
            state.adopt_recovered(doc);
        }
    } else if discard {
        state.recovery_prompt = None;
        state.autosave.clear_recovery();
    } else if !open || escape {
        // Defer: dismiss the prompt this session but keep the files on disk.
        state.recovery_prompt = None;
    }
}
