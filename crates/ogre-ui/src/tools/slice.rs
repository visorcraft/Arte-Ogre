// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Slice tool (§3.5.1): draw named rectangular slice regions stored as document
//! metadata for web export. Slices are metadata — they do not modify pixels.
//! A drag creates a slice; Enter commits it; Escape cancels.

use glam::Vec2;
use ogre_core::SliceRect;

use crate::tools::{Phase, PointerEvent, Tool};

/// A command that adds a slice rect to the document.
#[derive(Debug)]
pub struct AddSliceCmd {
    slice: SliceRect,
    applied: bool,
}

impl AddSliceCmd {
    /// Create a command that appends `slice` to the document.
    pub fn new(slice: SliceRect) -> Self {
        Self {
            slice,
            applied: false,
        }
    }
}

impl ogre_core::Command for AddSliceCmd {
    fn label(&self) -> &'static str {
        "Add slice"
    }

    fn apply(&mut self, doc: &mut ogre_core::Document) -> ogre_core::Result<()> {
        if !self.applied {
            doc.slices.push(self.slice.clone());
            self.applied = true;
        }
        Ok(())
    }

    fn undo(&mut self, doc: &mut ogre_core::Document) {
        if self.applied {
            doc.slices.pop();
            self.applied = false;
        }
    }
}

/// Slice tool: drag to define a named slice rect.
#[derive(Debug, Default)]
pub struct SliceTool {
    start: Option<glam::IVec2>,
    current: Option<glam::IVec2>,
    slice_count: usize,
}

impl SliceTool {
    /// Create a new Slice tool with no active drag.
    pub fn new() -> Self {
        Self {
            start: None,
            current: None,
            slice_count: 0,
        }
    }
}

impl Tool for SliceTool {
    fn on_pointer(
        &mut self,
        doc: &ogre_core::Document,
        ev: PointerEvent,
    ) -> Option<Box<dyn ogre_core::Command>> {
        match ev.phase {
            Phase::Down => {
                self.start = Some(ev.doc_pos);
                self.current = Some(ev.doc_pos);
                None
            }
            Phase::Drag => {
                self.current = Some(ev.doc_pos);
                None
            }
            Phase::Up => {
                let start = self.start.take()?;
                let end = self.current.take()?;
                if (end - start).as_vec2().length() < 2.0 {
                    return None;
                }
                let x = start.x.min(end.x);
                let y = start.y.min(end.y);
                let w = (start.x.max(end.x) - x) as u32;
                let h = (start.y.max(end.y) - y) as u32;
                if w == 0 || h == 0 {
                    return None;
                }
                self.slice_count += 1;
                let rect = ogre_core::Rect::new(x, y, w, h);
                let name = format!("slice_{}", self.slice_count);
                let _ = doc; // no document mutation needed
                Some(Box::new(AddSliceCmd::new(SliceRect { name, rect }))
                    as Box<dyn ogre_core::Command>)
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
        let to_screen = |p: glam::IVec2| -> egui::Pos2 {
            let v = (p.as_vec2() - viewport.pan) * viewport.zoom / pixels_per_point
                + Vec2::new(canvas_rect.min.x, canvas_rect.min.y);
            egui::Pos2::new(v.x, v.y)
        };
        let stroke = egui::Stroke::new(1.5, egui::Color32::from_rgb(100, 200, 255));
        // Draw existing slices.
        for slice in &doc.slices {
            let a = to_screen(glam::IVec2::new(slice.rect.x, slice.rect.y));
            let b = to_screen(glam::IVec2::new(
                slice.rect.x + slice.rect.w as i32,
                slice.rect.y + slice.rect.h as i32,
            ));
            painter.rect_stroke(
                egui::Rect::from_min_max(a, b),
                egui::CornerRadius::ZERO,
                stroke,
                egui::StrokeKind::Middle,
            );
        }
        // Draw the in-progress slice.
        if let (Some(s), Some(c)) = (self.start, self.current) {
            let a = to_screen(glam::IVec2::new(s.x.min(c.x), s.y.min(c.y)));
            let b = to_screen(glam::IVec2::new(s.x.max(c.x), s.y.max(c.y)));
            painter.rect_stroke(
                egui::Rect::from_min_max(a, b),
                egui::CornerRadius::ZERO,
                egui::Stroke::new(1.5, egui::Color32::from_rgb(150, 220, 255)),
                egui::StrokeKind::Middle,
            );
        }
    }

    fn cancel(&mut self) {
        self.start = None;
        self.current = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ogre_core::Command;

    #[test]
    fn slice_tool_drag_creates_command() {
        let doc = ogre_core::Document::new(100, 100);
        let mut tool = SliceTool::new();
        tool.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(10, 10), Phase::Down, egui::Modifiers::NONE),
        );
        tool.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(50, 60), Phase::Drag, egui::Modifiers::NONE),
        );
        let cmd = tool.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(50, 60), Phase::Up, egui::Modifiers::NONE),
        );
        assert!(cmd.is_some());
    }

    #[test]
    fn slice_tool_click_no_command() {
        let doc = ogre_core::Document::new(100, 100);
        let mut tool = SliceTool::new();
        tool.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(10, 10), Phase::Down, egui::Modifiers::NONE),
        );
        let cmd = tool.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(10, 10), Phase::Up, egui::Modifiers::NONE),
        );
        assert!(cmd.is_none());
    }

    #[test]
    fn add_slice_cmd_round_trips() {
        let mut doc = ogre_core::Document::new(100, 100);
        assert!(doc.slices.is_empty());
        let mut cmd = AddSliceCmd::new(SliceRect {
            name: "test".into(),
            rect: ogre_core::Rect::new(0, 0, 10, 10),
        });
        cmd.apply(&mut doc).unwrap();
        assert_eq!(doc.slices.len(), 1);
        cmd.undo(&mut doc);
        assert!(doc.slices.is_empty());
    }
}
