// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Selection model: empty, rectangle, and per-pixel mask coverage.
//!
//! A [`Selection`] describes a region of the document that operations such as
//! copy/cut, fill, or filter should affect. Rectangular selections are stored
//! as a fast path; non-rectangular results fall back to a sparse tiled mask
//! where coverage is stored in the red channel of a [`TiledBuffer`].

use crate::buffer::TiledBuffer;
use crate::coord::{tile_rect, IVec2, Rect, TileCoord};
use crate::pixel::Rgba32F;

use crate::pixel::clamp01_nan0 as sanitize_coverage;

/// Largest feather radius accepted by [`Selection::feather`], in pixels.
const MAX_FEATHER_RADIUS: f32 = 1_000.0;

/// Largest grow/shrink amount accepted by [`Selection::grow`] and
/// [`Selection::shrink`], in pixels.
const MAX_MORPH_PX: u32 = 10_000;

/// Hermite interpolation between `edge0` and `edge1`, clamped to `[0, 1]`.
fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    if edge0 == edge1 {
        return if x < edge0 { 0.0 } else { 1.0 };
    }
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// A normalized 1-D Gaussian kernel with `2*radius + 1` taps and sigma `sigma`.
fn gaussian_kernel(radius: i32, sigma: f32) -> Vec<f32> {
    if sigma <= 0.0 {
        // Degenerate case: a single full-weight tap.
        return vec![1.0];
    }
    let size = 2 * radius as usize + 1;
    let mut weights = Vec::with_capacity(size);
    let two_sigma2 = 2.0 * sigma * sigma;
    let mut sum = 0.0f32;
    for i in 0..size {
        let x = (i as i32 - radius) as f32;
        let w = (-(x * x) / two_sigma2).exp();
        weights.push(w);
        sum += w;
    }
    if sum > 0.0 {
        for w in &mut weights {
            *w /= sum;
        }
    }
    weights
}

/// Add `(dx, dy)` to `p`, clamping each coordinate to the `i32` range.
fn offset_clamped(p: IVec2, dx: i64, dy: i64) -> IVec2 {
    let x = (p.x as i64)
        .checked_add(dx)
        .map(|v| v.clamp(i32::MIN as i64, i32::MAX as i64))
        .unwrap_or(i32::MAX as i64) as i32;
    let y = (p.y as i64)
        .checked_add(dy)
        .map(|v| v.clamp(i32::MIN as i64, i32::MAX as i64))
        .unwrap_or(i32::MAX as i64) as i32;
    IVec2::new(x, y)
}

/// Tight integer bounds of a set of polygon vertices.
fn polygon_bounds(points: &[IVec2]) -> Option<Rect> {
    let (min_x, min_y, max_x, max_y) = points.iter().fold(
        (i32::MAX, i32::MAX, i32::MIN, i32::MIN),
        |(min_x, min_y, max_x, max_y), p| {
            (
                min_x.min(p.x),
                min_y.min(p.y),
                max_x.max(p.x),
                max_y.max(p.y),
            )
        },
    );
    let w = (max_x as i64 - min_x as i64).checked_add(1)?;
    let h = (max_y as i64 - min_y as i64).checked_add(1)?;
    if w < 0 || h < 0 || w > u32::MAX as i64 || h > u32::MAX as i64 {
        return None;
    }
    Some(Rect::new(min_x, min_y, w as u32, h as u32))
}

/// True if the union of two rectangles is itself a rectangle.
///
/// This happens when the two rectangles overlap or touch such that their
/// combined area has no gaps, i.e. `area(a ∪ b) == area(a) + area(b) - area(a ∩ b)`.
fn rect_union_is_rect(a: Rect, b: Rect) -> Option<Rect> {
    let union = a.union(b);
    let intersect_area = a.intersect(b).map(|r| r.area()).unwrap_or(0);
    let combined = a.area() as u128 + b.area() as u128 - intersect_area as u128;
    if union.area() as u128 == combined {
        Some(union)
    } else {
        None
    }
}

/// The concrete shape of a [`Selection`].
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum SelectionKind {
    /// No selection; empty coverage everywhere.
    #[default]
    None,
    /// Axis-aligned rectangle with full interior coverage.
    Rect(Rect),
    /// Complement of an axis-aligned rectangle inside a canvas rectangle.
    ///
    /// Coverage is full (`1.0`) for every pixel inside `canvas` that lies
    /// outside `rect`. This is the cheap representation of an inverted
    /// rectangular selection: it avoids allocating a mask covering the whole
    /// canvas. `rect` is always clipped to `canvas` on construction.
    InvertedRect {
        /// Canvas (and bounding box) of the selection.
        canvas: Rect,
        /// Hole inside the canvas that is *not* selected.
        rect: Rect,
    },
    /// Per-pixel coverage mask.
    ///
    /// Coverage values are in the range `0.0..=1.0` and are stored in the
    /// `.r` channel of `coverage`. `bounds` caches the mask's extent so
    /// iteration does not have to scan the whole tile map.
    Mask {
        /// Per-pixel coverage stored in the red channel.
        coverage: TiledBuffer,
        /// Bounding rectangle of the mask in document space.
        bounds: Rect,
    },
}

/// How a new selection should be combined with the existing selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SelectionMode {
    /// Replace the current selection entirely.
    #[default]
    Replace,
    /// Add the new region to the current selection (union).
    Add,
    /// Remove the new region from the current selection.
    Subtract,
    /// Keep only the overlap between the new region and the current selection.
    Intersect,
}

/// A selection represents a region of the document.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Selection {
    /// The concrete shape and coverage of this selection.
    pub kind: SelectionKind,
}

impl Selection {
    /// Create an empty selection.
    pub fn none() -> Self {
        Self {
            kind: SelectionKind::None,
        }
    }

    /// Create a rectangular selection with full interior coverage.
    ///
    /// Empty rectangles are normalised to [`SelectionKind::None`].
    pub fn rect(r: Rect) -> Self {
        if r.is_empty() {
            Self::none()
        } else {
            Self {
                kind: SelectionKind::Rect(r),
            }
        }
    }

    /// Create an inverted-rect selection: every pixel inside `canvas` but
    /// outside `rect` is fully selected.
    ///
    /// `rect` is clipped to `canvas`. If it does not intersect the canvas, the
    /// whole canvas is selected; if it covers the canvas, the result is empty.
    fn inverted_rect(canvas: Rect, rect: Rect) -> Self {
        if canvas.is_empty() {
            return Self::none();
        }
        let Some(rect) = rect.intersect(canvas) else {
            return Self::rect(canvas);
        };
        if rect == canvas {
            return Self::none();
        }
        Self {
            kind: SelectionKind::InvertedRect { canvas, rect },
        }
    }

    /// Create a selection that covers the whole canvas.
    ///
    /// Because the canvas is a rectangle, this stays on the fast rectangular
    /// path.
    pub fn select_all(canvas: Rect) -> Self {
        Self::rect(canvas)
    }

    /// Coverage at a document-space pixel, in the range `0.0..=1.0`.
    pub fn coverage_at(&self, p: IVec2) -> f32 {
        match &self.kind {
            SelectionKind::None => 0.0,
            SelectionKind::Rect(r) => {
                if r.contains(p) {
                    1.0
                } else {
                    0.0
                }
            }
            SelectionKind::InvertedRect { canvas, rect } => {
                if canvas.contains(p) && !rect.contains(p) {
                    1.0
                } else {
                    0.0
                }
            }
            SelectionKind::Mask { coverage, bounds } => {
                if bounds.contains(p) {
                    sanitize_coverage(coverage.get_pixel(p).r)
                } else {
                    0.0
                }
            }
        }
    }

    /// The bounding rectangle of the selection, if any.
    pub fn bounds(&self) -> Option<Rect> {
        match &self.kind {
            SelectionKind::None => None,
            SelectionKind::Rect(r) => Some(*r),
            SelectionKind::InvertedRect { canvas, .. } => Some(*canvas),
            SelectionKind::Mask { bounds, .. } => Some(*bounds),
        }
    }

    /// Combine this selection with `other` using `mode`.
    pub fn combine(&self, other: &Self, mode: SelectionMode) -> Self {
        match mode {
            SelectionMode::Replace => other.clone(),
            SelectionMode::Add => self.union(other),
            SelectionMode::Subtract => self.subtract(other),
            SelectionMode::Intersect => self.intersect(other),
        }
    }

    /// True if this selection covers no pixels.
    pub fn is_empty(&self) -> bool {
        match &self.kind {
            SelectionKind::None => true,
            SelectionKind::Rect(r) => r.is_empty(),
            SelectionKind::InvertedRect { canvas, .. } => canvas.is_empty(),
            SelectionKind::Mask { coverage, bounds } => coverage.is_empty() || bounds.is_empty(),
        }
    }

    /// Invert this selection within `canvas`.
    ///
    /// The result selects every pixel inside `canvas` that this selection did
    /// not select. Inverting an empty selection selects the whole canvas.
    pub fn invert(&self, canvas: Rect) -> Self {
        if canvas.is_empty() {
            return Self::none();
        }
        if self.is_empty() {
            return Self::rect(canvas);
        }
        match &self.kind {
            SelectionKind::None => Self::rect(canvas),
            SelectionKind::Rect(r) => Self::inverted_rect(canvas, *r),
            SelectionKind::InvertedRect { canvas: c, rect } => {
                if *c == canvas {
                    // Inverting the complement returns the original hole.
                    return Self::rect(*rect);
                }
                // Different canvas: fall back to a mask.
                let mut new_coverage = TiledBuffer::new();
                for_pixels_in_rect(canvas, |p| {
                    let cov = sanitize_coverage(1.0 - self.coverage_at(p));
                    if cov > 0.0 {
                        new_coverage.set_pixel(p, Rgba32F::new(cov, 0.0, 0.0, 1.0));
                    }
                });
                Self {
                    kind: SelectionKind::Mask {
                        coverage: new_coverage,
                        bounds: canvas,
                    },
                }
            }
            SelectionKind::Mask { coverage, .. } => {
                let mut new_coverage = TiledBuffer::new();
                for_pixels_in_rect(canvas, |p| {
                    let cov = sanitize_coverage(1.0 - coverage.get_pixel(p).r);
                    if cov > 0.0 {
                        new_coverage.set_pixel(p, Rgba32F::new(cov, 0.0, 0.0, 1.0));
                    }
                });
                Self {
                    kind: SelectionKind::Mask {
                        coverage: new_coverage,
                        bounds: canvas,
                    },
                }
            }
        }
    }

    /// Union of two selections.
    ///
    /// The union of two rectangles stays a rectangle; every other combination
    /// promotes to a mask using per-pixel `max` coverage.
    pub fn union(&self, other: &Self) -> Self {
        if self.is_empty() {
            return other.clone();
        }
        if other.is_empty() {
            return self.clone();
        }
        match (&self.kind, &other.kind) {
            (SelectionKind::Rect(a), SelectionKind::Rect(b)) => {
                if let Some(union) = rect_union_is_rect(*a, *b) {
                    Self::rect(union)
                } else {
                    Self::combine_as_mask(self, other, a.union(*b), |a, b| a.max(b))
                }
            }
            (
                SelectionKind::InvertedRect { canvas, rect },
                SelectionKind::InvertedRect {
                    canvas: c2,
                    rect: r2,
                },
            ) => {
                if *canvas == *c2 {
                    // canvas \ r1 ∪ canvas \ r2 == canvas \ (r1 ∩ r2)
                    let hole = rect
                        .intersect(*r2)
                        .unwrap_or(Rect::new(canvas.x, canvas.y, 0, 0));
                    Self::inverted_rect(*canvas, hole)
                } else {
                    let bounds = canvas.union(*c2);
                    Self::combine_as_mask(self, other, bounds, |a, b| a.max(b))
                }
            }
            (SelectionKind::InvertedRect { canvas, rect }, SelectionKind::Rect(b)) => {
                Self::union_inverted_rect_and_rect(*canvas, *rect, *b, self, other)
            }
            (SelectionKind::Rect(b), SelectionKind::InvertedRect { canvas, rect }) => {
                Self::union_inverted_rect_and_rect(*canvas, *rect, *b, other, self)
            }
            _ => {
                let bounds = self
                    .bounds()
                    .expect("non-empty selection has bounds")
                    .union(other.bounds().expect("non-empty selection has bounds"));
                Self::combine_as_mask(self, other, bounds, |a, b| a.max(b))
            }
        }
    }

    /// Union helper: `(canvas \ hole) ∪ rect`.
    fn union_inverted_rect_and_rect(
        canvas: Rect,
        hole: Rect,
        rect: Rect,
        inverted: &Self,
        rect_sel: &Self,
    ) -> Self {
        let Some(rect_in_canvas) = rect.intersect(canvas) else {
            // The rect is outside the canvas and adds nothing.
            return inverted.clone();
        };
        if rect_in_canvas.intersect(hole).is_none() {
            // The rect lies entirely in the already-selected exterior.
            return inverted.clone();
        }
        if hole.intersect(rect_in_canvas) == Some(hole) {
            // The rect covers the hole entirely.
            return Self::rect(canvas);
        }
        // If the combined hole is still a rectangle, stay on the cheap path.
        if let Some(union_hole) = rect_union_is_rect(hole, rect_in_canvas) {
            return Self::inverted_rect(canvas, union_hole);
        }
        Self::combine_as_mask(inverted, rect_sel, canvas, |a, b| a.max(b))
    }

    /// Intersection of two selections.
    ///
    /// The intersection of two rectangles stays a rectangle; every other
    /// combination promotes to a mask using per-pixel `min` coverage.
    pub fn intersect(&self, other: &Self) -> Self {
        if self.is_empty() || other.is_empty() {
            return Self::none();
        }
        match (&self.kind, &other.kind) {
            (SelectionKind::Rect(a), SelectionKind::Rect(b)) => match a.intersect(*b) {
                Some(r) => Self::rect(r),
                None => Self::none(),
            },
            _ => {
                let bounds = match self
                    .bounds()
                    .expect("non-empty selection has bounds")
                    .intersect(other.bounds().expect("non-empty selection has bounds"))
                {
                    Some(b) => b,
                    None => return Self::none(),
                };
                Self::combine_as_mask(self, other, bounds, |a, b| a.min(b))
            }
        }
    }

    /// Subtract `other` from this selection.
    ///
    /// The result keeps pixels selected here but not in `other`. Subtraction
    /// always promotes to a mask because the difference of two rectangles is
    /// generally non-rectangular.
    pub fn subtract(&self, other: &Self) -> Self {
        if self.is_empty() {
            return Self::none();
        }
        if other.is_empty() {
            return self.clone();
        }
        match (&self.kind, &other.kind) {
            (SelectionKind::Rect(a), SelectionKind::InvertedRect { canvas, rect }) => {
                // a \ (canvas \ rect) == a ∩ rect (clipped to canvas).
                let Some(inter) = a.intersect(*canvas).and_then(|c| c.intersect(*rect)) else {
                    return Self::none();
                };
                Self::rect(inter)
            }
            (SelectionKind::InvertedRect { canvas, rect }, SelectionKind::Rect(b)) => {
                let Some(b_canvas) = b.intersect(*canvas) else {
                    // The rect is outside the canvas and removes nothing.
                    return self.clone();
                };
                // (canvas \ rect) \ b == canvas \ (rect ∪ b)
                if let Some(union_hole) = rect_union_is_rect(*rect, b_canvas) {
                    return Self::inverted_rect(*canvas, union_hole);
                }
                Self::combine_as_mask(self, other, *canvas, |a, b| (a - b).max(0.0))
            }
            _ => {
                let bounds = self.bounds().expect("non-empty selection has bounds");
                Self::combine_as_mask(self, other, bounds, |a, b| (a - b).max(0.0))
            }
        }
    }

    /// Create a selection from an axis-aligned ellipse inscribed in `rect`.
    ///
    /// The result is a coverage mask with anti-aliased edges. An empty
    /// rectangle normalises to [`SelectionKind::None`].
    pub fn ellipse(rect: Rect) -> Self {
        if rect.is_empty() {
            return Self::none();
        }
        let cx = rect.x as f32 + rect.w as f32 / 2.0;
        let cy = rect.y as f32 + rect.h as f32 / 2.0;
        let rx = rect.w as f32 / 2.0;
        let ry = rect.h as f32 / 2.0;
        if rx <= 0.0 || ry <= 0.0 {
            return Self::none();
        }
        // Anti-aliasing transition width: about one pixel measured in the
        // normalized ellipse coordinate space.
        let aa = 0.5 / rx.max(ry);
        let mut coverage = TiledBuffer::new();
        for_pixels_in_rect(rect, |p| {
            let px = p.x as f32 + 0.5;
            let py = p.y as f32 + 0.5;
            let dx = (px - cx) / rx;
            let dy = (py - cy) / ry;
            let d2 = dx * dx + dy * dy;
            let cov = smoothstep(1.0 + aa, 1.0 - aa, d2);
            if cov > 0.0 {
                coverage.set_pixel(p, Rgba32F::new(cov, 0.0, 0.0, 1.0));
            }
        });
        if coverage.is_empty() {
            Self::none()
        } else {
            Self {
                kind: SelectionKind::Mask {
                    coverage,
                    bounds: rect,
                },
            }
        }
    }

    /// Create a selection from a closed polygon.
    ///
    /// `points` are document-space vertices in order; the path is implicitly
    /// closed from the last vertex back to the first. Degenerate inputs
    /// (fewer than three vertices or zero area) normalise to
    /// [`SelectionKind::None`].
    pub fn polygon(points: &[IVec2]) -> Self {
        if points.len() < 3 {
            return Self::none();
        }
        let Some(bounds) = polygon_bounds(points) else {
            return Self::none();
        };
        if bounds.is_empty() {
            return Self::none();
        }
        let mut coverage = TiledBuffer::new();
        for y in bounds.y as i64..bounds.bottom() {
            let y_line = y as f32 + 0.5;
            let mut xs = Vec::new();
            let n = points.len();
            for i in 0..n {
                let a = points[i];
                let b = points[(i + 1) % n];
                // Skip horizontal edges; they are handled by the neighbours.
                if a.y == b.y {
                    continue;
                }
                let y_min = a.y.min(b.y) as f32;
                let y_max = a.y.max(b.y) as f32;
                if y_line > y_min && y_line <= y_max {
                    let t = (y_line - a.y as f32) / (b.y as f32 - a.y as f32);
                    let x = a.x as f32 + t * (b.x as f32 - a.x as f32);
                    xs.push(x);
                }
            }
            if xs.is_empty() {
                continue;
            }
            xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
            for pair in xs.chunks_exact(2) {
                let x_start = (pair[0].ceil() as i32).clamp(bounds.x, bounds.right() as i32);
                let x_end = (pair[1].floor() as i32 + 1).clamp(bounds.x, bounds.right() as i32);
                for x in x_start..x_end {
                    coverage.set_pixel(IVec2::new(x, y as i32), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
                }
            }
        }
        if coverage.is_empty() {
            Self::none()
        } else {
            Self {
                kind: SelectionKind::Mask { coverage, bounds },
            }
        }
    }

    /// Blur the selection edge with a Gaussian of the given `radius` (sigma).
    ///
    /// The result is always a coverage mask whose bounds are expanded by about
    /// three sigma so the blur has room to decay to zero. A non-positive radius
    /// or an empty selection returns the original selection unchanged.
    pub fn feather(&self, radius: f32) -> Self {
        if self.is_empty() || radius <= 0.0 {
            return self.clone();
        }
        let radius = radius.min(MAX_FEATHER_RADIUS);
        let k = (radius * 3.0).ceil() as i32;
        if k <= 0 {
            return self.clone();
        }
        let kernel = gaussian_kernel(k, radius);
        let Some(bounds) = self.bounds().map(|b| b.expand(k)) else {
            return self.clone();
        };

        // Horizontal pass.
        let mut temp = TiledBuffer::new();
        for y in bounds.y as i64..bounds.bottom() {
            for x in bounds.x as i64..bounds.right() {
                let p = IVec2::new(x as i32, y as i32);
                let mut sum = 0.0f32;
                for (i, w) in kernel.iter().enumerate() {
                    let dx = i as i32 - k;
                    let q = offset_clamped(p, dx as i64, 0);
                    sum += w * self.coverage_at(q);
                }
                if sum > 0.0 {
                    temp.set_pixel(p, Rgba32F::new(sum, 0.0, 0.0, 1.0));
                }
            }
        }

        // Vertical pass.
        let mut coverage = TiledBuffer::new();
        for y in bounds.y as i64..bounds.bottom() {
            for x in bounds.x as i64..bounds.right() {
                let p = IVec2::new(x as i32, y as i32);
                let mut sum = 0.0f32;
                for (i, w) in kernel.iter().enumerate() {
                    let dy = i as i32 - k;
                    let q = offset_clamped(p, 0, dy as i64);
                    sum += w * sanitize_coverage(temp.get_pixel(q).r);
                }
                if sum > 0.0 {
                    coverage.set_pixel(p, Rgba32F::new(sum, 0.0, 0.0, 1.0));
                }
            }
        }

        if coverage.is_empty() {
            Self::none()
        } else {
            Self {
                kind: SelectionKind::Mask { coverage, bounds },
            }
        }
    }

    /// Dilate the selection by `px` pixels using a square structuring element.
    ///
    /// Growing an empty selection or by zero pixels returns the original
    /// selection unchanged.
    pub fn grow(&self, px: u32) -> Self {
        if self.is_empty() || px == 0 {
            return self.clone();
        }
        let px = px.min(MAX_MORPH_PX);
        let r = px as i64;
        let Some(bounds) = self.bounds().map(|b| b.expand(px as i32)) else {
            return self.clone();
        };
        self.morph(bounds, |sel, p| {
            let mut max_cov = 0.0f32;
            for dy in -r..=r {
                for dx in -r..=r {
                    let q = offset_clamped(p, dx, dy);
                    max_cov = max_cov.max(sel.coverage_at(q));
                }
            }
            max_cov
        })
    }

    /// Erode the selection by `px` pixels using a square structuring element.
    ///
    /// Shrinking an empty selection, by zero pixels, or beyond the selection's
    /// size returns an empty selection.
    pub fn shrink(&self, px: u32) -> Self {
        if self.is_empty() || px == 0 {
            return self.clone();
        }
        let px = px.min(MAX_MORPH_PX);
        let r = px as i64;
        let Some(bounds) = self.bounds().map(|b| b.shrink(px as i32)) else {
            return Self::none();
        };
        if bounds.is_empty() {
            return Self::none();
        }
        self.morph(bounds, |sel, p| {
            let mut min_cov = 1.0f32;
            for dy in -r..=r {
                for dx in -r..=r {
                    let q = offset_clamped(p, dx, dy);
                    min_cov = min_cov.min(sel.coverage_at(q));
                }
            }
            min_cov
        })
    }

    /// Apply a per-pixel morphological operation over `bounds`.
    fn morph(&self, bounds: Rect, op: impl Fn(&Self, IVec2) -> f32) -> Self {
        let mut coverage = TiledBuffer::new();
        for_pixels_in_rect(bounds, |p| {
            let cov = sanitize_coverage(op(self, p));
            if cov > 0.0 {
                coverage.set_pixel(p, Rgba32F::new(cov, 0.0, 0.0, 1.0));
            }
        });
        if coverage.is_empty() {
            Self::none()
        } else {
            Self {
                kind: SelectionKind::Mask { coverage, bounds },
            }
        }
    }

    /// Iterate over every tile coordinate overlapped by this selection in
    /// layer-local space.
    ///
    /// Document coordinates relate to layer-local coordinates as
    /// `doc = local + layer_offset`, so the selection bounds are shifted by
    /// `-layer_offset` before enumerating tiles.
    pub fn selected_tiles(&self, layer_offset: IVec2) -> impl Iterator<Item = TileCoord> {
        let Some(bounds) = self.bounds() else {
            return Box::new(std::iter::empty()) as Box<dyn Iterator<Item = TileCoord>>;
        };
        let x = bounds.x as i64 - layer_offset.x as i64;
        let y = bounds.y as i64 - layer_offset.y as i64;
        let right = bounds.right() - layer_offset.x as i64;
        let bottom = bounds.bottom() - layer_offset.y as i64;
        if x < i32::MIN as i64
            || x > i32::MAX as i64
            || y < i32::MIN as i64
            || y > i32::MAX as i64
            || right < i32::MIN as i64
            || right > i32::MAX as i64
            || bottom < i32::MIN as i64
            || bottom > i32::MAX as i64
        {
            return Box::new(std::iter::empty()) as Box<dyn Iterator<Item = TileCoord>>;
        }
        let local = Rect::new(x as i32, y as i32, bounds.w, bounds.h);
        Box::new(local.tiles_covered())
    }

    /// Visit every covered document-space pixel and its coverage.
    ///
    /// Only pixels with coverage strictly greater than zero are visited. For
    /// rectangular selections every interior pixel is visited with coverage
    /// `1.0`; for masks only pixels with non-zero mask coverage are visited.
    pub fn for_each_pixel(&self, mut f: impl FnMut(IVec2, f32)) {
        match &self.kind {
            SelectionKind::None => {}
            SelectionKind::Rect(r) => {
                for_pixels_in_rect(*r, |p| f(p, 1.0));
            }
            SelectionKind::InvertedRect { canvas, rect } => {
                for_pixels_in_rect(*canvas, |p| {
                    if !rect.contains(p) {
                        f(p, 1.0);
                    }
                });
            }
            SelectionKind::Mask { coverage, bounds } => {
                for (c, tile) in coverage.occupied_tiles() {
                    let Some(t_rect) = tile_rect(c) else { continue };
                    let Some(inter) = t_rect.intersect(*bounds) else {
                        continue;
                    };
                    let tx = t_rect.x as i64;
                    let ty = t_rect.y as i64;
                    for y in inter.y as i64..inter.bottom() {
                        for x in inter.x as i64..inter.right() {
                            let lx = (x - tx) as usize;
                            let ly = (y - ty) as usize;
                            let cov = sanitize_coverage(tile.get(lx, ly).r);
                            if cov > 0.0 {
                                f(IVec2::new(x as i32, y as i32), cov);
                            }
                        }
                    }
                }
            }
        }
    }

    /// A cheap fingerprint that changes whenever the selection's outline would.
    ///
    /// `O(1)` for rectangular kinds and `O(occupied tiles)` for masks — far
    /// cheaper than recomputing [`Self::outline_edges`]. The UI caches the outline
    /// keyed on this so it is only retraced when the selection actually changes,
    /// not every frame. (A mask edited in place to the same bounds and tile count
    /// could in theory collide; the stale outline then self-heals on the next
    /// real change — acceptable for a visual-only cache.)
    pub fn outline_cache_key(&self) -> u64 {
        fn mix(h: &mut u64, v: u64) {
            *h ^= v;
            *h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        fn mix_rect(h: &mut u64, r: &Rect) {
            mix(h, r.x as u32 as u64);
            mix(h, r.y as u32 as u64);
            mix(h, r.w as u64);
            mix(h, r.h as u64);
        }
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        match &self.kind {
            SelectionKind::None => mix(&mut h, 0),
            SelectionKind::Rect(r) => {
                mix(&mut h, 1);
                mix_rect(&mut h, r);
            }
            SelectionKind::InvertedRect { canvas, rect } => {
                mix(&mut h, 2);
                mix_rect(&mut h, canvas);
                mix_rect(&mut h, rect);
            }
            SelectionKind::Mask { bounds, coverage } => {
                mix(&mut h, 3);
                mix_rect(&mut h, bounds);
                mix(&mut h, coverage.occupied_tiles().len() as u64);
            }
        }
        h
    }

    /// Trace the selection's boundary as a list of unit-length edge segments in
    /// **document space**, with integer pixel-corner endpoints (a pixel `(x, y)`
    /// occupies the square from corner `(x, y)` to `(x + 1, y + 1)`).
    ///
    /// Each edge sits between a selected pixel (coverage ≥ `0.5`) and an
    /// unselected neighbour (or the bounds edge), so the result is the exact
    /// pixel contour of the selection — what a "marching ants" outline draws.
    /// Rectangular kinds return their four (or eight, for an inverted rect) sides
    /// directly. A mask larger than `OUTLINE_AREA_CAP` pixels falls back to its
    /// bounding rectangle so the trace can never blow up memory or time.
    pub fn outline_edges(&self) -> Vec<[IVec2; 2]> {
        match &self.kind {
            SelectionKind::None => Vec::new(),
            SelectionKind::Rect(r) => rect_edges(*r),
            SelectionKind::InvertedRect { canvas, rect } => {
                let mut edges = rect_edges(*canvas);
                edges.extend(rect_edges(*rect));
                edges
            }
            SelectionKind::Mask { bounds, .. } => {
                let area = (bounds.w as u64) * (bounds.h as u64);
                if area == 0 || area > OUTLINE_AREA_CAP {
                    return rect_edges(*bounds);
                }
                self.mask_outline_edges(*bounds)
            }
        }
    }

    /// Contour a mask selection by rasterising it into a dense boolean grid over
    /// `bounds` and emitting the edge between every selected pixel and an
    /// unselected (or out-of-bounds) neighbour.
    fn mask_outline_edges(&self, bounds: Rect) -> Vec<[IVec2; 2]> {
        let w = bounds.w as usize;
        let h = bounds.h as usize;
        let mut sel = vec![false; w * h];
        self.for_each_pixel(|p, cov| {
            if cov >= 0.5 {
                let ix = (p.x as i64 - bounds.x as i64) as usize;
                let iy = (p.y as i64 - bounds.y as i64) as usize;
                if ix < w && iy < h {
                    sel[iy * w + ix] = true;
                }
            }
        });
        let at = |ix: i64, iy: i64| -> bool {
            ix >= 0
                && iy >= 0
                && (ix as usize) < w
                && (iy as usize) < h
                && sel[iy as usize * w + ix as usize]
        };
        let mut edges = Vec::new();
        for iy in 0..h as i64 {
            for ix in 0..w as i64 {
                if !at(ix, iy) {
                    continue;
                }
                let x = bounds.x + ix as i32;
                let y = bounds.y + iy as i32;
                if !at(ix - 1, iy) {
                    edges.push([IVec2::new(x, y), IVec2::new(x, y + 1)]);
                }
                if !at(ix + 1, iy) {
                    edges.push([IVec2::new(x + 1, y), IVec2::new(x + 1, y + 1)]);
                }
                if !at(ix, iy - 1) {
                    edges.push([IVec2::new(x, y), IVec2::new(x + 1, y)]);
                }
                if !at(ix, iy + 1) {
                    edges.push([IVec2::new(x, y + 1), IVec2::new(x + 1, y + 1)]);
                }
            }
        }
        edges
    }

    /// Combine two selections into a mask using the given per-pixel function.
    fn combine_as_mask(a: &Self, b: &Self, bounds: Rect, f: impl Fn(f32, f32) -> f32) -> Self {
        let mut coverage = TiledBuffer::new();
        for_pixels_in_rect(bounds, |p| {
            let cov = sanitize_coverage(f(a.coverage_at(p), b.coverage_at(p)));
            if cov > 0.0 {
                coverage.set_pixel(p, Rgba32F::new(cov, 0.0, 0.0, 1.0));
            }
        });
        if coverage.is_empty() {
            Self::none()
        } else {
            Self {
                kind: SelectionKind::Mask { coverage, bounds },
            }
        }
    }
}

/// Largest mask area (in pixels) [`Selection::outline_edges`] will trace
/// pixel-exactly before falling back to the cheap bounding-rectangle outline.
const OUTLINE_AREA_CAP: u64 = 16_000_000;

/// The four sides of `rect` as unit-corner edge segments, or empty if degenerate.
fn rect_edges(rect: Rect) -> Vec<[IVec2; 2]> {
    if rect.is_empty() {
        return Vec::new();
    }
    let x0 = rect.x;
    let y0 = rect.y;
    let x1 = i32::try_from(rect.right()).unwrap_or(i32::MAX);
    let y1 = i32::try_from(rect.bottom()).unwrap_or(i32::MAX);
    vec![
        [IVec2::new(x0, y0), IVec2::new(x1, y0)], // top
        [IVec2::new(x0, y1), IVec2::new(x1, y1)], // bottom
        [IVec2::new(x0, y0), IVec2::new(x0, y1)], // left
        [IVec2::new(x1, y0), IVec2::new(x1, y1)], // right
    ]
}

/// Iterate over every pixel in `rect`, skipping coordinates that do not fit
/// in `i32`.
fn for_pixels_in_rect(rect: Rect, mut f: impl FnMut(IVec2)) {
    if rect.is_empty() {
        return;
    }
    let y_start = rect.y as i64;
    let y_end = rect.bottom();
    let x_start = rect.x as i64;
    let x_end = rect.right();
    for y in y_start..y_end {
        for x in x_start..x_end {
            if let (Ok(xi), Ok(yi)) = (i32::try_from(x), i32::try_from(y)) {
                f(IVec2::new(xi, yi));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // Task 1.3.1 — Selection model
    // ------------------------------------------------------------------

    #[test]
    fn none_selection_is_empty() {
        let sel = Selection::none();
        assert!(sel.is_empty());
        assert_eq!(sel.bounds(), None);
        assert_eq!(sel.coverage_at(IVec2::new(0, 0)), 0.0);
    }

    #[test]
    fn outline_edges_trace_the_selection_boundary() {
        // None → no edges.
        assert!(Selection::none().outline_edges().is_empty());

        // A rectangle returns its four sides, with total edge length = perimeter.
        let r = Selection::rect(Rect::new(2, 3, 4, 5)); // 4 wide, 5 tall
        let edges = r.outline_edges();
        let len: i32 = edges
            .iter()
            .map(|[a, b]| (a.x - b.x).abs() + (a.y - b.y).abs())
            .sum();
        assert_eq!(len, 2 * (4 + 5));

        // A single-pixel mask has a 4-edge unit-square boundary.
        let mut cov = TiledBuffer::new();
        cov.set_pixel(IVec2::new(10, 10), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        let mask = Selection {
            kind: SelectionKind::Mask {
                coverage: cov,
                bounds: Rect::new(10, 10, 1, 1),
            },
        };
        let edges = mask.outline_edges();
        assert_eq!(edges.len(), 4);
        // Every endpoint is a corner of the pixel square (10,10)-(11,11).
        for [a, b] in edges {
            for p in [a, b] {
                assert!((10..=11).contains(&p.x) && (10..=11).contains(&p.y));
            }
        }
    }

    #[test]
    fn rect_coverage_is_full_inside_and_zero_outside() {
        let r = Rect::new(10, 20, 30, 40);
        let sel = Selection::rect(r);
        assert_eq!(sel.coverage_at(IVec2::new(10, 20)), 1.0);
        assert_eq!(sel.coverage_at(IVec2::new(39, 59)), 1.0);
        assert_eq!(sel.coverage_at(IVec2::new(9, 20)), 0.0);
        assert_eq!(sel.coverage_at(IVec2::new(10, 19)), 0.0);
        assert_eq!(sel.coverage_at(IVec2::new(40, 59)), 0.0);
        assert_eq!(sel.coverage_at(IVec2::new(39, 60)), 0.0);
    }

    #[test]
    fn rect_bounds_returns_the_rectangle() {
        let r = Rect::new(100, 200, 50, 60);
        let sel = Selection::rect(r);
        assert_eq!(sel.bounds(), Some(r));
    }

    #[test]
    fn empty_rect_normalises_to_none() {
        let sel = Selection::rect(Rect::new(0, 0, 0, 10));
        assert!(sel.is_empty());
        assert_eq!(sel.bounds(), None);
    }

    #[test]
    fn mask_coverage_returns_r_channel() {
        let mut coverage = TiledBuffer::new();
        coverage.set_pixel(IVec2::new(5, 5), Rgba32F::new(0.75, 0.0, 0.0, 0.0));
        let bounds = Rect::new(0, 0, 10, 10);
        let sel = Selection {
            kind: SelectionKind::Mask { coverage, bounds },
        };
        assert_eq!(sel.coverage_at(IVec2::new(5, 5)), 0.75);
        assert_eq!(sel.coverage_at(IVec2::new(4, 5)), 0.0);
    }

    #[test]
    fn mask_coverage_is_clamped() {
        let mut coverage = TiledBuffer::new();
        coverage.set_pixel(IVec2::new(0, 0), Rgba32F::new(1.5, 0.0, 0.0, 0.0));
        coverage.set_pixel(IVec2::new(1, 0), Rgba32F::new(-0.5, 0.0, 0.0, 0.0));
        let sel = Selection {
            kind: SelectionKind::Mask {
                coverage,
                bounds: Rect::new(0, 0, 2, 1),
            },
        };
        assert_eq!(sel.coverage_at(IVec2::new(0, 0)), 1.0);
        assert_eq!(sel.coverage_at(IVec2::new(1, 0)), 0.0);
    }

    #[test]
    fn nan_coverage_is_treated_as_zero() {
        let mut coverage = TiledBuffer::new();
        coverage.set_pixel(IVec2::new(1, 1), Rgba32F::new(f32::NAN, 0.0, 0.0, 0.0));
        let sel = Selection {
            kind: SelectionKind::Mask {
                coverage,
                bounds: Rect::new(0, 0, 3, 3),
            },
        };
        assert_eq!(sel.coverage_at(IVec2::new(1, 1)), 0.0);

        let mut visited = Vec::new();
        sel.for_each_pixel(|p, cov| visited.push((p, cov)));
        assert!(visited.is_empty());
    }

    #[test]
    fn nan_in_combine_is_treated_as_zero() {
        let a = Selection::rect(Rect::new(0, 0, 3, 3));
        let mut coverage = TiledBuffer::new();
        coverage.set_pixel(IVec2::new(1, 1), Rgba32F::new(f32::NAN, 0.0, 0.0, 0.0));
        let b = Selection {
            kind: SelectionKind::Mask {
                coverage,
                bounds: Rect::new(0, 0, 3, 3),
            },
        };
        let sel = a.union(&b);
        assert_eq!(sel.coverage_at(IVec2::new(1, 1)), 1.0);
    }

    #[test]
    fn empty_mask_behaves_like_none() {
        let empty = Selection {
            kind: SelectionKind::Mask {
                coverage: TiledBuffer::new(),
                bounds: Rect::new(0, 0, 10, 10),
            },
        };
        let rect = Selection::rect(Rect::new(0, 0, 5, 5));
        assert!(empty.is_empty());

        assert_eq!(empty.union(&rect).kind, rect.kind);
        assert_eq!(rect.union(&empty).kind, rect.kind);

        assert!(empty.intersect(&rect).is_empty());
        assert!(rect.intersect(&empty).is_empty());

        assert_eq!(rect.subtract(&empty).kind, rect.kind);
        assert!(empty.subtract(&rect).is_empty());

        let canvas = Rect::new(0, 0, 5, 5);
        assert_eq!(empty.invert(canvas).kind, SelectionKind::Rect(canvas));
    }

    // ------------------------------------------------------------------
    // Task 1.3.2 — Selection operations
    // ------------------------------------------------------------------

    #[test]
    fn invert_of_rect_covers_complement() {
        let canvas = Rect::new(0, 0, 10, 10);
        let sel = Selection::rect(Rect::new(2, 2, 3, 3));
        let inv = sel.invert(canvas);
        assert_eq!(inv.coverage_at(IVec2::new(0, 0)), 1.0);
        assert_eq!(inv.coverage_at(IVec2::new(2, 2)), 0.0);
        assert_eq!(inv.coverage_at(IVec2::new(4, 4)), 0.0);
        assert_eq!(inv.bounds(), Some(canvas));
    }

    #[test]
    fn invert_of_empty_selects_canvas() {
        let canvas = Rect::new(0, 0, 10, 10);
        let inv = Selection::none().invert(canvas);
        assert_eq!(inv.coverage_at(IVec2::new(5, 5)), 1.0);
        assert!(matches!(inv.kind, SelectionKind::Rect(_)));
    }

    #[test]
    fn intersect_two_rects_stays_rect() {
        let a = Selection::rect(Rect::new(0, 0, 10, 10));
        let b = Selection::rect(Rect::new(5, 5, 10, 10));
        let sel = a.intersect(&b);
        assert!(matches!(sel.kind, SelectionKind::Rect(_)));
        assert_eq!(sel.bounds(), Some(Rect::new(5, 5, 5, 5)));
        assert_eq!(sel.coverage_at(IVec2::new(5, 5)), 1.0);
        assert_eq!(sel.coverage_at(IVec2::new(4, 5)), 0.0);
    }

    #[test]
    fn union_two_rects_stays_rect() {
        // Adjacent rectangles that form a larger rectangle: no gaps, no overlap.
        let a = Selection::rect(Rect::new(0, 0, 5, 10));
        let b = Selection::rect(Rect::new(5, 0, 5, 10));
        let sel = a.union(&b);
        assert!(matches!(sel.kind, SelectionKind::Rect(_)));
        assert_eq!(sel.bounds(), Some(Rect::new(0, 0, 10, 10)));
    }

    #[test]
    fn union_disjoint_rects_promotes_to_mask() {
        let a = Selection::rect(Rect::new(0, 0, 5, 5));
        let b = Selection::rect(Rect::new(10, 0, 5, 5));
        let sel = a.union(&b);
        assert!(matches!(sel.kind, SelectionKind::Mask { .. }));
        assert_eq!(sel.coverage_at(IVec2::new(0, 0)), 1.0);
        assert_eq!(sel.coverage_at(IVec2::new(10, 0)), 1.0);
        assert_eq!(sel.coverage_at(IVec2::new(7, 0)), 0.0); // gap
    }

    #[test]
    fn union_l_shaped_rects_promotes_to_mask() {
        // Two overlapping rectangles that form an L-shape; the bounding box has gaps.
        let a = Selection::rect(Rect::new(0, 0, 5, 5));
        let b = Selection::rect(Rect::new(0, 3, 8, 5));
        let sel = a.union(&b);
        assert!(matches!(sel.kind, SelectionKind::Mask { .. }));
        assert_eq!(sel.coverage_at(IVec2::new(0, 0)), 1.0);
        assert_eq!(sel.coverage_at(IVec2::new(0, 6)), 1.0);
        assert_eq!(sel.coverage_at(IVec2::new(6, 6)), 1.0);
        assert_eq!(sel.coverage_at(IVec2::new(6, 0)), 0.0); // gap in L
    }

    #[test]
    fn union_with_mask_promotes_to_mask() {
        let a = Selection::rect(Rect::new(0, 0, 5, 5));
        let mut coverage = TiledBuffer::new();
        coverage.set_pixel(IVec2::new(10, 0), Rgba32F::new(1.0, 0.0, 0.0, 0.0));
        let b = Selection {
            kind: SelectionKind::Mask {
                coverage,
                bounds: Rect::new(10, 0, 1, 1),
            },
        };
        let sel = a.union(&b);
        assert!(matches!(sel.kind, SelectionKind::Mask { .. }));
        assert_eq!(sel.coverage_at(IVec2::new(0, 0)), 1.0);
        assert_eq!(sel.coverage_at(IVec2::new(10, 0)), 1.0);
    }

    #[test]
    fn subtract_two_rects_promotes_to_mask() {
        let a = Selection::rect(Rect::new(0, 0, 5, 5));
        let b = Selection::rect(Rect::new(2, 0, 5, 5));
        let sel = a.subtract(&b);
        assert!(matches!(sel.kind, SelectionKind::Mask { .. }));
        assert_eq!(sel.coverage_at(IVec2::new(0, 0)), 1.0);
        assert_eq!(sel.coverage_at(IVec2::new(3, 0)), 0.0);
    }

    #[test]
    fn intersect_rect_and_mask_promotes_to_mask() {
        let rect = Selection::rect(Rect::new(0, 0, 5, 5));
        let mut coverage = TiledBuffer::new();
        coverage.set_pixel(IVec2::new(3, 3), Rgba32F::new(0.5, 0.0, 0.0, 0.0));
        coverage.set_pixel(IVec2::new(7, 7), Rgba32F::new(1.0, 0.0, 0.0, 0.0));
        let mask = Selection {
            kind: SelectionKind::Mask {
                coverage,
                bounds: Rect::new(0, 0, 10, 10),
            },
        };
        let sel = rect.intersect(&mask);
        assert!(matches!(sel.kind, SelectionKind::Mask { .. }));
        assert_eq!(sel.coverage_at(IVec2::new(3, 3)), 0.5);
        assert_eq!(sel.coverage_at(IVec2::new(7, 7)), 0.0);
        assert_eq!(sel.coverage_at(IVec2::new(3, 4)), 0.0);
    }

    #[test]
    fn subtract_mask_from_rect_gives_fractional_coverage() {
        let rect = Selection::rect(Rect::new(0, 0, 3, 3));
        let mut coverage = TiledBuffer::new();
        coverage.set_pixel(IVec2::new(1, 1), Rgba32F::new(0.25, 0.0, 0.0, 0.0));
        let mask = Selection {
            kind: SelectionKind::Mask {
                coverage,
                bounds: Rect::new(0, 0, 3, 3),
            },
        };
        let sel = rect.subtract(&mask);
        assert_eq!(sel.coverage_at(IVec2::new(0, 0)), 1.0);
        assert_eq!(sel.coverage_at(IVec2::new(1, 1)), 0.75);
    }

    #[test]
    fn invert_mask_preserves_fractional_coverage() {
        let mut coverage = TiledBuffer::new();
        coverage.set_pixel(IVec2::new(1, 1), Rgba32F::new(0.25, 0.0, 0.0, 0.0));
        let mask = Selection {
            kind: SelectionKind::Mask {
                coverage,
                bounds: Rect::new(0, 0, 3, 3),
            },
        };
        let inv = mask.invert(Rect::new(0, 0, 3, 3));
        assert_eq!(inv.coverage_at(IVec2::new(1, 1)), 0.75);
        assert_eq!(inv.coverage_at(IVec2::new(0, 0)), 1.0);
    }

    #[test]
    fn invert_of_rect_uses_inverted_rect_kind() {
        let canvas = Rect::new(0, 0, 10, 10);
        let sel = Selection::rect(Rect::new(2, 2, 3, 3));
        let inv = sel.invert(canvas);
        assert!(matches!(
            inv.kind,
            SelectionKind::InvertedRect {
                canvas: c,
                rect: r
            }
            if c == canvas && r == Rect::new(2, 2, 3, 3)
        ));
        assert_eq!(inv.bounds(), Some(canvas));
        assert_eq!(inv.coverage_at(IVec2::new(0, 0)), 1.0);
        assert_eq!(inv.coverage_at(IVec2::new(3, 3)), 0.0);

        let mut visited = Vec::new();
        inv.for_each_pixel(|p, cov| visited.push((p, cov)));
        assert_eq!(visited.len(), 91); // 100 - 9
        assert!(visited.iter().all(|(_, cov)| *cov == 1.0));

        let tiles: Vec<_> = inv.selected_tiles(IVec2::ZERO).collect();
        assert_eq!(tiles.len(), 1);
        assert!(tiles.contains(&TileCoord { x: 0, y: 0 }));
    }

    #[test]
    fn invert_of_rect_outside_canvas_selects_canvas() {
        let canvas = Rect::new(0, 0, 10, 10);
        let sel = Selection::rect(Rect::new(20, 20, 5, 5));
        let inv = sel.invert(canvas);
        assert!(matches!(inv.kind, SelectionKind::Rect(r) if r == canvas));
    }

    #[test]
    fn invert_of_full_canvas_rect_is_empty() {
        let canvas = Rect::new(0, 0, 10, 10);
        let sel = Selection::rect(canvas);
        let inv = sel.invert(canvas);
        assert!(inv.is_empty());
    }

    #[test]
    fn invert_of_inverted_rect_returns_original() {
        let canvas = Rect::new(0, 0, 10, 10);
        let original = Selection::rect(Rect::new(2, 2, 3, 3));
        let inv = original.invert(canvas);
        let restored = inv.invert(canvas);
        assert_eq!(restored.kind, original.kind);
    }

    #[test]
    fn union_inverted_rect_with_disjoint_rect_stays_inverted() {
        let canvas = Rect::new(0, 0, 10, 10);
        let inv = Selection::rect(Rect::new(2, 2, 3, 3)).invert(canvas);
        let add = Selection::rect(Rect::new(6, 2, 2, 2));
        let union = inv.union(&add);
        assert!(matches!(union.kind, SelectionKind::InvertedRect { .. }));
        assert_eq!(union.coverage_at(IVec2::new(6, 2)), 1.0);
        assert_eq!(union.coverage_at(IVec2::new(3, 3)), 0.0);
    }

    #[test]
    fn union_inverted_rect_with_rect_covering_hole_selects_canvas() {
        let canvas = Rect::new(0, 0, 10, 10);
        let inv = Selection::rect(Rect::new(2, 2, 3, 3)).invert(canvas);
        let add = Selection::rect(Rect::new(0, 0, 10, 10));
        let union = inv.union(&add);
        assert!(matches!(union.kind, SelectionKind::Rect(r) if r == canvas));
    }

    #[test]
    fn subtract_rect_from_inverted_rect_shrinks_hole() {
        let canvas = Rect::new(0, 0, 10, 10);
        let inv = Selection::rect(Rect::new(2, 2, 6, 6)).invert(canvas);
        // Remove a disjoint vertical strip so the combined hole is still a rect.
        let removed = Selection::rect(Rect::new(8, 2, 2, 6));
        let sub = inv.subtract(&removed);
        assert!(matches!(sub.kind, SelectionKind::InvertedRect { .. }));
        assert_eq!(sub.coverage_at(IVec2::new(8, 2)), 0.0);
        assert_eq!(sub.coverage_at(IVec2::new(0, 5)), 1.0);
    }

    #[test]
    fn select_all_covers_canvas() {
        let canvas = Rect::new(0, 0, 20, 20);
        let sel = Selection::select_all(canvas);
        assert_eq!(sel.bounds(), Some(canvas));
        assert_eq!(sel.coverage_at(IVec2::new(0, 0)), 1.0);
        assert_eq!(sel.coverage_at(IVec2::new(19, 19)), 1.0);
        assert_eq!(sel.coverage_at(IVec2::new(20, 20)), 0.0);
    }

    #[test]
    fn union_with_none_returns_other() {
        let a = Selection::rect(Rect::new(0, 0, 5, 5));
        let sel = a.union(&Selection::none());
        assert_eq!(sel.bounds(), Some(Rect::new(0, 0, 5, 5)));
    }

    #[test]
    fn intersect_with_none_is_empty() {
        let a = Selection::rect(Rect::new(0, 0, 5, 5));
        let sel = a.intersect(&Selection::none());
        assert!(sel.is_empty());
    }

    // ------------------------------------------------------------------
    // Task 1.3.3 — Selection iteration
    // ------------------------------------------------------------------

    #[test]
    fn selected_tiles_for_40x40_rect_at_doc_990_690() {
        let sel = Selection::rect(Rect::new(990, 690, 40, 40));
        let tiles: Vec<_> = sel.selected_tiles(IVec2::ZERO).collect();
        assert!(tiles.contains(&TileCoord { x: 3, y: 2 }));
        assert!(tiles.contains(&TileCoord { x: 4, y: 2 }));
        assert_eq!(tiles.len(), 2);
    }

    #[test]
    fn selected_tiles_respects_layer_offset() {
        let sel = Selection::rect(Rect::new(10, 10, 20, 20));
        let tiles: Vec<_> = sel.selected_tiles(IVec2::new(10, 10)).collect();
        // Local bounds become (0,0,20,20) => tile (0,0) only.
        assert_eq!(tiles, vec![TileCoord { x: 0, y: 0 }]);
    }

    #[test]
    fn selected_tiles_negative_offset_spans_expected_tiles() {
        // A 2x2 rect at doc (-1,-1). With layer offset (-1,-1), local bounds
        // become (0,0,2,2) and cover tile (0,0).
        let sel = Selection::rect(Rect::new(-1, -1, 2, 2));
        let tiles: Vec<_> = sel.selected_tiles(IVec2::new(-1, -1)).collect();
        assert_eq!(tiles, vec![TileCoord { x: 0, y: 0 }]);
    }

    #[test]
    fn selected_tiles_for_empty_selection_is_empty() {
        let sel = Selection::none();
        let tiles: Vec<_> = sel.selected_tiles(IVec2::ZERO).collect();
        assert!(tiles.is_empty());
    }

    #[test]
    fn selected_tiles_overflow_on_far_edge_returns_empty() {
        // The right edge overflows i32 after shifting by the layer offset.
        let sel = Selection::rect(Rect::new(i32::MAX - 10, 0, 20, 1));
        let tiles: Vec<_> = sel.selected_tiles(IVec2::new(-5, 0)).collect();
        assert!(tiles.is_empty());
    }

    #[test]
    fn for_each_pixel_visits_all_rect_pixels() {
        let sel = Selection::rect(Rect::new(5, 5, 2, 2));
        let mut visited = Vec::new();
        sel.for_each_pixel(|p, cov| visited.push((p, cov)));
        assert_eq!(visited.len(), 4);
        assert!(visited.iter().all(|(_, cov)| *cov == 1.0));
        assert!(visited.iter().any(|(p, _)| *p == IVec2::new(5, 5)));
        assert!(visited.iter().any(|(p, _)| *p == IVec2::new(6, 6)));
    }

    #[test]
    fn for_each_pixel_on_mask_visits_only_nonzero_coverage() {
        let mut coverage = TiledBuffer::new();
        coverage.set_pixel(IVec2::new(7, 7), Rgba32F::new(0.5, 0.0, 0.0, 0.0));
        let sel = Selection {
            kind: SelectionKind::Mask {
                coverage,
                bounds: Rect::new(0, 0, 10, 10),
            },
        };
        let mut visited = Vec::new();
        sel.for_each_pixel(|p, cov| visited.push((p, cov)));
        assert_eq!(visited, vec![(IVec2::new(7, 7), 0.5)]);
    }

    #[test]
    fn for_each_pixel_negative_coords() {
        let sel = Selection::rect(Rect::new(-2, -2, 2, 2));
        let mut visited = Vec::new();
        sel.for_each_pixel(|p, cov| visited.push((p, cov)));
        assert_eq!(visited.len(), 4);
        assert!(visited.iter().all(|(p, _)| p.x < 0 && p.y < 0));
    }

    #[test]
    fn combine_replace_returns_other() {
        let a = Selection::rect(Rect::new(0, 0, 10, 10));
        let b = Selection::rect(Rect::new(5, 5, 10, 10));
        let result = a.combine(&b, SelectionMode::Replace);
        assert_eq!(result, b);
    }

    #[test]
    fn combine_add_unions_selections() {
        let a = Selection::rect(Rect::new(0, 0, 10, 10));
        let b = Selection::rect(Rect::new(5, 5, 10, 10));
        let result = a.combine(&b, SelectionMode::Add);
        let expected = a.union(&b);
        assert_eq!(result, expected);
    }

    #[test]
    fn combine_subtract_removes_region() {
        let a = Selection::rect(Rect::new(0, 0, 10, 10));
        let b = Selection::rect(Rect::new(5, 5, 10, 10));
        let result = a.combine(&b, SelectionMode::Subtract);
        assert!(result.coverage_at(IVec2::new(2, 2)) > 0.0);
        assert_eq!(result.coverage_at(IVec2::new(7, 7)), 0.0);
    }

    #[test]
    fn combine_intersect_keeps_overlap() {
        let a = Selection::rect(Rect::new(0, 0, 10, 10));
        let b = Selection::rect(Rect::new(5, 5, 10, 10));
        let result = a.combine(&b, SelectionMode::Intersect);
        assert_eq!(result.coverage_at(IVec2::new(7, 7)), 1.0);
        assert_eq!(result.coverage_at(IVec2::new(2, 2)), 0.0);
        assert_eq!(result.coverage_at(IVec2::new(12, 12)), 0.0);
    }

    #[test]
    fn ellipse_select_center_is_selected_and_boundary_is_fractional() {
        let sel = Selection::ellipse(Rect::new(0, 0, 20, 10));
        // Center is fully inside.
        assert_eq!(sel.coverage_at(IVec2::new(10, 5)), 1.0);
        // Far outside is zero.
        assert_eq!(sel.coverage_at(IVec2::new(-1, -1)), 0.0);
        // At least one pixel on the anti-aliased boundary has fractional coverage.
        let has_fractional = (sel.bounds().unwrap().y as i64..sel.bounds().unwrap().bottom())
            .flat_map(|y| {
                (sel.bounds().unwrap().x as i64..sel.bounds().unwrap().right())
                    .map(move |x| IVec2::new(x as i32, y as i32))
            })
            .any(|p| {
                let c = sel.coverage_at(p);
                c > 0.0 && c < 1.0
            });
        assert!(has_fractional);
    }

    #[test]
    fn ellipse_select_empty_rect_is_none() {
        assert!(Selection::ellipse(Rect::new(0, 0, 0, 10)).is_empty());
    }

    #[test]
    fn polygon_select_fills_triangle() {
        let points = vec![IVec2::new(0, 0), IVec2::new(10, 0), IVec2::new(5, 10)];
        let sel = Selection::polygon(&points);
        assert_eq!(sel.coverage_at(IVec2::new(5, 5)), 1.0);
        assert_eq!(sel.coverage_at(IVec2::new(0, 5)), 0.0);
        assert_eq!(sel.coverage_at(IVec2::new(5, -1)), 0.0);
    }

    #[test]
    fn polygon_select_too_few_points_is_empty() {
        assert!(Selection::polygon(&[IVec2::new(0, 0), IVec2::new(10, 0)]).is_empty());
    }

    #[test]
    fn polygon_select_self_intersecting_uses_even_odd_rule() {
        // A bowtie: (0,0)-(10,10)-(10,0)-(0,10).
        let points = vec![
            IVec2::new(0, 0),
            IVec2::new(10, 10),
            IVec2::new(10, 0),
            IVec2::new(0, 10),
        ];
        let sel = Selection::polygon(&points);
        // Center (5,5) is crossed twice, so it is outside by even-odd rule.
        assert_eq!(sel.coverage_at(IVec2::new(5, 5)), 0.0);
        // One of the lobes is inside.
        assert_eq!(sel.coverage_at(IVec2::new(2, 2)), 1.0);
    }

    // ------------------------------------------------------------------
    // Task 4.B.3 — Selection refinement
    // ------------------------------------------------------------------

    #[test]
    fn feather_zero_radius_leaves_selection_unchanged() {
        let sel = Selection::rect(Rect::new(10, 10, 20, 20));
        assert_eq!(sel.feather(0.0), sel);
        assert!(Selection::none().feather(2.0).is_empty());
    }

    #[test]
    fn feather_blurs_hard_rect_edge() {
        let sel = Selection::rect(Rect::new(10, 10, 20, 20));
        let feathered = sel.feather(2.0);

        // The interior, far from the blurred edge, is still fully selected.
        let center_cov = feathered.coverage_at(IVec2::new(20, 20));
        assert!(
            (center_cov - 1.0).abs() < 1e-5,
            "center coverage was {}",
            center_cov
        );
        // Far outside the influence region is still zero.
        assert_eq!(feathered.coverage_at(IVec2::new(0, 15)), 0.0);
        // The bounds grew because the Gaussian kernel extends past the original edge.
        assert!(feathered.bounds().unwrap().x < 10);
        assert!(feathered.bounds().unwrap().y < 10);
        assert!(feathered.bounds().unwrap().right() > 30);
        assert!(feathered.bounds().unwrap().bottom() > 30);
        // At least one pixel on the blurred boundary has fractional coverage.
        let has_fractional = (feathered.bounds().unwrap().y as i64
            ..feathered.bounds().unwrap().bottom())
            .flat_map(|y| {
                (feathered.bounds().unwrap().x as i64..feathered.bounds().unwrap().right())
                    .map(move |x| IVec2::new(x as i32, y as i32))
            })
            .any(|p| {
                let c = feathered.coverage_at(p);
                c > 0.0 && c < 1.0
            });
        assert!(has_fractional);
    }

    #[test]
    fn grow_expands_selection_by_pixels() {
        let sel = Selection::rect(Rect::new(10, 10, 10, 10));
        let grown = sel.grow(2);

        // Interior untouched.
        assert_eq!(grown.coverage_at(IVec2::new(12, 12)), 1.0);
        // Grew by exactly two pixels.
        assert_eq!(grown.coverage_at(IVec2::new(8, 12)), 1.0);
        assert_eq!(grown.coverage_at(IVec2::new(7, 12)), 0.0);
        assert_eq!(grown.coverage_at(IVec2::new(12, 7)), 0.0);
    }

    #[test]
    fn shrink_contracts_selection_by_pixels() {
        let sel = Selection::rect(Rect::new(10, 10, 10, 10));
        let shrunk = sel.shrink(2);

        // Interior two pixels in from every edge is still selected.
        assert_eq!(shrunk.coverage_at(IVec2::new(12, 12)), 1.0);
        // The original border is now outside the selection.
        assert_eq!(shrunk.coverage_at(IVec2::new(10, 12)), 0.0);
        assert_eq!(shrunk.coverage_at(IVec2::new(12, 10)), 0.0);
    }

    #[test]
    fn shrink_can_remove_selection_entirely() {
        let sel = Selection::rect(Rect::new(10, 10, 3, 3));
        assert!(sel.shrink(2).is_empty());
    }

    #[test]
    fn grow_and_shrink_zero_are_no_ops() {
        let sel = Selection::rect(Rect::new(10, 10, 10, 10));
        assert_eq!(sel.grow(0).kind, sel.kind);
        assert_eq!(sel.shrink(0).kind, sel.kind);
    }
}
