// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! A single 256×256 pixel tile.

use crate::pixel::Rgba32F;

/// Width and height of a tile, in pixels.
pub const TILE_SIZE: usize = 256;

/// Number of pixels in a fully-populated tile.
pub(crate) const TILE_PIXELS: usize = TILE_SIZE * TILE_SIZE;

/// A dense 256×256 tile of [`Rgba32F`] pixels.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Tile {
    px: Box<[Rgba32F]>,
    /// Number of pixels that are not fully transparent.
    ///
    /// This is a cached value maintained by every write so that callers can
    /// detect an empty tile in O(1) instead of scanning 65,536 pixels.
    #[serde(skip)]
    non_transparent_count: usize,
}

impl<'de> serde::Deserialize<'de> for Tile {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        struct De {
            px: Box<[Rgba32F]>,
        }
        let de = De::deserialize(deserializer)?;
        if de.px.len() != TILE_PIXELS {
            return Err(serde::de::Error::custom(format!(
                "tile pixel data must contain exactly {TILE_PIXELS} pixels"
            )));
        }
        let non_transparent_count = de.px.iter().filter(|p| **p != Rgba32F::TRANSPARENT).count();
        Ok(Self {
            px: de.px,
            non_transparent_count,
        })
    }
}

impl Tile {
    /// Create a tile filled with fully transparent pixels.
    pub fn transparent() -> Self {
        Self {
            px: vec![Rgba32F::TRANSPARENT; TILE_PIXELS].into_boxed_slice(),
            non_transparent_count: 0,
        }
    }

    /// Create a tile filled with the given color.
    pub fn filled(c: Rgba32F) -> Self {
        let non_transparent_count = if c == Rgba32F::TRANSPARENT {
            0
        } else {
            TILE_PIXELS
        };
        Self {
            px: vec![c; TILE_PIXELS].into_boxed_slice(),
            non_transparent_count,
        }
    }

    /// Read the pixel at local tile coordinates `(lx, ly)`.
    ///
    /// Panics if `lx` or `ly` is out of range.
    pub fn get(&self, lx: usize, ly: usize) -> Rgba32F {
        assert!(lx < TILE_SIZE && ly < TILE_SIZE);
        self.px[ly * TILE_SIZE + lx]
    }

    /// Write the pixel at local tile coordinates `(lx, ly)`.
    ///
    /// Panics if `lx` or `ly` is out of range.
    pub fn set(&mut self, lx: usize, ly: usize, px: Rgba32F) {
        assert!(lx < TILE_SIZE && ly < TILE_SIZE);
        let idx = ly * TILE_SIZE + lx;
        let old = self.px[idx];
        self.px[idx] = px;
        match (old == Rgba32F::TRANSPARENT, px == Rgba32F::TRANSPARENT) {
            (true, false) => self.non_transparent_count += 1,
            (false, true) => self.non_transparent_count -= 1,
            _ => {}
        }
    }

    /// Access the tile as a contiguous slice, row-major.
    pub fn as_slice(&self) -> &[Rgba32F] {
        &self.px
    }

    /// Access the tile as a contiguous mutable slice, row-major.
    ///
    /// # Caution
    ///
    /// Mutating the slice directly bypasses the non-transparent pixel counter.
    /// Callers that modify pixels through this slice must use
    /// [`recount_non_transparent`](Self::recount_non_transparent) afterwards to
    /// keep the cache consistent.
    pub fn as_mut_slice(&mut self) -> &mut [Rgba32F] {
        &mut self.px
    }

    /// Recompute the non-transparent pixel counter from the pixel data.
    ///
    /// Use this after mutating the tile through [`as_mut_slice`](Self::as_mut_slice).
    pub fn recount_non_transparent(&mut self) {
        self.non_transparent_count = self
            .px
            .iter()
            .filter(|p| **p != Rgba32F::TRANSPARENT)
            .count();
    }

    /// Number of pixels that are not fully transparent.
    pub fn non_transparent_count(&self) -> usize {
        self.non_transparent_count
    }

    /// True if every pixel in the tile is fully transparent.
    pub fn is_empty(&self) -> bool {
        self.non_transparent_count == 0
    }

    /// Create a tile from an existing boxed pixel slice.
    ///
    /// # Panics
    ///
    /// Panics if `px` does not contain exactly `TILE_SIZE * TILE_SIZE` pixels.
    pub fn from_boxed_slice(px: Box<[Rgba32F]>) -> Self {
        assert_eq!(
            px.len(),
            TILE_PIXELS,
            "tile slice must contain exactly {} pixels",
            TILE_PIXELS
        );
        let non_transparent_count = px.iter().filter(|p| **p != Rgba32F::TRANSPARENT).count();
        Self {
            px,
            non_transparent_count,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transparent_tile_is_all_zero() {
        let tile = Tile::transparent();
        assert_eq!(tile.px.len(), TILE_PIXELS);
        assert!(tile.px.iter().all(|p| *p == Rgba32F::TRANSPARENT));
    }

    #[test]
    fn filled_tile_has_uniform_color() {
        let c = Rgba32F::new(0.1, 0.2, 0.3, 0.4);
        let tile = Tile::filled(c);
        assert!(tile.px.iter().all(|p| *p == c));
    }

    #[test]
    fn get_set_roundtrip() {
        let mut tile = Tile::transparent();
        let c = Rgba32F::new(1.0, 0.5, 0.0, 1.0);
        tile.set(12, 34, c);
        assert_eq!(tile.get(12, 34), c);
        assert_eq!(tile.get(0, 0), Rgba32F::TRANSPARENT);
    }

    #[test]
    #[should_panic]
    fn get_out_of_range_panics() {
        Tile::transparent().get(TILE_SIZE, 0);
    }

    #[test]
    #[should_panic]
    fn set_out_of_range_panics() {
        let mut tile = Tile::transparent();
        tile.set(0, TILE_SIZE, Rgba32F::new(1.0, 1.0, 1.0, 1.0));
    }

    #[test]
    fn tile_tracks_non_transparent_count() {
        let mut tile = Tile::transparent();
        assert_eq!(tile.non_transparent_count(), 0);

        tile.set(12, 34, Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        assert_eq!(tile.non_transparent_count(), 1);

        tile.set(12, 34, Rgba32F::new(0.0, 1.0, 0.0, 1.0));
        assert_eq!(tile.non_transparent_count(), 1);

        tile.set(12, 34, Rgba32F::TRANSPARENT);
        assert_eq!(tile.non_transparent_count(), 0);

        let mut tile2 = Tile::filled(Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        assert_eq!(tile2.non_transparent_count(), TILE_PIXELS);
        tile2.set(0, 0, Rgba32F::TRANSPARENT);
        assert_eq!(tile2.non_transparent_count(), TILE_PIXELS - 1);
    }

    #[test]
    fn filled_transparent_tile_has_zero_count() {
        let tile = Tile::filled(Rgba32F::TRANSPARENT);
        assert_eq!(tile.non_transparent_count(), 0);
    }

    #[test]
    fn from_boxed_slice_counts_non_transparent_pixels() {
        let mut pixels = vec![Rgba32F::TRANSPARENT; TILE_PIXELS];
        pixels[100] = Rgba32F::new(1.0, 0.0, 0.0, 1.0);
        pixels[200] = Rgba32F::new(0.0, 1.0, 0.0, 1.0);
        let tile = Tile::from_boxed_slice(pixels.into_boxed_slice());
        assert_eq!(tile.non_transparent_count(), 2);
    }
}
