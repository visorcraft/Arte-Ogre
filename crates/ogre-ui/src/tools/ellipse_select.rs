// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Elliptical selection tool.
//!
//! A primary-button drag defines the bounding box of an axis-aligned ellipse.
//! The resulting selection is a coverage mask with anti-aliased edges. Modifier
//! keys combine the new ellipse with the existing selection.

use egui::{Pos2, Stroke};
use glam::IVec2;

use crate::panels::canvas::doc_to_screen;
use crate::tools::{Phase, PointerEvent, Tool};

/// Number of line segments used to approximate the ellipse outline in the
/// live preview.
const ELLIPSE_SEGMENTS: usize = 64;

/// Ellipse Select state machine.
#[derive(Debug, Default)]
pub struct EllipseSelectTool {
    anchor: Option<IVec2>,
    current: IVec2,
}

impl EllipseSelectTool {
    /// Create a new ellipse-select tool with no active drag.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build the bounding rectangle of the active drag, normalised so dragging
    /// in any direction works.
    fn drag_rect(&self) -> Option<ogre_core::Rect> {
        self.anchor
            .map(|a| super::rect_select::half_open_rect(a, self.current))
    }
}

impl Tool for EllipseSelectTool {
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
                let mode = crate::tools::selection_mode(ev.modifiers);
                let cmd = self.anchor.and_then(|anchor| {
                    let rect = super::rect_select::half_open_rect(anchor, ev.doc_pos);
                    if rect.is_empty() && mode != ogre_core::SelectionMode::Replace {
                        return None;
                    }
                    Some(Box::new(ogre_core::SetSelectionCmd::with_mode(
                        ogre_core::Selection::ellipse(rect),
                        mode,
                    )) as Box<dyn ogre_core::Command>)
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
        canvas_rect: egui::Rect,
        pixels_per_point: f32,
        time: f64,
    ) {
        // Only the active drag preview; the committed selection outline is drawn
        // centrally by the canvas panel.
        if let Some(rect) = self.drag_rect() {
            draw_ellipse_outline(painter, rect, viewport, canvas_rect, pixels_per_point, time);
        }
    }

    fn cancel(&mut self) {
        self.anchor = None;
    }

    fn pending_selection_rect(&self) -> Option<ogre_core::Rect> {
        self.drag_rect()
    }
}

fn draw_ellipse_outline(
    painter: &egui::Painter,
    rect: ogre_core::Rect,
    viewport: &ogre_gpu::Viewport,
    canvas_rect: egui::Rect,
    pixels_per_point: f32,
    time: f64,
) {
    let cx = rect.x as f32 + rect.w as f32 / 2.0;
    let cy = rect.y as f32 + rect.h as f32 / 2.0;
    let rx = rect.w as f32 / 2.0;
    let ry = rect.h as f32 / 2.0;
    if rx <= 0.0 || ry <= 0.0 {
        return;
    }

    let offset = ((time as f32) * super::rect_select::ANT_SPEED_PX_PER_SEC)
        % super::rect_select::DASH_PERIOD;
    let stroke = Stroke::new(1.0, egui::Color32::WHITE);

    let mut prev: Option<Pos2> = None;
    let mut first: Option<Pos2> = None;
    for i in 0..=ELLIPSE_SEGMENTS {
        let theta = 2.0 * std::f32::consts::PI * (i as f32) / ELLIPSE_SEGMENTS as f32;
        let doc_p = IVec2::new(
            (cx + rx * theta.cos()).round() as i32,
            (cy + ry * theta.sin()).round() as i32,
        );
        let screen = doc_to_screen(doc_p, viewport, pixels_per_point)
            + glam::Vec2::new(canvas_rect.min.x, canvas_rect.min.y);
        let pos = Pos2::new(screen.x, screen.y);
        if let Some(prev) = prev {
            draw_dashed_line(painter, prev, pos, offset, stroke);
        }
        if i == 0 {
            first = Some(pos);
        }
        prev = Some(pos);
    }
    if let (Some(prev), Some(first)) = (prev, first) {
        draw_dashed_line(painter, prev, first, offset, stroke);
    }
}

fn draw_dashed_line(painter: &egui::Painter, a: Pos2, b: Pos2, offset: f32, stroke: Stroke) {
    let dir = b - a;
    let len = dir.length();
    if len <= 0.0 {
        return;
    }
    let unit = dir / len;
    let mut d = -offset;
    while d < len {
        let seg_start = (a + unit * d.max(0.0)).to_vec2();
        let seg_end = (a + unit * (d + super::rect_select::DASH_PERIOD / 2.0).min(len)).to_vec2();
        if seg_end.x > seg_start.x
            || seg_end.y > seg_start.y
            || (seg_end - seg_start).length() > 0.0
        {
            painter.line_segment(
                [
                    Pos2::new(seg_start.x, seg_start.y),
                    Pos2::new(seg_end.x, seg_end.y),
                ],
                stroke,
            );
        }
        d += super::rect_select::DASH_PERIOD;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;
    use crate::tools::{Phase, PointerEvent};
    use ogre_core::Selection;

    fn send(
        tool: &mut EllipseSelectTool,
        doc: &ogre_core::Document,
        pos: IVec2,
        phase: Phase,
    ) -> Option<Box<dyn ogre_core::Command>> {
        tool.on_pointer(doc, PointerEvent::new(pos, phase, egui::Modifiers::NONE))
    }

    #[test]
    fn ellipse_select_drag_dispatches_ellipse_selection() {
        let mut state = AppState::new_document(2000, 1500);
        let mut tool = EllipseSelectTool::new();
        assert!(send(
            &mut tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(0, 0),
            Phase::Down
        )
        .is_none());
        assert!(send(
            &mut tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(20, 10),
            Phase::Drag
        )
        .is_none());
        let cmd = send(
            &mut tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(20, 10),
            Phase::Up,
        )
        .unwrap();
        crate::dispatch::dispatch(&mut state, cmd).unwrap();

        assert_eq!(state.history().undo_len(), 1);
        // Center is fully inside.
        assert_eq!(state.doc().selection.coverage_at(IVec2::new(10, 5)), 1.0);
        // At least one pixel is on the anti-aliased boundary.
        let has_boundary = (0..10)
            .flat_map(|y| (0..20).map(move |x| IVec2::new(x, y)))
            .any(|p| {
                let c = state.doc().selection.coverage_at(p);
                c > 0.0 && c < 1.0
            });
        assert!(has_boundary);
    }

    #[test]
    fn ellipse_select_zero_area_drag_clears_selection() {
        let mut state = AppState::new_document(100, 100);
        let canvas = state.doc().canvas;
        crate::dispatch::dispatch(
            &mut state,
            Box::new(ogre_core::SetSelectionCmd::new(Selection::select_all(
                canvas,
            ))),
        )
        .unwrap();

        let mut tool = EllipseSelectTool::new();
        send(
            &mut tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(10, 10),
            Phase::Down,
        );
        let cmd = send(
            &mut tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(10, 10),
            Phase::Up,
        )
        .unwrap();
        crate::dispatch::dispatch(&mut state, cmd).unwrap();

        assert!(state.doc().selection.is_empty());
    }

    #[test]
    fn ellipse_select_respects_modifiers() {
        let mut state = AppState::new_document(2000, 1500);
        let mut tool = EllipseSelectTool::new();

        send(
            &mut tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(0, 0),
            Phase::Down,
        );
        let cmd = send(
            &mut tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(20, 10),
            Phase::Up,
        );
        crate::dispatch::dispatch(&mut state, cmd.unwrap()).unwrap();

        send(
            &mut tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(20, 0),
            Phase::Down,
        );
        let cmd = tool.on_pointer(
            &state.tabs[state.active_tab].doc,
            PointerEvent::new(IVec2::new(40, 10), Phase::Up, egui::Modifiers::SHIFT),
        );
        crate::dispatch::dispatch(&mut state, cmd.unwrap()).unwrap();

        assert!(state.doc().selection.coverage_at(IVec2::new(10, 5)) > 0.0);
        assert!(state.doc().selection.coverage_at(IVec2::new(30, 5)) > 0.0);
    }
}
