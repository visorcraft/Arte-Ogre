// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Integration readback test for the GPU compositor.
//!
//! Verifies that `Compositor::read_region` matches the `ogre-core` CPU reference
//! compositor over the same region within the standard `1e-4` per-channel
//! tolerance.

use ogre_core::{BlendMode, Document, IVec2, Rect, Rgba32F, TiledBuffer};

use ogre_gpu::compositor::Compositor;
use ogre_gpu::context::GpuContext;

const EPSILON: f32 = 1e-4;

fn fill_rect(buffer: &mut TiledBuffer, rect: Rect, color: Rgba32F) {
    let area = (rect.w as usize)
        .checked_mul(rect.h as usize)
        .expect("rect too large");
    let data = vec![color; area];
    buffer.blit_rect(rect, &data);
}

fn assert_region_approx_eq(actual: &[Rgba32F], expected: &[Rgba32F], eps: f32) {
    assert_eq!(
        actual.len(),
        expected.len(),
        "region pixel counts differ: {} vs {}",
        actual.len(),
        expected.len()
    );
    for (i, (a, b)) in actual.iter().zip(expected.iter()).enumerate() {
        assert!(
            (a.r - b.r).abs() < eps
                && (a.g - b.g).abs() < eps
                && (a.b - b.b).abs() < eps
                && (a.a - b.a).abs() < eps,
            "pixel {} differs: got {:?}, expected {:?}",
            i,
            a,
            b
        );
    }
}

#[test]
fn read_region_of_multi_layer_document_matches_cpu_reference() {
    let ctx = GpuContext::headless();
    let mut doc = Document::new(512, 512);

    let bg = doc.add_raster_layer("bg");
    fill_rect(
        doc.layer_mut(bg).unwrap().buffer_mut().unwrap(),
        Rect::new(0, 0, 512, 512),
        Rgba32F::new(1.0, 0.0, 0.0, 1.0),
    );

    let fg1 = doc.add_raster_layer("fg1");
    fill_rect(
        doc.layer_mut(fg1).unwrap().buffer_mut().unwrap(),
        Rect::new(0, 0, 256, 256),
        Rgba32F::new(0.0, 1.0, 0.0, 0.5),
    );
    doc.layer_mut(fg1).unwrap().offset = IVec2::new(30, 30);
    doc.layer_mut(fg1).unwrap().blend = BlendMode::Multiply;
    doc.layer_mut(fg1).unwrap().opacity = 0.75;

    let fg2 = doc.add_raster_layer("fg2");
    fill_rect(
        doc.layer_mut(fg2).unwrap().buffer_mut().unwrap(),
        Rect::new(0, 0, 256, 256),
        Rgba32F::new(0.0, 0.0, 1.0, 0.5),
    );
    doc.layer_mut(fg2).unwrap().offset = IVec2::new(100, 100);
    doc.layer_mut(fg2).unwrap().blend = BlendMode::Screen;
    doc.layer_mut(fg2).unwrap().opacity = 0.5;

    let mut compositor = Compositor::new(&ctx);
    compositor
        .composite(&ctx, &doc, Rect::new(0, 0, 512, 512))
        .unwrap();

    let region = Rect::new(20, 20, 300, 300);
    let gpu = compositor.read_region(&ctx, region);
    let cpu = ogre_core::compositor::composite_document(&doc, region).unwrap();

    assert_region_approx_eq(&gpu, &cpu, EPSILON);
}

#[test]
fn read_region_with_non_tile_aligned_bounds_matches_cpu_reference() {
    let ctx = GpuContext::headless();
    let mut doc = Document::new(300, 300);

    let bg = doc.add_raster_layer("bg");
    fill_rect(
        doc.layer_mut(bg).unwrap().buffer_mut().unwrap(),
        Rect::new(0, 0, 300, 300),
        Rgba32F::new(0.2, 0.4, 0.6, 1.0),
    );

    let fg = doc.add_raster_layer("fg");
    fill_rect(
        doc.layer_mut(fg).unwrap().buffer_mut().unwrap(),
        Rect::new(0, 0, 150, 150),
        Rgba32F::new(1.0, 0.5, 0.0, 0.6),
    );
    doc.layer_mut(fg).unwrap().offset = IVec2::new(50, 50);

    let mut compositor = Compositor::new(&ctx);
    compositor
        .composite(&ctx, &doc, Rect::new(0, 0, 300, 300))
        .unwrap();

    // A region that does not start at a tile boundary and spans partial tiles.
    let region = Rect::new(37, 42, 199, 167);
    let gpu = compositor.read_region(&ctx, region);
    let cpu = ogre_core::compositor::composite_document(&doc, region).unwrap();

    assert_region_approx_eq(&gpu, &cpu, EPSILON);
}
