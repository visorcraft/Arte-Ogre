// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Gradient fill operation (§3.2.1).
//!
//! Builds a per-pixel write list that paints a foreground→background gradient
//! along a document-space line, composited over the existing layer pixels. The
//! four standard kinds are supported; Conical wraps via `rem_euclid` so the full
//! circle is covered (a naive signed `atan2` + clamp collapses half the circle).

use glam::Vec2;

use crate::buffer::TiledBuffer;
use crate::compositor::blend_pixel;
use crate::coord::{IVec2, Rect};
use crate::layer::BlendMode;
use crate::pixel::Rgba32F;
use crate::selection::Selection;

/// The shape of a gradient.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum GradientKind {
    /// Axis-aligned linear blend along `p0 → p1`.
    Linear,
    /// Radial blend from `p0` outward, reaching `p1` at the radius.
    Radial,
    /// Angular blend around `p0`, with `p0 → p1` as the zero-angle direction.
    Conical,
    /// L1 (diamond) blend from `p0` reaching `p1` at the L1 radius.
    Diamond,
}

/// How the gradient parameter `t` behaves outside `[0, 1]`. Only affects
/// Linear/Radial/Diamond; Conical always wraps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub enum WrapMode {
    /// Clamp `t` to `[0, 1]`.
    #[default]
    Clamp,
    /// Repeat: `t = t.rem_euclid(1)` (sawtooth 0→1, 0→1, …).
    Repeat,
    /// Reflect: mirror at integer boundaries (triangle wave 0→1→0→1, …).
    Reflect,
}

impl WrapMode {
    /// Apply the wrap to a raw (possibly out-of-range) `t`.
    pub fn wrap(self, t: f32) -> f32 {
        match self {
            WrapMode::Clamp => t.clamp(0.0, 1.0),
            WrapMode::Repeat => t.rem_euclid(1.0),
            WrapMode::Reflect => {
                let f = t.rem_euclid(2.0);
                1.0 - (f - 1.0).abs()
            }
        }
    }
}

/// Gradient parameter `t ∈ [0,1]` (Conical in `[0,1)`) at document point `p` for
/// the line `p0 → p1` and the given `kind`. Linear/Radial/Diamond clamp to
/// `[0,1]`; Conical wraps via `rem_euclid(TAU)` so the full circle is covered.
pub fn gradient_t(p: Vec2, p0: Vec2, p1: Vec2, kind: GradientKind) -> f32 {
    let d = p1 - p0;
    let v = p - p0;
    match kind {
        GradientKind::Linear => {
            let dsq = d.x * d.x + d.y * d.y;
            if dsq <= 0.0 {
                0.0
            } else {
                // Raw (unclamped) parameter; `fill_gradient` applies the WrapMode.
                v.dot(d) / dsq
            }
        }
        GradientKind::Radial => {
            let dl = d.length();
            if dl <= 0.0 {
                0.0
            } else {
                v.length() / dl
            }
        }
        GradientKind::Conical => {
            // Zero-angle direction is `d` (p0 → p1). atan2(cross, dot) gives the
            // signed angle in [-π, π]; rem_euclid maps it onto [0, 2π) so no half
            // of the circle collapses to 0 under clamp.
            let cross = d.x * v.y - d.y * v.x;
            let dot = d.x * v.x + d.y * v.y;
            let theta = cross.atan2(dot).rem_euclid(std::f32::consts::TAU);
            theta / std::f32::consts::TAU
        }
        GradientKind::Diamond => {
            let dl1 = d.x.abs() + d.y.abs();
            if dl1 <= 0.0 {
                0.0
            } else {
                (v.x.abs() + v.y.abs()) / dl1
            }
        }
    }
}

/// Foreground→background color at parameter `t`, linear-light blend (the buffer
/// stores linear light, per C2). `reverse` flips the blend direction.
pub fn gradient_color(t: f32, fg: Rgba32F, bg: Rgba32F, reverse: bool) -> Rgba32F {
    let tp = if reverse { 1.0 - t } else { t };
    fg.lerp(bg, tp)
}

/// Fill the layer with a gradient along `p0 → p1` (document coordinates),
/// returning `(layer-local pixel, new composited value)` writes — the same
/// contract as [`crate::ops::fill_region`].
///
/// An **empty** selection means **no masking** (whole layer filled); a non-empty
/// selection clips each pixel to its `coverage_at(doc)` weight. Each gradient
/// pixel is composited over the existing layer pixel with
/// [`BlendMode::Normal`], weighted by `opacity * coverage`.
#[allow(clippy::too_many_arguments)] // gradient params are inherently a flat bundle
pub fn fill_gradient(
    buffer: &TiledBuffer,
    offset: IVec2,
    p0_doc: Vec2,
    p1_doc: Vec2,
    kind: GradientKind,
    fg: Rgba32F,
    bg: Rgba32F,
    reverse: bool,
    opacity: f32,
    selection: &Selection,
    canvas: Rect,
    wrap: WrapMode,
) -> Vec<(IVec2, Rgba32F)> {
    let mut out = Vec::new();
    let clip = !selection.is_empty();
    // The fill domain is the whole canvas (clipped per-pixel by the selection
    // via `coverage_at`). Iterating the buffer's occupied footprint instead
    // would skip blank layers entirely — a gradient on a fresh layer would
    // produce nothing. `buffer.get_pixel` returns TRANSPARENT for unoccupied
    // local pixels, so reading the existing pixel is always safe.
    for y in canvas.y as i64..canvas.bottom() {
        for x in canvas.x as i64..canvas.right() {
            let doc = IVec2::new(x as i32, y as i32);
            let local = doc - offset;
            let cov = if clip {
                selection.coverage_at(doc)
            } else {
                1.0
            };
            if cov <= 0.0 {
                continue;
            }
            let p_doc = Vec2::new(doc.x as f32 + 0.5, doc.y as f32 + 0.5);
            let raw_t = gradient_t(p_doc, p0_doc, p1_doc, kind);
            // Conical already wraps; the others honour the WrapMode.
            let t = if matches!(kind, GradientKind::Conical) {
                raw_t
            } else {
                wrap.wrap(raw_t)
            };
            let g = gradient_color(t, fg, bg, reverse);
            let existing = buffer.get_pixel(local);
            let blended = blend_pixel(BlendMode::Normal, existing, g, opacity * cov);
            out.push((local, blended));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_t_at_endpoints_and_beyond() {
        let p0 = Vec2::new(0.0, 0.0);
        let p1 = Vec2::new(10.0, 0.0);
        assert!((gradient_t(Vec2::new(0.0, 0.0), p0, p1, GradientKind::Linear) - 0.0).abs() < 1e-6);
        assert!((gradient_t(Vec2::new(5.0, 0.0), p0, p1, GradientKind::Linear) - 0.5).abs() < 1e-6);
        assert!(
            (gradient_t(Vec2::new(10.0, 0.0), p0, p1, GradientKind::Linear) - 1.0).abs() < 1e-6
        );
        // `gradient_t` now returns the raw (unclamped) parameter; wrapping is
        // applied by `fill_gradient` via `WrapMode`.
        assert!(
            (gradient_t(Vec2::new(15.0, 0.0), p0, p1, GradientKind::Linear) - 1.5).abs() < 1e-6
        );
        assert!(
            (gradient_t(Vec2::new(-5.0, 0.0), p0, p1, GradientKind::Linear) - -0.5).abs() < 1e-6
        );
    }

    #[test]
    fn wrap_modes_clamp_repeat_reflect() {
        assert_eq!(WrapMode::Clamp.wrap(1.5), 1.0);
        assert_eq!(WrapMode::Clamp.wrap(-0.5), 0.0);
        assert!((WrapMode::Repeat.wrap(1.25) - 0.25).abs() < 1e-6);
        assert!((WrapMode::Repeat.wrap(-0.25) - 0.75).abs() < 1e-6);
        assert!((WrapMode::Reflect.wrap(1.25) - 0.75).abs() < 1e-6);
        assert!((WrapMode::Reflect.wrap(-0.25) - 0.25).abs() < 1e-6);
        assert!((WrapMode::Reflect.wrap(2.5) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn conical_covers_full_circle() {
        // p1 along +x ⇒ zero angle is +x. The four cardinal points must map to
        // t = 0, 0.25, 0.5, 0.75 (no collapse to 0).
        let p0 = Vec2::new(0.0, 0.0);
        let p1 = Vec2::new(1.0, 0.0);
        let at = |x, y| gradient_t(Vec2::new(x, y), p0, p1, GradientKind::Conical);
        assert!((at(1.0, 0.0) - 0.0).abs() < 1e-5);
        assert!((at(0.0, 1.0) - 0.25).abs() < 1e-5);
        assert!((at(-1.0, 0.0) - 0.5).abs() < 1e-5);
        assert!((at(0.0, -1.0) - 0.75).abs() < 1e-5);
        // And t stays in [0,1).
        for (x, y) in [(1.0, 1.0), (-2.0, 3.0), (0.5, -0.5)] {
            let t = at(x, y);
            assert!((0.0..1.0).contains(&t), "t={t} out of [0,1) at ({x},{y})");
        }
    }

    #[test]
    fn degenerate_line_is_solid_foreground() {
        let p0 = Vec2::new(5.0, 5.0);
        let p1 = p0;
        for kind in [
            GradientKind::Linear,
            GradientKind::Radial,
            GradientKind::Diamond,
        ] {
            let t = gradient_t(Vec2::new(100.0, 100.0), p0, p1, kind);
            assert!((t - 0.0).abs() < 1e-6, "degenerate {kind:?} t={t}");
        }
    }

    #[test]
    fn gradient_color_linear_blend_and_reverse() {
        let fg = Rgba32F::new(0.0, 0.0, 0.0, 1.0);
        let bg = Rgba32F::new(1.0, 1.0, 1.0, 1.0);
        assert_eq!(gradient_color(0.0, fg, bg, false), fg);
        assert_eq!(gradient_color(1.0, fg, bg, false), bg);
        let mid = gradient_color(0.5, fg, bg, false);
        assert!((mid.r - 0.5).abs() < 1e-6);
        // Reverse flips the endpoint mapping.
        assert_eq!(gradient_color(0.0, fg, bg, true), bg);
        assert_eq!(gradient_color(1.0, fg, bg, true), fg);
    }

    #[test]
    fn fill_gradient_fills_blank_layer_across_canvas() {
        // Regression: a blank (empty) raster layer has no occupied tiles, so the
        // fill domain must be the canvas, not `exact_bounds()` — otherwise a
        // gradient on a fresh layer produces no pixels.
        use crate::buffer::TiledBuffer;
        use crate::coord::{IVec2, Rect};
        use crate::selection::Selection;
        use glam::Vec2;

        let blank = TiledBuffer::new(); // no tiles
        let canvas = Rect::new(0, 0, 8, 4);
        let writes = fill_gradient(
            &blank,
            IVec2::ZERO,
            Vec2::new(0.5, 2.0),
            Vec2::new(7.5, 2.0),
            GradientKind::Linear,
            Rgba32F::new(0.0, 0.0, 0.0, 1.0),
            Rgba32F::new(1.0, 1.0, 1.0, 1.0),
            false,
            1.0,
            &Selection::none(),
            canvas,
            WrapMode::Clamp,
        );
        // Every canvas pixel got a write (empty selection = unmasked).
        assert_eq!(writes.len(), 32);
        // Left edge ≈ fg, right edge ≈ bg.
        let left = writes
            .iter()
            .find(|(p, _)| *p == IVec2::new(0, 0))
            .unwrap()
            .1;
        assert!(left.r < 0.5);
    }
}
