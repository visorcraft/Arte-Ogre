// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Heavy CPU benchmarks for `ogre-core`.
//!
//! These run with fewer samples and shorter measurement times because each
//! iteration touches a large document.
//!
//! | Benchmark | Target | Measured |
//! |-----------|--------|----------|
//! | `cut_selection_to_new_layer` over 512×512 | < 30 ms | ~7.97 s |
//! | `composite_document` of 50 layers over 3840×2160 | < 16 ms (GPU) | ~3.87 s |

use std::time::Duration;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};

use ogre_core::{
    composite_document, coord::Rect, cut_selection_to_new_layer, document::Document,
    pixel::Rgba32F, selection::Selection,
};

fn bench_cut_selection_to_new_layer(c: &mut Criterion) {
    let mut doc = Document::new(512, 512);
    let src = doc.add_raster_layer("src");

    let region = Rect::new(0, 0, 512, 512);
    let red = Rgba32F::new(1.0, 0.0, 0.0, 1.0);
    let pixels = vec![red; (region.w * region.h) as usize];
    doc.layer_mut(src)
        .unwrap()
        .buffer_mut()
        .unwrap()
        .blit_rect(region, &pixels);

    let sel = Selection::rect(region);

    c.bench_function("cut_selection_to_new_layer 512x512", |b| {
        b.iter_batched(
            || doc.clone(),
            |mut doc| {
                cut_selection_to_new_layer(&mut doc, src, &sel).unwrap();
            },
            BatchSize::LargeInput,
        )
    });
}

fn bench_composite_4k_50_layers(c: &mut Criterion) {
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

    c.bench_function("composite_document 50 layers 3840x2160", |b| {
        b.iter(|| composite_document(&doc, region).unwrap())
    });
}

criterion_group! {
    name = heavy_benches;
    config = Criterion::default()
        .sample_size(10)
        .measurement_time(Duration::from_secs(2));
    targets = bench_cut_selection_to_new_layer,
        bench_composite_4k_50_layers
}
criterion_main!(heavy_benches);
