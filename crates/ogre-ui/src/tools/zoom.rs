// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! The Zoom tool (§3.1.1).
//!
//! Like Hand and Eyedropper, Zoom edits the [`ogre_gpu::Viewport`], never the
//! document, so [`ZoomTool`] is a no-op [`Tool`] whose real work happens in the
//! canvas input layer (`panels::canvas::handle_canvas_input`). Keeping it a real
//! `Tool` makes it uniform with the tool manager / sidebar / keymap.

use super::{PointerEvent, Tool};

/// The Zoom tool. Every `Tool` method is a no-op; the canvas handler performs
/// click-to-zoom (in on Primary, out on Alt+Primary) anchored at the cursor.
#[derive(Debug, Default)]
pub struct ZoomTool;

impl Tool for ZoomTool {
    fn on_pointer(
        &mut self,
        _doc: &ogre_core::Document,
        _ev: PointerEvent,
    ) -> Option<Box<dyn ogre_core::Command>> {
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
    use crate::tools::Phase;
    use glam::IVec2;

    #[test]
    fn zoom_tool_never_emits_a_command() {
        let doc = ogre_core::Document::new(16, 16);
        let mut tool = ZoomTool;
        for phase in [Phase::Down, Phase::Drag, Phase::Up] {
            assert!(tool
                .on_pointer(
                    &doc,
                    PointerEvent::new(IVec2::new(3, 3), phase, egui::Modifiers::NONE)
                )
                .is_none());
        }
    }
}
