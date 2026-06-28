// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Polygon and freehand lasso selection tools.
//!
//! * **Polygon lasso** places a vertex on each primary click and forms a *live*
//!   selection (≥3 vertices) that the canvas commits as it is built; a
//!   double-click, Enter, or a click near the first vertex ends the path.
//! * **Freehand lasso** samples the pointer path while dragging and closes the
//!   path on release.
//!
//! Both produce a coverage mask and respect the Shift/Alt modifier modes.

use egui::{Pos2, Stroke};
use glam::IVec2;

use crate::panels::canvas::doc_to_screen;
use crate::tools::{Phase, PointerEvent, Tool};

/// A click within this many document pixels of the first polygon vertex closes
/// the path.
const CLOSE_THRESHOLD_PX: i32 = 5;

/// Minimum squared distance between successive freehand samples.
const FREEHAND_SAMPLE_DIST_SQ: i32 = 4;

/// Minimum number of vertices for a closed lasso to produce a selection.
const MIN_LASSO_POINTS: usize = 3;

/// Maximum vertices retained for a single freehand lasso stroke.
const MAX_FREEHAND_POINTS: usize = 10_000;

/// Maximum vertices retained for a polygon lasso path.
const MAX_POLYGON_POINTS: usize = 10_000;

/// Polygon lasso: click vertices to build a closed selection.
#[derive(Debug, Default)]
pub struct PolygonLassoTool {
    points: Vec<IVec2>,
}

impl PolygonLassoTool {
    /// Create a new polygon-lasso tool with no active path.
    pub fn new() -> Self {
        Self::default()
    }

    /// The selection the current vertices form, once there are enough of them.
    ///
    /// The canvas panel reads this after each placed vertex and commits it as a
    /// *live*, coalesced selection — so the shape becomes the selection (with
    /// marching ants) as you click, with no separate "close" step.
    pub fn live_selection(&self) -> Option<ogre_core::Selection> {
        if self.points.len() < MIN_LASSO_POINTS {
            return None;
        }
        let sel = ogre_core::Selection::polygon(&self.points);
        (!sel.is_empty()).then_some(sel)
    }
}

impl Tool for PolygonLassoTool {
    fn on_pointer(
        &mut self,
        _doc: &ogre_core::Document,
        ev: PointerEvent,
    ) -> Option<Box<dyn ogre_core::Command>> {
        // The selection is driven live by the canvas via `live_selection`, so a
        // click only manages vertices here; it never returns a command. A click
        // back near the first vertex ends the path (closes it).
        if ev.phase == Phase::Down {
            if self.points.is_empty() {
                self.points.push(ev.doc_pos);
            } else if let Some(&first) = self.points.first() {
                let near_start = (ev.doc_pos - first).length_squared()
                    <= CLOSE_THRESHOLD_PX * CLOSE_THRESHOLD_PX;
                if near_start && self.points.len() >= MIN_LASSO_POINTS {
                    self.points.clear();
                } else if self.points.len() < MAX_POLYGON_POINTS {
                    self.points.push(ev.doc_pos);
                }
            }
        }
        None
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
        draw_polyline(
            painter,
            &self.points,
            viewport,
            canvas_rect,
            pixels_per_point,
            true,
        );
    }

    /// Finalize the path on Enter (the live selection is already committed).
    fn commit(&mut self, _doc: &ogre_core::Document) -> Option<Box<dyn ogre_core::Command>> {
        self.points.clear();
        None
    }

    fn cancel(&mut self) {
        self.points.clear();
    }
}

/// Freehand lasso: drag to draw a closed path.
#[derive(Debug, Default)]
pub struct FreehandLassoTool {
    points: Vec<IVec2>,
    drawing: bool,
}

impl FreehandLassoTool {
    /// Create a new freehand-lasso tool with no active path.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Tool for FreehandLassoTool {
    fn on_pointer(
        &mut self,
        _doc: &ogre_core::Document,
        ev: PointerEvent,
    ) -> Option<Box<dyn ogre_core::Command>> {
        match ev.phase {
            Phase::Down => {
                self.drawing = true;
                self.points.clear();
                self.points.push(ev.doc_pos);
                None
            }
            Phase::Drag => {
                if !self.drawing {
                    return None;
                }
                if self.points.len() < MAX_FREEHAND_POINTS {
                    if let Some(&last) = self.points.last() {
                        if (ev.doc_pos - last).length_squared() >= FREEHAND_SAMPLE_DIST_SQ {
                            self.points.push(ev.doc_pos);
                        }
                    }
                }
                None
            }
            Phase::Up => {
                if !self.drawing {
                    return None;
                }
                self.drawing = false;
                if self.points.len() < MIN_LASSO_POINTS {
                    self.points.clear();
                    return None;
                }
                let selection = ogre_core::Selection::polygon(&self.points);
                self.points.clear();
                if selection.is_empty() {
                    return None;
                }
                Some(Box::new(ogre_core::SetSelectionCmd::with_mode(
                    selection,
                    crate::tools::selection_mode(ev.modifiers),
                )) as Box<dyn ogre_core::Command>)
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
        draw_polyline(
            painter,
            &self.points,
            viewport,
            canvas_rect,
            pixels_per_point,
            false,
        );
    }

    fn cancel(&mut self) {
        self.drawing = false;
        self.points.clear();
    }
}

fn draw_polyline(
    painter: &egui::Painter,
    points: &[IVec2],
    viewport: &ogre_gpu::Viewport,
    canvas_rect: egui::Rect,
    pixels_per_point: f32,
    closing_line: bool,
) {
    if points.len() < 2 {
        return;
    }
    let stroke = Stroke::new(1.0, egui::Color32::WHITE);
    let screen_points: Vec<Pos2> = points
        .iter()
        .map(|p| {
            let s = doc_to_screen(*p, viewport, pixels_per_point)
                + glam::Vec2::new(canvas_rect.min.x, canvas_rect.min.y);
            Pos2::new(s.x, s.y)
        })
        .collect();
    for window in screen_points.windows(2) {
        painter.line_segment([window[0], window[1]], stroke);
    }
    if closing_line {
        painter.line_segment(
            [
                *screen_points.last().unwrap(),
                *screen_points.first().unwrap(),
            ],
            stroke,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;
    use crate::tools::Phase;

    fn polygon_event(
        tool: &mut PolygonLassoTool,
        doc: &ogre_core::Document,
        pos: IVec2,
        phase: Phase,
    ) -> Option<Box<dyn ogre_core::Command>> {
        tool.on_pointer(doc, PointerEvent::new(pos, phase, egui::Modifiers::NONE))
    }

    fn freehand_event(
        tool: &mut FreehandLassoTool,
        doc: &ogre_core::Document,
        pos: IVec2,
        phase: Phase,
    ) -> Option<Box<dyn ogre_core::Command>> {
        tool.on_pointer(doc, PointerEvent::new(pos, phase, egui::Modifiers::NONE))
    }

    #[test]
    fn polygon_lasso_forms_live_selection_after_three_points() {
        let state = AppState::new_document(2000, 1500);
        let doc = &state.tabs[state.active_tab].doc;
        let mut tool = PolygonLassoTool::new();

        // Vertices place via Down; on_pointer never returns a command (the canvas
        // drives the live selection via live_selection()).
        for p in [IVec2::new(0, 0), IVec2::new(10, 0)] {
            assert!(polygon_event(&mut tool, doc, p, Phase::Down).is_none());
            assert!(tool.live_selection().is_none(), "needs >= 3 vertices");
        }
        assert!(polygon_event(&mut tool, doc, IVec2::new(10, 10), Phase::Down).is_none());

        // Three vertices → a live triangle selection.
        let sel = tool.live_selection().expect("triangle selection");
        assert!(sel.coverage_at(IVec2::new(7, 3)) > 0.0);

        // Clicking back near the first vertex ends the path.
        polygon_event(&mut tool, doc, IVec2::new(1, 1), Phase::Down);
        assert!(tool.live_selection().is_none(), "path ended on close");
    }

    #[test]
    fn polygon_lasso_close_is_one_coalesced_undo_entry() {
        // Building a polygon vertex-by-vertex must collapse to a single undo
        // entry via coalesce_selection, restoring the pre-polygon selection.
        let mut state = AppState::new_document(200, 200);
        let pts = [
            IVec2::new(20, 20),
            IVec2::new(80, 20),
            IVec2::new(80, 80),
            IVec2::new(20, 80),
        ];
        for i in 0..pts.len() {
            let sel = ogre_core::Selection::polygon(&pts[..=i]);
            if i + 1 < 3 {
                continue; // < 3 vertices: no live selection yet
            }
            if i == 2 {
                crate::dispatch::dispatch(
                    &mut state,
                    Box::new(ogre_core::SetSelectionCmd::new(sel.clone())),
                )
                .unwrap();
            } else {
                let tab = state.current_tab_mut();
                let top = tab.history.undo_top_mut().unwrap();
                assert!(top.coalesce_selection(&mut tab.doc, &sel));
            }
        }
        assert_eq!(state.history().undo_len(), 1, "one entry for the polygon");
        assert!(state.doc().selection.coverage_at(IVec2::new(50, 50)) > 0.0);
        // Undo restores no-selection (the pre-polygon state).
        crate::dispatch::undo(&mut state);
        assert!(state.doc().selection.is_empty());
    }

    #[test]
    fn freehand_lasso_samples_drag_and_selects() {
        let mut state = AppState::new_document(2000, 1500);
        let mut tool = FreehandLassoTool::new();

        assert!(freehand_event(
            &mut tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(0, 0),
            Phase::Down
        )
        .is_none());
        assert!(freehand_event(
            &mut tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(10, 0),
            Phase::Drag
        )
        .is_none());
        assert!(freehand_event(
            &mut tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(10, 10),
            Phase::Drag
        )
        .is_none());
        assert!(freehand_event(
            &mut tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(0, 10),
            Phase::Drag
        )
        .is_none());
        let cmd = freehand_event(
            &mut tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(0, 0),
            Phase::Up,
        );
        assert!(cmd.is_some());
        crate::dispatch::dispatch(&mut state, cmd.unwrap()).unwrap();

        assert_eq!(state.history().undo_len(), 1);
        assert!(state.doc().selection.coverage_at(IVec2::new(5, 5)) > 0.0);
    }

    #[test]
    fn freehand_lasso_short_drag_produces_no_command() {
        let state = AppState::new_document(100, 100);
        let mut tool = FreehandLassoTool::new();
        assert!(freehand_event(
            &mut tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(0, 0),
            Phase::Down
        )
        .is_none());
        let cmd = freehand_event(
            &mut tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(1, 0),
            Phase::Up,
        );
        assert!(cmd.is_none());
    }

    #[test]
    fn lasso_respects_shift_add_mode() {
        let mut state = AppState::new_document(2000, 1500);
        let mut tool = FreehandLassoTool::new();

        let points = [
            IVec2::new(0, 0),
            IVec2::new(10, 0),
            IVec2::new(10, 10),
            IVec2::new(0, 10),
        ];
        for (i, p) in points.iter().enumerate() {
            let phase = if i == 0 { Phase::Down } else { Phase::Drag };
            freehand_event(&mut tool, &state.tabs[state.active_tab].doc, *p, phase);
        }
        let cmd = tool.on_pointer(
            &state.tabs[state.active_tab].doc,
            PointerEvent::new(IVec2::new(0, 0), Phase::Up, egui::Modifiers::NONE),
        );
        crate::dispatch::dispatch(&mut state, cmd.unwrap()).unwrap();

        // Shift-add a second rectangle to the right.
        tool.cancel();
        let points2 = [
            IVec2::new(20, 0),
            IVec2::new(30, 0),
            IVec2::new(30, 10),
            IVec2::new(20, 10),
        ];
        for (i, p) in points2.iter().enumerate() {
            let phase = if i == 0 { Phase::Down } else { Phase::Drag };
            freehand_event(&mut tool, &state.tabs[state.active_tab].doc, *p, phase);
        }
        let cmd = tool.on_pointer(
            &state.tabs[state.active_tab].doc,
            PointerEvent::new(IVec2::new(20, 0), Phase::Up, egui::Modifiers::SHIFT),
        );
        crate::dispatch::dispatch(&mut state, cmd.unwrap()).unwrap();

        assert!(state.doc().selection.coverage_at(IVec2::new(5, 5)) > 0.0);
        assert!(state.doc().selection.coverage_at(IVec2::new(25, 5)) > 0.0);
    }

    #[test]
    fn freehand_lasso_respects_point_budget() {
        let mut state = AppState::new_document(2000, 1500);
        let mut tool = FreehandLassoTool::new();

        assert!(freehand_event(
            &mut tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(0, 0),
            Phase::Down
        )
        .is_none());

        // Leg 1: along the x-axis to (12000, 0).
        for i in 1..=4_000 {
            assert!(
                tool.points.len() <= MAX_FREEHAND_POINTS,
                "freehand budget exceeded on leg 1 at sample {}",
                i
            );
            freehand_event(
                &mut tool,
                &state.tabs[state.active_tab].doc,
                IVec2::new(i * 3, 0),
                Phase::Drag,
            );
        }
        // Leg 2: to (0, 12000).
        for i in 1..=4_000 {
            assert!(
                tool.points.len() <= MAX_FREEHAND_POINTS,
                "freehand budget exceeded on leg 2 at sample {}",
                i
            );
            freehand_event(
                &mut tool,
                &state.tabs[state.active_tab].doc,
                IVec2::new(12_000 - i * 3, i * 3),
                Phase::Drag,
            );
        }
        // Leg 3: back toward the origin.
        for i in 1..=4_000 {
            assert!(
                tool.points.len() <= MAX_FREEHAND_POINTS,
                "freehand budget exceeded on leg 3 at sample {}",
                i
            );
            freehand_event(
                &mut tool,
                &state.tabs[state.active_tab].doc,
                IVec2::new(0, 12_000 - i * 3),
                Phase::Drag,
            );
        }
        assert!(tool.points.len() <= MAX_FREEHAND_POINTS);

        let cmd = freehand_event(
            &mut tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(0, 0),
            Phase::Up,
        );
        assert!(
            cmd.is_some(),
            "closed freehand selection should still commit after budget is reached"
        );
        crate::dispatch::dispatch(&mut state, cmd.unwrap()).unwrap();
        assert_eq!(state.history().undo_len(), 1);
    }

    #[test]
    fn polygon_lasso_respects_point_budget() {
        let state = AppState::new_document(2000, 1500);
        let mut tool = PolygonLassoTool::new();

        // Start at the centre of a large circle so every vertex is far enough
        // from the first point to avoid auto-closing.
        let centre = IVec2::new(750, 750);
        let radius = 700;
        assert!(polygon_event(
            &mut tool,
            &state.tabs[state.active_tab].doc,
            centre,
            Phase::Down
        )
        .is_none());

        for i in 1..=12_000 {
            assert!(
                tool.points.len() <= MAX_POLYGON_POINTS,
                "polygon budget exceeded at vertex {}",
                i
            );
            let angle = 2.0 * std::f64::consts::PI * (i as f64) / 12_000.0;
            let x = (centre.x as f64 + radius as f64 * angle.cos()).round() as i32;
            let y = (centre.y as f64 + radius as f64 * angle.sin()).round() as i32;
            polygon_event(
                &mut tool,
                &state.tabs[state.active_tab].doc,
                IVec2::new(x, y),
                Phase::Down,
            );
        }
        assert!(tool.points.len() <= MAX_POLYGON_POINTS);

        // Even capped at the budget, the vertices still form a live selection.
        assert!(
            tool.live_selection().is_some(),
            "polygon should still form a selection after budget is reached"
        );
    }
}
