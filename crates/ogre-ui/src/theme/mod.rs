// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Theme model: the 13 Grexa palettes and their egui mapping.

pub(crate) mod derive;
pub mod visuals;

use egui::Color32;

/// Spacing scale (Grexa 4px rhythm).
pub const SPACE_XS: f32 = 4.0;
/// Small spacing.
pub const SPACE_S: f32 = 8.0;
/// Medium spacing.
pub const SPACE_M: f32 = 12.0;
/// Large spacing.
pub const SPACE_L: f32 = 16.0;
/// Extra-large spacing.
pub const SPACE_XL: f32 = 24.0;
/// Button corner radius.
pub const RADIUS_BUTTON: u8 = 6;
/// Input corner radius.
pub const RADIUS_INPUT: u8 = 8;
/// Card / window corner radius.
pub const RADIUS_CARD: u8 = 10;
/// Pill radius (version badge).
pub const RADIUS_PILL: u8 = 255;
/// Caption text size.
pub const TEXT_CAPTION: f32 = 11.0;
/// Subheading text size (group labels).
pub const TEXT_SUBHEADING: f32 = 16.0;
/// Heading text size (modal titles).
pub const TEXT_HEADING: f32 = 18.0;

/// UI theme preference. Variant names are serialized verbatim (TOML), so the
/// original `System`/`Dark`/`Light` names stay load-compatible.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Theme {
    /// Follow the OS theme (resolved via `ctx.system_theme()`; unknown → dark).
    #[default]
    System,
    /// Light.
    Light,
    /// Dark.
    Dark,
    /// Gentle Gecko (green-on-black).
    GentleGecko,
    /// Black Knight (blue-on-black).
    BlackKnight,
    /// Diamond (teal).
    Diamond,
    /// Dreams (purple/magenta).
    Dreams,
    /// Paranoid (indigo).
    Paranoid,
    /// Red Velvet (deep red).
    RedVelvet,
    /// Subspace (violet).
    Subspace,
    /// Tiefling (magenta/amber).
    Tiefling,
    /// Vibes (neon).
    Vibes,
    /// OLED Black (true-black canvas, Dark accent).
    OledBlack,
}

impl Theme {
    /// All variants, enum order.
    pub const ALL: [Theme; 13] = [
        Theme::System,
        Theme::Light,
        Theme::Dark,
        Theme::GentleGecko,
        Theme::BlackKnight,
        Theme::Diamond,
        Theme::Dreams,
        Theme::Paranoid,
        Theme::RedVelvet,
        Theme::Subspace,
        Theme::Tiefling,
        Theme::Vibes,
        Theme::OledBlack,
    ];

    /// Order shown in the selector (matches Grexa's UI).
    pub const DISPLAY_ORDER: [Theme; 13] = [
        Theme::System,
        Theme::Light,
        Theme::Dark,
        Theme::OledBlack,
        Theme::GentleGecko,
        Theme::BlackKnight,
        Theme::Diamond,
        Theme::Dreams,
        Theme::Paranoid,
        Theme::RedVelvet,
        Theme::Subspace,
        Theme::Tiefling,
        Theme::Vibes,
    ];

    /// Human-friendly selector label.
    pub fn label(self) -> &'static str {
        match self {
            Theme::System => "Follow system",
            Theme::Light => "Light",
            Theme::Dark => "Dark",
            Theme::GentleGecko => "Gentle Gecko",
            Theme::BlackKnight => "Black Knight",
            Theme::Diamond => "Diamond",
            Theme::Dreams => "Dreams",
            Theme::Paranoid => "Paranoid",
            Theme::RedVelvet => "Red Velvet",
            Theme::Subspace => "Subspace",
            Theme::Tiefling => "Tiefling",
            Theme::Vibes => "Vibes",
            Theme::OledBlack => "OLED Black",
        }
    }
}

/// Resolve a [`Theme`] to a [`Palette`], probing the OS theme for `System`.
pub fn resolve(theme: Theme, ctx: &egui::Context) -> Palette {
    let hint = match ctx.system_theme() {
        Some(egui::Theme::Light) => false,
        _ => true, // Some(Dark) or None → dark (headless default)
    };
    theme.palette(hint)
}

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

const ACCENT_DEFAULT: Color32 = rgb(0x2D7FF9);

/// A fully-resolved theme palette (Grexa's 5 stops plus derived values).
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    /// Canvas / window background (Grexa surface0).
    pub bg: Color32,
    /// Sidebar chrome / card surface (surfaceSidebar / surface1).
    pub secondary: Color32,
    /// Hover / pressed lift (surface2).
    pub tertiary: Color32,
    /// Primary text (reads on `bg`).
    pub text: Color32,
    /// Active row / primary button / selection.
    pub accent: Color32,
    /// `accent` at alpha 0.18.
    pub accent_mute: Color32,
    /// `accent` at alpha 0.28.
    pub accent_mute_strong: Color32,
    /// `accent` at alpha 0.40 (focus ring).
    pub accent_ring: Color32,
    /// `text` at low alpha.
    pub separator: Color32,
    /// `text` at medium alpha.
    pub separator_strong: Color32,
    /// Near drop-shadow tint.
    pub shadow_near: Color32,
    /// Whether this is a dark-leaning theme (drives tint direction).
    pub is_dark: bool,
}

impl Theme {
    /// Raw `bg` stop, or `None` when derived (System only).
    fn raw_bg(self) -> Option<Color32> {
        match self {
            Theme::System => None,
            Theme::Light => Some(rgb(0xF5F5F5)),
            Theme::Dark => Some(rgb(0x181818)),
            Theme::GentleGecko | Theme::BlackKnight | Theme::OledBlack => Some(rgb(0x000000)),
            Theme::Diamond => Some(rgb(0x2D5B67)),
            Theme::Dreams => Some(rgb(0x210B4B)),
            Theme::Paranoid => Some(rgb(0x1D1D4E)),
            Theme::RedVelvet => Some(rgb(0x1A0F0F)),
            Theme::Subspace => Some(rgb(0x2E1A47)),
            Theme::Tiefling => Some(rgb(0x3A0A4D)),
            Theme::Vibes => Some(rgb(0x0F0F1E)),
        }
    }

    /// Raw `secondary` stop, or `None` when derived (System/Light/Dark).
    fn raw_secondary(self) -> Option<Color32> {
        match self {
            Theme::System | Theme::Light | Theme::Dark => None,
            Theme::GentleGecko => Some(rgb(0x003322)),
            Theme::BlackKnight => Some(rgb(0x003366)),
            Theme::Diamond => Some(rgb(0x4F7F8C)),
            Theme::Dreams => Some(rgb(0x3F1C6D)),
            Theme::Paranoid => Some(rgb(0x3F3F88)),
            Theme::RedVelvet => Some(rgb(0x3C1414)),
            Theme::Subspace => Some(rgb(0x4A2A6A)),
            Theme::Tiefling => Some(rgb(0x711D9A)),
            Theme::Vibes => Some(rgb(0x1E1E3C)),
            Theme::OledBlack => Some(rgb(0x050505)),
        }
    }

    /// Raw `tertiary` stop, or `None` when derived (System/Light/Dark).
    fn raw_tertiary(self) -> Option<Color32> {
        match self {
            Theme::System | Theme::Light | Theme::Dark => None,
            Theme::GentleGecko => Some(rgb(0x00593D)),
            Theme::BlackKnight => Some(rgb(0x00478F)),
            Theme::Diamond => Some(rgb(0x7CA2B1)),
            Theme::Dreams => Some(rgb(0x6A2A98)),
            Theme::Paranoid => Some(rgb(0x5F5FBF)),
            Theme::RedVelvet => Some(rgb(0x8B2323)),
            Theme::Subspace => Some(rgb(0x794B8B)),
            Theme::Tiefling => Some(rgb(0xA42DB4)),
            Theme::Vibes => Some(rgb(0xCC00FF)),
            Theme::OledBlack => Some(rgb(0x111111)),
        }
    }

    /// Raw `text` stop, or `None` when derived (System only).
    fn raw_text(self) -> Option<Color32> {
        match self {
            Theme::System => None,
            Theme::Light => Some(rgb(0x1A1A1A)),
            Theme::Dark | Theme::OledBlack => Some(rgb(0xF5F5F5)),
            Theme::GentleGecko | Theme::BlackKnight => Some(rgb(0xFFFFFF)),
            Theme::Diamond => Some(rgb(0xFFFFFF)),
            Theme::Dreams => Some(rgb(0xFF3D94)),
            Theme::Paranoid => Some(rgb(0xD2D2F4)),
            Theme::RedVelvet => Some(rgb(0xFFDCDC)),
            Theme::Subspace => Some(rgb(0xE2C7E6)),
            Theme::Tiefling => Some(rgb(0xF9C54E)),
            Theme::Vibes => Some(rgb(0x00FFCC)),
        }
    }

    fn accent(self) -> Color32 {
        match self {
            Theme::GentleGecko => rgb(0x00B86B),
            Theme::BlackKnight => rgb(0x0078D4),
            Theme::Diamond => rgb(0xA5C5D5),
            Theme::Dreams => rgb(0xB5307E),
            Theme::Paranoid => rgb(0x9A9AE0),
            Theme::RedVelvet => rgb(0xDC3C3C),
            Theme::Subspace => rgb(0xB77BB4),
            Theme::Tiefling => rgb(0xFF5C8A),
            Theme::Vibes => rgb(0xFFCC00),
            _ => ACCENT_DEFAULT, // System / Light / Dark / OledBlack
        }
    }

    /// `is_dark` per Grexa: Light=false, Dark/named=true, System=hint.
    fn is_dark(self, hint: bool) -> bool {
        match self {
            Theme::Light => false,
            Theme::System => hint,
            _ => true,
        }
    }

    /// Alpha used for subtle separators and widget borders.
    fn separator_alpha(self, is_dark: bool) -> f32 {
        match self {
            Theme::OledBlack => 0.12,
            _ => {
                if is_dark {
                    0.12
                } else {
                    0.09
                }
            }
        }
    }

    /// Alpha used for muted labels (section headers, setting labels, grip).
    fn separator_strong_alpha(self, is_dark: bool) -> f32 {
        match self {
            // Light's near-white sidebar needs higher alpha so black muted labels
            // don't wash out.
            Theme::Light => 0.55,
            // Several named palettes have colored text on a colored secondary
            // surface with low contrast; they need a much stronger muted label.
            Theme::Diamond => 0.70,
            Theme::Dreams => 0.85,
            Theme::Tiefling => 0.85,
            // All dark themes share the same problem: 22 % white text on a dark
            // sidebar is too faint to read.
            _ => {
                if is_dark {
                    0.55
                } else {
                    0.16
                }
            }
        }
    }

    /// Resolve the full palette. `is_dark_hint` is only consulted for `System`.
    pub fn palette(self, is_dark_hint: bool) -> Palette {
        use derive::{tint, with_alpha};
        let is_dark = self.is_dark(is_dark_hint);

        // bg: explicit, or derived from a neutral base for System.
        let bg = self.raw_bg().unwrap_or(if is_dark {
            rgb(0x1A1A1A) // intentionally slightly lighter than Dark's 0x181818 — neutral system default, not a typo
        } else {
            rgb(0xF5F5F5)
        });

        // surfaces: named themes use stops; otherwise luminance tints of bg.
        let secondary = self.raw_secondary().unwrap_or_else(|| {
            tint(
                bg,
                if is_dark {
                    Color32::from_rgba_unmultiplied(0, 0, 0, 56) // black @ 0.22
                } else {
                    Color32::from_rgba_unmultiplied(51, 77, 128, 15) // navy @ 0.06
                },
            )
        });
        let tertiary = self.raw_tertiary().unwrap_or_else(|| {
            tint(
                bg,
                if is_dark {
                    Color32::from_rgba_unmultiplied(255, 255, 255, 31) // white @ 0.12
                } else {
                    Color32::from_rgba_unmultiplied(0, 0, 0, 18) // black @ 0.07
                },
            )
        });

        let text = self.raw_text().unwrap_or(if is_dark {
            rgb(0xF5F5F5)
        } else {
            rgb(0x1A1A1A)
        });
        let accent = self.accent();

        Palette {
            bg,
            secondary,
            tertiary,
            text,
            accent,
            accent_mute: with_alpha(accent, 0.18),
            accent_mute_strong: with_alpha(accent, 0.28),
            accent_ring: with_alpha(accent, 0.40),
            separator: with_alpha(text, self.separator_alpha(is_dark)),
            separator_strong: with_alpha(text, self.separator_strong_alpha(is_dark)),
            shadow_near: with_alpha(Color32::BLACK, if is_dark { 0.45 } else { 0.10 }),
            is_dark,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_order_is_grexa_order_and_complete() {
        let order = Theme::DISPLAY_ORDER;
        assert_eq!(order.len(), 13);
        assert_eq!(order[0], Theme::System);
        assert_eq!(order[3], Theme::OledBlack); // OLED Black shown 4th, per the screenshot
                                                // every variant appears exactly once
        let mut all = Theme::ALL.to_vec();
        all.sort_by_key(|t| format!("{t:?}"));
        let mut ord = order.to_vec();
        ord.sort_by_key(|t| format!("{t:?}"));
        assert_eq!(all, ord);
    }

    #[test]
    fn labels_are_friendly() {
        assert_eq!(Theme::System.label(), "Follow system");
        assert_eq!(Theme::OledBlack.label(), "OLED Black");
        assert_eq!(Theme::GentleGecko.label(), "Gentle Gecko");
    }

    #[test]
    fn default_is_system() {
        assert_eq!(Theme::default(), Theme::System);
    }

    #[test]
    fn serde_roundtrips_all_13() {
        for t in Theme::ALL {
            let s = toml::to_string(&Wrap { theme: t }).unwrap();
            let back: Wrap = toml::from_str(&s).unwrap();
            assert_eq!(back.theme, t);
        }
    }

    #[test]
    fn legacy_theme_names_load() {
        for name in ["System", "Dark", "Light"] {
            let w: Wrap = toml::from_str(&format!("theme = \"{name}\"")).unwrap();
            let _ = w.theme;
        }
    }

    #[derive(serde::Serialize, serde::Deserialize)]
    struct Wrap {
        theme: Theme,
    }

    fn c(hex: u32) -> Color32 {
        Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
    }

    #[test]
    fn stops_match_grexa_named_themes() {
        let p = Theme::GentleGecko.palette(true);
        assert_eq!(p.bg, c(0x000000));
        assert_eq!(p.secondary, c(0x003322));
        assert_eq!(p.tertiary, c(0x00593D));
        assert_eq!(p.text, c(0xFFFFFF));
        assert_eq!(p.accent, c(0x00B86B));

        let v = Theme::Vibes.palette(true);
        assert_eq!(
            (v.bg, v.secondary, v.tertiary, v.text, v.accent),
            (
                c(0x0F0F1E),
                c(0x1E1E3C),
                c(0xCC00FF),
                c(0x00FFCC),
                c(0xFFCC00)
            )
        );

        let o = Theme::OledBlack.palette(true);
        assert_eq!(
            (o.bg, o.secondary, o.tertiary, o.text, o.accent),
            (
                c(0x000000),
                c(0x050505),
                c(0x111111),
                c(0xF5F5F5),
                c(0x2D7FF9)
            )
        );
    }

    #[test]
    fn light_dark_pin_bg_and_text_with_default_accent() {
        let l = Theme::Light.palette(false);
        assert_eq!(l.bg, c(0xF5F5F5));
        assert_eq!(l.text, c(0x1A1A1A));
        assert_eq!(l.accent, c(0x2D7FF9));
        assert!(!l.is_dark);

        let d = Theme::Dark.palette(true);
        assert_eq!(d.bg, c(0x181818));
        assert_eq!(d.text, c(0xF5F5F5));
        assert!(d.is_dark);
    }

    #[test]
    fn system_uses_hint_for_is_dark() {
        assert!(Theme::System.palette(true).is_dark);
        assert!(!Theme::System.palette(false).is_dark);
    }

    #[test]
    fn named_themes_are_dark() {
        for t in [
            Theme::GentleGecko,
            Theme::Dreams,
            Theme::Tiefling,
            Theme::OledBlack,
        ] {
            assert!(t.palette(false).is_dark, "{t:?} should be dark");
        }
    }

    #[test]
    fn derived_accents_have_expected_alpha() {
        let p = Theme::Dark.palette(true);
        assert_eq!(p.accent_mute.a(), 46); // 0.18
        assert_eq!(p.accent_mute_strong.a(), 71); // 0.28
        assert_eq!(p.accent_ring.a(), 102); // 0.40
    }

    #[test]
    fn every_theme_text_is_readable_on_bg() {
        for t in Theme::ALL {
            let p = t.palette(true);
            let r = derive::contrast_ratio(p.text, p.bg);
            assert!(r >= 3.0, "{t:?} text/bg contrast too low: {r}");
        }
    }

    #[test]
    fn oled_black_muted_text_is_readable_on_sidebar() {
        // Section headers and the six-dot reorder grip are painted with
        // `separator_strong` over the sidebar surface (`secondary`). On OLED Black
        // the near-black secondary made the 22 % white text nearly invisible.
        let p = Theme::OledBlack.palette(true);
        let effective = derive::tint(p.secondary, p.separator_strong);
        let r = derive::contrast_ratio(effective, p.secondary);
        assert!(
            r >= 3.0,
            "OLED Black muted text on sidebar contrast too low: {r}"
        );
    }

    #[test]
    fn light_muted_text_is_readable_on_sidebar() {
        // Light has the inverse issue: the pale sidebar surface makes the
        // default 16 % black muted labels wash out.
        let p = Theme::Light.palette(false);
        let effective = derive::tint(p.secondary, p.separator_strong);
        let r = derive::contrast_ratio(effective, p.secondary);
        assert!(
            r >= 3.0,
            "Light muted text on sidebar contrast too low: {r}"
        );
    }

    #[test]
    fn dark_muted_text_is_readable_on_sidebar() {
        // Dark has the same low-contrast issue as OLED Black on its dark gray
        // sidebar surface.
        let p = Theme::Dark.palette(true);
        let effective = derive::tint(p.secondary, p.separator_strong);
        let r = derive::contrast_ratio(effective, p.secondary);
        assert!(r >= 3.0, "Dark muted text on sidebar contrast too low: {r}");
    }

    #[test]
    fn every_dark_theme_muted_text_is_readable_on_sidebar() {
        for t in Theme::ALL {
            let p = t.palette(true);
            if !p.is_dark {
                continue;
            }
            let effective = derive::tint(p.secondary, p.separator_strong);
            let r = derive::contrast_ratio(effective, p.secondary);
            assert!(
                r >= 3.0,
                "{t:?} muted text on sidebar contrast too low: {r}"
            );
        }
    }

    #[test]
    fn resolve_system_defaults_dark_without_os_signal() {
        let ctx = egui::Context::default();
        // No system theme set → System resolves dark.
        let p = resolve(Theme::System, &ctx);
        assert!(p.is_dark);
    }
}
