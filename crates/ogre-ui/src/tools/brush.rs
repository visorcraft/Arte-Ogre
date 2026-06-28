// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Brush, Pencil, and Eraser tools.
//!
//! Each tool is a distinct type (`BrushTool`, `PencilTool`, `EraserTool`) that
//! delegates its behaviour to a shared [`PaintTool`] engine.  Down→Drag→Up
//! collects input samples and commits a [`PaintStrokeCmd`] on release.  While
//! the drag is in progress the smoothed stroke path and a brush-cursor ring are
//! drawn as an overlay.

use egui::{Pos2, Stroke};
use glam::IVec2;
use kurbo::Point;
use ogre_core::{BrushSettings, InputSample, PaintMode, PaintStrokeCmd, Rgba32F};
use ogre_vector::stroke::{sample_stroke, StrokeBuilder, StrokeSegment};
use std::cell::{Cell, RefCell};

use crate::panels::canvas::doc_to_screen;
use crate::tools::{Phase, PointerEvent, Tool};

/// Maximum raw pointer samples retained for a single paint stroke.
const MAX_PAINT_SAMPLES: usize = 10_000;

/// A painting tool: Brush, Pencil, or Eraser.
#[derive(Debug)]
pub struct PaintTool {
    /// Current tool settings (size, hardness, opacity, flow, spacing).
    settings: BrushSettings,
    /// Brush colour.  Ignored for the eraser.
    color: Rgba32F,
    /// Brush or eraser mode.
    mode: PaintMode,
    /// Pencil tools have fixed settings and should not show the settings panel.
    ui_editable: bool,
    /// Active layer for the current stroke, if any.
    active_layer: Option<ogre_core::LayerId>,
    /// Settings snapshot taken when the stroke starts.  Used for both the live
    /// preview and the committed command so a settings change mid-stroke does
    /// not desynchronise the two.
    stroke_settings: Option<BrushSettings>,
    /// Raw input samples for the stroke in progress.
    samples: Vec<InputSample>,
    /// Smoothed stroke geometry used for the live preview. Wrapped in `RefCell`
    /// so the overlay can be drawn through an immutable `&self` trait receiver.
    stroke_builder: RefCell<StrokeBuilder>,
    /// Finalized segments already emitted by `stroke_builder` are cached here so
    /// the overlay does not rebuild the entire path every frame. This avoids the
    /// O(n²) work of calling `segments()` on every UI frame during a long stroke.
    finalized_segments: RefCell<Vec<StrokeSegment>>,
    /// Number of finalized segments already moved into `finalized_segments`.
    /// Stored separately because `StrokeBuilder` is behind `RefCell`.
    finalized_count: Cell<usize>,
    /// Latest pointer position in document pixels, for the cursor ring.
    current_pos: IVec2,
}

impl PaintTool {
    /// Create a default round brush.
    pub fn brush() -> Self {
        Self {
            settings: BrushSettings::default(),
            color: Rgba32F::new(1.0, 0.0, 0.0, 1.0),
            mode: PaintMode::Brush,
            ui_editable: true,
            active_layer: None,
            stroke_settings: None,
            samples: Vec::new(),
            stroke_builder: RefCell::new(StrokeBuilder::new(BrushSettings::default())),
            finalized_segments: RefCell::new(Vec::new()),
            finalized_count: Cell::new(0),
            current_pos: IVec2::ZERO,
        }
    }

    /// Create a one-pixel hard pencil.
    pub fn pencil() -> Self {
        let settings = BrushSettings {
            size: 1.0,
            hardness: 1.0,
            opacity: 1.0,
            flow: 1.0,
            spacing: 1.0,
            pressure_size: false,
            pressure_opacity: false,
        };
        Self {
            settings,
            color: Rgba32F::new(0.0, 0.0, 0.0, 1.0),
            mode: PaintMode::Brush,
            ui_editable: false,
            active_layer: None,
            stroke_settings: None,
            samples: Vec::new(),
            stroke_builder: RefCell::new(StrokeBuilder::new(settings)),
            finalized_segments: RefCell::new(Vec::new()),
            finalized_count: Cell::new(0),
            current_pos: IVec2::ZERO,
        }
    }

    /// Create an eraser.
    pub fn eraser() -> Self {
        let settings = BrushSettings {
            size: 20.0,
            ..Default::default()
        };
        Self {
            settings,
            color: Rgba32F::TRANSPARENT,
            mode: PaintMode::Eraser,
            ui_editable: true,
            active_layer: None,
            stroke_settings: None,
            samples: Vec::new(),
            stroke_builder: RefCell::new(StrokeBuilder::new(settings)),
            finalized_segments: RefCell::new(Vec::new()),
            finalized_count: Cell::new(0),
            current_pos: IVec2::ZERO,
        }
    }

    /// Current tool settings.
    pub fn settings(&self) -> &BrushSettings {
        &self.settings
    }

    /// Mutable access to tool settings.
    pub fn settings_mut(&mut self) -> &mut BrushSettings {
        &mut self.settings
    }

    /// Current brush colour.
    pub fn color(&self) -> Rgba32F {
        self.color
    }

    /// Mutable access to brush colour.
    pub fn color_mut(&mut self) -> &mut Rgba32F {
        &mut self.color
    }

    /// True if this tool's settings can be edited from the settings panel.
    pub fn is_ui_editable(&self) -> bool {
        self.ui_editable
    }

    /// Paint mode of this tool.
    pub fn mode(&self) -> PaintMode {
        self.mode
    }

    fn sample_from_event(ev: PointerEvent) -> InputSample {
        // `doc_pos` is the top-left of the pixel under the pointer; sample at
        // the pixel centre for consistent stamping.
        InputSample::with_pressure(
            glam::Vec2::new(ev.doc_pos.x as f32 + 0.5, ev.doc_pos.y as f32 + 0.5),
            ev.pressure,
        )
    }
}

impl Tool for PaintTool {
    fn on_pointer(
        &mut self,
        doc: &ogre_core::Document,
        ev: PointerEvent,
    ) -> Option<Box<dyn ogre_core::Command>> {
        self.current_pos = ev.doc_pos;

        match ev.phase {
            Phase::Down => {
                self.samples.clear();
                self.finalized_segments.borrow_mut().clear();
                self.finalized_count.set(0);
                self.active_layer = None;
                self.stroke_settings = None;

                // Require a raster active layer.  Painting without one is a no-op.
                if let Some(&layer) = doc.active.as_ref() {
                    if doc.layer(layer).ok()?.is_raster() {
                        self.active_layer = Some(layer);
                        self.stroke_settings = Some(self.settings);
                        let settings = self.stroke_settings.unwrap_or(self.settings);
                        self.stroke_builder = RefCell::new(StrokeBuilder::new(settings));
                        let s = Self::sample_from_event(ev);
                        self.samples.push(s);
                        self.stroke_builder.borrow_mut().append(s);
                    }
                }
                None
            }
            Phase::Drag => {
                if self.active_layer.is_some() && self.samples.len() < MAX_PAINT_SAMPLES {
                    let s = Self::sample_from_event(ev);
                    self.samples.push(s);
                    self.stroke_builder.borrow_mut().append(s);
                }
                None
            }
            Phase::Up => {
                let cmd = self.active_layer.and_then(|layer| {
                    let settings = self.stroke_settings.unwrap_or(self.settings);
                    // Commit the smoothed stroke geometry, not the raw polyline,
                    // so the result matches the live preview.
                    let commit_samples = if self.stroke_builder.borrow().sample_count() >= 2 {
                        sample_stroke(&self.stroke_builder.borrow().segments(), &settings)
                    } else {
                        self.samples.clone()
                    };
                    if commit_samples.is_empty() {
                        return None;
                    }
                    Some(Box::new(PaintStrokeCmd::new(
                        layer,
                        commit_samples,
                        settings,
                        self.color,
                        self.mode,
                    )) as Box<dyn ogre_core::Command>)
                });
                self.samples.clear();
                self.stroke_builder.borrow_mut().clear();
                self.finalized_segments.borrow_mut().clear();
                self.finalized_count.set(0);
                self.active_layer = None;
                self.stroke_settings = None;
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
        let stroke = Stroke::new(1.0, egui::Color32::WHITE);

        // Drain newly-finalized segments from the builder and cache them so we
        // don't rebuild the whole path every frame (that was O(n²) for a stroke
        // with n samples). `RefCell` is used because the `Tool` trait draws
        // through an immutable receiver.
        {
            let count = self.stroke_builder.borrow().finalized_count();
            let last = self.finalized_count.get();
            if count > last {
                let new = self.stroke_builder.borrow().finalized_since(last);
                self.finalized_segments.borrow_mut().extend(new);
                self.finalized_count.set(count);
            }
        }

        // Draw the smoothed preview path.
        use kurbo::PathEl;
        for seg in self
            .finalized_segments
            .borrow()
            .iter()
            .chain(self.stroke_builder.borrow().preview_segment().iter())
        {
            let [PathEl::MoveTo(p0), PathEl::CurveTo(c1, c2, p1)] = seg.path.elements() else {
                continue;
            };
            painter.add(egui::Shape::CubicBezier(
                egui::epaint::CubicBezierShape::from_points_stroke(
                    [
                        to_screen_pos(*p0, viewport, canvas_rect, pixels_per_point),
                        to_screen_pos(*c1, viewport, canvas_rect, pixels_per_point),
                        to_screen_pos(*c2, viewport, canvas_rect, pixels_per_point),
                        to_screen_pos(*p1, viewport, canvas_rect, pixels_per_point),
                    ],
                    false,
                    egui::Color32::TRANSPARENT,
                    stroke,
                ),
            ));
        }

        // Brush cursor ring at the latest pointer position.
        if self.active_layer.is_some() || !self.samples.is_empty() {
            let settings = self.stroke_settings.unwrap_or(self.settings);
            let radius_screen =
                (settings.width_at_pressure(1.0) / 2.0) * viewport.zoom / pixels_per_point;
            if radius_screen > 0.5 {
                let centre = doc_to_screen(self.current_pos, viewport, pixels_per_point)
                    + glam::Vec2::new(canvas_rect.min.x, canvas_rect.min.y);
                let centre = Pos2::new(centre.x, centre.y);
                painter.circle_stroke(centre, radius_screen, stroke);
            }
        }
    }

    fn cancel(&mut self) {
        self.samples.clear();
        self.stroke_builder.borrow_mut().clear();
        self.finalized_segments.borrow_mut().clear();
        self.finalized_count.set(0);
        self.active_layer = None;
        self.stroke_settings = None;
    }
}

/// Round brush with pressure and variable settings.
#[derive(Debug)]
pub struct BrushTool(pub(crate) PaintTool);

impl BrushTool {
    /// Create a default round brush.
    pub fn new() -> Self {
        Self(PaintTool::brush())
    }
}

impl Default for BrushTool {
    fn default() -> Self {
        Self::new()
    }
}

impl std::ops::Deref for BrushTool {
    type Target = PaintTool;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for BrushTool {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Tool for BrushTool {
    fn on_pointer(
        &mut self,
        doc: &ogre_core::Document,
        ev: PointerEvent,
    ) -> Option<Box<dyn ogre_core::Command>> {
        self.0.on_pointer(doc, ev)
    }

    fn draw_overlay(
        &self,
        painter: &egui::Painter,
        doc: &ogre_core::Document,
        viewport: &ogre_gpu::Viewport,
        canvas_rect: egui::Rect,
        pixels_per_point: f32,
        time: f64,
    ) {
        self.0
            .draw_overlay(painter, doc, viewport, canvas_rect, pixels_per_point, time)
    }

    fn cancel(&mut self) {
        self.0.cancel();
    }
}

/// One-pixel hard brush (aliasing off).
#[derive(Debug)]
pub struct PencilTool(pub(crate) PaintTool);

impl PencilTool {
    /// Create a one-pixel hard pencil.
    pub fn new() -> Self {
        Self(PaintTool::pencil())
    }
}

impl Default for PencilTool {
    fn default() -> Self {
        Self::new()
    }
}

impl std::ops::Deref for PencilTool {
    type Target = PaintTool;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for PencilTool {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Tool for PencilTool {
    fn on_pointer(
        &mut self,
        doc: &ogre_core::Document,
        ev: PointerEvent,
    ) -> Option<Box<dyn ogre_core::Command>> {
        self.0.on_pointer(doc, ev)
    }

    fn draw_overlay(
        &self,
        painter: &egui::Painter,
        doc: &ogre_core::Document,
        viewport: &ogre_gpu::Viewport,
        canvas_rect: egui::Rect,
        pixels_per_point: f32,
        time: f64,
    ) {
        self.0
            .draw_overlay(painter, doc, viewport, canvas_rect, pixels_per_point, time)
    }

    fn cancel(&mut self) {
        self.0.cancel();
    }
}

/// Round brush that removes alpha.
#[derive(Debug)]
pub struct EraserTool(pub(crate) PaintTool);

impl EraserTool {
    /// Create an eraser.
    pub fn new() -> Self {
        Self(PaintTool::eraser())
    }
}

impl Default for EraserTool {
    fn default() -> Self {
        Self::new()
    }
}

impl std::ops::Deref for EraserTool {
    type Target = PaintTool;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for EraserTool {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Tool for EraserTool {
    fn on_pointer(
        &mut self,
        doc: &ogre_core::Document,
        ev: PointerEvent,
    ) -> Option<Box<dyn ogre_core::Command>> {
        self.0.on_pointer(doc, ev)
    }

    fn draw_overlay(
        &self,
        painter: &egui::Painter,
        doc: &ogre_core::Document,
        viewport: &ogre_gpu::Viewport,
        canvas_rect: egui::Rect,
        pixels_per_point: f32,
        time: f64,
    ) {
        self.0
            .draw_overlay(painter, doc, viewport, canvas_rect, pixels_per_point, time)
    }

    fn cancel(&mut self) {
        self.0.cancel();
    }
}

fn to_screen_pos(
    p: Point,
    viewport: &ogre_gpu::Viewport,
    canvas_rect: egui::Rect,
    pixels_per_point: f32,
) -> Pos2 {
    // Keep sub-pixel precision from the smoothed Bézier control points.
    let doc = glam::Vec2::new(p.x as f32, p.y as f32);
    let v = (doc - viewport.pan) * viewport.zoom / pixels_per_point
        + glam::Vec2::new(canvas_rect.min.x, canvas_rect.min.y);
    Pos2::new(v.x, v.y)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch;
    use crate::state::AppState;
    use crate::tools::{Phase, PointerEvent};

    fn send(
        tool: &mut PaintTool,
        doc: &ogre_core::Document,
        pos: IVec2,
        phase: Phase,
    ) -> Option<Box<dyn ogre_core::Command>> {
        tool.on_pointer(doc, PointerEvent::new(pos, phase, egui::Modifiers::NONE))
    }

    #[test]
    fn brush_tool_down_drag_up_commits_stroke() {
        let mut state = AppState::new_document(2000, 1500);
        let layer = state.doc().active.unwrap();
        state.tool_manager.set_tool(crate::tools::ToolKind::Brush);

        let tool = state.tool_manager.active_paint_settings_mut().unwrap();
        assert!(send(
            tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(100, 100),
            Phase::Down
        )
        .is_none());
        assert!(send(
            tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(110, 100),
            Phase::Drag
        )
        .is_none());
        let cmd = send(
            tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(120, 100),
            Phase::Up,
        );
        assert!(cmd.is_some());

        dispatch::dispatch(&mut state, cmd.unwrap()).unwrap();
        assert_eq!(state.history().undo_len(), 1);
        assert!(
            state
                .doc()
                .layer(layer)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(100, 100))
                .a
                > 0.0
        );
    }

    #[test]
    fn brush_tool_drag_does_not_push_history() {
        let mut state = AppState::new_document(2000, 1500);
        state.tool_manager.set_tool(crate::tools::ToolKind::Brush);

        let tool = state.tool_manager.active_paint_settings_mut().unwrap();
        send(
            tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(100, 100),
            Phase::Down,
        );
        send(
            tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(110, 100),
            Phase::Drag,
        );
        send(
            tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(120, 100),
            Phase::Drag,
        );
        assert_eq!(state.history().undo_len(), 0);
    }

    #[test]
    fn brush_tool_cancel_drops_pending_stroke() {
        let mut state = AppState::new_document(2000, 1500);
        state.tool_manager.set_tool(crate::tools::ToolKind::Brush);

        let tool = state.tool_manager.active_paint_settings_mut().unwrap();
        send(
            tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(100, 100),
            Phase::Down,
        );
        send(
            tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(110, 100),
            Phase::Drag,
        );
        tool.cancel();
        let cmd = send(
            tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(120, 100),
            Phase::Up,
        );
        assert!(cmd.is_none());
    }

    #[test]
    fn brush_tool_single_click_deposits_stamp() {
        let mut state = AppState::new_document(2000, 1500);
        let layer = state.doc().active.unwrap();
        state.tool_manager.set_tool(crate::tools::ToolKind::Brush);

        let tool = state.tool_manager.active_paint_settings_mut().unwrap();
        send(
            tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(200, 200),
            Phase::Down,
        );
        let cmd = send(
            tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(200, 200),
            Phase::Up,
        );
        assert!(cmd.is_some());

        dispatch::dispatch(&mut state, cmd.unwrap()).unwrap();
        assert_eq!(state.history().undo_len(), 1);
        assert!(
            state
                .doc()
                .layer(layer)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(200, 200))
                .a
                > 0.0
        );
    }

    #[test]
    fn eraser_tool_reduces_alpha() {
        let mut state = AppState::new_document(2000, 1500);
        let layer = state.doc().active.unwrap();
        state.tool_manager.set_tool(crate::tools::ToolKind::Brush);

        // Paint a red dot.
        let tool = state.tool_manager.active_paint_settings_mut().unwrap();
        send(
            tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(300, 300),
            Phase::Down,
        );
        let cmd = send(
            tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(300, 300),
            Phase::Up,
        )
        .unwrap();
        dispatch::dispatch(&mut state, cmd).unwrap();
        assert!(
            state
                .doc()
                .layer(layer)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(300, 300))
                .a
                > 0.9
        );

        // Erase it.
        state.tool_manager.set_tool(crate::tools::ToolKind::Eraser);
        let tool = state.tool_manager.active_paint_settings_mut().unwrap();
        send(
            tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(300, 300),
            Phase::Down,
        );
        let cmd = send(
            tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(300, 300),
            Phase::Up,
        )
        .unwrap();
        dispatch::dispatch(&mut state, cmd).unwrap();

        assert!(
            state
                .doc()
                .layer(layer)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(300, 300))
                .a
                < 0.5
        );
    }

    #[test]
    fn tool_switch_mid_stroke_cancels_paint_tool() {
        let mut state = AppState::new_document(2000, 1500);
        state.tool_manager.set_tool(crate::tools::ToolKind::Brush);

        let tool = state.tool_manager.active_paint_settings_mut().unwrap();
        send(
            tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(400, 400),
            Phase::Down,
        );
        send(
            tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(410, 400),
            Phase::Drag,
        );

        // Switching tools cancels the pending stroke.
        state
            .tool_manager
            .set_tool(crate::tools::ToolKind::RectSelect);
        state.tool_manager.set_tool(crate::tools::ToolKind::Brush);
        let tool = state.tool_manager.active_paint_settings_mut().unwrap();
        let cmd = send(
            tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(420, 400),
            Phase::Up,
        );
        assert!(cmd.is_none());
    }

    #[test]
    fn brush_tool_uses_event_pressure() {
        let mut state = AppState::new_document(2000, 1500);
        let layer = state.doc().active.unwrap();
        state.tool_manager.set_tool(crate::tools::ToolKind::Brush);

        let tool = state.tool_manager.active_paint_settings_mut().unwrap();
        let mut ev = PointerEvent::new(IVec2::new(500, 500), Phase::Down, egui::Modifiers::NONE);
        ev.pressure = 0.25;
        tool.on_pointer(&state.tabs[state.active_tab].doc, ev);

        let mut ev = PointerEvent::new(IVec2::new(500, 500), Phase::Up, egui::Modifiers::NONE);
        ev.pressure = 0.25;
        let cmd = tool
            .on_pointer(&state.tabs[state.active_tab].doc, ev)
            .unwrap();
        dispatch::dispatch(&mut state, cmd).unwrap();

        let alpha = state
            .doc()
            .layer(layer)
            .unwrap()
            .buffer()
            .unwrap()
            .get_pixel(IVec2::new(500, 500))
            .a;
        // Low pressure should deposit less opaque paint.
        assert!(alpha > 0.0 && alpha < 0.9);
    }

    #[test]
    fn brush_tool_respects_sample_budget() {
        let mut state = AppState::new_document(2000, 1500);
        state.tool_manager.set_tool(crate::tools::ToolKind::Brush);

        let tool = state.tool_manager.active_paint_settings_mut().unwrap();
        assert!(send(
            tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(100, 100),
            Phase::Down
        )
        .is_none());

        for i in 0..12_000 {
            assert!(
                tool.samples.len() <= MAX_PAINT_SAMPLES,
                "sample budget exceeded at drag {}",
                i
            );
            assert!(send(
                tool,
                &state.tabs[state.active_tab].doc,
                IVec2::new(100 + i, 100),
                Phase::Drag
            )
            .is_none());
        }
        assert!(tool.samples.len() <= MAX_PAINT_SAMPLES);

        let cmd = send(
            tool,
            &state.tabs[state.active_tab].doc,
            IVec2::new(200, 100),
            Phase::Up,
        );
        assert!(
            cmd.is_some(),
            "stroke should still commit after budget is reached"
        );
    }
}
