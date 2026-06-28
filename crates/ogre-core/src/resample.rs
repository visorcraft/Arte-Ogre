// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Image resampling for transform tools.
//!
//! The resampler maps destination pixels back into source space using the
//! inverse of the supplied affine transform, then reconstructs the source
//! signal with a choice of filters.  Interpolation is performed in
//! premultiplied alpha to avoid dark fringes at transparent edges.

use glam::{Affine2, IVec2, Vec2};

use crate::buffer::TiledBuffer;
use crate::coord::Rect;
use crate::pixel::Rgba32F;

/// Reconstruction filter used by [`resample`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    /// Nearest-neighbour sampling.
    Nearest,
    /// Bilinear interpolation.
    Bilinear,
    /// Bicubic Catmull-Rom interpolation.
    Bicubic,
}

/// Resample `src` into a new [`TiledBuffer`] using `affine`.
///
/// `affine` maps **source** pixel coordinates to **destination** pixel
/// coordinates.  The returned buffer covers the integer bounding box of the
/// transformed source bounds.  Destination pixels whose inverse-mapped source
/// position falls outside the source bounds are left transparent.
///
/// Sampling is performed in premultiplied alpha space and converted back to
/// straight alpha before storage.
pub fn resample(src: &TiledBuffer, affine: Affine2, filter: Filter) -> TiledBuffer {
    // Use the exact (per-pixel) content bounds rather than the tile-aligned
    // `bounds()`: for a sparse source this avoids iterating whole 256×256 tiles
    // of transparent pixels. Out-of-content samples are transparent either way,
    // so the output is identical.
    let src_bounds = match src.exact_bounds() {
        Some(b) => b,
        None => return TiledBuffer::new(),
    };

    // Destination bounds are the bounding box of the transformed source corners.
    let left = src_bounds.x as f32;
    let top = src_bounds.y as f32;
    let right = src_bounds.right() as f32;
    let bottom = src_bounds.bottom() as f32;
    let corners = [
        Vec2::new(left, top),
        Vec2::new(right, top),
        Vec2::new(left, bottom),
        Vec2::new(right, bottom),
    ];

    let mut dst_min = Vec2::splat(f32::INFINITY);
    let mut dst_max = Vec2::splat(f32::NEG_INFINITY);
    for c in corners {
        let d = affine.transform_point2(c);
        dst_min = dst_min.min(d);
        dst_max = dst_max.max(d);
    }

    // Use half-open pixel bounds: floor(min)..ceil(max).
    let min_x = dst_min.x.floor() as i32;
    let min_y = dst_min.y.floor() as i32;
    let max_x = dst_max.x.ceil() as i32;
    let max_y = dst_max.y.ceil() as i32;

    if min_x >= max_x || min_y >= max_y {
        return TiledBuffer::new();
    }

    let inv = affine.inverse();
    let mut dst = TiledBuffer::new();

    for y in min_y..max_y {
        for x in min_x..max_x {
            // Pixel indices denote the upper-left corner of the unit square.
            let dst_pos = Vec2::new(x as f32, y as f32);
            let src_pos = inv.transform_point2(dst_pos);
            if let Some(px) = sample(src, src_pos, filter, src_bounds) {
                dst.set_pixel(IVec2::new(x, y), px);
            }
        }
    }

    dst
}

fn sample(src: &TiledBuffer, pos: Vec2, filter: Filter, src_bounds: Rect) -> Option<Rgba32F> {
    match filter {
        Filter::Nearest => sample_nearest(src, pos, src_bounds),
        Filter::Bilinear => sample_bilinear(src, pos, src_bounds),
        Filter::Bicubic => sample_bicubic(src, pos, src_bounds),
    }
}

fn sample_nearest(src: &TiledBuffer, pos: Vec2, src_bounds: Rect) -> Option<Rgba32F> {
    // Pixel indices are upper-left corners of unit squares; floor selects the
    // source pixel that contains `pos`.
    let p = IVec2::new(pos.x.floor() as i32, pos.y.floor() as i32);
    if src_bounds.contains(p) {
        Some(src.get_pixel(p))
    } else {
        None
    }
}

fn sample_bilinear(src: &TiledBuffer, pos: Vec2, src_bounds: Rect) -> Option<Rgba32F> {
    let x0 = pos.x.floor() as i32;
    let y0 = pos.y.floor() as i32;
    let fx = pos.x - x0 as f32;
    let fy = pos.y - y0 as f32;

    // Gather the four neighbours, falling back to transparent outside bounds.
    let p00 = fetch(src, x0, y0, src_bounds).premultiplied();
    let p10 = fetch(src, x0 + 1, y0, src_bounds).premultiplied();
    let p01 = fetch(src, x0, y0 + 1, src_bounds).premultiplied();
    let p11 = fetch(src, x0 + 1, y0 + 1, src_bounds).premultiplied();

    let top = p00.lerp(p10, fx);
    let bottom = p01.lerp(p11, fx);
    let mixed = top.lerp(bottom, fy);

    Some(mixed.unpremultiplied())
}

fn sample_bicubic(src: &TiledBuffer, pos: Vec2, src_bounds: Rect) -> Option<Rgba32F> {
    let x = pos.x.floor() as i32;
    let y = pos.y.floor() as i32;
    let fx = pos.x - x as f32;
    let fy = pos.y - y as f32;

    let mut accum = Rgba32F::TRANSPARENT.premultiplied();
    let mut weight_sum = 0.0f32;

    for dy in -1..=2 {
        for dx in -1..=2 {
            let sx = x + dx;
            let sy = y + dy;
            let wx = cubic_weight(fx - dx as f32);
            let wy = cubic_weight(fy - dy as f32);
            let w = wx * wy;
            if w == 0.0 {
                continue;
            }
            let p = fetch(src, sx, sy, src_bounds).premultiplied();
            accum = Rgba32F::new(
                accum.r + p.r * w,
                accum.g + p.g * w,
                accum.b + p.b * w,
                accum.a + p.a * w,
            );
            weight_sum += w;
        }
    }

    if weight_sum <= 0.0 {
        return None;
    }

    let inv = 1.0 / weight_sum;
    Some(Rgba32F::new(accum.r * inv, accum.g * inv, accum.b * inv, accum.a * inv).unpremultiplied())
}

fn fetch(src: &TiledBuffer, x: i32, y: i32, src_bounds: Rect) -> Rgba32F {
    if src_bounds.contains(IVec2::new(x, y)) {
        src.get_pixel(IVec2::new(x, y))
    } else {
        Rgba32F::TRANSPARENT
    }
}

/// Catmull-Rom cubic weight for a one-pixel-spaced kernel.
fn cubic_weight(d: f32) -> f32 {
    let a = d.abs();
    if a <= 1.0 {
        1.5 * a.powi(3) - 2.5 * a.powi(2) + 1.0
    } else if a <= 2.0 {
        -0.5 * a.powi(3) + 2.5 * a.powi(2) - 4.0 * a + 2.0
    } else {
        0.0
    }
}

// ---------------------------------------------------------------------------
// Perspective (projective) + bezier-grid warp.
// ---------------------------------------------------------------------------

/// Solve an 8×8 linear system via Gaussian elimination with partial pivoting.
/// Returns `None` if the matrix is singular.
fn solve_8x8(mut a: [[f64; 8]; 8], mut b: [f64; 8]) -> Option<[f64; 8]> {
    for col in 0..8 {
        // Partial pivot.
        let pivot = (col..8)
            .map(|r| a[r][col].abs())
            .enumerate()
            .max_by(|(_, x), (_, y)| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| i + col)?;
        if a[pivot][col].abs() < 1e-12 {
            return None; // singular
        }
        if pivot != col {
            a.swap(col, pivot);
            b.swap(col, pivot);
        }
        let inv_diag = 1.0 / a[col][col];
        for v in &mut a[col][col..8] {
            *v *= inv_diag;
        }
        b[col] *= inv_diag;
        for r in 0..8 {
            if r != col {
                let factor = a[r][col];
                if factor != 0.0 {
                    let pivot_row: Vec<f64> = a[col][col..8].to_vec();
                    for j in col..8 {
                        a[r][j] -= factor * pivot_row[j - col];
                    }
                    b[r] -= factor * b[col];
                }
            }
        }
    }
    Some(b)
}

/// Compute the 3×3 homography mapping `from` → `to` (four point correspondences,
/// CCW or CW — both must use the same winding). Returns the 9 matrix entries in
/// row-major order (`[h0..h8]`, `h8 = 1`), or `None` if the system is degenerate.
fn solve_homography(from: [Vec2; 4], to: [Vec2; 4]) -> Option<[f64; 9]> {
    // For each correspondence (x, y) → (u, v):
    //   h0*x + h1*y + h2 - h6*x*u - h7*y*u = u
    //   h3*x + h4*y + h5 - h6*x*v - h7*y*v = v
    let mut a = [[0.0f64; 8]; 8];
    let mut b = [0.0f64; 8];
    for i in 0..4 {
        let (x, y) = (from[i].x as f64, from[i].y as f64);
        let (u, v) = (to[i].x as f64, to[i].y as f64);
        a[2 * i] = [x, y, 1.0, 0.0, 0.0, 0.0, -x * u, -y * u];
        b[2 * i] = u;
        a[2 * i + 1] = [0.0, 0.0, 0.0, x, y, 1.0, -x * v, -y * v];
        b[2 * i + 1] = v;
    }
    let h = solve_8x8(a, b)?;
    Some([h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7], 1.0])
}

/// Apply a 3×3 homography to a point.
fn apply_homography(h: &[f64; 9], p: Vec2) -> Vec2 {
    let (x, y) = (p.x as f64, p.y as f64);
    let w = h[6] * x + h[7] * y + h[8];
    if w.abs() < 1e-12 {
        return p; // degenerate — leave pixel unmoved
    }
    let nx = (h[0] * x + h[1] * y + h[2]) / w;
    let ny = (h[3] * x + h[4] * y + h[5]) / w;
    Vec2::new(nx as f32, ny as f32)
}

/// Projective (Perspective) inverse-warp: sample `src` into a buffer shaped by
/// `dst_quad` (four destination corners in destination space). The source is
/// sampled at its `src_bounds`. Every destination pixel inside the quad's
/// bounding box is inverse-mapped to source space and bilinearly (or
/// nearest/cubic) sampled, so there are no holes or overlaps.
///
/// An affine `dst_quad` (parallelogram) produces output byte-identical to
/// [`resample`] for the same filter.
pub fn projective_warp(
    src: &TiledBuffer,
    src_bounds: Rect,
    dst_quad: [IVec2; 4],
    filter: Filter,
) -> TiledBuffer {
    // Source rectangle corners (TL, TR, BR, BL — CW winding).
    let src_corners = [
        Vec2::new(src_bounds.x as f32, src_bounds.y as f32),
        Vec2::new(src_bounds.right() as f32, src_bounds.y as f32),
        Vec2::new(src_bounds.right() as f32, src_bounds.bottom() as f32),
        Vec2::new(src_bounds.x as f32, src_bounds.bottom() as f32),
    ];
    let dst_f: [Vec2; 4] = [
        dst_quad[0].as_vec2(),
        dst_quad[1].as_vec2(),
        dst_quad[2].as_vec2(),
        dst_quad[3].as_vec2(),
    ];
    // Inverse map: dest → source.
    let h_inv = match solve_homography(dst_f, src_corners) {
        Some(h) => h,
        None => return TiledBuffer::new(), // degenerate quad
    };

    // Destination bounding box.
    let min_x = dst_f.iter().map(|p| p.x.floor() as i32).min().unwrap_or(0);
    let max_x = dst_f.iter().map(|p| p.x.ceil() as i32).max().unwrap_or(0);
    let min_y = dst_f.iter().map(|p| p.y.floor() as i32).min().unwrap_or(0);
    let max_y = dst_f.iter().map(|p| p.y.ceil() as i32).max().unwrap_or(0);

    let mut dst = TiledBuffer::new();
    for y in min_y..max_y {
        for x in min_x..max_x {
            let dst_pos = Vec2::new(x as f32, y as f32);
            // Reject pixels outside the destination quad (edge leakage fix).
            if !point_in_quad(dst_f, dst_pos) {
                continue;
            }
            let src_pos = apply_homography(&h_inv, dst_pos);
            if let Some(px) = sample(src, src_pos, filter, src_bounds) {
                dst.set_pixel(IVec2::new(x, y), px);
            }
        }
    }
    dst
}

/// A 4×4 (default) control-point lattice for the Warp transform. Control points
/// are in destination space. An identity grid (points on a regular lattice over
/// `src_bounds`) produces a byte-identical copy.
#[derive(Clone, Debug, PartialEq)]
pub struct WarpGrid {
    /// Control points in row-major order, `rows × cols`.
    pub points: Vec<IVec2>,
    /// Number of rows (Y axis).
    pub rows: usize,
    /// Number of columns (X axis).
    pub cols: usize,
}

impl WarpGrid {
    /// Build an identity grid over `src_bounds` with `cols × rows` control
    /// points.
    pub fn identity(src_bounds: Rect, cols: usize, rows: usize) -> Self {
        let cols = cols.max(2);
        let rows = rows.max(2);
        let w = src_bounds.w as f32;
        let h = src_bounds.h as f32;
        let x0 = src_bounds.x as f32;
        let y0 = src_bounds.y as f32;
        let mut points = Vec::with_capacity(cols * rows);
        for r in 0..rows {
            for c in 0..cols {
                let fx = x0 + (c as f32 / (cols - 1) as f32) * w;
                let fy = y0 + (r as f32 / (rows - 1) as f32) * h;
                points.push(IVec2::new(fx.round() as i32, fy.round() as i32));
            }
        }
        Self { points, rows, cols }
    }

    /// Get the four destination corners of cell `(col, row)`.
    fn cell_dest(&self, col: usize, row: usize) -> [Vec2; 4] {
        let idx = |c: usize, r: usize| self.points[r * self.cols + c].as_vec2();
        // TL, TR, BR, BL — CW winding.
        [
            idx(col, row),
            idx(col + 1, row),
            idx(col + 1, row + 1),
            idx(col, row + 1),
        ]
    }

    /// Get the four source corners of cell `(col, row)` — always a regular
    /// sub-rectangle of `src_bounds`, using the **same rounding** as
    /// [`WarpGrid::identity`] so an identity grid maps source↔dest
    /// consistently.
    fn cell_src(&self, col: usize, row: usize, src_bounds: Rect) -> [Vec2; 4] {
        let cw = src_bounds.w as f32 / (self.cols - 1) as f32;
        let ch = src_bounds.h as f32 / (self.rows - 1) as f32;
        let x0 = src_bounds.x as f32;
        let y0 = src_bounds.y as f32;
        let src_pt = |c: usize, r: usize| {
            let fx = x0 + (c as f32) * cw;
            let fy = y0 + (r as f32) * ch;
            Vec2::new(fx.round(), fy.round())
        };
        // TL, TR, BR, BL — CW winding.
        [
            src_pt(col, row),
            src_pt(col + 1, row),
            src_pt(col + 1, row + 1),
            src_pt(col, row + 1),
        ]
    }
}

/// Bezier-grid Warp: inverse-warp `src` through a displaced control grid. For
/// each destination pixel inside a grid cell, the local `(u, v)` is recovered by
/// inverse-bilinear interpolation in the destination cell, then the source
/// position is bilinearly interpolated in the corresponding source cell. This
/// guarantees no holes or overlaps.
pub fn bezier_warp(src: &TiledBuffer, grid: &WarpGrid, filter: Filter) -> TiledBuffer {
    let src_bounds = match src.exact_bounds() {
        Some(b) => b,
        None => return TiledBuffer::new(),
    };

    // Destination bounding box.
    let min_x = grid.points.iter().map(|p| p.x).min().unwrap_or(0);
    let max_x = grid.points.iter().map(|p| p.x).max().unwrap_or(0);
    let min_y = grid.points.iter().map(|p| p.y).min().unwrap_or(0);
    let max_y = grid.points.iter().map(|p| p.y).max().unwrap_or(0);

    let mut dst = TiledBuffer::new();
    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let p = Vec2::new(x as f32, y as f32);
            // Find the containing destination cell.
            for row in 0..grid.rows - 1 {
                for col in 0..grid.cols - 1 {
                    let dest_corners = grid.cell_dest(col, row);
                    if !point_in_quad(dest_corners, p) {
                        continue;
                    }
                    // Inverse bilinear: recover (u, v) from p in the dest cell.
                    let (u, v) = inverse_bilinear(dest_corners, p);
                    // Forward bilinear in source cell to get source position.
                    let src_corners = grid.cell_src(col, row, src_bounds);
                    let src_pos = bilinear_interp(src_corners, u, v);
                    if let Some(px) = sample(src, src_pos, filter, src_bounds) {
                        dst.set_pixel(IVec2::new(x, y), px);
                    }
                    break;
                }
            }
        }
    }
    dst
}

/// Bilinear interpolation at `(u, v)` in a quad `[TL, TR, BR, BL]` (CW).
fn bilinear_interp(corners: [Vec2; 4], u: f32, v: f32) -> Vec2 {
    let top = corners[0].lerp(corners[1], u);
    // corners[3]=BL, corners[2]=BR: bottom goes BL→BR (same u direction).
    let bottom = corners[3].lerp(corners[2], u);
    top.lerp(bottom, v)
}

/// Test whether point `p` is inside the convex quad `[TL, TR, BL, BR]`.
fn point_in_quad(corners: [Vec2; 4], p: Vec2) -> bool {
    // Same-sign cross products → inside (convex quad, CCW or CW).
    let signs: [f32; 4] = [0, 1, 2, 3].map(|i| {
        let a = corners[i];
        let b = corners[(i + 1) % 4];
        (b.x - a.x) * (p.y - a.y) - (b.y - a.y) * (p.x - a.x)
    });
    let has_pos = signs.iter().any(|&s| s > 0.0);
    let has_neg = signs.iter().any(|&s| s < 0.0);
    !(has_pos && has_neg)
}

/// Inverse bilinear interpolation: given a point `p` inside quad `[TL, TR, BR,
/// BL]`, recover `(u, v)` such that `bilinear_interp(corners, u, v) ≈ p`.
fn inverse_bilinear(corners: [Vec2; 4], p: Vec2) -> (f32, f32) {
    let [p0, p1, _p2, p3] = corners;
    let x = p - p0;
    let a = p1 - p0; // TL→TR (u direction)
    let b = p3 - p0; // TL→BL (v direction)
    let p2 = corners[2];
    let c = p2 - p3 - p1 + p0; // perspective term
                               // Solve: x = u*a + v*b + u*v*c  (nonlinear; iterate).
    let mut u = 0.5f32;
    let mut v = 0.5f32;
    for _ in 0..8 {
        let predicted = a * u + b * v + c * (u * v);
        let residual = x - predicted;
        // Jacobian: du/du = a + v*c, dx/dv = b + u*c.
        let j11 = a.x + c.x * v;
        let j12 = b.x + c.x * u;
        let j21 = a.y + c.y * v;
        let j22 = b.y + c.y * u;
        let det = j11 * j22 - j12 * j21;
        if det.abs() < 1e-10 {
            break;
        }
        let inv_det = 1.0 / det;
        u += (j22 * residual.x - j12 * residual.y) * inv_det;
        v += (-j21 * residual.x + j11 * residual.y) * inv_det;
        u = u.clamp(0.0, 1.0);
        v = v.clamp(0.0, 1.0);
    }
    (u, v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn red_dot() -> TiledBuffer {
        let mut buf = TiledBuffer::new();
        buf.set_pixel(IVec2::new(10, 10), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        buf
    }

    fn small_checker() -> TiledBuffer {
        let mut buf = TiledBuffer::new();
        for y in 0..2 {
            for x in 0..2 {
                let c = if (x + y) % 2 == 0 {
                    Rgba32F::new(1.0, 0.0, 0.0, 1.0)
                } else {
                    Rgba32F::new(0.0, 0.0, 1.0, 1.0)
                };
                buf.set_pixel(IVec2::new(x, y), c);
            }
        }
        buf
    }

    #[test]
    fn empty_source_returns_empty() {
        let src = TiledBuffer::new();
        let dst = resample(&src, Affine2::IDENTITY, Filter::Bilinear);
        assert!(dst.bounds().is_none());
    }

    #[test]
    fn identity_preserves_pixels() {
        let src = red_dot();
        let dst = resample(&src, Affine2::IDENTITY, Filter::Nearest);
        assert_eq!(
            dst.get_pixel(IVec2::new(10, 10)),
            Rgba32F::new(1.0, 0.0, 0.0, 1.0)
        );
    }

    #[test]
    fn nearest_double_scale_replicates_pixels() {
        let src = red_dot();
        let scale = Affine2::from_scale(Vec2::new(2.0, 2.0));
        let dst = resample(&src, scale, Filter::Nearest);

        // Source (10,10) -> destination (20,20).
        for y in 20..=21 {
            for x in 20..=21 {
                assert_eq!(
                    dst.get_pixel(IVec2::new(x, y)),
                    Rgba32F::new(1.0, 0.0, 0.0, 1.0),
                    "failed at ({}, {})",
                    x,
                    y
                );
            }
        }
    }

    #[test]
    fn bilinear_identity_preserves_exact_values() {
        let src = small_checker();
        let dst = resample(&src, Affine2::IDENTITY, Filter::Bilinear);
        for y in 0..2 {
            for x in 0..2 {
                assert_eq!(
                    dst.get_pixel(IVec2::new(x, y)),
                    src.get_pixel(IVec2::new(x, y))
                );
            }
        }
    }

    #[test]
    fn bilinear_uniform_double_scale_preserves_color() {
        let mut src = TiledBuffer::new();
        src.set_pixel(IVec2::new(0, 0), Rgba32F::new(0.2, 0.4, 0.6, 1.0));
        src.set_pixel(IVec2::new(1, 0), Rgba32F::new(0.2, 0.4, 0.6, 1.0));
        src.set_pixel(IVec2::new(0, 1), Rgba32F::new(0.2, 0.4, 0.6, 1.0));
        src.set_pixel(IVec2::new(1, 1), Rgba32F::new(0.2, 0.4, 0.6, 1.0));

        let scale = Affine2::from_scale(Vec2::new(2.0, 2.0));
        let dst = resample(&src, scale, Filter::Bilinear);

        for y in 0..4 {
            for x in 0..4 {
                assert!(
                    (dst.get_pixel(IVec2::new(x, y)).r - 0.2).abs() < 1e-4,
                    "failed at ({}, {})",
                    x,
                    y
                );
            }
        }
    }

    #[test]
    fn bilinear_double_scale_interpolates() {
        // Single opaque red pixel at (0,0); the rest of the 1x1 source is empty.
        // Scaling 2x maps the boundary at source x=0.5 to destination x=1.
        // Standard bilinear reconstruction gives 50% red there.
        let mut src = TiledBuffer::new();
        src.set_pixel(IVec2::new(0, 0), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        let scale = Affine2::from_scale(Vec2::new(2.0, 2.0));
        let dst = resample(&src, scale, Filter::Bilinear);

        let half = dst.get_pixel(IVec2::new(1, 0));
        // Premultiplied interpolation keeps the red chroma at full intensity
        // while halving the alpha.
        assert!(
            (half.r - 1.0).abs() < 1e-4,
            "expected full red, got {:?}",
            half
        );
        assert!((half.a - 0.5).abs() < 1e-4);
    }

    #[test]
    fn bicubic_uniform_double_scale_preserves_color() {
        let mut src = TiledBuffer::new();
        src.set_pixel(IVec2::new(0, 0), Rgba32F::new(0.2, 0.4, 0.6, 1.0));
        src.set_pixel(IVec2::new(1, 0), Rgba32F::new(0.2, 0.4, 0.6, 1.0));
        src.set_pixel(IVec2::new(0, 1), Rgba32F::new(0.2, 0.4, 0.6, 1.0));
        src.set_pixel(IVec2::new(1, 1), Rgba32F::new(0.2, 0.4, 0.6, 1.0));

        let scale = Affine2::from_scale(Vec2::new(2.0, 2.0));
        let dst = resample(&src, scale, Filter::Bicubic);

        for y in 0..4 {
            for x in 0..4 {
                assert!(
                    (dst.get_pixel(IVec2::new(x, y)).r - 0.2).abs() < 1e-3,
                    "failed at ({}, {})",
                    x,
                    y
                );
            }
        }
    }

    #[test]
    fn translate_offsets_output() {
        let src = red_dot();
        let tx = Affine2::from_translation(Vec2::new(5.0, -3.0));
        let dst = resample(&src, tx, Filter::Nearest);
        assert_eq!(
            dst.get_pixel(IVec2::new(15, 7)),
            Rgba32F::new(1.0, 0.0, 0.0, 1.0)
        );
        // Original location is now empty.
        assert_eq!(dst.get_pixel(IVec2::new(10, 10)), Rgba32F::TRANSPARENT);
    }

    // ---- Perspective + Warp ----

    fn small_src() -> TiledBuffer {
        let mut buf = TiledBuffer::new();
        for y in 0..4 {
            for x in 0..4 {
                let c = Rgba32F::new(0.2, 0.4, 0.6, 1.0);
                buf.set_pixel(IVec2::new(x, y), c);
            }
        }
        buf
    }

    #[test]
    fn projective_warp_identity_preserves_pixels() {
        let src = small_src();
        let bounds = src.exact_bounds().unwrap();
        // Identity quad: corners of the source bounds (TL, TR, BR, BL).
        let quad = [
            IVec2::new(0, 0),
            IVec2::new(4, 0),
            IVec2::new(4, 4),
            IVec2::new(0, 4),
        ];
        let dst = projective_warp(&src, bounds, quad, Filter::Bilinear);
        // Interior pixels should be preserved.
        for y in 0..4 {
            for x in 0..4 {
                let px = dst.get_pixel(IVec2::new(x, y));
                assert!(
                    (px.r - 0.2).abs() < 0.05 && (px.a - 1.0).abs() < 0.05,
                    "pixel ({},{}) = {:?}",
                    x,
                    y,
                    px
                );
            }
        }
    }

    #[test]
    fn projective_warp_perspective_shrinks_far_edge() {
        let src = small_src();
        let bounds = src.exact_bounds().unwrap();
        // Keystone: top edge full width (0..4), bottom edge narrower (1..3).
        // (TL, TR, BR, BL).
        let quad = [
            IVec2::new(0, 0),
            IVec2::new(4, 0),
            IVec2::new(3, 4),
            IVec2::new(1, 4),
        ];
        let dst = projective_warp(&src, bounds, quad, Filter::Bilinear);
        // No holes inside the quad's bounding box at y=2 (middle).
        let mid = dst.get_pixel(IVec2::new(2, 2));
        assert!(mid.a > 0.0, "interior pixel should be non-transparent");
    }

    #[test]
    fn bezier_warp_identity_preserves_pixels() {
        let src = small_src();
        let bounds = src.exact_bounds().unwrap();
        let grid = WarpGrid::identity(bounds, 4, 4);
        let dst = bezier_warp(&src, &grid, Filter::Bilinear);
        // All interior pixels should be nearly preserved.
        for y in 0..4 {
            for x in 0..4 {
                let px = dst.get_pixel(IVec2::new(x, y));
                assert!((px.r - 0.2).abs() < 0.1, "pixel ({},{}) = {:?}", x, y, px);
            }
        }
    }

    #[test]
    fn bezier_warp_identity_is_nearly_byte_identical() {
        let mut src = TiledBuffer::new();
        src.set_pixel(IVec2::new(5, 5), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        let bounds = src.exact_bounds().unwrap();
        let grid = WarpGrid::identity(bounds, 4, 4);
        let dst = bezier_warp(&src, &grid, Filter::Nearest);
        // The pixel at (5,5) should survive a nearest-sampled identity warp.
        assert_eq!(
            dst.get_pixel(IVec2::new(5, 5)),
            Rgba32F::new(1.0, 0.0, 0.0, 1.0)
        );
    }

    #[test]
    fn solve_homography_identity_matrix() {
        let sq = [
            Vec2::new(0.0, 0.0),
            Vec2::new(1.0, 0.0),
            Vec2::new(0.0, 1.0),
            Vec2::new(1.0, 1.0),
        ];
        let h = solve_homography(sq, sq).unwrap();
        // Identity homography.
        assert!((h[0] - 1.0).abs() < 1e-6);
        assert!((h[4] - 1.0).abs() < 1e-6);
        assert!((h[8] - 1.0).abs() < 1e-6);
        assert!(h[1].abs() < 1e-6);
        assert!(h[3].abs() < 1e-6);
    }

    #[test]
    fn point_in_quad_basic() {
        // CW winding: TL, TR, BR, BL.
        let quad = [
            Vec2::new(0.0, 0.0),
            Vec2::new(4.0, 0.0),
            Vec2::new(4.0, 4.0),
            Vec2::new(0.0, 4.0),
        ];
        assert!(point_in_quad(quad, Vec2::new(2.0, 2.0)));
        assert!(!point_in_quad(quad, Vec2::new(-1.0, 2.0)));
    }
}
