// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! The Magnetic Lasso (§3.4.2). A freehand lasso whose path snaps to the
//! strongest edge within a search disk of each cursor sample. The snapped
//! polyline is simplified (Douglas-Peucker) before building the selection.

use glam::Vec2;
use ogre_core::{edge_magnitude, simplify_path, snap_to_edge, Selection, SetSelectionCmd};

use crate::tools::{Phase, PointerEvent, Tool};

/// Minimum vertices for a valid lasso polygon (mirrors the freehand lasso).
const MIN_LASSO_POINTS: usize = 3;

/// Magnetic Lasso tool state.
#[derive(Debug)]
pub struct MagneticLassoTool {
    width: u32,
    contrast_threshold: f32,
    simplify_epsilon: f32,
    stroking: bool,
    path: Vec<glam::IVec2>,
    /// Pre-computed edge field over the canvas (document-space), frozen per
    /// stroke so dragging doesn't recompute it.
    field: Option<Vec<(glam::IVec2, f32)>>,
    last_sample: Option<glam::IVec2>,
}

impl MagneticLassoTool {
    /// Default tool: 10px width, 0.15 contrast, 1.5px simplify epsilon.
    pub fn new() -> Self {
        Self {
            width: 10,
            contrast_threshold: 0.15,
            simplify_epsilon: 1.5,
            stroking: false,
            path: Vec::new(),
            field: None,
            last_sample: None,
        }
    }

    /// Mutable `(width, contrast_threshold, simplify_epsilon)` for the sidebar.
    pub fn controls_mut(&mut self) -> (&mut u32, &mut f32, &mut f32) {
        (
            &mut self.width,
            &mut self.contrast_threshold,
            &mut self.simplify_epsilon,
        )
    }

    fn snap(&self, center: glam::IVec2) -> glam::IVec2 {
        let Some(field) = self.field.as_ref() else {
            return center;
        };
        if field.is_empty() {
            return center;
        }
        snap_to_edge(field, center, self.width, self.contrast_threshold).0
    }
}

impl Default for MagneticLassoTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for MagneticLassoTool {
    fn on_pointer(
        &mut self,
        doc: &ogre_core::Document,
        ev: PointerEvent,
    ) -> Option<Box<dyn ogre_core::Command>> {
        match ev.phase {
            Phase::Down => {
                self.stroking = false;
                self.path.clear();
                self.field = None;
                self.last_sample = None;
                // Freeze the composited backdrop and pre-compute its edge field.
                if let Ok(backdrop) = ogre_core::composite_region(doc, doc.canvas) {
                    self.field = Some(edge_magnitude(&backdrop, doc.canvas));
                    self.stroking = true;
                    let p = self.snap(ev.doc_pos);
                    self.path.push(p);
                    self.last_sample = Some(p);
                }
                None
            }
            Phase::Drag => {
                if self.stroking {
                    // Throttle: only add a vertex when the cursor has moved at
                    // least 2px from the last snapped vertex.
                    let moved = self
                        .last_sample
                        .map(|l| (ev.doc_pos - l).as_vec2().length() >= 2.0)
                        .unwrap_or(true);
                    if moved {
                        let p = self.snap(ev.doc_pos);
                        self.path.push(p);
                        self.last_sample = Some(p);
                    }
                }
                None
            }
            Phase::Up => {
                let cmd = if self.stroking && self.path.len() >= MIN_LASSO_POINTS {
                    let simplified =
                        simplify_path(std::mem::take(&mut self.path), self.simplify_epsilon);
                    if simplified.len() >= MIN_LASSO_POINTS {
                        Some(
                            Box::new(SetSelectionCmd::new(Selection::polygon(&simplified)))
                                as Box<dyn ogre_core::Command>,
                        )
                    } else {
                        None
                    }
                } else {
                    None
                };
                self.stroking = false;
                self.path.clear();
                self.field = None;
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
        if self.path.len() < 2 {
            return;
        }
        let to_screen = |p: glam::IVec2| -> egui::Pos2 {
            let v = (p.as_vec2() - viewport.pan) * viewport.zoom / pixels_per_point
                + Vec2::new(canvas_rect.min.x, canvas_rect.min.y);
            egui::Pos2::new(v.x, v.y)
        };
        let stroke = egui::Stroke::new(1.5, egui::Color32::WHITE);
        for w in self.path.windows(2) {
            painter.line_segment([to_screen(w[0]), to_screen(w[1])], stroke);
        }
    }

    fn cancel(&mut self) {
        self.stroking = false;
        self.path.clear();
        self.field = None;
        self.last_sample = None;
    }
}
