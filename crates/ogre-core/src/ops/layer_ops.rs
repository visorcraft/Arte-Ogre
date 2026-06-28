// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Layer manipulation operations: duplicate, delete, mutators, merge, and move.
//!
//! These functions are thin wrappers over the [`Document`] tree API. They are
//! intentionally low-level; undoable versions live in
//! [`crate::history`] as [`Command`](crate::history::Command) implementations.

use crate::buffer::TiledBuffer;
use crate::coord::IVec2;
use crate::document::Document;
use crate::error::{OgreError, Result};
use crate::layer::{BlendMode, Layer, LayerId};

use crate::pixel::clamp01_nan0 as sanitize_coverage;
use crate::pixel::clamp01_nan0 as sanitize_opacity;

/// Duplicate a raster layer, inserting the copy directly above the original.
///
/// The duplicate shares every occupied tile through [`Arc`](std::sync::Arc)
/// clones, so the operation is O(tiles) pointer copies and no pixel data is
/// duplicated.
///
/// # Errors
///
/// Returns [`OgreError::LayerNotFound`] if `id` does not exist and
/// [`OgreError::NotRaster`] if `id` refers to a group layer.
pub fn duplicate_layer(doc: &mut Document, id: LayerId) -> Result<LayerId> {
    let layer = doc.layer(id)?;
    if !layer.is_raster() {
        return Err(OgreError::NotRaster);
    }
    let mut dup = layer.clone();
    dup.name = format!("{} copy", dup.name);
    doc.insert_layer_above(dup, id)
}

/// Delete a layer (and its descendants, if it is a group) from the document.
///
/// # Errors
///
/// Returns [`OgreError::LayerNotFound`] if `id` does not exist.
pub fn delete_layer(doc: &mut Document, id: LayerId) -> Result<()> {
    doc.remove_layer(id)?;
    Ok(())
}

/// Set whether a layer is visible in the canvas.
///
/// # Errors
///
/// Returns [`OgreError::LayerNotFound`] if `id` does not exist.
pub fn set_layer_visible(doc: &mut Document, id: LayerId, visible: bool) -> Result<()> {
    doc.layer_mut(id)?.visible = visible;
    Ok(())
}

/// Set a layer's opacity, clamped to `[0.0, 1.0]`.
///
/// # Errors
///
/// Returns [`OgreError::LayerNotFound`] if `id` does not exist.
pub fn set_layer_opacity(doc: &mut Document, id: LayerId, opacity: f32) -> Result<()> {
    doc.layer_mut(id)?.opacity = sanitize_opacity(opacity);
    Ok(())
}

/// Set a layer's blend mode.
///
/// # Errors
///
/// Returns [`OgreError::LayerNotFound`] if `id` does not exist.
pub fn set_layer_blend(doc: &mut Document, id: LayerId, blend: BlendMode) -> Result<()> {
    doc.layer_mut(id)?.blend = blend;
    Ok(())
}

/// Set whether a layer is locked against edits.
///
/// # Errors
///
/// Returns [`OgreError::LayerNotFound`] if `id` does not exist.
pub fn set_layer_locked(doc: &mut Document, id: LayerId, locked: bool) -> Result<()> {
    doc.layer_mut(id)?.locked = locked;
    Ok(())
}

/// Rename a layer.
///
/// # Errors
///
/// Returns [`OgreError::LayerNotFound`] if `id` does not exist.
pub fn rename_layer(doc: &mut Document, id: LayerId, name: impl Into<String>) -> Result<()> {
    doc.layer_mut(id)?.name = name.into();
    Ok(())
}

/// Move a layer to a new index within its current sibling list.
///
/// The index is clamped to the valid range for that list.
///
/// # Errors
///
/// Returns [`OgreError::LayerNotFound`] if `id` does not exist.
pub fn reorder_layer(doc: &mut Document, id: LayerId, new_index: usize) -> Result<()> {
    doc.reorder(id, new_index)
}

/// Move a layer into a group at the given child index.
///
/// The index is clamped to the valid range. Moving a layer into itself or into
/// one of its descendants is rejected.
///
/// # Errors
///
/// Returns [`OgreError::LayerNotFound`] if `id` or `group` does not exist, or
/// an [`OgreError::InvalidOperation`] if the move would create a cycle or the
/// target is not a group.
pub fn move_layer_into_group(
    doc: &mut Document,
    id: LayerId,
    group: LayerId,
    index: usize,
) -> Result<()> {
    doc.move_into_group(id, group, index)
}

/// Move a layer by adding `delta` to its offset.
///
/// This is the primitive behind the canvas move tool: the composited result
/// shifts by exactly `delta`.
///
/// # Errors
///
/// Returns [`OgreError::LayerNotFound`] if `id` does not exist,
/// [`OgreError::LayerLocked`] if the layer is locked, or
/// [`OgreError::InvalidOperation`] if the resulting offset would overflow `i32`.
pub fn move_layer_by(doc: &mut Document, id: LayerId, delta: IVec2) -> Result<()> {
    let layer = doc.layer_mut(id)?;
    if layer.locked {
        return Err(OgreError::LayerLocked(id));
    }
    let new_offset = match (
        layer.offset.x.checked_add(delta.x),
        layer.offset.y.checked_add(delta.y),
    ) {
        (Some(x), Some(y)) => IVec2::new(x, y),
        _ => return Err(OgreError::InvalidOperation("layer offset out of range")),
    };
    layer.offset = new_offset;
    Ok(())
}

/// Maximum number of document-space tiles `merge_down` will process before
/// rejecting the operation as too large.
const MAX_MERGE_TILES: usize = 4096;

/// Rectangle of a tile in layer-local pixel coordinates.
///
/// Returns `None` if the tile rect does not fully fit inside `i32`.
fn local_tile_rect(c: crate::coord::TileCoord) -> Option<crate::coord::Rect> {
    let size = crate::tile::TILE_SIZE as i64;
    let x = (c.x as i64).checked_mul(size)?;
    let y = (c.y as i64).checked_mul(size)?;
    let right = x.checked_add(size)?;
    let bottom = y.checked_add(size)?;
    if x < i32::MIN as i64
        || right > i32::MAX as i64
        || y < i32::MIN as i64
        || bottom > i32::MAX as i64
    {
        return None;
    }
    Some(crate::coord::Rect::new(
        x as i32,
        y as i32,
        crate::tile::TILE_SIZE as u32,
        crate::tile::TILE_SIZE as u32,
    ))
}

/// Collect the document-space tile coordinates covered by a layer's occupied
/// tiles, shifted by the layer offset.
///
/// Returns an error if any occupied tile or the shifted coordinate would
/// overflow `i32`.
fn doc_space_occupied_tiles(
    buffer: &TiledBuffer,
    offset: IVec2,
) -> Result<ahash::AHashSet<crate::coord::TileCoord>> {
    let mut tiles = ahash::AHashSet::new();
    for (local, _) in buffer.occupied_tiles() {
        let rect = local_tile_rect(local)
            .ok_or(OgreError::InvalidOperation("layer offset out of range"))?;
        let x = rect
            .x
            .checked_add(offset.x)
            .ok_or(OgreError::InvalidOperation("layer offset out of range"))?;
        let y = rect
            .y
            .checked_add(offset.y)
            .ok_or(OgreError::InvalidOperation("layer offset out of range"))?;
        let doc_rect = crate::coord::Rect::new(x, y, rect.w, rect.h);
        for t in doc_rect.tiles_covered() {
            tiles.insert(t);
        }
    }
    Ok(tiles)
}

/// Merge `upper` down onto the layer directly below it.
///
/// The two layers are composited over the union of their occupied document-space
/// tiles. The lower layer is replaced by a single zero-offset raster layer
/// containing the merged result, and `upper` is removed.
///
/// The lower layer must use [`BlendMode::Normal`]; merging is not defined for
/// other lower-layer blend modes because the result would have to represent a
/// non-separable blend as a single raster layer.
///
/// # Errors
///
/// Returns [`OgreError::LayerNotFound`] if `upper` does not exist,
/// [`OgreError::NotRaster`] if either layer is a group,
/// [`OgreError::LayerLocked`] if either layer is locked,
/// or [`OgreError::InvalidOperation`] if there is no layer below `upper`, the
/// lower layer's blend mode is not `Normal`, the merged region would overflow
/// `i32`, or it exceeds the internal tile budget.
/// Composite two raster layers into a single document-space (zero-offset)
/// [`TiledBuffer`], applying visibility, opacity, masks, and the upper layer's
/// blend mode exactly as [`merge_down`] does. Both layers must be visible-eligible
/// raster layers in `Normal` (lower) blend; this is validated here.
///
/// Extracted so an undoable command can reuse the composite without the
/// hard-remove that [`merge_down`] performs.
pub(crate) fn merge_raster_layers(lower: &Layer, upper: &Layer) -> Result<TiledBuffer> {
    if !lower.is_raster() || !upper.is_raster() {
        return Err(OgreError::NotRaster);
    }
    if lower.locked {
        return Err(OgreError::LayerLocked(lower.id));
    }
    if upper.locked {
        return Err(OgreError::LayerLocked(upper.id));
    }
    if lower.blend != BlendMode::Normal {
        return Err(OgreError::InvalidOperation(
            "lower layer must be Normal blend for merge",
        ));
    }

    let lower_mask = match &lower.content {
        crate::layer::LayerContent::Raster { mask, .. } => mask.clone(),
        _ => None,
    };
    let upper_mask = match &upper.content {
        crate::layer::LayerContent::Raster { mask, .. } => mask.clone(),
        _ => None,
    };
    let (
        lower_offset,
        upper_offset,
        lower_visible,
        upper_visible,
        lower_opacity,
        upper_opacity,
        upper_blend,
        lower_buffer,
        upper_buffer,
    ) = (
        lower.offset,
        upper.offset,
        lower.visible,
        upper.visible,
        sanitize_opacity(lower.opacity),
        sanitize_opacity(upper.opacity),
        upper.blend,
        lower.buffer().unwrap().clone(),
        upper.buffer().unwrap().clone(),
    );

    let mut merged_tiles = doc_space_occupied_tiles(&lower_buffer, lower_offset)?;
    let upper_tiles = doc_space_occupied_tiles(&upper_buffer, upper_offset)?;
    merged_tiles.extend(upper_tiles);

    if merged_tiles.is_empty() {
        return Ok(TiledBuffer::new());
    }
    if merged_tiles.len() > MAX_MERGE_TILES {
        return Err(OgreError::InvalidOperation(
            "merged region exceeds tile budget",
        ));
    }

    let mut merged_buffer = TiledBuffer::new();
    for t in merged_tiles {
        let t_rect =
            local_tile_rect(t).ok_or(OgreError::InvalidOperation("layer offset out of range"))?;
        for y in t_rect.y as i64..t_rect.bottom() {
            for x in t_rect.x as i64..t_rect.right() {
                let doc_p = IVec2::new(x as i32, y as i32);
                let lower_p = match (
                    doc_p.x.checked_sub(lower_offset.x),
                    doc_p.y.checked_sub(lower_offset.y),
                ) {
                    (Some(lx), Some(ly)) => IVec2::new(lx, ly),
                    _ => continue,
                };
                let upper_p = match (
                    doc_p.x.checked_sub(upper_offset.x),
                    doc_p.y.checked_sub(upper_offset.y),
                ) {
                    (Some(lx), Some(ly)) => IVec2::new(lx, ly),
                    _ => continue,
                };

                let mut lower_px = if lower_visible && lower_opacity > 0.0 {
                    lower_buffer.get_pixel(lower_p)
                } else {
                    crate::pixel::Rgba32F::TRANSPARENT
                };
                if let Some(mask) = &lower_mask {
                    let cov = mask.get_pixel(lower_p).r;
                    lower_px.a *= sanitize_coverage(cov);
                }
                lower_px.a *= lower_opacity;

                let mut upper_px = if upper_visible && upper_opacity > 0.0 {
                    upper_buffer.get_pixel(upper_p)
                } else {
                    crate::pixel::Rgba32F::TRANSPARENT
                };
                if let Some(mask) = &upper_mask {
                    let cov = mask.get_pixel(upper_p).r;
                    upper_px.a *= sanitize_coverage(cov);
                }

                let out =
                    crate::compositor::blend_pixel(upper_blend, lower_px, upper_px, upper_opacity);
                if out.a > 0.0 {
                    merged_buffer.set_pixel(doc_p, out);
                }
            }
        }
    }
    Ok(merged_buffer)
}

/// Merge the raster layer `upper` down into the raster layer below it,
/// replacing the lower with the composited result and removing the upper.
///
/// This is the raw (non-undoable) op; the UI routes through
/// [`MergeDownCmd`](crate::MergeDownCmd)
/// when undo is required. Returns the id of the merged (lower) layer. Errors
/// if `upper` is not raster/locked, there is no layer below, the lower layer
/// is not raster/locked or not `Normal` blend, or the merged region exceeds
/// the internal tile budget.
pub fn merge_down(doc: &mut Document, upper: LayerId) -> Result<LayerId> {
    {
        let upper_layer = doc.layer(upper)?;
        if !upper_layer.is_raster() {
            return Err(OgreError::NotRaster);
        }
        if upper_layer.locked {
            return Err(OgreError::LayerLocked(upper));
        }
    }

    let (_parent, upper_index) = doc
        .sibling_index(upper)
        .ok_or(OgreError::LayerNotFound(upper))?;
    if upper_index == 0 {
        return Err(OgreError::InvalidOperation("no layer below the target"));
    }

    // Identify the lower layer via the sibling list so the merge is visually
    // correct even inside groups.
    let lower = doc
        .siblings(upper)
        .and_then(|list| list.get(upper_index - 1).copied())
        .ok_or(OgreError::InvalidOperation("no layer below the target"))?;

    let merged_buffer = {
        let lower_layer = doc.layer(lower)?;
        let upper_layer = doc.layer(upper)?;
        merge_raster_layers(lower_layer, upper_layer)?
    };

    let lower_name = doc.layer(lower).unwrap().name.clone();
    let mut merged = Layer::new_raster(lower_name);
    merged.id = lower;
    merged.offset = IVec2::ZERO;
    merged.content = crate::layer::LayerContent::Raster {
        buffer: merged_buffer,
        mask: None,
    };
    *doc.layer_mut(lower).unwrap() = merged;

    doc.remove_layer(upper)?;
    if doc.active == Some(upper) {
        doc.active = Some(lower);
    }

    Ok(lower)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::compositor::{blend_pixel, composite_document};
    use crate::coord::Rect;
    use crate::document::Document;
    use crate::pixel::Rgba32F;

    // ------------------------------------------------------------------
    // Task 1.6.1 — Cheap duplicate
    // ------------------------------------------------------------------

    #[test]
    fn duplicate_layer_shares_all_tiles_by_arc() {
        let mut doc = Document::new(200, 200);
        let src = doc.add_raster_layer("src");
        doc.layer_mut(src)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(10, 10), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        doc.layer_mut(src)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(300, 10), Rgba32F::new(0.0, 1.0, 0.0, 1.0));

        let dup = duplicate_layer(&mut doc, src).unwrap();

        let src_buffer = doc.layer(src).unwrap().buffer().unwrap();
        let dup_buffer = doc.layer(dup).unwrap().buffer().unwrap();

        for (coord, tile) in src_buffer.occupied_tiles() {
            assert!(
                Arc::ptr_eq(&tile, &dup_buffer.get_tile(coord).unwrap()),
                "tile {:?} was deep-copied",
                coord
            );
        }

        assert_eq!(doc.layer(dup).unwrap().name, "src copy");
        assert_eq!(doc.order, vec![src, dup]);
    }

    #[test]
    fn duplicate_group_errors() {
        let mut doc = Document::new(100, 100);
        let raster = doc.add_raster_layer("raster");
        let group = Layer::new_group("group");
        let group_id = doc.insert_layer_above(group, raster).unwrap();

        assert!(matches!(
            duplicate_layer(&mut doc, group_id),
            Err(OgreError::NotRaster)
        ));
    }

    // ------------------------------------------------------------------
    // Task 1.6.2 — Mutators
    // ------------------------------------------------------------------

    #[test]
    fn delete_layer_removes_from_order_and_clears_active() {
        let mut doc = Document::new(100, 100);
        let a = doc.add_raster_layer("A");
        let b = doc.add_raster_layer("B");
        assert_eq!(doc.active, Some(b));

        delete_layer(&mut doc, b).unwrap();
        assert_eq!(doc.order, vec![a]);
        assert_eq!(doc.active, None);
    }

    #[test]
    fn delete_layer_inside_group_updates_parent() {
        let mut doc = Document::new(100, 100);
        let bg = doc.add_raster_layer("bg");
        let group = Layer::new_group("group");
        let group_id = doc.insert_layer_above(group, bg).unwrap();
        let child = doc.add_raster_layer("child");
        doc.move_into_group(child, group_id, 0).unwrap();

        delete_layer(&mut doc, child).unwrap();
        let children = match &doc.layer(group_id).unwrap().content {
            crate::layer::LayerContent::Group { children } => children.clone(),
            _ => panic!("expected a group"),
        };
        assert!(children.is_empty());
    }

    #[test]
    fn mutators_update_layer_properties() {
        let mut doc = Document::new(100, 100);
        let id = doc.add_raster_layer("L");

        set_layer_visible(&mut doc, id, false).unwrap();
        assert!(!doc.layer(id).unwrap().visible);

        set_layer_opacity(&mut doc, id, 1.5).unwrap();
        assert_eq!(doc.layer(id).unwrap().opacity, 1.0);
        set_layer_opacity(&mut doc, id, -0.5).unwrap();
        assert_eq!(doc.layer(id).unwrap().opacity, 0.0);
        set_layer_opacity(&mut doc, id, 0.75).unwrap();
        assert_eq!(doc.layer(id).unwrap().opacity, 0.75);

        set_layer_blend(&mut doc, id, BlendMode::Multiply).unwrap();
        assert_eq!(doc.layer(id).unwrap().blend, BlendMode::Multiply);

        set_layer_locked(&mut doc, id, true).unwrap();
        assert!(doc.layer(id).unwrap().locked);

        rename_layer(&mut doc, id, "Renamed").unwrap();
        assert_eq!(doc.layer(id).unwrap().name, "Renamed");
    }

    #[test]
    fn reorder_layer_preserves_other_layers() {
        let mut doc = Document::new(100, 100);
        let a = doc.add_raster_layer("A");
        let b = doc.add_raster_layer("B");
        let c = doc.add_raster_layer("C");

        reorder_layer(&mut doc, a, 2).unwrap();
        assert_eq!(doc.order, vec![b, c, a]);
    }

    #[test]
    fn move_layer_into_group_preserves_other_layers() {
        let mut doc = Document::new(100, 100);
        let bg = doc.add_raster_layer("bg");
        let group = Layer::new_group("group");
        let group_id = doc.insert_layer_above(group, bg).unwrap();
        let a = doc.add_raster_layer("A");
        let b = doc.add_raster_layer("B");

        move_layer_into_group(&mut doc, a, group_id, 0).unwrap();
        assert_eq!(doc.order, vec![bg, group_id, b]);
        assert_eq!(doc.parent_of(a), Some(group_id));
    }

    #[test]
    fn mutators_return_not_found_for_missing_layer() {
        let mut doc = Document::new(100, 100);
        let missing = LayerId::default();

        assert!(matches!(
            delete_layer(&mut doc, missing),
            Err(OgreError::LayerNotFound(_))
        ));
        assert!(matches!(
            set_layer_visible(&mut doc, missing, false),
            Err(OgreError::LayerNotFound(_))
        ));
        assert!(matches!(
            set_layer_opacity(&mut doc, missing, 0.5),
            Err(OgreError::LayerNotFound(_))
        ));
        assert!(matches!(
            move_layer_by(&mut doc, missing, IVec2::new(1, 1)),
            Err(OgreError::LayerNotFound(_))
        ));
    }

    #[test]
    fn move_layer_by_rejects_locked_layer() {
        let mut doc = Document::new(100, 100);
        let id = doc.add_raster_layer("L");
        doc.layer_mut(id).unwrap().locked = true;

        assert!(matches!(
            move_layer_by(&mut doc, id, IVec2::new(5, 5)),
            Err(OgreError::LayerLocked(_))
        ));
    }

    // ------------------------------------------------------------------
    // Task 1.6.4 — Merge & move
    // ------------------------------------------------------------------

    #[test]
    fn merge_down_equals_composite_document() {
        let mut doc = Document::new(100, 100);
        let lower = doc.add_raster_layer("lower");
        let upper = doc.add_raster_layer("upper");

        doc.layer_mut(lower)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        doc.layer_mut(upper)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(0.0, 0.0, 1.0, 0.5));

        // Compute the expected merged appearance over the union bounds.
        let lower_bounds = doc
            .layer(lower)
            .unwrap()
            .buffer()
            .unwrap()
            .bounds()
            .unwrap();
        let upper_bounds = doc
            .layer(upper)
            .unwrap()
            .buffer()
            .unwrap()
            .bounds()
            .unwrap();
        let union = lower_bounds.union(upper_bounds);
        let expected = composite_document(&doc, union).unwrap();

        let merged = merge_down(&mut doc, upper).unwrap();
        assert_eq!(merged, lower);
        assert_eq!(doc.order, vec![lower]);

        let actual = doc.layer(lower).unwrap().buffer().unwrap().read_rect(union);
        assert_eq!(actual, expected);
    }

    #[test]
    fn merge_down_with_offsets_equals_composite_document() {
        let mut doc = Document::new(200, 200);
        let lower = doc.add_raster_layer("lower");
        let upper = doc.add_raster_layer("upper");

        doc.layer_mut(lower).unwrap().offset = IVec2::new(10, 10);
        doc.layer_mut(lower)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(0, 0), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        doc.layer_mut(upper).unwrap().offset = IVec2::new(8, 8);
        doc.layer_mut(upper)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(2, 2), Rgba32F::new(0.0, 0.0, 1.0, 0.5));

        let lower_bounds = doc
            .layer(lower)
            .unwrap()
            .buffer()
            .unwrap()
            .bounds()
            .unwrap();
        let upper_bounds = doc
            .layer(upper)
            .unwrap()
            .buffer()
            .unwrap()
            .bounds()
            .unwrap();
        let lower_offset = doc.layer(lower).unwrap().offset;
        let upper_offset = doc.layer(upper).unwrap().offset;
        let union = Rect::new(
            lower_bounds.x + lower_offset.x,
            lower_bounds.y + lower_offset.y,
            lower_bounds.w,
            lower_bounds.h,
        )
        .union(Rect::new(
            upper_bounds.x + upper_offset.x,
            upper_bounds.y + upper_offset.y,
            upper_bounds.w,
            upper_bounds.h,
        ));

        let expected = composite_document(&doc, union).unwrap();
        merge_down(&mut doc, upper).unwrap();

        let actual = doc.layer(lower).unwrap().buffer().unwrap().read_rect(union);
        assert_eq!(actual, expected);
    }

    #[test]
    fn merge_down_respects_lower_opacity() {
        let mut doc = Document::new(100, 100);
        let lower = doc.add_raster_layer("lower");
        let upper = doc.add_raster_layer("upper");

        doc.layer_mut(lower)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        doc.layer_mut(lower).unwrap().opacity = 0.25;

        doc.layer_mut(upper)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(0.0, 0.0, 1.0, 0.5));

        let lower_bounds = doc
            .layer(lower)
            .unwrap()
            .buffer()
            .unwrap()
            .bounds()
            .unwrap();
        let upper_bounds = doc
            .layer(upper)
            .unwrap()
            .buffer()
            .unwrap()
            .bounds()
            .unwrap();
        let union = lower_bounds.union(upper_bounds);
        let expected = composite_document(&doc, union).unwrap();

        let merged = merge_down(&mut doc, upper).unwrap();
        assert_eq!(merged, lower);

        let actual = doc.layer(lower).unwrap().buffer().unwrap().read_rect(union);
        assert_eq!(actual, expected);
    }

    #[test]
    fn merge_down_removes_upper_and_keeps_lower_position() {
        let mut doc = Document::new(100, 100);
        let lower = doc.add_raster_layer("lower");
        let upper = doc.add_raster_layer("upper");
        let unrelated = doc.add_raster_layer("unrelated");

        doc.layer_mut(upper)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(7, 7), Rgba32F::new(0.0, 1.0, 0.0, 1.0));

        let merged = merge_down(&mut doc, upper).unwrap();
        assert_eq!(merged, lower);
        assert_eq!(doc.order, vec![lower, unrelated]);
        assert!(matches!(doc.layer(upper), Err(OgreError::LayerNotFound(_))));
    }

    #[test]
    fn merge_down_errors_when_nothing_below() {
        let mut doc = Document::new(100, 100);
        let only = doc.add_raster_layer("only");
        assert!(matches!(
            merge_down(&mut doc, only),
            Err(OgreError::InvalidOperation(_))
        ));
    }

    #[test]
    fn merge_down_errors_on_group_source_or_target() {
        let mut doc = Document::new(100, 100);
        let raster = doc.add_raster_layer("raster");
        let group = Layer::new_group("group");
        let group_id = doc.insert_layer_above(group, raster).unwrap();

        assert!(matches!(
            merge_down(&mut doc, group_id),
            Err(OgreError::NotRaster)
        ));

        let upper = doc.add_raster_layer("upper");
        assert!(matches!(
            merge_down(&mut doc, upper),
            Err(OgreError::NotRaster)
        ));
    }

    #[test]
    fn merge_down_rejects_locked_layer() {
        let mut doc = Document::new(100, 100);
        let lower = doc.add_raster_layer("lower");
        let upper = doc.add_raster_layer("upper");
        doc.layer_mut(upper).unwrap().locked = true;

        assert!(matches!(
            merge_down(&mut doc, upper),
            Err(OgreError::LayerLocked(_))
        ));

        doc.layer_mut(upper).unwrap().locked = false;
        doc.layer_mut(lower).unwrap().locked = true;
        assert!(matches!(
            merge_down(&mut doc, upper),
            Err(OgreError::LayerLocked(_))
        ));
    }

    #[test]
    fn move_layer_by_shifts_composited_result_exactly() {
        let mut doc = Document::new(100, 100);
        let id = doc.add_raster_layer("L");
        doc.layer_mut(id).unwrap().offset = IVec2::new(5, 5);
        doc.layer_mut(id)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(0, 0), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        let before = composite_document(&doc, Rect::new(5, 5, 1, 1)).unwrap();
        assert_eq!(before[0], Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        move_layer_by(&mut doc, id, IVec2::new(10, 20)).unwrap();
        assert_eq!(doc.layer(id).unwrap().offset, IVec2::new(15, 25));

        let after = composite_document(&doc, Rect::new(15, 25, 1, 1)).unwrap();
        assert_eq!(after[0], Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        let old_spot = composite_document(&doc, Rect::new(5, 5, 1, 1)).unwrap();
        assert_eq!(old_spot[0], Rgba32F::TRANSPARENT);
    }

    #[test]
    fn merge_down_respects_selection_like_mask_on_source() {
        let mut doc = Document::new(100, 100);
        let lower = doc.add_raster_layer("lower");
        let upper = doc.add_raster_layer("upper");

        doc.layer_mut(lower)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        doc.layer_mut(upper)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(0.0, 0.0, 1.0, 1.0));

        // Add a layer mask to the upper layer that masks out half the pixel.
        let mut mask = TiledBuffer::new();
        mask.set_pixel(IVec2::new(5, 5), Rgba32F::new(0.5, 0.0, 0.0, 1.0));
        doc.layer_mut(upper).unwrap().content = crate::layer::LayerContent::Raster {
            buffer: doc.layer(upper).unwrap().buffer().unwrap().clone(),
            mask: Some(mask),
        };

        merge_down(&mut doc, upper).unwrap();

        // Expected: upper contributes 50% alpha blue over red.
        let expected = blend_pixel(
            BlendMode::Normal,
            Rgba32F::new(1.0, 0.0, 0.0, 1.0),
            Rgba32F::new(0.0, 0.0, 1.0, 0.5),
            1.0,
        );
        let actual = doc
            .layer(lower)
            .unwrap()
            .buffer()
            .unwrap()
            .get_pixel(IVec2::new(5, 5));
        assert_pixel_approx_eq(actual, expected, 1e-5);
    }

    #[test]
    fn merge_down_preserves_lower_layer_id() {
        let mut doc = Document::new(100, 100);
        let lower = doc.add_raster_layer("lower");
        let upper = doc.add_raster_layer("upper");

        doc.layer_mut(lower)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        doc.layer_mut(upper)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(0.0, 0.0, 1.0, 0.5));

        let merged = merge_down(&mut doc, upper).unwrap();
        assert_eq!(merged, lower);
        assert_eq!(doc.layer(lower).unwrap().id, lower);
    }

    #[test]
    fn merge_down_rejects_non_normal_lower_blend() {
        let mut doc = Document::new(100, 100);
        let lower = doc.add_raster_layer("lower");
        let upper = doc.add_raster_layer("upper");
        doc.layer_mut(lower).unwrap().blend = BlendMode::Multiply;

        assert!(matches!(
            merge_down(&mut doc, upper),
            Err(OgreError::InvalidOperation(_))
        ));
    }

    #[test]
    fn merge_down_with_upper_opacity_less_than_one() {
        let mut doc = Document::new(100, 100);
        let lower = doc.add_raster_layer("lower");
        let upper = doc.add_raster_layer("upper");

        doc.layer_mut(lower)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        doc.layer_mut(upper)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(0.0, 0.0, 1.0, 1.0));
        doc.layer_mut(upper).unwrap().opacity = 0.25;

        let lower_bounds = doc
            .layer(lower)
            .unwrap()
            .buffer()
            .unwrap()
            .bounds()
            .unwrap();
        let upper_bounds = doc
            .layer(upper)
            .unwrap()
            .buffer()
            .unwrap()
            .bounds()
            .unwrap();
        let union = lower_bounds.union(upper_bounds);
        let expected = composite_document(&doc, union).unwrap();

        let merged = merge_down(&mut doc, upper).unwrap();
        assert_eq!(merged, lower);

        let actual = doc.layer(lower).unwrap().buffer().unwrap().read_rect(union);
        assert_eq!(actual, expected);
    }

    #[test]
    fn merge_down_with_lower_layer_mask() {
        let mut doc = Document::new(100, 100);
        let lower = doc.add_raster_layer("lower");
        let upper = doc.add_raster_layer("upper");

        doc.layer_mut(lower)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        doc.layer_mut(upper)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(0.0, 0.0, 1.0, 1.0));

        // Mask the lower layer so it contributes only 50% alpha.
        let mut mask = TiledBuffer::new();
        mask.set_pixel(IVec2::new(5, 5), Rgba32F::new(0.5, 0.0, 0.0, 1.0));
        doc.layer_mut(lower).unwrap().content = crate::layer::LayerContent::Raster {
            buffer: doc.layer(lower).unwrap().buffer().unwrap().clone(),
            mask: Some(mask),
        };

        let lower_bounds = doc
            .layer(lower)
            .unwrap()
            .buffer()
            .unwrap()
            .bounds()
            .unwrap();
        let upper_bounds = doc
            .layer(upper)
            .unwrap()
            .buffer()
            .unwrap()
            .bounds()
            .unwrap();
        let union = lower_bounds.union(upper_bounds);
        let expected = composite_document(&doc, union).unwrap();

        let merged = merge_down(&mut doc, upper).unwrap();
        assert_eq!(merged, lower);

        let actual = doc.layer(lower).unwrap().buffer().unwrap().read_rect(union);
        assert_eq!(actual, expected);
    }

    #[test]
    fn merge_down_when_both_layers_are_empty() {
        let mut doc = Document::new(100, 100);
        let lower = doc.add_raster_layer("lower");
        let upper = doc.add_raster_layer("upper");

        let merged = merge_down(&mut doc, upper).unwrap();
        assert_eq!(merged, lower);
        assert_eq!(doc.order, vec![lower]);
        assert_eq!(doc.layer(lower).unwrap().id, lower);
        assert!(doc.layer(lower).unwrap().buffer().unwrap().is_empty());
    }

    #[test]
    fn merge_down_uses_exact_pixel_bounds() {
        let mut doc = Document::new(300, 300);
        let lower = doc.add_raster_layer("lower");
        let upper = doc.add_raster_layer("upper");

        doc.layer_mut(lower)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(10, 10), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        doc.layer_mut(upper)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(15, 12), Rgba32F::new(0.0, 0.0, 1.0, 1.0));

        merge_down(&mut doc, upper).unwrap();

        let merged = doc.layer(lower).unwrap().buffer().unwrap();
        assert_eq!(merged.exact_bounds(), Some(Rect::new(10, 10, 6, 3)));
    }

    #[test]
    fn move_layer_by_rejects_overflowing_offset() {
        let mut doc = Document::new(100, 100);
        let id = doc.add_raster_layer("L");
        doc.layer_mut(id).unwrap().offset = IVec2::new(i32::MAX, 0);

        assert!(matches!(
            move_layer_by(&mut doc, id, IVec2::new(1, 0)),
            Err(OgreError::InvalidOperation("layer offset out of range"))
        ));
        // The offset must remain unchanged after a rejected move.
        assert_eq!(doc.layer(id).unwrap().offset, IVec2::new(i32::MAX, 0));
    }

    #[test]
    fn merge_down_rejects_overflowing_offsets() {
        let mut doc = Document::new(100, 100);
        let lower = doc.add_raster_layer("lower");
        let upper = doc.add_raster_layer("upper");

        // An offset of i32::MAX pushes the 256-wide tile rect past i32::MAX.
        doc.layer_mut(lower).unwrap().offset = IVec2::new(i32::MAX, 0);
        doc.layer_mut(lower)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(0, 0), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        doc.layer_mut(upper)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(0, 0), Rgba32F::new(0.0, 1.0, 0.0, 1.0));

        assert!(matches!(
            merge_down(&mut doc, upper),
            Err(OgreError::InvalidOperation("layer offset out of range"))
        ));
    }

    #[test]
    fn merge_down_rejects_extreme_tile_budget() {
        let mut doc = Document::new(100, 100);
        let lower = doc.add_raster_layer("lower");
        let upper = doc.add_raster_layer("upper");

        // Fill just enough tiles in the lower layer to exceed the merge budget.
        for i in 0..=MAX_MERGE_TILES {
            doc.layer_mut(lower)
                .unwrap()
                .buffer_mut()
                .unwrap()
                .set_pixel(
                    IVec2::new(i as i32 * 256, 0),
                    Rgba32F::new(1.0, 0.0, 0.0, 1.0),
                );
        }

        assert!(matches!(
            merge_down(&mut doc, upper),
            Err(OgreError::InvalidOperation(
                "merged region exceeds tile budget"
            ))
        ));
    }

    fn assert_pixel_approx_eq(a: Rgba32F, b: Rgba32F, eps: f32) {
        assert!(
            (a.r - b.r).abs() < eps
                && (a.g - b.g).abs() < eps
                && (a.b - b.b).abs() < eps
                && (a.a - b.a).abs() < eps,
            "{:?} != {:?} within {}",
            a,
            b,
            eps
        );
    }
}
