// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Proptest invariant suite for `ogre-core`.
//!
//! These properties must hold for every random input. They are run with
//! `PROPTEST_CASES=1024` as part of the quality gate.

use std::sync::Arc;

use proptest::prelude::*;

use ogre_core::{
    buffer::TiledBuffer,
    coord::{tile_to_pixel, IVec2, Rect, TileCoord},
    document::Document,
    history::{
        Command, CopyToNewLayerCmd, CutToNewLayerCmd, DeleteLayerCmd, History, PaintCmd, ReorderCmd,
    },
    layer::{BlendMode, LayerContent},
    ops::extract_selection,
    pixel::Rgba32F,
    selection::Selection,
};

// ------------------------------------------------------------------
// Content comparison helpers (ignore opaque LayerIds)
// ------------------------------------------------------------------

#[derive(Debug, PartialEq)]
struct LayerSnapshot {
    name: String,
    offset: IVec2,
    opacity: f32,
    blend: BlendMode,
    visible: bool,
    locked: bool,
    tiles: Vec<(TileCoord, Vec<Rgba32F>)>,
}

fn snapshot_layers(doc: &Document) -> Vec<LayerSnapshot> {
    doc.order
        .iter()
        .map(|&id| {
            let layer = doc.layer(id).expect("ordered layer exists");
            let mut tiles = match &layer.content {
                LayerContent::Raster { buffer, .. } => buffer
                    .occupied_tiles()
                    .into_iter()
                    .map(|(c, t)| (c, t.as_slice().to_vec()))
                    .collect::<Vec<_>>(),
                LayerContent::Group { .. }
                | LayerContent::Adjustment(_)
                | LayerContent::Vector(_) => Vec::new(),
            };
            tiles.sort_by_key(|(c, _)| (c.x, c.y));
            LayerSnapshot {
                name: layer.name.clone(),
                offset: layer.offset,
                opacity: layer.opacity,
                blend: layer.blend,
                visible: layer.visible,
                locked: layer.locked,
                tiles,
            }
        })
        .collect()
}

fn assert_content_equal(expected: &[LayerSnapshot], doc: &Document) {
    let actual = snapshot_layers(doc);
    assert_eq!(expected, &actual);
}

// ------------------------------------------------------------------
// (a) COW isolation across random clones+writes
// ------------------------------------------------------------------

proptest! {
    #[test]
    fn cow_isolation_across_random_clones_and_writes(
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

        prop_assert_eq!(original.get_pixel(seed), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        prop_assert_eq!(snapshot.get_pixel(seed), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        // The tile at (0,0) was never written in `original`/`snapshot`, so it
        // must still be Arc-shared between them.
        let shared = Arc::ptr_eq(
            &original.get_tile(TileCoord { x: 0, y: 0 }).unwrap(),
            &snapshot.get_tile(TileCoord { x: 0, y: 0 }).unwrap(),
        );
        prop_assert!(shared);
    }
}

// ------------------------------------------------------------------
// (b) undo/redo round-trip on random op sequences
// ------------------------------------------------------------------

#[derive(Debug, Clone)]
enum RandomOp {
    Paint {
        x: i32,
        y: i32,
        r: u8,
        g: u8,
        b: u8,
        a: u8,
    },
    Copy {
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    },
    Cut {
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    },
    Delete,
}

fn random_op() -> impl Strategy<Value = RandomOp> {
    prop_oneof![
        (
            (-50..=250i32),
            (-50..=250i32),
            (0u8..=255),
            (0u8..=255),
            (0u8..=255),
            (1u8..=255)
        )
            .prop_map(|(x, y, r, g, b, a)| RandomOp::Paint { x, y, r, g, b, a }),
        ((-50..=200i32), (-50..=200i32), (1u32..=50), (1u32..=50))
            .prop_map(|(x, y, w, h)| RandomOp::Copy { x, y, w, h }),
        ((-50..=200i32), (-50..=200i32), (1u32..=50), (1u32..=50))
            .prop_map(|(x, y, w, h)| RandomOp::Cut { x, y, w, h }),
        Just(RandomOp::Delete),
    ]
}

fn apply_random_op(
    doc: &mut Document,
    history: &mut History,
    available: &mut Vec<ogre_core::LayerId>,
    deletable: &mut Vec<ogre_core::LayerId>,
    op: &RandomOp,
) {
    if available.is_empty() {
        return;
    }

    // All mutating commands reference only the original layers whose ids are
    // preserved across undo/redo. Layers created by copy/cut are never touched
    // again, so their changing ids on redo cannot break the command stack.
    let source = deletable[0 % deletable.len()];

    let cmd: Box<dyn Command> = match *op {
        RandomOp::Paint { x, y, r, g, b, a } => {
            let color = Rgba32F::new(
                r as f32 / 255.0,
                g as f32 / 255.0,
                b as f32 / 255.0,
                a as f32 / 255.0,
            );
            Box::new(PaintCmd::new(source, vec![(IVec2::new(x, y), color)]))
        }
        RandomOp::Copy { x, y, w, h } => {
            let sel = Selection::rect(Rect::new(x, y, w, h));
            Box::new(CopyToNewLayerCmd::new(source, sel))
        }
        RandomOp::Cut { x, y, w, h } => {
            let sel = Selection::rect(Rect::new(x, y, w, h));
            Box::new(CutToNewLayerCmd::new(source, sel))
        }
        RandomOp::Delete => {
            if deletable.len() <= 1 {
                return;
            }
            let layer = deletable.remove(0 % deletable.len());
            if let Some(pos) = available.iter().position(|&id| id == layer) {
                available.remove(pos);
            }
            Box::new(DeleteLayerCmd::new(layer))
        }
    };

    if history.do_command(doc, cmd).is_ok() {
        if let Some(new_id) = doc.active {
            if !available.contains(&new_id) {
                available.push(new_id);
            }
        }
    }
}

proptest! {
    #[test]
    fn undo_redo_round_trip_on_random_op_sequences(
        ops in prop::collection::vec(random_op(), 1..16usize)
    ) {
        let mut doc = Document::new(300, 300);
        let a = doc.add_raster_layer("A");
        let b = doc.add_raster_layer("B");
        let original = snapshot_layers(&doc);

        let mut history = History::new(0);
        let mut available = vec![a, b];
        let mut deletable = vec![a, b];

        for op in &ops {
            apply_random_op(&mut doc, &mut history, &mut available, &mut deletable, op);
        }

        let post_apply = snapshot_layers(&doc);

        while history.undo_len() > 0 {
            history.undo(&mut doc);
        }
        assert_content_equal(&original, &doc);

        while history.redo_len() > 0 {
            let before = history.redo_len();
            history.redo(&mut doc);
            assert!(
                history.redo_len() < before,
                "redo failed and was pushed back; stack stuck at {}",
                before
            );
        }
        assert_content_equal(&post_apply, &doc);
    }
}

// ------------------------------------------------------------------
// Reorder round-trip (delete-free so layer ids stay stable)
// ------------------------------------------------------------------

#[derive(Debug, Clone)]
enum ReorderOp {
    Paint {
        x: i32,
        y: i32,
        r: u8,
        g: u8,
        b: u8,
        a: u8,
    },
    Copy {
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    },
    Cut {
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    },
    Reorder {
        new_idx: usize,
    },
}

fn reorder_op() -> impl Strategy<Value = ReorderOp> {
    prop_oneof![
        (
            (-50..=250i32),
            (-50..=250i32),
            (0u8..=255),
            (0u8..=255),
            (0u8..=255),
            (1u8..=255)
        )
            .prop_map(|(x, y, r, g, b, a)| ReorderOp::Paint { x, y, r, g, b, a }),
        ((-50..=200i32), (-50..=200i32), (1u32..=50), (1u32..=50))
            .prop_map(|(x, y, w, h)| ReorderOp::Copy { x, y, w, h }),
        ((-50..=200i32), (-50..=200i32), (1u32..=50), (1u32..=50))
            .prop_map(|(x, y, w, h)| ReorderOp::Cut { x, y, w, h }),
        (0usize..=10usize).prop_map(|new_idx| ReorderOp::Reorder { new_idx }),
    ]
}

proptest! {
    #[test]
    fn undo_redo_round_trip_with_reorder(
        ops in prop::collection::vec(reorder_op(), 1..16usize)
    ) {
        let mut doc = Document::new(300, 300);
        let a = doc.add_raster_layer("A");
        let b = doc.add_raster_layer("B");
        let original = snapshot_layers(&doc);

        let mut history = History::new(0);
        let mut available = vec![a, b];
        let originals = [a, b];

        for op in &ops {
            let source = originals[0 % originals.len()];
            let cmd: Box<dyn Command> = match *op {
                ReorderOp::Paint { x, y, r, g, b, a } => {
                    let color = Rgba32F::new(
                        r as f32 / 255.0,
                        g as f32 / 255.0,
                        b as f32 / 255.0,
                        a as f32 / 255.0,
                    );
                    Box::new(PaintCmd::new(source, vec![(IVec2::new(x, y), color)]))
                }
                ReorderOp::Copy { x, y, w, h } => {
                    Box::new(CopyToNewLayerCmd::new(source, Selection::rect(Rect::new(x, y, w, h))))
                }
                ReorderOp::Cut { x, y, w, h } => {
                    Box::new(CutToNewLayerCmd::new(source, Selection::rect(Rect::new(x, y, w, h))))
                }
                ReorderOp::Reorder { new_idx } => {
                    let target = new_idx % available.len().max(1);
                    Box::new(ReorderCmd::new(source, target))
                }
            };

            if history.do_command(&mut doc, cmd).is_ok() {
                if let Some(new_id) = doc.active {
                    if !available.contains(&new_id) {
                        available.push(new_id);
                    }
                }
            }
        }

        let post_apply = snapshot_layers(&doc);

        while history.undo_len() > 0 {
            history.undo(&mut doc);
        }
        assert_content_equal(&original, &doc);

        while history.redo_len() > 0 {
            history.redo(&mut doc);
        }
        assert_content_equal(&post_apply, &doc);
    }
}

// ------------------------------------------------------------------
// (c) pixel_to_tile ∘ tile_to_pixel round-trips
// ------------------------------------------------------------------

proptest! {
    #[test]
    fn pixel_to_tile_tile_to_pixel_round_trips(x in any::<i32>(), y in any::<i32>()) {
        let p = IVec2::new(x, y);
        let (c, lx, ly) = ogre_core::coord::pixel_to_tile(p);
        prop_assert_eq!(tile_to_pixel(c, lx, ly), Some(p));
    }
}

// ------------------------------------------------------------------
// (d) extract → place is exact for random selections/offsets
// ------------------------------------------------------------------

proptest! {
    #[test]
    fn extract_then_place_is_exact(
        offset in (-500i32..=500, -500i32..=500),
        target in (-400i32..=400, -400i32..=400),
    ) {
        let src_offset = IVec2::new(offset.0, offset.1);
        let target = IVec2::new(target.0, target.1);
        let local = target - src_offset;

        let mut source = TiledBuffer::new();
        let color = Rgba32F::new(0.2, 0.4, 0.9, 1.0);
        source.set_pixel(local, color);

        // Selection comfortably contains the target pixel.
        let sel = Selection::rect(Rect::new(target.x - 5, target.y - 5, 11, 11));
        let extracted = extract_selection(&source, src_offset, &sel);

        let mut placed = TiledBuffer::new();
        if let Some(bounds) = extracted.bounds() {
            placed.copy_region(&extracted, bounds, IVec2::ZERO);
        }

        prop_assert_eq!(placed.get_pixel(target), color);
    }
}
