// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Rectangle Select tool.
//!
//! A primary-button drag on the canvas creates a rectangular selection.
//! The rectangle is normalized so dragging in any direction works; a zero-area
//! drag clears the selection. While the drag is in progress a dashed
//! marching-ants outline previews the pending selection.

use egui::{Pos2, Rect as EguiRect, Stroke};
use glam::IVec2;

use crate::tools::{Phase, PointerEvent, Tool};

/// Length of one dash plus one gap in screen pixels for the marching-ants outline.
pub(crate) const DASH_PERIOD: f32 = 10.0;

/// Speed of the marching-ants animation in screen pixels per second.
pub(crate) const ANT_SPEED_PX_PER_SEC: f32 = 20.0;

/// Rectangle Select state machine.
#[derive(Debug, Default)]
pub struct RectSelectTool {
    /// Anchor corner of the drag, in document pixels.
    anchor: Option<IVec2>,
    /// Current pointer position during the drag, in document pixels.
    current: IVec2,
}

impl RectSelectTool {
    /// Create a new rectangle-select tool with no active drag.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build the half-open document rectangle for the active drag.
    fn drag_rect(&self) -> Option<ogre_core::Rect> {
        self.anchor.map(|a| half_open_rect(a, self.current))
    }
}

impl Tool for RectSelectTool {
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
                    let selection = if anchor == ev.doc_pos {
                        ogre_core::Selection::none()
                    } else {
                        ogre_core::Selection::rect(half_open_rect(anchor, ev.doc_pos))
                    };
                    // A modifier click without a drag does nothing visible and
                    // should not create a no-op history entry.
                    if selection.is_empty() && mode != ogre_core::SelectionMode::Replace {
                        return None;
                    }
                    Some(
                        Box::new(ogre_core::SetSelectionCmd::with_mode(selection, mode))
                            as Box<dyn ogre_core::Command>,
                    )
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
        time: f64,
    ) {
        // Only the active drag preview; the committed selection outline is drawn
        // centrally by the canvas panel for every tool.
        if let Some(rect) = self.drag_rect() {
            if let Some(screen) =
                selection_screen_rect(rect, viewport, canvas_rect, pixels_per_point)
            {
                draw_marching_ants(painter, screen, time);
            }
        }
    }

    fn cancel(&mut self) {
        self.anchor = None;
    }

    fn pending_selection_rect(&self) -> Option<ogre_core::Rect> {
        self.drag_rect()
    }
}

/// Build a half-open rectangle from two diagonal corners.
///
/// The rectangle includes the minimum corner and excludes the maximum corner,
/// so a drag from `(5,5)` to `(105,85)` yields `Rect::new(5, 5, 100, 80)`.
pub(crate) fn half_open_rect(a: IVec2, b: IVec2) -> ogre_core::Rect {
    let min = a.min(b);
    let max = a.max(b);
    let w = max.x.saturating_sub(min.x).max(0) as u32;
    let h = max.y.saturating_sub(min.y).max(0) as u32;
    ogre_core::Rect::new(min.x, min.y, w, h)
}

/// Map a document-space selection rectangle to an on-screen egui rectangle.
///
/// Returns `None` if the rectangle is empty or degenerate.
pub fn selection_screen_rect(
    rect: ogre_core::Rect,
    viewport: &ogre_gpu::Viewport,
    canvas_rect: EguiRect,
    pixels_per_point: f32,
) -> Option<EguiRect> {
    if rect.is_empty() {
        return None;
    }
    let min = crate::panels::canvas::doc_to_screen(
        IVec2::new(rect.x, rect.y),
        viewport,
        pixels_per_point,
    ) + glam::Vec2::new(canvas_rect.min.x, canvas_rect.min.y);
    let max_x = i32::try_from(rect.right()).unwrap_or(i32::MAX);
    let max_y = i32::try_from(rect.bottom()).unwrap_or(i32::MAX);
    let max =
        crate::panels::canvas::doc_to_screen(IVec2::new(max_x, max_y), viewport, pixels_per_point)
            + glam::Vec2::new(canvas_rect.min.x, canvas_rect.min.y);
    Some(EguiRect::from_min_max(
        Pos2::new(min.x, min.y),
        Pos2::new(max.x, max.y),
    ))
}

pub(crate) fn draw_marching_ants(painter: &egui::Painter, rect: EguiRect, time: f64) {
    let offset = ((time as f32) * ANT_SPEED_PX_PER_SEC) % DASH_PERIOD;
    let color = egui::Color32::WHITE;
    let stroke = Stroke::new(1.0, color);

    draw_dashed_hline(
        painter,
        rect.left()..=rect.right(),
        rect.top(),
        offset,
        stroke,
    );
    draw_dashed_hline(
        painter,
        rect.left()..=rect.right(),
        rect.bottom(),
        offset,
        stroke,
    );
    draw_dashed_vline(
        painter,
        rect.left(),
        rect.top()..=rect.bottom(),
        offset,
        stroke,
    );
    draw_dashed_vline(
        painter,
        rect.right(),
        rect.top()..=rect.bottom(),
        offset,
        stroke,
    );
}

fn draw_dashed_hline(
    painter: &egui::Painter,
    x_range: std::ops::RangeInclusive<f32>,
    y: f32,
    offset: f32,
    stroke: Stroke,
) {
    let start = *x_range.start();
    let end = *x_range.end();
    if end <= start {
        return;
    }
    let mut x = start - offset;
    while x < end {
        let dash_start = x.max(start);
        let dash_end = (x + DASH_PERIOD / 2.0).min(end);
        if dash_end > dash_start {
            painter.line_segment([Pos2::new(dash_start, y), Pos2::new(dash_end, y)], stroke);
        }
        x += DASH_PERIOD;
    }
}

fn draw_dashed_vline(
    painter: &egui::Painter,
    x: f32,
    y_range: std::ops::RangeInclusive<f32>,
    offset: f32,
    stroke: Stroke,
) {
    let start = *y_range.start();
    let end = *y_range.end();
    if end <= start {
        return;
    }
    let mut y = start - offset;
    while y < end {
        let dash_start = y.max(start);
        let dash_end = (y + DASH_PERIOD / 2.0).min(end);
        if dash_end > dash_start {
            painter.line_segment([Pos2::new(x, dash_start), Pos2::new(x, dash_end)], stroke);
        }
        y += DASH_PERIOD;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch;
    use crate::state::AppState;
    use crate::tools::{Phase, PointerEvent};
    use ogre_core::{Rect, Selection};

    fn send_mod(
        tool: &mut RectSelectTool,
        doc: &ogre_core::Document,
        pos: IVec2,
        phase: Phase,
        modifiers: egui::Modifiers,
    ) -> Option<Box<dyn ogre_core::Command>> {
        tool.on_pointer(doc, PointerEvent::new(pos, phase, modifiers))
    }

    fn send(
        tool: &mut RectSelectTool,
        doc: &ogre_core::Document,
        pos: IVec2,
        phase: Phase,
    ) -> Option<Box<dyn ogre_core::Command>> {
        send_mod(tool, doc, pos, phase, egui::Modifiers::NONE)
    }

    #[test]
    fn rect_select_drag_dispatches_set_selection_cmd() {
        let mut state = AppState::new_document(2000, 1500);

        let mut tool = RectSelectTool::new();
        assert!(send(
            &mut tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(5, 5),
            Phase::Down
        )
        .is_none());
        assert!(send(
            &mut tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(105, 85),
            Phase::Drag
        )
        .is_none());
        let cmd = send(
            &mut tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(105, 85),
            Phase::Up,
        )
        .unwrap();
        dispatch::dispatch(&mut state, cmd).unwrap();

        assert_eq!(state.history().undo_len(), 1);
        assert_eq!(
            state.doc().selection,
            Selection::rect(Rect::new(5, 5, 100, 80))
        );
    }

    #[test]
    fn rect_select_drag_normalizes_reversed_direction() {
        let mut state = AppState::new_document(2000, 1500);

        let mut tool = RectSelectTool::new();
        send(
            &mut tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(105, 85),
            Phase::Down,
        );
        send(
            &mut tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(5, 5),
            Phase::Drag,
        );
        let cmd = send(
            &mut tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(5, 5),
            Phase::Up,
        )
        .unwrap();
        dispatch::dispatch(&mut state, cmd).unwrap();

        assert_eq!(
            state.doc().selection,
            Selection::rect(Rect::new(5, 5, 100, 80))
        );
    }

    #[test]
    fn rect_select_zero_area_drag_clears_selection() {
        let mut state = AppState::new_document(100, 100);
        let canvas = state.doc().canvas;
        dispatch::dispatch(
            &mut state,
            Box::new(ogre_core::SetSelectionCmd::new(Selection::select_all(
                canvas,
            ))),
        )
        .unwrap();

        let mut tool = RectSelectTool::new();
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
        dispatch::dispatch(&mut state, cmd).unwrap();

        assert_eq!(state.history().undo_len(), 2);
        assert!(state.doc().selection.is_empty());
    }

    #[test]
    fn selection_screen_rect_maps_at_zoom_and_pan() {
        // Pan so that doc (0,0) appears at screen (50,60); zoom 2x doubles sizes.
        let viewport = ogre_gpu::Viewport::new(glam::Vec2::new(-25.0, -30.0), 2.0);
        let canvas_rect = EguiRect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(500.0, 400.0));
        let rect = Rect::new(0, 0, 100, 50);

        let screen = selection_screen_rect(rect, &viewport, canvas_rect, 1.0).unwrap();
        assert!((screen.min.x - 50.0).abs() < 1e-3);
        assert!((screen.min.y - 60.0).abs() < 1e-3);
        assert!((screen.width() - 200.0).abs() < 1e-3);
        assert!((screen.height() - 100.0).abs() < 1e-3);
    }

    #[test]
    fn selection_screen_rect_maps_at_default_viewport() {
        let viewport = ogre_gpu::Viewport::new(glam::Vec2::ZERO, 1.0);
        let canvas_rect = EguiRect::from_min_max(Pos2::new(10.0, 20.0), Pos2::new(1010.0, 620.0));
        let rect = Rect::new(0, 0, 100, 50);

        let screen = selection_screen_rect(rect, &viewport, canvas_rect, 1.0).unwrap();
        assert!((screen.min.x - 10.0).abs() < 1e-3);
        assert!((screen.min.y - 20.0).abs() < 1e-3);
        assert!((screen.width() - 100.0).abs() < 1e-3);
        assert!((screen.height() - 50.0).abs() < 1e-3);
    }

    #[test]
    fn half_open_rect_excludes_max_corner() {
        assert_eq!(
            half_open_rect(IVec2::new(5, 5), IVec2::new(105, 85)),
            Rect::new(5, 5, 100, 80)
        );
        assert_eq!(
            half_open_rect(IVec2::new(105, 85), IVec2::new(5, 5)),
            Rect::new(5, 5, 100, 80)
        );
    }

    #[test]
    fn rect_select_shift_drag_adds_to_selection() {
        let mut state = AppState::new_document(2000, 1500);

        let mut tool = RectSelectTool::new();
        send(
            &mut tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(0, 0),
            Phase::Down,
        );
        let cmd = send(
            &mut tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(10, 10),
            Phase::Up,
        )
        .unwrap();
        dispatch::dispatch(&mut state, cmd).unwrap();
        assert_eq!(
            state.doc().selection,
            Selection::rect(Rect::new(0, 0, 10, 10))
        );

        send_mod(
            &mut tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(20, 0),
            Phase::Down,
            egui::Modifiers::SHIFT,
        );
        let cmd = send_mod(
            &mut tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(30, 10),
            Phase::Up,
            egui::Modifiers::SHIFT,
        )
        .unwrap();
        dispatch::dispatch(&mut state, cmd).unwrap();

        assert!(state.doc().selection.coverage_at(IVec2::new(5, 5)) > 0.0);
        assert!(state.doc().selection.coverage_at(IVec2::new(25, 5)) > 0.0);
    }

    #[test]
    fn rect_select_modifier_click_without_drag_ignores() {
        let mut state = AppState::new_document(100, 100);
        let canvas = state.doc().canvas;
        dispatch::dispatch(
            &mut state,
            Box::new(ogre_core::SetSelectionCmd::new(Selection::select_all(
                canvas,
            ))),
        )
        .unwrap();

        let mut tool = RectSelectTool::new();
        for modifiers in [
            egui::Modifiers::SHIFT,
            egui::Modifiers::ALT,
            egui::Modifiers::SHIFT | egui::Modifiers::ALT,
        ] {
            send_mod(
                &mut tool,
                &state.tabs[state.active_tab].doc,
                IVec2::new(10, 10),
                Phase::Down,
                modifiers,
            );
            let cmd = send_mod(
                &mut tool,
                &state.tabs[state.active_tab].doc,
                IVec2::new(10, 10),
                Phase::Up,
                modifiers,
            );
            assert!(cmd.is_none(), "{:?} click should not dispatch", modifiers);
        }

        assert_eq!(
            state.doc().selection,
            Selection::select_all(state.doc().canvas)
        );
        assert_eq!(state.history().undo_len(), 1);
    }
}
