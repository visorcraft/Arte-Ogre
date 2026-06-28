// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Path Select and Direct Select tools (§3.3.4) for non-destructive vector
//! layers. Path Select moves an entire path; Direct Select drags one anchor.
//! Both dispatch [`EditVectorCmd`] on release.

use glam::Vec2;
use ogre_core::{EditVectorCmd, VectorData};

use crate::tools::{Phase, PointerEvent, Tool};

/// Click within this many document pixels of a vertex selects it.
const VERTEX_HIT_RADIUS_PX: i32 = 8;

/// Path Select: click on a vector layer and drag to translate all paths.
#[derive(Debug, Default)]
pub struct PathSelectTool {
    layer: Option<ogre_core::LayerId>,
    start: Option<glam::IVec2>,
    current: glam::IVec2,
    original_data: Option<VectorData>,
}

impl PathSelectTool {
    /// Create a new Path Select tool with no active drag.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Tool for PathSelectTool {
    fn on_pointer(
        &mut self,
        doc: &ogre_core::Document,
        ev: PointerEvent,
    ) -> Option<Box<dyn ogre_core::Command>> {
        match ev.phase {
            Phase::Down => {
                let id = doc.active?;
                let layer = doc.layer(id).ok()?;
                if !layer.is_vector() {
                    return None;
                }
                self.layer = Some(id);
                self.start = Some(ev.doc_pos);
                self.current = ev.doc_pos;
                self.original_data = layer.vector_data().cloned();
                None
            }
            Phase::Drag => {
                self.current = ev.doc_pos;
                None
            }
            Phase::Up => {
                let layer = self.layer.take()?;
                let start = self.start.take()?;
                let original = self.original_data.take()?;
                let delta = ev.doc_pos - start;
                if delta == glam::IVec2::ZERO {
                    return None;
                }
                // Translate all path vertices by the delta (in layer-local).
                let doc_layer = doc.layer(layer).ok()?;
                let offset = doc_layer.offset;
                let local_delta = glam::IVec2::new(delta.x, delta.y);
                let mut new_data = original.clone();
                for path in &mut new_data.paths {
                    for v in &mut path.vertices {
                        *v += local_delta;
                    }
                }
                new_data.mark_dirty();
                let _ = offset; // vertices are already in layer-local space
                if original.svg_source.is_some() {
                    Some(Box::new(crate::tools::vector_commit::EditSvgVectorCmd::new(
                        layer, original, new_data,
                    )) as Box<dyn ogre_core::Command>)
                } else {
                    Some(Box::new(EditVectorCmd::new(layer, new_data))
                        as Box<dyn ogre_core::Command>)
                }
            }
        }
    }

    fn draw_overlay(
        &self,
        _painter: &egui::Painter,
        _doc: &ogre_core::Document,
        _viewport: &ogre_gpu::Viewport,
        _canvas_rect: egui::Rect,
        _pixels_per_point: f32,
        _time: f64,
    ) {
    }

    fn cancel(&mut self) {
        self.layer = None;
        self.start = None;
        self.original_data = None;
    }
}

/// Direct Select: click near a vertex and drag to move just that vertex.
#[derive(Debug, Default)]
pub struct DirectSelectTool {
    layer: Option<ogre_core::LayerId>,
    /// Which (path_index, vertex_index) is being dragged.
    selected: Option<(usize, usize)>,
    start: Option<glam::IVec2>,
    original_data: Option<VectorData>,
}

impl DirectSelectTool {
    /// Create a new Direct Select tool with no active selection.
    pub fn new() -> Self {
        Self::default()
    }

    /// Find the closest vertex to `doc_pos` within the hit radius, returning
    /// `(path_index, vertex_index)`.
    fn hit_test(
        data: &VectorData,
        doc_pos: glam::IVec2,
        offset: glam::IVec2,
    ) -> Option<(usize, usize)> {
        let mut best: Option<(usize, usize, i32)> = None;
        for (pi, path) in data.paths.iter().enumerate() {
            for (vi, &v) in path.vertices.iter().enumerate() {
                let doc_v = v + offset;
                let d2 = (doc_v - doc_pos).length_squared();
                if d2 <= VERTEX_HIT_RADIUS_PX * VERTEX_HIT_RADIUS_PX
                    && best.map(|(_, _, bd2)| d2 < bd2).unwrap_or(true)
                {
                    best = Some((pi, vi, d2));
                }
            }
        }
        best.map(|(pi, vi, _)| (pi, vi))
    }
}

impl Tool for DirectSelectTool {
    fn on_pointer(
        &mut self,
        doc: &ogre_core::Document,
        ev: PointerEvent,
    ) -> Option<Box<dyn ogre_core::Command>> {
        match ev.phase {
            Phase::Down => {
                let id = doc.active?;
                let layer = doc.layer(id).ok()?;
                if !layer.is_vector() {
                    return None;
                }
                let data = layer.vector_data()?;
                let offset = layer.offset;
                let hit = Self::hit_test(data, ev.doc_pos, offset);
                hit?;
                self.layer = Some(id);
                self.selected = hit;
                self.start = Some(ev.doc_pos);
                self.original_data = layer.vector_data().cloned();
                None
            }
            Phase::Drag => None,
            Phase::Up => {
                let layer = self.layer.take()?;
                let (pi, vi) = self.selected.take()?;
                let start = self.start.take()?;
                let original = self.original_data.take()?;
                let delta = ev.doc_pos - start;
                if delta == glam::IVec2::ZERO {
                    return None;
                }
                let mut new_data = original.clone();
                if let Some(path) = new_data.paths.get_mut(pi) {
                    if let Some(vertex) = path.vertices.get_mut(vi) {
                        *vertex += delta;
                    }
                }
                new_data.mark_dirty();
                if original.svg_source.is_some() {
                    Some(Box::new(crate::tools::vector_commit::EditSvgVectorCmd::new(
                        layer, original, new_data,
                    )) as Box<dyn ogre_core::Command>)
                } else {
                    Some(Box::new(EditVectorCmd::new(layer, new_data))
                        as Box<dyn ogre_core::Command>)
                }
            }
        }
    }

    fn draw_overlay(
        &self,
        painter: &egui::Painter,
        doc: &ogre_core::Document,
        viewport: &ogre_gpu::Viewport,
        canvas_rect: egui::Rect,
        pixels_per_point: f32,
        _time: f64,
    ) {
        let id = match doc.active {
            Some(id) => id,
            None => return,
        };
        let Ok(layer) = doc.layer(id) else {
            return;
        };
        let Some(data) = layer.vector_data() else {
            return;
        };
        let to_screen = |p: glam::IVec2| -> egui::Pos2 {
            let v = (p.as_vec2() - viewport.pan) * viewport.zoom / pixels_per_point
                + Vec2::new(canvas_rect.min.x, canvas_rect.min.y);
            egui::Pos2::new(v.x, v.y)
        };
        // Draw all vertices as small dots; highlight the selected one.
        for (pi, path) in data.paths.iter().enumerate() {
            for (vi, &v) in path.vertices.iter().enumerate() {
                let doc_v = v + layer.offset;
                let pos = to_screen(doc_v);
                let is_selected = self.selected == Some((pi, vi));
                let color = if is_selected {
                    egui::Color32::YELLOW
                } else {
                    egui::Color32::from_gray(180)
                };
                let r = if is_selected { 4.0 } else { 2.5 };
                painter.circle_filled(pos, r, color);
            }
        }
    }

    fn cancel(&mut self) {
        self.layer = None;
        self.selected = None;
        self.start = None;
        self.original_data = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ogre_core::{IVec2, Rgba32F, VectorPath};

    fn doc_with_vector() -> ogre_core::Document {
        let mut doc = ogre_core::Document::new(100, 100);
        let mut data = VectorData::new();
        data.paths.push(VectorPath {
            vertices: vec![IVec2::new(10, 10), IVec2::new(20, 10), IVec2::new(15, 20)],
            fill: ogre_core::VectorFill::Solid(Rgba32F::new(1.0, 0.0, 0.0, 1.0)),
            stroke: ogre_core::VectorStroke {
                color: Rgba32F::TRANSPARENT,
                width: 0.0,
                dash: Vec::new(),
                cap: ogre_core::StrokeCap::Butt,
                join: ogre_core::StrokeJoin::Miter,
            },
            closed: true,
        });
        let id = doc.add_vector_layer("Vec", data);
        doc.active = Some(id);
        doc
    }

    #[test]
    fn path_select_drag_produces_command() {
        let doc = doc_with_vector();
        let mut tool = PathSelectTool::new();
        tool.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(15, 15), Phase::Down, egui::Modifiers::NONE),
        );
        let cmd = tool.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(25, 25), Phase::Up, egui::Modifiers::NONE),
        );
        assert!(cmd.is_some(), "drag should produce an EditVectorCmd");
    }

    #[test]
    fn path_select_zero_delta_no_command() {
        let doc = doc_with_vector();
        let mut tool = PathSelectTool::new();
        tool.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(15, 15), Phase::Down, egui::Modifiers::NONE),
        );
        let cmd = tool.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(15, 15), Phase::Up, egui::Modifiers::NONE),
        );
        assert!(cmd.is_none());
    }

    #[test]
    fn direct_select_hit_test_finds_vertex() {
        let doc = doc_with_vector();
        let layer = doc.layer(doc.active.unwrap()).unwrap();
        let data = layer.vector_data().unwrap();
        let hit = DirectSelectTool::hit_test(data, glam::IVec2::new(11, 11), glam::IVec2::ZERO);
        assert_eq!(hit, Some((0, 0)));
        let miss = DirectSelectTool::hit_test(data, glam::IVec2::new(90, 90), glam::IVec2::ZERO);
        assert!(miss.is_none());
    }

    #[test]
    fn direct_select_drag_vertex_produces_command() {
        let doc = doc_with_vector();
        let mut tool = DirectSelectTool::new();
        tool.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(10, 10), Phase::Down, egui::Modifiers::NONE),
        );
        assert!(tool.selected.is_some());
        let cmd = tool.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(15, 12), Phase::Up, egui::Modifiers::NONE),
        );
        assert!(cmd.is_some());
    }

    #[test]
    fn direct_select_empty_click_no_command() {
        let doc = doc_with_vector();
        let mut tool = DirectSelectTool::new();
        tool.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(90, 90), Phase::Down, egui::Modifiers::NONE),
        );
        assert!(tool.selected.is_none());
        assert!(tool.layer.is_none());
    }
}
