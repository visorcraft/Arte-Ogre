// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Pen tool (§3.3.3). Click to place corner anchors; click-drag to place
//! smooth anchors with symmetric bezier handles. Enter or clicking the first
//! anchor closes and commits the path as either a vector layer (default) or
//! rasterized pixels. Path→Selection converts a closed path to a selection
//! coverage mask.

use glam::Vec2;
use ogre_core::{
    AddVectorLayerCmd, BrushStrokeCmd, Rgba32F, Selection, SetSelectionCmd, StrokeCap, StrokeJoin,
    VectorData, VectorFill, VectorPath, VectorStroke,
};
use ogre_vector::{bezpath_to_polygon, rasterize_bezpath, Fill, Stroke};

use crate::tools::vector_commit::{buffer_to_edits, localize_path};
use crate::tools::{Phase, PointerEvent, Tool, VectorCommitMode};

/// Click within this many document pixels of the first anchor closes the path.
const CLOSE_THRESHOLD_PX: i32 = 6;

/// Minimum drag distance (document px) before a click becomes a drag.
const DRAG_THRESHOLD_PX: i32 = 3;

/// Pen tool: build a bezier path, commit as vector or raster pixels.
#[derive(Debug)]
pub struct PenTool {
    /// The bezier path under construction.
    path: kurbo::BezPath,
    /// Placed anchor points (for close detection and count).
    anchor_count: usize,
    /// The drag start position for the current anchor being placed.
    drag_start: Option<glam::IVec2>,
    /// Current cursor position (document space) for the rubber-band preview.
    cursor: Option<glam::IVec2>,
    /// Outgoing control handle offset from the last-placed anchor (for smooth
    /// anchors created via drag). None = corner anchor.
    last_handle: Option<kurbo::Point>,
    /// Active raster layer for destructive Pixels-mode commit.
    active_layer: Option<ogre_core::LayerId>,
    /// Fill color (used when the path is closed).
    fill: Rgba32F,
    /// Stroke color.
    stroke_color: Rgba32F,
    /// Stroke width in pixels.
    stroke_width: u32,
    /// Whether the committed path should be filled (closed paths only).
    fill_closed: bool,
    /// Whether to commit as a vector layer or rasterize into the active layer.
    mode: VectorCommitMode,
    /// Mode snapshot taken at the start of a gesture so a mid-gesture sidebar
    /// change cannot switch the target of the in-progress path.
    committed_mode: Option<VectorCommitMode>,
    /// When re-editing an existing vector layer, the id of that layer; commit
    /// dispatches [`EditVectorCmd`] instead of [`AddVectorLayerCmd`].
    editing_layer: Option<ogre_core::LayerId>,
}

impl Default for PenTool {
    fn default() -> Self {
        Self::new()
    }
}

impl PenTool {
    /// Create a new pen tool with no active path.
    pub fn new() -> Self {
        Self {
            path: kurbo::BezPath::new(),
            anchor_count: 0,
            drag_start: None,
            cursor: None,
            last_handle: None,
            active_layer: None,
            fill: Rgba32F::new(0.0, 0.0, 0.0, 1.0),
            stroke_color: Rgba32F::new(1.0, 1.0, 1.0, 1.0),
            stroke_width: 2,
            fill_closed: true,
            mode: VectorCommitMode::Vector,
            committed_mode: None,
            editing_layer: None,
        }
    }

    /// Mutable access to the commit mode for the sidebar.
    pub fn mode_mut(&mut self) -> &mut VectorCommitMode {
        &mut self.mode
    }

    /// The mode that should govern the current in-progress gesture.
    fn commit_mode(&self) -> VectorCommitMode {
        self.committed_mode.unwrap_or(self.mode)
    }

    /// Push shared fg/bg colors.
    pub fn set_colors(&mut self, fill: Rgba32F, stroke: Rgba32F) {
        self.fill = fill;
        self.stroke_color = stroke;
    }

    /// Mutable access to `(fill, stroke_color, stroke_width, fill_closed, mode)`.
    pub fn controls_mut(
        &mut self,
    ) -> (
        &mut Rgba32F,
        &mut Rgba32F,
        &mut u32,
        &mut bool,
        &mut VectorCommitMode,
    ) {
        (
            &mut self.fill,
            &mut self.stroke_color,
            &mut self.stroke_width,
            &mut self.fill_closed,
            &mut self.mode,
        )
    }

    fn is_empty(&self) -> bool {
        self.anchor_count == 0
    }

    fn first_anchor(&self) -> Option<kurbo::Point> {
        self.path.elements().first().and_then(|el| match *el {
            kurbo::PathEl::MoveTo(p) => Some(p),
            _ => None,
        })
    }

    fn is_closed(&self) -> bool {
        self.path
            .elements()
            .iter()
            .any(|el| matches!(*el, kurbo::PathEl::ClosePath))
    }

    /// Build a `VectorPath` from the current bezier path.
    fn vector_path(&self) -> VectorPath {
        let vertices = bezpath_to_polygon(&self.path, 0.5);
        VectorPath {
            vertices,
            fill: if self.fill_closed && self.is_closed() {
                VectorFill::Solid(self.fill)
            } else {
                VectorFill::None
            },
            stroke: VectorStroke {
                color: self.stroke_color,
                width: self.stroke_width as f32,
                dash: Vec::new(),
                cap: StrokeCap::Butt,
                join: StrokeJoin::Miter,
            },
            closed: self.is_closed(),
        }
    }

    /// Commit the current path as a new vector layer.
    fn commit_vector(&self) -> Option<Box<dyn ogre_core::Command>> {
        if self.anchor_count < 2 {
            return None;
        }
        let mut vpath = self.vector_path();
        if vpath.vertices.len() < 2 {
            return None;
        }
        let offset = localize_path(&mut vpath);
        let data = VectorData {
            paths: vec![vpath],
            ..Default::default()
        };
        Some(
            Box::new(AddVectorLayerCmd::new("Path", data).with_offset(offset))
                as Box<dyn ogre_core::Command>,
        )
    }

    /// Replace the path on an existing vector layer (re-edit path). Vertices
    /// are re-localized against the layer's existing offset so the layer stays
    /// put while the edited geometry updates in place.
    fn commit_vector_edit(
        &self,
        doc: &ogre_core::Document,
        id: ogre_core::LayerId,
    ) -> Option<Box<dyn ogre_core::Command>> {
        if self.anchor_count < 2 {
            return None;
        }
        let layer = doc.layer(id).ok()?;
        let layer_offset = layer.offset;
        let mut vpath = self.vector_path();
        if vpath.vertices.len() < 2 {
            return None;
        }
        // The pen path is in document space; shift into the layer's local space
        // using its existing offset (which stays fixed across the edit).
        for v in &mut vpath.vertices {
            *v -= layer_offset;
        }
        // Preserve any pre-rasterized content, text source, or SVG source on the
        // layer and bump the version so caches re-rasterize.
        let old_data = layer.vector_data()?.clone();
        let mut data = old_data.clone();
        data.paths = vec![vpath];
        data.mark_dirty();
        if old_data.svg_source.is_some() {
            Some(Box::new(crate::tools::vector_commit::EditSvgVectorCmd::new(
                id, old_data, data,
            )) as Box<dyn ogre_core::Command>)
        } else {
            Some(Box::new(ogre_core::EditVectorCmd::new(id, data)) as Box<dyn ogre_core::Command>)
        }
    }

    /// Rasterize the current path into layer edits (destructive Pixels mode).
    fn rasterize(
        &self,
        layer: ogre_core::LayerId,
        offset: glam::IVec2,
    ) -> Option<Box<dyn ogre_core::Command>> {
        let fill = if self.fill_closed && self.is_closed() && self.fill.a > 0.0 {
            Fill::Solid(self.fill)
        } else {
            Fill::None
        };
        let stroke = if self.stroke_width > 0 && self.stroke_color.a > 0.0 {
            Stroke {
                color: self.stroke_color,
                width: self.stroke_width as f32,
            }
        } else {
            Stroke::NONE
        };
        let buf = rasterize_bezpath(&self.path, fill, stroke);
        let edits = buffer_to_edits(&buf, glam::IVec2::ZERO, offset);
        if edits.is_empty() {
            return None;
        }
        Some(Box::new(BrushStrokeCmd::new(layer, edits)) as Box<dyn ogre_core::Command>)
    }

    /// Convert the current closed path to a selection (Path→Selection).
    /// Returns None if the path is open or has fewer than 3 anchors.
    pub fn make_selection(&self) -> Option<Box<dyn ogre_core::Command>> {
        if self.anchor_count < 3 || !self.is_closed() {
            return None;
        }
        let poly = bezpath_to_polygon(&self.path, 0.5);
        if poly.len() < 3 {
            return None;
        }
        let sel = Selection::polygon(&poly);
        if sel.is_empty() {
            return None;
        }
        Some(Box::new(SetSelectionCmd::new(sel)) as Box<dyn ogre_core::Command>)
    }

    fn clear(&mut self) {
        self.path = kurbo::BezPath::new();
        self.anchor_count = 0;
        self.drag_start = None;
        self.cursor = None;
        self.last_handle = None;
        self.active_layer = None;
        self.committed_mode = None;
        self.editing_layer = None;
    }

    /// If the active layer is a vector layer with at least one path, load its
    /// geometry back into the pen for continued editing. Original bezier
    /// handles are lost (paths are stored flattened); the loaded polygon can be
    /// extended and re-committed in place via [`ogre_core::EditVectorCmd`].
    ///
    /// Returns `true` if a path was loaded.
    ///
    /// Selecting a layer mid-**freehand** draw (anchors placed, not from a
    /// re-edit) refuses, so in-progress work is not clobbered. Selecting a layer
    /// while **re-editing** another drops the prior loaded geometry/target and
    /// loads the new one, so a re-edit target can never go stale.
    pub fn load_active_vector_layer(&mut self, doc: &ogre_core::Document) -> bool {
        // A freehand gesture (no re-edit target) in progress: preserve it.
        if self.editing_layer.is_none() && !self.is_empty() {
            return false;
        }
        // Either idle, or switching away from a re-edit: reset gesture state
        // before attempting the load.
        self.path = kurbo::BezPath::new();
        self.anchor_count = 0;
        self.last_handle = None;
        self.cursor = None;
        self.drag_start = None;
        self.editing_layer = None;
        self.committed_mode = None;
        let Some(id) = doc.active else {
            return false;
        };
        let Ok(layer) = doc.layer(id) else {
            return false;
        };
        let ogre_core::LayerContent::Vector(data) = &layer.content else {
            return false;
        };
        if data.paths.is_empty() {
            return false;
        }
        let offset = layer.offset;
        let mut path = kurbo::BezPath::new();
        let mut count = 0usize;
        for vpath in &data.paths {
            if vpath.vertices.is_empty() {
                continue;
            }
            let first = vpath.vertices[0] + offset;
            path.move_to((first.x as f64, first.y as f64));
            count += 1;
            for &p in vpath.vertices.iter().skip(1) {
                let dp = p + offset;
                path.line_to((dp.x as f64, dp.y as f64));
                count += 1;
            }
            if vpath.closed {
                path.close_path();
            }
        }
        if count < 2 {
            return false;
        }
        self.path = path;
        self.anchor_count = count;
        self.last_handle = None;
        // Inherit style from the first path.
        if let Some(first) = data.paths.first() {
            self.fill = match first.fill {
                VectorFill::Solid(color) => color,
                VectorFill::None => Rgba32F::TRANSPARENT,
            };
            self.stroke_color = first.stroke.color;
            self.stroke_width = first.stroke.width.round() as u32;
            self.fill_closed = first.closed && matches!(first.fill, VectorFill::Solid(_));
        }
        self.editing_layer = Some(id);
        self.committed_mode = Some(VectorCommitMode::Vector);
        true
    }
}

impl Tool for PenTool {
    fn on_pointer(
        &mut self,
        doc: &ogre_core::Document,
        ev: PointerEvent,
    ) -> Option<Box<dyn ogre_core::Command>> {
        match ev.phase {
            Phase::Down => {
                if self.committed_mode.is_none() {
                    self.committed_mode = Some(self.mode);
                }
                // Resolve the active raster layer on the first click in Pixels mode.
                if self.commit_mode() == VectorCommitMode::Pixels && self.active_layer.is_none() {
                    if let Some(&layer) = doc.active.as_ref() {
                        if let Ok(l) = doc.layer(layer) {
                            if l.is_raster() && !l.locked {
                                self.active_layer = Some(layer);
                            }
                        }
                    }
                    self.active_layer?;
                }
                self.cursor = Some(ev.doc_pos);
                // Check for close (click near first anchor with ≥3 anchors).
                if self.anchor_count >= 3 {
                    if let Some(first) = self.first_anchor() {
                        let dx = ev.doc_pos.x as f64 - first.x;
                        let dy = ev.doc_pos.y as f64 - first.y;
                        if (dx * dx + dy * dy).sqrt() <= CLOSE_THRESHOLD_PX as f64 {
                            self.path.close_path();
                            return self.commit(doc);
                        }
                    }
                }
                self.drag_start = Some(ev.doc_pos);
                None
            }
            Phase::Drag => {
                self.cursor = Some(ev.doc_pos);
                None
            }
            Phase::Up => {
                let start = self.drag_start.take()?;
                self.cursor = Some(ev.doc_pos);
                let drag_dist = (ev.doc_pos - start).length_squared();
                let pt = kurbo::Point::new(start.x as f64, start.y as f64);
                if drag_dist <= DRAG_THRESHOLD_PX * DRAG_THRESHOLD_PX {
                    // Click: corner anchor.
                    if self.is_empty() {
                        self.path.move_to(pt);
                    } else {
                        self.path.line_to(pt);
                    }
                    self.anchor_count += 1;
                    self.last_handle = None;
                } else {
                    // Click-drag: smooth anchor with symmetric handles.
                    let drag = kurbo::Vec2::new(
                        (ev.doc_pos.x - start.x) as f64,
                        (ev.doc_pos.y - start.y) as f64,
                    );
                    let cp_out = pt + drag;
                    let cp_in = pt - drag;
                    if self.is_empty() {
                        self.path.move_to(pt);
                        self.anchor_count += 1;
                    } else {
                        // Use the incoming handle from the previous anchor's
                        // outgoing handle, and the outgoing handle from this
                        // drag.
                        let prev = self.last_handle.unwrap_or(pt).midpoint(pt);
                        self.path.curve_to(prev, cp_in, pt);
                        self.anchor_count += 1;
                    }
                    self.last_handle = Some(cp_out);
                }
                None
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
        if self.is_empty() {
            return;
        }
        let to_screen = |p: kurbo::Point| -> egui::Pos2 {
            let v = (Vec2::new(p.x as f32, p.y as f32) - viewport.pan) * viewport.zoom
                / pixels_per_point
                + Vec2::new(canvas_rect.min.x, canvas_rect.min.y);
            egui::Pos2::new(v.x, v.y)
        };
        let stroke = egui::Stroke::new(1.5, egui::Color32::WHITE);
        // Draw the flattened path as a polyline preview.
        let poly = bezpath_to_polygon(&self.path, 0.5);
        let pts: Vec<egui::Pos2> = poly
            .iter()
            .map(|&p| to_screen(kurbo::Point::new(p.x as f64, p.y as f64)))
            .collect();
        if pts.len() >= 2 {
            painter.add(egui::Shape::line(pts.clone(), stroke));
        }
        // Rubber-band from last point to cursor.
        if let Some(cursor) = self.cursor {
            let c = to_screen(kurbo::Point::new(cursor.x as f64, cursor.y as f64));
            if let Some(&last) = pts.last() {
                painter.line_segment(
                    [last, c],
                    egui::Stroke::new(1.0, egui::Color32::from_gray(120)),
                );
            }
        }
        // Anchor glyphs.
        for el in self.path.elements().iter() {
            if let kurbo::PathEl::MoveTo(p) | kurbo::PathEl::LineTo(p) = *el {
                painter.circle_filled(to_screen(p), 3.0, egui::Color32::WHITE);
            }
        }
    }

    fn commit(&mut self, doc: &ogre_core::Document) -> Option<Box<dyn ogre_core::Command>> {
        if self.anchor_count < 2 {
            self.clear();
            return None;
        }
        let result = if let Some(id) = self.editing_layer {
            // Re-edit an existing vector layer in place.
            self.commit_vector_edit(doc, id)
        } else if self.commit_mode() == VectorCommitMode::Vector {
            self.commit_vector()
        } else {
            let layer = self.active_layer?;
            let Ok(l) = doc.layer(layer) else {
                self.clear();
                return None;
            };
            if !l.is_raster() || l.locked {
                self.clear();
                return None;
            }
            let offset = l.offset;
            self.rasterize(layer, offset)
        };
        // Clear all gesture state (including the mode snapshot and the resolved
        // active layer) so the next gesture re-snapshots the current mode and
        // re-resolves the active layer. Skipping this leaks `committed_mode`
        // and `active_layer` across gestures, which silently ignores sidebar
        // mode changes on the next path.
        self.clear();
        result
    }

    fn cancel(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc_with_raster() -> ogre_core::Document {
        let mut doc = ogre_core::Document::new(100, 100);
        let id = doc.add_layer(ogre_core::Layer::new_raster("Bg"));
        doc.order.push(id);
        doc.active = Some(id);
        doc
    }

    #[test]
    fn pen_starts_empty() {
        let pen = PenTool::new();
        assert!(pen.is_empty());
        assert_eq!(pen.anchor_count, 0);
    }

    #[test]
    fn pen_click_adds_corner_anchor() {
        let doc = doc_with_raster();
        let mut pen = PenTool::new();
        pen.mode = VectorCommitMode::Pixels;
        pen.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(10, 10), Phase::Down, egui::Modifiers::NONE),
        );
        pen.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(10, 10), Phase::Up, egui::Modifiers::NONE),
        );
        assert_eq!(pen.anchor_count, 1);
        assert!(!pen.is_empty());
    }

    #[test]
    fn pen_drag_adds_smooth_anchor() {
        let doc = doc_with_raster();
        let mut pen = PenTool::new();
        pen.mode = VectorCommitMode::Pixels;
        // First click places a move_to.
        pen.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(10, 10), Phase::Down, egui::Modifiers::NONE),
        );
        pen.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(10, 10), Phase::Up, egui::Modifiers::NONE),
        );
        // Second: click-drag to (40, 40) — a smooth anchor.
        pen.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(20, 20), Phase::Down, egui::Modifiers::NONE),
        );
        pen.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(40, 40), Phase::Drag, egui::Modifiers::NONE),
        );
        pen.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(40, 40), Phase::Up, egui::Modifiers::NONE),
        );
        assert_eq!(pen.anchor_count, 2);
        // The path should contain a curve element.
        assert!(pen
            .path
            .elements()
            .iter()
            .any(|el| matches!(*el, kurbo::PathEl::CurveTo(_, _, _))));
    }

    #[test]
    fn pen_cancel_clears() {
        let doc = doc_with_raster();
        let mut pen = PenTool::new();
        pen.mode = VectorCommitMode::Pixels;
        pen.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(10, 10), Phase::Down, egui::Modifiers::NONE),
        );
        pen.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(10, 10), Phase::Up, egui::Modifiers::NONE),
        );
        assert!(!pen.is_empty());
        pen.cancel();
        assert!(pen.is_empty());
    }

    #[test]
    fn pen_commit_with_too_few_anchors_cancels() {
        let doc = doc_with_raster();
        let mut pen = PenTool::new();
        pen.mode = VectorCommitMode::Pixels;
        pen.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(10, 10), Phase::Down, egui::Modifiers::NONE),
        );
        pen.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(10, 10), Phase::Up, egui::Modifiers::NONE),
        );
        let cmd = pen.commit(&doc);
        assert!(cmd.is_none());
        assert!(pen.is_empty());
    }

    /// After a successful commit the per-gesture mode snapshot and resolved
    /// active layer must be cleared so the next gesture honors a sidebar mode
    /// change. Regression test for the committed_mode/active_layer leak.
    #[test]
    fn pen_commit_clears_mode_snapshot_for_next_gesture() {
        let doc = doc_with_raster();
        let mut pen = PenTool::new();
        // First path: Vector mode (the default). Two anchors + commit.
        pen.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(10, 10), Phase::Down, egui::Modifiers::NONE),
        );
        pen.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(10, 10), Phase::Up, egui::Modifiers::NONE),
        );
        pen.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(40, 40), Phase::Down, egui::Modifiers::NONE),
        );
        pen.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(40, 40), Phase::Up, egui::Modifiers::NONE),
        );
        {
            let cmd = pen.commit(&doc).expect("first path commits");
            assert_eq!(cmd.label(), "Add vector layer");
        }
        assert!(pen.is_empty());

        // User switches to Pixels in the sidebar.
        pen.mode = VectorCommitMode::Pixels;

        // Second path: must honor Pixels → BrushStrokeCmd, not a stale Vector
        // snapshot from the previous gesture.
        pen.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(10, 10), Phase::Down, egui::Modifiers::NONE),
        );
        pen.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(10, 10), Phase::Up, egui::Modifiers::NONE),
        );
        pen.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(40, 40), Phase::Down, egui::Modifiers::NONE),
        );
        pen.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(40, 40), Phase::Up, egui::Modifiers::NONE),
        );
        let cmd = pen.commit(&doc).expect("second path commits");
        assert_eq!(
            cmd.label(),
            "Brush stroke",
            "second gesture must honor the Pixels mode selected after the first commit"
        );
    }

    #[test]
    fn pen_vector_mode_creates_vector_layer() {
        let doc = doc_with_raster();
        let mut pen = PenTool::new();
        // Default mode is Vector.
        assert_eq!(pen.mode, VectorCommitMode::Vector);
        // First anchor.
        pen.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(10, 10), Phase::Down, egui::Modifiers::NONE),
        );
        pen.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(10, 10), Phase::Up, egui::Modifiers::NONE),
        );
        // Second anchor.
        pen.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(40, 40), Phase::Down, egui::Modifiers::NONE),
        );
        pen.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(40, 40), Phase::Up, egui::Modifiers::NONE),
        );
        let mut cmd = pen
            .commit(&doc)
            .expect("vector mode should produce AddVectorLayerCmd");
        let mut doc = doc;
        cmd.apply(&mut doc).unwrap();
        let id = doc.active.unwrap();
        let layer = doc.layer(id).unwrap();
        assert_eq!(layer.name, "Path");
        assert!(matches!(layer.content, ogre_core::LayerContent::Vector(_)));
        if let ogre_core::LayerContent::Vector(data) = &layer.content {
            assert_eq!(layer.offset, glam::IVec2::new(10, 10));
            let min = data.paths[0]
                .vertices
                .iter()
                .copied()
                .reduce(|a, b| glam::IVec2::new(a.x.min(b.x), a.y.min(b.y)))
                .unwrap();
            assert_eq!(min, glam::IVec2::ZERO);
        }
        assert!(pen.is_empty());
    }

    /// Re-edit: loading an existing vector layer restores the geometry and
    /// style into the pen; committing then edits the layer in place via
    /// `EditVectorCmd`.
    #[test]
    fn pen_re_edit_loads_and_commits_in_place() {
        let mut doc = ogre_core::Document::new(200, 200);
        let _bg = doc.add_raster_layer("Bg");
        // A vector layer with a triangle in layer-local space, offset (15, 25).
        let data = ogre_core::VectorData {
            paths: vec![ogre_core::VectorPath {
                vertices: vec![
                    glam::IVec2::new(0, 0),
                    glam::IVec2::new(30, 0),
                    glam::IVec2::new(15, 30),
                ],
                fill: ogre_core::VectorFill::Solid(Rgba32F::new(0.0, 1.0, 0.0, 1.0)),
                stroke: ogre_core::VectorStroke {
                    color: Rgba32F::new(1.0, 1.0, 1.0, 1.0),
                    width: 3.0,
                    dash: Vec::new(),
                    cap: ogre_core::StrokeCap::Butt,
                    join: ogre_core::StrokeJoin::Miter,
                },
                closed: true,
            }],
            rasterized: None,
            text: None,
            svg_source: None,
            version: 0,
        };
        let vid = doc.add_vector_layer("Tri", data);
        doc.layer_mut(vid).unwrap().offset = glam::IVec2::new(15, 25);
        doc.active = Some(vid);

        let mut pen = PenTool::new();
        assert!(
            pen.load_active_vector_layer(&doc),
            "loading an active vector layer should succeed"
        );
        // Style inherited from the stored path.
        assert_eq!(pen.fill, Rgba32F::new(0.0, 1.0, 0.0, 1.0));
        assert_eq!(pen.stroke_width, 3);
        assert_eq!(pen.editing_layer, Some(vid));
        // The reconstructed path has the triangle's vertices (in doc space).
        assert!(!pen.is_empty());

        // Commit (without changes) edits the layer in place — no new layer.
        let mut cmd = pen.commit(&doc).expect("re-edit should commit");
        assert_eq!(cmd.label(), "Edit vector");
        let mut doc = doc;
        cmd.apply(&mut doc).unwrap();
        assert_eq!(doc.active, Some(vid));
        assert_eq!(doc.order.len(), 2, "re-edit must not add a new layer");
    }

    #[test]
    fn pen_re_edit_rejects_raster_layer() {
        let mut doc = ogre_core::Document::new(100, 100);
        let bg = doc.add_raster_layer("Bg");
        doc.active = Some(bg);
        let mut pen = PenTool::new();
        assert!(!pen.load_active_vector_layer(&doc));
        assert!(pen.editing_layer.is_none());
    }

    /// Selecting a non-vector layer after a successful re-edit must clear the
    /// stale editing target and loaded geometry, so the next commit cannot edit
    /// the previously-selected (wrong) layer. Regression test for the stale
    /// `editing_layer` / path-leak found in adversarial review.
    #[test]
    fn pen_re_edit_clears_stale_target_when_active_layer_changes() {
        let mut doc = ogre_core::Document::new(200, 200);
        let _bg = doc.add_raster_layer("Bg");
        let data = ogre_core::VectorData {
            paths: vec![ogre_core::VectorPath {
                vertices: vec![
                    glam::IVec2::new(0, 0),
                    glam::IVec2::new(30, 0),
                    glam::IVec2::new(15, 30),
                ],
                fill: ogre_core::VectorFill::Solid(Rgba32F::new(0.0, 1.0, 0.0, 1.0)),
                stroke: ogre_core::VectorStroke {
                    color: Rgba32F::TRANSPARENT,
                    width: 0.0,
                    dash: Vec::new(),
                    cap: ogre_core::StrokeCap::Butt,
                    join: ogre_core::StrokeJoin::Miter,
                },
                closed: true,
            }],
            rasterized: None,
            text: None,
            svg_source: None,
            version: 0,
        };
        let vid = doc.add_vector_layer("Tri", data);
        doc.active = Some(vid);

        let mut pen = PenTool::new();
        assert!(pen.load_active_vector_layer(&doc));
        assert_eq!(pen.editing_layer, Some(vid));
        assert!(!pen.is_empty());

        // User clicks a raster layer in the panel: active changes, load fails.
        doc.active = Some(_bg);
        assert!(
            !pen.load_active_vector_layer(&doc),
            "raster layer must not load into the pen"
        );
        assert!(
            pen.editing_layer.is_none(),
            "stale editing target must be cleared on failed re-load"
        );
        assert!(
            pen.is_empty(),
            "loaded geometry must be discarded when exiting re-edit"
        );

        // A commit now must NOT touch the vector layer — it has too few anchors
        // to commit at all, so it is a clean no-op rather than an edit of `vid`.
        let cmd = pen.commit(&doc);
        assert!(
            cmd.is_none(),
            "no command after exiting re-edit with an empty path"
        );
    }

    /// A freehand pen gesture (anchors placed, not from a re-edit) must survive
    /// a layer-panel selection: the load refuses so the in-progress work is not
    /// clobbered or re-targeted. Regression test for the clobber finding.
    #[test]
    fn pen_re_edit_refuses_during_freehand_gesture() {
        let mut doc = ogre_core::Document::new(200, 200);
        let _bg = doc.add_raster_layer("Bg");
        // A compatible vector layer that would otherwise load.
        let data = ogre_core::VectorData {
            paths: vec![ogre_core::VectorPath {
                vertices: vec![
                    glam::IVec2::new(0, 0),
                    glam::IVec2::new(30, 0),
                    glam::IVec2::new(15, 30),
                ],
                fill: ogre_core::VectorFill::None,
                stroke: ogre_core::VectorStroke {
                    color: Rgba32F::new(1.0, 1.0, 1.0, 1.0),
                    width: 2.0,
                    dash: Vec::new(),
                    cap: ogre_core::StrokeCap::Butt,
                    join: ogre_core::StrokeJoin::Miter,
                },
                closed: true,
            }],
            rasterized: None,
            text: None,
            svg_source: None,
            version: 0,
        };
        let vid = doc.add_vector_layer("Tri", data);
        doc.active = Some(_bg);

        let mut pen = PenTool::new();
        pen.mode = VectorCommitMode::Pixels;
        // Place two freehand anchors (not a re-edit).
        pen.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(5, 5), Phase::Down, egui::Modifiers::NONE),
        );
        pen.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(5, 5), Phase::Up, egui::Modifiers::NONE),
        );
        pen.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(50, 50), Phase::Down, egui::Modifiers::NONE),
        );
        pen.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(50, 50), Phase::Up, egui::Modifiers::NONE),
        );
        assert_eq!(pen.anchor_count, 2);
        assert!(pen.editing_layer.is_none());

        // User selects the vector layer in the panel: load must refuse.
        doc.active = Some(vid);
        assert!(
            !pen.load_active_vector_layer(&doc),
            "freehand gesture must not be clobbered by a layer switch"
        );
        assert!(pen.editing_layer.is_none(), "no re-edit target acquired");
        assert_eq!(pen.anchor_count, 2, "freehand anchors preserved");
    }

    /// Re-editing a vector layer must bump `VectorData::version` via
    /// `mark_dirty()` so the compositor re-rasterizes, and must preserve the
    /// existing version counter rather than resetting it.
    #[test]
    fn pen_re_edit_bumps_vector_version() {
        let mut doc = ogre_core::Document::new(200, 200);
        let _bg = doc.add_raster_layer("Bg");
        let starting_version = 5;
        let data = ogre_core::VectorData {
            paths: vec![ogre_core::VectorPath {
                vertices: vec![
                    glam::IVec2::new(0, 0),
                    glam::IVec2::new(30, 0),
                    glam::IVec2::new(15, 30),
                ],
                fill: ogre_core::VectorFill::None,
                stroke: ogre_core::VectorStroke {
                    color: Rgba32F::new(1.0, 1.0, 1.0, 1.0),
                    width: 2.0,
                    dash: Vec::new(),
                    cap: ogre_core::StrokeCap::Butt,
                    join: ogre_core::StrokeJoin::Miter,
                },
                closed: true,
            }],
            rasterized: None,
            text: None,
            svg_source: None,
            version: starting_version,
        };
        let vid = doc.add_vector_layer("Tri", data);
        doc.active = Some(vid);

        let mut pen = PenTool::new();
        assert!(pen.load_active_vector_layer(&doc));
        let mut cmd = pen.commit(&doc).expect("re-edit should commit");
        let mut doc = doc;
        cmd.apply(&mut doc).unwrap();
        let layer = doc.layer(vid).unwrap();
        let ogre_core::LayerContent::Vector(data) = &layer.content else {
            panic!("layer must remain vector");
        };
        assert!(
            data.version > starting_version,
            "mark_dirty must increment the existing version, not reset it"
        );
    }
}
