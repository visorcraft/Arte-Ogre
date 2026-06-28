// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Tile and pixel coordinate types.
//!
//! Coordinates use a top-left origin with +y down. Tiles live on an infinite
//! signed grid; negative tile coordinates are valid. All tile math uses floored
//! (`div_euclid` / `rem_euclid`) division so negative coordinates round toward
//! negative infinity, not toward zero.

use crate::tile::TILE_SIZE;

pub use glam::IVec2;

/// Tile side length as an `i32`.
pub const fn tile_size_i() -> i32 {
    TILE_SIZE as i32
}

/// A tile coordinate on the infinite signed grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct TileCoord {
    /// Horizontal tile index.
    pub x: i32,
    /// Vertical tile index.
    pub y: i32,
}

/// An axis-aligned integer rectangle.
///
/// `(x, y)` is the top-left corner (inclusive); `w` and `h` are the width and
/// height in pixels. An empty rectangle has `w == 0` or `h == 0`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Rect {
    /// Left edge, inclusive.
    pub x: i32,
    /// Top edge, inclusive.
    pub y: i32,
    /// Width in pixels.
    pub w: u32,
    /// Height in pixels.
    pub h: u32,
}

/// Map a pixel coordinate to its containing tile and local tile pixel.
///
/// Uses floored division so negative document coordinates map to the correct
/// tile and local index (e.g. `(-1, -1)` → tile `(-1, -1)`, local `(255, 255)`).
pub fn pixel_to_tile(p: IVec2) -> (TileCoord, usize, usize) {
    let t = tile_size_i();
    let tx = p.x.div_euclid(t);
    let ty = p.y.div_euclid(t);
    let lx = p.x.rem_euclid(t) as usize;
    let ly = p.y.rem_euclid(t) as usize;
    (TileCoord { x: tx, y: ty }, lx, ly)
}

/// Map a tile coordinate and local pixel index back to a document pixel
/// coordinate.
///
/// This is the inverse of [`pixel_to_tile`]: for every `i32` coordinate `p`,
/// `tile_to_pixel(pixel_to_tile(p).0, pixel_to_tile(p).1, pixel_to_tile(p).2)`
/// returns `Some(p)`.
///
/// Returns `None` if the resulting coordinate does not fit in an `i32`.
pub fn tile_to_pixel(c: TileCoord, lx: usize, ly: usize) -> Option<IVec2> {
    let t = tile_size_i() as i64;
    let x = (c.x as i64).checked_mul(t)?.checked_add(lx as i64)?;
    let y = (c.y as i64).checked_mul(t)?.checked_add(ly as i64)?;
    Some(IVec2::new(i32::try_from(x).ok()?, i32::try_from(y).ok()?))
}

/// The pixel rectangle occupied by a single tile.
///
/// Returns `None` if the tile's origin does not fit in an `i32`.
pub fn tile_rect(c: TileCoord) -> Option<Rect> {
    let size = tile_size_i() as i64;
    let x = (c.x as i64).checked_mul(size)?;
    let y = (c.y as i64).checked_mul(size)?;
    if x < i32::MIN as i64 || x > i32::MAX as i64 || y < i32::MIN as i64 || y > i32::MAX as i64 {
        return None;
    }
    Some(Rect::new(
        x as i32,
        y as i32,
        TILE_SIZE as u32,
        TILE_SIZE as u32,
    ))
}

/// Convert a document-space coordinate to layer-local space given the layer
/// `offset` (`doc = local + offset`), returning `None` on `i32` overflow.
pub fn local_coord(doc_p: IVec2, offset: IVec2) -> Option<IVec2> {
    let x = doc_p.x.checked_sub(offset.x)?;
    let y = doc_p.y.checked_sub(offset.y)?;
    Some(IVec2::new(x, y))
}

impl Rect {
    /// Create a rectangle from origin and size.
    pub const fn new(x: i32, y: i32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    /// Create the smallest rectangle containing both integer corners, inclusive.
    ///
    /// Overflowing dimensions are clamped to `u32::MAX`.
    pub fn from_corners(a: IVec2, b: IVec2) -> Self {
        let left = std::cmp::min(a.x, b.x) as i64;
        let top = std::cmp::min(a.y, b.y) as i64;
        let right = std::cmp::max(a.x, b.x) as i64 + 1;
        let bottom = std::cmp::max(a.y, b.y) as i64 + 1;
        let w = u32::try_from(right - left).unwrap_or(u32::MAX);
        let h = u32::try_from(bottom - top).unwrap_or(u32::MAX);
        Self::new(left as i32, top as i32, w, h)
    }

    /// Exclusive right edge (`x + w`).
    pub fn right(self) -> i64 {
        self.x as i64 + self.w as i64
    }

    /// Exclusive bottom edge (`y + h`).
    pub fn bottom(self) -> i64 {
        self.y as i64 + self.h as i64
    }

    /// True if the rectangle contains no pixels.
    pub fn is_empty(self) -> bool {
        self.w == 0 || self.h == 0
    }

    /// Number of pixels in the rectangle, as `u64` to avoid overflow.
    pub fn area(self) -> u64 {
        self.w as u64 * self.h as u64
    }

    /// True if the rectangle contains the point.
    pub fn contains(self, p: IVec2) -> bool {
        let px = p.x as i64;
        let py = p.y as i64;
        px >= self.x as i64 && px < self.right() && py >= self.y as i64 && py < self.bottom()
    }

    /// Intersection of two rectangles, or `None` if they do not overlap.
    pub fn intersect(self, other: Self) -> Option<Self> {
        let left = std::cmp::max(self.x as i64, other.x as i64);
        let top = std::cmp::max(self.y as i64, other.y as i64);
        let right = std::cmp::min(self.right(), other.right());
        let bottom = std::cmp::min(self.bottom(), other.bottom());
        if left >= right || top >= bottom {
            return None;
        }
        let w = u32::try_from(right - left).ok()?;
        let h = u32::try_from(bottom - top).ok()?;
        Some(Self::new(left as i32, top as i32, w, h))
    }

    /// Smallest rectangle containing both rectangles.
    ///
    /// Overflowing dimensions are clamped to `u32::MAX`.
    pub fn union(self, other: Self) -> Self {
        let left = std::cmp::min(self.x as i64, other.x as i64);
        let top = std::cmp::min(self.y as i64, other.y as i64);
        let right = std::cmp::max(self.right(), other.right());
        let bottom = std::cmp::max(self.bottom(), other.bottom());
        let w = u32::try_from(right - left).unwrap_or(u32::MAX);
        let h = u32::try_from(bottom - top).unwrap_or(u32::MAX);
        Self::new(left as i32, top as i32, w, h)
    }

    /// Expand this rectangle by `margin` pixels on all sides.
    ///
    /// Overflowing coordinates are clamped to `i32` range. An empty rectangle
    /// is returned unchanged.
    pub fn expand(self, margin: i32) -> Self {
        if self.is_empty() {
            return self;
        }
        let m = margin as i64;
        let left = (self.x as i64)
            .saturating_sub(m)
            .clamp(i32::MIN as i64, i32::MAX as i64);
        let top = (self.y as i64)
            .saturating_sub(m)
            .clamp(i32::MIN as i64, i32::MAX as i64);
        let right = self
            .right()
            .saturating_add(m)
            .clamp(i32::MIN as i64, i32::MAX as i64);
        let bottom = self
            .bottom()
            .saturating_add(m)
            .clamp(i32::MIN as i64, i32::MAX as i64);
        if right <= left || bottom <= top {
            return Self::new(left as i32, top as i32, 0, 0);
        }
        let w = u32::try_from(right - left).unwrap_or(u32::MAX);
        let h = u32::try_from(bottom - top).unwrap_or(u32::MAX);
        Self::new(left as i32, top as i32, w, h)
    }

    /// Shrink this rectangle by `margin` pixels on all sides.
    ///
    /// If the margin is larger than half the rectangle's size, an empty
    /// rectangle is returned. Overflowing coordinates are clamped to `i32` range.
    pub fn shrink(self, margin: i32) -> Self {
        if self.is_empty() {
            return self;
        }
        let m = margin as i64;
        let left = (self.x as i64)
            .saturating_add(m)
            .clamp(i32::MIN as i64, i32::MAX as i64);
        let top = (self.y as i64)
            .saturating_add(m)
            .clamp(i32::MIN as i64, i32::MAX as i64);
        let right = self
            .right()
            .saturating_sub(m)
            .clamp(i32::MIN as i64, i32::MAX as i64);
        let bottom = self
            .bottom()
            .saturating_sub(m)
            .clamp(i32::MIN as i64, i32::MAX as i64);
        if right <= left || bottom <= top {
            return Self::new(left as i32, top as i32, 0, 0);
        }
        let w = u32::try_from(right - left).unwrap_or(u32::MAX);
        let h = u32::try_from(bottom - top).unwrap_or(u32::MAX);
        Self::new(left as i32, top as i32, w, h)
    }

    /// Iterate over every tile coordinate touched by this rectangle.
    pub fn tiles_covered(self) -> impl Iterator<Item = TileCoord> {
        let t = tile_size_i() as i64;
        let mut tx_start = (self.x as i64).div_euclid(t);
        let mut tx_end = (self.right() - 1).div_euclid(t);
        let mut ty_start = (self.y as i64).div_euclid(t);
        let mut ty_end = (self.bottom() - 1).div_euclid(t);
        if self.is_empty() {
            // Return an empty iterator by using an empty range.
            tx_start = 1;
            tx_end = 0;
            ty_start = 1;
            ty_end = 0;
        }
        (ty_start..=ty_end).flat_map(move |ty| {
            (tx_start..=tx_end).map(move |tx| TileCoord {
                x: tx as i32,
                y: ty as i32,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_contains_and_excludes_points() {
        let r = Rect::new(990, 690, 40, 40);
        assert!(r.contains(IVec2::new(1000, 700)));
        assert!(!r.contains(IVec2::new(1031, 700)));
        assert!(r.contains(IVec2::new(990, 690))); // top-left inclusive
        assert!(!r.contains(IVec2::new(990, 730))); // bottom exclusive
    }

    #[test]
    fn rect_intersection_basic() {
        let a = Rect::new(0, 0, 100, 100);
        let b = Rect::new(50, 50, 100, 100);
        assert_eq!(a.intersect(b), Some(Rect::new(50, 50, 50, 50)));
    }

    #[test]
    fn rect_intersection_empty() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(20, 20, 10, 10);
        assert_eq!(a.intersect(b), None);
    }

    #[test]
    fn empty_rect_intersects_none() {
        let a = Rect::new(0, 0, 0, 10);
        let b = Rect::new(0, 0, 10, 10);
        assert_eq!(a.intersect(b), None);
    }

    #[test]
    fn pixel_to_tile_positive() {
        let (c, lx, ly) = pixel_to_tile(IVec2::new(0, 0));
        assert_eq!(c, TileCoord { x: 0, y: 0 });
        assert_eq!((lx, ly), (0, 0));

        let (c, lx, ly) = pixel_to_tile(IVec2::new(255, 256));
        assert_eq!(c, TileCoord { x: 0, y: 1 });
        assert_eq!((lx, ly), (255, 0));
    }

    #[test]
    fn pixel_to_tile_negative_uses_floor_division() {
        let (c, lx, ly) = pixel_to_tile(IVec2::new(-1, -1));
        assert_eq!(c, TileCoord { x: -1, y: -1 });
        assert_eq!((lx, ly), (255, 255));

        let (c, lx, ly) = pixel_to_tile(IVec2::new(-256, -257));
        assert_eq!(c, TileCoord { x: -1, y: -2 });
        assert_eq!((lx, ly), (0, 255));
    }

    #[test]
    fn tile_to_pixel_round_trips() {
        let samples = [
            IVec2::new(0, 0),
            IVec2::new(255, 256),
            IVec2::new(-1, -1),
            IVec2::new(-256, -257),
            IVec2::new(i32::MAX, i32::MIN),
        ];
        for p in samples {
            let (c, lx, ly) = pixel_to_tile(p);
            assert_eq!(tile_to_pixel(c, lx, ly), Some(p));
        }
    }

    #[test]
    fn tile_to_pixel_returns_none_on_overflow() {
        let far = TileCoord {
            x: i32::MAX / 256 + 2,
            y: 0,
        };
        assert!(tile_to_pixel(far, 0, 0).is_none());
    }

    #[test]
    fn tiles_covered_for_40x40_rect() {
        // A 40x40 rect at (990,690) crosses the x tile boundary but stays
        // within tile row y=2 (tile rows span 256 px starting at y=512).
        let r = Rect::new(990, 690, 40, 40);
        let tiles: Vec<_> = r.tiles_covered().collect();
        assert!(tiles.contains(&TileCoord { x: 3, y: 2 }));
        assert!(tiles.contains(&TileCoord { x: 4, y: 2 }));
        assert_eq!(tiles.len(), 2);
    }

    #[test]
    fn tiles_covered_negative_rect() {
        let r = Rect::new(-10, -10, 20, 20);
        let tiles: Vec<_> = r.tiles_covered().collect();
        assert!(tiles.contains(&TileCoord { x: -1, y: -1 }));
        assert!(tiles.contains(&TileCoord { x: 0, y: -1 }));
        assert!(tiles.contains(&TileCoord { x: -1, y: 0 }));
        assert!(tiles.contains(&TileCoord { x: 0, y: 0 }));
    }

    #[test]
    fn rect_expand_grows_by_margin() {
        let r = Rect::new(10, 20, 30, 40);
        let expanded = r.expand(5);
        assert_eq!(expanded, Rect::new(5, 15, 40, 50));
    }

    #[test]
    fn rect_shrink_contracts_by_margin() {
        let r = Rect::new(10, 20, 30, 40);
        let shrunk = r.shrink(5);
        assert_eq!(shrunk, Rect::new(15, 25, 20, 30));
    }

    #[test]
    fn rect_shrink_to_empty_returns_empty() {
        let r = Rect::new(0, 0, 5, 5);
        assert!(r.shrink(3).is_empty());
    }
}
