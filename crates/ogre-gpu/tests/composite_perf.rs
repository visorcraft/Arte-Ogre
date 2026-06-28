// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Characterizes the GPU compositor's interactive recomposite cost.
//!
//! `#[ignore]`d by default (GPU-heavy, perf measurement, not a correctness
//! gate). Run with:
//!   cargo test -p ogre-gpu --test composite_perf -- --ignored --nocapture

use std::time::Instant;

use ogre_core::{Document, IVec2, Rect, Rgba32F};
use ogre_gpu::compositor::Compositor;
use ogre_gpu::context::GpuContext;

fn build_50_layer_4k() -> Document {
    let mut doc = Document::new(3840, 2160);
    let region = Rect::new(0, 0, 3840, 2160);
    for i in 0..50 {
        let id = doc.add_raster_layer(&format!("layer-{i}"));
        let t = i as f32 / 50.0;
        let color = Rgba32F::new(t, 0.5, 1.0 - t, 0.9);
        let pixels = vec![color; (region.w * region.h) as usize];
        doc.layer_mut(id)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .blit_rect(region, &pixels);
    }
    doc
}

#[test]
#[ignore = "GPU perf measurement; run explicitly with --ignored --nocapture"]
fn gpu_recomposite_50_layer_4k() {
    let ctx = GpuContext::headless();
    let mut compositor = Compositor::new(&ctx);
    let mut doc = build_50_layer_4k();
    let region = Rect::new(0, 0, 3840, 2160);

    // Worst case: first composite, every output tile dirty. Read back one tile
    // to force the GPU submission to complete before stopping the clock.
    let t0 = Instant::now();
    compositor.composite(&ctx, &doc, region).unwrap();
    let full_dispatches = compositor.dispatch_count();
    let _ = compositor.read_result_tile(&ctx, ogre_core::TileCoord { x: 0, y: 0 });
    let full = t0.elapsed();

    // Steady state: nothing changed, every output tile served from cache.
    let t1 = Instant::now();
    compositor.composite(&ctx, &doc, region).unwrap();
    let steady_dispatches = compositor.dispatch_count();
    let _ = compositor.read_result_tile(&ctx, ogre_core::TileCoord { x: 0, y: 0 });
    let steady = t1.elapsed();

    // Interactive edit: paint one pixel (dirties one source tile) and recomposite.
    doc.layer_mut(doc.order[25])
        .unwrap()
        .buffer_mut()
        .unwrap()
        .set_pixel(IVec2::new(100, 100), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
    let t2 = Instant::now();
    compositor.composite(&ctx, &doc, region).unwrap();
    let edit_dispatches = compositor.dispatch_count();
    let _ = compositor.read_result_tile(&ctx, ogre_core::TileCoord { x: 0, y: 0 });
    let edit = t2.elapsed();

    println!("GPU 50-layer 4K composite:");
    println!("  full (cold):   {full:?}  ({full_dispatches} dispatches)");
    println!("  steady (warm): {steady:?}  ({steady_dispatches} dispatches)");
    println!("  one-tile edit: {edit:?}  ({edit_dispatches} dispatches)");
}
