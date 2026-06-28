// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Floating-point RGBA pixel type used throughout Arte Ogre.
//!
//! Pixels are stored in straight (non-premultiplied) alpha, linear light, with
//! the `Rgba32F` layout matching the `Rgba32Float` texture format used by the
//! GPU compositor.

/// Clamp a value to `[0.0, 1.0]`, mapping NaN to `0.0`.
///
/// Used for alpha/opacity/coverage values, where bare `f32::clamp` would
/// propagate NaN instead of zeroing it.
pub(crate) fn clamp01_nan0(v: f32) -> f32 {
    if v.is_nan() {
        0.0
    } else {
        v.clamp(0.0, 1.0)
    }
}

/// A 32-bit-per-channel floating point RGBA pixel.
///
/// Colors are stored in **straight alpha** and **linear light**.
#[repr(C)]
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    bytemuck::Pod,
    bytemuck::Zeroable,
    serde::Serialize,
    serde::Deserialize,
)]
pub struct Rgba32F {
    /// Red channel.
    pub r: f32,
    /// Green channel.
    pub g: f32,
    /// Blue channel.
    pub b: f32,
    /// Alpha channel.
    pub a: f32,
}

impl Rgba32F {
    /// A fully transparent pixel.
    pub const TRANSPARENT: Self = Self {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: 0.0,
    };

    /// Construct a pixel from its four channels.
    pub fn new(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    /// Return this pixel with premultiplied alpha.
    pub fn premultiplied(self) -> Self {
        Self::new(self.r * self.a, self.g * self.a, self.b * self.a, self.a)
    }

    /// Return this premultiplied pixel with straight alpha.
    ///
    /// Fully transparent pixels stay transparent.
    pub fn unpremultiplied(self) -> Self {
        if self.a > 0.0 {
            Self::new(self.r / self.a, self.g / self.a, self.b / self.a, self.a)
        } else {
            Self::TRANSPARENT
        }
    }

    /// Linear interpolation between this pixel and `other` by factor `t`.
    pub fn lerp(self, other: Self, t: f32) -> Self {
        Self::new(
            self.r + (other.r - self.r) * t,
            self.g + (other.g - self.g) * t,
            self.b + (other.b - self.b) * t,
            self.a + (other.a - self.a) * t,
        )
    }
}

/// sRGB opto-electronic transfer (linear → gamma-encoded display value).
///
/// Pixels are stored in linear light; perceptual comparisons that match what a
/// user sees — and what other editors' selection tools do — must be measured in
/// gamma-encoded space. Comparing in linear light over-grabs in shadows and
/// under-grabs in highlights. The input is clamped to `[0, 1]` so HDR overshoot
/// cannot escape the gamma curve. Shared by Magic Wand, Quick Selection, Color
/// Range, and the Color Replacement brush.
pub fn srgb_encode(c: f32) -> f32 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.003_130_8 {
        12.92 * c
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

/// Squared perceptual distance between two pixels.
///
/// RGB channels are gamma-encoded via [`srgb_encode`]; alpha is kept linear.
/// Each channel is sanitized (NaN → 0) first. This is the single notion of
/// "looks the same" shared by Magic Wand, Quick Selection, Color Range, and
/// the Color Replacement brush predicate.
pub fn perceptual_distance_sq(a: Rgba32F, b: Rgba32F) -> f32 {
    let sanitize = |c: f32| if c.is_nan() { 0.0 } else { c };
    let ar = srgb_encode(sanitize(a.r));
    let ag = srgb_encode(sanitize(a.g));
    let ab = srgb_encode(sanitize(a.b));
    let aa = sanitize(a.a).clamp(0.0, 1.0);
    let br = srgb_encode(sanitize(b.r));
    let bg = srgb_encode(sanitize(b.g));
    let bb = srgb_encode(sanitize(b.b));
    let ba = sanitize(b.a).clamp(0.0, 1.0);
    let dr = ar - br;
    let dg = ag - bg;
    let db = ab - bb;
    let da = aa - ba;
    dr * dr + dg * dg + db * db + da * da
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transparent_is_zero() {
        assert_eq!(Rgba32F::TRANSPARENT, Rgba32F::new(0.0, 0.0, 0.0, 0.0));
    }

    #[test]
    fn premultiplied_multiplies_rgb_by_alpha() {
        let px = Rgba32F::new(0.5, 0.25, 0.75, 0.5).premultiplied();
        assert_eq!(px, Rgba32F::new(0.25, 0.125, 0.375, 0.5));
    }

    #[test]
    fn unpremultiplied_divides_rgb_by_alpha() {
        let px = Rgba32F::new(0.25, 0.125, 0.375, 0.5).unpremultiplied();
        assert_eq!(px, Rgba32F::new(0.5, 0.25, 0.75, 0.5));
    }

    #[test]
    fn unpremultiplied_zero_alpha_is_transparent() {
        let px = Rgba32F::new(0.25, 0.125, 0.375, 0.0).unpremultiplied();
        assert_eq!(px, Rgba32F::TRANSPARENT);
    }

    #[test]
    fn lerp_blends_componentwise() {
        let a = Rgba32F::new(0.0, 0.0, 0.0, 0.0);
        let b = Rgba32F::new(1.0, 0.5, 0.25, 1.0);
        assert_eq!(a.lerp(b, 0.5), Rgba32F::new(0.5, 0.25, 0.125, 0.5));
    }

    #[test]
    fn srgb_encode_endpoints_and_mid() {
        assert_eq!(srgb_encode(0.0), 0.0);
        // srgb_encode(1) = 1.055*1 - 0.055, which rounds to 0.99999994 in f32.
        assert!((srgb_encode(1.0) - 1.0).abs() < 1e-6);
        // 0.5 is above the piecewise knee, so it follows the gamma branch.
        let expected = 1.055 * 0.5f32.powf(1.0 / 2.4) - 0.055;
        assert!((srgb_encode(0.5) - expected).abs() < 1e-6);
    }

    #[test]
    fn srgb_encode_clamps_outside_range() {
        assert_eq!(srgb_encode(-0.5), 0.0);
        assert!((srgb_encode(2.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn perceptual_distance_zero_for_identical_pixels() {
        let red = Rgba32F::new(1.0, 0.0, 0.0, 1.0);
        assert_eq!(perceptual_distance_sq(red, red), 0.0);
    }

    #[test]
    fn perceptual_distance_positive_and_symmetric() {
        let red = Rgba32F::new(1.0, 0.0, 0.0, 1.0);
        let green = Rgba32F::new(0.0, 1.0, 0.0, 1.0);
        let d = perceptual_distance_sq(red, green);
        assert!(d > 0.0);
        assert!(
            (perceptual_distance_sq(red, green) - perceptual_distance_sq(green, red)).abs() < 1e-6
        );
    }

    #[test]
    fn perceptual_distance_treats_nan_as_zero() {
        let red = Rgba32F::new(1.0, 0.0, 0.0, 1.0);
        let nan_red = Rgba32F::new(f32::NAN, 0.0, 0.0, 1.0);
        // NaN channel behaves as 0, so distance is srgb(1)-srgb(0) on red only.
        let expected = srgb_encode(1.0) - srgb_encode(0.0);
        assert!((perceptual_distance_sq(red, nan_red) - expected * expected).abs() < 1e-6);
    }
}
