// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Color helpers: alpha tinting, straight-alpha over-compositing, and WCAG
//! contrast ratio.

use egui::Color32;

/// Straight-alpha tint of `c` (alpha in 0.0..=1.0).
pub fn with_alpha(c: Color32, a: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(
        c.r(),
        c.g(),
        c.b(),
        (a.clamp(0.0, 1.0) * 255.0).round() as u8,
    )
}

/// Straight alpha-over compositing of `over` onto opaque `base`.
pub fn tint(base: Color32, over: Color32) -> Color32 {
    let a = over.a() as f32 / 255.0;
    let [or, og, ob, _] = over.to_srgba_unmultiplied();
    let mix = |b: u8, o: u8| ((b as f32) * (1.0 - a) + (o as f32) * a).round() as u8;
    Color32::from_rgb(mix(base.r(), or), mix(base.g(), og), mix(base.b(), ob))
}

/// WCAG contrast ratio between two opaque colors (>= 1.0).
pub fn contrast_ratio(a: Color32, b: Color32) -> f32 {
    let lum = |c: Color32| {
        let ch = |v: u8| {
            let s = v as f32 / 255.0;
            if s <= 0.03928 {
                s / 12.92
            } else {
                ((s + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * ch(c.r()) + 0.7152 * ch(c.g()) + 0.0722 * ch(c.b())
    };
    let (la, lb) = (lum(a), lum(b));
    let (hi, lo) = (la.max(lb), la.min(lb));
    (hi + 0.05) / (lo + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_alpha_sets_straight_alpha() {
        let c = with_alpha(Color32::from_rgb(0x2D, 0x7F, 0xF9), 0.18);
        assert_eq!(c.a(), (0.18_f32 * 255.0).round() as u8); // 46
                                                             // Color32 stores premultiplied; verify the straight-alpha rgb values survive
                                                             // the round-trip to within ±1 (precision loss from premultiply/un-premultiply).
        let [r, g, b, _] = c.to_srgba_unmultiplied();
        assert!((r as i16 - 0x2D).abs() <= 1);
        assert!((g as i16 - 0x7F).abs() <= 1);
        assert!((b as i16 - 0xF9_u8 as i16).abs() <= 1);
    }

    #[test]
    fn contrast_ratio_white_on_black_is_21() {
        let r = contrast_ratio(Color32::WHITE, Color32::BLACK);
        assert!((r - 21.0).abs() < 0.1, "got {r}");
    }

    #[test]
    fn tint_uses_straight_alpha_not_premultiplied() {
        // Pure-blue overlay at 50% alpha over black: straight-alpha-over puts
        // the blue channel at ~127. The premultiplied-byte bug would yield ~64.
        let r = tint(
            Color32::BLACK,
            Color32::from_rgba_unmultiplied(0, 0, 255, 128),
        );
        assert!((125..=129).contains(&r.b()), "blue channel = {}", r.b());
        assert_eq!((r.r(), r.g()), (0, 0));
    }
}
