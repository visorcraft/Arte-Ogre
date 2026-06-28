// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Verifies the bundled example plugins are discoverable and that the WASM
//! example actually loads and runs.

use ogre_core::Rgba32F;
use ogre_gpu::Filter;
use ogre_plugins::{PluginFilter, PluginKind, PluginManager};

fn examples_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins")
}

#[test]
fn example_plugins_are_discovered() {
    let manager = PluginManager::new(examples_dir());
    let valid = manager.valid_plugins();
    assert!(
        valid.iter().any(|p| p.kind == PluginKind::Lua),
        "the Lua macro example should be discovered"
    );
    assert!(
        valid.iter().any(|p| p.kind == PluginKind::Wasm),
        "the WASM invert example should be discovered"
    );
}

#[test]
fn invert_wasm_example_inverts_pixels() {
    let manager = PluginManager::new(examples_dir());
    let wasm_plugin = manager
        .valid_plugins()
        .into_iter()
        .find(|p| p.kind == PluginKind::Wasm)
        .expect("WASM invert example is present and valid");

    // The host loads the `.wat` text directly (wasmtime's `wat` feature).
    let module = std::fs::read(&wasm_plugin.entry).expect("read example module");
    let plugin = PluginFilter::new("Invert", module, Vec::new());
    let mut pixels = [Rgba32F::new(0.2, 0.4, 0.6, 1.0)]; // one straight-alpha RGBA pixel
    plugin.apply_cpu(&mut pixels, 1);

    assert!((pixels[0].r - 0.8).abs() < 1e-6, "r inverted");
    assert!((pixels[0].g - 0.6).abs() < 1e-6, "g inverted");
    assert!((pixels[0].b - 0.4).abs() < 1e-6, "b inverted");
    assert!((pixels[0].a - 1.0).abs() < 1e-6, "alpha untouched");
}
