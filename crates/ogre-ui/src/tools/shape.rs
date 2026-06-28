// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Shape tools (§3.3.2). Drag a rectangle / ellipse / line; on release the
//! shape is either committed as a non-destructive vector layer (default) or
//! rasterized into the active raster layer via a [`BrushStrokeCmd`] in Pixels
//! mode. Polygon uses click-to-add (Enter / close-vertex to commit).

use glam::Vec2;
use kurbo::Shape;
use ogre_core::{
    AddVectorLayerCmd, BrushStrokeCmd, Rgba32F, StrokeCap, StrokeJoin, VectorData, VectorFill,
    VectorPath, VectorStroke,
};
use ogre_vector::{bezpath_to_polygon, ellipse_bezpath, rasterize_bezpath, Fill, Stroke};

use crate::tools::vector_commit::{buffer_to_edits, localize_path};
use crate::tools::{Phase, PointerEvent, Tool, VectorCommitMode};

/// Click within this many document pixels of the first polygon vertex closes
/// the path.
const POLY_CLOSE_THRESHOLD_PX: i32 = 6;

/// Which shape a [`ShapeTool`] draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShapeKind {
    /// Axis-aligned rectangle (Shift forces a square).
    Rect,
    /// Ellipse (Shift forces a circle).
    Ellipse,
    /// Straight line (Shift constrains to 45° increments).
    Line,
    /// Click-to-add polygon (Enter or close-vertex to commit).
    Polygon,
}

/// Shape tool: drag p0→p1, release to commit. For Polygon, click-to-add
/// vertices; Enter or clicking the first vertex closes.
#[derive(Debug)]
pub struct ShapeTool {
    kind: ShapeKind,
    fill: Rgba32F,
    stroke_color: Rgba32F,
    stroke_width: u32,
    /// Whether to commit as a vector layer or rasterize into the active layer.
    mode: VectorCommitMode,
    /// Mode snapshot taken at the start of a gesture so a mid-gesture sidebar
    /// change cannot switch the target of the in-progress shape.
    committed_mode: Option<VectorCommitMode>,
    active_layer: Option<ogre_core::LayerId>,
    p0: Option<glam::IVec2>,
    p1: Option<glam::IVec2>,
    /// In-progress polygon vertices (document space) for Polygon mode.
    poly_pts: Vec<glam::IVec2>,
    /// Current cursor position for the polygon rubber-band preview.
    poly_cursor: Option<glam::IVec2>,
    /// When re-editing an existing vector layer, the id of that layer; the next
    /// commit dispatches [`ogre_core::EditVectorCmd`] instead of
    /// [`AddVectorLayerCmd`].
    editing_layer: Option<ogre_core::LayerId>,
}

impl ShapeTool {
    /// Construct a shape tool of the given kind with default fg fill / bg stroke.
    pub fn new(kind: ShapeKind) -> Self {
        Self {
            kind,
            fill: Rgba32F::new(0.0, 0.0, 0.0, 1.0),
            stroke_color: Rgba32F::new(1.0, 1.0, 1.0, 1.0),
            stroke_width: 1,
            mode: VectorCommitMode::Vector,
            committed_mode: None,
            active_layer: None,
            p0: None,
            p1: None,
            poly_pts: Vec::new(),
            poly_cursor: None,
            editing_layer: None,
        }
    }

    /// Mutable `(fill, stroke_color, stroke_width, commit_mode)` for the sidebar.
    pub fn controls_mut(
        &mut self,
    ) -> (&mut Rgba32F, &mut Rgba32F, &mut u32, &mut VectorCommitMode) {
        (
            &mut self.fill,
            &mut self.stroke_color,
            &mut self.stroke_width,
            &mut self.mode,
        )
    }

    /// Mutable access to the commit mode for the sidebar.
    pub fn mode_mut(&mut self) -> &mut VectorCommitMode {
        &mut self.mode
    }

    /// The mode that should govern the current in-progress gesture.
    fn commit_mode(&self) -> VectorCommitMode {
        self.committed_mode.unwrap_or(self.mode)
    }

    /// Push the shared foreground (fill) + background (stroke) colors.
    pub fn set_colors(&mut self, fill: Rgba32F, stroke: Rgba32F) {
        self.fill = fill;
        self.stroke_color = stroke;
    }

    /// Build the shape's `kurbo::BezPath` (document space) from the two
    /// endpoints, with Shift = constrain and Alt = from-center modifiers applied.
    fn bezpath(&self, p0: glam::IVec2, p1: glam::IVec2, shift: bool) -> Option<kurbo::BezPath> {
        match self.kind {
            ShapeKind::Rect | ShapeKind::Ellipse => {
                let (a, b) = if shift {
                    // Force a square: side = max(dx, dy).
                    let dx = (p1.x - p0.x).abs();
                    let dy = (p1.y - p0.y).abs();
                    let s = dx.max(dy);
                    let sx = if p1.x >= p0.x { s } else { -s };
                    let sy = if p1.y >= p0.y { s } else { -s };
                    (p0, glam::IVec2::new(p0.x + sx, p0.y + sy))
                } else {
                    (p0, p1)
                };
                if self.kind == ShapeKind::Rect {
                    let x = a.x.min(b.x) as f64;
                    let y = a.y.min(b.y) as f64;
                    let w = (b.x - a.x).abs() as f64;
                    let h = (b.y - a.y).abs() as f64;
                    Some(kurbo::Rect::new(x, y, x + w, y + h).to_path(1e-3))
                } else {
                    let cx = ((a.x + b.x) / 2) as f64;
                    let cy = ((a.y + b.y) / 2) as f64;
                    let rx = ((b.x - a.x).abs() / 2).max(1) as f64;
                    let ry = ((b.y - a.y).abs() / 2).max(1) as f64;
                    Some(ellipse_bezpath(cx, cy, rx, ry))
                }
            }
            ShapeKind::Line => {
                let mut end = p1;
                if shift {
                    // Snap to the nearest 45° increment.
                    let d = end - p0;
                    let angle = (d.y as f32).atan2(d.x as f32);
                    let snap =
                        (angle / std::f32::consts::FRAC_PI_4).round() * std::f32::consts::FRAC_PI_4;
                    let len = (d.as_vec2().length()) as i32;
                    end = p0
                        + glam::IVec2::new(
                            (snap.cos() * len as f32) as i32,
                            (snap.sin() * len as f32) as i32,
                        );
                }
                let mut path = kurbo::BezPath::new();
                path.move_to((p0.x as f64, p0.y as f64));
                path.line_to((end.x as f64, end.y as f64));
                Some(path)
            }
            ShapeKind::Polygon => None,
        }
    }

    /// Build a `VectorPath` from a `kurbo::BezPath` in document space.
    fn vector_path_from_bezpath(&self, path: &kurbo::BezPath, closed: bool) -> VectorPath {
        let vertices = bezpath_to_polygon(path, 0.5);
        VectorPath {
            vertices,
            fill: if closed {
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
            closed,
        }
    }

    /// Build a `VectorPath` from the in-progress polygon vertices.
    fn vector_path_from_polygon(&self) -> VectorPath {
        VectorPath {
            vertices: self.poly_pts.clone(),
            fill: VectorFill::Solid(self.fill),
            stroke: VectorStroke {
                color: self.stroke_color,
                width: self.stroke_width as f32,
                dash: Vec::new(),
                cap: StrokeCap::Butt,
                join: StrokeJoin::Miter,
            },
            closed: true,
        }
    }

    /// Commit a shape as a new vector layer.
    fn commit_vector(
        &self,
        path: &kurbo::BezPath,
        closed: bool,
    ) -> Option<Box<dyn ogre_core::Command>> {
        let mut vpath = self.vector_path_from_bezpath(path, closed);
        if vpath.vertices.len() < 2 {
            return None;
        }
        let offset = localize_path(&mut vpath);
        let name = match self.kind {
            ShapeKind::Rect => "Rectangle",
            ShapeKind::Ellipse => "Ellipse",
            ShapeKind::Line => "Line",
            ShapeKind::Polygon => "Polygon",
        };
        let data = VectorData {
            paths: vec![vpath],
            ..Default::default()
        };
        Some(
            Box::new(AddVectorLayerCmd::new(name, data).with_offset(offset))
                as Box<dyn ogre_core::Command>,
        )
    }

    /// Replace the shape on an existing vector layer (re-edit path). The new
    /// geometry is localized against the layer's existing offset so the layer
    /// stays put. Used by both the drag tools (Rect/Ellipse/Line) and Polygon.
    fn commit_vector_edit(
        &self,
        doc: &ogre_core::Document,
        id: ogre_core::LayerId,
        path: &kurbo::BezPath,
        closed: bool,
    ) -> Option<Box<dyn ogre_core::Command>> {
        let layer = doc.layer(id).ok()?;
        let layer_offset = layer.offset;
        let mut vpath = self.vector_path_from_bezpath(path, closed);
        if vpath.vertices.len() < 2 {
            return None;
        }
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

    /// Rasterize a `kurbo::BezPath` into layer edits, mapping document→local.
    fn rasterize_path(
        &self,
        layer: ogre_core::LayerId,
        offset: glam::IVec2,
        path: &kurbo::BezPath,
    ) -> Option<Box<dyn ogre_core::Command>> {
        let fill = if self.fill.a > 0.0 {
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
        let buf = rasterize_bezpath(path, fill, stroke);
        let edits = buffer_to_edits(&buf, glam::IVec2::ZERO, offset);
        if edits.is_empty() {
            return None;
        }
        Some(Box::new(BrushStrokeCmd::new(layer, edits)) as Box<dyn ogre_core::Command>)
    }

    /// If the active layer is a vector layer whose single path matches this
    /// tool's `kind`, inherit its fill/stroke style and target it for in-place
    /// re-edit. Returns `true` if loaded.
    ///
    /// Detection is deliberately conservative (spec §4 risks): a rect needs an
    /// axis-aligned rectangular outline; a line needs exactly two open
    /// vertices; a polygon needs a closed ≥3-vertex outline. Ellipses are not
    /// detected from their flattened polygon — fall back to creating a new
    /// layer.
    ///
    /// Always drops any prior re-edit target first, so a failed load cannot
    /// leave a stale `editing_layer` pointing at the previously-selected layer
    /// (which would make the next commit edit the wrong layer).
    ///
    /// Refuses (returns `false`, no mutation) when a gesture is in progress, so
    /// selecting a layer mid-draw does not clobber the in-progress work.
    pub fn load_active_vector_layer(&mut self, doc: &ogre_core::Document) -> bool {
        if self.p0.is_some() || self.p1.is_some() || !self.poly_pts.is_empty() {
            return false;
        }
        self.editing_layer = None;
        let Some(id) = doc.active else {
            return false;
        };
        let Ok(layer) = doc.layer(id) else {
            return false;
        };
        let ogre_core::LayerContent::Vector(data) = &layer.content else {
            return false;
        };
        if data.paths.len() != 1 {
            return false;
        }
        let vpath = &data.paths[0];
        if !path_matches_kind(self.kind, vpath) {
            return false;
        }
        // Inherit style so the next shape matches.
        self.fill = match vpath.fill {
            VectorFill::Solid(color) => color,
            VectorFill::None => Rgba32F::TRANSPARENT,
        };
        self.stroke_color = vpath.stroke.color;
        self.stroke_width = vpath.stroke.width.round() as u32;
        self.editing_layer = Some(id);
        true
    }

    /// Polygon click-to-add interaction: each click adds a vertex; clicking
    /// near the first vertex (≥3 placed) or pressing Enter closes.
    fn on_pointer_polygon(
        &mut self,
        doc: &ogre_core::Document,
        ev: PointerEvent,
    ) -> Option<Box<dyn ogre_core::Command>> {
        match ev.phase {
            Phase::Down => {
                if self.committed_mode.is_none() {
                    self.committed_mode = Some(self.mode);
                }
                // In Pixels mode we need a raster layer to paint into.
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
                if self.poly_pts.is_empty() {
                    self.poly_pts.push(ev.doc_pos);
                } else if let Some(&first) = self.poly_pts.first() {
                    let near_start = (ev.doc_pos - first).length_squared()
                        <= POLY_CLOSE_THRESHOLD_PX * POLY_CLOSE_THRESHOLD_PX;
                    if near_start && self.poly_pts.len() >= 3 {
                        return self.commit_polygon(doc);
                    } else {
                        self.poly_pts.push(ev.doc_pos);
                    }
                }
                None
            }
            Phase::Drag => {
                self.poly_cursor = Some(ev.doc_pos);
                None
            }
            Phase::Up => {
                self.poly_cursor = Some(ev.doc_pos);
                None
            }
        }
    }

    /// Close and commit the polygon (≥3 vertices required).
    fn commit_polygon(&mut self, doc: &ogre_core::Document) -> Option<Box<dyn ogre_core::Command>> {
        if self.poly_pts.len() < 3 {
            self.poly_pts.clear();
            self.committed_mode = None;
            self.editing_layer = None;
            return None;
        }
        let cmd = if self.commit_mode() == VectorCommitMode::Vector {
            if let Some(id) = self.editing_layer {
                // Re-edit in place: build a kurbo path from the polygon points.
                let mut path = kurbo::BezPath::new();
                if let Some(&first) = self.poly_pts.first() {
                    path.move_to((first.x as f64, first.y as f64));
                    for &p in self.poly_pts.iter().skip(1) {
                        path.line_to((p.x as f64, p.y as f64));
                    }
                    path.close_path();
                }
                self.commit_vector_edit(doc, id, &path, true)
            } else {
                self.commit_vector_polygon()
            }
        } else {
            let layer = self.active_layer?;
            let Ok(l) = doc.layer(layer) else {
                self.poly_pts.clear();
                self.committed_mode = None;
                self.editing_layer = None;
                return None;
            };
            if !l.is_raster() || l.locked {
                self.poly_pts.clear();
                self.committed_mode = None;
                self.editing_layer = None;
                return None;
            }
            let offset = l.offset;
            let mut path = kurbo::BezPath::new();
            if let Some(&first) = self.poly_pts.first() {
                path.move_to((first.x as f64, first.y as f64));
                for &p in self.poly_pts.iter().skip(1) {
                    path.line_to((p.x as f64, p.y as f64));
                }
                path.close_path();
            }
            self.rasterize_path(layer, offset, &path)
        };
        self.poly_pts.clear();
        self.poly_cursor = None;
        self.committed_mode = None;
        self.editing_layer = None;
        cmd
    }

    /// Commit the polygon as a new vector layer.
    fn commit_vector_polygon(&self) -> Option<Box<dyn ogre_core::Command>> {
        let mut vpath = self.vector_path_from_polygon();
        if vpath.vertices.len() < 3 {
            return None;
        }
        let offset = localize_path(&mut vpath);
        let data = VectorData {
            paths: vec![vpath],
            ..Default::default()
        };
        Some(
            Box::new(AddVectorLayerCmd::new("Polygon", data).with_offset(offset))
                as Box<dyn ogre_core::Command>,
        )
    }

    fn draw_overlay_polygon(
        &self,
        painter: &egui::Painter,
        viewport: &ogre_gpu::Viewport,
        canvas_rect: egui::Rect,
        pixels_per_point: f32,
    ) {
        if self.poly_pts.is_empty() {
            return;
        }
        let to_screen = |p: glam::IVec2| -> egui::Pos2 {
            let v = (p.as_vec2() - viewport.pan) * viewport.zoom / pixels_per_point
                + Vec2::new(canvas_rect.min.x, canvas_rect.min.y);
            egui::Pos2::new(v.x, v.y)
        };
        let stroke = egui::Stroke::new(1.0, egui::Color32::WHITE);
        let pts: Vec<egui::Pos2> = self.poly_pts.iter().map(|&p| to_screen(p)).collect();
        if pts.len() >= 2 {
            painter.add(egui::Shape::line(pts.clone(), stroke));
        }
        // Rubber-band line to cursor.
        if let Some(cursor) = self.poly_cursor {
            let c = to_screen(cursor);
            if let Some(&last) = pts.last() {
                painter.line_segment(
                    [last, c],
                    egui::Stroke::new(1.0, egui::Color32::from_gray(120)),
                );
            }
        }
        // Anchor glyphs.
        for &p in &pts {
            painter.circle_filled(p, 3.0, egui::Color32::WHITE);
        }
    }
}

impl Tool for ShapeTool {
    fn on_pointer(
        &mut self,
        doc: &ogre_core::Document,
        ev: PointerEvent,
    ) -> Option<Box<dyn ogre_core::Command>> {
        if self.kind == ShapeKind::Polygon {
            return self.on_pointer_polygon(doc, ev);
        }
        match ev.phase {
            Phase::Down => {
                self.active_layer = None;
                self.p0 = None;
                self.p1 = None;
                self.committed_mode = Some(self.mode);
                // A re-edit target is a vector layer we edit in place — it does
                // not need an unlocked raster layer. Otherwise, Pixels mode
                // needs one to paint into; Vector mode creates a new layer.
                let can_paint = if self.editing_layer.is_some() {
                    true
                } else if self.commit_mode() == VectorCommitMode::Pixels {
                    doc.active.is_some_and(|layer| {
                        doc.layer(layer).is_ok_and(|l| l.is_raster() && !l.locked)
                    })
                } else {
                    true
                };
                if can_paint {
                    if self.editing_layer.is_none() {
                        if let Some(&layer) = doc.active.as_ref() {
                            self.active_layer = Some(layer);
                        }
                    }
                    self.p0 = Some(ev.doc_pos);
                    self.p1 = Some(ev.doc_pos);
                }
                None
            }
            Phase::Drag => {
                if self.p0.is_some() {
                    self.p1 = Some(ev.doc_pos);
                }
                None
            }
            Phase::Up => {
                let cmd = (|| {
                    let (Some(p0), Some(p1)) = (self.p0, self.p1) else {
                        return None;
                    };
                    if (p1 - p0).as_vec2().length() < 1.0 {
                        return None; // ignore a pure click
                    }
                    let path = self.bezpath(p0, p1, ev.modifiers.shift)?;
                    let closed = self.kind != ShapeKind::Line;
                    // A loaded re-edit target is authoritative: edit it in place
                    // regardless of the sidebar Vector/Pixels mode (re-edit only
                    // ever targets a vector layer).
                    if let Some(id) = self.editing_layer {
                        return self.commit_vector_edit(doc, id, &path, closed);
                    }
                    if self.commit_mode() == VectorCommitMode::Vector {
                        return self.commit_vector(&path, closed);
                    }
                    let layer = self.active_layer?;
                    let Ok(l) = doc.layer(layer) else { return None };
                    if !l.is_raster() || l.locked {
                        return None;
                    }
                    let offset = l.offset;
                    self.rasterize_path(layer, offset, &path)
                })();
                self.active_layer = None;
                self.p0 = None;
                self.p1 = None;
                self.committed_mode = None;
                self.editing_layer = None;
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
        if self.kind == ShapeKind::Polygon {
            self.draw_overlay_polygon(painter, viewport, canvas_rect, pixels_per_point);
            return;
        }
        let (Some(p0), Some(p1)) = (self.p0, self.p1) else {
            return;
        };
        let to_screen = |p: glam::IVec2| -> egui::Pos2 {
            let v = (p.as_vec2() - viewport.pan) * viewport.zoom / pixels_per_point
                + Vec2::new(canvas_rect.min.x, canvas_rect.min.y);
            egui::Pos2::new(v.x, v.y)
        };
        let stroke = egui::Stroke::new(1.0, egui::Color32::WHITE);
        match self.kind {
            ShapeKind::Rect => {
                let a = to_screen(glam::IVec2::new(p0.x.min(p1.x), p0.y.min(p1.y)));
                let b = to_screen(glam::IVec2::new(p0.x.max(p1.x), p0.y.max(p1.y)));
                painter.rect_stroke(
                    egui::Rect::from_min_max(a, b),
                    egui::CornerRadius::ZERO,
                    stroke,
                    egui::StrokeKind::Middle,
                );
            }
            ShapeKind::Ellipse => {
                let c = to_screen(glam::IVec2::new((p0.x + p1.x) / 2, (p0.y + p1.y) / 2));
                let r = ((p1 - p0).as_vec2() * viewport.zoom / pixels_per_point / 2.0).abs();
                painter.circle_stroke(c, r.x.max(r.y), stroke);
            }
            ShapeKind::Line => {
                painter.line_segment([to_screen(p0), to_screen(p1)], stroke);
            }
            ShapeKind::Polygon => {}
        }
    }

    fn commit(&mut self, doc: &ogre_core::Document) -> Option<Box<dyn ogre_core::Command>> {
        if self.kind == ShapeKind::Polygon {
            return self.commit_polygon(doc);
        }
        None
    }

    fn cancel(&mut self) {
        self.active_layer = None;
        self.p0 = None;
        self.p1 = None;
        self.poly_pts.clear();
        self.poly_cursor = None;
        self.committed_mode = None;
        self.editing_layer = None;
    }
}

/// Whether a stored flattened path is compatible with `kind` for re-edit.
///
/// Conservative on purpose (spec §4 risks): ellipses are not detected from
/// their flattened polygon and fall back to creating a new layer.
fn path_matches_kind(kind: ShapeKind, vpath: &ogre_core::VectorPath) -> bool {
    let verts = &vpath.vertices;
    match kind {
        ShapeKind::Line => verts.len() == 2 && !vpath.closed,
        ShapeKind::Rect => {
            // Axis-aligned rectangle: at least 3 closed vertices, every vertex
            // lying on the bounding-box perimeter.
            if !vpath.closed || verts.len() < 3 {
                return false;
            }
            let (min, max) = verts.iter().fold(
                (
                    glam::IVec2::new(i32::MAX, i32::MAX),
                    glam::IVec2::new(i32::MIN, i32::MIN),
                ),
                |(mn, mx), &p| (mn.min(p), mx.max(p)),
            );
            if min.x >= max.x || min.y >= max.y {
                return false;
            }
            verts
                .iter()
                .all(|&p| (p.x == min.x || p.x == max.x) && (p.y == min.y || p.y == max.y))
        }
        ShapeKind::Polygon => vpath.closed && verts.len() >= 3,
        // Ellipses are stored as flattened polygons; recognising them reliably
        // is fragile, so the Ellipse tool does not re-edit for this pass.
        ShapeKind::Ellipse => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{Phase, PointerEvent};

    fn doc_with_layer() -> ogre_core::Document {
        let mut doc = ogre_core::Document::new(100, 100);
        let id = doc.add_raster_layer("L");
        doc.active = Some(id);
        doc
    }

    #[test]
    fn rect_vector_mode_creates_vector_layer() {
        let doc = doc_with_layer();
        let mut tool = ShapeTool::new(ShapeKind::Rect);
        assert_eq!(tool.mode, VectorCommitMode::Vector);

        let _ = tool.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(10, 10), Phase::Down, egui::Modifiers::NONE),
        );
        let _ = tool.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(40, 40), Phase::Drag, egui::Modifiers::NONE),
        );
        let mut cmd = tool
            .on_pointer(
                &doc,
                PointerEvent::new(glam::IVec2::new(40, 40), Phase::Up, egui::Modifiers::NONE),
            )
            .expect("drag should produce a command");

        let mut doc = doc;
        cmd.apply(&mut doc).unwrap();
        let id = doc.active.unwrap();
        let layer = doc.layer(id).unwrap();
        assert_eq!(layer.name, "Rectangle");
        assert!(matches!(layer.content, ogre_core::LayerContent::Vector(_)));
        if let ogre_core::LayerContent::Vector(data) = &layer.content {
            assert_eq!(layer.offset, glam::IVec2::new(10, 10));
            let min = data.paths[0]
                .vertices
                .iter()
                .copied()
                .reduce(|a, b| glam::IVec2::new(a.x.min(b.x), a.y.min(b.y)))
                .unwrap();
            let max = data.paths[0]
                .vertices
                .iter()
                .copied()
                .reduce(|a, b| glam::IVec2::new(a.x.max(b.x), a.y.max(b.y)))
                .unwrap();
            assert_eq!(min, glam::IVec2::ZERO);
            assert_eq!(max, glam::IVec2::new(30, 30));
        }
    }

    #[test]
    fn polygon_vector_mode_creates_vector_layer() {
        let doc = doc_with_layer();
        let mut tool = ShapeTool::new(ShapeKind::Polygon);
        assert_eq!(tool.mode, VectorCommitMode::Vector);

        for p in [
            glam::IVec2::new(10, 10),
            glam::IVec2::new(40, 10),
            glam::IVec2::new(25, 40),
        ] {
            let _ = tool.on_pointer(
                &doc,
                PointerEvent::new(p, Phase::Down, egui::Modifiers::NONE),
            );
        }
        let mut cmd = tool.commit(&doc).expect("polygon should commit");

        let mut doc = doc;
        cmd.apply(&mut doc).unwrap();
        let id = doc.active.unwrap();
        let layer = doc.layer(id).unwrap();
        assert_eq!(layer.name, "Polygon");
        assert!(matches!(layer.content, ogre_core::LayerContent::Vector(_)));
        if let ogre_core::LayerContent::Vector(data) = &layer.content {
            assert_eq!(layer.offset, glam::IVec2::new(10, 10));
            let min = data.paths[0]
                .vertices
                .iter()
                .copied()
                .reduce(|a, b| glam::IVec2::new(a.x.min(b.x), a.y.min(b.y)))
                .unwrap();
            let max = data.paths[0]
                .vertices
                .iter()
                .copied()
                .reduce(|a, b| glam::IVec2::new(a.x.max(b.x), a.y.max(b.y)))
                .unwrap();
            assert_eq!(min, glam::IVec2::ZERO);
            assert_eq!(max, glam::IVec2::new(30, 30));
        }
    }

    /// Re-edit: a stored rectangle loads into the Rect tool (style inherited,
    /// layer targeted); a subsequent drag replaces it in place via
    /// `EditVectorCmd`. Unsupported geometry (ellipse polygon) falls back.
    #[test]
    fn shape_rect_re_edit_loads_and_commits_in_place() {
        let mut doc = ogre_core::Document::new(200, 200);
        let _bg = doc.add_raster_layer("Bg");
        let data = ogre_core::VectorData {
            paths: vec![ogre_core::VectorPath {
                vertices: vec![
                    glam::IVec2::new(0, 0),
                    glam::IVec2::new(40, 0),
                    glam::IVec2::new(40, 30),
                    glam::IVec2::new(0, 30),
                ],
                fill: ogre_core::VectorFill::Solid(Rgba32F::new(0.0, 0.0, 1.0, 1.0)),
                stroke: ogre_core::VectorStroke {
                    color: Rgba32F::new(1.0, 1.0, 1.0, 1.0),
                    width: 5.0,
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
        let vid = doc.add_vector_layer("Rectangle", data);
        doc.layer_mut(vid).unwrap().offset = glam::IVec2::new(11, 22);
        doc.active = Some(vid);

        let mut tool = ShapeTool::new(ShapeKind::Rect);
        assert!(
            tool.load_active_vector_layer(&doc),
            "rect vector layer should load into the Rect tool"
        );
        assert_eq!(tool.fill, Rgba32F::new(0.0, 0.0, 1.0, 1.0));
        assert_eq!(tool.stroke_width, 5);
        assert_eq!(tool.editing_layer, Some(vid));

        // Drag a new rect; commit replaces the layer in place.
        let _ = tool.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(5, 5), Phase::Down, egui::Modifiers::NONE),
        );
        let _ = tool.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(55, 55), Phase::Drag, egui::Modifiers::NONE),
        );
        let mut cmd = tool
            .on_pointer(
                &doc,
                PointerEvent::new(glam::IVec2::new(55, 55), Phase::Up, egui::Modifiers::NONE),
            )
            .expect("drag should commit");
        assert_eq!(cmd.label(), "Edit vector");
        let mut doc = doc;
        cmd.apply(&mut doc).unwrap();
        assert_eq!(doc.active, Some(vid));
        assert_eq!(doc.order.len(), 2, "re-edit must not add a new layer");
        // The editing flag clears after the commit so the next drag is a new layer.
        assert!(tool.editing_layer.is_none());
    }

    #[test]
    fn shape_ellipse_re_edit_falls_back() {
        let mut doc = ogre_core::Document::new(200, 200);
        let _bg = doc.add_raster_layer("Bg");
        // Many-vertex closed polygon (as an ellipse would flatten to).
        let verts: Vec<glam::IVec2> = (0..24)
            .map(|i| {
                let a = (i as f32) * std::f32::consts::TAU / 24.0;
                glam::IVec2::new((a.cos() * 20.0) as i32, (a.sin() * 20.0) as i32)
            })
            .collect();
        let data = ogre_core::VectorData {
            paths: vec![ogre_core::VectorPath {
                vertices: verts,
                fill: ogre_core::VectorFill::None,
                stroke: ogre_core::VectorStroke {
                    color: Rgba32F::new(1.0, 1.0, 1.0, 1.0),
                    width: 1.0,
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
        let vid = doc.add_vector_layer("Ellipse", data);
        doc.active = Some(vid);

        // Ellipse detection is intentionally not implemented for this pass.
        let mut tool = ShapeTool::new(ShapeKind::Ellipse);
        assert!(
            !tool.load_active_vector_layer(&doc),
            "ellipse re-edit must fall back to creating a new layer"
        );
        assert!(tool.editing_layer.is_none());

        // The Rect tool also rejects the ellipse polygon (vertices not on an
        // axis-aligned rectangle's perimeter).
        let mut rect_tool = ShapeTool::new(ShapeKind::Rect);
        assert!(!rect_tool.load_active_vector_layer(&doc));
    }

    /// After a successful re-edit load, selecting an incompatible layer must
    /// clear the stale `editing_layer` so the next drag cannot replace the
    /// previously-selected (wrong) layer. Regression test for the stale
    /// editing-target found in adversarial review.
    #[test]
    fn shape_re_edit_clears_stale_target_when_active_layer_changes() {
        let mut doc = ogre_core::Document::new(200, 200);
        let bg = doc.add_raster_layer("Bg");
        let data = ogre_core::VectorData {
            paths: vec![ogre_core::VectorPath {
                vertices: vec![
                    glam::IVec2::new(0, 0),
                    glam::IVec2::new(40, 0),
                    glam::IVec2::new(40, 30),
                    glam::IVec2::new(0, 30),
                ],
                fill: ogre_core::VectorFill::Solid(Rgba32F::new(0.0, 0.0, 1.0, 1.0)),
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
        let vid = doc.add_vector_layer("Rectangle", data);
        doc.active = Some(vid);

        let mut tool = ShapeTool::new(ShapeKind::Rect);
        assert!(tool.load_active_vector_layer(&doc));
        assert_eq!(tool.editing_layer, Some(vid));

        // User selects the raster background: load must fail AND clear the
        // stale editing target.
        doc.active = Some(bg);
        assert!(!tool.load_active_vector_layer(&doc));
        assert!(
            tool.editing_layer.is_none(),
            "stale editing target must be cleared on failed re-load"
        );

        // A subsequent drag must create a new layer, not edit `vid`.
        let _ = tool.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(5, 5), Phase::Down, egui::Modifiers::NONE),
        );
        let _ = tool.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(50, 50), Phase::Drag, egui::Modifiers::NONE),
        );
        let mut cmd = tool
            .on_pointer(
                &doc,
                PointerEvent::new(glam::IVec2::new(50, 50), Phase::Up, egui::Modifiers::NONE),
            )
            .expect("drag should commit a new layer");
        assert_ne!(
            cmd.label(),
            "Edit vector",
            "must not edit the stale target after active-layer change"
        );
        let mut doc = doc;
        cmd.apply(&mut doc).unwrap();
        assert_eq!(doc.order.len(), 3, "a new layer was added, vid untouched");
    }

    /// An in-progress polygon (poly_pts placed) must survive a layer-panel
    /// selection: the load refuses so the polygon is not clobbered or silently
    /// re-targeted at the newly selected layer. Regression test for the HIGH
    /// clobber finding (Shape/Polygon path).
    #[test]
    fn shape_re_edit_refuses_during_in_progress_polygon() {
        let mut doc = ogre_core::Document::new(200, 200);
        let _bg = doc.add_raster_layer("Bg");
        // A compatible rect vector layer that would otherwise load.
        let data = ogre_core::VectorData {
            paths: vec![ogre_core::VectorPath {
                vertices: vec![
                    glam::IVec2::new(0, 0),
                    glam::IVec2::new(40, 0),
                    glam::IVec2::new(40, 30),
                    glam::IVec2::new(0, 30),
                ],
                fill: ogre_core::VectorFill::None,
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
        let vid = doc.add_vector_layer("Rectangle", data);

        let mut tool = ShapeTool::new(ShapeKind::Polygon);
        // Place two polygon vertices (in-progress polygon).
        doc.active = Some(_bg);
        for p in [glam::IVec2::new(5, 5), glam::IVec2::new(50, 5)] {
            let _ = tool.on_pointer(
                &doc,
                PointerEvent::new(p, Phase::Down, egui::Modifiers::NONE),
            );
        }
        assert_eq!(tool.poly_pts.len(), 2);

        // User selects the rect layer in the panel: load must refuse.
        doc.active = Some(vid);
        assert!(
            !tool.load_active_vector_layer(&doc),
            "in-progress polygon must not be clobbered by a layer switch"
        );
        assert!(tool.editing_layer.is_none(), "no re-edit target acquired");
        assert_eq!(tool.poly_pts.len(), 2, "polygon vertices preserved");
    }

    /// Re-edit in the Shape tool works even when the sidebar is in Pixels mode:
    /// `editing_layer` is authoritative, so the commit edits the loaded layer in
    /// place rather than no-oping. Regression test for the MEDIUM mode-disagreement
    /// finding.
    #[test]
    fn shape_re_edit_authoritative_even_in_pixels_mode() {
        let mut doc = ogre_core::Document::new(200, 200);
        let _bg = doc.add_raster_layer("Bg");
        let data = ogre_core::VectorData {
            paths: vec![ogre_core::VectorPath {
                vertices: vec![
                    glam::IVec2::new(0, 0),
                    glam::IVec2::new(40, 0),
                    glam::IVec2::new(40, 30),
                    glam::IVec2::new(0, 30),
                ],
                fill: ogre_core::VectorFill::Solid(Rgba32F::new(0.0, 0.0, 1.0, 1.0)),
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
        let vid = doc.add_vector_layer("Rectangle", data);
        doc.active = Some(vid);

        let mut tool = ShapeTool::new(ShapeKind::Rect);
        // Sidebar is in Pixels mode, but a compatible vector layer loads.
        tool.mode = VectorCommitMode::Pixels;
        assert!(tool.load_active_vector_layer(&doc));
        assert_eq!(tool.editing_layer, Some(vid));

        // Drag a new rect and release: must edit `vid` in place despite Pixels mode.
        let _ = tool.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(0, 0), Phase::Down, egui::Modifiers::NONE),
        );
        let _ = tool.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(20, 20), Phase::Drag, egui::Modifiers::NONE),
        );
        let mut cmd = tool
            .on_pointer(
                &doc,
                PointerEvent::new(glam::IVec2::new(20, 20), Phase::Up, egui::Modifiers::NONE),
            )
            .expect("drag should commit");
        assert_eq!(
            cmd.label(),
            "Edit vector",
            "editing_layer must be authoritative over Pixels mode"
        );
        let mut doc = doc;
        cmd.apply(&mut doc).unwrap();
        assert_eq!(doc.order.len(), 2, "edited in place, no new layer");
    }
}
