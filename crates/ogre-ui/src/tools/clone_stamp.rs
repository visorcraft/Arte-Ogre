// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! The Clone Stamp tool (§3.2.2). Alt+click sets a source anchor; a subsequent
//! drag paints from a frozen source snapshot (the merged composite or the active
//! layer captured at stroke start) at the captured offset. Reuses
//! [`finger_stroke`] with a `per_pixel` that reads the source buffer.

use glam::{IVec2, Vec2};
use ogre_core::{BrushSettings, InputSample, TiledBuffer};

use crate::tools::finger::finger_stroke;
use crate::tools::{Phase, PointerEvent, Tool};

/// Clone Stamp tool: paint pixels sampled from a source anchor.
#[derive(Debug)]
pub struct CloneStampTool {
    settings: BrushSettings,
    aligned: bool,
    sample_all_layers: bool,
    /// Alt-set source anchor (document space).
    source_anchor: Option<IVec2>,
    /// Offset captured at stroke start: `target_doc = source_doc + offset`.
    stroke_offset: Option<IVec2>,
    stroking: bool,
    active_layer: Option<ogre_core::LayerId>,
    samples: Vec<InputSample>,
    /// Frozen source snapshot (document space), captured at stroke start.
    source: Option<TiledBuffer>,
    last_pos: IVec2,
}

impl CloneStampTool {
    /// Default clone stamp (aligned, sample all layers).
    pub fn new() -> Self {
        Self {
            settings: BrushSettings {
                size: 20.0,
                hardness: 0.5,
                opacity: 1.0,
                flow: 1.0,
                spacing: 0.25,
                pressure_size: true,
                pressure_opacity: false,
            },
            aligned: true,
            sample_all_layers: true,
            source_anchor: None,
            stroke_offset: None,
            stroking: false,
            active_layer: None,
            samples: Vec::new(),
            source: None,
            last_pos: IVec2::ZERO,
        }
    }

    /// Mutable `(settings, aligned, sample_all_layers)` for the sidebar.
    pub fn controls_mut(&mut self) -> (&mut BrushSettings, &mut bool, &mut bool) {
        (
            &mut self.settings,
            &mut self.aligned,
            &mut self.sample_all_layers,
        )
    }

    fn sample(ev: PointerEvent) -> InputSample {
        InputSample::with_pressure(
            Vec2::new(ev.doc_pos.x as f32 + 0.5, ev.doc_pos.y as f32 + 0.5),
            ev.pressure,
        )
    }
}

impl Default for CloneStampTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for CloneStampTool {
    fn on_pointer(
        &mut self,
        doc: &ogre_core::Document,
        ev: PointerEvent,
    ) -> Option<Box<dyn ogre_core::Command>> {
        self.last_pos = ev.doc_pos;
        // Alt+Down sets the source anchor and emits no command.
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
                let anchor = self.source_anchor?;
                if let Some(&layer) = doc.active.as_ref() {
                    if let Ok(l) = doc.layer(layer) {
                        if l.is_raster() {
                            self.active_layer = Some(layer);
                            // Aligned: keep the offset captured on the first
                            // post-Alt stroke (source tracks the cursor across
                            // strokes). Unaligned: reset so each stroke starts
                            // from the source anchor.
                            if !self.aligned || self.stroke_offset.is_none() {
                                self.stroke_offset = Some(anchor - ev.doc_pos);
                            }
                            self.source = if self.sample_all_layers {
                                ogre_core::composite_region(doc, doc.canvas).ok()
                            } else {
                                // Clone the active layer's buffer into a
                                // document-space snapshot.
                                let mut snap = TiledBuffer::new();
                                if let Some(b) = l.buffer() {
                                    if let Some(bounds) = b.exact_bounds() {
                                        for y in bounds.y..bounds.bottom() as i32 {
                                            for x in bounds.x..bounds.right() as i32 {
                                                let p = IVec2::new(x, y);
                                                snap.set_pixel(p + l.offset, b.get_pixel(p));
                                            }
                                        }
                                    }
                                }
                                Some(snap)
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
                    let src_offset = self.stroke_offset?;
                    let src = self.source.as_ref()?;
                    let edits = finger_stroke(
                        &self.samples,
                        &self.settings,
                        offset,
                        buf,
                        &doc.selection,
                        |_local, doc| {
                            // Source pixel: same relative offset from the anchor.
                            let src_doc = doc + src_offset;
                            Some(src.get_pixel(src_doc))
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
                self.source = None;
                self.active_layer = None;
                // Non-aligned resets the offset so the next stroke re-anchors.
                if !self.aligned {
                    self.stroke_offset = None;
                }
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
        self.source = None;
        self.active_layer = None;
        self.stroke_offset = None;
    }
}
