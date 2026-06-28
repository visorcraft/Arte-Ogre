// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Healing (§3.2.3) and Spot Healing tools.
//!
//! `HealingTool` is a mean-matched clone: like Clone Stamp it samples a frozen
//! source at an Alt-set anchor, but shifts the source by `(mu_dst - mu_src)` per
//! channel (means over the stroke footprint) so the patch adopts the
//! destination's local lighting. `SpotHealingTool` replaces each pixel with a
//! box-average of the frozen backdrop around it (a proximity-match approximation
//! of the spec's ring-weighted average) — no source anchor needed.

use glam::{IVec2, Vec2};
use ogre_core::{heal_target, BrushSettings, InputSample, Rgba32F, TiledBuffer};

use crate::tools::finger::{finger_stroke, neighbourhood_average};
use crate::tools::{Phase, PointerEvent, Tool};

/// Shared brush settings + stroke state for the two healing tools.
fn default_settings() -> BrushSettings {
    BrushSettings {
        size: 20.0,
        hardness: 0.5,
        opacity: 1.0,
        flow: 1.0,
        spacing: 0.25,
        pressure_size: true,
        pressure_opacity: false,
    }
}

fn sample(ev: PointerEvent) -> InputSample {
    InputSample::with_pressure(
        Vec2::new(ev.doc_pos.x as f32 + 0.5, ev.doc_pos.y as f32 + 0.5),
        ev.pressure,
    )
}

/// Healing brush: Alt-set a source anchor, then paint a mean-matched clone.
#[derive(Debug)]
pub struct HealingTool {
    settings: BrushSettings,
    source_anchor: Option<IVec2>,
    stroke_offset: Option<IVec2>,
    stroking: bool,
    active_layer: Option<ogre_core::LayerId>,
    samples: Vec<InputSample>,
    source: Option<TiledBuffer>,
    last_pos: IVec2,
}

impl HealingTool {
    /// Default healing brush.
    pub fn new() -> Self {
        Self {
            settings: default_settings(),
            source_anchor: None,
            stroke_offset: None,
            stroking: false,
            active_layer: None,
            samples: Vec::new(),
            source: None,
            last_pos: IVec2::ZERO,
        }
    }
    /// Mutable brush settings (sidebar).
    pub fn settings_mut(&mut self) -> &mut BrushSettings {
        &mut self.settings
    }
}

impl Default for HealingTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for HealingTool {
    fn on_pointer(
        &mut self,
        doc: &ogre_core::Document,
        ev: PointerEvent,
    ) -> Option<Box<dyn ogre_core::Command>> {
        self.last_pos = ev.doc_pos;
        if ev.phase == Phase::Down && ev.modifiers.alt {
            self.source_anchor = Some(ev.doc_pos);
            return None;
        }
        match ev.phase {
            Phase::Down => {
                self.stroking = false;
                self.samples.clear();
                self.source = None;
                self.active_layer = None;
                self.stroke_offset = None;
                let anchor = self.source_anchor?;
                if let Some(&layer) = doc.active.as_ref() {
                    if let Ok(l) = doc.layer(layer) {
                        if l.is_raster() {
                            self.active_layer = Some(layer);
                            self.stroke_offset = Some(anchor - ev.doc_pos);
                            self.source = ogre_core::composite_region(doc, doc.canvas).ok();
                            self.stroking = true;
                            self.samples.push(sample(ev));
                        }
                    }
                }
                None
            }
            Phase::Drag => {
                if self.stroking {
                    self.samples.push(sample(ev));
                }
                None
            }
            Phase::Up => {
                let cmd = self.active_layer.and_then(|layer| {
                    let Ok(l) = doc.layer(layer) else { return None };
                    let offset = l.offset;
                    let buf = l.buffer()?;
                    let src_offset = self.stroke_offset?;
                    let src = self.source.as_ref()?;
                    // Per-channel means over the stroke's pixel footprint, used
                    // to mean-match the source patch to the destination.
                    let (mu_src, mu_dst) =
                        stroke_means(src, src_offset, buf, offset, &self.samples);
                    let radius = (self.settings.size / 2.0) as i32;
                    let edits = finger_stroke(
                        &self.samples,
                        &self.settings,
                        offset,
                        buf,
                        &doc.selection,
                        |local, _| {
                            let src_doc = local + offset + src_offset;
                            let src_px = src.get_pixel(src_doc);
                            let dst_px = buf.get_pixel(local);
                            Some(heal_target(src_px, mu_src, mu_dst, dst_px))
                        },
                    );
                    let _ = radius;
                    if edits.is_empty() {
                        return None;
                    }
                    Some(Box::new(ogre_core::BrushStrokeCmd::new(layer, edits))
                        as Box<dyn ogre_core::Command>)
                });
                self.stroking = false;
                self.samples.clear();
                self.source = None;
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
        let r = (self.settings.size / 2.0) * viewport.zoom / pixels_per_point;
        if r > 0.5 {
            painter.circle_stroke(
                egui::Pos2::new(v.x, v.y),
                r,
                egui::Stroke::new(1.0, egui::Color32::WHITE),
            );
        }
    }

    fn cancel(&mut self) {
        self.stroking = false;
        self.samples.clear();
        self.source = None;
        self.active_layer = None;
        self.stroke_offset = None;
    }
}

/// Spot Healing: replace each pixel with a box-average of the frozen backdrop
/// (a proximity-match approximation — no source anchor).
#[derive(Debug)]
pub struct SpotHealingTool {
    settings: BrushSettings,
    stroking: bool,
    active_layer: Option<ogre_core::LayerId>,
    samples: Vec<InputSample>,
    backdrop: Option<TiledBuffer>,
    last_pos: IVec2,
}

impl SpotHealingTool {
    /// Default spot-healing brush.
    pub fn new() -> Self {
        Self {
            settings: default_settings(),
            stroking: false,
            active_layer: None,
            samples: Vec::new(),
            backdrop: None,
            last_pos: IVec2::ZERO,
        }
    }
    /// Mutable brush settings (sidebar).
    pub fn settings_mut(&mut self) -> &mut BrushSettings {
        &mut self.settings
    }
}

impl Default for SpotHealingTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for SpotHealingTool {
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
                self.backdrop = None;
                self.active_layer = None;
                if let Some(&layer) = doc.active.as_ref() {
                    if let Ok(l) = doc.layer(layer) {
                        if l.is_raster() {
                            self.active_layer = Some(layer);
                            self.backdrop = ogre_core::composite_region(doc, doc.canvas).ok();
                            self.stroking = true;
                            self.samples.push(sample(ev));
                        }
                    }
                }
                None
            }
            Phase::Drag => {
                if self.stroking {
                    self.samples.push(sample(ev));
                }
                None
            }
            Phase::Up => {
                let cmd = self.active_layer.and_then(|layer| {
                    let Ok(l) = doc.layer(layer) else { return None };
                    let offset = l.offset;
                    let buf = l.buffer()?;
                    let bd = self.backdrop.as_ref();
                    let radius = (self.settings.size / 2.0) as i32;
                    let edits = finger_stroke(
                        &self.samples,
                        &self.settings,
                        offset,
                        buf,
                        &doc.selection,
                        |local, _| Some(neighbourhood_average(bd, local + offset, radius)),
                    );
                    if edits.is_empty() {
                        return None;
                    }
                    Some(Box::new(ogre_core::BrushStrokeCmd::new(layer, edits))
                        as Box<dyn ogre_core::Command>)
                });
                self.stroking = false;
                self.samples.clear();
                self.backdrop = None;
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
        let r = (self.settings.size / 2.0) * viewport.zoom / pixels_per_point;
        if r > 0.5 {
            painter.circle_stroke(
                egui::Pos2::new(v.x, v.y),
                r,
                egui::Stroke::new(1.0, egui::Color32::WHITE),
            );
        }
    }

    fn cancel(&mut self) {
        self.stroking = false;
        self.samples.clear();
        self.backdrop = None;
        self.active_layer = None;
    }
}

/// Per-channel means of the source patch and destination patch over the stroke's
/// pixel footprint (used by [`HealingTool`] to mean-match). Approximate: samples
/// the buffers at each stamp's offset center.
fn stroke_means(
    src: &TiledBuffer,
    src_offset: IVec2,
    dst: &TiledBuffer,
    dst_offset: IVec2,
    samples: &[InputSample],
) -> (Rgba32F, Rgba32F) {
    let mut s_acc = Vec2::ZERO;
    let mut s_b = 0.0;
    let mut s_a = 0.0;
    let mut d_acc = Vec2::ZERO;
    let mut d_b = 0.0;
    let mut d_a = 0.0;
    let mut n = 0.0;
    for s in samples {
        let dst_doc = IVec2::new(s.pos.x.floor() as i32, s.pos.y.floor() as i32);
        let src_doc = dst_doc + src_offset;
        let dp = dst.get_pixel(dst_doc - dst_offset);
        let sp = src.get_pixel(src_doc);
        s_acc.x += sp.r;
        s_acc.y += sp.g;
        s_b += sp.b;
        s_a += sp.a;
        d_acc.x += dp.r;
        d_acc.y += dp.g;
        d_b += dp.b;
        d_a += dp.a;
        n += 1.0;
    }
    if n <= 0.0 {
        return (Rgba32F::TRANSPARENT, Rgba32F::TRANSPARENT);
    }
    (
        Rgba32F::new(s_acc.x / n, s_acc.y / n, s_b / n, s_a / n),
        Rgba32F::new(d_acc.x / n, d_acc.y / n, d_b / n, d_a / n),
    )
}
