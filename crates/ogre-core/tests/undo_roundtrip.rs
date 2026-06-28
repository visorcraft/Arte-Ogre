// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Round-trip tests for the undo system.
//!
//! These tests apply a pseudo-random sequence of document operations through
//! the [`History`] stack, then undo every step and assert that the document
//! content is byte-identical to the original. Because deleting a layer and
//! undoing the deletion re-inserts it into the [`slotmap::SlotMap`] with a
//! fresh key, the comparison ignores layer ids and compares tree structure,
//! layer properties, and tile pixels.

use std::sync::Arc;

use ogre_core::{
    coord::{IVec2, Rect},
    document::Document,
    history::{
        Command, CopyToNewLayerCmd, CutToNewLayerCmd, DeleteLayerCmd, History, PaintCmd, ReorderCmd,
    },
    layer::LayerId,
    pixel::Rgba32F,
    selection::Selection,
};

/// A tiny deterministic LCG for test generation.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
        self.0
    }

    fn range(&mut self, max: usize) -> usize {
        if max == 0 {
            return 0;
        }
        (self.next() as usize) % max
    }

    fn i32_range(&mut self, min: i32, max: i32) -> i32 {
        min + (self.next() as i32).rem_euclid(max - min + 1)
    }
}

fn random_color(rng: &mut Rng) -> Rgba32F {
    let mut f = || (rng.next() % 256) as f32 / 255.0;
    Rgba32F::new(f(), f(), f(), f().max(0.01))
}

/// Compare two documents by content, ignoring the opaque [`LayerId`]s that
/// change when a deleted layer is re-inserted by undo.
fn assert_docs_content_equal(original: &Document, actual_doc: &Document) {
    assert_eq!(original.canvas, actual_doc.canvas);
    assert_eq!(original.selection, actual_doc.selection);
    assert_eq!(original.color_space, actual_doc.color_space);
    assert_eq!(original.order.len(), actual_doc.order.len());

    for (orig_id, actual_id) in original.order.iter().zip(actual_doc.order.iter()) {
        let orig = original.layer(*orig_id).unwrap();
        let actual = actual_doc.layer(*actual_id).unwrap();

        assert_eq!(orig.name, actual.name);
        assert_eq!(orig.offset, actual.offset);
        assert_eq!(orig.opacity, actual.opacity);
        assert_eq!(orig.blend, actual.blend);
        assert_eq!(orig.visible, actual.visible);
        assert_eq!(orig.locked, actual.locked);

        match (&orig.content, &actual.content) {
            (
                ogre_core::layer::LayerContent::Raster { buffer: ba, .. },
                ogre_core::layer::LayerContent::Raster { buffer: bb, .. },
            ) => {
                assert_eq!(ba.bounds(), bb.bounds());
                assert_eq!(ba.occupied_tiles().len(), bb.occupied_tiles().len());
                for (coord, tile_a) in ba.occupied_tiles() {
                    let tile_b = bb
                        .get_tile(coord)
                        .unwrap_or_else(|| panic!("missing tile at {:?}", coord));
                    assert!(
                        Arc::ptr_eq(&tile_a, &tile_b) || tile_a.as_slice() == tile_b.as_slice(),
                        "tile at {:?} differs",
                        coord
                    );
                }
            }
            (
                ogre_core::layer::LayerContent::Adjustment(ka),
                ogre_core::layer::LayerContent::Adjustment(kb),
            ) => assert_eq!(ka, kb),
            _ => panic!("non-raster layer in flat test"),
        }
    }

    // Both active layers must point to layers with the same position in the
    // root order, or both be None.
    match (original.active, actual_doc.active) {
        (Some(orig), Some(actual_id)) => {
            let orig_pos = original.order.iter().position(|&id| id == orig);
            let actual_pos = actual_doc_order_position(actual_id, actual_doc);
            assert_eq!(
                orig_pos, actual_pos,
                "active layer position changed after undo"
            );
        }
        (None, None) => {}
        _ => panic!("active layer mismatch"),
    }
}

fn actual_doc_order_position(id: LayerId, doc: &Document) -> Option<usize> {
    doc.order.iter().position(|&x| x == id)
}

#[test]
fn deterministic_op_sequence_undo_returns_to_original() {
    let mut doc = Document::new(200, 200);
    let a = doc.add_raster_layer("A");
    let b = doc.add_raster_layer("B");
    let c = doc.add_raster_layer("C");

    // Seed each layer with a few pixels so copy/cut have content to move.
    doc.layer_mut(a)
        .unwrap()
        .buffer_mut()
        .unwrap()
        .set_pixel(IVec2::new(10, 10), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
    doc.layer_mut(b)
        .unwrap()
        .buffer_mut()
        .unwrap()
        .set_pixel(IVec2::new(50, 50), Rgba32F::new(0.0, 1.0, 0.0, 1.0));
    doc.layer_mut(c)
        .unwrap()
        .buffer_mut()
        .unwrap()
        .set_pixel(IVec2::new(90, 90), Rgba32F::new(0.0, 0.0, 1.0, 1.0));

    doc.active = Some(a);

    let original = doc.clone();
    let mut history = History::new(0);
    let mut rng = Rng(12345);

    // Maintain the list of layers that currently exist and can be edited.
    let mut available = vec![a, b, c];

    for _ in 0..40 {
        if available.is_empty() {
            break;
        }
        let op = rng.range(5);
        let layer = available[rng.range(available.len())];

        let cmd: Box<dyn Command> = match op {
            0 => {
                let p = IVec2::new(rng.i32_range(0, 199), rng.i32_range(0, 199));
                Box::new(PaintCmd::new(layer, vec![(p, random_color(&mut rng))]))
            }
            1 => {
                let x = rng.i32_range(0, 180);
                let y = rng.i32_range(0, 180);
                let w = rng.i32_range(1, 20) as u32;
                let h = rng.i32_range(1, 20) as u32;
                let sel = Selection::rect(Rect::new(x, y, w, h));
                Box::new(CopyToNewLayerCmd::new(layer, sel))
            }
            2 => {
                let x = rng.i32_range(0, 180);
                let y = rng.i32_range(0, 180);
                let w = rng.i32_range(1, 20) as u32;
                let h = rng.i32_range(1, 20) as u32;
                let sel = Selection::rect(Rect::new(x, y, w, h));
                Box::new(CutToNewLayerCmd::new(layer, sel))
            }
            3 => {
                if let Some(pos) = available.iter().position(|&id| id == layer) {
                    available.remove(pos);
                }
                Box::new(DeleteLayerCmd::new(layer))
            }
            _ => {
                let new_index = rng.range(available.len().max(1));
                Box::new(ReorderCmd::new(layer, new_index))
            }
        };

        if let Ok(()) = history.do_command(&mut doc, cmd) {
            // If the command created a new layer, add it to the available pool.
            // Newly created layers are never added to `deletable`, so they can
            // never be the source of a stale id after an undo.
            if let Some(new_id) = doc.active {
                if !available.contains(&new_id) {
                    available.push(new_id);
                }
            }
        }
    }

    while history.undo_len() > 0 {
        history.undo(&mut doc);
    }

    assert_docs_content_equal(&original, &doc);
    // Clear the history so commands can reclaim any soft-deleted layers
    // (e.g. from CutToNewLayerCmd) that are no longer referenced.
    history.clear(&mut doc);
    // With the soft-delete id-preservation in layer commands, the documents
    // should now be fully equal, including layer ids.
    assert_eq!(original, doc);
}
