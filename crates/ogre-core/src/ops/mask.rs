// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Layer-mask construction helpers.
//!
//! A layer mask is a [`TiledBuffer`] storing
//! per-pixel coverage in its red channel (see [`LayerContent::Raster`]).
//! Coverage `1.0` reveals the underlying pixel; `0.0` hides it. The mask lives
//! in the same layer-local space as the layer buffer, so unwritten mask pixels
//! (which read back as coverage `0.0`) hide the corresponding layer pixel.
//!
//! [`LayerContent::Raster`]: crate::layer::LayerContent::Raster

use crate::buffer::TiledBuffer;
use crate::coord::{tile_rect, IVec2, TileCoord};
use crate::pixel::clamp01_nan0 as sanitize_coverage;
use crate::pixel::Rgba32F;
use crate::selection::Selection;
use crate::tile::{Tile, TILE_SIZE};

/// Mask coverage that reveals the underlying pixel fully.
const COVERAGE_FULL: Rgba32F = Rgba32F {
    r: 1.0,
    g: 0.0,
    b: 0.0,
    a: 1.0,
};

/// How a newly added layer mask should be initialized.
///
/// Every variant produces a **sparse** mask: only tiles that actually carry
/// information are materialised, so a mask over a 4K canvas costs nothing if the
/// layer or selection is small.
#[derive(Clone, Debug, PartialEq)]
pub enum MaskInit {
    /// Coverage `1.0` over the layer's existing content tiles, unwritten
    /// elsewhere. Equivalent to "no mask yet" but establishes an editable mask
    /// the user can paint black onto. Sparse: only materialises tiles the
    /// source buffer already occupies.
    RevealAll,
    /// Coverage `0.0` everywhere (an empty buffer). Fully hides the layer until
    /// the user paints white onto the mask.
    HideAll,
    /// Coverage `1.0` inside the selection (document space), unwritten
    /// elsewhere. The canonical "mask to selection" operation.
    RevealSelection(Selection),
}

impl MaskInit {
    /// Build the mask buffer implied by this init kind.
    ///
    /// `source` is the layer's raster buffer (used to bound a `RevealAll` mask)
    /// and `layer_offset` converts document-space selections to the layer-local
    /// space the mask is stored in (`local = doc - offset`).
    pub fn build(&self, source: &TiledBuffer, layer_offset: IVec2) -> TiledBuffer {
        match self {
            MaskInit::HideAll => TiledBuffer::new(),
            MaskInit::RevealAll => {
                let mut mask = TiledBuffer::new();
                for (coord, _) in source.occupied_tiles() {
                    *mask.tile_mut(coord) = Tile::filled(COVERAGE_FULL);
                }
                mask
            }
            MaskInit::RevealSelection(selection) => {
                let mut mask = TiledBuffer::new();
                let Some(bbox) = selection.bounds() else {
                    return mask;
                };
                let (x0, y0) = (bbox.x, bbox.y);
                let x1 = i32::try_from(bbox.right()).unwrap_or(i32::MAX);
                let y1 = i32::try_from(bbox.bottom()).unwrap_or(i32::MAX);
                for y in y0..y1 {
                    for x in x0..x1 {
                        let doc_p = IVec2::new(x, y);
                        if selection.coverage_at(doc_p) > 0.0 {
                            let local = doc_p - layer_offset;
                            mask.set_pixel(local, COVERAGE_FULL);
                        }
                    }
                }
                mask
            }
        }
    }
}

/// Bake a mask into a layer buffer: multiply each pixel's alpha by the mask
/// coverage at that layer-local position.
///
/// Tiles that end up fully transparent are pruned, preserving the sparse
/// representation. This is the destructive "apply mask" step.
pub fn apply_mask_to_buffer(buffer: &mut TiledBuffer, mask: &TiledBuffer) {
    let coords: Vec<TileCoord> = buffer
        .occupied_tiles()
        .into_iter()
        .map(|(c, _)| c)
        .collect();
    for c in coords {
        let Some(t_rect) = tile_rect(c) else {
            continue;
        };
        let tile = buffer.tile_mut(c);
        for ly in 0..TILE_SIZE {
            for lx in 0..TILE_SIZE {
                let local = IVec2::new(t_rect.x + lx as i32, t_rect.y + ly as i32);
                let cov = sanitize_coverage(mask.get_pixel(local).r);
                if cov >= 1.0 {
                    continue;
                }
                let mut px = tile.get(lx, ly);
                if px.a == 0.0 {
                    continue;
                }
                px.a *= cov;
                tile.set(lx, ly, px);
            }
        }
    }
    buffer.prune_empty_tiles();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coord::Rect;

    #[test]
    fn hide_all_is_empty() {
        let src = TiledBuffer::new();
        let mask = MaskInit::HideAll.build(&src, IVec2::ZERO);
        assert!(mask.is_empty(), "HideAll mask must be sparse/empty");
    }

    #[test]
    fn reveal_all_mirrors_source_tiles() {
        let mut src = TiledBuffer::new();
        src.set_pixel(IVec2::new(5, 7), Rgba32F::new(0.5, 0.5, 0.5, 1.0));
        let mask = MaskInit::RevealAll.build(&src, IVec2::ZERO);
        // Same single occupied tile as the source.
        assert_eq!(mask.occupied_tiles().len(), src.occupied_tiles().len());
        // Coverage is full where the source has content.
        assert_eq!(mask.get_pixel(IVec2::new(5, 7)), COVERAGE_FULL);
        // And unwritten (coverage 0) elsewhere.
        assert_eq!(mask.get_pixel(IVec2::new(500, 500)).r, 0.0);
    }

    #[test]
    fn reveal_selection_is_sparse_and_offset_correct() {
        // Selection in document space covering (10..14, 20..24); layer offset (2, 4)
        // → mask must be white at local (8..12, 16..20).
        let sel = Selection::rect(Rect::new(10, 20, 4, 4));
        let src = TiledBuffer::new();
        let mask = MaskInit::RevealSelection(sel).build(&src, IVec2::new(2, 4));
        assert_eq!(mask.get_pixel(IVec2::new(8, 16)), COVERAGE_FULL);
        assert_eq!(mask.get_pixel(IVec2::new(11, 19)), COVERAGE_FULL);
        // Outside the selection → unwritten (coverage 0).
        assert_eq!(mask.get_pixel(IVec2::new(12, 16)).r, 0.0);
    }

    #[test]
    fn reveal_selection_with_empty_selection_is_empty() {
        let sel = Selection::none();
        let src = TiledBuffer::new();
        let mask = MaskInit::RevealSelection(sel).build(&src, IVec2::ZERO);
        assert!(mask.is_empty());
    }
}
