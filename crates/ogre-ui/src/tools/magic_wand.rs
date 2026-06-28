// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Magic Wand and Select-by-Color tools.
//!
//! A primary click on the canvas selects pixels in the active raster layer that
//! are similar to the clicked pixel. The `contiguous` flag switches between a
//! flood-fill selection and a global color-based selection.

use egui::{Pos2, Stroke};
use glam::IVec2;

use crate::panels::canvas::doc_to_screen;
use crate::tools::{Phase, PointerEvent, Tool};

/// Default color-distance tolerance for the magic wand.
const DEFAULT_TOLERANCE: f32 = 0.1;

/// Magic Wand / Select-by-Color tool.
#[derive(Debug)]
pub struct MagicWandTool {
    /// If `true`, only pixels connected to the seed are selected.
    pub contiguous: bool,
    /// Maximum color distance from the seed pixel.
    pub tolerance: f32,
    /// Stored down position so we only act on clean clicks.
    down_pos: Option<IVec2>,
}

impl Default for MagicWandTool {
    fn default() -> Self {
        Self::new()
    }
}

impl MagicWandTool {
    /// Create a magic-wand tool with the default tolerance and contiguous mode.
    pub fn new() -> Self {
        Self {
            contiguous: true,
            tolerance: DEFAULT_TOLERANCE,
            down_pos: None,
        }
    }
}

impl Tool for MagicWandTool {
    fn on_pointer(
        &mut self,
        _doc: &ogre_core::Document,
        ev: PointerEvent,
    ) -> Option<Box<dyn ogre_core::Command>> {
        match ev.phase {
            Phase::Down => {
                self.down_pos = Some(ev.doc_pos);
                None
            }
            Phase::Up => {
                // Seed from the press point, not the release point, so a slightly
                // draggy click still selects (the wand is click-based; small
                // movement between press and release must not cancel it).
                let cmd = self.down_pos.map(|down| {
                    Box::new(ogre_core::MagicWandCmd::with_mode(
                        down,
                        self.tolerance,
                        self.contiguous,
                        crate::tools::selection_mode(ev.modifiers),
                    )) as Box<dyn ogre_core::Command>
                });
                self.down_pos = None;
                cmd
            }
            Phase::Drag => None,
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
        // The committed selection outline is drawn centrally by the canvas panel.
        // Draw a small crosshair at the last click position while it is held.
        if let Some(down) = self.down_pos {
            let screen = doc_to_screen(down, viewport, pixels_per_point)
                + glam::Vec2::new(canvas_rect.min.x, canvas_rect.min.y);
            let center = Pos2::new(screen.x, screen.y);
            let stroke = Stroke::new(1.0, egui::Color32::WHITE);
            painter.line_segment(
                [
                    Pos2::new(center.x - 4.0, center.y),
                    Pos2::new(center.x + 4.0, center.y),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    Pos2::new(center.x, center.y - 4.0),
                    Pos2::new(center.x, center.y + 4.0),
                ],
                stroke,
            );
        }
    }

    fn cancel(&mut self) {
        self.down_pos = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;
    use crate::tools::Phase;
    use ogre_core::Rgba32F;

    fn click(
        tool: &mut MagicWandTool,
        doc: &ogre_core::Document,
        pos: IVec2,
    ) -> Option<Box<dyn ogre_core::Command>> {
        tool.on_pointer(
            doc,
            PointerEvent::new(pos, Phase::Down, egui::Modifiers::NONE),
        );
        tool.on_pointer(
            doc,
            PointerEvent::new(pos, Phase::Up, egui::Modifiers::NONE),
        )
    }

    #[test]
    fn magic_wand_click_selects_connected_region() {
        let mut state = AppState::new_document(2000, 1500);
        let id = state.doc_mut().add_raster_layer("paint");
        state.doc_mut().active = Some(id);

        // Draw two disconnected red squares.
        for y in 0..5 {
            for x in 0..5 {
                state
                    .doc_mut()
                    .layer_mut(id)
                    .unwrap()
                    .buffer_mut()
                    .unwrap()
                    .set_pixel(IVec2::new(x, y), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
            }
        }
        for y in 0..5 {
            for x in 10..15 {
                state
                    .doc_mut()
                    .layer_mut(id)
                    .unwrap()
                    .buffer_mut()
                    .unwrap()
                    .set_pixel(IVec2::new(x, y), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
            }
        }

        let mut tool = MagicWandTool::new();
        let cmd = click(
            &mut tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(2, 2),
        )
        .unwrap();
        crate::dispatch::dispatch(&mut state, cmd).unwrap();

        assert_eq!(state.history().undo_len(), 1);
        assert!(state.doc().selection.coverage_at(IVec2::new(2, 2)) > 0.0);
        assert_eq!(state.doc().selection.coverage_at(IVec2::new(12, 2)), 0.0);
    }

    #[test]
    fn select_by_color_selects_all_matching_pixels() {
        let mut state = AppState::new_document(2000, 1500);
        let id = state.doc_mut().add_raster_layer("paint");
        state.doc_mut().active = Some(id);

        for y in 0..5 {
            for x in 0..5 {
                state
                    .doc_mut()
                    .layer_mut(id)
                    .unwrap()
                    .buffer_mut()
                    .unwrap()
                    .set_pixel(IVec2::new(x, y), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
            }
        }
        for y in 0..5 {
            for x in 10..15 {
                state
                    .doc_mut()
                    .layer_mut(id)
                    .unwrap()
                    .buffer_mut()
                    .unwrap()
                    .set_pixel(IVec2::new(x, y), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
            }
        }

        let mut tool = MagicWandTool::new();
        tool.contiguous = false;
        let cmd = click(
            &mut tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(2, 2),
        )
        .unwrap();
        crate::dispatch::dispatch(&mut state, cmd).unwrap();

        assert!(state.doc().selection.coverage_at(IVec2::new(2, 2)) > 0.0);
        assert!(state.doc().selection.coverage_at(IVec2::new(12, 2)) > 0.0);
    }

    #[test]
    fn magic_wand_no_active_layer_errors() {
        let mut state = AppState::new_document(2000, 1500);
        state.doc_mut().active = None;
        let mut tool = MagicWandTool::new();
        let cmd = click(
            &mut tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(2, 2),
        )
        .unwrap();
        assert!(crate::dispatch::dispatch(&mut state, cmd).is_err());
    }
}
