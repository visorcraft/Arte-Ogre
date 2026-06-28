// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Criterion benchmark for native `.ogre` save/load throughput.
//!
//! # Performance Budget
//!
//! | Benchmark | Target | Measured |
//! |-----------|--------|----------|
//! | `save_ogre_4_layer_1k` | < 50 ms | ~11.6 ms |
//! | `load_ogre_4_layer_1k` | < 100 ms | ~34.9 ms |

use std::hint::black_box;
use std::path::PathBuf;

use criterion::{criterion_group, criterion_main, Criterion};
use ogre_core::{Document, IVec2, Rgba32F};

fn make_document() -> Document {
    let mut doc = Document::new(1024, 1024);
    for i in 0..4 {
        let id = doc.add_raster_layer(&format!("Layer {i}"));
        let layer = doc.layer_mut(id).unwrap();
        let buffer = layer.buffer_mut().unwrap();
        // Touch a few tiles so the document has real pixel data.
        for y in (0..1024).step_by(256) {
            for x in (0..1024).step_by(256) {
                buffer.set_pixel(IVec2::new(x + i, y + i), Rgba32F::new(0.2, 0.4, 0.6, 1.0));
            }
        }
    }
    doc
}

fn temp_path(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ogre_io_bench_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn bench_native_save_load(c: &mut Criterion) {
    let doc = make_document();
    let path = temp_path("bench.ogre");

    c.bench_function("save_ogre_4_layer_1k", |b| {
        b.iter(|| {
            ogre_io::ogre::save(black_box(&doc), &path).unwrap();
        })
    });

    c.bench_function("load_ogre_4_layer_1k", |b| {
        b.iter(|| {
            let _loaded = ogre_io::ogre::load(&path).unwrap();
        })
    });
}

criterion_group!(benches, bench_native_save_load);
criterion_main!(benches);
