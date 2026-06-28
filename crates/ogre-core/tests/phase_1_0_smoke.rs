// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Smoke test: every declared core module is reachable.

#[test]
fn modules_are_declared() {
    use ogre_core::{
        brush, buffer, commands, compositor, coord, document, error, history, layer, ops, pixel,
        resample, selection, text, tile,
    };

    let _ = std::any::type_name::<brush::BrushSettings>();
    let _ = std::any::type_name::<buffer::TiledBuffer>();
    let _ = std::any::type_name::<commands::BatchCmd>();
    let _ = compositor::composite_document;
    let _ = std::any::type_name::<coord::Rect>();
    let _ = std::any::type_name::<document::Document>();
    let _ = std::any::type_name::<error::OgreError>();
    let _ = std::any::type_name::<history::History>();
    let _ = std::any::type_name::<layer::Layer>();
    let _ = ops::fill_region;
    let _ = std::any::type_name::<pixel::Rgba32F>();
    let _ = std::any::type_name::<resample::WarpGrid>();
    let _ = std::any::type_name::<selection::Selection>();
    let _ = std::any::type_name::<text::TextBlock>();
    let _ = std::any::type_name::<tile::Tile>();
}
