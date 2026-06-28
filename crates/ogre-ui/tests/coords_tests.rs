// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Screen↔document coordinate mapping tests for the canvas panel.

use glam::{IVec2, Vec2};
use ogre_ui::panels::canvas::{apply_pan, apply_zoom, doc_to_screen, screen_to_doc};

fn vp(pan: Vec2, zoom: f32) -> ogre_gpu::Viewport {
    ogre_gpu::Viewport::new(pan, zoom)
}

#[test]
fn screen_to_doc_top_left_at_origin() {
    let viewport = vp(Vec2::ZERO, 1.0);
    let doc = screen_to_doc(Vec2::ZERO, &viewport, 1.0);
    assert_eq!(doc, IVec2::ZERO);
}

#[test]
fn screen_to_doc_zoom_two() {
    let viewport = vp(Vec2::ZERO, 2.0);
    let doc = screen_to_doc(Vec2::new(20.0, 30.0), &viewport, 1.0);
    assert_eq!(doc, IVec2::new(10, 15));
}

#[test]
fn screen_to_doc_dpi_two() {
    let viewport = vp(Vec2::ZERO, 1.0);
    // At DPI 2.0, 10 logical pixels = 20 physical pixels, so doc = (20, 20).
    let doc = screen_to_doc(Vec2::new(10.0, 10.0), &viewport, 2.0);
    assert_eq!(doc, IVec2::new(20, 20));
}

#[test]
fn doc_to_screen_and_back_round_trips() {
    let viewport = vp(Vec2::new(12.5, -7.0), 2.5);
    let ppp = 1.5;

    for doc in [IVec2::new(0, 0), IVec2::new(55, 33), IVec2::new(-10, 200)] {
        let screen = doc_to_screen(doc, &viewport, ppp);
        let back = screen_to_doc(screen, &viewport, ppp);
        assert_eq!(back, doc, "round-trip failed for {:?}", doc);
    }
}

#[test]
fn apply_zoom_keeps_cursor_doc_fixed() {
    let mut viewport = vp(Vec2::ZERO, 1.0);
    let cursor = Vec2::new(100.0, 80.0);
    let ppp = 1.0;

    let before = screen_to_doc(cursor, &viewport, ppp);
    apply_zoom(&mut viewport, cursor, 120.0, ppp);
    let after = screen_to_doc(cursor, &viewport, ppp);

    assert!(
        (viewport.zoom - 1.0).abs() > 1e-3,
        "zoom did not change: {}",
        viewport.zoom
    );
    assert_eq!(before, after, "doc point under cursor changed");
}

#[test]
fn apply_pan_shifts_pan_by_delta_over_zoom() {
    let mut viewport = vp(Vec2::new(10.0, 20.0), 2.0);
    let delta = Vec2::new(30.0, -15.0);
    apply_pan(&mut viewport, delta);
    let expected = Vec2::new(10.0 + 30.0 / 2.0, 20.0 + (-15.0) / 2.0);
    assert!(
        (viewport.pan - expected).length() < 1e-4,
        "pan mismatch: {:?} vs {:?}",
        viewport.pan,
        expected
    );
}
