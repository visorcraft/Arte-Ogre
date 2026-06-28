// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Filter adapter: run a WASM tile-filter plugin as an [`ogre_gpu::Filter`].
//!
//! The plugin is executed on the CPU tile-by-tile by [`PluginFilter::apply_cpu`];
//! the GPU pipeline sees a passthrough shader so preview/compute paths do not
//! crash, but plugins are currently CPU-only destructive filters.
//!
//! Plugins receive one RGBA32F tile (up to 256×256 pixels) at a time and must
//! respect the `width`/`height` passed to `process`. They should be local,
//! point-wise tile filters. Spatial filters that sample neighbouring pixels
//! will see tile boundaries and are not supported by this adapter.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use ogre_core::{tile_rect, IVec2, OgreError, Rect, Rgba32F, TileCoord, TiledBuffer};
use ogre_gpu::{Filter, ParamsBuffer};
use wasmtime::{Config, Engine, Instance, Module, ResourceLimiter, Store, StoreLimitsBuilder};

/// Maximum number of compiled WASM modules kept in the process-wide cache.
const MAX_CACHED_MODULES: usize = 32;

fn plugin_engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE.get_or_init(|| {
        let mut config = Config::new();
        config.consume_fuel(true);
        Engine::new(&config).expect("wasmtime engine creation failed")
    })
}

fn module_cache() -> &'static Mutex<HashMap<Vec<u8>, Arc<Module>>> {
    static CACHE: OnceLock<Mutex<HashMap<Vec<u8>, Arc<Module>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A destructive filter backed by a WASM tile-filter plugin.
#[derive(Clone)]
pub struct PluginFilter {
    /// Interned `'static` display name for the [`Filter::label`] contract.
    /// Leaked exactly once per filter in [`Self::new`] (not per `label()` call).
    label: &'static str,
    /// Compiled WASM module bytes.
    wasm: Vec<u8>,
    /// Parameter values passed to the plugin's `process` function.
    params: Vec<f32>,
    /// Fuel budget for one `apply_cpu` call.
    fuel: u64,
}

impl std::fmt::Debug for PluginFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginFilter")
            .field("label", &self.label)
            .field("wasm_len", &self.wasm.len())
            .field("params", &self.params)
            .field("fuel", &self.fuel)
            .finish()
    }
}

impl PluginFilter {
    /// Create a filter from a compiled WASM tile-filter plugin.
    pub fn new(name: impl Into<String>, wasm: Vec<u8>, params: Vec<f32>) -> Self {
        // The `Filter::label` trait requires `&'static str`. Intern the name a
        // single time here; cloning the filter copies this `&'static str`
        // rather than leaking again.
        let label: &'static str = Box::leak(name.into().into_boxed_str());
        Self {
            label,
            wasm,
            params,
            fuel: 10_000_000,
        }
    }

    /// Set the fuel budget for one application of the filter.
    #[cfg(test)]
    pub fn with_fuel(mut self, fuel: u64) -> Self {
        self.fuel = fuel;
        self
    }

    /// CPU reference implementation with an explicit per-invocation fuel budget.
    ///
    /// The public [`Filter::apply_cpu`] calls this with the filter's configured
    /// fuel; the tile-at-a-time [`Filter::apply_to_tiled_buffer`] divides that
    /// per-filter budget across the occupied tiles, capping total instructions
    /// to roughly `self.fuel`.
    fn apply_cpu_with_fuel(
        &self,
        pixels: &mut [Rgba32F],
        width: u32,
        fuel: u64,
    ) -> ogre_core::Result<()> {
        // Nothing to do for an empty slice.
        if pixels.is_empty() {
            return Ok(());
        }
        // Require a whole number of rows; a non-rectangular slice would make the
        // returned `w*h*4` buffer shorter than `tile` and panic in
        // `copy_from_slice`.
        if width == 0 || !pixels.len().is_multiple_of(width as usize) {
            return Err(OgreError::FilterFailed(
                "plugin filter received non-rectangular pixel slice",
            ));
        }
        let height = pixels.len() as u32 / width;
        let tile: &mut [f32] = bytemuck::cast_slice_mut(pixels);

        let Some(module) = self.compiled_module() else {
            return Err(OgreError::FilterFailed(
                "plugin filter failed to compile WASM module",
            ));
        };

        let limits = StoreLimitsBuilder::new()
            .memory_size(crate::MAX_PLUGIN_MEMORY)
            .build();
        let mut store = Store::new(module.engine(), limits);
        if store.set_fuel(fuel).is_err() {
            return Err(OgreError::FilterFailed(
                "plugin filter failed to set per-tile fuel budget",
            ));
        }
        store.limiter(|limits| limits as &mut dyn ResourceLimiter);

        let instance = match Instance::new(&mut store, &module, &[]) {
            Ok(i) => i,
            Err(_) => {
                return Err(OgreError::FilterFailed(
                    "plugin filter failed to instantiate WASM module",
                ))
            }
        };

        let memory = match instance.get_memory(&mut store, "memory") {
            Some(m) => m,
            None => {
                return Err(OgreError::FilterFailed(
                    "plugin filter has no exported memory",
                ))
            }
        };
        let process = match instance
            .get_typed_func::<(i32, i32, i32, i32, i32, i32), i32>(&mut store, "process")
        {
            Ok(f) => f,
            Err(_) => {
                return Err(OgreError::FilterFailed(
                    "plugin filter has no exported `process` function",
                ))
            }
        };

        let in_bytes = bytemuck::cast_slice(tile);
        let params_bytes = bytemuck::cast_slice(&self.params);

        let in_offset = 0;
        let params_offset = in_bytes.len().next_multiple_of(4);
        let out_offset = (params_offset + params_bytes.len()).next_multiple_of(4);
        let required = out_offset + in_bytes.len();
        if crate::grow_memory_to(&mut store, &memory, required).is_err() {
            return Err(OgreError::FilterFailed("plugin filter memory grow denied"));
        }

        if memory.write(&mut store, in_offset, in_bytes).is_err() {
            return Err(OgreError::FilterFailed("plugin filter input write failed"));
        }
        if memory
            .write(&mut store, params_offset, params_bytes)
            .is_err()
        {
            return Err(OgreError::FilterFailed("plugin filter params write failed"));
        }

        let out_ptr = match process.call(
            &mut store,
            (
                in_offset as i32,
                in_bytes.len() as i32,
                width as i32,
                height as i32,
                params_offset as i32,
                params_bytes.len() as i32,
            ),
        ) {
            Ok(p) => p,
            Err(_) => {
                return Err(OgreError::FilterFailed(
                    "plugin filter execution failed (out of fuel or trap)",
                ))
            }
        };

        let expected_floats = (width as usize)
            .saturating_mul(height as usize)
            .saturating_mul(4);
        let expected_bytes = expected_floats.saturating_mul(4);
        let mut out_bytes = vec![0u8; expected_bytes];
        if memory
            .read(&store, out_ptr as usize, &mut out_bytes)
            .is_err()
        {
            return Err(OgreError::FilterFailed("plugin filter output read failed"));
        }
        let out: &[f32] = bytemuck::cast_slice(&out_bytes);
        if out.len() != expected_floats {
            return Err(OgreError::FilterFailed(
                "plugin filter returned unexpected output length",
            ));
        }
        tile.copy_from_slice(out);
        Ok(())
    }

    /// Return a compiled module for this filter's WASM, using the process-wide
    /// cache so repeated applications (and separate clones of the filter) share
    /// the compilation result.
    fn compiled_module(&self) -> Option<Arc<Module>> {
        {
            let cache = module_cache().lock().ok()?;
            if let Some(m) = cache.get(&self.wasm) {
                return Some(Arc::clone(m));
            }
        }

        let module = Module::new(plugin_engine(), &self.wasm).ok()?;
        let module = Arc::new(module);

        let mut cache = module_cache().lock().ok()?;
        if cache.len() >= MAX_CACHED_MODULES {
            // Simple eviction: clear half the cache when full.
            let keys: Vec<Vec<u8>> = cache.keys().take(cache.len() / 2).cloned().collect();
            for k in keys {
                cache.remove(&k);
            }
        }
        cache.insert(self.wasm.clone(), Arc::clone(&module));
        Some(module)
    }
}

impl Filter for PluginFilter {
    fn label(&self) -> &'static str {
        // Interned once in `new`; returning the stored `&'static str` avoids
        // leaking a fresh allocation on every call (this runs per history/menu
        // render).
        self.label
    }

    fn wgsl(&self) -> &str {
        // Plugins run on the CPU, so the GPU path uses a no-op passthrough.
        include_str!("shaders/plugin_passthrough.wgsl")
    }

    fn params(&self) -> ParamsBuffer {
        // A single dummy 16-byte uniform so the compute pipeline has a valid
        // binding even though the passthrough shader ignores it.
        ParamsBuffer::new(vec![0u8; 16])
    }

    fn apply_to_tiled_buffer(&self, buffer: &TiledBuffer) -> ogre_core::Result<TiledBuffer> {
        let bounds = buffer.exact_bounds().unwrap_or(Rect::new(0, 0, 0, 0));
        if bounds.is_empty() {
            return Ok(TiledBuffer::new());
        }

        let coords: Vec<TileCoord> = bounds.tiles_covered().collect();
        let tile_count = coords.len() as u64;
        let mut remaining_fuel = self.fuel;

        let mut out = TiledBuffer::new();
        for (idx, coord) in coords.into_iter().enumerate() {
            let tile_rect = tile_rect(coord).ok_or(OgreError::FilterFailed(
                "tile coordinate overflow during plugin filter",
            ))?;
            let Some(inter) = tile_rect.intersect(bounds) else {
                continue;
            };

            let tiles_left = (tile_count - idx as u64).max(1);
            let fuel_per_tile = if remaining_fuel == 0 {
                0
            } else {
                (remaining_fuel / tiles_left).max(1)
            };

            let mut pixels: Vec<Rgba32F> =
                Vec::with_capacity((inter.w as usize) * (inter.h as usize));
            for dy in 0..inter.h {
                let y = inter.y + dy as i32;
                for dx in 0..inter.w {
                    let x = inter.x + dx as i32;
                    pixels.push(buffer.get_pixel(IVec2::new(x, y)));
                }
            }

            self.apply_cpu_with_fuel(&mut pixels, inter.w, fuel_per_tile)?;
            remaining_fuel = remaining_fuel.saturating_sub(fuel_per_tile);

            let mut iter = pixels.iter();
            for dy in 0..inter.h {
                let y = inter.y + dy as i32;
                for dx in 0..inter.w {
                    let x = inter.x + dx as i32;
                    out.set_pixel(IVec2::new(x, y), *iter.next().unwrap());
                }
            }
        }
        Ok(out)
    }

    fn apply_cpu(&self, pixels: &mut [Rgba32F], width: u32) {
        // The public `Filter::apply_cpu` contract is best-effort: a per-tile
        // failure leaves the input unchanged rather than surfacing an error.
        let _ = self.apply_cpu_with_fuel(pixels, width, self.fuel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Inverts the R, G and B channels of each pixel, leaving alpha alone.
    const INVERT_WAT: &str = r#"
        (module
            (memory (export "memory") 1)
            (func (export "process") (param $in i32) (param $len i32) (param $w i32) (param $h i32) (param $pp i32) (param $plen i32) (result i32)
                (local $i i32)
                (local $end i32)
                (local $off i32)
                local.get $in
                local.get $len
                i32.const 4
                i32.div_u
                i32.const 4
                i32.mul
                i32.add
                local.set $end
                i32.const 0
                local.set $i
                block $done
                    loop $loop
                        local.get $i
                        local.get $end
                        i32.ge_u
                        br_if $done
                        local.get $in
                        local.get $i
                        i32.add
                        local.tee $off
                        f32.const 1.0
                        local.get $off
                        f32.load
                        f32.sub
                        f32.store
                        local.get $off
                        i32.const 4
                        i32.add
                        local.tee $off
                        f32.const 1.0
                        local.get $off
                        f32.load
                        f32.sub
                        f32.store
                        local.get $off
                        i32.const 4
                        i32.add
                        local.tee $off
                        f32.const 1.0
                        local.get $off
                        f32.load
                        f32.sub
                        f32.store
                        local.get $i
                        i32.const 16
                        i32.add
                        local.set $i
                        br $loop
                    end
                end
                local.get $in
            )
        )
    "#;

    const INFINITE_WAT: &str = r#"
        (module
            (memory (export "memory") 1)
            (func (export "process") (param i32 i32 i32 i32 i32 i32) (result i32)
                loop $l
                    br $l
                end
                i32.const 0
            )
        )
    "#;

    #[test]
    fn plugin_invert_matches_native_invert_cpu() {
        let wasm = wat::parse_str(INVERT_WAT).expect("invert WAT compiles");
        let plugin = PluginFilter::new("Invert", wasm, Vec::new());

        let mut plugin_pixels = vec![ogre_core::Rgba32F::new(0.2, 0.4, 0.6, 1.0); 4];
        let mut native_pixels = plugin_pixels.clone();

        plugin.apply_cpu(&mut plugin_pixels, 2);
        ogre_gpu::InvertFilter::new().apply_cpu(&mut native_pixels, 2);

        assert_eq!(plugin_pixels, native_pixels);
    }

    #[test]
    fn apply_cpu_non_rectangular_slice_does_not_panic() {
        // 3 pixels with width 2 is not a whole number of rows. The host must
        // refuse the run and leave the pixels untouched rather than panicking
        // in `copy_from_slice` on a length mismatch.
        let wasm = wat::parse_str(INVERT_WAT).expect("invert WAT compiles");
        let plugin = PluginFilter::new("Invert", wasm, Vec::new());
        let original = vec![ogre_core::Rgba32F::new(0.2, 0.4, 0.6, 1.0); 3];
        let mut pixels = original.clone();
        plugin.apply_cpu(&mut pixels, 2);
        assert_eq!(pixels, original);
    }

    #[test]
    fn label_is_stable_across_calls() {
        let plugin = PluginFilter::new("MyFilter", Vec::new(), Vec::new());
        let a = plugin.label();
        let b = plugin.label();
        assert_eq!(a, "MyFilter");
        // The label must be interned once, not leaked afresh on every call.
        assert!(
            std::ptr::eq(a.as_ptr(), b.as_ptr()),
            "label() must return a stable pointer, not re-leak per call"
        );
    }

    #[test]
    fn out_of_fuel_plugin_leaves_pixels_unchanged() {
        let wasm = wat::parse_str(INFINITE_WAT).expect("infinite WAT compiles");
        let plugin = PluginFilter::new("Loop", wasm, Vec::new()).with_fuel(100);

        let original = vec![ogre_core::Rgba32F::new(0.2, 0.4, 0.6, 1.0); 4];
        let mut pixels = original.clone();
        plugin.apply_cpu(&mut pixels, 2);

        assert_eq!(pixels, original);
    }

    #[test]
    fn plugin_filter_apply_filter_cmd_inverts_and_undo_restores() {
        use ogre_core::{Command, Document, IVec2, Rgba32F};
        use ogre_gpu::ApplyFilterCmd;

        let wasm = wat::parse_str(INVERT_WAT).expect("invert WAT compiles");
        let mut doc = Document::new(10, 10);
        let id = doc.add_raster_layer("L");
        doc.layer_mut(id)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(0, 0), Rgba32F::new(0.2, 0.4, 0.6, 1.0));

        let mut cmd =
            ApplyFilterCmd::new(id, Box::new(PluginFilter::new("Invert", wasm, Vec::new())));
        cmd.apply(&mut doc).unwrap();
        let after = doc
            .layer(id)
            .unwrap()
            .buffer()
            .unwrap()
            .get_pixel(IVec2::new(0, 0));
        assert!(
            (after.r - 0.8).abs() < 1e-4
                && (after.g - 0.6).abs() < 1e-4
                && (after.b - 0.4).abs() < 1e-4
                && (after.a - 1.0).abs() < 1e-4,
            "unexpected inverted pixel {:?}",
            after
        );

        cmd.undo(&mut doc);
        let restored = doc
            .layer(id)
            .unwrap()
            .buffer()
            .unwrap()
            .get_pixel(IVec2::new(0, 0));
        assert!(
            (restored.r - 0.2).abs() < 1e-4
                && (restored.g - 0.4).abs() < 1e-4
                && (restored.b - 0.6).abs() < 1e-4
                && (restored.a - 1.0).abs() < 1e-4,
            "unexpected restored pixel {:?}",
            restored
        );
    }

    #[test]
    fn plugin_filter_apply_to_tiled_buffer_spans_multiple_tiles() {
        use ogre_core::{Document, IVec2, Rgba32F};

        let wasm = wat::parse_str(INVERT_WAT).expect("invert WAT compiles");
        let plugin = PluginFilter::new("Invert", wasm, Vec::new());

        let mut doc = Document::new(300, 300);
        let id = doc.add_raster_layer("L");
        let buffer = doc.layer_mut(id).unwrap().buffer_mut().unwrap();
        let color = Rgba32F::new(0.2, 0.4, 0.6, 1.0);
        for y in 0..300 {
            for x in 0..300 {
                buffer.set_pixel(IVec2::new(x, y), color);
            }
        }

        // Apply directly through the overridden tile-at-a-time path.
        let filtered = plugin.apply_to_tiled_buffer(buffer).unwrap();
        let tiles = filtered.occupied_tiles();
        assert!(
            tiles.len() > 1,
            "a 300x300 layer must produce more than one occupied tile, got {}",
            tiles.len()
        );

        let expected = Rgba32F::new(0.8, 0.6, 0.4, 1.0);
        for &(x, y) in &[
            (0, 0),
            (255, 0),
            (0, 255),
            (255, 255),
            (256, 256),
            (299, 299),
        ] {
            let actual = filtered.get_pixel(IVec2::new(x, y));
            assert!(
                (actual.r - expected.r).abs() < 1e-4
                    && (actual.g - expected.g).abs() < 1e-4
                    && (actual.b - expected.b).abs() < 1e-4
                    && (actual.a - expected.a).abs() < 1e-4,
                "pixel at ({}, {}) mismatch: got {:?}, expected {:?}",
                x,
                y,
                actual,
                expected
            );
        }
    }

    #[test]
    fn plugin_filter_apply_to_tiled_buffer_treats_missing_tiles_as_transparent() {
        use ogre_core::{Document, IVec2, Rgba32F};

        let wasm = wat::parse_str(INVERT_WAT).expect("invert WAT compiles");
        let plugin = PluginFilter::new("Invert", wasm, Vec::new());

        let mut doc = Document::new(300, 300);
        let id = doc.add_raster_layer("L");
        let buffer = doc.layer_mut(id).unwrap().buffer_mut().unwrap();
        let color = Rgba32F::new(0.2, 0.4, 0.6, 1.0);
        // Two pixels in diagonally adjacent tiles; the other two tiles in the
        // 2×2 tile rectangle covered by exact_bounds are empty.
        buffer.set_pixel(IVec2::new(255, 255), color);
        buffer.set_pixel(IVec2::new(256, 256), color);

        let original_bounds = buffer.exact_bounds().unwrap();
        let filtered = plugin.apply_to_tiled_buffer(buffer).unwrap();
        assert_eq!(
            filtered.exact_bounds().unwrap(),
            original_bounds,
            "output extent must match the default dense-path extent"
        );

        let expected = Rgba32F::new(0.8, 0.6, 0.4, 1.0);
        for &(x, y) in &[(255, 255), (256, 256)] {
            let actual = filtered.get_pixel(IVec2::new(x, y));
            assert!(
                (actual.r - expected.r).abs() < 1e-4
                    && (actual.g - expected.g).abs() < 1e-4
                    && (actual.b - expected.b).abs() < 1e-4
                    && (actual.a - expected.a).abs() < 1e-4,
                "pixel at ({}, {}) not inverted: got {:?}",
                x,
                y,
                actual
            );
        }

        // Pixels in the two empty tiles were fed transparent input to the
        // plugin. With the invert fixture that becomes white with zero alpha,
        // proving the missing tiles were processed rather than skipped.
        let inverted_transparent = Rgba32F::new(1.0, 1.0, 1.0, 0.0);
        assert_eq!(
            filtered.get_pixel(IVec2::new(256, 255)),
            inverted_transparent
        );
        assert_eq!(
            filtered.get_pixel(IVec2::new(255, 256)),
            inverted_transparent
        );
    }
}
