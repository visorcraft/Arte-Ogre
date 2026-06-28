// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Move and Free Transform tools.
//!
//! [`MoveTool`] drags a raster layer by an integer pixel offset without
//! resampling. [`FreeTransformTool`] previews an arbitrary affine transform
//! (scale, rotate, skew, translate) with a wireframe overlay and commits it on
//! Enter. A pure integer translation is dispatched as a
//! [`ogre_core::MoveLayerByCmd`] so pixels are preserved exactly.

use egui::{Pos2, Rect as EguiRect, Stroke};
use glam::{Affine2, IVec2, Vec2};

use crate::panels::canvas::{doc_to_screen, doc_to_screen_f};
use crate::tools::{Phase, PointerEvent, Tool};

/// Axis for a skew interaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Axis {
    /// Horizontal shear (parallel to the top/bottom edges).
    X,
    /// Vertical shear (parallel to the left/right edges).
    Y,
}

/// Pointer-driven interaction mode for [`FreeTransformTool`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PointerMode {
    /// Dragging inside the layer bounds translates the layer.
    Translate,
    /// Dragging near a corner scales uniformly around the centre.
    Scale,
    /// Dragging outside the bounds (not near a handle) rotates around the centre.
    Rotate,
    /// Dragging near an edge midpoint skews along the given axis.
    Skew { axis: Axis },
}

/// Sidebar-selectable transform mode for [`FreeTransformTool`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum FreeTransformMode {
    /// Combined scale/rotate/translate by pointer location.
    #[default]
    ScaleRotate,
    /// Skew by dragging edge midpoints.
    Skew,
    /// Free corner dragging (arbitrary quadrilateral).
    Distort,
    /// Corner dragging constrained to a perspective projection.
    Perspective,
    /// Bezier-grid warp with a 4x4 control lattice.
    Warp,
}

/// Move a raster layer by an integer pixel offset.
///
/// Dragging the canvas adds the drag delta to the layer offset. No resampling
/// occurs, so pixels are preserved exactly.
#[derive(Debug, Default)]
pub struct MoveTool {
    /// Layer being moved, if any.
    layer: Option<ogre_core::LayerId>,
    /// Pointer position at the start of the drag.
    start: Option<IVec2>,
    /// Current pointer position during the drag.
    current: IVec2,
}

impl MoveTool {
    /// Create a new move tool with no active drag.
    pub fn new() -> Self {
        Self::default()
    }

    /// True if the tool has an active drag.
    fn is_active(&self) -> bool {
        self.layer.is_some() && self.start.is_some()
    }
}

impl Tool for MoveTool {
    fn on_pointer(
        &mut self,
        doc: &ogre_core::Document,
        ev: PointerEvent,
    ) -> Option<Box<dyn ogre_core::Command>> {
        match ev.phase {
            Phase::Down => {
                let id = doc.active?;
                let layer = doc.layer(id).ok()?;
                if !layer.is_raster() || layer.locked {
                    return None;
                }
                self.layer = Some(id);
                self.start = Some(ev.doc_pos);
                self.current = ev.doc_pos;
                None
            }
            Phase::Drag => {
                self.current = ev.doc_pos;
                None
            }
            Phase::Up => {
                let layer = self.layer.take()?;
                let start = self.start.take()?;
                self.current = ev.doc_pos;

                let delta = self.current - start;
                if delta == IVec2::ZERO {
                    return None;
                }

                let layer_ref = doc.layer(layer).ok()?;
                if layer_ref.locked {
                    return None;
                }

                Some(Box::new(ogre_core::MoveLayerByCmd::new(layer, delta)))
            }
        }
    }

    fn draw_overlay(
        &self,
        painter: &egui::Painter,
        doc: &ogre_core::Document,
        viewport: &ogre_gpu::Viewport,
        canvas_rect: EguiRect,
        pixels_per_point: f32,
        _time: f64,
    ) {
        if !self.is_active() {
            return;
        }
        let Some(id) = self.layer else { return };
        let Ok(layer) = doc.layer(id) else { return };
        let Some(buffer) = layer.buffer() else { return };
        let Some(bounds) = buffer.exact_bounds() else {
            return;
        };
        let doc_bounds = ogre_core::Rect::new(
            bounds.x + layer.offset.x,
            bounds.y + layer.offset.y,
            bounds.w,
            bounds.h,
        );
        let Some(screen) =
            selection_screen_rect(doc_bounds, viewport, canvas_rect, pixels_per_point)
        else {
            return;
        };
        draw_wireframe(painter, screen);
    }

    fn cancel(&mut self) {
        self.layer = None;
        self.start = None;
    }
}

/// Free-transform a raster layer with a live affine/projective/warp preview.
///
/// Primary-button drag starts a transform. The sidebar-selected
/// [`FreeTransformMode`] determines the handle set and interaction:
/// - `ScaleRotate`: dragging inside the layer bounds translates; near a corner
///   scales uniformly; near an edge midpoint skews; elsewhere rotates.
/// - `Skew`: dragging edge midpoints skews (other locations translate).
/// - `Distort`/`Perspective`: drag the four corner handles independently.
/// - `Warp`: drag the points of a 4x4 control grid.
///
/// Pressing Enter commits the previewed transform.
#[derive(Debug)]
pub struct FreeTransformTool {
    /// Layer being transformed, if any.
    layer: Option<ogre_core::LayerId>,
    /// Pointer position at the start of the drag.
    start: Option<IVec2>,
    /// Current pointer position during the drag.
    current: IVec2,
    /// Centre of the layer bounds in document coordinates.
    center: Vec2,
    /// Angle from the centre to the start pointer, in radians.
    start_angle: f32,
    /// Sidebar-selected transform mode.
    transform_mode: FreeTransformMode,
    /// Current pointer-driven interaction mode (ScaleRotate/Skew only).
    pointer_mode: PointerMode,
    /// Preview affine: source document space -> destination document space.
    affine: Affine2,
    /// Source document-space bounds corners (TL, TR, BR, BL) for
    /// Distort/Perspective/Warp.
    src_quad: [IVec2; 4],
    /// Destination document-space quad for Distort/Perspective.
    dst_quad: [IVec2; 4],
    /// Control grid for Warp mode (document-space points).
    warp_grid: ogre_core::resample::WarpGrid,
    /// Handle currently being dragged, if any.
    handle: Option<Handle>,
}

/// A transform handle being dragged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Handle {
    /// One of the four corner handles (0=TL, 1=TR, 2=BR, 3=BL).
    Corner(usize),
    /// One point in the warp grid (row-major index).
    GridPoint(usize),
}

impl Default for FreeTransformTool {
    fn default() -> Self {
        Self {
            layer: None,
            start: None,
            current: IVec2::ZERO,
            center: Vec2::ZERO,
            start_angle: 0.0,
            transform_mode: FreeTransformMode::ScaleRotate,
            pointer_mode: PointerMode::Translate,
            affine: Affine2::IDENTITY,
            src_quad: [IVec2::ZERO; 4],
            dst_quad: [IVec2::ZERO; 4],
            warp_grid: ogre_core::resample::WarpGrid::identity(
                ogre_core::Rect::new(0, 0, 1, 1),
                4,
                4,
            ),
            handle: None,
        }
    }
}

impl FreeTransformTool {
    /// Radius in document pixels used to detect corner and edge handles.
    const HANDLE_RADIUS_DOC: f32 = 6.0;

    /// Create a new free-transform tool with no active transform.
    pub fn new() -> Self {
        Self::default()
    }

    /// True if the tool has an active transform preview.
    pub fn is_active(&self) -> bool {
        self.layer.is_some() && self.start.is_some()
    }

    /// Mutable access to the sidebar-selected transform mode.
    pub fn mode_mut(&mut self) -> &mut FreeTransformMode {
        &mut self.transform_mode
    }

    /// Current sidebar-selected transform mode.
    pub fn mode(&self) -> FreeTransformMode {
        self.transform_mode
    }

    /// Compute the document-space bounds of the source layer.
    fn source_doc_bounds(&self, doc: &ogre_core::Document) -> Option<ogre_core::Rect> {
        let id = self.layer?;
        let layer = doc.layer(id).ok()?;
        let buffer = layer.buffer()?;
        let bounds = buffer.exact_bounds()?;
        Some(ogre_core::Rect::new(
            bounds.x + layer.offset.x,
            bounds.y + layer.offset.y,
            bounds.w,
            bounds.h,
        ))
    }

    /// Build a translate affine from the drag delta.
    fn update_translate(&mut self) {
        let Some(start) = self.start else { return };
        let delta = self.current.as_vec2() - start.as_vec2();
        self.affine = Affine2::from_translation(delta);
    }

    /// Build a uniform scale affine around `center` from the drag distance.
    fn update_scale(&mut self) {
        let Some(start) = self.start else { return };
        let start_vec = start.as_vec2();
        let start_dist = (start_vec - self.center).length();
        if start_dist < 1e-4 {
            self.affine = Affine2::IDENTITY;
            return;
        }
        let current_dist = (self.current.as_vec2() - self.center).length();
        let scale = current_dist / start_dist;
        self.affine = scale_around(self.center, scale);
    }

    /// Build a rotation affine around `center` from the angle delta.
    fn update_rotate(&mut self) {
        let current_vec = self.current.as_vec2() - self.center;
        if current_vec.length() < 1e-4 {
            self.affine = Affine2::IDENTITY;
            return;
        }
        let current_angle = current_vec.y.atan2(current_vec.x);
        let delta = current_angle - self.start_angle;
        self.affine = rotate_around(self.center, delta);
    }

    /// Build a skew affine around `center` from the drag delta.
    ///
    /// The shear coefficient is computed so that the dragged handle moves by
    /// exactly the pointer delta: for horizontal shear the displacement is
    /// proportional to the handle's vertical offset from the centre, and vice
    /// versa.
    fn update_skew(&mut self, axis: Axis) {
        let start_vec = self.start.map(|s| s.as_vec2()).unwrap_or(self.center);
        let current_vec = self.current.as_vec2();
        let delta = current_vec - start_vec;

        let relative = start_vec - self.center;
        let denom = match axis {
            Axis::X => relative.y,
            Axis::Y => relative.x,
        };

        let coeff = if denom.abs() < 1e-4 {
            0.0
        } else {
            match axis {
                Axis::X => delta.x / denom,
                Axis::Y => delta.y / denom,
            }
        };
        // Clamp to a reasonable range to keep the transform numerically stable.
        let coeff = coeff.clamp(-2.0, 2.0);
        self.affine = skew_around(self.center, axis, coeff);
    }

    /// Return the integer translation if `affine` is a pure integer
    /// translation, otherwise `None`.
    fn extract_integer_translation(affine: Affine2) -> Option<IVec2> {
        let eps = 1e-4;
        let m = affine.matrix2;
        if (m.x_axis.x - 1.0).abs() > eps
            || (m.y_axis.y - 1.0).abs() > eps
            || m.x_axis.y.abs() > eps
            || m.y_axis.x.abs() > eps
        {
            return None;
        }
        let t = affine.translation;
        // Beyond this magnitude f32 cannot represent every integer, so a
        // non-integer translation could round to an integer. Fall back to the
        // resampling path for very large translations.
        const EXACT_INT_LIMIT: f32 = 16_777_216.0; // 2^24
        if t.x.abs() >= EXACT_INT_LIMIT || t.y.abs() >= EXACT_INT_LIMIT {
            return None;
        }
        let dx = t.x.round();
        let dy = t.y.round();
        if (t.x - dx).abs() > eps || (t.y - dy).abs() > eps {
            return None;
        }
        let dx = i32::try_from(dx as i64).ok()?;
        let dy = i32::try_from(dy as i64).ok()?;
        Some(IVec2::new(dx, dy))
    }

    /// Initialise src/dst quads and warp grid from document-space bounds.
    fn init_handles(&mut self, doc_bounds: ogre_core::Rect) {
        let left = doc_bounds.x;
        let top = doc_bounds.y;
        let right = i32::try_from(doc_bounds.right()).unwrap_or(i32::MAX);
        let bottom = i32::try_from(doc_bounds.bottom()).unwrap_or(i32::MAX);
        self.src_quad = [
            IVec2::new(left, top),
            IVec2::new(right, top),
            IVec2::new(right, bottom),
            IVec2::new(left, bottom),
        ];
        self.dst_quad = self.src_quad;
        self.warp_grid = ogre_core::resample::WarpGrid::identity(doc_bounds, 4, 4);
    }

    /// Find the nearest corner handle within `radius` document pixels.
    fn nearest_corner(&self, pos: IVec2, radius: f32) -> Option<usize> {
        let pos_f = pos.as_vec2();
        let mut best = None;
        let mut best_d2 = radius * radius;
        for (i, c) in self.dst_quad.iter().enumerate() {
            let d2 = (pos_f - c.as_vec2()).length_squared();
            if d2 < best_d2 {
                best_d2 = d2;
                best = Some(i);
            }
        }
        best
    }

    /// Find the nearest warp-grid point within `radius` document pixels.
    fn nearest_grid_point(&self, pos: IVec2, radius: f32) -> Option<usize> {
        let pos_f = pos.as_vec2();
        let mut best = None;
        let mut best_d2 = radius * radius;
        for (i, p) in self.warp_grid.points.iter().enumerate() {
            let d2 = (pos_f - p.as_vec2()).length_squared();
            if d2 < best_d2 {
                best_d2 = d2;
                best = Some(i);
            }
        }
        best
    }

    /// Update the dragged handle to the current pointer position.
    fn update_handle(&mut self, pos: IVec2) {
        match self.handle {
            Some(Handle::Corner(i)) => {
                self.dst_quad[i] = pos;
            }
            Some(Handle::GridPoint(i)) => {
                self.warp_grid.points[i] = pos;
            }
            None => {}
        }
    }

    /// Reset all preview state except the sidebar-selected mode.
    fn reset_preview(&mut self) {
        self.layer = None;
        self.start = None;
        self.current = IVec2::ZERO;
        self.affine = Affine2::IDENTITY;
        self.pointer_mode = PointerMode::Translate;
        self.handle = None;
    }
}

impl Tool for FreeTransformTool {
    fn on_pointer(
        &mut self,
        doc: &ogre_core::Document,
        ev: PointerEvent,
    ) -> Option<Box<dyn ogre_core::Command>> {
        match ev.phase {
            Phase::Down => {
                let id = doc.active?;
                let layer = doc.layer(id).ok()?;
                if !layer.is_raster() || layer.locked {
                    return None;
                }

                self.layer = Some(id);
                self.start = Some(ev.doc_pos);
                self.current = ev.doc_pos;
                self.affine = Affine2::IDENTITY;
                self.handle = None;

                if let Some(bounds) = layer.buffer().and_then(|b| b.exact_bounds()) {
                    let min = IVec2::new(bounds.x, bounds.y) + layer.offset;
                    let doc_bounds = ogre_core::Rect::new(min.x, min.y, bounds.w, bounds.h);
                    self.init_handles(doc_bounds);

                    // Use the half-open geometric centre so it matches the
                    // wireframe corners drawn by `draw_overlay`.
                    self.center = min.as_vec2() + Vec2::new(bounds.w as f32, bounds.h as f32) * 0.5;

                    let start_vec = ev.doc_pos.as_vec2();
                    let relative = start_vec - self.center;
                    self.start_angle = relative.y.atan2(relative.x);

                    match self.transform_mode {
                        FreeTransformMode::ScaleRotate => {
                            self.pointer_mode =
                                classify_pointer(doc_bounds, ev.doc_pos, Self::HANDLE_RADIUS_DOC);
                        }
                        FreeTransformMode::Skew => {
                            self.pointer_mode =
                                classify_skew(doc_bounds, ev.doc_pos, Self::HANDLE_RADIUS_DOC);
                        }
                        FreeTransformMode::Distort | FreeTransformMode::Perspective => {
                            if let Some(i) =
                                self.nearest_corner(ev.doc_pos, Self::HANDLE_RADIUS_DOC)
                            {
                                self.handle = Some(Handle::Corner(i));
                            }
                        }
                        FreeTransformMode::Warp => {
                            if let Some(i) =
                                self.nearest_grid_point(ev.doc_pos, Self::HANDLE_RADIUS_DOC)
                            {
                                self.handle = Some(Handle::GridPoint(i));
                            }
                        }
                    }
                } else {
                    self.center = layer.offset.as_vec2();
                    self.start_angle = 0.0;
                    self.pointer_mode = PointerMode::Translate;
                }

                None
            }
            Phase::Drag => {
                self.current = ev.doc_pos;
                match self.transform_mode {
                    FreeTransformMode::ScaleRotate | FreeTransformMode::Skew => {
                        match self.pointer_mode {
                            PointerMode::Translate => self.update_translate(),
                            PointerMode::Scale => self.update_scale(),
                            PointerMode::Rotate => self.update_rotate(),
                            PointerMode::Skew { axis } => self.update_skew(axis),
                        }
                    }
                    FreeTransformMode::Distort
                    | FreeTransformMode::Perspective
                    | FreeTransformMode::Warp => {
                        self.update_handle(ev.doc_pos);
                    }
                }
                None
            }
            Phase::Up => {
                // Free transform commits on Enter, not on pointer up.
                self.current = ev.doc_pos;
                match self.transform_mode {
                    FreeTransformMode::ScaleRotate | FreeTransformMode::Skew => {
                        match self.pointer_mode {
                            PointerMode::Translate => self.update_translate(),
                            PointerMode::Scale => self.update_scale(),
                            PointerMode::Rotate => self.update_rotate(),
                            PointerMode::Skew { axis } => self.update_skew(axis),
                        }
                    }
                    FreeTransformMode::Distort
                    | FreeTransformMode::Perspective
                    | FreeTransformMode::Warp => {
                        self.update_handle(ev.doc_pos);
                    }
                }
                None
            }
        }
    }

    fn commit(&mut self, doc: &ogre_core::Document) -> Option<Box<dyn ogre_core::Command>> {
        let layer = self.layer.take()?;
        self.start = None;
        self.handle = None;

        let layer_ref = doc.layer(layer).ok()?;
        if !layer_ref.is_raster() || layer_ref.locked {
            self.reset_preview();
            return None;
        }

        let cmd: Box<dyn ogre_core::Command> = match self.transform_mode {
            FreeTransformMode::ScaleRotate | FreeTransformMode::Skew => {
                let affine = self.affine;
                self.affine = Affine2::IDENTITY;

                if !affine.is_finite() || affine.matrix2.determinant().abs() <= 1e-6 {
                    return None;
                }

                if let Some(delta) = Self::extract_integer_translation(affine) {
                    if delta != IVec2::ZERO {
                        return Some(Box::new(ogre_core::MoveLayerByCmd::new(layer, delta)));
                    }
                    return None;
                }

                Box::new(ogre_core::TransformLayerCmd::new(layer, affine))
            }
            FreeTransformMode::Distort | FreeTransformMode::Perspective => {
                let quad = self.dst_quad;
                self.dst_quad = self.src_quad;
                Box::new(ogre_core::TransformLayerCmd::new_perspective(layer, quad))
            }
            FreeTransformMode::Warp => {
                let grid = self.warp_grid.clone();
                if let Some(bounds) = layer_ref.buffer().and_then(|b| b.exact_bounds()) {
                    let min = IVec2::new(bounds.x, bounds.y) + layer_ref.offset;
                    let doc_bounds = ogre_core::Rect::new(min.x, min.y, bounds.w, bounds.h);
                    self.warp_grid = ogre_core::resample::WarpGrid::identity(doc_bounds, 4, 4);
                }
                Box::new(ogre_core::TransformLayerCmd::new_warp(layer, grid))
            }
        };

        Some(cmd)
    }

    fn draw_overlay(
        &self,
        painter: &egui::Painter,
        doc: &ogre_core::Document,
        viewport: &ogre_gpu::Viewport,
        canvas_rect: EguiRect,
        pixels_per_point: f32,
        _time: f64,
    ) {
        let Some(bounds) = self.source_doc_bounds(doc) else {
            return;
        };

        let stroke = Stroke::new(1.0, egui::Color32::WHITE);
        let handle_size = 3.0f32;

        match self.transform_mode {
            FreeTransformMode::ScaleRotate | FreeTransformMode::Skew => {
                // Transform the four half-open corners of the source bounds.
                let left = bounds.x as f32;
                let top = bounds.y as f32;
                let right = left + bounds.w as f32;
                let bottom = top + bounds.h as f32;
                let corners = [
                    Vec2::new(left, top),
                    Vec2::new(right, top),
                    Vec2::new(right, bottom),
                    Vec2::new(left, bottom),
                ];

                let mut screen_corners = [Pos2::ZERO; 4];
                for (i, c) in corners.iter().enumerate() {
                    let dst = self.affine.transform_point2(*c);
                    let screen = doc_to_screen_f(dst, viewport, pixels_per_point)
                        + Vec2::new(canvas_rect.min.x, canvas_rect.min.y);
                    screen_corners[i] = Pos2::new(screen.x, screen.y);
                }

                for i in 0..4 {
                    painter.line_segment([screen_corners[i], screen_corners[(i + 1) % 4]], stroke);
                }

                // Compute transformed edge midpoints for skew handles.
                let midpoints = [
                    Vec2::new((left + right) * 0.5, top),    // top
                    Vec2::new((left + right) * 0.5, bottom), // bottom
                    Vec2::new(left, (top + bottom) * 0.5),   // left
                    Vec2::new(right, (top + bottom) * 0.5),  // right
                ];

                // Draw small handle squares at the corners (scale handles).
                for c in screen_corners {
                    draw_handle_square(painter, c, handle_size, stroke);
                }

                // Draw small handle squares at the edge midpoints (skew handles).
                for m in midpoints {
                    let dst = self.affine.transform_point2(m);
                    let screen = doc_to_screen_f(dst, viewport, pixels_per_point)
                        + Vec2::new(canvas_rect.min.x, canvas_rect.min.y);
                    draw_handle_square(painter, Pos2::new(screen.x, screen.y), handle_size, stroke);
                }

                // In rotate mode, draw a line from the transformed centre to the pointer.
                if self.pointer_mode == PointerMode::Rotate {
                    let transformed_center = self.affine.transform_point2(self.center);
                    let center_screen =
                        doc_to_screen_f(transformed_center, viewport, pixels_per_point)
                            + Vec2::new(canvas_rect.min.x, canvas_rect.min.y);
                    let current_screen =
                        doc_to_screen_f(self.current.as_vec2(), viewport, pixels_per_point)
                            + Vec2::new(canvas_rect.min.x, canvas_rect.min.y);
                    painter.line_segment(
                        [
                            Pos2::new(center_screen.x, center_screen.y),
                            Pos2::new(current_screen.x, current_screen.y),
                        ],
                        stroke,
                    );
                }
            }
            FreeTransformMode::Distort | FreeTransformMode::Perspective => {
                let mut screen_corners = [Pos2::ZERO; 4];
                for (i, c) in self.dst_quad.iter().enumerate() {
                    let screen = doc_to_screen_f(c.as_vec2(), viewport, pixels_per_point)
                        + Vec2::new(canvas_rect.min.x, canvas_rect.min.y);
                    screen_corners[i] = Pos2::new(screen.x, screen.y);
                }
                for i in 0..4 {
                    painter.line_segment([screen_corners[i], screen_corners[(i + 1) % 4]], stroke);
                }
                for c in screen_corners {
                    draw_handle_square(painter, c, handle_size, stroke);
                }
            }
            FreeTransformMode::Warp => {
                draw_warp_grid(
                    painter,
                    &self.warp_grid,
                    viewport,
                    canvas_rect,
                    pixels_per_point,
                    stroke,
                    handle_size,
                );
            }
        }
    }

    fn cancel(&mut self) {
        self.reset_preview();
    }
}

/// Numeric affine parameters (ScaleRotate mode only) for the sidebar fields.
///
/// Decomposed from / recomposed onto the preview affine around the layer
/// centre: `translate` is the net centre displacement in document pixels,
/// `scale` is a uniform multiplier, and `rotation_deg` is clockwise.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TransformParams {
    /// Net X translation of the layer centre, document pixels.
    pub translate_x: f32,
    /// Net Y translation of the layer centre, document pixels.
    pub translate_y: f32,
    /// Uniform scale multiplier (`1.0` = unchanged).
    pub scale: f32,
    /// Clockwise rotation in degrees.
    pub rotation_deg: f32,
}

impl Default for TransformParams {
    fn default() -> Self {
        Self {
            translate_x: 0.0,
            translate_y: 0.0,
            scale: 1.0,
            rotation_deg: 0.0,
        }
    }
}

impl FreeTransformTool {
    /// The numeric affine parameters of the current preview, or `None` if no
    /// transform is in progress. Exact only in ScaleRotate mode (no skew).
    pub fn numeric_params(&self) -> Option<TransformParams> {
        self.layer?;
        let center = self.center;
        let translated_center = self.affine.transform_point2(center);
        let translate = translated_center - center;
        // Transform a unit X vector from the centre to recover scale + rotation.
        let v = self.affine.transform_point2(center + Vec2::X) - translated_center;
        let scale = v.length();
        let rotation_deg = v.to_angle().to_degrees();
        Some(TransformParams {
            translate_x: translate.x,
            translate_y: translate.y,
            scale,
            rotation_deg,
        })
    }

    /// Rebuild the preview affine from numeric parameters (ScaleRotate mode).
    /// Recomposes as translate · rotate · scale around the layer centre.
    pub fn set_numeric_params(&mut self, p: TransformParams) {
        if self.layer.is_none() {
            return;
        }
        let center = self.center;
        let translate = Vec2::new(p.translate_x, p.translate_y);
        let scale = p.scale.max(0.001); // guard against zero/negative
        let rot = p.rotation_deg.to_radians();
        self.affine = Affine2::from_translation(translate + center)
            * Affine2::from_angle(rot)
            * Affine2::from_scale(Vec2::splat(scale))
            * Affine2::from_translation(-center);
    }
}

/// Build an affine that scales uniformly around `center` by `scale`.
fn scale_around(center: Vec2, scale: f32) -> Affine2 {
    Affine2::from_translation(center)
        * Affine2::from_scale(Vec2::splat(scale))
        * Affine2::from_translation(-center)
}

/// Build an affine that rotates around `center` by `angle` radians.
fn rotate_around(center: Vec2, angle: f32) -> Affine2 {
    Affine2::from_translation(center)
        * Affine2::from_angle(angle)
        * Affine2::from_translation(-center)
}

/// Build an affine that skews around `center` along `axis` by `coeff`.
///
/// `coeff` is the tangent of the shear angle: for `Axis::X`, `y` is unchanged
/// and `x' = x + coeff * y`; for `Axis::Y`, `x` is unchanged and
/// `y' = y + coeff * x`.
fn skew_around(center: Vec2, axis: Axis, coeff: f32) -> Affine2 {
    let shear = match axis {
        Axis::X => Affine2::from_mat2(glam::Mat2::from_cols(
            Vec2::new(1.0, 0.0),
            Vec2::new(coeff, 1.0),
        )),
        Axis::Y => Affine2::from_mat2(glam::Mat2::from_cols(
            Vec2::new(1.0, coeff),
            Vec2::new(0.0, 1.0),
        )),
    };
    Affine2::from_translation(center) * shear * Affine2::from_translation(-center)
}

/// Classify a pointer position relative to a document bounds rectangle.
///
/// - Inside bounds -> Translate.
/// - Within `handle_radius` of a corner -> Scale.
/// - Within `handle_radius` of an edge midpoint -> Skew along that edge.
/// - Otherwise -> Rotate.
fn classify_pointer(bounds: ogre_core::Rect, pos: IVec2, handle_radius: f32) -> PointerMode {
    let pos_f = pos.as_vec2();
    let min = Vec2::new(bounds.x as f32, bounds.y as f32);
    // Use the half-open corner for handle placement to match the wireframe
    // drawn by `draw_overlay`.
    let max = Vec2::new(
        (bounds.x + bounds.w as i32) as f32,
        (bounds.y + bounds.h as i32) as f32,
    );

    if bounds.contains(pos) {
        return PointerMode::Translate;
    }

    let corners = [min, Vec2::new(max.x, min.y), max, Vec2::new(min.x, max.y)];
    for c in corners {
        if (pos_f - c).length() <= handle_radius {
            return PointerMode::Scale;
        }
    }

    let midpoints = [
        (Vec2::new((min.x + max.x) * 0.5, min.y), Axis::X), // top
        (Vec2::new((min.x + max.x) * 0.5, max.y), Axis::X), // bottom
        (Vec2::new(min.x, (min.y + max.y) * 0.5), Axis::Y), // left
        (Vec2::new(max.x, (min.y + max.y) * 0.5), Axis::Y), // right
    ];
    for (m, axis) in midpoints {
        if (pos_f - m).length() <= handle_radius {
            return PointerMode::Skew { axis };
        }
    }

    PointerMode::Rotate
}

/// Classify a pointer for Skew mode: inside bounds translates, near an edge
/// midpoint skews; otherwise defaults to horizontal/vertical skew based on
/// closest edge.
fn classify_skew(bounds: ogre_core::Rect, pos: IVec2, handle_radius: f32) -> PointerMode {
    let pos_f = pos.as_vec2();
    let min = Vec2::new(bounds.x as f32, bounds.y as f32);
    let max = Vec2::new(
        (bounds.x + bounds.w as i32) as f32,
        (bounds.y + bounds.h as i32) as f32,
    );

    if bounds.contains(pos) {
        return PointerMode::Translate;
    }

    let midpoints = [
        (Vec2::new((min.x + max.x) * 0.5, min.y), Axis::X), // top
        (Vec2::new((min.x + max.x) * 0.5, max.y), Axis::X), // bottom
        (Vec2::new(min.x, (min.y + max.y) * 0.5), Axis::Y), // left
        (Vec2::new(max.x, (min.y + max.y) * 0.5), Axis::Y), // right
    ];
    for (m, axis) in midpoints {
        if (pos_f - m).length() <= handle_radius {
            return PointerMode::Skew { axis };
        }
    }

    // Default to the nearest edge midpoint.
    let mut best = None;
    let mut best_d2 = f32::INFINITY;
    for (m, axis) in midpoints {
        let d2 = (pos_f - m).length_squared();
        if d2 < best_d2 {
            best_d2 = d2;
            best = Some(PointerMode::Skew { axis });
        }
    }
    best.unwrap_or(PointerMode::Translate)
}

/// Draw a warp grid as a lattice of lines with handles at control points.
fn draw_warp_grid(
    painter: &egui::Painter,
    grid: &ogre_core::resample::WarpGrid,
    viewport: &ogre_gpu::Viewport,
    canvas_rect: EguiRect,
    pixels_per_point: f32,
    stroke: Stroke,
    handle_size: f32,
) {
    let cols = grid.cols;
    let rows = grid.rows;

    // Horizontal lines.
    for r in 0..rows {
        for c in 1..cols {
            let a = doc_to_screen_f(
                grid.points[r * cols + (c - 1)].as_vec2(),
                viewport,
                pixels_per_point,
            ) + Vec2::new(canvas_rect.min.x, canvas_rect.min.y);
            let b = doc_to_screen_f(
                grid.points[r * cols + c].as_vec2(),
                viewport,
                pixels_per_point,
            ) + Vec2::new(canvas_rect.min.x, canvas_rect.min.y);
            painter.line_segment([Pos2::new(a.x, a.y), Pos2::new(b.x, b.y)], stroke);
        }
    }
    // Vertical lines.
    for c in 0..cols {
        for r in 1..rows {
            let a = doc_to_screen_f(
                grid.points[(r - 1) * cols + c].as_vec2(),
                viewport,
                pixels_per_point,
            ) + Vec2::new(canvas_rect.min.x, canvas_rect.min.y);
            let b = doc_to_screen_f(
                grid.points[r * cols + c].as_vec2(),
                viewport,
                pixels_per_point,
            ) + Vec2::new(canvas_rect.min.x, canvas_rect.min.y);
            painter.line_segment([Pos2::new(a.x, a.y), Pos2::new(b.x, b.y)], stroke);
        }
    }

    // Handles.
    for p in &grid.points {
        let s = doc_to_screen_f(p.as_vec2(), viewport, pixels_per_point)
            + Vec2::new(canvas_rect.min.x, canvas_rect.min.y);
        draw_handle_square(painter, Pos2::new(s.x, s.y), handle_size, stroke);
    }
}

/// Map a document-space rectangle to an on-screen egui rectangle.
fn selection_screen_rect(
    rect: ogre_core::Rect,
    viewport: &ogre_gpu::Viewport,
    canvas_rect: EguiRect,
    pixels_per_point: f32,
) -> Option<EguiRect> {
    if rect.is_empty() {
        return None;
    }
    let min = doc_to_screen(IVec2::new(rect.x, rect.y), viewport, pixels_per_point)
        + Vec2::new(canvas_rect.min.x, canvas_rect.min.y);
    let max_x = i32::try_from(rect.right()).unwrap_or(i32::MAX);
    let max_y = i32::try_from(rect.bottom()).unwrap_or(i32::MAX);
    let max = doc_to_screen(IVec2::new(max_x, max_y), viewport, pixels_per_point)
        + Vec2::new(canvas_rect.min.x, canvas_rect.min.y);
    Some(EguiRect::from_min_max(
        Pos2::new(min.x, min.y),
        Pos2::new(max.x, max.y),
    ))
}

/// Draw a simple wireframe rectangle.
fn draw_wireframe(painter: &egui::Painter, rect: EguiRect) {
    let stroke = Stroke::new(1.0, egui::Color32::WHITE);
    let tl = rect.left_top();
    let tr = rect.right_top();
    let br = rect.right_bottom();
    let bl = rect.left_bottom();
    painter.line_segment([tl, tr], stroke);
    painter.line_segment([tr, br], stroke);
    painter.line_segment([br, bl], stroke);
    painter.line_segment([bl, tl], stroke);
}

/// Draw a small square handle centred at `pos` with half-size `size`.
fn draw_handle_square(painter: &egui::Painter, pos: Pos2, size: f32, stroke: Stroke) {
    let r = EguiRect::from_center_size(pos, egui::vec2(size * 2.0, size * 2.0));
    let tl = r.left_top();
    let tr = r.right_top();
    let br = r.right_bottom();
    let bl = r.left_bottom();
    painter.line_segment([tl, tr], stroke);
    painter.line_segment([tr, br], stroke);
    painter.line_segment([br, bl], stroke);
    painter.line_segment([bl, tl], stroke);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch;
    use crate::state::AppState;
    use crate::tools::ToolKind;
    use ogre_core::{IVec2, Rgba32F};

    #[test]
    fn numeric_params_round_trip_through_set() {
        let mut tool = FreeTransformTool::new();
        // Simulate a transform-in-progress by setting a layer + centre.
        tool.layer = Some(ogre_core::LayerId::default());
        tool.center = Vec2::new(50.0, 50.0);
        let p = TransformParams {
            translate_x: 12.0,
            translate_y: -8.0,
            scale: 1.5,
            rotation_deg: 30.0,
        };
        tool.set_numeric_params(p);
        let back = tool.numeric_params().expect("transform in progress");
        assert!((back.translate_x - 12.0).abs() < 1e-3);
        assert!((back.translate_y - (-8.0)).abs() < 1e-3);
        assert!((back.scale - 1.5).abs() < 1e-3);
        assert!((back.rotation_deg - 30.0).abs() < 1e-2);
    }

    #[test]
    fn numeric_params_none_without_active_transform() {
        let tool = FreeTransformTool::new();
        assert!(tool.numeric_params().is_none());
    }

    fn send(
        manager: &mut crate::tools::ToolManager,
        doc: &ogre_core::Document,
        pos: IVec2,
        phase: Phase,
    ) -> Option<Box<dyn ogre_core::Command>> {
        manager.on_pointer(doc, PointerEvent::new(pos, phase, egui::Modifiers::NONE))
    }

    fn commit(
        manager: &mut crate::tools::ToolManager,
        doc: &ogre_core::Document,
    ) -> Option<Box<dyn ogre_core::Command>> {
        manager.commit_active(doc)
    }

    #[test]
    fn move_tool_drag_commits_move_layer_by() {
        let mut state = AppState::new_document(200, 200);
        let id = state.doc().active.unwrap();
        state.tool_manager.set_tool(ToolKind::Move);

        assert!(send(
            &mut state.tool_manager,
            &state.tabs[state.active_tab].doc,
            IVec2::new(10, 10),
            Phase::Down
        )
        .is_none());
        assert!(send(
            &mut state.tool_manager,
            &state.tabs[state.active_tab].doc,
            IVec2::new(25, 30),
            Phase::Drag
        )
        .is_none());
        let cmd = send(
            &mut state.tool_manager,
            &state.tabs[state.active_tab].doc,
            IVec2::new(25, 30),
            Phase::Up,
        )
        .unwrap();
        dispatch::dispatch(&mut state, cmd).unwrap();

        assert_eq!(state.doc().layer(id).unwrap().offset, IVec2::new(15, 20));
        assert_eq!(state.history().undo_len(), 1);
    }

    #[test]
    fn move_tool_zero_drag_returns_no_command() {
        let mut state = AppState::new_document(200, 200);
        state.tool_manager.set_tool(ToolKind::Move);

        send(
            &mut state.tool_manager,
            &state.tabs[state.active_tab].doc,
            IVec2::new(10, 10),
            Phase::Down,
        );
        let cmd = send(
            &mut state.tool_manager,
            &state.tabs[state.active_tab].doc,
            IVec2::new(10, 10),
            Phase::Up,
        );
        assert!(cmd.is_none());
        assert_eq!(state.history().undo_len(), 0);
    }

    fn fill_red_block(doc: &mut ogre_core::Document, id: ogre_core::LayerId) {
        let buffer = doc.layer_mut(id).unwrap().buffer_mut().unwrap();
        for y in 0..5 {
            for x in 0..5 {
                buffer.set_pixel(IVec2::new(x, y), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
            }
        }
    }

    fn fill_red_rect(
        doc: &mut ogre_core::Document,
        id: ogre_core::LayerId,
        width: i32,
        height: i32,
    ) {
        let buffer = doc.layer_mut(id).unwrap().buffer_mut().unwrap();
        for y in 0..height {
            for x in 0..width {
                buffer.set_pixel(IVec2::new(x, y), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
            }
        }
    }

    #[test]
    fn free_transform_tool_commit_applies_affine() {
        let mut state = AppState::new_document(200, 200);
        let id = state.doc().active.unwrap();
        // Use a 6x6 block so the half-open geometric centre is an integer
        // coordinate and uniform scaling is easy to verify.
        fill_red_rect(&mut state.tabs[state.active_tab].doc, id, 6, 6);
        state.tool_manager.set_tool(ToolKind::FreeTransform);

        // Layer bounds are half-open (0,0)-(6,6); centre is (3,3). Start at (7,3)
        // distance 4 from centre, drag to (11,3) distance 8 -> uniform scale 2x
        // about centre.
        send(
            &mut state.tool_manager,
            &state.tabs[state.active_tab].doc,
            IVec2::new(7, 3),
            Phase::Down,
        );
        send(
            &mut state.tool_manager,
            &state.tabs[state.active_tab].doc,
            IVec2::new(11, 3),
            Phase::Drag,
        );
        let cmd = commit(&mut state.tool_manager, &state.tabs[state.active_tab].doc).unwrap();
        dispatch::dispatch(&mut state, cmd).unwrap();

        let layer = state.doc().layer(id).unwrap();
        // Source corner (0,0) maps to (-3,-3); new offset is floor of that.
        assert_eq!(layer.offset, IVec2::new(-3, -3));
        // Source (3,3) is the centre of the 6x6 block; scaling about that centre
        // maps it to dest doc (3,3), which with offset (-3,-3) is local (6,6).
        assert_eq!(
            layer.buffer().unwrap().get_pixel(IVec2::new(6, 6)),
            Rgba32F::new(1.0, 0.0, 0.0, 1.0)
        );
        // Bounds grew substantially (the 6x6 interior scaled to 12x12).
        let bounds = layer.buffer().unwrap().exact_bounds().unwrap();
        assert!(bounds.w >= 12 && bounds.h >= 12);
    }

    #[test]
    fn free_transform_tool_pure_translation_uses_move_layer_by() {
        let mut state = AppState::new_document(200, 200);
        let id = state.doc().active.unwrap();
        fill_red_block(&mut state.tabs[state.active_tab].doc, id);
        state.tool_manager.set_tool(ToolKind::FreeTransform);

        // Start inside the bounds and drag by an integer vector.
        send(
            &mut state.tool_manager,
            &state.tabs[state.active_tab].doc,
            IVec2::new(0, 0),
            Phase::Down,
        );
        send(
            &mut state.tool_manager,
            &state.tabs[state.active_tab].doc,
            IVec2::new(5, 7),
            Phase::Drag,
        );
        let cmd = commit(&mut state.tool_manager, &state.tabs[state.active_tab].doc).unwrap();
        dispatch::dispatch(&mut state, cmd).unwrap();

        assert_eq!(state.doc().layer(id).unwrap().offset, IVec2::new(5, 7));
        assert_eq!(state.history().undo_len(), 1);
        // Pixels must not have been resampled.
        assert_eq!(
            state
                .doc()
                .layer(id)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(0, 0)),
            Rgba32F::new(1.0, 0.0, 0.0, 1.0)
        );
    }

    #[test]
    fn free_transform_tool_rotate_applies_affine() {
        let mut state = AppState::new_document(200, 200);
        let id = state.doc().active.unwrap();
        // Use a 20x20 block so the half-open centre is an integer coordinate and
        // a 180° rotation lands cleanly on integer pixels.
        fill_red_rect(&mut state.tabs[state.active_tab].doc, id, 20, 20);
        state.tool_manager.set_tool(ToolKind::FreeTransform);

        // Layer bounds are half-open (0,0)-(20,20), centre (10,10). (30,15) is
        // outside and well away from any handle, so it triggers rotate. Drag to
        // (-10,5) for a 180° rotation around the centre.
        send(
            &mut state.tool_manager,
            &state.tabs[state.active_tab].doc,
            IVec2::new(30, 15),
            Phase::Down,
        );
        send(
            &mut state.tool_manager,
            &state.tabs[state.active_tab].doc,
            IVec2::new(-10, 5),
            Phase::Drag,
        );
        let cmd = commit(&mut state.tool_manager, &state.tabs[state.active_tab].doc).unwrap();
        dispatch::dispatch(&mut state, cmd).unwrap();

        let layer = state.doc().layer(id).unwrap();
        // A 180° rotation about the half-open centre maps the bounds onto
        // itself.  The tiny f32 rounding pulls the lower-right corner just
        // below 0, so the floored offset is (-1, 0).
        assert_eq!(layer.offset, IVec2::new(-1, 0));
        // Source centre (10,10) maps to itself; with offset (-1,0) that is
        // local (11,10).
        assert_eq!(
            layer.buffer().unwrap().get_pixel(IVec2::new(11, 10)),
            Rgba32F::new(1.0, 0.0, 0.0, 1.0)
        );
    }

    #[test]
    fn free_transform_tool_skew_applies_affine() {
        let mut state = AppState::new_document(200, 200);
        let id = state.doc().active.unwrap();
        // Use a larger block so the right-edge skew handle is reachable without
        // overlapping a corner handle at the 6-pixel radius.
        fill_red_rect(&mut state.tabs[state.active_tab].doc, id, 20, 20);
        state.tool_manager.set_tool(ToolKind::FreeTransform);

        // Layer bounds are half-open (0,0)-(20,20), centre (10,10). (26,10) is
        // within 6 pixels of the right edge midpoint (20,10) and well away
        // from the corners, so it triggers skew Y. Drag down by 2 pixels.
        send(
            &mut state.tool_manager,
            &state.tabs[state.active_tab].doc,
            IVec2::new(26, 10),
            Phase::Down,
        );
        send(
            &mut state.tool_manager,
            &state.tabs[state.active_tab].doc,
            IVec2::new(26, 12),
            Phase::Drag,
        );
        let cmd = commit(&mut state.tool_manager, &state.tabs[state.active_tab].doc).unwrap();
        dispatch::dispatch(&mut state, cmd).unwrap();

        let layer = state.doc().layer(id).unwrap();
        // Start is 16 pixels from the centre; dragging down 2 pixels gives a
        // Y-skew coefficient of 2/16 = 0.125.  The top-left corner shifts up by
        // coeff * 10 = 1.25, so the floored offset is (0, -2).
        assert_eq!(layer.offset, IVec2::new(0, -2));
        let bounds = layer.buffer().unwrap().exact_bounds().unwrap();
        assert!(bounds.h > 20, "skew should increase the occupied height");
        // The bottom-right source corner (20,0) maps to (18.77, 0); the
        // top-right corner (20,20) maps to (21.35, 20).  Some of the skewed
        // interior should still be fully red near the original top-left area.
        let px = layer.buffer().unwrap().get_pixel(IVec2::new(2, 2));
        assert!(
            px.r > 0.9 && px.a > 0.9,
            "expected a solid red pixel near the transformed top-left, got {:?}",
            px
        );
    }

    #[test]
    fn move_tool_locked_layer_returns_no_command() {
        let mut state = AppState::new_document(200, 200);
        let id = state.doc().active.unwrap();
        state.doc_mut().layer_mut(id).unwrap().locked = true;
        state.tool_manager.set_tool(ToolKind::Move);

        send(
            &mut state.tool_manager,
            &state.tabs[state.active_tab].doc,
            IVec2::new(10, 10),
            Phase::Down,
        );
        let cmd = send(
            &mut state.tool_manager,
            &state.tabs[state.active_tab].doc,
            IVec2::new(20, 25),
            Phase::Up,
        );
        assert!(cmd.is_none());
        assert_eq!(state.history().undo_len(), 0);
    }

    #[test]
    fn move_tool_group_active_layer_returns_no_command() {
        let mut state = AppState::new_document(200, 200);
        let bg = state.doc().active.unwrap();
        let group = ogre_core::Layer::new_group("G");
        let group_id = state.doc_mut().insert_layer_above(group, bg).unwrap();
        state.doc_mut().active = Some(group_id);
        state.tool_manager.set_tool(ToolKind::Move);

        send(
            &mut state.tool_manager,
            &state.tabs[state.active_tab].doc,
            IVec2::new(10, 10),
            Phase::Down,
        );
        let cmd = send(
            &mut state.tool_manager,
            &state.tabs[state.active_tab].doc,
            IVec2::new(20, 25),
            Phase::Up,
        );
        assert!(cmd.is_none());
        assert_eq!(state.history().undo_len(), 0);
    }

    #[test]
    fn free_transform_tool_cancel_drops_pending() {
        let mut state = AppState::new_document(200, 200);
        let id = state.doc().active.unwrap();
        fill_red_block(&mut state.tabs[state.active_tab].doc, id);
        state.tool_manager.set_tool(ToolKind::FreeTransform);

        send(
            &mut state.tool_manager,
            &state.tabs[state.active_tab].doc,
            IVec2::new(10, 2),
            Phase::Down,
        );
        send(
            &mut state.tool_manager,
            &state.tabs[state.active_tab].doc,
            IVec2::new(12, 4),
            Phase::Drag,
        );

        // Switching tools cancels the in-progress transform.
        state.tool_manager.set_tool(ToolKind::RectSelect);
        let cmd = commit(&mut state.tool_manager, &state.tabs[state.active_tab].doc);
        assert!(cmd.is_none());
        assert_eq!(state.history().undo_len(), 0);
    }

    #[test]
    fn free_transform_tool_non_integer_translation_uses_transform_cmd() {
        use glam::Vec2;

        let mut state = AppState::new_document(200, 200);
        let id = state.doc().active.unwrap();
        fill_red_block(&mut state.tabs[state.active_tab].doc, id);

        // Build the tool directly and commit a non-integer translation.
        let mut tool = FreeTransformTool::new();
        tool.layer = Some(id);
        tool.start = Some(IVec2::ZERO);
        tool.affine = Affine2::from_translation(Vec2::new(5.5, 7.0));

        let cmd = Tool::commit(&mut tool, &state.tabs[state.active_tab].doc).unwrap();
        assert_eq!(cmd.label(), "Transform layer");
        dispatch::dispatch(&mut state, cmd).unwrap();

        let layer = state.doc().layer(id).unwrap();
        // Non-integer translation goes through resampling, not MoveLayerByCmd.
        assert_eq!(layer.offset, IVec2::new(5, 7));
        assert_eq!(state.history().undo_len(), 1);
        // The resampled buffer is at least as wide as the source after a
        // sub-pixel shift.
        let bounds = layer.buffer().unwrap().exact_bounds().unwrap();
        assert!(bounds.w >= 5);
    }

    #[test]
    fn free_transform_tool_invalid_affine_does_not_commit() {
        let mut state = AppState::new_document(200, 200);
        let id = state.doc().active.unwrap();
        fill_red_block(&mut state.tabs[state.active_tab].doc, id);

        let mut tool = FreeTransformTool::new();
        tool.layer = Some(id);
        tool.start = Some(IVec2::ZERO);

        // Non-finite matrix.
        let mut bad = Affine2::IDENTITY;
        bad.matrix2.x_axis.x = f32::NAN;
        tool.affine = bad;
        assert!(Tool::commit(&mut tool, &state.tabs[state.active_tab].doc).is_none());
        assert_eq!(state.history().undo_len(), 0);

        // Near-zero determinant.
        let mut tool = FreeTransformTool::new();
        tool.layer = Some(id);
        tool.start = Some(IVec2::ZERO);
        tool.affine = Affine2::from_scale(Vec2::new(1e-7, 1e-7));
        assert!(Tool::commit(&mut tool, &state.tabs[state.active_tab].doc).is_none());
        assert_eq!(state.history().undo_len(), 0);
    }

    #[test]
    fn free_transform_tool_commit_rejects_locked_layer() {
        let mut state = AppState::new_document(200, 200);
        let id = state.doc().active.unwrap();
        fill_red_block(&mut state.tabs[state.active_tab].doc, id);
        state.doc_mut().layer_mut(id).unwrap().locked = true;

        let mut tool = FreeTransformTool::new();
        tool.layer = Some(id);
        tool.start = Some(IVec2::ZERO);
        tool.affine = Affine2::from_translation(Vec2::new(5.0, 7.0));
        assert!(Tool::commit(&mut tool, &state.tabs[state.active_tab].doc).is_none());
        assert_eq!(state.history().undo_len(), 0);
    }

    #[test]
    fn free_transform_tool_commit_rejects_missing_layer() {
        let mut state = AppState::new_document(200, 200);
        let id = state.doc().active.unwrap();
        fill_red_block(&mut state.tabs[state.active_tab].doc, id);

        let mut tool = FreeTransformTool::new();
        // Use a layer id that was never assigned.
        tool.layer = Some(ogre_core::LayerId::default());
        tool.start = Some(IVec2::ZERO);
        tool.affine = Affine2::from_translation(Vec2::new(5.0, 7.0));
        assert!(Tool::commit(&mut tool, &state.tabs[state.active_tab].doc).is_none());
    }

    #[test]
    fn free_transform_tool_commit_rejects_non_raster_layer() {
        let mut state = AppState::new_document(200, 200);
        let raster = state.doc().active.unwrap();
        let group = ogre_core::Layer::new_group("group");
        let group_id = state.doc_mut().insert_layer_above(group, raster).unwrap();
        state.doc_mut().active = Some(group_id);

        let mut tool = FreeTransformTool::new();
        tool.layer = Some(group_id);
        tool.start = Some(IVec2::ZERO);
        tool.affine = Affine2::from_translation(Vec2::new(5.0, 7.0));
        assert!(Tool::commit(&mut tool, &state.tabs[state.active_tab].doc).is_none());
    }

    #[test]
    fn free_transform_tool_integer_translation_on_empty_layer_uses_move_layer_by() {
        let mut state = AppState::new_document(200, 200);
        let id = state.doc().active.unwrap();
        state.tool_manager.set_tool(ToolKind::FreeTransform);

        // Drag inside the empty layer bounds (0,0)-(0,0) by an integer vector.
        send(
            &mut state.tool_manager,
            &state.tabs[state.active_tab].doc,
            IVec2::new(0, 0),
            Phase::Down,
        );
        send(
            &mut state.tool_manager,
            &state.tabs[state.active_tab].doc,
            IVec2::new(3, 4),
            Phase::Drag,
        );
        let cmd = commit(&mut state.tool_manager, &state.tabs[state.active_tab].doc).unwrap();
        dispatch::dispatch(&mut state, cmd).unwrap();

        assert_eq!(state.doc().layer(id).unwrap().offset, IVec2::new(3, 4));
        assert_eq!(state.history().undo_len(), 1);
    }

    #[test]
    fn move_tool_undo_redo_restores_offset() {
        let mut state = AppState::new_document(200, 200);
        let id = state.doc().active.unwrap();
        state.tool_manager.set_tool(ToolKind::Move);

        send(
            &mut state.tool_manager,
            &state.tabs[state.active_tab].doc,
            IVec2::new(10, 10),
            Phase::Down,
        );
        let cmd = send(
            &mut state.tool_manager,
            &state.tabs[state.active_tab].doc,
            IVec2::new(25, 30),
            Phase::Up,
        )
        .unwrap();
        dispatch::dispatch(&mut state, cmd).unwrap();
        assert_eq!(state.doc().layer(id).unwrap().offset, IVec2::new(15, 20));

        let tab = state.current_tab_mut();
        tab.history.undo(&mut tab.doc);
        assert_eq!(state.doc().layer(id).unwrap().offset, IVec2::ZERO);

        let tab = state.current_tab_mut();
        tab.history.redo(&mut tab.doc);
        assert_eq!(state.doc().layer(id).unwrap().offset, IVec2::new(15, 20));
    }

    #[test]
    fn free_transform_tool_undo_redo_restores_pixels() {
        let mut state = AppState::new_document(200, 200);
        let id = state.doc().active.unwrap();
        fill_red_block(&mut state.tabs[state.active_tab].doc, id);
        state.tool_manager.set_tool(ToolKind::FreeTransform);

        // Uniform scale 2x about the centre.
        send(
            &mut state.tool_manager,
            &state.tabs[state.active_tab].doc,
            IVec2::new(6, 2),
            Phase::Down,
        );
        send(
            &mut state.tool_manager,
            &state.tabs[state.active_tab].doc,
            IVec2::new(10, 2),
            Phase::Drag,
        );
        let original_buffer = state.doc().layer(id).unwrap().buffer().unwrap().clone();
        let original_offset = state.doc().layer(id).unwrap().offset;

        let cmd = commit(&mut state.tool_manager, &state.tabs[state.active_tab].doc).unwrap();
        dispatch::dispatch(&mut state, cmd).unwrap();
        assert!(state.doc().layer(id).unwrap().offset != original_offset);

        let tab = state.current_tab_mut();
        tab.history.undo(&mut tab.doc);
        assert_eq!(
            state.doc().layer(id).unwrap().buffer().unwrap(),
            &original_buffer
        );
        assert_eq!(state.doc().layer(id).unwrap().offset, original_offset);

        let tab = state.current_tab_mut();
        tab.history.redo(&mut tab.doc);
        assert!(state.doc().layer(id).unwrap().offset != original_offset);
    }

    #[test]
    fn free_transform_tool_distort_commits_perspective_command() {
        let mut state = AppState::new_document(200, 200);
        let id = state.doc().active.unwrap();
        fill_red_rect(&mut state.tabs[state.active_tab].doc, id, 10, 10);

        let mut tool = FreeTransformTool::new();
        tool.transform_mode = FreeTransformMode::Distort;
        tool.layer = Some(id);
        tool.start = Some(IVec2::new(10, 0));
        tool.current = IVec2::new(12, 1);
        tool.init_handles(ogre_core::Rect::new(0, 0, 10, 10));
        tool.handle = Some(Handle::Corner(1)); // TR
        tool.update_handle(tool.current);

        let cmd = Tool::commit(&mut tool, &state.tabs[state.active_tab].doc).unwrap();
        assert_eq!(cmd.label(), "Transform layer");
        dispatch::dispatch(&mut state, cmd).unwrap();

        let layer = state.doc().layer(id).unwrap();
        // The TR corner moved, so the layer should have been resampled.
        let bounds = layer.buffer().unwrap().exact_bounds().unwrap();
        assert!(bounds.w >= 10 && bounds.h >= 10);
    }

    #[test]
    fn free_transform_tool_warp_commits_warp_command() {
        let mut state = AppState::new_document(200, 200);
        let id = state.doc().active.unwrap();
        fill_red_rect(&mut state.tabs[state.active_tab].doc, id, 10, 10);

        let mut tool = FreeTransformTool::new();
        tool.transform_mode = FreeTransformMode::Warp;
        tool.layer = Some(id);
        tool.start = Some(IVec2::new(5, 0));
        tool.current = IVec2::new(6, 2);
        tool.init_handles(ogre_core::Rect::new(0, 0, 10, 10));
        // Displace a top-edge grid point downward.
        if let Some(idx) = tool.nearest_grid_point(IVec2::new(5, 0), 6.0) {
            tool.handle = Some(Handle::GridPoint(idx));
            tool.update_handle(tool.current);
        }

        let cmd = Tool::commit(&mut tool, &state.tabs[state.active_tab].doc).unwrap();
        assert_eq!(cmd.label(), "Transform layer");
        dispatch::dispatch(&mut state, cmd).unwrap();

        let layer = state.doc().layer(id).unwrap();
        let bounds = layer.buffer().unwrap().exact_bounds().unwrap();
        assert!(bounds.w >= 10 && bounds.h >= 10);
    }
}
