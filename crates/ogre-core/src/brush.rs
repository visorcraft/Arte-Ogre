// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Brush input and settings shared by the paint tools and the vector renderer.
//!
//! This module keeps the headless document model (`ogre-core`) independent of
//! any vector-graphics library.  `ogre-vector` consumes these types when it
//! converts raw input samples into a smoothed stroke path.

use glam::Vec2;

use crate::buffer::TiledBuffer;
use crate::coord::IVec2;
use crate::pixel::Rgba32F;

/// A single tablet/mouse input sample in document-space pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InputSample {
    /// Position in document pixels.  The origin is at the top-left with +y down.
    pub pos: Vec2,
    /// Normalised pressure in `[0, 1]`.  Mouse input should report `1.0`.
    pub pressure: f32,
}

impl InputSample {
    /// Create a sample with the default pressure (`1.0`).
    pub const fn new(pos: Vec2) -> Self {
        Self { pos, pressure: 1.0 }
    }

    /// Create a sample with explicit pressure.
    pub const fn with_pressure(pos: Vec2, pressure: f32) -> Self {
        Self { pos, pressure }
    }
}

/// Settings that describe a round brush tip.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BrushSettings {
    /// Base brush diameter in pixels.
    pub size: f32,
    /// How hard the brush edge is.  `0.0` is very soft, `1.0` is fully hard.
    pub hardness: f32,
    /// Maximum opacity applied by one stamp (`0.0..=1.0`).
    pub opacity: f32,
    /// How much colour is deposited per stamp (`0.0..=1.0`).
    pub flow: f32,
    /// Stamp distance as a fraction of the brush diameter.
    pub spacing: f32,
    /// When `true`, brush size scales with pressure.
    pub pressure_size: bool,
    /// When `true`, brush opacity scales with pressure.
    pub pressure_opacity: bool,
}

impl Default for BrushSettings {
    fn default() -> Self {
        Self {
            size: 10.0,
            hardness: 0.5,
            opacity: 1.0,
            flow: 1.0,
            spacing: 0.25,
            pressure_size: true,
            pressure_opacity: true,
        }
    }
}

impl BrushSettings {
    /// Clamp all fields to sensible ranges.
    pub fn sanitised(self) -> Self {
        Self {
            size: self.size.max(0.0),
            hardness: self.hardness.clamp(0.0, 1.0),
            opacity: self.opacity.clamp(0.0, 1.0),
            flow: self.flow.clamp(0.0, 1.0),
            spacing: self.spacing.clamp(0.0, 1.0),
            pressure_size: self.pressure_size,
            pressure_opacity: self.pressure_opacity,
        }
    }

    /// Brush radius in pixels.
    pub fn radius(self) -> f32 {
        self.size.max(0.0) / 2.0
    }

    /// Map pressure to stamp width for this brush.
    pub fn width_at_pressure(self, pressure: f32) -> f32 {
        if self.pressure_size {
            self.size.max(0.0) * pressure.clamp(0.0, 1.0)
        } else {
            self.size.max(0.0)
        }
    }

    /// Map pressure to stamp opacity for this brush.
    pub fn opacity_at_pressure(self, pressure: f32) -> f32 {
        if self.pressure_opacity {
            self.opacity.clamp(0.0, 1.0) * pressure.clamp(0.0, 1.0)
        } else {
            self.opacity.clamp(0.0, 1.0)
        }
    }

    /// Distance between consecutive stamps in pixels.
    ///
    /// A spacing of `0.0` would produce an infinite number of stamps, so the
    /// result is clamped to at least one pixel.
    pub fn step_distance(self) -> f32 {
        let diameter = self.size.max(0.0);
        (diameter * self.spacing.clamp(0.0, 1.0)).max(1.0)
    }

    /// Radial alpha falloff for a round brush stamp.
    ///
    /// `distance` is the distance from the stamp centre in pixels.  The result
    /// is in `[0, 1]` and is `1.0` at the centre and `0.0` at `radius`.  Higher
    /// `hardness` keeps the alpha high further toward the edge; a hardness of
    /// `1.0` is fully hard (opaque inside the radius, transparent outside).
    pub fn falloff(self, distance: f32) -> f32 {
        let radius = self.radius();
        if radius <= 0.0 {
            return 0.0;
        }
        falloff_normalized(distance / radius, self.hardness)
    }
}

/// Hardness-shaped radial alpha for a normalized position `t`, where `t == 0`
/// is the stamp centre and `t == 1` is the stamp edge.
///
/// Callers normalize `distance` against the *effective* (e.g. pressure-scaled)
/// stamp radius so the soft edge always decays to zero at the actual stamp
/// boundary. `hardness == 0` gives a linear falloff; as hardness approaches 1
/// the curve stays opaque longer, reaching a hard edge at `hardness == 1`.
fn falloff_normalized(t: f32, hardness: f32) -> f32 {
    if t <= 0.0 {
        return 1.0;
    }
    if t >= 1.0 {
        return 0.0;
    }
    let hardness = hardness.clamp(0.0, 1.0);
    if hardness >= 1.0 {
        return 1.0;
    }
    let shape = 1.0 / (1.0 - hardness).max(0.001);
    (1.0 - t.powf(shape)).clamp(0.0, 1.0)
}

/// How a stroke should be composited into the destination layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaintMode {
    /// Normal brush: stamp colour is blended over the existing pixels.
    Brush,
    /// Eraser: the stamp reduces the existing pixel's alpha.
    Eraser,
}

/// Rasterize a single round brush stamp into `buffer`.
///
/// `center` is in layer-local pixel coordinates.  The stamp is clipped to the
/// pixel grid using the brush radius and hardness falloff.
pub fn stamp(
    buffer: &mut TiledBuffer,
    center: Vec2,
    settings: &BrushSettings,
    color: Rgba32F,
    pressure: f32,
    mode: PaintMode,
) {
    let settings = settings.sanitised();
    let radius = settings.width_at_pressure(pressure) / 2.0;
    if radius <= 0.0 {
        return;
    }

    let stamp_alpha = settings.opacity_at_pressure(pressure) * settings.flow;
    if stamp_alpha <= 0.0 {
        return;
    }

    // The eraser ignores the colour and uses the stamp alpha directly.
    let color_alpha = match mode {
        PaintMode::Brush => color.a,
        PaintMode::Eraser => 1.0,
    };

    let min_x = (center.x - radius).floor() as i32;
    let min_y = (center.y - radius).floor() as i32;
    let max_x = (center.x + radius).ceil() as i32;
    let max_y = (center.y + radius).ceil() as i32;

    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let dx = x as f32 + 0.5 - center.x;
            let dy = y as f32 + 0.5 - center.y;
            let distance = (dx * dx + dy * dy).sqrt();
            if distance >= radius {
                continue;
            }
            // Normalize against the *scaled* stamp radius so the soft edge
            // decays to zero at the actual boundary, not the unscaled base
            // radius (which left a hard cliff when pressure shrank the stamp).
            let alpha = falloff_normalized(distance / radius, settings.hardness)
                * stamp_alpha
                * color_alpha;
            if alpha <= 0.0 {
                continue;
            }
            let p = IVec2::new(x, y);
            match mode {
                PaintMode::Brush => {
                    let src = Rgba32F::new(color.r, color.g, color.b, alpha);
                    let dst = buffer.get_pixel(p);
                    let blended = crate::compositor::blend_pixel(
                        crate::layer::BlendMode::Normal,
                        dst,
                        src,
                        1.0,
                    );
                    buffer.set_pixel(p, blended);
                }
                PaintMode::Eraser => {
                    let mut dst = buffer.get_pixel(p);
                    dst.a *= 1.0 - alpha;
                    if dst.a <= 0.0 {
                        dst = Rgba32F::TRANSPARENT;
                    }
                    buffer.set_pixel(p, dst);
                }
            }
        }
    }
}

/// Rasterize a stroke defined by `samples` into `buffer`.
///
/// Samples are in layer-local pixel coordinates.  Stamps are placed along the
/// polyline with spacing controlled by `settings.spacing`, and width/opacity
/// are interpolated between neighbouring samples.
pub fn rasterize_stroke(
    buffer: &mut TiledBuffer,
    samples: &[InputSample],
    settings: &BrushSettings,
    color: Rgba32F,
    mode: PaintMode,
) {
    if samples.len() < 2 {
        if let Some(s) = samples.first() {
            stamp(buffer, s.pos, settings, color, s.pressure, mode);
        }
        return;
    }

    let settings = settings.sanitised();
    let step = settings.step_distance();
    let mut dist_acc = 0.0;

    for i in 0..samples.len() - 1 {
        let s0 = samples[i];
        let s1 = samples[i + 1];
        let seg = s1.pos - s0.pos;
        let seg_len = seg.length();
        if seg_len == 0.0 {
            continue;
        }
        let dir = seg / seg_len;

        let mut d = dist_acc;
        while d < seg_len {
            let t = d / seg_len;
            let pos = s0.pos + dir * d;
            let pressure = s0.pressure + (s1.pressure - s0.pressure) * t;
            stamp(buffer, pos, &settings, color, pressure, mode);
            d += step;
        }
        dist_acc = d - seg_len;
    }

    // Ensure the very last sample is always stamped.
    if let Some(last) = samples.last() {
        stamp(buffer, last.pos, &settings, color, last.pressure, mode);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_sample_defaults_to_full_pressure() {
        let s = InputSample::new(Vec2::new(100.0, 200.0));
        assert_eq!(s.pos, Vec2::new(100.0, 200.0));
        assert_eq!(s.pressure, 1.0);
    }

    #[test]
    fn brush_sanitise_clamps_fields() {
        let b = BrushSettings {
            size: -5.0,
            hardness: 2.0,
            opacity: -0.1,
            flow: 1.5,
            spacing: -0.5,
            ..Default::default()
        }
        .sanitised();
        assert_eq!(b.size, 0.0);
        assert_eq!(b.hardness, 1.0);
        assert_eq!(b.opacity, 0.0);
        assert_eq!(b.flow, 1.0);
        assert_eq!(b.spacing, 0.0);
    }

    #[test]
    fn width_respects_pressure_size_toggle() {
        let mut b = BrushSettings {
            size: 20.0,
            pressure_size: true,
            ..Default::default()
        };
        assert!((b.width_at_pressure(0.5) - 10.0).abs() < f32::EPSILON);
        assert!((b.width_at_pressure(0.0) - 0.0).abs() < f32::EPSILON);

        b.pressure_size = false;
        assert!((b.width_at_pressure(0.5) - 20.0).abs() < f32::EPSILON);
    }

    #[test]
    fn opacity_respects_pressure_opacity_toggle() {
        let mut b = BrushSettings {
            opacity: 0.5,
            pressure_opacity: true,
            ..Default::default()
        };
        assert!((b.opacity_at_pressure(0.5) - 0.25).abs() < f32::EPSILON);

        b.pressure_opacity = false;
        assert!((b.opacity_at_pressure(0.5) - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn spacing_step_is_size_times_spacing() {
        let mut b = BrushSettings {
            size: 20.0,
            spacing: 0.25,
            ..Default::default()
        };
        assert!((b.step_distance() - 5.0).abs() < f32::EPSILON);

        b.spacing = 0.0;
        assert_eq!(b.step_distance(), 1.0);
    }

    #[test]
    fn falloff_is_one_at_centre_and_zero_at_edge() {
        let b = BrushSettings::default();
        assert_eq!(b.falloff(0.0), 1.0);
        assert_eq!(b.falloff(b.radius()), 0.0);
        assert_eq!(b.falloff(b.radius() + 1.0), 0.0);
    }

    #[test]
    fn harder_brush_keeps_higher_alpha_mid_radius() {
        let soft = BrushSettings {
            hardness: 0.0,
            ..Default::default()
        };
        let hard = BrushSettings {
            hardness: 1.0,
            ..Default::default()
        };
        let mid = soft.radius() * 0.5;
        assert!(hard.falloff(mid) > soft.falloff(mid));
    }

    #[test]
    fn hard_stamp_writes_opaque_pixel() {
        let mut buffer = TiledBuffer::new();
        let settings = BrushSettings {
            size: 20.0,
            hardness: 1.0,
            opacity: 1.0,
            pressure_size: false,
            pressure_opacity: false,
            ..Default::default()
        };

        let center = Vec2::new(1000.5, 700.5);
        let red = Rgba32F::new(1.0, 0.0, 0.0, 1.0);
        stamp(&mut buffer, center, &settings, red, 1.0, PaintMode::Brush);

        let px = buffer.get_pixel(IVec2::new(1000, 700));
        assert!((px.r - 1.0).abs() < 1e-4);
        assert!((px.g - 0.0).abs() < 1e-4);
        assert!((px.a - 1.0).abs() < 1e-4);
    }

    #[test]
    fn soft_stamp_falloff_matches_hardness_curve() {
        let mut buffer = TiledBuffer::new();
        let settings = BrushSettings {
            size: 20.0,
            hardness: 0.5,
            opacity: 1.0,
            pressure_size: false,
            pressure_opacity: false,
            ..Default::default()
        };

        let center = Vec2::new(500.5, 500.5);
        let red = Rgba32F::new(1.0, 0.0, 0.0, 1.0);
        stamp(&mut buffer, center, &settings, red, 1.0, PaintMode::Brush);

        let radius = settings.radius();
        for offset in [0, 2, 4, 7] {
            let p = IVec2::new(500 + offset, 500);
            let distance = offset as f32;
            if distance >= radius {
                assert_eq!(buffer.get_pixel(p).a, 0.0);
                continue;
            }
            let expected = settings.falloff(distance);
            let actual = buffer.get_pixel(p).a;
            assert!(
                (actual - expected).abs() < 1e-4,
                "offset {}: expected {}, got {}",
                offset,
                expected,
                actual
            );
        }
    }

    #[test]
    fn pressure_scaled_soft_stamp_decays_at_scaled_edge() {
        // Default-style soft brush with pressure-driven size. At pressure 0.5 the
        // stamp radius is half the base radius, and the soft falloff must decay
        // to ~0 at that *scaled* edge — not be normalized against the unscaled
        // base radius (which produced a hard cliff).
        let mut buffer = TiledBuffer::new();
        let settings = BrushSettings {
            size: 40.0, // base radius 20
            hardness: 0.5,
            opacity: 1.0,
            flow: 1.0,
            pressure_size: true,
            pressure_opacity: false,
            ..Default::default()
        };
        let center = Vec2::new(500.5, 500.5);
        let red = Rgba32F::new(1.0, 0.0, 0.0, 1.0);
        // pressure 0.5 -> stamp width 20 -> scaled radius 10.
        stamp(&mut buffer, center, &settings, red, 0.5, PaintMode::Brush);

        // Center stays near-opaque.
        assert!(buffer.get_pixel(IVec2::new(500, 500)).a > 0.9);
        // Near the scaled edge (distance 9 of 10) the alpha must have decayed.
        // With the bug (normalized vs base radius 20) this is ~0.80.
        let near_edge = buffer.get_pixel(IVec2::new(509, 500)).a;
        assert!(
            near_edge < 0.3,
            "soft edge must decay toward 0 at the scaled radius, got {near_edge}"
        );
        // Beyond the scaled radius nothing is painted.
        assert_eq!(buffer.get_pixel(IVec2::new(511, 500)).a, 0.0);
    }

    #[test]
    fn stroke_along_segment_deposits_pixels() {
        let mut buffer = TiledBuffer::new();
        let settings = BrushSettings {
            size: 10.0,
            hardness: 1.0,
            opacity: 1.0,
            spacing: 0.5,
            pressure_size: false,
            pressure_opacity: false,
            ..Default::default()
        };

        let samples = [
            InputSample::new(Vec2::new(100.0, 100.0)),
            InputSample::new(Vec2::new(120.0, 100.0)),
        ];
        let red = Rgba32F::new(1.0, 0.0, 0.0, 1.0);
        rasterize_stroke(&mut buffer, &samples, &settings, red, PaintMode::Brush);

        assert!(buffer.get_pixel(IVec2::new(100, 100)).a > 0.0);
        assert!(buffer.get_pixel(IVec2::new(110, 100)).a > 0.0);
        assert!(buffer.get_pixel(IVec2::new(120, 100)).a > 0.0);
    }

    #[test]
    fn eraser_reduces_alpha() {
        let mut buffer = TiledBuffer::new();
        let settings = BrushSettings {
            size: 20.0,
            hardness: 1.0,
            opacity: 1.0,
            pressure_size: false,
            pressure_opacity: false,
            ..Default::default()
        };

        let center = Vec2::new(200.5, 200.5);
        let red = Rgba32F::new(1.0, 0.0, 0.0, 1.0);
        stamp(&mut buffer, center, &settings, red, 1.0, PaintMode::Brush);
        assert_eq!(buffer.get_pixel(IVec2::new(200, 200)).a, 1.0);

        stamp(&mut buffer, center, &settings, red, 1.0, PaintMode::Eraser);
        assert!(buffer.get_pixel(IVec2::new(200, 200)).a < 1.0);
    }
}
