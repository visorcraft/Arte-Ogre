// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Extract selections and copy/cut them to new layers.
//!
//! This module implements Arte Ogre's headline feature: copying or cutting a
//! selection to a new layer while retaining the exact document position of the
//! selected pixels. The extracted pixels live in a zero-offset raster layer, so
//! their document coordinates are preserved unchanged.

use crate::buffer::TiledBuffer;
use crate::coord::{local_coord, IVec2, Rect};
use crate::document::Document;
use crate::error::{OgreError, Result};
use crate::layer::{Layer, LayerId};
use crate::pixel::Rgba32F;
use crate::selection::{Selection, SelectionKind};

/// Read `source` (in layer-local space, shifted by `src_offset`) over the
/// selection and return a buffer keyed in **document** coordinates.
///
/// Each extracted pixel has its alpha multiplied by the selection coverage at
/// that document pixel. RGB channels are left untouched. Pixels whose local
/// coordinate overflows `i32`, or whose scaled alpha is exactly zero, become
/// transparent. Pixels outside the selection are left transparent and empty
/// tiles are pruned.
pub fn extract_selection(source: &TiledBuffer, src_offset: IVec2, sel: &Selection) -> TiledBuffer {
    let mut out = TiledBuffer::new();
    sel.for_each_pixel(|doc_p, cov| {
        let Some(local_p) = local_coord(doc_p, src_offset) else {
            return;
        };
        let mut px = source.get_pixel(local_p);
        let a = if px.a.is_nan() { 0.0 } else { px.a };
        px.a = a * cov;
        if px.a == 0.0 {
            px = Rgba32F::TRANSPARENT;
        }
        out.set_pixel(doc_p, px);
    });
    out.prune_empty_tiles();
    out
}

/// Return a copy of `source` with the selected pixels erased.
///
/// Full coverage removes the pixel, partial coverage reduces alpha as
/// `alpha * (1 - coverage)` — the same rule [`cut_selection_to_new_layer`]
/// applies to the source layer. This is the buffer-level building block for
/// "cut to clipboard" (clear in place) without creating a new layer.
pub fn erase_selection_from_buffer(
    source: &TiledBuffer,
    src_offset: IVec2,
    sel: &Selection,
) -> TiledBuffer {
    let mut out = source.clone();
    sel.for_each_pixel(|doc_p, cov| {
        let Some(local_p) = local_coord(doc_p, src_offset) else {
            return;
        };
        let mut px = out.get_pixel(local_p);
        let a = if px.a.is_nan() { 0.0 } else { px.a };
        px.a = a * (1.0 - cov);
        if px.a == 0.0 {
            px = Rgba32F::TRANSPARENT;
        }
        out.set_pixel(local_p, px);
    });
    out.prune_empty_tiles();
    out
}

/// Copy the selected pixels from `source` to a new raster layer inserted
/// directly above `source`.
///
/// The new layer has zero offset, so the copied pixels keep their exact
/// document coordinates. It also becomes the active layer.
///
/// # Errors
///
/// Returns [`OgreError::EmptySelection`] if the selection is empty or does not
/// intersect the canvas, [`OgreError::LayerNotFound`] if `source` does not
/// exist, and [`OgreError::NotRaster`] if `source` is a group layer.
pub fn copy_selection_to_new_layer(
    doc: &mut Document,
    source: LayerId,
    sel: &Selection,
) -> Result<LayerId> {
    let canvas_sel = Selection::rect(doc.canvas).intersect(sel);
    if canvas_sel.is_empty() {
        return Err(OgreError::EmptySelection);
    }

    let src_layer = doc.layer(source)?;
    if !src_layer.is_raster() {
        return Err(OgreError::NotRaster);
    }

    let src_offset = src_layer.offset;
    let src_name = src_layer.name.clone();
    let extracted = extract_selection(src_layer.buffer().unwrap(), src_offset, &canvas_sel);

    let mut new_layer = Layer::new_raster(format!("{} copy", src_name));
    new_layer.content = crate::layer::LayerContent::Raster {
        buffer: extracted,
        mask: None,
    };

    let id = doc.insert_layer_above(new_layer, source)?;
    doc.active = Some(id);
    Ok(id)
}

/// Cut the selected pixels from `source` to a new raster layer inserted
/// directly above `source`.
///
/// After copying the selection to a new layer, the source layer has the
/// selected region cleared: full coverage removes the pixel, partial coverage
/// reduces alpha as `alpha * (1 - coverage)`. Empty tiles are pruned.
///
/// # Errors
///
/// Returns [`OgreError::EmptySelection`] if the selection is empty or does not
/// intersect the canvas, [`OgreError::LayerNotFound`] if `source` does not
/// exist, [`OgreError::LayerLocked`] if `source` is locked, and
/// [`OgreError::NotRaster`] if `source` is a group layer.
pub fn cut_selection_to_new_layer(
    doc: &mut Document,
    source: LayerId,
    sel: &Selection,
) -> Result<LayerId> {
    let canvas_sel = Selection::rect(doc.canvas).intersect(sel);
    if canvas_sel.is_empty() {
        return Err(OgreError::EmptySelection);
    }

    {
        let src_layer = doc.layer(source)?;
        if src_layer.locked {
            return Err(OgreError::LayerLocked(source));
        }
        if !src_layer.is_raster() {
            return Err(OgreError::NotRaster);
        }
    }

    let new_id = copy_selection_to_new_layer(doc, source, &canvas_sel)?;
    erase_selection(doc, source, &canvas_sel)?;

    Ok(new_id)
}

/// Erase the selected pixels from `source` without creating a new layer.
///
/// Full coverage removes the pixel, partial coverage reduces alpha as
/// `alpha * (1 - coverage)`. Empty tiles are pruned.
pub(crate) fn erase_selection(doc: &mut Document, source: LayerId, sel: &Selection) -> Result<()> {
    let canvas_sel = Selection::rect(doc.canvas).intersect(sel);
    if canvas_sel.is_empty() {
        return Ok(());
    }

    let src_layer = doc.layer_mut(source)?;
    let src_offset = src_layer.offset;
    let buffer = src_layer.buffer_mut().unwrap();

    // Fast path: a full-coverage rectangular selection clears a rectangle in a
    // single pass. The general per-pixel path below is correct but, combined
    // with `set_pixel`'s prune, was O(pixels × tile) for large selections.
    if let SelectionKind::Rect(r) = canvas_sel.kind {
        if let (Some(x), Some(y)) = (r.x.checked_sub(src_offset.x), r.y.checked_sub(src_offset.y)) {
            buffer.clear_rect(Rect::new(x, y, r.w, r.h));
            return Ok(());
        }
    }

    canvas_sel.for_each_pixel(|doc_p, cov| {
        let Some(local_p) = local_coord(doc_p, src_offset) else {
            return;
        };
        let mut px = buffer.get_pixel(local_p);
        let a = if px.a.is_nan() { 0.0 } else { px.a };
        px.a = a * (1.0 - cov);
        if px.a == 0.0 {
            px = Rgba32F::TRANSPARENT;
        }
        buffer.set_pixel(local_p, px);
    });
    buffer.prune_empty_tiles();
    Ok(())
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::coord::Rect;
    use crate::document::Document;
    use crate::pixel::Rgba32F;
    use crate::selection::SelectionKind;

    // ------------------------------------------------------------------
    // Task 1.4.1 — Extract selection
    // ------------------------------------------------------------------

    #[test]
    fn erase_selection_from_buffer_clears_selected_pixels() {
        let mut source = TiledBuffer::new();
        source.set_pixel(IVec2::new(5, 5), Rgba32F::new(0.2, 0.4, 0.9, 1.0));
        source.set_pixel(IVec2::new(50, 50), Rgba32F::new(0.1, 0.2, 0.3, 1.0));

        let sel = Selection::rect(Rect::new(0, 0, 10, 10));
        let out = erase_selection_from_buffer(&source, IVec2::ZERO, &sel);

        // Pixel inside the selection is erased; the one outside is untouched.
        assert_eq!(out.get_pixel(IVec2::new(5, 5)), Rgba32F::TRANSPARENT);
        assert_eq!(
            out.get_pixel(IVec2::new(50, 50)),
            Rgba32F::new(0.1, 0.2, 0.3, 1.0)
        );
        // The source is not mutated.
        assert_eq!(
            source.get_pixel(IVec2::new(5, 5)),
            Rgba32F::new(0.2, 0.4, 0.9, 1.0)
        );
    }

    #[test]
    fn extract_rect_selection_copies_covered_pixels_to_doc_coords() {
        let mut source = TiledBuffer::new();
        source.set_pixel(IVec2::new(1000, 700), Rgba32F::new(0.2, 0.4, 0.9, 1.0));
        source.set_pixel(IVec2::new(995, 695), Rgba32F::new(0.1, 0.2, 0.3, 0.5));

        let sel = Selection::rect(Rect::new(990, 690, 40, 40));
        let out = extract_selection(&source, IVec2::ZERO, &sel);

        assert_eq!(
            out.get_pixel(IVec2::new(1000, 700)),
            Rgba32F::new(0.2, 0.4, 0.9, 1.0)
        );
        assert_eq!(
            out.get_pixel(IVec2::new(995, 695)),
            Rgba32F::new(0.1, 0.2, 0.3, 0.5)
        );
        assert_eq!(out.get_pixel(IVec2::new(989, 700)), Rgba32F::TRANSPARENT);
        assert_eq!(out.get_pixel(IVec2::new(1030, 700)), Rgba32F::TRANSPARENT);
    }

    #[test]
    fn extract_with_nonzero_source_offset_preserves_doc_position() {
        let mut source = TiledBuffer::new();
        // The layer is offset so its local (0,0) maps to document (50,50).
        let src_offset = IVec2::new(50, 50);
        source.set_pixel(IVec2::new(0, 0), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        let sel = Selection::rect(Rect::new(45, 45, 20, 20));
        let out = extract_selection(&source, src_offset, &sel);

        // The pixel is at document (50,50), inside the selection.
        assert_eq!(
            out.get_pixel(IVec2::new(50, 50)),
            Rgba32F::new(1.0, 0.0, 0.0, 1.0)
        );
        // Local (0,0) of the source must not appear in the output.
        assert_eq!(out.get_pixel(IVec2::new(0, 0)), Rgba32F::TRANSPARENT);
    }

    #[test]
    fn extract_partial_coverage_scales_alpha() {
        let mut source = TiledBuffer::new();
        source.set_pixel(IVec2::new(5, 5), Rgba32F::new(1.0, 1.0, 1.0, 1.0));

        let mut coverage = TiledBuffer::new();
        coverage.set_pixel(IVec2::new(5, 5), Rgba32F::new(0.25, 0.0, 0.0, 0.0));
        let sel = Selection {
            kind: crate::selection::SelectionKind::Mask {
                coverage,
                bounds: Rect::new(0, 0, 10, 10),
            },
        };

        let out = extract_selection(&source, IVec2::ZERO, &sel);
        assert_eq!(
            out.get_pixel(IVec2::new(5, 5)),
            Rgba32F::new(1.0, 1.0, 1.0, 0.25)
        );
    }

    #[test]
    fn extract_prunes_empty_tiles() {
        let source = TiledBuffer::new();
        let sel = Selection::rect(Rect::new(0, 0, 10, 10));
        let out = extract_selection(&source, IVec2::ZERO, &sel);
        assert!(out.is_empty());
    }

    // ------------------------------------------------------------------
    // Task 1.4.2 — Copy to new layer
    // ------------------------------------------------------------------

    #[test]
    fn copy_to_new_layer_retains_exact_position() {
        let mut doc = Document::new(2000, 1500);
        let src = doc.add_raster_layer("src");
        let target = IVec2::new(1000, 700);
        doc.layer_mut(src)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(target, Rgba32F::new(0.2, 0.4, 0.9, 1.0));

        let sel = Selection::rect(Rect::new(990, 690, 40, 40));
        let new = copy_selection_to_new_layer(&mut doc, src, &sel).unwrap();

        let l = doc.layer(new).unwrap();
        assert_eq!(l.offset, IVec2::ZERO);
        assert_eq!(
            l.buffer().unwrap().get_pixel(target),
            Rgba32F::new(0.2, 0.4, 0.9, 1.0)
        );
        assert_eq!(
            doc.layer(src)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(target)
                .a,
            1.0
        );
        assert_eq!(doc.active, Some(new));
    }

    #[test]
    fn copy_with_source_offset_still_exact() {
        let mut doc = Document::new(2000, 1500);
        let src = doc.add_raster_layer("src");
        let offset = IVec2::new(500, 300);
        doc.layer_mut(src).unwrap().offset = offset;
        let target = IVec2::new(1000, 700);
        let local = target - offset;
        doc.layer_mut(src)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(local, Rgba32F::new(0.2, 0.4, 0.9, 1.0));

        let sel = Selection::rect(Rect::new(990, 690, 40, 40));
        let new = copy_selection_to_new_layer(&mut doc, src, &sel).unwrap();

        let l = doc.layer(new).unwrap();
        assert_eq!(l.offset, IVec2::ZERO);
        assert_eq!(
            l.buffer().unwrap().get_pixel(target),
            Rgba32F::new(0.2, 0.4, 0.9, 1.0)
        );
    }

    // ------------------------------------------------------------------
    // Task 1.4.3 — Cut to new layer
    // ------------------------------------------------------------------

    #[test]
    fn cut_to_new_layer_clears_source_exactly() {
        let mut doc = Document::new(2000, 1500);
        let src = doc.add_raster_layer("src");
        let target = IVec2::new(1000, 700);
        doc.layer_mut(src)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(target, Rgba32F::new(0.2, 0.4, 0.9, 1.0));

        let sel = Selection::rect(Rect::new(990, 690, 40, 40));
        let new = cut_selection_to_new_layer(&mut doc, src, &sel).unwrap();

        let l = doc.layer(new).unwrap();
        assert_eq!(l.offset, IVec2::ZERO);
        assert_eq!(
            l.buffer().unwrap().get_pixel(target),
            Rgba32F::new(0.2, 0.4, 0.9, 1.0)
        );

        assert_eq!(
            doc.layer(src).unwrap().buffer().unwrap().get_pixel(target),
            Rgba32F::TRANSPARENT
        );
    }

    #[test]
    fn cut_with_partial_coverage_reduces_source_alpha() {
        let mut doc = Document::new(100, 100);
        let src = doc.add_raster_layer("src");
        doc.layer_mut(src)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        let mut coverage = TiledBuffer::new();
        coverage.set_pixel(IVec2::new(5, 5), Rgba32F::new(0.5, 0.0, 0.0, 0.0));
        let sel = Selection {
            kind: crate::selection::SelectionKind::Mask {
                coverage,
                bounds: Rect::new(0, 0, 10, 10),
            },
        };

        let new = cut_selection_to_new_layer(&mut doc, src, &sel).unwrap();
        assert_eq!(
            doc.layer(new)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(5, 5)),
            Rgba32F::new(1.0, 0.0, 0.0, 0.5)
        );
        assert_eq!(
            doc.layer(src)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(5, 5)),
            Rgba32F::new(1.0, 0.0, 0.0, 0.5)
        );
    }

    #[test]
    fn cut_with_nonzero_offset_retains_exact_position() {
        let mut doc = Document::new(2000, 1500);
        let src = doc.add_raster_layer("src");
        let offset = IVec2::new(123, -456);
        doc.layer_mut(src).unwrap().offset = offset;

        let target = IVec2::new(1000, 700);
        let local = target - offset;
        doc.layer_mut(src)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(local, Rgba32F::new(0.2, 0.4, 0.9, 1.0));

        let sel = Selection::rect(Rect::new(990, 690, 40, 40));
        let new = cut_selection_to_new_layer(&mut doc, src, &sel).unwrap();

        let l = doc.layer(new).unwrap();
        assert_eq!(l.offset, IVec2::ZERO);
        assert_eq!(
            l.buffer().unwrap().get_pixel(target),
            Rgba32F::new(0.2, 0.4, 0.9, 1.0)
        );
        assert_eq!(
            doc.layer(src).unwrap().buffer().unwrap().get_pixel(local),
            Rgba32F::TRANSPARENT
        );
    }

    #[test]
    fn cut_with_negative_offset_retains_exact_position() {
        let mut doc = Document::new(2000, 1500);
        let src = doc.add_raster_layer("src");
        let offset = IVec2::new(-200, -100);
        doc.layer_mut(src).unwrap().offset = offset;

        // Target is inside the canvas; the negative offset makes the local
        // coordinate positive.
        let target = IVec2::new(50, 50);
        let local = target - offset;
        doc.layer_mut(src)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(local, Rgba32F::new(0.2, 0.4, 0.9, 1.0));

        let sel = Selection::rect(Rect::new(target.x - 5, target.y - 5, 15, 15));
        let new = cut_selection_to_new_layer(&mut doc, src, &sel).unwrap();

        let l = doc.layer(new).unwrap();
        assert_eq!(l.offset, IVec2::ZERO);
        assert_eq!(
            l.buffer().unwrap().get_pixel(target),
            Rgba32F::new(0.2, 0.4, 0.9, 1.0)
        );
        assert_eq!(
            doc.layer(src).unwrap().buffer().unwrap().get_pixel(local),
            Rgba32F::TRANSPARENT
        );
    }

    #[test]
    fn cut_preserves_rgb_outside_unit_range() {
        let mut doc = Document::new(100, 100);
        let src = doc.add_raster_layer("src");
        doc.layer_mut(src)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(1.5, -0.5, 2.0, 1.0));

        let mut coverage = TiledBuffer::new();
        coverage.set_pixel(IVec2::new(5, 5), Rgba32F::new(0.5, 0.0, 0.0, 0.0));
        let sel = Selection {
            kind: SelectionKind::Mask {
                coverage,
                bounds: Rect::new(0, 0, 10, 10),
            },
        };
        let new = cut_selection_to_new_layer(&mut doc, src, &sel).unwrap();

        let src_px = doc
            .layer(src)
            .unwrap()
            .buffer()
            .unwrap()
            .get_pixel(IVec2::new(5, 5));
        assert_eq!(src_px.r, 1.5);
        assert_eq!(src_px.g, -0.5);
        assert_eq!(src_px.b, 2.0);
        assert_eq!(src_px.a, 0.5);

        let new_px = doc
            .layer(new)
            .unwrap()
            .buffer()
            .unwrap()
            .get_pixel(IVec2::new(5, 5));
        assert_eq!(new_px.r, 1.5);
        assert_eq!(new_px.g, -0.5);
        assert_eq!(new_px.b, 2.0);
        assert_eq!(new_px.a, 0.5);
    }

    #[test]
    fn cut_near_zero_alpha_retains_pixel() {
        let mut doc = Document::new(100, 100);
        let src = doc.add_raster_layer("src");
        doc.layer_mut(src)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(1.5, -0.5, 2.0, 1e-13_f32));

        let mut coverage = TiledBuffer::new();
        coverage.set_pixel(IVec2::new(5, 5), Rgba32F::new(0.5, 0.0, 0.0, 0.0));
        let sel = Selection {
            kind: SelectionKind::Mask {
                coverage,
                bounds: Rect::new(0, 0, 10, 10),
            },
        };

        cut_selection_to_new_layer(&mut doc, src, &sel).unwrap();

        let src_px = doc
            .layer(src)
            .unwrap()
            .buffer()
            .unwrap()
            .get_pixel(IVec2::new(5, 5));
        assert_eq!(src_px.r, 1.5);
        assert_eq!(src_px.g, -0.5);
        assert_eq!(src_px.b, 2.0);
        assert_eq!(src_px.a, 5e-14_f32);
    }

    #[test]
    fn cut_nan_alpha_treated_as_zero() {
        let mut doc = Document::new(100, 100);
        let src = doc.add_raster_layer("src");
        doc.layer_mut(src)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(1.0, 0.5, 0.25, f32::NAN));

        let sel = Selection::rect(Rect::new(0, 0, 10, 10));
        let new = cut_selection_to_new_layer(&mut doc, src, &sel).unwrap();

        assert_eq!(
            doc.layer(src)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(5, 5)),
            Rgba32F::TRANSPARENT
        );
        assert_eq!(
            doc.layer(new)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(5, 5)),
            Rgba32F::TRANSPARENT
        );
    }

    #[test]
    fn extract_does_not_panic_on_overflowing_local_coords() {
        let source = TiledBuffer::new();
        let src_offset = IVec2::new(i32::MIN, i32::MIN);
        let sel = Selection::rect(Rect::new(i32::MAX - 5, i32::MAX - 5, 10, 10));
        let out = extract_selection(&source, src_offset, &sel);
        assert!(out.is_empty());
    }

    #[test]
    fn extract_zero_alpha_becomes_transparent() {
        let mut source = TiledBuffer::new();
        source.set_pixel(IVec2::new(5, 5), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        let mut coverage = TiledBuffer::new();
        coverage.set_pixel(IVec2::new(5, 5), Rgba32F::new(0.0, 0.0, 0.0, 0.0));
        let sel = Selection {
            kind: SelectionKind::Mask {
                coverage,
                bounds: Rect::new(0, 0, 10, 10),
            },
        };

        let out = extract_selection(&source, IVec2::ZERO, &sel);
        assert_eq!(out.get_pixel(IVec2::new(5, 5)), Rgba32F::TRANSPARENT);
    }

    #[test]
    fn extract_preserves_rgb_outside_unit_range() {
        let mut source = TiledBuffer::new();
        source.set_pixel(IVec2::new(5, 5), Rgba32F::new(1.5, -0.5, 2.0, 0.5));

        let sel = Selection::rect(Rect::new(0, 0, 10, 10));
        let out = extract_selection(&source, IVec2::ZERO, &sel);

        let px = out.get_pixel(IVec2::new(5, 5));
        assert_eq!(px.r, 1.5);
        assert_eq!(px.g, -0.5);
        assert_eq!(px.b, 2.0);
        assert_eq!(px.a, 0.5);
    }

    #[test]
    fn extract_near_zero_alpha_retains_pixel() {
        let mut source = TiledBuffer::new();
        source.set_pixel(IVec2::new(5, 5), Rgba32F::new(1.5, -0.5, 2.0, 1e-13_f32));

        let mut coverage = TiledBuffer::new();
        coverage.set_pixel(IVec2::new(5, 5), Rgba32F::new(0.5, 0.0, 0.0, 0.0));
        let sel = Selection {
            kind: SelectionKind::Mask {
                coverage,
                bounds: Rect::new(0, 0, 10, 10),
            },
        };

        let out = extract_selection(&source, IVec2::ZERO, &sel);
        let px = out.get_pixel(IVec2::new(5, 5));
        assert_eq!(px.r, 1.5);
        assert_eq!(px.g, -0.5);
        assert_eq!(px.b, 2.0);
        assert_eq!(px.a, 5e-14_f32);
    }

    #[test]
    fn extract_nan_alpha_treated_as_zero() {
        let mut source = TiledBuffer::new();
        source.set_pixel(IVec2::new(5, 5), Rgba32F::new(1.0, 0.5, 0.25, f32::NAN));

        let sel = Selection::rect(Rect::new(0, 0, 10, 10));
        let out = extract_selection(&source, IVec2::ZERO, &sel);
        assert_eq!(out.get_pixel(IVec2::new(5, 5)), Rgba32F::TRANSPARENT);
    }

    proptest! {
        #[test]
        fn extract_place_exactness_proptest(
            offset in (-1000i32..=1000, -1000i32..=1000),
            center in (-500i32..=500, -500i32..=500),
        ) {
            let src_offset = IVec2::new(offset.0, offset.1);
            let target = IVec2::new(center.0, center.1);
            let local = target - src_offset;

            let mut source = TiledBuffer::new();
            source.set_pixel(local, Rgba32F::new(0.2, 0.4, 0.9, 1.0));

            let sel = Selection::rect(Rect::new(target.x - 3, target.y - 3, 7, 7));
            let extracted = extract_selection(&source, src_offset, &sel);

            let mut placed = TiledBuffer::new();
            if let Some(bounds) = extracted.bounds() {
                placed.copy_region(&extracted, bounds, IVec2::ZERO);
            }

            prop_assert_eq!(placed.get_pixel(target), Rgba32F::new(0.2, 0.4, 0.9, 1.0));
        }
    }

    // ------------------------------------------------------------------
    // Task 1.4.4 — Edge cases
    // ------------------------------------------------------------------

    #[test]
    fn copy_empty_selection_errors() {
        let mut doc = Document::new(100, 100);
        let src = doc.add_raster_layer("src");
        assert!(matches!(
            copy_selection_to_new_layer(&mut doc, src, &Selection::none()),
            Err(OgreError::EmptySelection)
        ));
    }

    #[test]
    fn cut_empty_selection_errors() {
        let mut doc = Document::new(100, 100);
        let src = doc.add_raster_layer("src");
        assert!(matches!(
            cut_selection_to_new_layer(&mut doc, src, &Selection::none()),
            Err(OgreError::EmptySelection)
        ));
    }

    #[test]
    fn copy_selection_fully_outside_canvas_errors_empty_selection() {
        let mut doc = Document::new(100, 100);
        let src = doc.add_raster_layer("src");
        doc.layer_mut(src)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        // Selection is entirely outside the 100x100 canvas and the source content.
        let sel = Selection::rect(Rect::new(200, 200, 10, 10));
        assert!(matches!(
            copy_selection_to_new_layer(&mut doc, src, &sel),
            Err(OgreError::EmptySelection)
        ));
    }

    #[test]
    fn cut_selection_fully_outside_canvas_errors_empty_selection() {
        let mut doc = Document::new(100, 100);
        let src = doc.add_raster_layer("src");
        doc.layer_mut(src)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        let sel = Selection::rect(Rect::new(200, 200, 10, 10));
        assert!(matches!(
            cut_selection_to_new_layer(&mut doc, src, &sel),
            Err(OgreError::EmptySelection)
        ));
    }

    #[test]
    fn copy_from_group_errors() {
        let mut doc = Document::new(100, 100);
        let raster = doc.add_raster_layer("raster");
        let group = Layer::new_group("group");
        let group_id = doc.insert_layer_above(group, raster).unwrap();

        let sel = Selection::rect(Rect::new(0, 0, 10, 10));
        assert!(matches!(
            copy_selection_to_new_layer(&mut doc, group_id, &sel),
            Err(OgreError::NotRaster)
        ));
    }

    #[test]
    fn cut_locked_layer_errors() {
        let mut doc = Document::new(100, 100);
        let src = doc.add_raster_layer("src");
        doc.layer_mut(src).unwrap().locked = true;

        let sel = Selection::rect(Rect::new(0, 0, 10, 10));
        assert!(matches!(
            cut_selection_to_new_layer(&mut doc, src, &sel),
            Err(OgreError::LayerLocked(id)) if id == src
        ));
    }

    #[test]
    fn cut_from_group_errors_before_locked_check_for_groups() {
        let mut doc = Document::new(100, 100);
        let raster = doc.add_raster_layer("raster");
        let group = Layer::new_group("group");
        let group_id = doc.insert_layer_above(group, raster).unwrap();

        let sel = Selection::rect(Rect::new(0, 0, 10, 10));
        assert!(matches!(
            cut_selection_to_new_layer(&mut doc, group_id, &sel),
            Err(OgreError::NotRaster)
        ));
    }

    #[test]
    fn cut_locked_group_returns_layer_locked() {
        let mut doc = Document::new(100, 100);
        let raster = doc.add_raster_layer("raster");
        let mut group = Layer::new_group("group");
        group.locked = true;
        let group_id = doc.insert_layer_above(group, raster).unwrap();

        let sel = Selection::rect(Rect::new(0, 0, 10, 10));
        assert!(matches!(
            cut_selection_to_new_layer(&mut doc, group_id, &sel),
            Err(OgreError::LayerLocked(id)) if id == group_id
        ));
    }

    #[test]
    fn selection_partially_off_canvas_copies_only_on_canvas_pixels() {
        let mut doc = Document::new(100, 100);
        let src = doc.add_raster_layer("src");
        // Place a pixel at the bottom-right edge of the canvas.
        doc.layer_mut(src)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(99, 99), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        // 10x10 selection straddles the canvas edge; only (99,99) is inside both.
        let sel = Selection::rect(Rect::new(95, 95, 10, 10));
        let new = copy_selection_to_new_layer(&mut doc, src, &sel).unwrap();

        assert_eq!(
            doc.layer(new)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(99, 99)),
            Rgba32F::new(1.0, 0.0, 0.0, 1.0)
        );
        // Off-canvas document coordinate must not materialise in the buffer.
        assert_eq!(
            doc.layer(new)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(105, 105)),
            Rgba32F::TRANSPARENT
        );
    }

    #[test]
    fn cut_selection_partially_off_canvas_clears_only_on_canvas_pixels() {
        let mut doc = Document::new(100, 100);
        let src = doc.add_raster_layer("src");
        // Place a pixel at the bottom-right edge of the canvas.
        doc.layer_mut(src)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(99, 99), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        // 10x10 selection straddles the canvas edge; only (99,99) is inside both.
        let sel = Selection::rect(Rect::new(95, 95, 10, 10));
        let new = cut_selection_to_new_layer(&mut doc, src, &sel).unwrap();

        assert_eq!(
            doc.layer(new)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(99, 99)),
            Rgba32F::new(1.0, 0.0, 0.0, 1.0)
        );
        assert_eq!(
            doc.layer(src)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(99, 99)),
            Rgba32F::TRANSPARENT
        );
        // Off-canvas document coordinate must not be written back to the source.
        assert_eq!(
            doc.layer(src)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(105, 105)),
            Rgba32F::TRANSPARENT
        );
    }

    #[test]
    fn cut_does_not_panic_on_overflowing_local_coords() {
        let mut doc = Document::new(100, 100);
        let src = doc.add_raster_layer("src");
        doc.layer_mut(src).unwrap().offset = IVec2::new(i32::MIN, i32::MIN);

        // Selection is inside the canvas, but every local coordinate overflows.
        let sel = Selection::rect(Rect::new(0, 0, 10, 10));
        let result = cut_selection_to_new_layer(&mut doc, src, &sel);
        assert!(result.is_ok());
    }
}
