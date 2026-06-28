// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Edge detection + edge-snapping for the Magnetic Lasso (§3.4.2).
//!
//! The lasso snaps each cursor sample to the strongest-edge pixel within a disk
//! (`width`) of the cursor, where "strongest edge" is the Sobel gradient
//! magnitude of the perceptual (sRGB-encoded) luma. The path is then
//! simplified with a Douglas-Peucker-style pass before building the selection.

use crate::buffer::TiledBuffer;
use crate::coord::{IVec2, Rect};
use crate::pixel::{srgb_encode, Rgba32F};

/// Rec.709 luma of a (linear) pixel, gamma-encoded for perceptual weighting.
fn perceptual_luma(px: Rgba32F) -> f32 {
    let r = srgb_encode(px.r);
    let g = srgb_encode(px.g);
    let b = srgb_encode(px.b);
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

/// Compute the Sobel gradient-magnitude field of `source` over `region`
/// (document-space coordinates). Unoccupied pixels are treated as their nearest
/// neighbour (clamped at the buffer's occupied bounds). Used by the Magnetic
/// Lasso to find edges to snap to.
pub fn edge_magnitude(source: &TiledBuffer, region: Rect) -> Vec<(IVec2, f32)> {
    let mut out = Vec::new();
    for y in region.y as i64..region.bottom() {
        for x in region.x as i64..region.right() {
            let p = IVec2::new(x as i32, y as i32);
            let l = |q: IVec2| perceptual_luma(source.get_pixel(q));
            // Sobel kernels. Pixels outside the buffer read as the centre pixel
            // (nearest-neighbour clamp) so the border doesn't ring.
            let gx = -l(p + IVec2::new(-1, -1))
                - 2.0 * l(p + IVec2::new(-1, 0))
                - l(p + IVec2::new(-1, 1))
                + l(p + IVec2::new(1, -1))
                + 2.0 * l(p + IVec2::new(1, 0))
                + l(p + IVec2::new(1, 1));
            let gy = -l(p + IVec2::new(-1, -1))
                - 2.0 * l(p + IVec2::new(0, -1))
                - l(p + IVec2::new(1, -1))
                + l(p + IVec2::new(-1, 1))
                + 2.0 * l(p + IVec2::new(0, 1))
                + l(p + IVec2::new(1, 1));
            out.push((p, (gx * gx + gy * gy).sqrt()));
        }
    }
    out
}

/// Find the strongest-edge pixel within `radius` (Euclidean) of `center` in
/// `field`, returning it if its magnitude exceeds `contrast_threshold`.
/// `field` is the list returned by [`edge_magnitude`]. Returns `center` if no
/// pixel in the disk clears the threshold.
pub fn snap_to_edge(
    field: &[(IVec2, f32)],
    center: IVec2,
    radius: u32,
    contrast_threshold: f32,
) -> &(IVec2, f32) {
    let r2 = (radius as f32) * (radius as f32);
    let mut best: Option<&(IVec2, f32)> = None;
    for entry @ (p, mag) in field {
        let d = *p - center;
        if (d.x as f32).mul_add(d.x as f32, d.y as f32 * d.y as f32) > r2 {
            continue;
        }
        if *mag < contrast_threshold {
            continue;
        }
        if best.is_none_or(|(_, bm)| *mag > *bm) {
            best = Some(entry);
        }
    }
    // Fall back to the field entry closest to center if nothing clears the
    // threshold (so the lasso still follows the cursor loosely).
    best.unwrap_or_else(|| {
        field
            .iter()
            .min_by_key(|(p, _)| {
                let d = *p - center;
                (d.x as i64) * (d.x as i64) + (d.y as i64) * (d.y as i64)
            })
            .expect("non-empty field")
    })
}

/// Douglas-Peucker-style simplification: drop vertices whose perpendicular
/// distance from the segment between their neighbours is below `epsilon`.
/// Keeps the path's sharp corners so the snapped polygon tracks real edges.
pub fn simplify_path(points: Vec<IVec2>, epsilon: f32) -> Vec<IVec2> {
    fn perp_dist(p: IVec2, a: IVec2, b: IVec2) -> f32 {
        let dx = (b.x - a.x) as f32;
        let dy = (b.y - a.y) as f32;
        let len2 = dx * dx + dy * dy;
        if len2 < 1e-6 {
            return (((p.x - a.x) as f32).powi(2) + ((p.y - a.y) as f32).powi(2)).sqrt();
        }
        let t = (((p.x - a.x) as f32) * dx + ((p.y - a.y) as f32) * dy) / len2;
        let proj_x = a.x as f32 + t * dx;
        let proj_y = a.y as f32 + t * dy;
        (((p.x as f32) - proj_x).powi(2) + ((p.y as f32) - proj_y).powi(2)).sqrt()
    }
    fn rec(points: &[IVec2], epsilon: f32, out: &mut Vec<IVec2>) {
        if points.len() < 3 {
            out.extend_from_slice(points);
            return;
        }
        let (a, b) = (points[0], *points.last().unwrap());
        let mut max_d = 0.0f32;
        let mut idx = 0;
        for (i, p) in points.iter().enumerate().skip(1) {
            let d = perp_dist(*p, a, b);
            if d > max_d {
                max_d = d;
                idx = i;
            }
        }
        if max_d > epsilon {
            rec(&points[..=idx], epsilon, out);
            out.pop(); // avoid duplicating the pivot
            rec(&points[idx..], epsilon, out);
        } else {
            out.push(a);
            out.push(b);
        }
    }
    if points.len() < 3 {
        return points;
    }
    let mut out = Vec::new();
    rec(&points, epsilon, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc_with_circle() -> TiledBuffer {
        // A hard circle edge: bright disk of radius 4 on a dark field, centred
        // at (10,10).
        let mut buf = TiledBuffer::new();
        for y in 0..20 {
            for x in 0..20 {
                let d =
                    ((x - 10) as f32).mul_add((x - 10) as f32, (y - 10) as f32 * (y - 10) as f32);
                let c = if d <= 16.0 {
                    Rgba32F::new(0.9, 0.9, 0.9, 1.0)
                } else {
                    Rgba32F::new(0.1, 0.1, 0.1, 1.0)
                };
                buf.set_pixel(IVec2::new(x, y), c);
            }
        }
        buf
    }

    #[test]
    fn edge_magnitude_peaks_at_circle_boundary() {
        let buf = doc_with_circle();
        let region = Rect::new(0, 0, 20, 20);
        let field = edge_magnitude(&buf, region);
        let mag_at = |x, y| {
            field
                .iter()
                .find(|(p, _)| *p == IVec2::new(x, y))
                .unwrap()
                .1
        };
        // Boundary pixel (on the circle edge) has a much higher gradient than an
        // interior (uniform) pixel.
        let boundary = mag_at(10, 6); // top of the circle ≈ edge
        let interior = mag_at(10, 10); // centre, uniform
        assert!(
            boundary > interior * 5.0,
            "boundary={boundary} interior={interior}"
        );
    }

    #[test]
    fn simplify_path_keeps_corners() {
        // A square path: (0,0)-(10,0)-(10,10)-(0,10). All four corners survive.
        let pts = vec![
            IVec2::new(0, 0),
            IVec2::new(5, 0),
            IVec2::new(10, 0),
            IVec2::new(10, 5),
            IVec2::new(10, 10),
            IVec2::new(5, 10),
            IVec2::new(0, 10),
        ];
        let s = simplify_path(pts, 0.5);
        assert!(s.contains(&IVec2::new(0, 0)));
        assert!(s.contains(&IVec2::new(10, 0)));
        assert!(s.contains(&IVec2::new(10, 10)));
        assert!(s.contains(&IVec2::new(0, 10)));
        assert!(s.len() <= 5);
    }
}
