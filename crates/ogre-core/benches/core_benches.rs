// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Criterion benchmarks for `ogre-core`.
//!
//! # Performance Budget
//!
//! These budgets are initial targets for the CPU reference implementation.
//! They should be tightened as GPU paths replace CPU-only hot paths.
//!
//! | Benchmark | Target | Measured |
//! |-----------|--------|----------|
//! | `copy_selection_to_new_layer` over 2048×2048 | < 30 ms | ~93.8 ms |
//! | `composite_document` of 20 layers over 1920×1080 | < 200 ms | ~390.8 ms |
//! | `paint` snapshot + undo of one pixel | O(1) one tile | ~15.4 µs |
//! | `paint_stroke` 20 samples with 20 px brush | < 16 ms | ~715 ms |
//!
//! The CPU reference currently exceeds the aggressive initial cut-to-layer and
//! composite budgets; GPU compositing should beat the CPU composite by at least
//! 10×.

use std::time::Duration;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};

use ogre_core::{
    brush::{BrushSettings, InputSample, PaintMode},
    composite_document,
    coord::{IVec2, Rect},
    copy_selection_to_new_layer,
    document::Document,
    history::{History, PaintCmd, PaintStrokeCmd},
    pixel::Rgba32F,
    selection::Selection,
};

fn bench_copy_selection_to_new_layer(c: &mut Criterion) {
    let mut doc = Document::new(2048, 2048);
    let src = doc.add_raster_layer("src");

    let region = Rect::new(0, 0, 2048, 2048);
    let red = Rgba32F::new(1.0, 0.0, 0.0, 1.0);
    let pixels = vec![red; (region.w * region.h) as usize];
    doc.layer_mut(src)
        .unwrap()
        .buffer_mut()
        .unwrap()
        .blit_rect(region, &pixels);

    let sel = Selection::rect(region);

    c.bench_function("copy_selection_to_new_layer 2048x2048", |b| {
        b.iter_batched(
            || doc.clone(),
            |mut doc| {
                copy_selection_to_new_layer(&mut doc, src, &sel).unwrap();
            },
            BatchSize::LargeInput,
        )
    });
}

fn bench_composite_document(c: &mut Criterion) {
    let mut doc = Document::new(1920, 1080);

    let region = Rect::new(0, 0, 1920, 1080);
    for i in 0..20 {
        let name = format!("layer-{i}");
        let id = doc.add_raster_layer(&name);
        let t = i as f32 / 20.0;
        let color = Rgba32F::new(t, 0.5, 1.0 - t, 0.9);
        let pixels = vec![color; (region.w * region.h) as usize];
        doc.layer_mut(id)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .blit_rect(region, &pixels);
    }

    c.bench_function("composite_document 20 layers 1920x1080", |b| {
        b.iter(|| composite_document(&doc, region).unwrap())
    });
}

fn bench_paint_snapshot_undo(c: &mut Criterion) {
    let mut doc = Document::new(256, 256);
    let layer = doc.add_raster_layer("paint");

    // Materialise exactly one tile so the snapshot captures one Arc<Tile>.
    doc.layer_mut(layer)
        .unwrap()
        .buffer_mut()
        .unwrap()
        .set_pixel(IVec2::new(0, 0), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

    let stroke = vec![(IVec2::new(1, 1), Rgba32F::new(0.0, 1.0, 0.0, 1.0))];

    c.bench_function("paint snapshot+undo single pixel", |b| {
        b.iter_batched(
            || doc.clone(),
            |mut doc| {
                let mut history = History::new(0);
                history
                    .do_command(&mut doc, Box::new(PaintCmd::new(layer, stroke.clone())))
                    .unwrap();
                history.undo(&mut doc);
            },
            BatchSize::SmallInput,
        )
    });
}

fn bench_brush_stroke_latency(c: &mut Criterion) {
    let mut doc = Document::new(1024, 1024);
    let layer = doc.add_raster_layer("paint");

    // A 20-sample stroke with a 20 px hard brush.
    let samples: Vec<InputSample> = (0..20)
        .map(|i| InputSample::new(glam::Vec2::new(100.0 + i as f32 * 20.0, 512.0)))
        .collect();
    let settings = BrushSettings {
        size: 20.0,
        hardness: 1.0,
        opacity: 1.0,
        flow: 1.0,
        spacing: 0.25,
        pressure_size: false,
        pressure_opacity: false,
    };
    let color = Rgba32F::new(1.0, 0.0, 0.0, 1.0);

    c.bench_function("paint_stroke 20 samples 20px brush", |b| {
        b.iter_batched(
            || doc.clone(),
            |mut doc| {
                let mut history = History::new(0);
                history
                    .do_command(
                        &mut doc,
                        Box::new(PaintStrokeCmd::new(
                            layer,
                            samples.clone(),
                            settings,
                            color,
                            PaintMode::Brush,
                        )),
                    )
                    .unwrap();
            },
            BatchSize::SmallInput,
        )
    });
}

criterion_group! {
    name = core_benches;
    config = Criterion::default()
        .sample_size(20)
        .measurement_time(Duration::from_secs(3));
    targets = bench_copy_selection_to_new_layer,
        bench_composite_document,
        bench_paint_snapshot_undo,
        bench_brush_stroke_latency
}
criterion_main!(core_benches);
