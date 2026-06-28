// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Exact (discrete) buffer mirroring for layer flip.
//!
//! Unlike the affine [`resample`](crate::resample) path (bicubic, which can
//! mis-center a pure mirror), this writes each source pixel to its exact
//! mirrored local coordinate, so the result is byte-faithful.

use crate::buffer::TiledBuffer;
use crate::coord::Rect;
use crate::pixel::Rgba32F;

/// Which axis to mirror across.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlipAxis {
    /// Mirror left↔right (across the vertical centre).
    Horizontal,
    /// Mirror top↔bottom (across the horizontal centre).
    Vertical,
}

/// Mirror a buffer's content exactly across `axis_bounds`.
///
/// Each non-transparent source pixel at local `(x, y)` is written to its
/// mirrored local coordinate, so the result is an exact reflection with no
/// interpolation artifacts. `axis_bounds` is the pixel rectangle to mirror
/// within (typically the layer's content bounds, shared with its mask so the
/// two stay aligned). An empty/zero-area `axis_bounds` yields an empty buffer.
pub fn flip_buffer(buffer: &TiledBuffer, axis: FlipAxis, axis_bounds: Rect) -> TiledBuffer {
    let mut out = TiledBuffer::new();
    if axis_bounds.is_empty() {
        return out;
    }
    // Inclusive mirror range: pixel at the low edge maps to the high edge.
    let x0 = axis_bounds.x as i64;
    let x1 = axis_bounds.right() - 1;
    let y0 = axis_bounds.y as i64;
    let y1 = axis_bounds.bottom() - 1;
    for (c, tile) in buffer.occupied_tiles() {
        let Some(t_rect) = crate::coord::tile_rect(c) else {
            continue;
        };
        for ly in 0..crate::tile::TILE_SIZE {
            for lx in 0..crate::tile::TILE_SIZE {
                let px = tile.get(lx, ly);
                if px == Rgba32F::TRANSPARENT {
                    continue;
                }
                let src_x = t_rect.x as i64 + lx as i64;
                let src_y = t_rect.y as i64 + ly as i64;
                let (dst_x, dst_y) = match axis {
                    FlipAxis::Horizontal => (x0 + x1 - src_x, src_y),
                    FlipAxis::Vertical => (src_x, y0 + y1 - src_y),
                };
                if let (Ok(dx), Ok(dy)) = (i32::try_from(dst_x), i32::try_from(dst_y)) {
                    out.set_pixel(crate::coord::IVec2::new(dx, dy), px);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coord::IVec2;

    fn set(buf: &mut TiledBuffer, x: i32, y: i32, c: Rgba32F) {
        buf.set_pixel(IVec2::new(x, y), c);
    }

    #[test]
    fn horizontal_flip_mirrors_within_bounds() {
        let mut buf = TiledBuffer::new();
        // Content at local x=2..5 (3px), y=0. Mirror within bounds (2,0,3,1).
        set(&mut buf, 2, 0, Rgba32F::new(0.1, 0.0, 0.0, 1.0));
        set(&mut buf, 3, 0, Rgba32F::new(0.2, 0.0, 0.0, 1.0));
        set(&mut buf, 4, 0, Rgba32F::new(0.3, 0.0, 0.0, 1.0));
        let bounds = Rect::new(2, 0, 3, 1);
        let flipped = flip_buffer(&buf, FlipAxis::Horizontal, bounds);
        // x=2 (left) ↔ x=4 (right); x=3 (centre) stays.
        assert_eq!(
            flipped.get_pixel(IVec2::new(2, 0)),
            Rgba32F::new(0.3, 0.0, 0.0, 1.0)
        );
        assert_eq!(
            flipped.get_pixel(IVec2::new(4, 0)),
            Rgba32F::new(0.1, 0.0, 0.0, 1.0)
        );
        assert_eq!(
            flipped.get_pixel(IVec2::new(3, 0)),
            Rgba32F::new(0.2, 0.0, 0.0, 1.0)
        );
    }

    #[test]
    fn vertical_flip_mirrors_y() {
        let mut buf = TiledBuffer::new();
        set(&mut buf, 0, 1, Rgba32F::new(0.5, 0.0, 0.0, 1.0));
        set(&mut buf, 0, 3, Rgba32F::new(0.9, 0.0, 0.0, 1.0));
        let bounds = Rect::new(0, 1, 1, 3); // y=1..3 inclusive
        let flipped = flip_buffer(&buf, FlipAxis::Vertical, bounds);
        // y=1 ↔ y=3.
        assert_eq!(
            flipped.get_pixel(IVec2::new(0, 1)),
            Rgba32F::new(0.9, 0.0, 0.0, 1.0)
        );
        assert_eq!(
            flipped.get_pixel(IVec2::new(0, 3)),
            Rgba32F::new(0.5, 0.0, 0.0, 1.0)
        );
    }

    #[test]
    fn double_flip_restores_original() {
        let mut buf = TiledBuffer::new();
        set(&mut buf, 5, 7, Rgba32F::new(0.4, 0.6, 0.8, 1.0));
        set(&mut buf, 10, 2, Rgba32F::new(0.1, 0.2, 0.3, 1.0));
        let bounds = buf.exact_bounds().unwrap();
        let once = flip_buffer(&buf, FlipAxis::Horizontal, bounds);
        let twice = flip_buffer(&once, FlipAxis::Horizontal, bounds);
        assert_eq!(&twice, &buf, "double horizontal flip is identity");
    }

    #[test]
    fn empty_bounds_yields_empty_buffer() {
        let mut buf = TiledBuffer::new();
        set(&mut buf, 1, 1, Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        let flipped = flip_buffer(&buf, FlipAxis::Horizontal, Rect::new(0, 0, 0, 0));
        assert!(flipped.is_empty());
    }
}
