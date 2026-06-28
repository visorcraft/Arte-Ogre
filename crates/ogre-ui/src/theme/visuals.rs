// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Maps a [`Palette`] onto egui `Visuals` and `Style`.

use egui::{Color32, CornerRadius, Stroke, Visuals};

use super::Palette;

/// Readable label color (black or white) to place on an `accent` fill —
/// whichever has the higher WCAG contrast against it.
pub fn on_accent(accent: Color32) -> Color32 {
    use super::derive::contrast_ratio;
    if contrast_ratio(Color32::WHITE, accent) >= contrast_ratio(Color32::BLACK, accent) {
        Color32::WHITE
    } else {
        Color32::BLACK
    }
}

/// Build egui `Visuals` from a resolved palette.
pub fn to_visuals(p: &Palette) -> Visuals {
    let mut v = if p.is_dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };

    v.dark_mode = p.is_dark;
    v.override_text_color = Some(p.text);
    v.hyperlink_color = p.accent;
    v.panel_fill = p.secondary;
    v.window_fill = p.secondary;
    v.extreme_bg_color = p.bg;
    v.faint_bg_color = p.secondary;
    v.window_stroke = Stroke::new(1.0, p.separator);
    v.window_corner_radius = CornerRadius::same(super::RADIUS_CARD);
    v.menu_corner_radius = CornerRadius::same(super::RADIUS_INPUT);
    v.window_shadow.color = p.shadow_near;

    v.selection.bg_fill = p.accent_mute_strong;
    v.selection.stroke = Stroke::new(1.0, p.accent);

    // Fill the slider track up to the handle with the accent color. Without this
    // the rail uses `widgets.inactive.bg_fill`, which equals `window_fill`, so a
    // slider is invisible on flat themes like OLED Black.
    v.slider_trailing_fill = true;

    let txt = Stroke::new(1.0, p.text);
    let sep = Stroke::new(1.0, p.separator);
    let br = CornerRadius::same(super::RADIUS_BUTTON);

    let w = &mut v.widgets;
    for s in [
        &mut w.noninteractive,
        &mut w.inactive,
        &mut w.hovered,
        &mut w.active,
        &mut w.open,
    ] {
        s.fg_stroke = txt;
        s.bg_stroke = sep;
        s.corner_radius = br;
    }
    w.noninteractive.bg_fill = p.secondary;
    w.noninteractive.weak_bg_fill = p.secondary;
    // `inactive.bg_fill` is the slider/progress rail. Use `tertiary` (a raised
    // surface) rather than `secondary`, which equals `window_fill` and would
    // make the rail invisible on flat themes like OLED Black.
    w.inactive.bg_fill = p.tertiary;
    w.inactive.weak_bg_fill = p.bg;
    w.hovered.bg_fill = p.tertiary;
    w.hovered.weak_bg_fill = p.tertiary;
    w.active.bg_fill = p.accent_mute_strong;
    w.active.weak_bg_fill = p.accent_mute_strong;
    w.open.bg_fill = p.tertiary;

    v
}

/// Apply the palette's visuals and spacing tokens to the egui context.
pub fn apply_style(ctx: &egui::Context, palette: &Palette) {
    ctx.set_visuals(to_visuals(palette));
    // Use `global_style_mut`; `style_mut` is deprecated and `-D warnings`
    // promotes deprecation lints to hard errors.
    ctx.global_style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(super::SPACE_S, 6.0);
        s.spacing.button_padding = egui::vec2(10.0, 6.0);
        s.spacing.menu_margin = egui::Margin::same(super::SPACE_S as i8);
        s.spacing.window_margin = egui::Margin::same(super::SPACE_M as i8);
        s.spacing.indent = super::SPACE_L;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;

    #[test]
    fn on_accent_picks_readable_label() {
        // Light yellow accent (Vibes) must use black, not white.
        assert_eq!(
            on_accent(Color32::from_rgb(0xFF, 0xCC, 0x00)),
            Color32::BLACK
        );
        // Near-black accent must use white.
        assert_eq!(
            on_accent(Color32::from_rgb(0x10, 0x10, 0x10)),
            Color32::WHITE
        );
    }

    #[test]
    fn dark_mode_tracks_is_dark() {
        assert!(to_visuals(&Theme::Dark.palette(true)).dark_mode);
        assert!(!to_visuals(&Theme::Light.palette(false)).dark_mode);
    }

    #[test]
    fn text_and_surfaces_come_from_palette() {
        let p = Theme::Dreams.palette(true);
        let v = to_visuals(&p);
        assert_eq!(v.override_text_color, Some(p.text));
        assert_eq!(v.panel_fill, p.secondary);
        assert_eq!(v.extreme_bg_color, p.bg);
        assert_eq!(v.hyperlink_color, p.accent);
    }

    #[test]
    fn selection_uses_accent() {
        let p = Theme::Tiefling.palette(true);
        let v = to_visuals(&p);
        assert_eq!(v.selection.bg_fill, p.accent_mute_strong);
        assert_eq!(v.selection.stroke.color, p.accent);
    }
}
