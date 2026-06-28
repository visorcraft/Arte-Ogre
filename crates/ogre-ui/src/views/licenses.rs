// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC
//! The Licenses page: the app's GPL-3.0 license and bundled third-party licenses.
use crate::state::AppState;

const GPL_TEXT: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../LICENSE"));
const THIRD_PARTY_TEXT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../THIRD_PARTY_LICENSES.md"
));

/// Render the Licenses page (tabbed: GPL-3.0 / Third-party).
pub fn render(ui: &mut egui::Ui, state: &mut AppState) {
    let palette = crate::theme::resolve(state.preferences.theme, ui.ctx());
    super::page_header(ui, "Licenses", &mut state.view);
    ui.horizontal(|ui| {
        ui.selectable_value(&mut state.licenses_tab, 0u8, "Arte Ogre (GPL-3.0)");
        ui.selectable_value(&mut state.licenses_tab, 1u8, "Third-party");
    });
    let (title, text) = if state.licenses_tab == 0 {
        ("Arte Ogre — GPL-3.0", GPL_TEXT)
    } else {
        ("Third-party licenses", THIRD_PARTY_TEXT)
    };
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(crate::theme::SPACE_M);
            super::card(
                ui,
                &palette,
                title,
                "Full license text for the bundled software.",
                |ui| {
                    ui.monospace(text);
                },
            );
        });
}

#[cfg(test)]
mod tests {
    #[test]
    fn embedded_license_text_is_present() {
        assert!(super::GPL_TEXT.contains("GNU GENERAL PUBLIC LICENSE"));
        assert!(!super::THIRD_PARTY_TEXT.is_empty());
    }
}
