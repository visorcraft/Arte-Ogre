// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Magic-wand and select-by-color operations.
//!
//! These functions build a coverage [`Selection`] mask from a raster layer based
//! on the color at a seed point and a tolerance.

use std::collections::VecDeque;

use ahash::AHashSet;

use crate::buffer::TiledBuffer;
use crate::coord::{local_coord, IVec2, Rect};
use crate::pixel::{perceptual_distance_sq, Rgba32F};
use crate::selection::{Selection, SelectionKind};

/// Build a selection from a magic-wand click.
///
/// * `source` — layer-local pixel data.
/// * `src_offset` — document-space offset of the layer (`doc = local + offset`).
/// * `seed_doc` — document-space seed point (typically the pointer position).
/// * `tolerance` — maximum color distance a pixel may have from the seed pixel
///   to be included. Values below zero are treated as zero.
/// * `contiguous` — if `true`, only pixels connected to the seed by matching
///   neighbours are selected; otherwise every matching pixel in the layer is
///   selected.
///
/// The search is bounded by the occupied tiles of `source` so that fully
/// transparent areas outside the layer do not participate.
pub fn magic_wand(
    source: &TiledBuffer,
    src_offset: IVec2,
    seed_doc: IVec2,
    tolerance: f32,
    contiguous: bool,
) -> Selection {
    let tolerance = tolerance.max(0.0);
    let Some(seed_local) = local_coord(seed_doc, src_offset) else {
        return Selection::none();
    };
    let seed_color = source.get_pixel(seed_local);

    let Some(local_bounds) = source.bounds() else {
        return Selection::none();
    };

    if contiguous {
        flood_fill(
            source,
            src_offset,
            seed_local,
            seed_color,
            tolerance,
            local_bounds,
            None,
        )
    } else {
        select_by_color(source, src_offset, seed_color, tolerance, local_bounds)
    }
}

/// Select every pixel in `source` (within `scan_bounds_local`, layer-local
/// coordinates) whose perceptual color distance from `seed_color` is within
/// `tolerance` (non-contiguous). This is the non-contiguous branch of
/// [`magic_wand`] — which passes the source's occupied-tile bounds — and the
/// core of Color Range (§3.4.3), which passes the canvas so transparent canvas
/// pixels are considered too (`get_pixel` returns TRANSPARENT for unoccupied
/// local pixels).
pub fn select_by_color(
    source: &TiledBuffer,
    src_offset: IVec2,
    seed_color: Rgba32F,
    tolerance: f32,
    scan_bounds_local: Rect,
) -> Selection {
    let tolerance = tolerance.max(0.0);
    global_select(source, src_offset, seed_color, tolerance, scan_bounds_local)
}

fn matches(px: Rgba32F, seed: Rgba32F, tolerance: f32) -> bool {
    perceptual_distance_sq(px, seed) <= tolerance * tolerance
}

/// True if `p` is within `max_radius` pixels (Euclidean) of `seed`, or if no
/// radius bound was supplied.
#[inline]
fn within_radius(p: IVec2, seed: IVec2, max_radius: Option<u32>) -> bool {
    match max_radius {
        Some(r) => {
            let d = p - seed;
            let rf = r as f32;
            (d.x as f32).mul_add(d.x as f32, d.y as f32 * d.y as f32) <= rf * rf
        }
        None => true,
    }
}

fn flood_fill(
    source: &TiledBuffer,
    src_offset: IVec2,
    seed_local: IVec2,
    seed_color: Rgba32F,
    tolerance: f32,
    local_bounds: Rect,
    max_radius: Option<u32>,
) -> Selection {
    if !local_bounds.contains(seed_local) {
        return Selection::none();
    }

    let mut coverage = TiledBuffer::new();
    let mut visited = AHashSet::new();
    let mut queue = VecDeque::new();

    visited.insert(seed_local);
    queue.push_back(seed_local);

    while let Some(p) = queue.pop_front() {
        // Bound the grow to a disk around the seed (Quick Selection's brush
        // radius). Magic Wand passes `None` so the grow is unbounded.
        if !within_radius(p, seed_local, max_radius) {
            continue;
        }
        if !matches(source.get_pixel(p), seed_color, tolerance) {
            continue;
        }
        add_match(p, src_offset, &mut coverage);

        for n in neighbours(p) {
            if local_bounds.contains(n)
                && within_radius(n, seed_local, max_radius)
                && visited.insert(n)
            {
                queue.push_back(n);
            }
        }
    }

    build_selection(coverage)
}

/// Grow a selection from a single seed (`seed_doc`, document coords), bounded to
/// a disk of `max_radius` pixels around the seed when `Some`. This is the
/// shared core of Magic Wand (`max_radius = None`, contiguous) and Quick
/// Selection (one `region_grow` per brush stamp with `max_radius = brush
/// radius`).
pub fn region_grow(
    source: &TiledBuffer,
    src_offset: IVec2,
    seed_doc: IVec2,
    tolerance: f32,
    max_radius: Option<u32>,
) -> Selection {
    let tolerance = tolerance.max(0.0);
    let Some(seed_local) = local_coord(seed_doc, src_offset) else {
        return Selection::none();
    };
    let seed_color = source.get_pixel(seed_local);
    let Some(local_bounds) = source.bounds() else {
        return Selection::none();
    };
    flood_fill(
        source,
        src_offset,
        seed_local,
        seed_color,
        tolerance,
        local_bounds,
        max_radius,
    )
}

/// Build a selection by unioning one `region_grow` per sample point along a
/// brush stroke (Quick Selection, §3.4.1). Each sample seeds a bounded grow
/// (disk of `max_radius` around the sample); the per-sample results are
/// **always unioned (Add)** here. The caller combines the returned selection
/// with the existing one under its own mode (e.g. `Subtract` for Alt-drag), so
/// this function's union is independent of how the result is merged in.
pub fn quick_select(
    source: &TiledBuffer,
    src_offset: IVec2,
    samples: &[IVec2],
    tolerance: f32,
    max_radius: u32,
) -> Selection {
    use crate::selection::SelectionMode;
    let mut acc = Selection::none();
    for &seed_doc in samples {
        let grow = region_grow(source, src_offset, seed_doc, tolerance, Some(max_radius));
        acc = acc.combine(&grow, SelectionMode::Add);
    }
    acc
}

fn global_select(
    source: &TiledBuffer,
    src_offset: IVec2,
    seed_color: Rgba32F,
    tolerance: f32,
    local_bounds: Rect,
) -> Selection {
    let mut coverage = TiledBuffer::new();

    for y in local_bounds.y as i64..local_bounds.bottom() {
        for x in local_bounds.x as i64..local_bounds.right() {
            let p = IVec2::new(x as i32, y as i32);
            if matches(source.get_pixel(p), seed_color, tolerance) {
                add_match(p, src_offset, &mut coverage);
            }
        }
    }

    build_selection(coverage)
}

fn add_match(local: IVec2, offset: IVec2, coverage: &mut TiledBuffer) {
    let doc = local + offset;
    coverage.set_pixel(doc, Rgba32F::new(1.0, 0.0, 0.0, 1.0));
}

fn neighbours(p: IVec2) -> [IVec2; 4] {
    [
        IVec2::new(p.x + 1, p.y),
        IVec2::new(p.x - 1, p.y),
        IVec2::new(p.x, p.y + 1),
        IVec2::new(p.x, p.y - 1),
    ]
}

// ponytail: bounds come from a full re-scan of the coverage buffer via
// exact_bounds rather than an O(1) min/max accumulator. Magic-wand isn't a hot
// path; restore an inline bbox accumulator if its latency ever matters.
fn build_selection(coverage: TiledBuffer) -> Selection {
    match coverage.exact_bounds() {
        Some(bounds) => Selection {
            kind: SelectionKind::Mask { coverage, bounds },
        },
        None => Selection::none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn red_layer() -> (TiledBuffer, IVec2) {
        let mut buf = TiledBuffer::new();
        // Two disconnected red squares.
        for y in 0..5 {
            for x in 0..5 {
                buf.set_pixel(IVec2::new(x, y), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
            }
        }
        for y in 0..5 {
            for x in 10..15 {
                buf.set_pixel(IVec2::new(x, y), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
            }
        }
        // A blue pixel in the middle.
        buf.set_pixel(IVec2::new(7, 2), Rgba32F::new(0.0, 0.0, 1.0, 1.0));
        (buf, IVec2::ZERO)
    }

    #[test]
    fn contiguous_selects_only_connected_region() {
        let (buf, off) = red_layer();
        let sel = magic_wand(&buf, off, IVec2::new(2, 2), 0.1, true);
        assert!(sel.coverage_at(IVec2::new(2, 2)) > 0.0);
        assert_eq!(sel.coverage_at(IVec2::new(12, 2)), 0.0);
        assert_eq!(sel.coverage_at(IVec2::new(7, 2)), 0.0);
    }

    #[test]
    fn global_selects_all_matching_pixels() {
        let (buf, off) = red_layer();
        let sel = magic_wand(&buf, off, IVec2::new(2, 2), 0.1, false);
        assert!(sel.coverage_at(IVec2::new(2, 2)) > 0.0);
        assert!(sel.coverage_at(IVec2::new(12, 2)) > 0.0);
        assert_eq!(sel.coverage_at(IVec2::new(7, 2)), 0.0);
    }

    #[test]
    fn tolerance_excludes_different_colors() {
        let (buf, off) = red_layer();
        let sel = magic_wand(&buf, off, IVec2::new(7, 2), 0.1, false);
        assert!(sel.coverage_at(IVec2::new(7, 2)) > 0.0);
        assert_eq!(sel.coverage_at(IVec2::new(2, 2)), 0.0);
    }

    #[test]
    fn seed_outside_occupied_bounds_is_empty() {
        let (buf, off) = red_layer();
        let sel = magic_wand(&buf, off, IVec2::new(1000, 1000), 0.1, true);
        assert!(sel.is_empty());
    }

    #[test]
    fn layer_offset_shifts_selection_to_doc_space() {
        let mut buf = TiledBuffer::new();
        buf.set_pixel(IVec2::new(0, 0), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        let offset = IVec2::new(100, 200);
        let sel = magic_wand(&buf, offset, IVec2::new(100, 200), 0.1, true);
        assert!(sel.coverage_at(IVec2::new(100, 200)) > 0.0);
    }
}
