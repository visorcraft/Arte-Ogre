// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! End-to-end acceptance smoke test.
//!
//! Drives the whole editing pipeline through the public command/history and
//! file-I/O APIs, asserting the headline guarantees along the way:
//!
//! blank -> paint (brush) -> select -> cut to new layer (pixel-exact) ->
//! transform (move) -> adjust -> save `.ogre` -> reload identical -> export PNG.

use glam::Vec2;
use ogre_core::{
    composite_document, AddAdjustmentLayerCmd, AddRasterLayerCmd, AdjustmentKind, BrushSettings,
    CutToNewLayerCmd, Document, History, IVec2, InputSample, MoveLayerByCmd, PaintMode,
    PaintStrokeCmd, Rect, Rgba32F, Selection, SetSelectionCmd,
};
use ogre_io::{export_image, ExportOptions, RasterFormat};

#[test]
fn full_pipeline_blank_to_export() {
    // 1. Start from a blank document with an undo history.
    let mut doc = Document::new(64, 64);
    let mut history = History::new(64);

    // 2. Paint a hard, opaque brush stroke onto a fresh raster layer.
    history
        .do_command(&mut doc, Box::new(AddRasterLayerCmd::new("paint")))
        .unwrap();
    let paint_layer = doc.active.expect("a new raster layer is active");

    let brush = BrushSettings {
        size: 6.0,
        hardness: 1.0,
        opacity: 1.0,
        flow: 1.0,
        spacing: 0.34,
        pressure_size: false,
        pressure_opacity: false,
    };
    let samples = vec![
        InputSample::new(Vec2::new(20.0, 20.0)),
        InputSample::new(Vec2::new(30.0, 20.0)),
    ];
    history
        .do_command(
            &mut doc,
            Box::new(PaintStrokeCmd::new(
                paint_layer,
                samples,
                brush,
                Rgba32F::new(1.0, 0.0, 0.0, 1.0),
                PaintMode::Brush,
            )),
        )
        .unwrap();

    let painted = doc
        .layer(paint_layer)
        .unwrap()
        .buffer()
        .unwrap()
        .get_pixel(IVec2::new(25, 20));
    assert!(
        painted.a > 0.9,
        "brush stroke should paint opaque pixels: {painted:?}"
    );

    // 3. Select a rectangle covering the stroke.
    history
        .do_command(
            &mut doc,
            Box::new(SetSelectionCmd::new(Selection::rect(Rect::new(
                14, 14, 24, 14,
            )))),
        )
        .unwrap();

    // 4. Cut to a new layer — the killer feature: the extracted pixels keep
    //    their EXACT document position, and the source is erased.
    let before: std::collections::HashSet<_> = doc.order.iter().copied().collect();
    let selection = doc.selection.clone();
    history
        .do_command(
            &mut doc,
            Box::new(CutToNewLayerCmd::new(paint_layer, selection)),
        )
        .unwrap();
    let cut_layer = *doc
        .order
        .iter()
        .find(|id| !before.contains(id))
        .expect("cut created a new layer");

    assert_eq!(
        doc.layer(paint_layer)
            .unwrap()
            .buffer()
            .unwrap()
            .get_pixel(IVec2::new(25, 20))
            .a,
        0.0,
        "cut must erase the selection from the source layer"
    );
    let cut_px = doc
        .layer(cut_layer)
        .unwrap()
        .buffer()
        .unwrap()
        .get_pixel(IVec2::new(25, 20));
    assert!(
        cut_px.a > 0.9,
        "cut layer must retain the pixel at its exact position: {cut_px:?}"
    );

    // 5. Transform: move the cut layer by an integer offset (pixel-exact, no
    //    resample).
    history
        .do_command(
            &mut doc,
            Box::new(MoveLayerByCmd::new(cut_layer, IVec2::new(8, 4))),
        )
        .unwrap();

    // 6. Adjust: add a non-destructive invert adjustment layer on top.
    history
        .do_command(
            &mut doc,
            Box::new(AddAdjustmentLayerCmd::new("invert", AdjustmentKind::Invert)),
        )
        .unwrap();

    // Reference composite of the finished scene.
    let region = Rect::new(0, 0, 64, 64);
    let reference = composite_document(&doc, region).unwrap();

    let dir = std::env::temp_dir().join(format!("ogre_e2e_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // 7 + 8. Save to `.ogre` and reload — the reloaded document must composite
    //        bit-identically.
    let ogre_path = dir.join("scene.ogre");
    ogre_io::save(&doc, &ogre_path).unwrap();
    let loaded = ogre_io::load(&ogre_path).unwrap();
    let restored = composite_document(&loaded, region).unwrap();
    assert_eq!(
        reference, restored,
        ".ogre save/reload must reproduce the document exactly"
    );

    // 9. Export a flattened PNG and confirm it decodes at the right size.
    let png_path = dir.join("scene.png");
    export_image(
        &doc,
        region,
        &ExportOptions {
            format: RasterFormat::Png,
            ..Default::default()
        },
        &png_path,
    )
    .unwrap();
    let decoded = image::open(&png_path).expect("exported PNG decodes");
    assert_eq!(decoded.width(), 64);
    assert_eq!(decoded.height(), 64);

    // Undo back to the blank document to confirm the whole pipeline is undoable.
    while history.undo_len() > 0 {
        history.undo(&mut doc);
    }
    assert!(
        doc.order.is_empty()
            || doc.order.iter().all(|&id| {
                doc.layer(id)
                    .map(|l| l.buffer().map(|b| b.is_empty()).unwrap_or(true))
                    .unwrap_or(true)
            }),
        "undoing the whole session returns to an empty canvas"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
