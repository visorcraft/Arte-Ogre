// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Tests for `InsertLayerSubtreeCmd`, the destination-side command that powers
//! cross-document layer move/copy in Bird's Eye View.

use ogre_core::buffer::TiledBuffer;
use ogre_core::coord::IVec2;
use ogre_core::document::Document;
use ogre_core::history::{Command, History, InsertLayerSubtreeCmd};
use ogre_core::layer::{AdjustmentKind, BlendMode, Layer, LayerContent, VectorData};
use ogre_core::pixel::Rgba32F;

const RED: Rgba32F = Rgba32F {
    r: 1.0,
    g: 0.0,
    b: 0.0,
    a: 1.0,
};

fn group_children(doc: &Document, id: ogre_core::layer::LayerId) -> Vec<ogre_core::layer::LayerId> {
    match &doc.layer(id).unwrap().content {
        LayerContent::Group { children } => children.clone(),
        other => panic!("expected group, got {other:?}"),
    }
}

#[test]
fn birdseye_inserts_raster_layer_at_index() {
    let mut src = Document::new(64, 64);
    let lid = src.add_raster_layer("src");

    let mut dest = Document::new(64, 64);
    dest.add_raster_layer("a");
    dest.add_raster_layer("b");

    let mut hist = History::new(0);
    let cmd = InsertLayerSubtreeCmd::new_from_source(&src, lid, 1).unwrap();
    hist.do_command(&mut dest, Box::new(cmd)).unwrap();

    assert_eq!(dest.order.len(), 3);
    let inserted = dest.order[1];
    assert_eq!(dest.layer(inserted).unwrap().name, "src");
    assert_eq!(dest.active, Some(inserted));
}

#[test]
fn birdseye_preserves_offset_opacity_blend_visibility_locked_and_mask() {
    let mut src = Document::new(64, 64);
    let lid = src.add_raster_layer("styled");
    {
        let layer = src.layer_mut(lid).unwrap();
        layer.offset = IVec2::new(7, 9);
        layer.opacity = 0.5;
        layer.blend = BlendMode::Multiply;
        layer.visible = false;
        layer.locked = true;
        layer.buffer_mut().unwrap().set_pixel(IVec2::new(2, 3), RED);
        let mut mask = TiledBuffer::new();
        mask.set_pixel(IVec2::new(2, 3), Rgba32F::new(0.5, 0.0, 0.0, 1.0));
        layer.set_mask(Some(mask));
    }

    let mut dest = Document::new(64, 64);
    let mut hist = History::new(0);
    let cmd = InsertLayerSubtreeCmd::new_from_source(&src, lid, 0).unwrap();
    hist.do_command(&mut dest, Box::new(cmd)).unwrap();

    let inserted = dest.active.unwrap();
    let layer = dest.layer(inserted).unwrap();
    assert_eq!(layer.offset, IVec2::new(7, 9));
    assert_eq!(layer.opacity, 0.5);
    assert_eq!(layer.blend, BlendMode::Multiply);
    assert!(!layer.visible);
    assert!(layer.locked);
    assert_eq!(layer.buffer().unwrap().get_pixel(IVec2::new(2, 3)), RED);
    assert_eq!(
        layer.mask().unwrap().get_pixel(IVec2::new(2, 3)),
        Rgba32F::new(0.5, 0.0, 0.0, 1.0)
    );
}

#[test]
fn birdseye_inserts_vector_and_adjustment_layers() {
    let mut src = Document::new(64, 64);
    let mut vdata = VectorData::new();
    vdata.version = 3;
    let vec_id = src.add_vector_layer("paths", vdata.clone());
    let adj_id = src.add_adjustment_layer("invert", AdjustmentKind::Invert);

    let mut dest = Document::new(64, 64);
    let mut hist = History::new(0);

    hist.do_command(
        &mut dest,
        Box::new(InsertLayerSubtreeCmd::new_from_source(&src, vec_id, 0).unwrap()),
    )
    .unwrap();
    let inserted_vec = dest.active.unwrap();
    assert_eq!(
        dest.layer(inserted_vec).unwrap().content,
        LayerContent::Vector(Box::new(vdata))
    );

    hist.do_command(
        &mut dest,
        Box::new(InsertLayerSubtreeCmd::new_from_source(&src, adj_id, 1).unwrap()),
    )
    .unwrap();
    let inserted_adj = dest.active.unwrap();
    assert_eq!(
        dest.layer(inserted_adj).unwrap().content,
        LayerContent::Adjustment(AdjustmentKind::Invert)
    );
}

#[test]
fn birdseye_copies_group_with_children_and_remaps_child_ids() {
    let mut src = Document::new(64, 64);
    let base = src.add_raster_layer("base");
    let group = src
        .insert_layer_above(Layer::new_group("grp"), base)
        .unwrap();
    let child = src.add_raster_layer("child");
    src.layer_mut(child)
        .unwrap()
        .buffer_mut()
        .unwrap()
        .set_pixel(IVec2::new(1, 1), RED);
    src.move_into_group(child, group, 0).unwrap();

    let mut dest = Document::new(64, 64);
    let mut hist = History::new(0);
    let cmd = InsertLayerSubtreeCmd::new_from_source(&src, group, 0).unwrap();
    hist.do_command(&mut dest, Box::new(cmd)).unwrap();

    let root = dest.active.unwrap();
    let children = group_children(&dest, root);
    assert_eq!(children.len(), 1);
    let dest_child = children[0];
    // The destination child must be a fresh id, not the source id.
    assert_ne!(dest_child, child);
    assert_eq!(dest.layer(dest_child).unwrap().name, "child");
    assert_eq!(
        dest.layer(dest_child)
            .unwrap()
            .buffer()
            .unwrap()
            .get_pixel(IVec2::new(1, 1)),
        RED
    );
}

#[test]
fn birdseye_cleanup_hard_removes_group_subtree_after_undo() {
    let mut src = Document::new(64, 64);
    let base = src.add_raster_layer("base");
    let group = src
        .insert_layer_above(Layer::new_group("grp"), base)
        .unwrap();
    let child = src.add_raster_layer("child");
    src.move_into_group(child, group, 0).unwrap();

    let mut dest = Document::new(64, 64);
    let mut hist = History::new(0);
    hist.do_command(
        &mut dest,
        Box::new(InsertLayerSubtreeCmd::new_from_source(&src, group, 0).unwrap()),
    )
    .unwrap();

    let inserted_group = dest.active.unwrap();
    let inserted_child = group_children(&dest, inserted_group)[0];

    hist.undo(&mut dest);
    hist.clear(&mut dest);

    assert!(
        !dest.all_layers().contains_key(inserted_group),
        "cleanup must hard-remove the inserted group"
    );
    assert!(
        !dest.all_layers().contains_key(inserted_child),
        "cleanup must hard-remove the inserted child"
    );
    assert!(
        dest.removed_layers().is_empty(),
        "cleanup must not leave stale removed ids"
    );
}

#[test]
fn birdseye_undo_redo_is_byte_identical_for_copied_raster() {
    let mut src = Document::new(64, 64);
    let lid = src.add_raster_layer("payload");
    src.layer_mut(lid)
        .unwrap()
        .buffer_mut()
        .unwrap()
        .set_pixel(IVec2::new(4, 5), RED);

    let mut dest = Document::new(64, 64);
    dest.add_raster_layer("bg");
    let baseline = dest.clone();

    let mut hist = History::new(0);
    hist.do_command(
        &mut dest,
        Box::new(InsertLayerSubtreeCmd::new_from_source(&src, lid, 1).unwrap()),
    )
    .unwrap();
    let inserted = dest.active.unwrap();
    let inserted_pixel = dest
        .layer(inserted)
        .unwrap()
        .buffer()
        .unwrap()
        .get_pixel(IVec2::new(4, 5));
    assert_eq!(inserted_pixel, RED);

    hist.undo(&mut dest);
    assert_eq!(dest.order, baseline.order);

    hist.redo(&mut dest);
    let redone = dest.active.unwrap();
    // Same stable id after redo (soft-restore), same pixels.
    assert_eq!(redone, inserted);
    assert_eq!(
        dest.layer(redone)
            .unwrap()
            .buffer()
            .unwrap()
            .get_pixel(IVec2::new(4, 5)),
        RED
    );
}

#[test]
fn birdseye_redo_after_cleanup_rebuilds_from_snapshot() {
    let mut src = Document::new(64, 64);
    let lid = src.add_raster_layer("payload");
    src.layer_mut(lid)
        .unwrap()
        .buffer_mut()
        .unwrap()
        .set_pixel(IVec2::new(4, 5), RED);

    let mut dest = Document::new(64, 64);
    let mut cmd = InsertLayerSubtreeCmd::new_from_source(&src, lid, 0).unwrap();

    cmd.apply(&mut dest).unwrap();
    let root1 = dest.active.unwrap();

    cmd.undo(&mut dest);
    cmd.cleanup(&mut dest);
    assert!(!dest.all_layers().contains_key(root1));

    // Redo after the prior copy was hard-removed must rebuild from the snapshot.
    cmd.apply(&mut dest).unwrap();
    let root2 = dest.active.unwrap();
    assert_eq!(dest.layer(root2).unwrap().name, "payload");
    assert_eq!(
        dest.layer(root2)
            .unwrap()
            .buffer()
            .unwrap()
            .get_pixel(IVec2::new(4, 5)),
        RED
    );
}
