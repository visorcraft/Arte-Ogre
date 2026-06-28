// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Crop tool.
//!
//! A primary-button drag on the canvas defines the new canvas rectangle.
//! The crop is committed on pointer up; layers keep their pixel data, only the
//! document canvas changes.

use egui::{Rect as EguiRect, Stroke, StrokeKind};
use glam::IVec2;

use crate::tools::{Phase, PointerEvent, Tool};

/// Crop tool state machine.
#[derive(Debug, Default)]
pub struct CropTool {
    /// Anchor corner of the drag, in document pixels.
    anchor: Option<IVec2>,
    /// Current pointer position during the drag, in document pixels.
    current: IVec2,
}

impl CropTool {
    /// Create a new crop tool with no active drag.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build the half-open document rectangle for the active drag.
    fn drag_rect(&self) -> Option<ogre_core::Rect> {
        self.anchor
            .map(|a| crate::tools::rect_select::half_open_rect(a, self.current))
    }
}

impl Tool for CropTool {
    fn on_pointer(
        &mut self,
        _doc: &ogre_core::Document,
        ev: PointerEvent,
    ) -> Option<Box<dyn ogre_core::Command>> {
        match ev.phase {
            Phase::Down => {
                self.anchor = Some(ev.doc_pos);
                self.current = ev.doc_pos;
                None
            }
            Phase::Drag => {
                self.current = ev.doc_pos;
                None
            }
            Phase::Up => {
                let cmd = self.anchor.and_then(|anchor| {
                    let rect = crate::tools::rect_select::half_open_rect(anchor, ev.doc_pos);
                    if rect.is_empty() {
                        None
                    } else {
                        Some(Box::new(ogre_core::CropCmd::new(rect)) as Box<dyn ogre_core::Command>)
                    }
                });
                self.anchor = None;
                cmd
            }
        }
    }

    fn draw_overlay(
        &self,
        painter: &egui::Painter,
        _doc: &ogre_core::Document,
        viewport: &ogre_gpu::Viewport,
        canvas_rect: EguiRect,
        pixels_per_point: f32,
        _time: f64,
    ) {
        if let Some(rect) = self.drag_rect() {
            if let Some(screen) = crate::tools::rect_select::selection_screen_rect(
                rect,
                viewport,
                canvas_rect,
                pixels_per_point,
            ) {
                let stroke = Stroke::new(1.0, egui::Color32::WHITE);
                painter.rect_stroke(screen, 0.0, stroke, StrokeKind::Inside);
            }
        }
    }

    fn cancel(&mut self) {
        self.anchor = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;
    use crate::tools::ToolKind;
    use ogre_core::{IVec2, Rect};

    fn send(
        manager: &mut crate::tools::ToolManager,
        doc: &ogre_core::Document,
        pos: IVec2,
        phase: Phase,
    ) -> Option<Box<dyn ogre_core::Command>> {
        manager.on_pointer(doc, PointerEvent::new(pos, phase, egui::Modifiers::NONE))
    }

    #[test]
    fn crop_tool_drag_commits_crop_cmd() {
        let mut state = AppState::new_document(200, 200);
        state.tool_manager.set_tool(ToolKind::Crop);

        assert!(send(
            &mut state.tool_manager,
            &state.tabs[state.active_tab].doc,
            IVec2::new(10, 20),
            Phase::Down
        )
        .is_none());
        assert!(send(
            &mut state.tool_manager,
            &state.tabs[state.active_tab].doc,
            IVec2::new(110, 90),
            Phase::Drag
        )
        .is_none());
        let cmd = send(
            &mut state.tool_manager,
            &state.tabs[state.active_tab].doc,
            IVec2::new(110, 90),
            Phase::Up,
        )
        .unwrap();
        crate::dispatch::dispatch(&mut state, cmd).unwrap();
        assert_eq!(state.doc().canvas, Rect::new(10, 20, 100, 70));
    }

    #[test]
    fn crop_tool_zero_drag_returns_no_command() {
        let mut state = AppState::new_document(200, 200);
        state.tool_manager.set_tool(ToolKind::Crop);

        send(
            &mut state.tool_manager,
            &state.tabs[state.active_tab].doc,
            IVec2::new(10, 20),
            Phase::Down,
        );
        let cmd = send(
            &mut state.tool_manager,
            &state.tabs[state.active_tab].doc,
            IVec2::new(10, 20),
            Phase::Up,
        );
        assert!(cmd.is_none());
    }

    #[test]
    fn crop_tool_reversed_drag_works() {
        let mut state = AppState::new_document(200, 200);
        state.tool_manager.set_tool(ToolKind::Crop);

        send(
            &mut state.tool_manager,
            &state.tabs[state.active_tab].doc,
            IVec2::new(110, 90),
            Phase::Down,
        );
        send(
            &mut state.tool_manager,
            &state.tabs[state.active_tab].doc,
            IVec2::new(10, 20),
            Phase::Drag,
        );
        let cmd = send(
            &mut state.tool_manager,
            &state.tabs[state.active_tab].doc,
            IVec2::new(10, 20),
            Phase::Up,
        )
        .unwrap();
        crate::dispatch::dispatch(&mut state, cmd).unwrap();
        assert_eq!(state.doc().canvas, Rect::new(10, 20, 100, 70));
    }

    #[test]
    fn crop_tool_cancel_drops_pending() {
        let mut state = AppState::new_document(200, 200);
        state.tool_manager.set_tool(ToolKind::Crop);

        send(
            &mut state.tool_manager,
            &state.tabs[state.active_tab].doc,
            IVec2::new(10, 20),
            Phase::Down,
        );
        send(
            &mut state.tool_manager,
            &state.tabs[state.active_tab].doc,
            IVec2::new(110, 90),
            Phase::Drag,
        );
        // Switching tools cancels the in-progress crop.
        state.tool_manager.set_tool(ToolKind::RectSelect);
        let cmd = send(
            &mut state.tool_manager,
            &state.tabs[state.active_tab].doc,
            IVec2::new(110, 90),
            Phase::Up,
        );
        assert!(cmd.is_none());
        assert_eq!(state.doc().canvas, Rect::new(0, 0, 200, 200));
    }
}
