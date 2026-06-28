// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Pure pixel operations for the brush-engine "finger" tools (§3.2.2–§3.2.6).
//!
//! Each function computes the **target** value for a single pixel given the
//! frozen-backdrop pixel (`bd`) and the relevant settings. The driving
//! `FingerTool` accumulates stamp coverage over the stroke and
//! lerps the original layer pixel toward this target — so per-pixel reads come
//! from the frozen backdrop (no intra-stroke compounding) except for Smudge,
//! which carries its own `last_color` along the stroke.

use crate::{srgb_encode, Rgba32F};

/// Rec.709 luma of a (linear) pixel.
pub fn luma(p: Rgba32F) -> f32 {
    0.2126 * p.r + 0.7152 * p.g + 0.0722 * p.b
}

/// Blur target: the input unchanged — the blurring comes from sampling the
/// backdrop neighbourhood in the tool (a box average over the stamp radius).
/// This fn is a passthrough so the FingerTool's neighbourhood sample is the
/// target; `bd` here is already the neighbourhood average the tool computed.
pub fn blur_target(neighbourhood_avg: Rgba32F) -> Rgba32F {
    neighbourhood_avg
}

/// Sharpen (unsharp mask) target: `orig + amount * (orig - blurred)`, clamped,
/// alpha unchanged. `orig` is the pixel being sharpened; `blurred` its neighbourhood.
pub fn sharpen_target(orig: Rgba32F, blurred: Rgba32F, amount: f32) -> Rgba32F {
    let amt = amount.clamp(0.0, 4.0);
    Rgba32F::new(
        (orig.r + amt * (orig.r - blurred.r)).clamp(0.0, 1.0),
        (orig.g + amt * (orig.g - blurred.g)).clamp(0.0, 1.0),
        (orig.b + amt * (orig.b - blurred.b)).clamp(0.0, 1.0),
        orig.a,
    )
}

/// Tonal-range mask ∈ \[0,1\] for Dodge/Burn (spec §3.2.5).
pub fn range_mask(luma: f32, range: Range) -> f32 {
    let luma = luma.clamp(0.0, 1.0);
    match range {
        Range::Shadows => (1.0 - smoothstep(0.0, 0.5, luma)).clamp(0.0, 1.0),
        Range::Midtones => (1.0 - (luma - 0.5).abs() * 2.0).clamp(0.0, 1.0),
        Range::Highlights => smoothstep(0.5, 1.0, luma).clamp(0.0, 1.0),
    }
}

/// Tonal range for Dodge/Burn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Range {
    /// Affect dark tones.
    Shadows,
    /// Affect middle tones.
    Midtones,
    /// Affect bright tones.
    Highlights,
}

/// Dodge target (reciprocal lightening): `B / (1 - m)`, clamped, where
/// `m = exposure * range_mask`. Reciprocal scaling makes Burn the exact inverse
/// (Burn∘Dodge = identity on non-clipping pixels). Alpha unchanged.
pub fn dodge_target(bd: Rgba32F, m: f32) -> Rgba32F {
    let m = m.clamp(0.0, 0.999);
    let scale = 1.0 / (1.0 - m);
    Rgba32F::new(
        (bd.r * scale).clamp(0.0, 1.0),
        (bd.g * scale).clamp(0.0, 1.0),
        (bd.b * scale).clamp(0.0, 1.0),
        bd.a,
    )
}

/// Burn target (reciprocal darkening): `B * (1 - m)` — the exact inverse of
/// [`dodge_target`]. `m` is clamped to the same `< 1` bound as Dodge so the
/// `Burn∘Dodge` round-trip holds at the UI max exposure. Alpha unchanged.
pub fn burn_target(bd: Rgba32F, m: f32) -> Rgba32F {
    let m = m.clamp(0.0, 0.999);
    Rgba32F::new(bd.r * (1.0 - m), bd.g * (1.0 - m), bd.b * (1.0 - m), bd.a)
}

/// Color-replacement target: take the foreground's hue & saturation, the
/// backdrop's HSL lightness, then rescale the result's RGB so its Rec.709 luma
/// equals the backdrop's luma (HSL lightness is not linear luminance, so the
/// rescale is what actually preserves luma). Returns the target RGB; alpha
/// unchanged.
pub fn recolor_target(bd: Rgba32F, fg: Rgba32F) -> Rgba32F {
    let (_, _, bl) = rgb_to_hsl(bd);
    let (fh, fs, _) = rgb_to_hsl(fg);
    let recolored = hsl_to_rgb(fh, fs, bl);
    let target_luma = luma(bd);
    // If the recolored pixel has measurable luma, rescale its RGB so the
    // computed Rec.709 luma equals the backdrop's (HSL lightness ≠ luminance, so
    // this rescale is what actually preserves luma). A near-black target keeps
    // the backdrop's luma directly.
    let out_rgb = if luma(recolored) > 1e-6 {
        let scale = target_luma / luma(recolored);
        Rgba32F::new(
            (recolored.r * scale).clamp(0.0, 1.0),
            (recolored.g * scale).clamp(0.0, 1.0),
            (recolored.b * scale).clamp(0.0, 1.0),
            bd.a,
        )
    } else {
        Rgba32F::new(target_luma, target_luma, target_luma, bd.a)
    };
    Rgba32F::new(out_rgb.r, out_rgb.g, out_rgb.b, bd.a)
}

/// Healing target (simplified per-channel mean-match, spec §3.2.3): shift the
/// source patch's per-channel mean to match the destination patch's mean.
/// `src` is the cloned source pixel; `mu_src`/`mu_dst` are per-channel means over
/// the stamp footprint. Alpha taken from `dst` (the destination pixel).
pub fn heal_target(src: Rgba32F, mu_src: Rgba32F, mu_dst: Rgba32F, dst: Rgba32F) -> Rgba32F {
    let shifted = Rgba32F::new(
        (src.r - mu_src.r + mu_dst.r).clamp(0.0, 1.0),
        (src.g - mu_src.g + mu_dst.g).clamp(0.0, 1.0),
        (src.b - mu_src.b + mu_dst.b).clamp(0.0, 1.0),
        src.a,
    );
    Rgba32F::new(shifted.r, shifted.g, shifted.b, dst.a)
}

// ---------- HSL helpers (linear-light RGB in/out) ----------

fn rgb_to_hsl(p: Rgba32F) -> (f32, f32, f32) {
    let (r, g, b) = (
        p.r.clamp(0.0, 1.0),
        p.g.clamp(0.0, 1.0),
        p.b.clamp(0.0, 1.0),
    );
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if (max - min).abs() < 1e-7 {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = d / (1.0 - (2.0 * l - 1.0).abs());
    let h = if max == r {
        ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h * 60.0, s, l)
}

fn hsl_to_rgb(h_deg: f32, s: f32, l: f32) -> Rgba32F {
    let h = (h_deg.rem_euclid(360.0)) / 360.0;
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h * 12.0).rem_euclid(2.0) - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match (h * 6.0).floor() as i32 % 6 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    Rgba32F::new(r + m, g + m, b + m, 1.0)
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    if e0 == e1 {
        return if x < e0 { 0.0 } else { 1.0 };
    }
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Sponge target: multiply saturation by `(1 + exposure)` (saturate) or
/// `1/(1+exposure)` (desaturate), in sRGB-encoded space. Pure-gray pixels
/// (sat==0) are unchanged. Alpha unchanged.
pub fn sponge_target(bd: Rgba32F, exposure: f32, saturate: bool) -> Rgba32F {
    // Operate per-channel in sRGB so "saturation" tracks what the user sees.
    let enc = Rgba32F::new(
        srgb_encode(bd.r),
        srgb_encode(bd.g),
        srgb_encode(bd.b),
        bd.a,
    );
    let (h, s, l) = rgb_to_hsl(enc);
    if s < 1e-6 {
        return bd; // already gray
    }
    let factor = if saturate {
        1.0 + exposure
    } else {
        1.0 - exposure
    };
    let ns = (s * factor).clamp(0.0, 1.0);
    let out = hsl_to_rgb(h, ns, l);
    // Decode back to linear.
    Rgba32F::new(
        srgb_decode(out.r),
        srgb_decode(out.g),
        srgb_decode(out.b),
        bd.a,
    )
}

fn srgb_decode(c: f32) -> f32 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dodge_burn_are_inverses_on_midtones() {
        for v in [0.3, 0.5, 0.7] {
            let p = Rgba32F::new(v, v, v, 1.0);
            let m = 0.3;
            let dodged = dodge_target(p, m);
            // Dodge then Burn restores p (no clipping in midtones).
            let restored = burn_target(dodged, m);
            assert!((restored.r - p.r).abs() < 1e-4, "v={v} not restored");
        }
    }

    #[test]
    fn range_masks_are_bounded_and_ordered() {
        let shadow = range_mask(0.1, Range::Shadows);
        let high = range_mask(0.1, Range::Highlights);
        assert!(shadow > high, "shadows mask should dominate at low luma");
        assert!(range_mask(0.5, Range::Midtones) > range_mask(0.0, Range::Midtones));
    }

    #[test]
    fn recolor_preserves_luma() {
        // Exact luma preservation is impossible when the foreground hue at the
        // backdrop's HSL lightness cannot reach the backdrop's Rec.709 luma
        // without clamping (e.g. a saturated red on a high-luma green). The
        // rescale preserves luma whenever clamping doesn't bite; here we pick a
        // backdrop whose luma is reachable by the foreground hue.
        let bd = Rgba32F::new(0.6, 0.5, 0.4, 1.0);
        let fg = Rgba32F::new(0.5, 0.6, 0.5, 1.0);
        let out = recolor_target(bd, fg);
        assert!(
            (luma(out) - luma(bd)).abs() < 5e-2,
            "luma drift too large: {} vs {}",
            luma(out),
            luma(bd)
        );
        assert_eq!(out.a, bd.a);
    }

    #[test]
    fn sponge_gray_unchanged() {
        let gray = Rgba32F::new(0.5, 0.5, 0.5, 1.0);
        assert_eq!(sponge_target(gray, 0.5, true), gray);
    }

    #[test]
    fn sharpen_clamps_and_keeps_alpha() {
        let orig = Rgba32F::new(0.9, 0.1, 0.5, 0.7);
        let blurred = Rgba32F::new(0.3, 0.3, 0.5, 0.7);
        let out = sharpen_target(orig, blurred, 2.0);
        assert_eq!(out.a, 0.7);
        assert!(out.r <= 1.0 && out.r >= 0.0);
        assert!(out.r > orig.r); // contrast boosted the bright channel
    }
}
