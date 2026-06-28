// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Tile texture upload and caching.
//!
//! Tiles from [`ogre_core`] live on the GPU as `Rgba32Float` textures. The
//! cache uses the identity of the source [`Arc<Tile>`][Arc] to decide whether
//! a tile needs re-uploading. Under `ogre-core`'s single-mutation-path rule,
//! every edit is pushed as a `Command` that snapshots the affected tiles
//! before mutating them; the snapshot keeps the `Arc` shared, so the clone-on-
//! write step produces a new `Arc` pointer exactly when pixel contents change.
//!
//! [Arc]: std::sync::Arc

use std::cell::Cell;
use std::sync::Arc;
use std::time::Instant;

use ahash::{AHashMap, AHashSet};
use ogre_core::{Layer, LayerId, Rgba32F, Tile, TileCoord, TiledBuffer, TILE_SIZE};

use crate::context::GpuContext;

/// Number of bytes in one `Rgba32Float` texel.
const BYTES_PER_PIXEL: u32 = 16;

/// Number of bytes in one full `TILE_SIZE × TILE_SIZE` tile.
const TILE_BYTES: usize = TILE_SIZE * TILE_SIZE * BYTES_PER_PIXEL as usize;

/// Tiles per staging chunk when batch-uploading. Bounds the size of each
/// transient staging buffer (`UPLOAD_CHUNK_TILES × TILE_BYTES`).
const UPLOAD_CHUNK_TILES: usize = 64;

/// Upload a single [`Tile`] to a fresh `Rgba32Float` GPU texture.
///
/// The returned texture has usage `TEXTURE_BINDING | COPY_DST | COPY_SRC` and
/// dimensions `TILE_SIZE × TILE_SIZE`, making it suitable both as a shader
/// input and as a copy source for tests/readback.
pub fn upload_tile(ctx: &GpuContext, tile: &Tile) -> wgpu::Texture {
    let size = wgpu::Extent3d {
        width: TILE_SIZE as u32,
        height: TILE_SIZE as u32,
        depth_or_array_layers: 1,
    };

    let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("ogre tile"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba32Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });

    ctx.queue.write_texture(
        texture.as_image_copy(),
        bytemuck::cast_slice(tile.as_slice()),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(TILE_SIZE as u32 * BYTES_PER_PIXEL),
            rows_per_image: Some(TILE_SIZE as u32),
        },
        size,
    );

    texture
}

/// Allocate a fresh tile-sized `Rgba32Float` texture (no data uploaded).
///
/// Same descriptor as [`upload_tile`], used as the destination for batched
/// staging-buffer uploads.
fn create_tile_texture(ctx: &GpuContext) -> wgpu::Texture {
    ctx.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("ogre tile"),
        size: wgpu::Extent3d {
            width: TILE_SIZE as u32,
            height: TILE_SIZE as u32,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba32Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

/// Map the reusable staging buffer for writing and block until it is ready.
///
/// Uses the same `map_async` + `device.poll(Wait)` handshake as the readback
/// path. After a chunk's `copy_buffer_to_texture` submit, the buffer is in use
/// by the GPU; mapping it for the next chunk waits for that copy to retire,
/// which is near-instant since the copies are tiny.
fn map_staging_for_write(ctx: &GpuContext, staging: &wgpu::Buffer, len: usize) {
    let (tx, rx) = std::sync::mpsc::channel();
    staging
        .slice(0..len as u64)
        .map_async(wgpu::MapMode::Write, move |result| {
            let _ = tx.send(result);
        });
    ctx.device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        })
        .expect("device poll failed");
    rx.recv()
        .expect("map_async callback dropped")
        .expect("failed to map tile upload staging buffer");
}

impl TileTextureCache {
    /// Upload many tiles to fresh GPU textures using batched staging copies,
    /// returning the textures in the same order as `tiles`.
    ///
    /// Per-tile [`wgpu::Queue::write_texture`] carries a fixed cost per call
    /// (the staging belt must place and schedule each write); for the thousands
    /// of tiles a large cold composite touches, that overhead dominates the
    /// upload. Instead this packs each chunk of tiles into a reused staging
    /// buffer (a single contiguous copy per tile into write-combining memory)
    /// and records one `copy_buffer_to_texture` per tile, submitting once per
    /// chunk. The GPU copy itself is near-free.
    ///
    /// The staging buffer is allocated once and reused (mapped for write each
    /// chunk) so its memory is zero-initialized a single time rather than
    /// re-allocated and re-zeroed for every chunk — that re-zeroing was, for a
    /// large cold composite, as expensive as the upload copy itself. The
    /// remaining cost is the host→device copy, bound by the PCIe link rather
    /// than CPU throughput; measured attempts to parallelize it across cores
    /// regressed (the driver serializes allocation and a single link
    /// saturates), so chunks are filled serially.
    fn upload_tiles_batched(
        &mut self,
        ctx: &GpuContext,
        tiles: &[Arc<Tile>],
    ) -> Vec<wgpu::Texture> {
        let textures: Vec<wgpu::Texture> = tiles.iter().map(|_| create_tile_texture(ctx)).collect();

        let staging = self.upload_staging.get_or_insert_with(|| {
            ctx.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("ogre tile upload staging"),
                size: (UPLOAD_CHUNK_TILES * TILE_BYTES) as u64,
                usage: wgpu::BufferUsages::MAP_WRITE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            })
        });

        for chunk_start in (0..tiles.len()).step_by(UPLOAD_CHUNK_TILES) {
            let chunk_end = (chunk_start + UPLOAD_CHUNK_TILES).min(tiles.len());
            let count = chunk_end - chunk_start;
            let bytes = count * TILE_BYTES;

            map_staging_for_write(ctx, staging, bytes);
            {
                let mut view = staging.get_mapped_range_mut(0..bytes as u64);
                for i in 0..count {
                    let src: &[u8] = bytemuck::cast_slice(tiles[chunk_start + i].as_slice());
                    view.slice(i * TILE_BYTES..(i + 1) * TILE_BYTES)
                        .copy_from_slice(src);
                }
            }
            staging.unmap();

            let mut encoder = ctx
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("ogre tile upload"),
                });
            for i in 0..count {
                encoder.copy_buffer_to_texture(
                    wgpu::TexelCopyBufferInfo {
                        buffer: staging,
                        layout: wgpu::TexelCopyBufferLayout {
                            offset: (i * TILE_BYTES) as u64,
                            bytes_per_row: Some(TILE_SIZE as u32 * BYTES_PER_PIXEL),
                            rows_per_image: Some(TILE_SIZE as u32),
                        },
                    },
                    textures[chunk_start + i].as_image_copy(),
                    wgpu::Extent3d {
                        width: TILE_SIZE as u32,
                        height: TILE_SIZE as u32,
                        depth_or_array_layers: 1,
                    },
                );
            }
            ctx.queue.submit(Some(encoder.finish()));
        }

        textures
    }
}

/// A GPU-resident copy of a single source tile, keyed by identity of the
/// source [`Arc<Tile>`][Arc].
///
/// The cache stores a clone of the source `Arc<Tile>` and compares it with
/// [`Arc::ptr_eq`]. Holding the `Arc` keeps the source allocation alive while
/// the tile is cached, so identity comparisons remain sound even if the
/// document evicts or replaces the tile.
///
/// This identity scheme relies on `ogre-core`'s single-mutation-path rule:
/// mutating code must push a `Command` that snapshots the affected tiles
/// first, keeping the `Arc` shared before `Arc::make_mut` clones it. Direct
/// in-place mutation of a uniquely-owned tile bypasses detection.
///
/// [Arc]: std::sync::Arc
struct CachedTile {
    /// The underlying texture. Kept alive so that [`view`](Self::view) remains
    /// valid, even though it is never read directly.
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    src: Arc<Tile>,
    /// Last time this cached tile was uploaded or accessed.
    last_used: Cell<Instant>,
}

/// A memory budget limiting the number of GPU-resident tiles.
///
/// A `max_gpu_tiles` of `0` means unlimited. The budget is enforced after each
/// `sync_*` call by evicting the least-recently-used tiles.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheBudget {
    /// Maximum number of tiles that may stay in GPU memory at once.
    ///
    /// `0` disables the limit.
    pub max_gpu_tiles: usize,
}

impl CacheBudget {
    /// No GPU tile limit.
    pub fn unlimited() -> Self {
        Self::default()
    }

    /// Limit GPU memory to at most `max` tiles.
    pub fn new(max: usize) -> Self {
        Self { max_gpu_tiles: max }
    }

    /// Returns `true` if no limit is configured.
    pub fn is_unlimited(&self) -> bool {
        self.max_gpu_tiles == 0
    }
}

/// Cache of GPU textures for the tiles currently occupied by [`Layer`]s.
///
/// The cache is updated by calling [`sync_layer`](Self::sync_layer), which
/// uploads only tiles whose source `Arc<Tile>` pointer has changed since the
/// last sync, and evicts entries for tiles that are no longer occupied.
pub struct TileTextureCache {
    entries: AHashMap<(LayerId, TileCoord), CachedTile>,
    upload_count: u64,
    budget: CacheBudget,
    /// Reusable mapped staging buffer for batched tile uploads, allocated on
    /// first use. Reused across uploads so its memory is zero-initialized once
    /// rather than re-allocated (and re-zeroed) for every batch.
    upload_staging: Option<wgpu::Buffer>,
}

impl TileTextureCache {
    /// Create an empty cache with no GPU tile limit.
    pub fn new() -> Self {
        Self {
            entries: AHashMap::new(),
            upload_count: 0,
            budget: CacheBudget::unlimited(),
            upload_staging: None,
        }
    }

    /// Create an empty cache with the given GPU tile budget.
    pub fn with_budget(budget: CacheBudget) -> Self {
        Self {
            entries: AHashMap::new(),
            upload_count: 0,
            budget,
            upload_staging: None,
        }
    }

    /// Return the current GPU tile budget.
    pub fn budget(&self) -> CacheBudget {
        self.budget
    }

    /// Change the GPU tile budget and evict tiles if necessary.
    pub fn set_budget(&mut self, budget: CacheBudget) {
        self.budget = budget;
        self.enforce_budget();
    }

    /// Evict least-recently-used tiles until the cache fits within `budget`.
    ///
    /// Call this after all tiles needed for the current operation are cached so
    /// that the budget does not evict tiles that are about to be read.
    pub fn enforce_budget(&mut self) {
        if self.budget.is_unlimited() || self.entries.len() <= self.budget.max_gpu_tiles {
            return;
        }
        let to_evict = self.entries.len() - self.budget.max_gpu_tiles;
        let mut by_age: Vec<((LayerId, TileCoord), Instant)> = self
            .entries
            .iter()
            .map(|(&key, entry)| (key, entry.last_used.get()))
            .collect();
        by_age.sort_by_key(|(_, t)| *t);
        for (key, _) in by_age.into_iter().take(to_evict) {
            self.entries.remove(&key);
        }
    }

    /// Number of GPU uploads performed by this cache since creation.
    ///
    /// This is test instrumentation; it is not part of the public runtime API.
    pub fn upload_count(&self) -> u64 {
        self.upload_count
    }

    /// Number of tiles currently cached in GPU memory.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no tiles are cached.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Synchronize the cache with the occupied tiles of `layer`.
    ///
    /// For each occupied tile, if there is no cached entry or the cached entry
    /// was uploaded from a different `Arc<Tile>` pointer, a fresh GPU texture
    /// is created and uploaded. Tiles that were previously cached for this
    /// layer but are no longer occupied are evicted.
    ///
    /// The configured GPU tile budget is **not** enforced by this call; call
    /// [`enforce_budget`](Self::enforce_budget) after all needed tiles for the
    /// current operation are cached.
    ///
    /// Group layers own no tiles, so syncing one evicts any cached textures
    /// that might have belonged to a previous raster layer with the same id.
    pub fn sync_layer(&mut self, ctx: &GpuContext, layer: &Layer) {
        let layer_id = layer.id;

        let Some(buffer) = layer.buffer() else {
            self.clear_layer(layer_id);
            return;
        };

        self.sync_buffer(ctx, layer_id, buffer);
    }

    /// Synchronize the cache with the occupied tiles of `layer`'s mask.
    ///
    /// If the layer has no mask, every cached mask texture for this layer is
    /// evicted. The configured GPU tile budget is **not** enforced by this
    /// call; call [`enforce_budget`](Self::enforce_budget) after all needed
    /// tiles for the current operation are cached.
    pub fn sync_layer_mask(&mut self, ctx: &GpuContext, layer: &Layer) {
        let layer_id = layer.id;

        let Some(mask) = layer.mask() else {
            self.clear_layer(layer_id);
            return;
        };

        self.sync_buffer(ctx, layer_id, mask);
    }

    /// Synchronize the cache with the occupied tiles of a generic buffer.
    ///
    /// This is the internal primitive used by [`sync_layer`](Self::sync_layer)
    /// and [`sync_layer_mask`](Self::sync_layer_mask), and by the GPU
    /// compositor's vector-layer rasterization cache.
    ///
    /// Budget enforcement is deliberately deferred: call
    /// [`enforce_budget`](Self::enforce_budget) after all tiles needed for the
    /// current frame/composite are cached. This prevents tiles from being
    /// evicted between upload and use.
    pub(crate) fn sync_buffer(
        &mut self,
        ctx: &GpuContext,
        layer_id: LayerId,
        buffer: &TiledBuffer,
    ) {
        let mut current_coords = AHashSet::new();

        // First pass: record which occupied tiles are present and which need a
        // (re-)upload, without touching the GPU. This lets the uploads be
        // batched into a few staging copies instead of one write per tile.
        let mut upload_coords: Vec<TileCoord> = Vec::new();
        let mut upload_tiles: Vec<Arc<Tile>> = Vec::new();
        for (coord, tile) in buffer.occupied_tiles() {
            current_coords.insert(coord);

            let needs_upload = match self.entries.get(&(layer_id, coord)) {
                Some(entry) => !Arc::ptr_eq(&entry.src, &tile),
                None => true,
            };
            if needs_upload {
                upload_coords.push(coord);
                upload_tiles.push(tile);
            }
        }

        if !upload_tiles.is_empty() {
            let textures = self.upload_tiles_batched(ctx, &upload_tiles);
            for (i, texture) in textures.into_iter().enumerate() {
                let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                self.entries.insert(
                    (layer_id, upload_coords[i]),
                    CachedTile {
                        texture,
                        view,
                        src: Arc::clone(&upload_tiles[i]),
                        last_used: Cell::new(Instant::now()),
                    },
                );
                self.upload_count += 1;
            }
        }

        self.entries
            .retain(|(id, coord), _| *id != layer_id || current_coords.contains(coord));
    }

    /// Drop every cached texture belonging to `layer_id`.
    pub fn clear_layer(&mut self, layer_id: LayerId) {
        self.entries.retain(|(id, _), _| *id != layer_id);
    }

    /// Drop every cached texture.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Return true if the cache holds any tile for `layer_id`.
    #[cfg(test)]
    pub fn contains_layer(&self, layer_id: LayerId) -> bool {
        self.entries.keys().any(|(id, _)| *id == layer_id)
    }

    /// Drop every cached texture whose layer id is not in `ids`.
    ///
    /// This must be called after layers are removed from the document so their
    /// GPU textures are not leaked.
    pub fn retain_layers(&mut self, ids: &AHashSet<LayerId>) {
        self.entries.retain(|(id, _), _| ids.contains(id));
    }

    /// Return the cached GPU view for a `(layer, tile)` pair, if any.
    ///
    /// Accessing a view counts as a use for LRU eviction purposes.
    pub fn view(&self, key: (LayerId, TileCoord)) -> Option<&wgpu::TextureView> {
        self.entries.get(&key).map(|entry| {
            entry.last_used.set(Instant::now());
            &entry.view
        })
    }

    /// Return the cached GPU texture for a `(layer, tile)` pair, if any.
    ///
    /// Accessing a texture counts as a use for LRU eviction purposes.
    pub fn texture(&self, key: (LayerId, TileCoord)) -> Option<&wgpu::Texture> {
        self.entries.get(&key).map(|entry| {
            entry.last_used.set(Instant::now());
            &entry.texture
        })
    }
}

impl Default for TileTextureCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Create a tile-sized `Rgba32Float` texture usable as a blend accumulator.
///
/// The texture has `STORAGE_BINDING | TEXTURE_BINDING | COPY_DST | COPY_SRC`
/// usage so it can be used both as a compute destination and as a source when
/// a group result is blended over the layer below it.
pub fn create_storage_texture(ctx: &GpuContext, label: &str) -> wgpu::Texture {
    let size = wgpu::Extent3d {
        width: TILE_SIZE as u32,
        height: TILE_SIZE as u32,
        depth_or_array_layers: 1,
    };
    ctx.device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba32Float,
        usage: wgpu::TextureUsages::STORAGE_BINDING
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

/// Fill a tile-sized storage texture with fully transparent pixels.
pub fn clear_storage_texture(queue: &wgpu::Queue, texture: &wgpu::Texture) {
    let clear = Tile::transparent();
    queue.write_texture(
        texture.as_image_copy(),
        bytemuck::cast_slice(clear.as_slice()),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(TILE_SIZE as u32 * BYTES_PER_PIXEL),
            rows_per_image: Some(TILE_SIZE as u32),
        },
        texture.size(),
    );
}

/// Copy the contents of an `Rgba32Float` texture into a `Vec<Rgba32F>`.
///
/// This is used by the golden-test harness and by the viewport readback path.
/// It is not optimized for performance.
pub fn read_texture_to_vec(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
) -> Vec<Rgba32F> {
    let size = texture.size();
    let width = size.width as usize;
    let height = size.height as usize;
    let bytes_per_pixel = BYTES_PER_PIXEL as usize;
    let unpadded_bytes_per_row = width * bytes_per_pixel;
    let padded_bytes_per_row = unpadded_bytes_per_row
        .div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as usize)
        * (wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as usize);
    let buffer_size = (padded_bytes_per_row * height) as u64;

    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("tile readback"),
        size: buffer_size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("tile readback"),
    });
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_bytes_per_row as u32),
                rows_per_image: Some(height as u32),
            },
        },
        size,
    );
    queue.submit(Some(encoder.finish()));
    device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        })
        .expect("device poll failed");

    let (tx, rx) = std::sync::mpsc::channel();
    buffer.map_async(wgpu::MapMode::Read, .., move |result| {
        let _ = tx.send(result);
    });
    device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        })
        .expect("device poll failed");
    rx.recv()
        .expect("map_async callback dropped")
        .expect("failed to map readback buffer");

    let data = buffer.get_mapped_range(..);
    let mut out = Vec::with_capacity(width * height);
    for row in 0..height {
        let start = row * padded_bytes_per_row;
        let row_bytes = &data[start..start + unpadded_bytes_per_row];
        out.extend_from_slice(bytemuck::cast_slice(row_bytes));
    }
    drop(data);
    buffer.unmap();

    out
}

/// Copy the contents of an `Rgba8Unorm` texture into a `Vec<Rgba32F>`.
///
/// Each byte is divided by 255.0 to produce linear values. This is used by
/// the vector golden-test harness to read back a `vello` render target.
pub fn read_rgba8_texture_to_vec(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
) -> Vec<Rgba32F> {
    let size = texture.size();
    let width = size.width as usize;
    let height = size.height as usize;
    let bytes_per_pixel = 4usize;
    let unpadded_bytes_per_row = width * bytes_per_pixel;
    let padded_bytes_per_row = unpadded_bytes_per_row
        .div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as usize)
        * (wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as usize);
    let buffer_size = (padded_bytes_per_row * height) as u64;

    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("rgba8 readback"),
        size: buffer_size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("rgba8 readback"),
    });
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_bytes_per_row as u32),
                rows_per_image: Some(height as u32),
            },
        },
        size,
    );
    queue.submit(Some(encoder.finish()));
    device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        })
        .expect("device poll failed");

    let (tx, rx) = std::sync::mpsc::channel();
    buffer.map_async(wgpu::MapMode::Read, .., move |result| {
        let _ = tx.send(result);
    });
    device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        })
        .expect("device poll failed");
    rx.recv()
        .expect("map_async callback dropped")
        .expect("failed to map readback buffer");

    let data = buffer.get_mapped_range(..);
    let mut out = Vec::with_capacity(width * height);
    for row in 0..height {
        let start = row * padded_bytes_per_row;
        let row_bytes = &data[start..start + unpadded_bytes_per_row];
        for chunk in row_bytes.chunks_exact(4) {
            let scale = |b: u8| b as f32 / 255.0;
            out.push(Rgba32F::new(
                scale(chunk[0]),
                scale(chunk[1]),
                scale(chunk[2]),
                scale(chunk[3]),
            ));
        }
    }
    drop(data);
    buffer.unmap();

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient_tile() -> Tile {
        let mut tile = Tile::transparent();
        let n = TILE_SIZE as f32;
        for y in 0..TILE_SIZE {
            for x in 0..TILE_SIZE {
                tile.set(x, y, Rgba32F::new(x as f32 / n, y as f32 / n, 0.5, 1.0));
            }
        }
        tile
    }

    fn layer_with_pixel(name: &str, p: ogre_core::IVec2, px: Rgba32F) -> Layer {
        let mut layer = Layer::new_raster(name);
        layer.buffer_mut().unwrap().set_pixel(p, px);
        layer
    }

    #[test]
    fn upload_tile_round_trip_is_bit_identical() {
        let ctx = GpuContext::headless();
        let tile = gradient_tile();

        let texture = upload_tile(&ctx, &tile);
        let readback = read_texture_to_vec(&ctx.device, &ctx.queue, &texture);

        assert_eq!(readback, tile.as_slice().to_vec());
    }

    #[test]
    fn cache_skips_reupload_for_unchanged_tile() {
        let ctx = GpuContext::headless();
        let mut cache = TileTextureCache::new();
        let layer = layer_with_pixel(
            "paint",
            ogre_core::IVec2::new(0, 0),
            Rgba32F::new(1.0, 0.0, 0.0, 1.0),
        );

        cache.sync_layer(&ctx, &layer);
        assert_eq!(cache.upload_count(), 1);

        // Syncing the same layer again must not re-upload anything.
        cache.sync_layer(&ctx, &layer);
        assert_eq!(cache.upload_count(), 1);

        // Cloning the layer then editing a tile triggers COW and therefore
        // changes the Arc pointer. Only the modified tile must be re-uploaded.
        let mut layer2 = layer.clone();
        layer2.buffer_mut().unwrap().set_pixel(
            ogre_core::IVec2::new(0, 0),
            Rgba32F::new(0.0, 1.0, 0.0, 1.0),
        );
        cache.sync_layer(&ctx, &layer2);
        assert_eq!(cache.upload_count(), 2);
    }

    #[test]
    fn cache_only_invalidates_changed_tile() {
        let ctx = GpuContext::headless();
        let mut cache = TileTextureCache::new();
        let mut layer = Layer::new_raster("paint");
        layer.buffer_mut().unwrap().set_pixel(
            ogre_core::IVec2::new(0, 0),
            Rgba32F::new(1.0, 0.0, 0.0, 1.0),
        );
        layer.buffer_mut().unwrap().set_pixel(
            ogre_core::IVec2::new(300, 0),
            Rgba32F::new(0.0, 0.0, 1.0, 1.0),
        );

        cache.sync_layer(&ctx, &layer);
        assert_eq!(cache.upload_count(), 2);

        // Clone then edit only the tile at (0, 0). COW gives it a new pointer,
        // so exactly one tile is re-uploaded.
        let mut layer2 = layer.clone();
        layer2.buffer_mut().unwrap().set_pixel(
            ogre_core::IVec2::new(0, 0),
            Rgba32F::new(0.0, 1.0, 0.0, 1.0),
        );
        cache.sync_layer(&ctx, &layer2);
        assert_eq!(cache.upload_count(), 3);

        // The untouched tile at (1, 0) must still be cached.
        assert!(cache.view((layer.id, TileCoord { x: 1, y: 0 })).is_some());
    }

    #[test]
    fn cache_evicts_tiles_removed_from_layer() {
        let ctx = GpuContext::headless();
        let mut cache = TileTextureCache::new();
        let mut layer = layer_with_pixel(
            "paint",
            ogre_core::IVec2::new(0, 0),
            Rgba32F::new(1.0, 0.0, 0.0, 1.0),
        );

        cache.sync_layer(&ctx, &layer);
        let key = (layer.id, TileCoord { x: 0, y: 0 });
        assert!(cache.view(key).is_some());

        // Clear the pixel so the buffer prunes the tile.
        layer
            .buffer_mut()
            .unwrap()
            .set_pixel(ogre_core::IVec2::new(0, 0), Rgba32F::TRANSPARENT);
        cache.sync_layer(&ctx, &layer);
        assert!(cache.view(key).is_none());
    }

    #[test]
    fn cache_clear_layer_drops_all_textures_for_layer() {
        let ctx = GpuContext::headless();
        let mut cache = TileTextureCache::new();
        let mut layer = Layer::new_raster("paint");
        layer.buffer_mut().unwrap().set_pixel(
            ogre_core::IVec2::new(0, 0),
            Rgba32F::new(1.0, 0.0, 0.0, 1.0),
        );
        layer.buffer_mut().unwrap().set_pixel(
            ogre_core::IVec2::new(300, 0),
            Rgba32F::new(0.0, 0.0, 1.0, 1.0),
        );

        cache.sync_layer(&ctx, &layer);
        assert_eq!(cache.entries.len(), 2);

        cache.clear_layer(layer.id);
        assert!(cache.view((layer.id, TileCoord { x: 0, y: 0 })).is_none());
        assert!(cache.view((layer.id, TileCoord { x: 1, y: 0 })).is_none());
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn cache_retain_layers_drops_textures_for_removed_layers() {
        let ctx = GpuContext::headless();
        let mut cache = TileTextureCache::new();
        let mut doc = ogre_core::Document::new(64, 64);

        let id_a = doc.add_raster_layer("a");
        doc.layer_mut(id_a)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(
                ogre_core::IVec2::new(0, 0),
                Rgba32F::new(1.0, 0.0, 0.0, 1.0),
            );
        let id_b = doc.add_raster_layer("b");
        doc.layer_mut(id_b)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(
                ogre_core::IVec2::new(0, 0),
                Rgba32F::new(0.0, 1.0, 0.0, 1.0),
            );

        cache.sync_layer(&ctx, doc.layer(id_a).unwrap());
        cache.sync_layer(&ctx, doc.layer(id_b).unwrap());
        assert_eq!(cache.entries.len(), 2);

        let keep = AHashSet::from_iter([id_a]);
        cache.retain_layers(&keep);
        assert!(cache.view((id_a, TileCoord { x: 0, y: 0 })).is_some());
        assert!(cache.view((id_b, TileCoord { x: 0, y: 0 })).is_none());
        assert_eq!(cache.entries.len(), 1);
    }

    #[test]
    fn cache_with_budget_evicts_to_limit() {
        let ctx = GpuContext::headless();
        let mut cache = TileTextureCache::with_budget(CacheBudget::new(2));
        let mut layer = Layer::new_raster("big");
        // Touch three different tiles.
        layer.buffer_mut().unwrap().set_pixel(
            ogre_core::IVec2::new(0, 0),
            Rgba32F::new(1.0, 0.0, 0.0, 1.0),
        );
        layer.buffer_mut().unwrap().set_pixel(
            ogre_core::IVec2::new(300, 0),
            Rgba32F::new(0.0, 1.0, 0.0, 1.0),
        );
        layer.buffer_mut().unwrap().set_pixel(
            ogre_core::IVec2::new(600, 0),
            Rgba32F::new(0.0, 0.0, 1.0, 1.0),
        );

        cache.sync_layer(&ctx, &layer);
        cache.enforce_budget();
        assert_eq!(cache.upload_count(), 3);
        assert!(cache.entries.len() <= 2);
    }

    #[test]
    fn cache_reuploads_after_budget_eviction() {
        let ctx = GpuContext::headless();
        let mut cache = TileTextureCache::with_budget(CacheBudget::new(1));
        let mut layer = Layer::new_raster("big");
        layer.buffer_mut().unwrap().set_pixel(
            ogre_core::IVec2::new(0, 0),
            Rgba32F::new(1.0, 0.0, 0.0, 1.0),
        );
        layer.buffer_mut().unwrap().set_pixel(
            ogre_core::IVec2::new(300, 0),
            Rgba32F::new(0.0, 1.0, 0.0, 1.0),
        );

        cache.sync_layer(&ctx, &layer);
        assert_eq!(cache.upload_count(), 2);

        // Force the cache to drop everything, restore an unlimited budget, then
        // re-sync the same layer.
        cache.clear();
        assert!(cache.entries.is_empty());
        cache.set_budget(CacheBudget::new(10));
        cache.sync_layer(&ctx, &layer);
        assert_eq!(cache.upload_count(), 4);
        assert_eq!(cache.entries.len(), 2);
    }
}
