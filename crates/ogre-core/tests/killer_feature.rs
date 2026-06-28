// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! End-to-end integration test for Arte Ogre's headline feature.
//!
//! The full headless product loop exercised here: create a document, fill a
//! region, rectangle-select it, cut it to a new layer, move the new layer,
//! then undo and redo everything. After the loop the document content must be
//! byte-identical to the pre-undo state.

use ogre_core::{
    coord::{IVec2, Rect, TileCoord},
    document::Document,
    history::{CutToNewLayerCmd, History, MoveLayerByCmd},
    layer::{BlendMode, LayerContent},
    pixel::Rgba32F,
    selection::Selection,
};

/// A content snapshot of a document that ignores the opaque [`LayerId`]s,
/// which change when a cut-to-new-layer command is redone.
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
            let layer = doc.layer(id).expect("order layer exists");
            let mut tiles = match &layer.content {
                LayerContent::Raster { buffer, .. } => buffer
                    .occupied_tiles()
                    .into_iter()
                    .map(|(c, t)| (c, t.as_slice().to_vec()))
                    .collect::<Vec<_>>(),
                LayerContent::Group { .. } => panic!("integration test uses only raster layers"),
                LayerContent::Adjustment(_) | LayerContent::Vector(_) => Vec::new(),
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

fn active_index(doc: &Document) -> Option<usize> {
    doc.active.map(|id| {
        doc.order
            .iter()
            .position(|&x| x == id)
            .expect("active in order")
    })
}

fn assert_content_equal(snapshot: &[LayerSnapshot], doc: &Document) {
    let actual = snapshot_layers(doc);
    assert_eq!(
        snapshot, &actual,
        "document content did not round-trip through undo/redo"
    );
    assert_eq!(
        active_index(doc),
        if doc.active.is_some() {
            Some(doc.order.len().saturating_sub(1))
        } else {
            None
        },
        "active layer position changed unexpectedly"
    );
}

#[test]
fn cut_move_undo_redo_loop_is_byte_identical() {
    let mut doc = Document::new(500, 500);
    let bg = doc.add_raster_layer("Background");

    // Fill a region on the background.
    let region = Rect::new(50, 50, 100, 100);
    let red = Rgba32F::new(1.0, 0.0, 0.0, 1.0);
    let pixels = vec![red; (region.w * region.h) as usize];
    doc.layer_mut(bg)
        .unwrap()
        .buffer_mut()
        .unwrap()
        .blit_rect(region, &pixels);

    // Cut the selection to a new layer via History.
    let sel = Selection::rect(region);
    let mut history = History::new(0);
    history
        .do_command(&mut doc, Box::new(CutToNewLayerCmd::new(bg, sel)))
        .unwrap();

    let new_id = doc.active.expect("cut creates and activates a new layer");
    let new_layer = doc.layer(new_id).unwrap();
    assert_eq!(new_layer.offset, IVec2::ZERO);
    assert_eq!(
        new_layer.buffer().unwrap().get_pixel(IVec2::new(50, 50)),
        red
    );
    assert_eq!(
        new_layer.buffer().unwrap().get_pixel(IVec2::new(149, 149)),
        red
    );
    assert_eq!(
        doc.layer(bg)
            .unwrap()
            .buffer()
            .unwrap()
            .get_pixel(IVec2::new(50, 50)),
        Rgba32F::TRANSPARENT,
        "source region must be cleared by the cut"
    );

    // Move the new layer.
    let delta = IVec2::new(30, -20);
    history
        .do_command(&mut doc, Box::new(MoveLayerByCmd::new(new_id, delta)))
        .unwrap();
    assert_eq!(
        doc.layer(doc.active.unwrap()).unwrap().offset,
        delta,
        "new layer should have moved by delta"
    );

    // Capture the pre-undo content.
    let pre_undo = snapshot_layers(&doc);

    // Undo twice: move, then cut.
    assert_eq!(history.undo(&mut doc), Some("Move layer"));
    assert_eq!(history.undo(&mut doc), Some("Cut to new layer"));
    assert!(history.undo(&mut doc).is_none());

    // Redo twice: cut, then move.
    assert_eq!(history.redo(&mut doc), Some("Cut to new layer"));
    assert_eq!(history.redo(&mut doc), Some("Move layer"));
    assert!(history.redo(&mut doc).is_none());

    assert_content_equal(&pre_undo, &doc);
}
