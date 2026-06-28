// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! The Gradient tool (§3.2.1).
//!
//! Drag a document-space line; on release, commit a [`GradientFillCmd`] that
//! paints a foreground→background gradient along that line on the active raster
//! layer (clipped to the current selection, if any). While dragging, the
//! overlay draws the gradient line with arrowheads.

use egui::{Pos2, Stroke};
use glam::Vec2;
use ogre_core::{GradientFillCmd, GradientKind, Rgba32F, WrapMode};

use crate::panels::canvas::doc_to_screen;
use crate::tools::{Phase, PointerEvent, Tool};

/// Editable gradient-tool settings surfaced in the sidebar.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GradientSettings {
    /// Linear / Radial / Conical / Diamond.
    pub kind: GradientKind,
    /// Swap foreground/background endpoint mapping.
    pub reverse: bool,
    /// Overall opacity applied to the gradient pixels.
    pub opacity: f32,
    /// How `t` behaves outside `[0,1]`.
    pub wrap: WrapMode,
}

impl Default for GradientSettings {
    fn default() -> Self {
        Self {
            kind: GradientKind::Linear,
            reverse: false,
            opacity: 1.0,
            wrap: WrapMode::Clamp,
        }
    }
}

/// The Gradient tool: drag p0→p1, release to commit.
#[derive(Debug)]
pub struct GradientTool {
    settings: GradientSettings,
    /// Foreground (gradient start) and background (gradient end) colors, pushed
    /// in from the shared AppState swatches on each stroke.
    fg: Rgba32F,
    bg: Rgba32F,
    active_layer: Option<ogre_core::LayerId>,
    p0: Option<glam::IVec2>,
    p1: Option<glam::IVec2>,
}

impl GradientTool {
    /// Create a default linear-gradient tool.
    pub fn new() -> Self {
        Self {
            settings: GradientSettings::default(),
            fg: Rgba32F::new(0.0, 0.0, 0.0, 1.0),
            bg: Rgba32F::new(1.0, 1.0, 1.0, 1.0),
            active_layer: None,
            p0: None,
            p1: None,
        }
    }

    /// Current settings (read by the sidebar).
    pub fn settings(&self) -> &GradientSettings {
        &self.settings
    }

    /// Mutable settings (sidebar controls).
    pub fn settings_mut(&mut self) -> &mut GradientSettings {
        &mut self.settings
    }

    /// Push the foreground/background swatches (driven by AppState).
    pub fn set_colors(&mut self, fg: Rgba32F, bg: Rgba32F) {
        self.fg = fg;
        self.bg = bg;
    }
}

impl Default for GradientTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for GradientTool {
    fn on_pointer(
        &mut self,
        doc: &ogre_core::Document,
        ev: PointerEvent,
    ) -> Option<Box<dyn ogre_core::Command>> {
        match ev.phase {
            Phase::Down => {
                self.active_layer = None;
                self.p0 = None;
                self.p1 = None;
                // Require a raster, unlocked active layer (C6: gate on
                // is_raster here; the command re-checks `locked` on apply).
                if let Some(&layer) = doc.active.as_ref() {
                    if doc.layer(layer).ok()?.is_raster() {
                        self.active_layer = Some(layer);
                        self.p0 = Some(ev.doc_pos);
                        self.p1 = Some(ev.doc_pos);
                    }
                }
                None
            }
            Phase::Drag => {
                if self.active_layer.is_some() {
                    self.p1 = Some(ev.doc_pos);
                }
                None
            }
            Phase::Up => {
                let cmd = self.active_layer.and_then(|layer| {
                    let (Some(p0), Some(p1)) = (self.p0, self.p1) else {
                        return None;
                    };
                    let canvas = doc.canvas;
                    // A drag shorter than ~3 px falls back to a left→right
                    // gradient across the canvas at the click row.
                    let (a, b) = if (p1 - p0).as_vec2().length() < 3.0 {
                        (
                            Vec2::new(canvas.x as f32, p0.y as f32 + 0.5),
                            Vec2::new((canvas.x + canvas.w as i32 - 1) as f32, p0.y as f32 + 0.5),
                        )
                    } else {
                        (
                            Vec2::new(p0.x as f32 + 0.5, p0.y as f32 + 0.5),
                            Vec2::new(p1.x as f32 + 0.5, p1.y as f32 + 0.5),
                        )
                    };
                    Some(Box::new(GradientFillCmd::new(
                        layer,
                        a,
                        b,
                        self.settings.kind,
                        self.fg,
                        self.bg,
                        self.settings.reverse,
                        self.settings.opacity,
                        self.settings.wrap,
                        doc.selection.clone(),
                    )) as Box<dyn ogre_core::Command>)
                });
                self.active_layer = None;
                self.p0 = None;
                self.p1 = None;
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
        let (Some(p0), Some(p1)) = (self.p0, self.p1) else {
            return;
        };
        let to_screen = |p: glam::IVec2| -> Pos2 {
            let v = doc_to_screen(p, viewport, pixels_per_point)
                + Vec2::new(canvas_rect.min.x, canvas_rect.min.y);
            Pos2::new(v.x, v.y)
        };
        let stroke = Stroke::new(1.5, egui::Color32::WHITE);
        painter.line_segment([to_screen(p0), to_screen(p1)], stroke);
        // Small endpoint dots so p0/p1 are distinguishable.
        painter.circle_filled(to_screen(p0), 3.0, egui::Color32::WHITE);
        painter.circle_stroke(to_screen(p1), 4.0, stroke);
    }

    fn cancel(&mut self) {
        self.active_layer = None;
        self.p0 = None;
        self.p1 = None;
    }
}

/// Re-export the kind enum under the module-friendly name `Gk` used by the
/// sidebar radio group.
pub use ogre_core::GradientKind as Gk;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Tool;
    use glam::IVec2;

    fn ev(pos: IVec2, phase: Phase) -> PointerEvent {
        PointerEvent::new(pos, phase, egui::Modifiers::NONE)
    }

    #[test]
    fn gradient_drag_emits_command_on_up() {
        let mut doc = ogre_core::Document::new(64, 32);
        let layer = doc.add_raster_layer("L");
        doc.active = Some(layer);
        // Seed a pixel so the layer has occupied bounds to fill into.
        doc.layer_mut(layer)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(0, 0), Rgba32F::new(0.0, 0.0, 0.0, 1.0));
        for x in 0..64 {
            doc.layer_mut(layer)
                .unwrap()
                .buffer_mut()
                .unwrap()
                .set_pixel(IVec2::new(x, 0), Rgba32F::new(0.0, 0.0, 0.0, 1.0));
        }

        let mut tool = GradientTool::new();
        tool.set_colors(
            Rgba32F::new(0.0, 0.0, 0.0, 1.0),
            Rgba32F::new(1.0, 1.0, 1.0, 1.0),
        );
        assert!(tool
            .on_pointer(&doc, ev(IVec2::new(0, 0), Phase::Down))
            .is_none());
        assert!(tool
            .on_pointer(&doc, ev(IVec2::new(63, 0), Phase::Drag))
            .is_none());
        let cmd = tool.on_pointer(&doc, ev(IVec2::new(63, 0), Phase::Up));
        assert!(cmd.is_some(), "drag must commit a GradientFillCmd");
    }

    #[test]
    fn gradient_no_raster_layer_emits_nothing() {
        let mut doc = ogre_core::Document::new(16, 16);
        doc.active = None;
        let mut tool = GradientTool::new();
        tool.on_pointer(&doc, ev(IVec2::new(0, 0), Phase::Down));
        let cmd = tool.on_pointer(&doc, ev(IVec2::new(10, 0), Phase::Up));
        assert!(cmd.is_none(), "no active raster layer ⇒ no command");
    }

    #[test]
    fn gradient_settings_default_is_linear() {
        use ogre_core::fill_gradient;
        let s = GradientSettings::default();
        assert_eq!(s.kind, GradientKind::Linear);
        assert!(!s.reverse);
        assert_eq!(s.opacity, 1.0);
        // Silence unused-import check for the re-export alias in tests.
        let _ = Gk::Linear;
        let _ = fill_gradient; // referenced for completeness
    }
}
