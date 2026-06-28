// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Criterion benchmarks for GPU filter throughput.
//!
//! # Performance Budget
//!
//! | Benchmark | Target | Measured |
//! |-----------|--------|----------|
//! | `invert_filter 1920x1080` | < 8 ms | ~21.3 ms |
//! | `brightness_contrast_filter 1920x1080` | < 8 ms | ~21.2 ms |
//! | `gaussian_blur_filter 1920x1080 sigma=5` | < 16 ms | ~21.4 ms |

use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion};
use ogre_core::Rgba32F;
use ogre_gpu::{BrightnessContrastFilter, FilterRunner, GaussianBlurFilter, InvertFilter};

fn make_input(width: u32, height: u32) -> Vec<Rgba32F> {
    (0..width * height)
        .map(|i| {
            let x = (i % width) as f32 / width.max(1) as f32;
            let y = (i / width) as f32 / height.max(1) as f32;
            Rgba32F::new(x, y, 0.5, 0.9)
        })
        .collect()
}

fn bench_invert_filter(c: &mut Criterion) {
    let ctx = ogre_gpu::context::GpuContext::headless();
    let runner = FilterRunner::new();
    let filter = InvertFilter::new();
    let input = make_input(1920, 1080);

    c.bench_function("invert_filter 1920x1080", |b| {
        b.iter(|| {
            let _ = runner.run_dyn(&ctx, &filter, &input, 1920, 1080).unwrap();
        })
    });
}

fn bench_brightness_contrast_filter(c: &mut Criterion) {
    let ctx = ogre_gpu::context::GpuContext::headless();
    let runner = FilterRunner::new();
    let filter = BrightnessContrastFilter::new(0.1, 1.2);
    let input = make_input(1920, 1080);

    c.bench_function("brightness_contrast_filter 1920x1080", |b| {
        b.iter(|| {
            let _ = runner.run_dyn(&ctx, &filter, &input, 1920, 1080).unwrap();
        })
    });
}

fn bench_gaussian_blur_filter(c: &mut Criterion) {
    let ctx = ogre_gpu::context::GpuContext::headless();
    let runner = FilterRunner::new();
    let filter = GaussianBlurFilter::new(5.0);
    let input = make_input(1920, 1080);

    c.bench_function("gaussian_blur_filter 1920x1080 sigma5", |b| {
        b.iter(|| {
            let _ = runner.run_dyn(&ctx, &filter, &input, 1920, 1080).unwrap();
        })
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(20)
        .measurement_time(Duration::from_secs(3));
    targets = bench_invert_filter, bench_brightness_contrast_filter, bench_gaussian_blur_filter
}
criterion_main!(benches);
