// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Sparse tiled copy-on-write pixel buffer.
//!
//! A [`TiledBuffer`] stores pixels in a hash map of 256×256 tiles. Tiles are
//! shared through [`Arc`] and cloned on write via [`Arc::make_mut`], so layer
//! duplication, undo snapshots, and extracts are cheap and pixel-exact.

use std::sync::Arc;

use ahash::AHashMap;

use crate::coord::{pixel_to_tile, tile_rect, IVec2, Rect, TileCoord};
use crate::pixel::Rgba32F;
use crate::tile::Tile;

/// A sparse, copy-on-write pixel buffer made of 256×256 tiles.
///
/// Every tile is resident in RAM, shared through [`Arc`] and cloned on write
/// via [`Arc::make_mut`], so layer duplication, undo snapshots, and extracts
/// are cheap and pixel-exact.
#[derive(Debug, Default, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TiledBuffer {
    tiles: AHashMap<TileCoord, Arc<Tile>>,
}

impl TiledBuffer {
    /// Create an empty buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a buffer from an existing tile map.
    pub fn from_tiles(tiles: AHashMap<TileCoord, Arc<Tile>>) -> Self {
        Self { tiles }
    }

    /// Number of tiles currently held in RAM (resident), for tests/diagnostics.
    pub fn resident_tile_count(&self) -> usize {
        self.tiles.len()
    }

    /// Access a shared tile by coordinate, if it exists.
    ///
    /// Returns an owned `Arc` (a cheap refcount clone).
    pub fn get_tile(&self, c: TileCoord) -> Option<Arc<Tile>> {
        self.tiles.get(&c).map(Arc::clone)
    }

    /// Read a pixel in document coordinates.
    ///
    /// Returns [`Rgba32F::TRANSPARENT`] if no tile exists at the coordinate.
    pub fn get_pixel(&self, p: IVec2) -> Rgba32F {
        let (c, lx, ly) = pixel_to_tile(p);
        self.tiles
            .get(&c)
            .map(|t| t.get(lx, ly))
            .unwrap_or(Rgba32F::TRANSPARENT)
    }

    /// Obtain a mutable reference to a tile, cloning it first if it is shared.
    ///
    /// Creates a transparent tile if the coordinate has no tile yet.
    pub fn tile_mut(&mut self, c: TileCoord) -> &mut Tile {
        let arc = self
            .tiles
            .entry(c)
            .or_insert_with(|| Arc::new(Tile::transparent()));
        Arc::make_mut(arc)
    }

    /// Write a pixel in document coordinates.
    pub fn set_pixel(&mut self, p: IVec2, px: Rgba32F) {
        let (c, lx, ly) = pixel_to_tile(p);
        // Don't materialise a tile just to store a transparent pixel.
        if px == Rgba32F::TRANSPARENT && !self.tiles.contains_key(&c) {
            return;
        }
        let tile = self.tile_mut(c);
        tile.set(lx, ly, px);
        // Keep the buffer sparse: drop tiles that became fully transparent. A
        // non-transparent write can never empty a tile, so skip the full
        // 65,536-element scan in that (hot, common) case — doing it
        // unconditionally made per-pixel ops like cut/erase O(pixels × tile).
        // The tile maintains an O(1) non-transparent pixel count for this
        // check.
        if px == Rgba32F::TRANSPARENT && tile.is_empty() {
            self.tiles.remove(&c);
        }
    }

    /// Return the bounding rectangle of all occupied tiles, or `None` if empty.
    pub fn bounds(&self) -> Option<Rect> {
        self.tiles
            .keys()
            .copied()
            .filter_map(tile_rect)
            .fold(None, |acc, r| {
                Some(acc.map(|a: Rect| a.union(r)).unwrap_or(r))
            })
    }

    /// Return the exact bounding rectangle of all non-transparent pixels, or
    /// `None` if the buffer is fully transparent.
    pub fn exact_bounds(&self) -> Option<Rect> {
        let mut min = None;
        let mut max = None;
        let t = crate::tile::TILE_SIZE as i32;

        for (&c, tile) in &self.tiles {
            let base_x = c.x.checked_mul(t)?;
            let base_y = c.y.checked_mul(t)?;
            for ly in 0..crate::tile::TILE_SIZE {
                for lx in 0..crate::tile::TILE_SIZE {
                    if tile.get(lx, ly) != Rgba32F::TRANSPARENT {
                        let x = base_x + lx as i32;
                        let y = base_y + ly as i32;
                        min = Some(
                            min.map(|m: IVec2| m.min(IVec2::new(x, y)))
                                .unwrap_or_else(|| IVec2::new(x, y)),
                        );
                        max = Some(
                            max.map(|m: IVec2| m.max(IVec2::new(x, y)))
                                .unwrap_or_else(|| IVec2::new(x, y)),
                        );
                    }
                }
            }
        }

        match (min, max) {
            (Some(a), Some(b)) => Some(Rect::from_corners(a, b)),
            _ => None,
        }
    }

    /// Collect all occupied tiles and their shared storage.
    pub fn occupied_tiles(&self) -> Vec<(TileCoord, Arc<Tile>)> {
        self.tiles
            .iter()
            .map(|(c, t)| (*c, Arc::clone(t)))
            .collect()
    }

    /// True if the buffer contains no tiles.
    pub fn is_empty(&self) -> bool {
        self.tiles.is_empty()
    }

    /// Remove tiles whose pixels are all transparent.
    pub fn prune_empty_tiles(&mut self) {
        self.tiles.retain(|_, tile| !tile.is_empty());
    }

    /// Read a rectangular region into a row-major `Vec`.
    ///
    /// # Panics
    ///
    /// Panics if `rect.w * rect.h` overflows `u32`.
    pub fn read_rect(&self, rect: Rect) -> Vec<Rgba32F> {
        let w = rect.w as usize;
        let h = rect.h as usize;
        let area = w.checked_mul(h).expect("rect dimensions too large");
        let mut out = vec![Rgba32F::TRANSPARENT; area];
        if area == 0 {
            return out;
        }
        let rx = rect.x as i64;
        let ry = rect.y as i64;
        for c in rect.tiles_covered() {
            let Some(t_rect) = tile_rect(c) else {
                continue;
            };
            let inter = t_rect.intersect(rect).unwrap();
            let tile = self.get_tile(c);
            let tx = t_rect.x as i64;
            let ty = t_rect.y as i64;
            for y in inter.y as i64..inter.bottom() {
                for x in inter.x as i64..inter.right() {
                    let lx = (x - tx) as usize;
                    let ly = (y - ty) as usize;
                    let px = tile
                        .as_ref()
                        .map(|t| t.get(lx, ly))
                        .unwrap_or(Rgba32F::TRANSPARENT);
                    let dx = (x - rx) as usize;
                    let dy = (y - ry) as usize;
                    out[dy * w + dx] = px;
                }
            }
        }
        out
    }

    /// Write a row-major rectangle of pixels into the buffer.
    ///
    /// # Panics
    ///
    /// Panics if `data.len()` does not equal `rect.w * rect.h`, or if
    /// `rect.w * rect.h` overflows `u32`.
    pub fn blit_rect(&mut self, rect: Rect, data: &[Rgba32F]) {
        let w = rect.w as usize;
        let h = rect.h as usize;
        let area = w.checked_mul(h).expect("rect dimensions too large");
        assert_eq!(data.len(), area, "data length must equal rect.w * rect.h");
        if area == 0 {
            return;
        }
        let rx = rect.x as i64;
        let ry = rect.y as i64;
        for c in rect.tiles_covered() {
            let Some(t_rect) = tile_rect(c) else {
                continue;
            };
            let inter = t_rect.intersect(rect).unwrap();

            // Avoid materialising a tile if this source region is entirely
            // transparent and the buffer has no tile there yet.
            let has_tile = self.tiles.contains_key(&c);
            if !has_tile {
                let all_transparent = (inter.y as i64..inter.bottom()).all(|y| {
                    let dy = (y - ry) as usize;
                    (inter.x as i64..inter.right())
                        .all(|x| data[dy * w + (x - rx) as usize] == Rgba32F::TRANSPARENT)
                });
                if all_transparent {
                    continue;
                }
            }

            let tile = self.tile_mut(c);
            let tx = t_rect.x as i64;
            let ty = t_rect.y as i64;
            for y in inter.y as i64..inter.bottom() {
                for x in inter.x as i64..inter.right() {
                    let lx = (x - tx) as usize;
                    let ly = (y - ty) as usize;
                    let dx = (x - rx) as usize;
                    let dy = (y - ry) as usize;
                    tile.set(lx, ly, data[dy * w + dx]);
                }
            }
        }
        self.prune_empty_tiles();
    }

    /// Set every pixel in `rect` to transparent and remove empty tiles.
    pub fn clear_rect(&mut self, rect: Rect) {
        for c in rect.tiles_covered() {
            if !self.tiles.contains_key(&c) {
                continue;
            }
            let Some(t_rect) = tile_rect(c) else {
                continue;
            };
            let inter = t_rect.intersect(rect).unwrap();
            let tile = self.tile_mut(c);
            for y in inter.y as i64..inter.bottom() {
                for x in inter.x as i64..inter.right() {
                    let lx = (x - t_rect.x as i64) as usize;
                    let ly = (y - t_rect.y as i64) as usize;
                    tile.set(lx, ly, Rgba32F::TRANSPARENT);
                }
            }
        }
        self.prune_empty_tiles();
    }

    /// Copy a rectangular region from `src` into this buffer at `dst_offset`.
    pub fn copy_region(&mut self, src: &Self, src_rect: Rect, dst_offset: IVec2) {
        if src_rect.is_empty() {
            return;
        }
        for c in src_rect.tiles_covered() {
            let Some(tile) = src.get_tile(c) else {
                continue;
            };
            let Some(t_rect) = tile_rect(c) else {
                continue;
            };
            let inter = t_rect.intersect(src_rect).unwrap();
            for y in inter.y as i64..inter.bottom() {
                for x in inter.x as i64..inter.right() {
                    let lx = (x - t_rect.x as i64) as usize;
                    let ly = (y - t_rect.y as i64) as usize;
                    let px = tile.get(lx, ly);
                    if px != Rgba32F::TRANSPARENT {
                        let dst_x = x + dst_offset.x as i64;
                        let dst_y = y + dst_offset.y as i64;
                        if let (Ok(dx), Ok(dy)) = (i32::try_from(dst_x), i32::try_from(dst_y)) {
                            self.set_pixel(IVec2::new(dx, dy), px);
                        }
                    }
                }
            }
        }
    }
}

impl TiledBuffer {
    /// Restore a tile coordinate to a previously captured [`Arc<Tile>`].
    ///
    /// This crate-private helper is the primitive used by the undo system to
    /// put back the exact shared tile that existed before an edit. Restoring
    /// by [`Arc`] clone keeps undo snapshots O(touched tiles) and avoids deep
    /// pixel copies.
    pub(crate) fn restore_tile(&mut self, c: TileCoord, tile: Option<Arc<Tile>>) {
        match tile {
            Some(t) => {
                self.tiles.insert(c, t);
            }
            None => {
                self.tiles.remove(&c);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use proptest::prelude::*;

    use super::*;

    #[test]
    fn cow_isolation() {
        let mut a = TiledBuffer::new();
        a.set_pixel(IVec2::new(10, 10), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        let b = a.clone();
        let shared_before = a.get_tile(TileCoord { x: 0, y: 0 }).unwrap();

        let mut b2 = b.clone();
        b2.set_pixel(IVec2::new(10, 10), Rgba32F::new(0.0, 1.0, 0.0, 1.0));

        assert_eq!(a.get_pixel(IVec2::new(10, 10)).g, 0.0); // original untouched
        assert_eq!(b2.get_pixel(IVec2::new(10, 10)).g, 1.0); // clone mutated
        assert!(Arc::ptr_eq(
            &shared_before,
            &b.get_tile(TileCoord { x: 0, y: 0 }).unwrap()
        )); // unshared tile still shared
    }

    #[test]
    fn get_pixel_outside_returns_transparent() {
        let buffer = TiledBuffer::new();
        assert_eq!(buffer.get_pixel(IVec2::new(-5, 1000)), Rgba32F::TRANSPARENT);
    }

    #[test]
    fn set_and_get_negative_pixel() {
        let mut buffer = TiledBuffer::new();
        let p = IVec2::new(-1, -1);
        let c = Rgba32F::new(0.2, 0.4, 0.6, 0.8);
        buffer.set_pixel(p, c);
        assert_eq!(buffer.get_pixel(p), c);
        assert_eq!(
            buffer
                .get_tile(TileCoord { x: -1, y: -1 })
                .unwrap()
                .get(255, 255),
            c
        );
    }

    #[test]
    fn bounds_on_empty_is_none() {
        assert!(TiledBuffer::new().bounds().is_none());
    }

    #[test]
    fn bounds_unions_occupied_tiles() {
        let mut buffer = TiledBuffer::new();
        buffer.set_pixel(IVec2::new(10, 10), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        buffer.set_pixel(IVec2::new(300, 600), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        // Tiles at (0,0) and (1,2).
        assert_eq!(buffer.bounds(), Some(Rect::new(0, 0, 512, 768)));
    }

    #[test]
    fn exact_bounds_on_empty_is_none() {
        assert!(TiledBuffer::new().exact_bounds().is_none());
    }

    #[test]
    fn exact_bounds_is_pixel_precise() {
        let mut buffer = TiledBuffer::new();
        buffer.set_pixel(IVec2::new(10, 10), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        buffer.set_pixel(IVec2::new(12, 15), Rgba32F::new(0.0, 1.0, 0.0, 1.0));
        assert_eq!(buffer.exact_bounds(), Some(Rect::new(10, 10, 3, 6)));
    }

    #[test]
    fn exact_bounds_ignores_fully_transparent_pixels() {
        let mut buffer = TiledBuffer::new();
        buffer.set_pixel(IVec2::new(5, 5), Rgba32F::TRANSPARENT);
        buffer.set_pixel(IVec2::new(20, 20), Rgba32F::new(0.0, 0.0, 1.0, 1.0));
        assert_eq!(buffer.exact_bounds(), Some(Rect::new(20, 20, 1, 1)));
    }

    #[test]
    fn exact_bounds_handles_negative_pixels() {
        let mut buffer = TiledBuffer::new();
        buffer.set_pixel(IVec2::new(-3, -7), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        buffer.set_pixel(IVec2::new(-1, -1), Rgba32F::new(0.0, 1.0, 0.0, 1.0));
        assert_eq!(buffer.exact_bounds(), Some(Rect::new(-3, -7, 3, 7)));
    }

    proptest! {
        #[test]
        fn cow_isolation_proptest(
            writes in prop::collection::vec(
                (-300i32..=300, -300i32..=300, 0u8..4, 0u8..=255u8),
                1..100usize,
            )
        ) {
            let mut original = TiledBuffer::new();
            let seed = IVec2::new(0, 0);
            original.set_pixel(seed, Rgba32F::new(1.0, 0.0, 0.0, 1.0));

            let snapshot = original.clone();
            let mut clone = original.clone();

            for (x, y, ch, v) in &writes {
                let f = *v as f32 / 255.0;
                let px = match *ch {
                    0 => Rgba32F::new(f, 0.0, 0.0, 1.0),
                    1 => Rgba32F::new(0.0, f, 0.0, 1.0),
                    2 => Rgba32F::new(0.0, 0.0, f, 1.0),
                    _ => Rgba32F::new(f, f, f, 1.0),
                };
                clone.set_pixel(IVec2::new(*x, *y), px);
            }

            // Original and snapshot must remain unchanged.
            assert_eq!(original.get_pixel(seed), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
            assert_eq!(snapshot.get_pixel(seed), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

            // The tile at (0,0) was never written in `original`/`snapshot`, so it
            // must still be Arc-shared between them.
            assert!(Arc::ptr_eq(
                &original.get_tile(TileCoord { x: 0, y: 0 }).unwrap(),
                &snapshot.get_tile(TileCoord { x: 0, y: 0 }).unwrap(),
            ));
        }
    }

    #[test]
    fn read_rect_row_major() {
        let mut buffer = TiledBuffer::new();
        buffer.set_pixel(IVec2::new(0, 0), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        buffer.set_pixel(IVec2::new(1, 0), Rgba32F::new(0.0, 1.0, 0.0, 1.0));
        buffer.set_pixel(IVec2::new(0, 1), Rgba32F::new(0.0, 0.0, 1.0, 1.0));
        buffer.set_pixel(IVec2::new(1, 1), Rgba32F::new(1.0, 1.0, 1.0, 1.0));

        let data = buffer.read_rect(Rect::new(0, 0, 2, 2));
        assert_eq!(data.len(), 4);
        assert_eq!(data[0], Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        assert_eq!(data[1], Rgba32F::new(0.0, 1.0, 0.0, 1.0));
        assert_eq!(data[2], Rgba32F::new(0.0, 0.0, 1.0, 1.0));
        assert_eq!(data[3], Rgba32F::new(1.0, 1.0, 1.0, 1.0));
    }

    #[test]
    fn blit_rect_roundtrip() {
        let mut buffer = TiledBuffer::new();
        let data = vec![
            Rgba32F::new(1.0, 0.0, 0.0, 1.0),
            Rgba32F::new(0.0, 1.0, 0.0, 1.0),
            Rgba32F::new(0.0, 0.0, 1.0, 1.0),
            Rgba32F::new(1.0, 1.0, 1.0, 1.0),
        ];
        buffer.blit_rect(Rect::new(5, 5, 2, 2), &data);
        assert_eq!(buffer.read_rect(Rect::new(5, 5, 2, 2)), data);
    }

    #[test]
    fn clear_rect_clears_and_prunes() {
        let mut buffer = TiledBuffer::new();
        buffer.set_pixel(IVec2::new(0, 0), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        buffer.set_pixel(IVec2::new(1, 0), Rgba32F::new(0.0, 1.0, 0.0, 1.0));
        buffer.set_pixel(IVec2::new(300, 0), Rgba32F::new(0.0, 0.0, 1.0, 1.0));

        buffer.clear_rect(Rect::new(0, 0, 2, 1));
        assert_eq!(buffer.get_pixel(IVec2::new(0, 0)), Rgba32F::TRANSPARENT);
        assert_eq!(buffer.get_pixel(IVec2::new(1, 0)), Rgba32F::TRANSPARENT);
        assert_eq!(
            buffer.get_pixel(IVec2::new(300, 0)),
            Rgba32F::new(0.0, 0.0, 1.0, 1.0)
        );
        // The tile at (0,0) became empty and should be pruned.
        assert!(buffer.get_tile(TileCoord { x: 0, y: 0 }).is_none());
    }

    #[test]
    fn copy_region_places_pixels_exactly() {
        let mut src = TiledBuffer::new();
        src.set_pixel(IVec2::new(10, 10), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        src.set_pixel(IVec2::new(11, 10), Rgba32F::new(0.0, 1.0, 0.0, 1.0));

        let mut dst = TiledBuffer::new();
        dst.copy_region(&src, Rect::new(10, 10, 2, 1), IVec2::new(100, 200));

        assert_eq!(
            dst.get_pixel(IVec2::new(110, 210)),
            Rgba32F::new(1.0, 0.0, 0.0, 1.0)
        );
        assert_eq!(
            dst.get_pixel(IVec2::new(111, 210)),
            Rgba32F::new(0.0, 1.0, 0.0, 1.0)
        );
        assert_eq!(dst.get_pixel(IVec2::new(10, 10)), Rgba32F::TRANSPARENT);
    }

    #[test]
    fn read_rect_spans_four_tiles() {
        let mut buffer = TiledBuffer::new();
        // Place pixels around the corner shared by tiles (0,0), (1,0), (0,1), (1,1).
        buffer.set_pixel(IVec2::new(255, 255), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        buffer.set_pixel(IVec2::new(256, 255), Rgba32F::new(0.0, 1.0, 0.0, 1.0));
        buffer.set_pixel(IVec2::new(255, 256), Rgba32F::new(0.0, 0.0, 1.0, 1.0));
        buffer.set_pixel(IVec2::new(256, 256), Rgba32F::new(1.0, 1.0, 1.0, 1.0));

        let data = buffer.read_rect(Rect::new(255, 255, 2, 2));
        assert_eq!(data.len(), 4);
        assert_eq!(data[0], Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        assert_eq!(data[1], Rgba32F::new(0.0, 1.0, 0.0, 1.0));
        assert_eq!(data[2], Rgba32F::new(0.0, 0.0, 1.0, 1.0));
        assert_eq!(data[3], Rgba32F::new(1.0, 1.0, 1.0, 1.0));
    }

    #[test]
    fn read_rect_negative_origin() {
        let mut buffer = TiledBuffer::new();
        buffer.set_pixel(IVec2::new(-1, -1), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        buffer.set_pixel(IVec2::new(0, -1), Rgba32F::new(0.0, 1.0, 0.0, 1.0));
        buffer.set_pixel(IVec2::new(-1, 0), Rgba32F::new(0.0, 0.0, 1.0, 1.0));
        buffer.set_pixel(IVec2::new(0, 0), Rgba32F::new(1.0, 1.0, 1.0, 1.0));

        let data = buffer.read_rect(Rect::new(-1, -1, 2, 2));
        assert_eq!(data.len(), 4);
        assert_eq!(data[0], Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        assert_eq!(data[1], Rgba32F::new(0.0, 1.0, 0.0, 1.0));
        assert_eq!(data[2], Rgba32F::new(0.0, 0.0, 1.0, 1.0));
        assert_eq!(data[3], Rgba32F::new(1.0, 1.0, 1.0, 1.0));
    }

    #[test]
    fn blit_rect_spans_four_tiles() {
        let mut buffer = TiledBuffer::new();
        let data = vec![
            Rgba32F::new(1.0, 0.0, 0.0, 1.0),
            Rgba32F::new(0.0, 1.0, 0.0, 1.0),
            Rgba32F::new(0.0, 0.0, 1.0, 1.0),
            Rgba32F::new(1.0, 1.0, 1.0, 1.0),
        ];
        buffer.blit_rect(Rect::new(255, 255, 2, 2), &data);
        assert_eq!(buffer.read_rect(Rect::new(255, 255, 2, 2)), data);
    }

    #[test]
    fn blit_rect_negative_offset() {
        let mut buffer = TiledBuffer::new();
        let data = vec![
            Rgba32F::new(1.0, 0.0, 0.0, 1.0),
            Rgba32F::new(0.0, 1.0, 0.0, 1.0),
            Rgba32F::new(0.0, 0.0, 1.0, 1.0),
            Rgba32F::new(1.0, 1.0, 1.0, 1.0),
        ];
        buffer.blit_rect(Rect::new(-1, -1, 2, 2), &data);
        assert_eq!(buffer.read_rect(Rect::new(-1, -1, 2, 2)), data);
    }

    #[test]
    fn blit_rect_prunes_empty_tiles() {
        let mut buffer = TiledBuffer::new();
        buffer.set_pixel(IVec2::new(10, 10), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        assert!(buffer.get_tile(TileCoord { x: 0, y: 0 }).is_some());

        let transparent = vec![Rgba32F::TRANSPARENT; 121];
        buffer.blit_rect(Rect::new(0, 0, 11, 11), &transparent);
        assert_eq!(buffer.get_pixel(IVec2::new(10, 10)), Rgba32F::TRANSPARENT);
        assert!(buffer.get_tile(TileCoord { x: 0, y: 0 }).is_none());
    }

    #[test]
    fn copy_region_spans_four_tiles() {
        let mut src = TiledBuffer::new();
        src.set_pixel(IVec2::new(255, 255), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        src.set_pixel(IVec2::new(256, 255), Rgba32F::new(0.0, 1.0, 0.0, 1.0));
        src.set_pixel(IVec2::new(255, 256), Rgba32F::new(0.0, 0.0, 1.0, 1.0));
        src.set_pixel(IVec2::new(256, 256), Rgba32F::new(1.0, 1.0, 1.0, 1.0));

        let mut dst = TiledBuffer::new();
        dst.copy_region(&src, Rect::new(255, 255, 2, 2), IVec2::new(-255, -255));

        assert_eq!(
            dst.get_pixel(IVec2::new(0, 0)),
            Rgba32F::new(1.0, 0.0, 0.0, 1.0)
        );
        assert_eq!(
            dst.get_pixel(IVec2::new(1, 0)),
            Rgba32F::new(0.0, 1.0, 0.0, 1.0)
        );
        assert_eq!(
            dst.get_pixel(IVec2::new(0, 1)),
            Rgba32F::new(0.0, 0.0, 1.0, 1.0)
        );
        assert_eq!(
            dst.get_pixel(IVec2::new(1, 1)),
            Rgba32F::new(1.0, 1.0, 1.0, 1.0)
        );
    }

    #[test]
    fn copy_region_negative_dst_offset() {
        let mut src = TiledBuffer::new();
        src.set_pixel(IVec2::new(0, 0), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        src.set_pixel(IVec2::new(1, 0), Rgba32F::new(0.0, 1.0, 0.0, 1.0));

        let mut dst = TiledBuffer::new();
        dst.copy_region(&src, Rect::new(0, 0, 2, 1), IVec2::new(-10, -10));

        assert_eq!(
            dst.get_pixel(IVec2::new(-10, -10)),
            Rgba32F::new(1.0, 0.0, 0.0, 1.0)
        );
        assert_eq!(
            dst.get_pixel(IVec2::new(-9, -10)),
            Rgba32F::new(0.0, 1.0, 0.0, 1.0)
        );
    }

    #[test]
    fn bounds_drops_out_of_range_tiles() {
        let mut buffer = TiledBuffer::new();
        // Tile coordinate that, when multiplied by 256, overflows i32.
        let far = TileCoord {
            x: i32::MAX / 256 + 2,
            y: 0,
        };
        buffer.tiles.insert(
            far,
            Arc::new(Tile::filled(Rgba32F::new(1.0, 0.0, 0.0, 1.0))),
        );
        // A normal tile should still produce a valid bound.
        buffer.set_pixel(IVec2::new(0, 0), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        assert_eq!(buffer.bounds(), Some(Rect::new(0, 0, 256, 256)));
    }

    #[test]
    fn read_rect_extreme_coords_does_not_panic() {
        let buffer = TiledBuffer::new();
        let far = i32::MAX - 10;
        let _ = buffer.read_rect(Rect::new(far, far, 20, 20));
    }

    #[test]
    fn blit_rect_extreme_coords_does_not_panic() {
        let mut buffer = TiledBuffer::new();
        let far = i32::MAX - 10;
        let data = vec![Rgba32F::new(1.0, 0.0, 0.0, 1.0); 400];
        buffer.blit_rect(Rect::new(far, far, 20, 20), &data);
    }

    #[test]
    fn clear_rect_extreme_coords_does_not_panic() {
        let mut buffer = TiledBuffer::new();
        let far = i32::MAX - 10;
        buffer.clear_rect(Rect::new(far, far, 20, 20));
    }

    #[test]
    fn copy_region_extreme_coords_does_not_panic() {
        let src = TiledBuffer::new();
        let mut dst = TiledBuffer::new();
        let far = i32::MAX - 10;
        dst.copy_region(&src, Rect::new(far, far, 20, 20), IVec2::new(0, 0));
    }

    #[test]
    fn set_pixel_drops_tile_when_it_becomes_empty_using_count() {
        let mut buffer = TiledBuffer::new();
        let p = IVec2::new(10, 10);
        let c = Rgba32F::new(1.0, 0.0, 0.0, 1.0);
        buffer.set_pixel(p, c);

        // The tile should exist and know it has one non-transparent pixel.
        let tile = buffer.get_tile(TileCoord { x: 0, y: 0 }).unwrap();
        assert_eq!(tile.non_transparent_count(), 1);

        // Erasing that single pixel should drop the tile without scanning.
        buffer.set_pixel(p, Rgba32F::TRANSPARENT);
        assert!(buffer.get_tile(TileCoord { x: 0, y: 0 }).is_none());
        assert_eq!(buffer.get_pixel(p), Rgba32F::TRANSPARENT);
    }
}
