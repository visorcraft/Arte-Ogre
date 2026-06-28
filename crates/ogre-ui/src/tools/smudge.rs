// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! The Smudge tool (§3.2.4). Stateful finger tool: a `last_color` advances
//! along the stroke (toward the frozen-backdrop pixel under each stamp) and is
//! painted with stamp coverage. Unlike the stateless finger tools, Smudge cannot
//! use `finger_stroke` (which is order-independent) — it walks the stamp path
//! itself so `last_color` advances in stroke order.

use std::collections::HashMap;

use glam::{IVec2, Vec2};
use ogre_core::{BrushSettings, InputSample, Rgba32F};

use crate::tools::{Phase, PointerEvent, Tool};

/// Smudge tool: drag to smear color along the stroke direction.
#[derive(Debug)]
pub struct SmudgeTool {
    settings: BrushSettings,
    strength: f32,
    finger_painting: bool,
    fg: Rgba32F,
    stroking: bool,
    active_layer: Option<ogre_core::LayerId>,
    samples: Vec<InputSample>,
    frozen_backdrop: Option<ogre_core::TiledBuffer>,
    last_color: Rgba32F,
    last_pos: IVec2,
}

impl SmudgeTool {
    /// Default smudge (strength 0.5, finger painting off).
    pub fn new() -> Self {
        Self {
            settings: BrushSettings {
                size: 20.0,
                hardness: 0.0,
                opacity: 1.0,
                flow: 1.0,
                spacing: 0.1,
                pressure_size: true,
                pressure_opacity: false,
            },
            strength: 0.5,
            finger_painting: false,
            fg: Rgba32F::new(0.0, 0.0, 0.0, 1.0),
            stroking: false,
            active_layer: None,
            samples: Vec::new(),
            frozen_backdrop: None,
            last_color: Rgba32F::TRANSPARENT,
            last_pos: IVec2::ZERO,
        }
    }

    /// Mutable `(settings, strength, finger_painting)` for the sidebar.
    pub fn controls_mut(&mut self) -> (&mut BrushSettings, &mut f32, &mut bool) {
        (
            &mut self.settings,
            &mut self.strength,
            &mut self.finger_painting,
        )
    }

    /// Push the foreground color (driven by AppState).
    pub fn set_fg(&mut self, fg: Rgba32F) {
        self.fg = fg;
    }

    fn sample(ev: PointerEvent) -> InputSample {
        InputSample::with_pressure(
            Vec2::new(ev.doc_pos.x as f32 + 0.5, ev.doc_pos.y as f32 + 0.5),
            ev.pressure,
        )
    }
}

impl Default for SmudgeTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for SmudgeTool {
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
                            self.frozen_backdrop =
                                ogre_core::composite_region(doc, doc.canvas).ok();
                            // last_color starts from the backdrop under the
                            // first stamp (or the foreground if finger painting).
                            self.last_color = if self.finger_painting {
                                self.fg
                            } else {
                                self.frozen_backdrop
                                    .as_ref()
                                    .map(|b| b.get_pixel(ev.doc_pos))
                                    .unwrap_or(Rgba32F::TRANSPARENT)
                            };
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
                    let Ok(l) = doc.layer(layer) else { return None };
                    let offset = l.offset;
                    let buf = l.buffer()?;
                    let backdrop = self.frozen_backdrop.as_ref();
                    let settings = self.settings.sanitised();
                    let step = settings.step_distance();
                    let strength = self.strength.clamp(0.0, 1.0);
                    // Advance last_color along the stamp path, painting it with
                    // accumulated coverage per pixel.
                    let mut target: HashMap<IVec2, Rgba32F> = HashMap::new();
                    let mut coverage: HashMap<IVec2, f32> = HashMap::new();
                    let mut last_color = self.last_color;
                    let emit = |center_doc: Vec2,
                                pressure: f32,
                                lc: &mut Rgba32F,
                                target: &mut HashMap<IVec2, Rgba32F>,
                                coverage: &mut HashMap<IVec2, f32>| {
                        let radius = settings.width_at_pressure(pressure) / 2.0;
                        if radius <= 0.0 {
                            return;
                        }
                        let stamp_alpha = settings.opacity_at_pressure(pressure) * settings.flow;
                        // Advance last_color toward the backdrop under this stamp.
                        if let Some(bd) = backdrop {
                            let local =
                                IVec2::new(center_doc.x as i32, center_doc.y as i32) - offset;
                            let bd_px = bd.get_pixel(local + offset);
                            let mix = strength; // per-stamp pick-up
                            *lc = lc.lerp(bd_px, mix);
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
                                let t = dist / radius;
                                let falloff = (1.0 - t) * (1.0 - settings.hardness * 0.5 * t);
                                let a = (falloff * stamp_alpha).clamp(0.0, 1.0);
                                let local = IVec2::new(x, y) - offset;
                                target.insert(local, *lc);
                                let cov = coverage.entry(local).or_insert(0.0);
                                if a > *cov {
                                    *cov = a;
                                }
                            }
                        }
                    };
                    if self.samples.len() == 1 {
                        emit(
                            self.samples[0].pos,
                            self.samples[0].pressure,
                            &mut last_color,
                            &mut target,
                            &mut coverage,
                        );
                    }
                    for i in 0..self.samples.len().saturating_sub(1) {
                        let s0 = self.samples[i];
                        let s1 = self.samples[i + 1];
                        let seg = s1.pos - s0.pos;
                        let len = seg.length();
                        if len == 0.0 {
                            continue;
                        }
                        let dir = seg / len;
                        let mut d = 0.0;
                        while d <= len {
                            let t = d / len;
                            let pos = s0.pos + dir * d;
                            let pr = s0.pressure + (s1.pressure - s0.pressure) * t;
                            emit(pos, pr, &mut last_color, &mut target, &mut coverage);
                            d += step;
                        }
                    }
                    // Persist the advanced last_color (harmless: the next Down
                    // resets it; this also marks the local as used).
                    self.last_color = last_color;
                    let mut edits = Vec::new();
                    for (local, cov) in coverage {
                        if cov <= 0.0 {
                            continue;
                        }
                        if let Some(&tgt) = target.get(&local) {
                            let original = buf.get_pixel(local);
                            edits.push((local, original.lerp(tgt, cov * strength)));
                        }
                    }
                    if edits.is_empty() {
                        return None;
                    }
                    Some(Box::new(ogre_core::BrushStrokeCmd::new(layer, edits))
                        as Box<dyn ogre_core::Command>)
                });
                self.reset_stroke();
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
        self.reset_stroke();
    }
}

impl SmudgeTool {
    fn reset_stroke(&mut self) {
        self.stroking = false;
        self.samples.clear();
        self.frozen_backdrop = None;
        self.active_layer = None;
    }
}
