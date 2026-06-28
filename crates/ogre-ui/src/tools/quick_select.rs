// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! The Quick Selection tool (§3.4.1).
//!
//! Brush a region; on release, one [`ogre_core::region_grow`] is run per sampled
//! point (bounded to the brush radius) against either the active layer or the
//! merged composite (Sample-All-Layers), and the per-sample results are unioned
//! (or subtracted on Alt-drag) into a single [`ogre_core::SetSelectionCmd`].
//! Live marching-ants preview during the drag is deferred — the selection is
//! computed and committed on pointer-up.

use ogre_core::{quick_select, SelectionMode, SetSelectionCmd};

use crate::tools::{Phase, PointerEvent, Tool};

/// Sample throttle: minimum distance (px) between two consecutive samples so a
/// stroke doesn't spawn one `region_grow` per pixel.
const SAMPLE_SPACING_PX: i32 = 4;

/// Quick Selection tool state.
#[derive(Debug)]
pub struct QuickSelectTool {
    /// Brush radius (px) — also the per-sample `region_grow` disk bound.
    radius: u32,
    /// Color-distance tolerance passed to `region_grow`.
    tolerance: f32,
    /// When true, sample the merged composite instead of the active layer.
    sample_all_layers: bool,
    /// Whether the tool currently has an in-progress stroke.
    stroking: bool,
    /// Alt held ⇒ subtract from the selection instead of adding.
    subtract: bool,
    /// Sampled document-space seed points (throttled by [`SAMPLE_SPACING_PX`]).
    samples: Vec<glam::IVec2>,
    last_sample: Option<glam::IVec2>,
}

impl Default for QuickSelectTool {
    fn default() -> Self {
        Self::new()
    }
}

impl QuickSelectTool {
    /// Create a default-radius (12 px), default-tolerance (0.2) tool.
    pub fn new() -> Self {
        Self {
            radius: 12,
            tolerance: 0.2,
            sample_all_layers: true,
            stroking: false,
            subtract: false,
            samples: Vec::new(),
            last_sample: None,
        }
    }

    /// Mutable `(radius, tolerance, sample_all_layers)` settings for the sidebar.
    pub fn settings_mut(&mut self) -> (&mut u32, &mut f32, &mut bool) {
        (
            &mut self.radius,
            &mut self.tolerance,
            &mut self.sample_all_layers,
        )
    }

    fn push_sample(&mut self, pos: glam::IVec2) {
        let accept = self
            .last_sample
            .map(|last| (pos - last).as_vec2().length() >= SAMPLE_SPACING_PX as f32)
            .unwrap_or(true);
        if accept {
            self.samples.push(pos);
            self.last_sample = Some(pos);
        }
    }
}

impl Tool for QuickSelectTool {
    fn on_pointer(
        &mut self,
        doc: &ogre_core::Document,
        ev: PointerEvent,
    ) -> Option<Box<dyn ogre_core::Command>> {
        match ev.phase {
            Phase::Down => {
                self.stroking = true;
                self.subtract = ev.modifiers.alt;
                self.samples.clear();
                self.last_sample = None;
                self.push_sample(ev.doc_pos);
                None
            }
            Phase::Drag => {
                if self.stroking {
                    self.push_sample(ev.doc_pos);
                }
                None
            }
            Phase::Up => {
                let cmd = if self.stroking && !self.samples.is_empty() {
                    let mode = if self.subtract {
                        SelectionMode::Subtract
                    } else {
                        SelectionMode::Add
                    };
                    let sel = if self.sample_all_layers {
                        // Grow against the merged composite so Quick Selection
                        // follows visible edges regardless of which layer is
                        // active.
                        let merged = ogre_core::composite_region(doc, doc.canvas).ok()?;
                        let base = doc.selection.clone();
                        let grown = quick_select(
                            &merged,
                            glam::IVec2::ZERO,
                            &self.samples,
                            self.tolerance,
                            self.radius,
                        );
                        base.combine(&grown, mode)
                    } else {
                        let layer = doc.active?;
                        let layer_ref = doc.layer(layer).ok()?;
                        let buffer = layer_ref.buffer()?;
                        let offset = layer_ref.offset;
                        let base = doc.selection.clone();
                        let grown = quick_select(
                            buffer,
                            offset,
                            &self.samples,
                            self.tolerance,
                            self.radius,
                        );
                        base.combine(&grown, mode)
                    };
                    Some(Box::new(SetSelectionCmd::new(sel)) as Box<dyn ogre_core::Command>)
                } else {
                    None
                };
                self.stroking = false;
                self.samples.clear();
                self.last_sample = None;
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
        // Draw a brush-radius ring at the cursor while stroking.
        if let Some(last) = self.last_sample {
            let centre_doc = last.as_vec2();
            let v = (centre_doc - viewport.pan) * viewport.zoom / pixels_per_point
                + glam::Vec2::new(canvas_rect.min.x, canvas_rect.min.y);
            let centre = egui::Pos2::new(v.x, v.y);
            let r = self.radius as f32 * viewport.zoom / pixels_per_point;
            painter.circle_stroke(centre, r, egui::Stroke::new(1.0, egui::Color32::WHITE));
        }
    }

    fn cancel(&mut self) {
        self.stroking = false;
        self.samples.clear();
        self.last_sample = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::IVec2;

    fn ev(pos: IVec2, phase: Phase, alt: bool) -> PointerEvent {
        let mut m = egui::Modifiers::NONE;
        m.alt = alt;
        PointerEvent::new(pos, phase, m)
    }

    #[test]
    fn quick_select_selects_uniform_region() {
        let mut doc = ogre_core::Document::new(32, 16);
        let layer = doc.add_raster_layer("L");
        doc.active = Some(layer);
        // Fill the left half red, right half blue.
        {
            let buf = doc.layer_mut(layer).unwrap().buffer_mut().unwrap();
            for y in 0..16 {
                for x in 0..32 {
                    let c = if x < 16 {
                        ogre_core::Rgba32F::new(1.0, 0.0, 0.0, 1.0)
                    } else {
                        ogre_core::Rgba32F::new(0.0, 0.0, 1.0, 1.0)
                    };
                    buf.set_pixel(IVec2::new(x, y), c);
                }
            }
        }

        let mut tool = QuickSelectTool::new();
        *tool.settings_mut().2 = false; // sample active layer only
        assert!(tool
            .on_pointer(&doc, ev(IVec2::new(8, 8), Phase::Down, false))
            .is_none());
        let mut cmd = tool
            .on_pointer(&doc, ev(IVec2::new(8, 8), Phase::Up, false))
            .unwrap();
        // Apply and check coverage: the red half (x<16) is selected, blue isn't.
        let mut d2 = doc.clone();
        cmd.apply(&mut d2).unwrap();
        assert!(d2.selection.coverage_at(IVec2::new(8, 8)) > 0.0);
        assert_eq!(d2.selection.coverage_at(IVec2::new(24, 8)), 0.0);
    }

    #[test]
    fn quick_select_alt_drag_subtracts() {
        // Start from a select-all, then Alt-brush a hole.
        let mut doc = ogre_core::Document::new(32, 16);
        let layer = doc.add_raster_layer("L");
        doc.active = Some(layer);
        {
            let buf = doc.layer_mut(layer).unwrap().buffer_mut().unwrap();
            for y in 0..16 {
                for x in 0..32 {
                    buf.set_pixel(
                        IVec2::new(x, y),
                        ogre_core::Rgba32F::new(1.0, 0.0, 0.0, 1.0),
                    );
                }
            }
        }
        doc.selection = ogre_core::Selection::select_all(doc.canvas);

        let mut tool = QuickSelectTool::new();
        *tool.settings_mut().2 = false;
        tool.on_pointer(&doc, ev(IVec2::new(16, 8), Phase::Down, true));
        let mut cmd = tool
            .on_pointer(&doc, ev(IVec2::new(16, 8), Phase::Up, true))
            .unwrap();
        let mut d2 = doc.clone();
        cmd.apply(&mut d2).unwrap();
        // The brushed point was subtracted from select-all.
        assert_eq!(d2.selection.coverage_at(IVec2::new(16, 8)), 0.0);
    }

    #[test]
    fn quick_select_settings_default() {
        let t = QuickSelectTool::new();
        assert_eq!(t.radius, 12);
        assert!((t.tolerance - 0.2).abs() < 1e-6);
        assert!(t.sample_all_layers);
    }
}
