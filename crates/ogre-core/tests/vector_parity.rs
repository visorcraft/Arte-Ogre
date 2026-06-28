// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Byte-identity integration tests for the vector-layer commands (spec test #4).
//!
//! `AddVectorLayerCmd` and `EditVectorCmd` must round-trip through History
//! undo/redo leaving the vector layer's content byte-identical to the pre-undo
//! state. Mirrors the raster-only `killer_feature` pattern, extended to capture
//! `LayerContent::Vector` data.

use ogre_core::{
    buffer::TiledBuffer,
    composite_document,
    coord::{IVec2, Rect},
    document::Document,
    history::History,
    layer::{
        LayerContent, StrokeCap, StrokeJoin, SvgSource, VectorData, VectorFill, VectorPath,
        VectorStroke,
    },
    pixel::Rgba32F,
    AddVectorLayerCmd, EditVectorCmd,
};

/// Content snapshot of a single vector layer (ignores the opaque `LayerId`).
#[derive(Debug, PartialEq)]
struct VectorLayerSnapshot {
    name: String,
    offset: IVec2,
    data: VectorData,
}

fn snapshot_vector_layer(doc: &Document) -> VectorLayerSnapshot {
    let id = doc.active.expect("active layer");
    let layer = doc.layer(id).expect("active layer exists");
    let LayerContent::Vector(data) = &layer.content else {
        panic!("active layer must be vector");
    };
    VectorLayerSnapshot {
        name: layer.name.clone(),
        offset: layer.offset,
        data: data.as_ref().clone(),
    }
}

fn sample_path(fill: Rgba32F, closed: bool) -> VectorPath {
    VectorPath {
        vertices: vec![
            IVec2::new(0, 0),
            IVec2::new(20, 0),
            IVec2::new(20, 12),
            IVec2::new(0, 12),
        ],
        fill: VectorFill::Solid(fill),
        stroke: VectorStroke {
            color: Rgba32F::new(1.0, 1.0, 1.0, 1.0),
            width: 2.0,
            dash: vec![],
            cap: StrokeCap::Butt,
            join: StrokeJoin::Miter,
        },
        closed,
    }
}

#[test]
fn add_vector_layer_cmd_undo_redo_is_byte_identical() {
    let mut doc = Document::new(100, 100);
    let _bg = doc.add_raster_layer("Background");

    let data = VectorData {
        paths: vec![sample_path(Rgba32F::new(0.1, 0.2, 0.3, 1.0), true)],
        rasterized: None,
        text: None,
        svg_source: None,
        version: 0,
    };

    let mut history = History::new(0);
    let cmd = AddVectorLayerCmd::new("Rectangle", data.clone()).with_offset(IVec2::new(7, 9));
    history.do_command(&mut doc, Box::new(cmd)).unwrap();

    let pre_undo = snapshot_vector_layer(&doc);
    assert_eq!(pre_undo.offset, IVec2::new(7, 9));

    // Undo: the vector layer is soft-removed, background is active again.
    assert_eq!(history.undo(&mut doc), Some("Add vector layer"));
    assert_eq!(doc.order.len(), 1, "undo must remove the vector layer");

    // Redo: the same layer is restored with identical content.
    assert_eq!(history.redo(&mut doc), Some("Add vector layer"));

    let post_redo = snapshot_vector_layer(&doc);
    assert_eq!(
        pre_undo, post_redo,
        "AddVectorLayerCmd must round-trip byte-identically"
    );
}

#[test]
fn edit_vector_cmd_undo_redo_is_byte_identical() {
    let mut doc = Document::new(100, 100);
    let _bg = doc.add_raster_layer("Background");

    // Start with a vector layer on the document.
    let original = VectorData {
        paths: vec![sample_path(Rgba32F::new(0.0, 0.0, 0.0, 1.0), true)],
        rasterized: None,
        text: None,
        svg_source: None,
        version: 0,
    };
    let vid = doc.add_vector_layer("Shape", original.clone());
    doc.active = Some(vid);

    let mut history = History::new(0);

    // Edit the vector data to a new color and version.
    let edited = VectorData {
        paths: vec![sample_path(Rgba32F::new(1.0, 0.0, 0.0, 1.0), true)],
        rasterized: None,
        text: None,
        svg_source: None,
        version: 1,
    };
    history
        .do_command(&mut doc, Box::new(EditVectorCmd::new(vid, edited.clone())))
        .unwrap();

    let after_edit = snapshot_vector_layer(&doc);
    assert_eq!(after_edit.data, edited);

    // Undo restores the original data.
    assert_eq!(history.undo(&mut doc), Some("Edit vector"));
    let after_undo = snapshot_vector_layer(&doc);
    assert_eq!(after_undo.data, original);

    // Redo restores the edited data byte-identically.
    assert_eq!(history.redo(&mut doc), Some("Edit vector"));
    let after_redo = snapshot_vector_layer(&doc);
    assert_eq!(
        after_redo, after_edit,
        "EditVectorCmd must round-trip byte-identically"
    );
}

/// The CPU compositor treats `VectorData::rasterized` as the authoritative pixel
/// source, even when an optional `SvgSource` payload is attached. `ogre-core`
/// does not need an SVG renderer.
#[test]
fn composite_document_vector_uses_rasterized_cache_with_svg_source() {
    let mut buffer = TiledBuffer::new();
    let red = Rgba32F::new(1.0, 0.0, 0.0, 1.0);
    buffer.set_pixel(IVec2::new(0, 0), red);

    let data = VectorData {
        paths: Vec::new(),
        rasterized: Some(buffer),
        text: None,
        svg_source: Some(SvgSource {
            source_bytes:
                br#"<svg xmlns="http://www.w3.org/2000/svg"><rect width="1" height="1"/></svg>"#
                    .to_vec(),
            element_id: String::new(),
        }),
        version: 0,
    };

    let mut doc = Document::new(1, 1);
    let vid = doc.add_vector_layer("Svg-backed", data);
    doc.layer_mut(vid).unwrap().offset = IVec2::ZERO;

    let region = Rect::new(0, 0, 1, 1);
    let output = composite_document(&doc, region).unwrap();
    assert_eq!(output[0], red);
}
