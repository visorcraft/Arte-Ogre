// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Paint-bucket flood fill.
//!
//! Computes the connected region of similar-colored pixels around a seed point
//! and paints the foreground color over it with anti-aliased coverage. The fill
//! is bounded by the document canvas and clipped to the active selection, and
//! reuses the same flood-fill shape as the magic-wand op.

use std::collections::VecDeque;

use ahash::AHashSet;

use crate::buffer::TiledBuffer;
use crate::compositor::blend_pixel;
use crate::coord::{IVec2, Rect};
use crate::layer::BlendMode;
use crate::pixel::Rgba32F;
use crate::selection::{Selection, SelectionKind};

/// Default color-distance tolerance for the paint bucket.
///
/// A moderate default that fills slightly beyond exact matches; the UI exposes
/// a slider so it can be raised to spread further before hitting an edge.
pub const DEFAULT_FILL_TOLERANCE: f32 = 0.2;

/// Fraction of `tolerance` over which the fill coverage ramps from 1 to 0,
/// giving anti-aliased edges. The inner `(1 - FILL_FEATHER)` of the tolerance
/// band fills fully; the outer `FILL_FEATHER` feathers out.
const FILL_FEATHER: f32 = 0.5;

/// Compute the paint-bucket writes for a click at `seed_doc`.
///
/// Returns `(layer-local coord, new pixel)` pairs: the foreground `color`
/// composited *over* the original pixel with anti-aliased coverage. The fill is
/// the 4-connected region of pixels within `tolerance` (RGBA distance) of the
/// seed color, bounded by `canvas` (in document space) and — when `selection`
/// is non-empty — clipped to its coverage. Returns an empty vec when the seed
/// is outside the canvas/layer.
pub fn fill_region(
    source: &TiledBuffer,
    src_offset: IVec2,
    seed_doc: IVec2,
    color: Rgba32F,
    tolerance: f32,
    selection: &Selection,
    canvas: Rect,
) -> Vec<(IVec2, Rgba32F)> {
    let tolerance = tolerance.max(0.0);
    let tol_sq = tolerance * tolerance;
    let inner = tolerance * (1.0 - FILL_FEATHER);

    // Canvas mapped into the layer's local space bounds the flood.
    let (Some(lx), Some(ly)) = (
        canvas.x.checked_sub(src_offset.x),
        canvas.y.checked_sub(src_offset.y),
    ) else {
        return Vec::new();
    };
    let local_canvas = Rect::new(lx, ly, canvas.w, canvas.h);

    let (Some(slx), Some(sly)) = (
        seed_doc.x.checked_sub(src_offset.x),
        seed_doc.y.checked_sub(src_offset.y),
    ) else {
        return Vec::new();
    };
    let seed_local = IVec2::new(slx, sly);

    if !local_canvas.contains(seed_local) {
        return Vec::new();
    }

    let seed_color = sanitize(source.get_pixel(seed_local));
    let clip = !selection.is_empty();

    let mut out = Vec::new();
    let mut visited = AHashSet::new();
    let mut queue = VecDeque::new();
    visited.insert(seed_local);
    queue.push_back(seed_local);

    while let Some(p) = queue.pop_front() {
        let px = sanitize(source.get_pixel(p));
        let d_sq = dist_sq(px, seed_color);
        if d_sq > tol_sq {
            // Outside the color region: do not fill or propagate.
            continue;
        }

        // Document coordinate (in range because `p` is inside `local_canvas`).
        let doc = IVec2::new(p.x + src_offset.x, p.y + src_offset.y);
        let sel_cov = if clip {
            selection.coverage_at(doc)
        } else {
            1.0
        };
        if sel_cov > 0.0 {
            let color_cov = coverage(d_sq.sqrt(), inner, tolerance);
            let cov = color_cov * sel_cov;
            if cov > 0.0 {
                // `px` is the sanitized original; blending over it composites
                // the foreground onto real content and keeps NaN out.
                out.push((p, blend_pixel(BlendMode::Normal, px, color, cov)));
            }
            // Propagate through the region (including feathered selection edge).
            for n in neighbours(p) {
                if local_canvas.contains(n) && visited.insert(n) {
                    queue.push_back(n);
                }
            }
        }
        // sel_cov == 0: outside the selection — stop the flood here.
    }

    out
}

fn coverage(dist: f32, inner: f32, tolerance: f32) -> f32 {
    if dist <= inner || tolerance <= inner {
        1.0
    } else {
        ((tolerance - dist) / (tolerance - inner)).clamp(0.0, 1.0)
    }
}

fn sanitize(px: Rgba32F) -> Rgba32F {
    let fix = |v: f32| if v.is_nan() { 0.0 } else { v };
    Rgba32F::new(fix(px.r), fix(px.g), fix(px.b), fix(px.a))
}

fn dist_sq(a: Rgba32F, b: Rgba32F) -> f32 {
    let dr = a.r - b.r;
    let dg = a.g - b.g;
    let db = a.b - b.b;
    let da = a.a - b.a;
    dr * dr + dg * dg + db * db + da * da
}

fn neighbours(p: IVec2) -> [IVec2; 4] {
    [
        IVec2::new(p.x + 1, p.y),
        IVec2::new(p.x - 1, p.y),
        IVec2::new(p.x, p.y + 1),
        IVec2::new(p.x, p.y - 1),
    ]
}

/// Compute the writes for a **Fill Selection** operation.
///
/// Unlike [`fill_region`] (a seed flood-fill), this fills the *entire* selection
/// coverage area with `color` composited over each existing pixel. With no
/// active selection, it fills the whole canvas. Coverage from a feathered / mask
/// selection modulates the blend at edges (anti-aliased fill). Returns
/// `(layer-local coord, new pixel)` pairs.
pub fn fill_selection(
    buffer: &TiledBuffer,
    offset: IVec2,
    color: Rgba32F,
    opacity: f32,
    selection: &Selection,
    canvas: Rect,
) -> Vec<(IVec2, Rgba32F)> {
    let mut out = Vec::new();
    // Iterate only the selection bounds when clipped; otherwise the whole
    // canvas (so a blank layer still fills, matching fill_gradient).
    let region = if selection.is_empty() {
        canvas
    } else {
        match selection.bounds() {
            Some(b) => match canvas.intersect(b) {
                Some(i) => i,
                None => return out,
            },
            None => return out,
        }
    };
    for y in region.y as i64..region.bottom() {
        for x in region.x as i64..region.right() {
            let doc = IVec2::new(x as i32, y as i32);
            let cov = if selection.is_empty() {
                1.0
            } else {
                selection.coverage_at(doc)
            };
            if cov <= 0.0 {
                continue;
            }
            let local = doc - offset;
            let existing = buffer.get_pixel(local);
            let blended = blend_pixel(BlendMode::Normal, existing, color, opacity * cov);
            out.push((local, blended));
        }
    }
    out
}

/// Compute the writes for a **Stroke Selection** operation (v1: `Rect` only).
///
/// Strokes the outline of a rectangular selection by filling the border band
/// of `width`/2 pixels around the rect's inside edge. Non-Rect selections
/// (Ellipse / Mask / InvertedRect) return no writes — their boundary extraction
/// is deferred. Returns `(layer-local coord, new pixel)`
/// pairs, like [`fill_selection`].
pub fn stroke_selection(
    buffer: &TiledBuffer,
    offset: IVec2,
    color: Rgba32F,
    width: f32,
    opacity: f32,
    selection: &Selection,
    canvas: Rect,
) -> Vec<(IVec2, Rgba32F)> {
    // v1: Rect selections only (ellipse/mask outline extraction is deferred).
    let rect = match &selection.kind {
        SelectionKind::Rect(r) => *r,
        _ => return Vec::new(),
    };
    // Half-width (rounded, min 1) determines the border band thickness.
    let half = (width / 2.0).round().max(1.0) as i32;
    // Build a doc-space coverage mask that is 1 on the border band, 0 in the
    // interior. Reuse fill_selection to composite the colour through it.
    let mut mask = TiledBuffer::new();
    let cov_full = Rgba32F::new(1.0, 0.0, 0.0, 1.0);
    let x0 = rect.x;
    let y0 = rect.y;
    let x1 = (rect.right() as i32).saturating_sub(half);
    let y1 = (rect.bottom() as i32).saturating_sub(half);
    for y in rect.y..rect.bottom() as i32 {
        for x in rect.x..rect.right() as i32 {
            let on_border = x < x0 + half || x >= x1 || y < y0 + half || y >= y1;
            if on_border {
                mask.set_pixel(IVec2::new(x, y), cov_full);
            }
        }
    }
    let stroke_sel = Selection {
        kind: SelectionKind::Mask {
            coverage: mask,
            bounds: rect,
        },
    };
    fill_selection(buffer, offset, color, opacity, &stroke_sel, canvas)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(r: f32, g: f32, b: f32, a: f32) -> Rgba32F {
        Rgba32F::new(r, g, b, a)
    }

    /// A `w`×`h` buffer filled with `color` at local origin.
    fn filled(w: i32, h: i32, color: Rgba32F) -> TiledBuffer {
        let mut buf = TiledBuffer::new();
        for y in 0..h {
            for x in 0..w {
                buf.set_pixel(IVec2::new(x, y), color);
            }
        }
        buf
    }

    fn apply(source: &TiledBuffer, writes: &[(IVec2, Rgba32F)]) -> TiledBuffer {
        let mut out = source.clone();
        for (p, c) in writes {
            out.set_pixel(*p, *c);
        }
        out
    }

    #[test]
    fn fill_selection_with_no_selection_fills_canvas() {
        let white = px(1.0, 1.0, 1.0, 1.0);
        let buf = filled(8, 8, white);
        let red = px(1.0, 0.0, 0.0, 1.0);
        let writes = fill_selection(
            &buf,
            IVec2::ZERO,
            red,
            1.0,
            &Selection::none(),
            Rect::new(0, 0, 8, 8),
        );
        let out = apply(&buf, &writes);
        // Every pixel becomes red.
        assert_eq!(out.get_pixel(IVec2::new(0, 0)), red);
        assert_eq!(out.get_pixel(IVec2::new(7, 7)), red);
    }

    #[test]
    fn fill_selection_clipped_to_rect() {
        let white = px(1.0, 1.0, 1.0, 1.0);
        let buf = filled(16, 16, white);
        let red = px(1.0, 0.0, 0.0, 1.0);
        let sel = Selection::rect(Rect::new(2, 2, 4, 4));
        let writes = fill_selection(&buf, IVec2::ZERO, red, 1.0, &sel, Rect::new(0, 0, 16, 16));
        let out = apply(&buf, &writes);
        // Inside the selection: red.
        assert_eq!(out.get_pixel(IVec2::new(3, 3)), red);
        assert_eq!(out.get_pixel(IVec2::new(5, 5)), red);
        // Outside: unchanged.
        assert_eq!(out.get_pixel(IVec2::new(0, 0)), white);
        assert_eq!(out.get_pixel(IVec2::new(10, 10)), white);
        // Writes only touched the selection footprint.
        assert_eq!(writes.len(), 16);
    }

    #[test]
    fn fill_selection_respects_offset() {
        let white = px(1.0, 1.0, 1.0, 1.0);
        let buf = filled(16, 16, white);
        let red = px(1.0, 0.0, 0.0, 1.0);
        // Document-space selection; layer offset (5, 0) → local coords shift.
        let sel = Selection::rect(Rect::new(5, 0, 2, 2));
        let writes = fill_selection(
            &buf,
            IVec2::new(5, 0),
            red,
            1.0,
            &sel,
            Rect::new(0, 0, 16, 16),
        );
        let out = apply(&buf, &writes);
        // Local (0,0) ↔ doc (5,0): filled.
        assert_eq!(out.get_pixel(IVec2::new(0, 0)), red);
        // Local (1,1) ↔ doc (6,1): filled.
        assert_eq!(out.get_pixel(IVec2::new(1, 1)), red);
        // Local (2,0) ↔ doc (7,0): outside selection, unchanged.
        assert_eq!(out.get_pixel(IVec2::new(2, 0)), white);
    }

    #[test]
    fn stroke_selection_rect_draws_only_border() {
        let white = px(1.0, 1.0, 1.0, 1.0);
        let buf = filled(8, 8, white);
        let red = px(1.0, 0.0, 0.0, 1.0);
        // 6×6 rect at (1,1); stroke width 2 → half=1 → border is 1px wide.
        let sel = Selection::rect(Rect::new(1, 1, 6, 6));
        let writes = stroke_selection(
            &buf,
            IVec2::ZERO,
            red,
            2.0,
            1.0,
            &sel,
            Rect::new(0, 0, 8, 8),
        );
        let out = apply(&buf, &writes);
        // Border pixels (edges of the rect) are stroked red.
        assert_eq!(out.get_pixel(IVec2::new(1, 1)), red); // corner
        assert_eq!(out.get_pixel(IVec2::new(6, 6)), red); // opposite corner
        assert_eq!(out.get_pixel(IVec2::new(3, 1)), red); // top edge
                                                          // Interior pixel is unchanged.
        assert_eq!(out.get_pixel(IVec2::new(3, 3)), white);
    }

    #[test]
    fn stroke_selection_non_rect_is_no_op() {
        let buf = filled(8, 8, px(1.0, 1.0, 1.0, 1.0));
        // A Mask selection (not Rect) → stroke is a no-op for v1.
        let sel = Selection {
            kind: SelectionKind::Mask {
                coverage: TiledBuffer::new(),
                bounds: Rect::new(0, 0, 4, 4),
            },
        };
        let writes = stroke_selection(
            &buf,
            IVec2::ZERO,
            px(1.0, 0.0, 0.0, 1.0),
            2.0,
            1.0,
            &sel,
            Rect::new(0, 0, 8, 8),
        );
        assert!(writes.is_empty(), "non-Rect selections are deferred");
    }

    #[test]
    fn interior_fills_with_foreground() {
        let white = px(1.0, 1.0, 1.0, 1.0);
        let buf = filled(16, 16, white);
        let fg = px(1.0, 0.0, 0.0, 1.0);
        let canvas = Rect::new(0, 0, 16, 16);
        let writes = fill_region(
            &buf,
            IVec2::ZERO,
            IVec2::new(8, 8),
            fg,
            DEFAULT_FILL_TOLERANCE,
            &Selection::none(),
            canvas,
        );
        let out = apply(&buf, &writes);
        // The whole flat region becomes the foreground color.
        assert_eq!(out.get_pixel(IVec2::new(8, 8)), fg);
        assert_eq!(out.get_pixel(IVec2::new(0, 0)), fg);
        assert_eq!(out.get_pixel(IVec2::new(15, 15)), fg);
    }

    #[test]
    fn disconnected_same_color_is_not_filled() {
        let white = px(1.0, 1.0, 1.0, 1.0);
        let mut buf = filled(40, 16, white);
        // A wall of red splits the canvas into two white halves.
        for y in 0..16 {
            buf.set_pixel(IVec2::new(20, y), px(1.0, 0.0, 0.0, 1.0));
        }
        let fg = px(0.0, 0.0, 1.0, 1.0);
        let canvas = Rect::new(0, 0, 40, 16);
        let writes = fill_region(
            &buf,
            IVec2::ZERO,
            IVec2::new(5, 8),
            fg,
            DEFAULT_FILL_TOLERANCE,
            &Selection::none(),
            canvas,
        );
        let out = apply(&buf, &writes);
        assert_eq!(out.get_pixel(IVec2::new(5, 8)), fg); // clicked half filled
        assert_eq!(out.get_pixel(IVec2::new(30, 8)), white); // far half untouched
        assert_eq!(out.get_pixel(IVec2::new(20, 8)), px(1.0, 0.0, 0.0, 1.0)); // wall intact
    }

    #[test]
    fn transparent_area_fills_up_to_opaque_content() {
        // Empty (transparent) canvas with an opaque square in the middle.
        let mut buf = TiledBuffer::new();
        let canvas = Rect::new(0, 0, 20, 20);
        for y in 8..12 {
            for x in 8..12 {
                buf.set_pixel(IVec2::new(x, y), px(0.0, 0.0, 0.0, 1.0));
            }
        }
        let fg = px(0.0, 1.0, 0.0, 1.0);
        let writes = fill_region(
            &buf,
            IVec2::ZERO,
            IVec2::new(0, 0), // click the transparent corner
            fg,
            DEFAULT_FILL_TOLERANCE,
            &Selection::none(),
            canvas,
        );
        let out = apply(&buf, &writes);
        assert_eq!(out.get_pixel(IVec2::new(0, 0)), fg); // transparent filled
        assert_eq!(out.get_pixel(IVec2::new(19, 19)), fg); // connected corner filled
        assert_eq!(out.get_pixel(IVec2::new(9, 9)), px(0.0, 0.0, 0.0, 1.0)); // opaque kept
    }

    #[test]
    fn seed_outside_canvas_is_empty() {
        let buf = filled(16, 16, px(1.0, 1.0, 1.0, 1.0));
        let writes = fill_region(
            &buf,
            IVec2::ZERO,
            IVec2::new(100, 100),
            px(1.0, 0.0, 0.0, 1.0),
            DEFAULT_FILL_TOLERANCE,
            &Selection::none(),
            Rect::new(0, 0, 16, 16),
        );
        assert!(writes.is_empty());
    }

    #[test]
    fn tolerance_excludes_distinct_colors() {
        let mut buf = filled(16, 16, px(1.0, 1.0, 1.0, 1.0));
        // A distinct blue pixel adjacent to the seed — must not be filled.
        buf.set_pixel(IVec2::new(9, 8), px(0.0, 0.0, 1.0, 1.0));
        let fg = px(1.0, 0.0, 0.0, 1.0);
        let writes = fill_region(
            &buf,
            IVec2::ZERO,
            IVec2::new(8, 8),
            fg,
            DEFAULT_FILL_TOLERANCE,
            &Selection::none(),
            Rect::new(0, 0, 16, 16),
        );
        let out = apply(&buf, &writes);
        assert_eq!(out.get_pixel(IVec2::new(8, 8)), fg);
        assert_eq!(out.get_pixel(IVec2::new(9, 8)), px(0.0, 0.0, 1.0, 1.0)); // blue kept
    }

    #[test]
    fn anti_aliased_boundary_gets_partial_coverage() {
        // White region with a single mid-grey pixel whose distance to white sits
        // inside the feather band, so it fills with partial alpha (blended), not
        // the full foreground.
        let white = px(1.0, 1.0, 1.0, 1.0);
        let mut buf = filled(16, 16, white);
        let tolerance = 0.6;
        // grey at distance ~0.45*sqrt? pick grey so RGBA distance from white is
        // between inner (0.3) and tolerance (0.6).
        let grey = px(0.8, 0.8, 0.8, 1.0); // dist = sqrt(3*0.04)=~0.346
        buf.set_pixel(IVec2::new(9, 8), grey);
        let fg = px(1.0, 0.0, 0.0, 1.0);
        let writes = fill_region(
            &buf,
            IVec2::ZERO,
            IVec2::new(8, 8),
            fg,
            tolerance,
            &Selection::none(),
            Rect::new(0, 0, 16, 16),
        );
        let out = apply(&buf, &writes);
        let edge = out.get_pixel(IVec2::new(9, 8));
        // Partially filled: not pure foreground, not the original grey.
        assert!(edge.r > 0.8 && edge.r < 1.0, "edge r {}", edge.r);
        assert!(edge.g > 0.0 && edge.g < 0.8, "edge g {}", edge.g);
    }

    #[test]
    fn selection_clips_the_fill() {
        let white = px(1.0, 1.0, 1.0, 1.0);
        let buf = filled(20, 20, white);
        let fg = px(1.0, 0.0, 0.0, 1.0);
        let canvas = Rect::new(0, 0, 20, 20);
        // Only the left 10 columns are selected.
        let selection = Selection::rect(Rect::new(0, 0, 10, 20));
        let writes = fill_region(
            &buf,
            IVec2::ZERO,
            IVec2::new(2, 2),
            fg,
            DEFAULT_FILL_TOLERANCE,
            &selection,
            canvas,
        );
        let out = apply(&buf, &writes);
        assert_eq!(out.get_pixel(IVec2::new(2, 2)), fg); // inside selection filled
        assert_eq!(out.get_pixel(IVec2::new(15, 2)), white); // outside selection untouched
    }

    #[test]
    fn negative_tolerance_clamped_fills_only_exact() {
        let white = px(1.0, 1.0, 1.0, 1.0);
        let mut buf = filled(8, 8, white);
        buf.set_pixel(IVec2::new(4, 4), px(0.9, 0.9, 0.9, 1.0));
        let writes = fill_region(
            &buf,
            IVec2::ZERO,
            IVec2::new(0, 0),
            px(1.0, 0.0, 0.0, 1.0),
            -5.0,
            &Selection::none(),
            Rect::new(0, 0, 8, 8),
        );
        let out = apply(&buf, &writes);
        // Exact-white pixels fill; the off-white pixel does not.
        assert_eq!(out.get_pixel(IVec2::new(0, 0)), px(1.0, 0.0, 0.0, 1.0));
        assert_eq!(out.get_pixel(IVec2::new(4, 4)), px(0.9, 0.9, 0.9, 1.0));
    }
}
