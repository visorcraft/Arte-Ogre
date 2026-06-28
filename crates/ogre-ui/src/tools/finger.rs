// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Shared stroke engine for the brush "finger" tools (§3.2.4–§3.2.6).
//!
//! [`finger_stroke`] walks the pointer samples, places stamps along the path
//! ( BrushSettings spacing ), accumulates a per-pixel coverage map, and lerps
//! each touched pixel from its original value toward the `per_pixel` target by
//! that coverage. The per-tool `per_pixel` closure computes the target from the
//! tool's frozen backdrop / source snapshot (so reads never compound within a
//! stroke). Each tool wraps this with its own op + backdrop-snapshot policy.

use std::collections::HashMap;

use glam::{IVec2, Vec2};
use ogre_core::{BrushSettings, InputSample, Rgba32F, TiledBuffer};

use crate::tools::{Phase, PointerEvent, Tool};

/// Build the `(layer-local pixel, new value)` edits for a finger-tool stroke.
///
/// `samples` are in **document** coordinates; `layer_offset` maps doc↔local
/// (`local = doc - layer_offset`); `layer_buffer` supplies the original pixels
/// for the lerp. `per_pixel(local, doc)` returns the **target** value for a
/// touched pixel (or `None` to leave it untouched). The final pixel is
/// `original.lerp(target, coverage)`, where coverage is the max stamp weight
/// (hardness falloff × flow × pressure opacity) over the stroke.
pub fn finger_stroke<F>(
    samples: &[InputSample],
    settings: &BrushSettings,
    layer_offset: IVec2,
    layer_buffer: &TiledBuffer,
    selection: &ogre_core::Selection,
    mut per_pixel: F,
) -> Vec<(IVec2, ogre_core::Rgba32F)>
where
    F: FnMut(IVec2, IVec2) -> Option<ogre_core::Rgba32F>,
{
    let settings = settings.sanitised();
    let step = settings.step_distance();
    let mut coverage: HashMap<IVec2, f32> = HashMap::new();
    let clip_selection = !selection.is_empty();

    // Place stamp sites along the polyline at the brush spacing.
    let mut sites: Vec<(Vec2, f32)> = Vec::new();
    if samples.len() == 1 {
        sites.push((samples[0].pos, samples[0].pressure));
    }
    for i in 0..samples.len().saturating_sub(1) {
        let s0 = samples[i];
        let s1 = samples[i + 1];
        let seg = s1.pos - s0.pos;
        let len = seg.length();
        if len == 0.0 {
            continue;
        }
        let dir = seg / len;
        let mut d = 0.0;
        while d <= len {
            let t = d / len;
            sites.push((
                s0.pos + dir * d,
                s0.pressure + (s1.pressure - s0.pressure) * t,
            ));
            d += step;
        }
    }
    if let Some(last) = samples.last() {
        sites.push((last.pos, last.pressure));
    }

    for (center_doc, pressure) in sites {
        let radius = settings.width_at_pressure(pressure) / 2.0;
        if radius <= 0.0 {
            continue;
        }
        let stamp_alpha = settings.opacity_at_pressure(pressure) * settings.flow;
        if stamp_alpha <= 0.0 {
            continue;
        }
        let min_x = (center_doc.x - radius).floor() as i32;
        let min_y = (center_doc.y - radius).floor() as i32;
        let max_x = (center_doc.x + radius).ceil() as i32;
        let max_y = (center_doc.y + radius).ceil() as i32;
        for y in min_y..=max_y {
            for x in min_x..=max_x {
                let dx = x as f32 + 0.5 - center_doc.x;
                let dy = y as f32 + 0.5 - center_doc.y;
                let dist = (dx * dx + dy * dy).sqrt();
                if dist >= radius {
                    continue;
                }
                // Linear falloff scaled by hardness (a smoothed approximation of
                // the deposit brush's curve; the finger tools' exact edge is not
                // perceptually critical).
                let t = dist / radius;
                let falloff = (1.0 - t) * (1.0 - settings.hardness * 0.5 * t);
                let mut a = (falloff * stamp_alpha).clamp(0.0, 1.0);
                // Honor the active selection: outside it, the stamp writes nothing.
                let doc = IVec2::new(x, y);
                if clip_selection {
                    a *= selection.coverage_at(doc);
                }
                if a <= 0.0 {
                    continue;
                }
                let local = doc - layer_offset;
                let cov = coverage.entry(local).or_insert(0.0);
                if a > *cov {
                    *cov = a;
                }
            }
        }
    }

    let mut edits = Vec::new();
    for (local, cov) in coverage {
        if cov <= 0.0 {
            continue;
        }
        let doc = local + layer_offset;
        if let Some(target) = per_pixel(local, doc) {
            let original = layer_buffer.get_pixel(local);
            let final_px = original.lerp(target, cov);
            // Skip no-op writes (e.g. uniform regions, strength=0) so they don't
            // create a history entry for an unchanged pixel.
            let delta = (final_px.r - original.r).abs()
                + (final_px.g - original.g).abs()
                + (final_px.b - original.b).abs()
                + (final_px.a - original.a).abs();
            if delta > 1e-4 {
                edits.push((local, final_px));
            }
        }
    }
    edits
}

/// Box-averaged neighbourhood sample of `backdrop` around `local` with the given
/// radius. Used by Blur/Sharpen. Returns the centre pixel (or transparent) if
/// the backdrop is missing.
pub fn neighbourhood_average(
    backdrop: Option<&TiledBuffer>,
    local: IVec2,
    radius: i32,
) -> ogre_core::Rgba32F {
    use ogre_core::Rgba32F;
    let Some(buf) = backdrop else {
        return Rgba32F::TRANSPARENT;
    };
    if radius <= 0 {
        return buf.get_pixel(local);
    }
    let mut acc = Vec2::ZERO; // r,g
    let mut acc_b = 0.0;
    let mut acc_a = 0.0;
    let mut n = 0.0;
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            let p = local + IVec2::new(dx, dy);
            let px = buf.get_pixel(p);
            acc.x += px.r;
            acc.y += px.g;
            acc_b += px.b;
            acc_a += px.a;
            n += 1.0;
        }
    }
    if n > 0.0 {
        Rgba32F::new(acc.x / n, acc.y / n, acc_b / n, acc_a / n)
    } else {
        Rgba32F::TRANSPARENT
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ogre_core::Rgba32F;

    fn layer(off: IVec2, value: Rgba32F, w: i32, h: i32) -> TiledBuffer {
        let mut buf = TiledBuffer::new();
        for y in 0..h {
            for x in 0..w {
                buf.set_pixel(off + IVec2::new(x, y), value);
            }
        }
        buf
    }

    #[test]
    fn finger_stroke_writes_constant_target_under_stamp() {
        let buf = layer(IVec2::ZERO, Rgba32F::new(0.0, 0.0, 0.0, 1.0), 32, 8);
        let settings = BrushSettings {
            size: 10.0,
            hardness: 0.0,
            opacity: 1.0,
            flow: 1.0,
            spacing: 0.5,
            pressure_size: false,
            pressure_opacity: false,
        };
        let samples = vec![InputSample::new(Vec2::new(5.5, 4.5))];
        // Target = white everywhere ⇒ stamp paints white over black, feathered.
        let edits = finger_stroke(
            &samples,
            &settings,
            IVec2::ZERO,
            &buf,
            &ogre_core::Selection::none(),
            |_, _| Some(Rgba32F::new(1.0, 1.0, 1.0, 1.0)),
        );
        assert!(!edits.is_empty());
        // The stamp center is fully covered ⇒ becomes white.
        let center = edits
            .iter()
            .find(|(p, _)| *p == IVec2::new(5, 4))
            .unwrap()
            .1;
        assert!(
            (center.r - 1.0).abs() < 1e-4,
            "center not fully covered: {:?}",
            center
        );
        // Outside the radius: untouched (not in edits).
        assert!(edits
            .iter()
            .all(|(p, _)| (p.x - 5).pow(2) + (p.y - 4).pow(2) <= 36));
    }

    #[test]
    fn finger_stroke_none_target_leaves_pixel_untouched() {
        let buf = layer(IVec2::ZERO, Rgba32F::new(0.0, 0.0, 0.0, 1.0), 16, 16);
        let settings = BrushSettings {
            size: 6.0,
            hardness: 0.0,
            opacity: 1.0,
            flow: 1.0,
            spacing: 0.5,
            pressure_size: false,
            pressure_opacity: false,
        };
        let samples = vec![InputSample::new(Vec2::new(8.0, 8.0))];
        let edits = finger_stroke(
            &samples,
            &settings,
            IVec2::ZERO,
            &buf,
            &ogre_core::Selection::none(),
            |_, _| None,
        );
        assert!(edits.is_empty(), "None target must produce no edits");
    }

    #[test]
    fn neighbourhood_average_is_uniform_for_uniform_field() {
        let buf = layer(IVec2::ZERO, Rgba32F::new(0.2, 0.4, 0.6, 1.0), 12, 12);
        let avg = neighbourhood_average(Some(&buf), IVec2::new(6, 6), 2);
        assert!((avg.r - 0.2).abs() < 1e-4);
        assert!((avg.g - 0.4).abs() < 1e-4);
        assert!((avg.b - 0.6).abs() < 1e-4);
    }
}

/// Which finger operation a [`FingerTool`] performs. Each variant maps to one
/// `ToolKind` (Blur/Sharpen/Dodge/Burn/Sponge/ColorReplacement). Smudge and
/// Clone Stamp have their own tool structs (stateful / source-snapshot).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FingerOp {
    /// Gaussian/box blur of the frozen backdrop.
    Blur,
    /// Unsharp-mask sharpen of the frozen backdrop.
    Sharpen {
        /// Unsharp-mask amount (0 = no effect).
        strength: f32,
    },
    /// Reciprocal lightening, tonally masked.
    Dodge {
        /// Tonal range to affect.
        range: ogre_core::Range,
        /// Effect strength in `[0, 1)` (reciprocal scaling).
        exposure: f32,
    },
    /// Reciprocal darkening, tonally masked.
    Burn {
        /// Tonal range to affect.
        range: ogre_core::Range,
        /// Effect strength in `[0, 1]`.
        exposure: f32,
    },
    /// Saturation up/down in sRGB space.
    Sponge {
        /// Effect strength in `[0, 1]`.
        exposure: f32,
        /// `true` to saturate, `false` to desaturate.
        saturate: bool,
    },
    /// Replace hue/saturation with `color`, preserving luma.
    Recolor {
        /// The replacement color (hue/saturation source).
        color: Rgba32F,
    },
}

/// A round-brush finger tool that reads a frozen composited backdrop and writes
/// its op's target value, accumulated over the stroke by [`finger_stroke`].
/// Drives Blur/Sharpen/Dodge/Burn/Sponge/Color Replacement (§3.2.4–§3.2.6).
#[derive(Debug)]
pub struct FingerTool {
    settings: BrushSettings,
    op: FingerOp,
    stroking: bool,
    active_layer: Option<ogre_core::LayerId>,
    samples: Vec<InputSample>,
    frozen_backdrop: Option<TiledBuffer>,
    last_pos: IVec2,
}

impl FingerTool {
    /// Construct a finger tool with the given op + default round-brush settings.
    pub fn new(op: FingerOp) -> Self {
        Self {
            settings: BrushSettings {
                size: 20.0,
                hardness: 0.0,
                opacity: 1.0,
                flow: 1.0,
                spacing: 0.25,
                pressure_size: true,
                pressure_opacity: false,
            },
            op,
            stroking: false,
            active_layer: None,
            samples: Vec::new(),
            frozen_backdrop: None,
            last_pos: IVec2::ZERO,
        }
    }

    /// Mutable brush settings (sidebar).
    pub fn settings_mut(&mut self) -> &mut BrushSettings {
        &mut self.settings
    }

    /// Mutable op (sidebar adjusts strength/exposure/range/saturate/color).
    pub fn op_mut(&mut self) -> &mut FingerOp {
        &mut self.op
    }

    fn sample(ev: PointerEvent) -> InputSample {
        InputSample::with_pressure(
            Vec2::new(ev.doc_pos.x as f32 + 0.5, ev.doc_pos.y as f32 + 0.5),
            ev.pressure,
        )
    }
}

impl Tool for FingerTool {
    fn on_pointer(
        &mut self,
        doc: &ogre_core::Document,
        ev: PointerEvent,
    ) -> Option<Box<dyn ogre_core::Command>> {
        self.last_pos = ev.doc_pos;
        match ev.phase {
            Phase::Down => {
                self.stroking = false;
                self.samples.clear();
                self.frozen_backdrop = None;
                self.active_layer = None;
                if let Some(&layer) = doc.active.as_ref() {
                    if let Ok(l) = doc.layer(layer) {
                        if l.is_raster() {
                            self.active_layer = Some(layer);
                            // Freeze the composited backdrop once per stroke so
                            // Blur/Sharpen/etc. don't compound within the drag.
                            self.frozen_backdrop =
                                ogre_core::composite_region(doc, doc.canvas).ok();
                            self.stroking = true;
                            self.samples.push(Self::sample(ev));
                        }
                    }
                }
                None
            }
            Phase::Drag => {
                if self.stroking {
                    self.samples.push(Self::sample(ev));
                }
                None
            }
            Phase::Up => {
                let cmd = self.active_layer.and_then(|layer| {
                    let Ok(l) = doc.layer(layer) else {
                        return None;
                    };
                    let offset = l.offset;
                    let buf = l.buffer()?;
                    let radius = (self.settings.size / 2.0) as i32;
                    let backdrop = self.frozen_backdrop.as_ref();
                    let op = self.op;
                    let edits = finger_stroke(
                        &self.samples,
                        &self.settings,
                        offset,
                        buf,
                        &doc.selection,
                        |local, _| {
                            let bd = backdrop
                                .map(|b| b.get_pixel(local + offset))
                                .unwrap_or(Rgba32F::TRANSPARENT);
                            Some(match op {
                                FingerOp::Blur => ogre_core::blur_target(neighbourhood_average(
                                    backdrop,
                                    local + offset,
                                    radius,
                                )),
                                FingerOp::Sharpen { strength } => {
                                    let blurred =
                                        neighbourhood_average(backdrop, local + offset, radius);
                                    ogre_core::sharpen_target(bd, blurred, strength)
                                }
                                FingerOp::Dodge { range, exposure } => {
                                    let m = exposure
                                        * ogre_core::range_mask(ogre_core::luma(bd), range);
                                    ogre_core::dodge_target(bd, m)
                                }
                                FingerOp::Burn { range, exposure } => {
                                    let m = exposure
                                        * ogre_core::range_mask(ogre_core::luma(bd), range);
                                    ogre_core::burn_target(bd, m)
                                }
                                FingerOp::Sponge { exposure, saturate } => {
                                    ogre_core::sponge_target(bd, exposure, saturate)
                                }
                                FingerOp::Recolor { color } => ogre_core::recolor_target(bd, color),
                            })
                        },
                    );
                    if edits.is_empty() {
                        return None;
                    }
                    Some(Box::new(ogre_core::BrushStrokeCmd::new(layer, edits))
                        as Box<dyn ogre_core::Command>)
                });
                self.stroking = false;
                self.samples.clear();
                self.frozen_backdrop = None;
                self.active_layer = None;
                cmd
            }
        }
    }

    fn draw_overlay(
        &self,
        painter: &egui::Painter,
        _doc: &ogre_core::Document,
        viewport: &ogre_gpu::Viewport,
        canvas_rect: egui::Rect,
        pixels_per_point: f32,
        _time: f64,
    ) {
        if !self.stroking {
            return;
        }
        let v = (self.last_pos.as_vec2() - viewport.pan) * viewport.zoom / pixels_per_point
            + Vec2::new(canvas_rect.min.x, canvas_rect.min.y);
        let centre = egui::Pos2::new(v.x, v.y);
        let r = (self.settings.size / 2.0) * viewport.zoom / pixels_per_point;
        if r > 0.5 {
            painter.circle_stroke(centre, r, egui::Stroke::new(1.0, egui::Color32::WHITE));
        }
    }

    fn cancel(&mut self) {
        self.stroking = false;
        self.samples.clear();
        self.frozen_backdrop = None;
        self.active_layer = None;
    }
}
