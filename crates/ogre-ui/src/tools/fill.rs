// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Paint-bucket and eyedropper tools.
//!
//! Both work with the shared foreground color in
//! [`AppState`](crate::state::AppState): the paint bucket fills with it, the
//! eyedropper sets it. The bucket emits a [`PaintBucketCmd`]; the eyedropper
//! only reads the document, so its sampling is handled in the canvas input
//! layer (it edits no pixels and produces no undoable command).

use ogre_core::{Command, PaintBucketCmd, Rgba32F, DEFAULT_FILL_TOLERANCE};

use super::{Phase, PointerEvent, Tool};

/// Flood-fill the active raster layer with the foreground color on click.
#[derive(Debug)]
pub struct PaintBucketTool {
    color: Rgba32F,
    tolerance: f32,
}

impl PaintBucketTool {
    /// Create a paint-bucket tool with the default fill tolerance.
    pub fn new() -> Self {
        Self {
            color: Rgba32F::new(0.0, 0.0, 0.0, 1.0),
            tolerance: DEFAULT_FILL_TOLERANCE,
        }
    }

    /// Set the fill color (driven by the shared foreground color).
    pub fn set_color(&mut self, color: Rgba32F) {
        self.color = color;
    }

    /// The current fill tolerance.
    pub fn tolerance(&self) -> f32 {
        self.tolerance
    }

    /// Mutable access to the fill tolerance (for a settings control).
    pub fn tolerance_mut(&mut self) -> &mut f32 {
        &mut self.tolerance
    }
}

impl Default for PaintBucketTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for PaintBucketTool {
    fn on_pointer(
        &mut self,
        doc: &ogre_core::Document,
        ev: PointerEvent,
    ) -> Option<Box<dyn Command>> {
        // One fill per press, and only for a click inside the canvas so an
        // out-of-bounds click does not push a no-op history entry.
        if ev.phase == Phase::Down && doc.canvas.contains(ev.doc_pos) {
            Some(Box::new(PaintBucketCmd::new(
                ev.doc_pos,
                self.color,
                self.tolerance,
            )))
        } else {
            None
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

    fn cancel(&mut self) {}
}

/// Pick the foreground color from the canvas.
/// Eyedropper sample neighborhood.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EyedropperSample {
    /// Single pixel (the default).
    #[default]
    Point,
    /// 3×3 average.
    Average3x3,
    /// 5×5 average.
    Average5x5,
}

impl EyedropperSample {
    /// Half-extent of the averaging neighborhood (0 for Point).
    pub fn radius(self) -> i32 {
        match self {
            EyedropperSample::Point => 0,
            EyedropperSample::Average3x3 => 1,
            EyedropperSample::Average5x5 => 2,
        }
    }
}

///
/// A no-op [`Tool`] (it produces no command); the actual sampling of the
/// visible composite into the foreground color happens in the canvas input
/// handler, which has access to [`AppState`](crate::state::AppState).
#[derive(Debug, Default)]
pub struct EyedropperTool {
    /// Sample neighborhood for color picking.
    pub sample: EyedropperSample,
}

impl Tool for EyedropperTool {
    fn on_pointer(
        &mut self,
        _doc: &ogre_core::Document,
        _ev: PointerEvent,
    ) -> Option<Box<dyn Command>> {
        None
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

    fn cancel(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use ogre_core::{Document, IVec2};

    fn ev(phase: Phase) -> PointerEvent {
        PointerEvent::new(IVec2::new(3, 3), phase, egui::Modifiers::NONE)
    }

    #[test]
    fn bucket_emits_command_on_press_only() {
        let doc = Document::new(16, 16);
        let mut tool = PaintBucketTool::new();
        assert!(tool.on_pointer(&doc, ev(Phase::Down)).is_some());
        assert!(tool.on_pointer(&doc, ev(Phase::Drag)).is_none());
        assert!(tool.on_pointer(&doc, ev(Phase::Up)).is_none());
    }

    #[test]
    fn bucket_ignores_clicks_outside_the_canvas() {
        let doc = Document::new(16, 16);
        let mut tool = PaintBucketTool::new();
        let outside = PointerEvent::new(IVec2::new(100, 100), Phase::Down, egui::Modifiers::NONE);
        assert!(tool.on_pointer(&doc, outside).is_none());
    }

    #[test]
    fn eyedropper_never_emits_a_command() {
        let doc = Document::new(16, 16);
        let mut tool = EyedropperTool::default();
        assert!(tool.on_pointer(&doc, ev(Phase::Down)).is_none());
        assert!(tool.on_pointer(&doc, ev(Phase::Up)).is_none());
    }
}
