// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Full-document GPU compositor with dirty-tile tracking.
//!
//! The [`Compositor`] walks a document's layer stack bottom-to-top, composites
//! each output tile through the blend compute pipeline, and caches the result.
//! Only output tiles whose contributing inputs have changed are recomputed on
//! the next `composite` call.
//!
//! The output of the GPU compositor is validated against
//! [`ogre_core::compositor::composite_document`] in the golden tests; any
//! divergence larger than `1e-4` per channel is a bug.

use std::sync::Arc;

use ahash::{AHashMap, AHashSet};
use ogre_core::{
    rasterize_vector_data, AdjustmentKind, BlendMode, Document, IVec2, Layer, LayerContent,
    LayerId, OgreError, Rect, Rgba32F, Tile, TileCoord, TiledBuffer, TILE_SIZE,
};

use crate::blend::{
    AdjustmentPipelines, BatchBlendPipeline, BlendPipelines, LayerParam, MAX_BATCH_SLICES,
};
use crate::context::GpuContext;
use crate::tile_cache::{
    clear_storage_texture, create_storage_texture, read_texture_to_vec, CacheBudget,
    TileTextureCache,
};

/// A single contribution to a result tile, recorded in order so that reordering
/// invalidates the cache.
///
/// Raw `Arc<Tile>` pointers are stored as `usize` values. They are only used for
/// identity comparison; they are never dereferenced by the GPU code.
#[derive(Clone, Debug, PartialEq)]
struct Contribution {
    layer_id: LayerId,
    /// Tile coordinate of the source pixel data, or `None` for a group marker.
    tile: Option<TileCoord>,
    /// Identity of the source colour tile, if any.
    src_ptr: Option<usize>,
    /// Identity of the source mask tile, if any.
    mask_ptr: Option<usize>,
    /// Layer offset at the time of compositing.
    offset: IVec2,
    /// Opacity stored as raw bits so the struct is `Eq`/`Hash`.
    opacity_bits: u32,
    /// Blend mode at the time of compositing.
    blend: BlendMode,
    /// Visibility at the time of compositing.
    visible: bool,
    /// Whether the layer had a mask at the time of compositing.
    has_mask: bool,
    /// Adjustment kind, if this contribution represents an adjustment layer.
    adjustment_kind: Option<AdjustmentKind>,
}

impl Contribution {
    /// Build a contribution for a raster tile dispatch.
    #[allow(clippy::too_many_arguments)]
    fn raster(
        layer_id: LayerId,
        tile: TileCoord,
        src: &Arc<Tile>,
        mask: Option<&Arc<Tile>>,
        offset: IVec2,
        opacity: f32,
        blend: BlendMode,
        visible: bool,
        has_mask: bool,
    ) -> Self {
        Self {
            layer_id,
            tile: Some(tile),
            src_ptr: Some(Arc::as_ptr(src) as usize),
            mask_ptr: mask.map(|m| Arc::as_ptr(m) as usize),
            offset,
            opacity_bits: opacity.to_bits(),
            blend,
            visible,
            has_mask,
            adjustment_kind: None,
        }
    }

    /// Build a contribution marker for a group layer.
    fn group(layer: &Layer, offset: IVec2) -> Self {
        Self {
            layer_id: layer.id,
            tile: None,
            src_ptr: None,
            mask_ptr: None,
            offset,
            opacity_bits: layer.opacity.to_bits(),
            blend: layer.blend,
            visible: layer.visible,
            has_mask: false,
            adjustment_kind: None,
        }
    }

    /// Build a contribution marker for an adjustment layer.
    ///
    /// Adjustment layers are spatially global and ignore their blend mode, so
    /// `offset` and `blend` are fixed to avoid over-invalidating cached tiles
    /// when those fields change.
    fn adjustment(layer: &Layer, kind: AdjustmentKind, opacity: f32) -> Self {
        Self {
            layer_id: layer.id,
            tile: None,
            src_ptr: None,
            mask_ptr: None,
            offset: IVec2::ZERO,
            opacity_bits: opacity.to_bits(),
            blend: BlendMode::Normal,
            visible: layer.visible,
            has_mask: false,
            adjustment_kind: Some(kind),
        }
    }
}

/// A cached output tile and the signature it was built from.
struct ResultTile {
    texture: wgpu::Texture,
    signature: Vec<Contribution>,
    /// Last time this tile was requested, for LRU eviction.
    last_used: std::time::Instant,
}

/// Cache of composited output tiles.
struct ResultTileCache {
    entries: AHashMap<TileCoord, ResultTile>,
    views: AHashMap<TileCoord, wgpu::TextureView>,
    budget: CacheBudget,
}

impl ResultTileCache {
    fn with_budget(budget: CacheBudget) -> Self {
        Self {
            entries: AHashMap::new(),
            views: AHashMap::new(),
            budget,
        }
    }

    /// Return the cached view map.
    fn views(&self) -> &AHashMap<TileCoord, wgpu::TextureView> {
        &self.views
    }

    /// Return the cached signature for `coord`, if any.
    fn signature(&mut self, coord: TileCoord) -> Option<&[Contribution]> {
        self.entries.get_mut(&coord).map(|e| {
            e.last_used = std::time::Instant::now();
            e.signature.as_slice()
        })
    }

    /// Return the cached result texture for `coord`, if any.
    fn texture(&self, coord: TileCoord) -> Option<&wgpu::Texture> {
        self.entries.get(&coord).map(|e| &e.texture)
    }

    /// Ensure a result tile exists for `coord`, clearing it if it is new.
    fn get_or_create(&mut self, ctx: &GpuContext, coord: TileCoord) -> &mut wgpu::Texture {
        if !self.entries.contains_key(&coord) {
            let texture = create_storage_texture(ctx, "ogre result tile");
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            self.views.insert(coord, view);
            self.entries.insert(
                coord,
                ResultTile {
                    texture,
                    signature: Vec::new(),
                    last_used: std::time::Instant::now(),
                },
            );
            clear_storage_texture(&ctx.queue, &self.entries.get(&coord).unwrap().texture);
        }
        &mut self.entries.get_mut(&coord).unwrap().texture
    }

    /// Update the signature for a cached tile.
    fn set_signature(&mut self, coord: TileCoord, signature: Vec<Contribution>) {
        if let Some(entry) = self.entries.get_mut(&coord) {
            entry.signature = signature;
        }
    }

    /// Drop every cached result tile and its view.
    fn clear(&mut self) {
        self.entries.clear();
        self.views.clear();
    }

    /// Mark tiles in `keep` as recently used and evict entries down to the
    /// configured budget using LRU order.
    ///
    /// Tiles outside `keep` are evicted first. Only if the budget is still
    /// exceeded after removing those tiles do we evict the oldest retained
    /// tiles. This keeps visible/near-visible tiles cached during small pans.
    fn retain(&mut self, keep: &AHashSet<TileCoord>) {
        let now = std::time::Instant::now();
        for coord in keep {
            if let Some(entry) = self.entries.get_mut(coord) {
                entry.last_used = now;
            }
        }

        if self.budget.is_unlimited() {
            return;
        }

        // Evict tiles that are no longer in the retained set.
        self.entries.retain(|coord, _| keep.contains(coord));

        // Then enforce the budget on retained tiles by LRU.
        while self.entries.len() > self.budget.max_gpu_tiles {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(&coord, _)| coord);
            if let Some(coord) = oldest {
                self.entries.remove(&coord);
            } else {
                break;
            }
        }
        self.views
            .retain(|coord, _| self.entries.contains_key(coord));
    }
}

/// An operation in the flattened render plan.
///
/// The plan is produced once per `composite` call from `Document::render_order`
/// and is then replayed for every output tile.
enum LayerOp {
    /// Start compositing a group. Children are composited into a temporary
    /// accumulator; the group result is blended over the parent accumulator
    /// when the matching `PopGroup` is reached.
    PushGroup {
        layer_id: LayerId,
        /// Cumulative offset applied to children inside this group.
        cumulative_offset: Option<IVec2>,
        blend: BlendMode,
        opacity: f32,
    },
    /// Blend the top group accumulator over the parent and discard it.
    PopGroup,
    /// Composite one raster layer over the current accumulator.
    Raster {
        layer_id: LayerId,
        effective_offset: Option<IVec2>,
        blend: BlendMode,
        opacity: f32,
        has_mask: bool,
    },
    /// Apply a non-destructive adjustment to the current accumulator.
    Adjustment {
        layer_id: LayerId,
        kind: AdjustmentKind,
        opacity: f32,
    },
}

/// One source-tile contribution pending in the current batched run.
///
/// Contributions accumulate while consecutive [`LayerOp::Raster`]s target the
/// same accumulator; the run is flushed (one batched dispatch per
/// [`MAX_BATCH_SLICES`] chunk) when a structural op changes the destination.
struct PendingContribution {
    layer_id: LayerId,
    src_tile: TileCoord,
    has_mask: bool,
    param: LayerParam,
}

/// Default GPU-resident tile budget for the source and mask caches.
///
/// Each tile is a 256×256 `Rgba32Float` texture (~1 MiB). A finite default
/// prevents long sessions on large documents from exhausting VRAM and forcing
/// the driver into swap/thrashing. Callers can override via
/// `Compositor::with_result_budget`.
pub const DEFAULT_GPU_TILE_BUDGET: usize = 1024;

/// Default GPU-resident tile budget for the result cache.
///
/// The result cache is sized separately from the source/mask caches so that
/// pan/zoom responsiveness can be tuned independently from layer data caching.
pub const DEFAULT_GPU_RESULT_TILE_BUDGET: usize = 512;

/// Number of tiles to retain beyond the visible region in each direction.
///
/// A margin of 2 tiles (~512 px) makes small pans cheap without bloating VRAM.
const RESULT_CACHE_MARGIN_TILES: i32 = 2;

/// Maximum nesting depth of layer groups the GPU compositor will render.
///
/// Each nesting level needs a temporary accumulator texture in
/// [`Compositor::group_pool`]; this constant bounds that pool so a malicious or
/// accidentally deeply nested document cannot allocate unbounded GPU memory.
/// The limit is kept well below the `u8` depth counter used by the document's
/// render order so that depth saturation cannot mask the cap.
const MAX_GROUP_DEPTH: usize = 128;

/// GPU compositor for an entire Arte Ogre document.
///
/// See the [module-level documentation](self) for usage notes.
pub struct Compositor {
    /// Colour tile cache.
    tiles: TileTextureCache,
    /// Mask tile cache.
    masks: TileTextureCache,
    /// Cached composited output tiles.
    result: ResultTileCache,
    /// Shared single-layer blend pipeline (used for group-result blends).
    pipelines: BlendPipelines,
    /// Multi-layer batched blend pipeline (used for raster runs).
    batch: BatchBlendPipeline,
    /// Shared adjustment pipeline.
    adjustment: AdjustmentPipelines,
    /// Reusable temporary accumulators for group isolation, indexed by stack
    /// depth.
    group_pool: Vec<Option<wgpu::Texture>>,
    /// CPU-rasterized cache for vector layers, keyed by layer id. The `u64` is
    /// the `VectorData::version` at the time the cache was built, so edits bump
    /// the version and trigger a re-rasterize + re-upload.
    vector_buffers: AHashMap<LayerId, (u64, TiledBuffer)>,
    /// Number of `composite_tile_over` dispatches issued during the last
    /// `composite` call.
    dispatch_count: u64,
    /// Output tiles that were recomputed during the last `composite` call.
    dirty_keys: AHashSet<TileCoord>,
}

impl Compositor {
    /// Create a new compositor on `ctx` with the default budgets.
    ///
    /// Source and mask caches use [`DEFAULT_GPU_TILE_BUDGET`]; the result cache
    /// uses [`DEFAULT_GPU_RESULT_TILE_BUDGET`]. Use
    /// [`Compositor::with_result_budget`] to override.
    pub fn new(ctx: &GpuContext) -> Self {
        Self::with_result_budget(
            ctx,
            CacheBudget::new(DEFAULT_GPU_TILE_BUDGET),
            CacheBudget::new(DEFAULT_GPU_RESULT_TILE_BUDGET),
        )
    }

    /// Create a new compositor on `ctx` with the given source/mask budget and
    /// the default result-cache budget.
    ///
    /// The budget applies separately to the source (colour) tile cache and the
    /// mask tile cache. The result cache uses [`DEFAULT_GPU_RESULT_TILE_BUDGET`].
    #[cfg(test)]
    pub fn with_budget(ctx: &GpuContext, budget: CacheBudget) -> Self {
        Self::with_result_budget(
            ctx,
            budget,
            CacheBudget::new(DEFAULT_GPU_RESULT_TILE_BUDGET),
        )
    }

    /// Create a compositor with the given source/mask budget and a separate
    /// result-cache budget.
    ///
    /// The source and mask caches use `source_budget`; the result cache uses
    /// `result_budget`. Source and mask default to [`DEFAULT_GPU_TILE_BUDGET`]
    /// and the result cache defaults to [`DEFAULT_GPU_RESULT_TILE_BUDGET`] when
    /// using [`Compositor::new`].
    pub fn with_result_budget(
        ctx: &GpuContext,
        source_budget: CacheBudget,
        result_budget: CacheBudget,
    ) -> Self {
        Self {
            tiles: TileTextureCache::with_budget(source_budget),
            masks: TileTextureCache::with_budget(source_budget),
            result: ResultTileCache::with_budget(result_budget),
            pipelines: BlendPipelines::new(ctx),
            batch: BatchBlendPipeline::new(ctx),
            adjustment: AdjustmentPipelines::new(ctx),
            group_pool: Vec::new(),
            vector_buffers: AHashMap::new(),
            dispatch_count: 0,
            dirty_keys: AHashSet::new(),
        }
    }

    /// Return the number of blend dispatches issued by the last `composite`
    /// call.
    pub fn dispatch_count(&self) -> u64 {
        self.dispatch_count
    }

    /// Return the set of output tiles that were recomputed during the last
    /// `composite` call.
    pub fn last_dirty_keys(&self) -> &AHashSet<TileCoord> {
        &self.dirty_keys
    }

    /// Drop all cached GPU tiles, result tiles, group accumulators, and render
    /// statistics.
    pub fn clear_caches(&mut self) {
        self.tiles.clear();
        self.masks.clear();
        self.result.clear();
        self.group_pool.clear();
        self.vector_buffers.clear();
        self.dispatch_count = 0;
        self.dirty_keys.clear();
    }

    /// Read back one cached result tile, if it exists.
    ///
    /// This is primarily a testing helper; it blocks on the GPU.
    pub fn read_result_tile(&self, ctx: &GpuContext, coord: TileCoord) -> Option<Vec<Rgba32F>> {
        self.result
            .texture(coord)
            .map(|t| read_texture_to_vec(&ctx.device, &ctx.queue, t))
    }

    /// Return the cached result texture for `coord`, if any.
    ///
    /// This is used by [`crate::canvas_renderer::CanvasRenderer`] to assemble
    /// result tiles into a single viewport-sized source texture.
    pub fn result_texture(&self, coord: TileCoord) -> Option<&wgpu::Texture> {
        self.result.texture(coord)
    }

    /// Composite `doc` over `region` and return the cached result tile views.
    ///
    /// The returned map contains one entry for every tile coordinate covered by
    /// an expanded region around `region`, clamped to the document canvas. Tiles
    /// whose contributing inputs have not changed since the previous call are
    /// served from the result cache.
    ///
    /// # Errors
    ///
    /// Returns [`OgreError::InvalidOperation`] if the document's layer tree
    /// contains a cycle.
    pub fn composite(
        &mut self,
        ctx: &GpuContext,
        doc: &Document,
        region: Rect,
    ) -> Result<&AHashMap<TileCoord, wgpu::TextureView>, OgreError> {
        let order = doc.render_order()?;
        let plan = self.build_plan(ctx, doc, &order)?;

        // Evict GPU textures for layers that no longer exist in the document.
        let layer_ids: AHashSet<LayerId> = order.iter().map(|(id, _)| *id).collect();
        self.tiles.retain_layers(&layer_ids);
        self.masks.retain_layers(&layer_ids);
        self.vector_buffers.retain(|id, _| layer_ids.contains(id));

        let expanded_region = expand_region_by_tiles(region, RESULT_CACHE_MARGIN_TILES)
            .intersect(doc.canvas)
            .unwrap_or(Rect::new(0, 0, 0, 0));
        let output_coords: AHashSet<TileCoord> = expanded_region.tiles_covered().collect();
        self.result.retain(&output_coords);

        self.dispatch_count = 0;
        self.dirty_keys.clear();

        for coord in &output_coords {
            self.sync_tile_inputs(ctx, doc, coord, &plan);
        }

        // Enforce GPU tile budgets only after all tiles needed for this
        // composite have been read. Evicting earlier could drop a tile between
        // upload and the batched blend dispatch that needs it.
        self.tiles.enforce_budget();
        self.masks.enforce_budget();

        Ok(self.result.views())
    }

    /// Build the render plan and sync tile caches for all layers.
    fn build_plan(
        &mut self,
        ctx: &GpuContext,
        doc: &Document,
        order: &[(LayerId, u8)],
    ) -> Result<Vec<LayerOp>, OgreError> {
        let mut plan = Vec::with_capacity(order.len());
        let mut open_groups: Vec<u8> = Vec::new();
        let mut max_depth_seen: usize = 0;
        let mut i = 0;
        while i < order.len() {
            // Close any groups that finish before the next layer.
            while open_groups
                .last()
                .map(|d| *d >= order[i].1)
                .unwrap_or(false)
            {
                plan.push(LayerOp::PopGroup);
                open_groups.pop();
            }

            let (id, depth) = order[i];
            let layer = match doc.layer(id) {
                Ok(l) => l,
                Err(_) => {
                    i += 1;
                    continue;
                }
            };

            if matches!(layer.content, LayerContent::Group { .. }) {
                // Groups own no tiles or masks. Evict any cached data left
                // behind when a layer id transitions from raster to group.
                self.tiles.clear_layer(id);
                self.masks.clear_layer(id);

                if !layer.visible || sanitize_opacity(layer.opacity) == 0.0 {
                    // Skip the entire subtree.
                    i += 1;
                    while i < order.len() && order[i].1 > depth {
                        i += 1;
                    }
                    continue;
                }

                let parent_offset = plan
                    .iter()
                    .rev()
                    .filter_map(|op| match op {
                        LayerOp::PushGroup {
                            cumulative_offset: Some(off),
                            ..
                        } => Some(*off),
                        _ => None,
                    })
                    .next()
                    .unwrap_or(IVec2::ZERO);

                let Some(cumulative_offset) = checked_add(parent_offset, layer.offset) else {
                    // The cumulative offset overflowed i32; treat the entire
                    // subtree as transparent, matching the CPU reference.
                    i += 1;
                    while i < order.len() && order[i].1 > depth {
                        i += 1;
                    }
                    continue;
                };

                plan.push(LayerOp::PushGroup {
                    layer_id: id,
                    cumulative_offset: Some(cumulative_offset),
                    blend: layer.blend,
                    opacity: sanitize_opacity(layer.opacity),
                });
                open_groups.push(depth);
                max_depth_seen = max_depth_seen.max(open_groups.len());
                i += 1;
                continue;
            }

            // Adjustment layer.
            if let LayerContent::Adjustment(kind) = &layer.content {
                if !layer.visible || sanitize_opacity(layer.opacity) == 0.0 {
                    i += 1;
                    continue;
                }

                plan.push(LayerOp::Adjustment {
                    layer_id: id,
                    kind: *kind,
                    opacity: sanitize_opacity(layer.opacity),
                });
                i += 1;
                continue;
            }

            // Raster or vector layer.
            if !layer.visible || sanitize_opacity(layer.opacity) == 0.0 {
                i += 1;
                continue;
            }

            let parent_offset = plan
                .iter()
                .rev()
                .filter_map(|op| match op {
                    LayerOp::PushGroup {
                        cumulative_offset: Some(off),
                        ..
                    } => Some(*off),
                    _ => None,
                })
                .next()
                .unwrap_or(IVec2::ZERO);
            let effective_offset = checked_add(parent_offset, layer.offset);

            let is_vector = matches!(layer.content, LayerContent::Vector(_));
            if is_vector {
                self.sync_vector_layer(ctx, layer);
                self.masks.clear_layer(id);
            } else {
                self.tiles.sync_layer(ctx, layer);
                if layer.mask().is_some() {
                    self.masks.sync_layer_mask(ctx, layer);
                } else {
                    self.masks.clear_layer(id);
                }
            }

            plan.push(LayerOp::Raster {
                layer_id: id,
                effective_offset,
                blend: layer.blend,
                opacity: sanitize_opacity(layer.opacity),
                has_mask: layer.mask().is_some(),
            });
            i += 1;
        }

        while !open_groups.is_empty() {
            plan.push(LayerOp::PopGroup);
            open_groups.pop();
        }

        if max_depth_seen > MAX_GROUP_DEPTH {
            return Err(OgreError::InvalidOperation(
                "layer group nesting exceeds GPU limit",
            ));
        }

        Ok(plan)
    }

    /// Ensure the CPU-rasterized cache for a vector layer is up to date, then
    /// upload its tiles to the GPU colour cache.
    fn sync_vector_layer(&mut self, ctx: &GpuContext, layer: &Layer) {
        let LayerContent::Vector(data) = &layer.content else {
            return;
        };
        let version = data.version;
        if self
            .vector_buffers
            .get(&layer.id)
            .map(|(v, _)| *v)
            .is_none_or(|v| v != version)
        {
            let buffer = rasterize_vector_data(data);
            self.vector_buffers.insert(layer.id, (version, buffer));
        }
        let (_, buffer) = self.vector_buffers.get(&layer.id).expect("just inserted");
        self.tiles.sync_buffer(ctx, layer.id, buffer);
    }

    /// Compute the signature for one output tile and, if it is dirty, composite
    /// it.
    fn sync_tile_inputs(
        &mut self,
        ctx: &GpuContext,
        doc: &Document,
        coord: &TileCoord,
        plan: &[LayerOp],
    ) {
        let new_signature = self.signature_for_tile(doc, coord, plan);
        let cached = self.result.signature(*coord);
        if cached
            .map(|s| s == new_signature.as_slice())
            .unwrap_or(false)
        {
            return;
        }

        self.dirty_keys.insert(*coord);
        let texture = self.result.get_or_create(ctx, *coord);
        clear_storage_texture(&ctx.queue, texture);

        // Stack of active group accumulators. Each entry records the pool
        // index, blend mode, and opacity needed to composite the group result.
        let mut group_stack: Vec<(usize, BlendMode, f32)> = Vec::new();
        // Consecutive raster contributions targeting the current accumulator.
        // They are flushed as a single batched dispatch (per chunk) whenever a
        // structural op changes the destination, so a flat N-layer stack costs
        // one dispatch instead of N.
        let mut run: Vec<PendingContribution> = Vec::new();

        for op in plan.iter() {
            match op {
                LayerOp::PushGroup { blend, opacity, .. } => {
                    // The pending run targets the parent accumulator; flush it
                    // before switching the destination to the new group.
                    let top = group_stack.last().map(|&(d, _, _)| d);
                    self.flush_run(ctx, coord, top, &mut run);

                    let depth = group_stack.len();
                    if self.group_pool.len() <= depth {
                        self.group_pool.push(None);
                    }
                    if self.group_pool[depth].is_none() {
                        let texture = create_storage_texture(ctx, "ogre group accumulator");
                        self.group_pool[depth] = Some(texture);
                    }
                    let group_texture = self.group_pool[depth].as_ref().unwrap();
                    clear_storage_texture(&ctx.queue, group_texture);
                    group_stack.push((depth, *blend, *opacity));
                }
                LayerOp::PopGroup => {
                    // Flush the group's own contributions into its accumulator
                    // before blending the group result over its parent.
                    let top = group_stack.last().map(|&(d, _, _)| d);
                    self.flush_run(ctx, coord, top, &mut run);

                    let (depth, blend, opacity) = group_stack
                        .pop()
                        .expect("group stack underflow in render plan");
                    let Some(group_texture) = self.group_pool[depth].as_ref() else {
                        continue;
                    };
                    let dst = if let Some(&(parent_depth, _, _)) = group_stack.last() {
                        self.group_pool[parent_depth].as_ref().unwrap()
                    } else {
                        self.result.get_or_create(ctx, *coord)
                    };
                    self.pipelines.composite_tile_over(
                        ctx,
                        group_texture,
                        dst,
                        blend,
                        opacity,
                        IVec2::ZERO,
                        None,
                    );
                    self.dispatch_count += 1;
                }
                LayerOp::Adjustment {
                    layer_id,
                    kind,
                    opacity,
                } => {
                    if *opacity == 0.0 {
                        continue;
                    }
                    let Some(_layer) = doc.layer(*layer_id).ok() else {
                        continue;
                    };
                    // The adjustment mutates the accumulator in place, so the
                    // preceding run must be flushed (stored) first.
                    let top = group_stack.last().map(|&(d, _, _)| d);
                    self.flush_run(ctx, coord, top, &mut run);

                    let dst = if let Some(&(depth, _, _)) = group_stack.last() {
                        self.group_pool[depth].as_ref().unwrap()
                    } else {
                        self.result.get_or_create(ctx, *coord)
                    };
                    self.adjustment
                        .apply_adjustment_inplace(ctx, dst, *kind, *opacity);
                }
                LayerOp::Raster {
                    layer_id,
                    effective_offset,
                    blend,
                    opacity,
                    has_mask,
                } => {
                    if *opacity == 0.0 {
                        continue;
                    }
                    let Some(offset) = effective_offset else {
                        continue;
                    };
                    let Some(layer) = doc.layer(*layer_id).ok() else {
                        continue;
                    };
                    let buffer = match &layer.content {
                        LayerContent::Raster { buffer, .. } => buffer,
                        LayerContent::Vector(_) => {
                            match self.vector_buffers.get(layer_id).map(|(_, b)| b) {
                                Some(buffer) => buffer,
                                None => continue,
                            }
                        }
                        LayerContent::Group { .. } | LayerContent::Adjustment(_) => continue,
                    };

                    // Compute the output tile origin in i64 to avoid i32 overflow
                    // on extreme tile coordinates or layer offsets.
                    let t = TILE_SIZE as i64;
                    let Some(doc_origin_x) = (coord.x as i64).checked_mul(t) else {
                        continue;
                    };
                    let Some(doc_origin_y) = (coord.y as i64).checked_mul(t) else {
                        continue;
                    };
                    let Some(local_x) = doc_origin_x.checked_sub(offset.x as i64) else {
                        continue;
                    };
                    let Some(local_y) = doc_origin_y.checked_sub(offset.y as i64) else {
                        continue;
                    };
                    let Ok(local_x) = i32::try_from(local_x) else {
                        continue;
                    };
                    let Ok(local_y) = i32::try_from(local_y) else {
                        continue;
                    };
                    let local_rect =
                        Rect::new(local_x, local_y, TILE_SIZE as u32, TILE_SIZE as u32);

                    for src_tile in local_rect.tiles_covered() {
                        if buffer.get_tile(src_tile).is_none() {
                            continue;
                        }
                        // A masked layer whose mask has no tile at this source
                        // coordinate has zero coverage there, so the
                        // contribution is a no-op and is dropped entirely. This
                        // matches the single-layer path, which composites with a
                        // transparent fallback mask (also a no-op).
                        let mask_present =
                            *has_mask && self.masks.texture((*layer_id, src_tile)).is_some();
                        if *has_mask && !mask_present {
                            continue;
                        }

                        let src_origin_x = (src_tile.x as i64) * t;
                        let src_origin_y = (src_tile.y as i64) * t;
                        let offset_x = offset.x as i64;
                        let offset_y = offset.y as i64;

                        let Some(src_offset_x) =
                            i32::try_from(src_origin_x + offset_x - doc_origin_x).ok()
                        else {
                            continue;
                        };
                        let Some(src_offset_y) =
                            i32::try_from(src_origin_y + offset_y - doc_origin_y).ok()
                        else {
                            continue;
                        };

                        run.push(PendingContribution {
                            layer_id: *layer_id,
                            src_tile,
                            has_mask: mask_present,
                            param: LayerParam::new(
                                *blend,
                                *opacity,
                                IVec2::new(src_offset_x, src_offset_y),
                                mask_present,
                            ),
                        });
                    }
                }
            }
        }

        // Flush any contributions remaining after the last structural op.
        let top = group_stack.last().map(|&(d, _, _)| d);
        self.flush_run(ctx, coord, top, &mut run);

        self.result.set_signature(*coord, new_signature);
    }

    /// Composite the pending raster `run` over a single accumulator and clear
    /// it.
    ///
    /// `top_depth` selects the destination: a group accumulator from the pool,
    /// or the result tile for `coord` when `None`. Runs longer than
    /// [`MAX_BATCH_SLICES`] are split into chunks; each chunk reads the
    /// accumulator the previous chunk stored, so the result is independent of
    /// the split.
    fn flush_run(
        &mut self,
        ctx: &GpuContext,
        coord: &TileCoord,
        top_depth: Option<usize>,
        run: &mut Vec<PendingContribution>,
    ) {
        if run.is_empty() {
            return;
        }

        for chunk in run.chunks(MAX_BATCH_SLICES) {
            let mut srcs: Vec<&wgpu::Texture> = Vec::with_capacity(chunk.len());
            let mut masks: Vec<Option<&wgpu::Texture>> = Vec::with_capacity(chunk.len());
            let mut params: Vec<LayerParam> = Vec::with_capacity(chunk.len());
            for c in chunk {
                let src = self
                    .tiles
                    .texture((c.layer_id, c.src_tile))
                    .expect("colour tile must be cached after sync_layer");
                srcs.push(src);
                let mask = if c.has_mask {
                    Some(
                        self.masks
                            .texture((c.layer_id, c.src_tile))
                            .expect("mask tile must be cached when has_mask is set"),
                    )
                } else {
                    None
                };
                masks.push(mask);
                params.push(c.param);
            }

            let dst = match top_depth {
                Some(depth) => self.group_pool[depth].as_ref().unwrap(),
                None => self.result.get_or_create(ctx, *coord),
            };
            self.batch.composite_run(ctx, dst, &srcs, &masks, &params);
            self.dispatch_count += 1;
        }

        run.clear();
    }

    /// Compute the ordered signature of contributions for one output tile.
    fn signature_for_tile(
        &self,
        doc: &Document,
        coord: &TileCoord,
        plan: &[LayerOp],
    ) -> Vec<Contribution> {
        let mut signature = Vec::new();
        for op in plan {
            match op {
                LayerOp::PushGroup {
                    layer_id,
                    cumulative_offset,
                    blend,
                    opacity,
                    ..
                } => {
                    let Some(layer) = doc.layer(*layer_id).ok() else {
                        continue;
                    };
                    // Record the group using its own offset, not the cumulative
                    // offset; ancestor offset changes are captured by ancestor
                    // contributions.
                    let mut contrib = Contribution::group(layer, layer.offset);
                    contrib.blend = *blend;
                    contrib.opacity_bits = opacity.to_bits();
                    // If the cumulative offset overflowed, the group contributes
                    // nothing; still record it so a fix invalidates the tile.
                    if cumulative_offset.is_none() {
                        contrib.visible = false;
                    }
                    signature.push(contrib);
                }
                LayerOp::PopGroup => {}
                LayerOp::Adjustment {
                    layer_id,
                    kind,
                    opacity,
                } => {
                    let Some(layer) = doc.layer(*layer_id).ok() else {
                        continue;
                    };
                    signature.push(Contribution::adjustment(layer, *kind, *opacity));
                }
                LayerOp::Raster {
                    layer_id,
                    effective_offset,
                    blend,
                    opacity,
                    has_mask,
                } => {
                    if *opacity == 0.0 {
                        continue;
                    }
                    let Some(offset) = effective_offset else {
                        continue;
                    };
                    let Some(layer) = doc.layer(*layer_id).ok() else {
                        continue;
                    };
                    let buffer = match &layer.content {
                        LayerContent::Raster { buffer, .. } => buffer,
                        LayerContent::Vector(_) => {
                            match self.vector_buffers.get(layer_id).map(|(_, b)| b) {
                                Some(buffer) => buffer,
                                None => continue,
                            }
                        }
                        LayerContent::Group { .. } | LayerContent::Adjustment(_) => continue,
                    };
                    let mask_buffer = layer.mask();

                    // Compute the output tile origin in i64 to avoid i32 overflow
                    // on extreme tile coordinates or layer offsets.
                    let t = TILE_SIZE as i64;
                    let Some(doc_origin_x) = (coord.x as i64).checked_mul(t) else {
                        continue;
                    };
                    let Some(doc_origin_y) = (coord.y as i64).checked_mul(t) else {
                        continue;
                    };
                    let Some(local_x) = doc_origin_x.checked_sub(offset.x as i64) else {
                        continue;
                    };
                    let Some(local_y) = doc_origin_y.checked_sub(offset.y as i64) else {
                        continue;
                    };
                    let Ok(local_x) = i32::try_from(local_x) else {
                        continue;
                    };
                    let Ok(local_y) = i32::try_from(local_y) else {
                        continue;
                    };
                    let local_rect =
                        Rect::new(local_x, local_y, TILE_SIZE as u32, TILE_SIZE as u32);

                    for src_tile in local_rect.tiles_covered() {
                        let Some(color_arc) = buffer.get_tile(src_tile) else {
                            continue;
                        };
                        let mask_arc = if *has_mask {
                            mask_buffer.and_then(|m| m.get_tile(src_tile))
                        } else {
                            None
                        };
                        signature.push(Contribution::raster(
                            *layer_id,
                            src_tile,
                            &color_arc,
                            mask_arc.as_ref(),
                            layer.offset,
                            *opacity,
                            *blend,
                            layer.visible,
                            *has_mask,
                        ));
                    }
                }
            }
        }
        signature
    }
}

/// Clamp an opacity value to `[0.0, 1.0]`, treating NaN as zero.
///
/// This matches the CPU reference compositor's treatment of layer opacity.
pub(crate) fn sanitize_opacity(v: f32) -> f32 {
    if v.is_nan() {
        0.0
    } else {
        v.clamp(0.0, 1.0)
    }
}

/// Checked addition of two `IVec2` values, returning `None` on overflow.
fn checked_add(a: IVec2, b: IVec2) -> Option<IVec2> {
    Some(IVec2::new(a.x.checked_add(b.x)?, a.y.checked_add(b.y)?))
}

/// Expand `region` by `margin` tiles in each direction, clamping to `i32` bounds.
///
/// An empty region is returned unchanged so the caller does not composite
/// margin tiles for a viewport that has no visible area.
fn expand_region_by_tiles(region: Rect, margin: i32) -> Rect {
    if region.is_empty() {
        return region;
    }

    let tile_size = TILE_SIZE as i32;
    let margin_px = margin.checked_mul(tile_size).unwrap_or(i32::MAX);

    let x = region.x.saturating_sub(margin_px);
    let y = region.y.saturating_sub(margin_px);
    let right = region
        .right()
        .saturating_add(margin_px as i64)
        .clamp(i32::MIN as i64, i32::MAX as i64) as i32;
    let bottom = region
        .bottom()
        .saturating_add(margin_px as i64)
        .clamp(i32::MIN as i64, i32::MAX as i64) as i32;

    let w = u32::try_from((right as i64 - x as i64).max(0)).unwrap_or(u32::MAX);
    let h = u32::try_from((bottom as i64 - y as i64).max(0)).unwrap_or(u32::MAX);
    Rect::new(x, y, w, h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::GpuContext;
    use ogre_core::TiledBuffer;

    const EPSILON: f32 = 1e-4;

    #[test]
    fn default_compositor_uses_finite_tile_budget() {
        let ctx = GpuContext::headless();
        let compositor = Compositor::new(&ctx);
        assert!(!compositor.tiles.budget().is_unlimited());
        assert!(!compositor.masks.budget().is_unlimited());
        assert!(!compositor.result.budget.is_unlimited());
        assert_eq!(
            compositor.tiles.budget().max_gpu_tiles,
            DEFAULT_GPU_TILE_BUDGET
        );
        assert_eq!(
            compositor.result.budget.max_gpu_tiles,
            DEFAULT_GPU_RESULT_TILE_BUDGET
        );
    }

    #[test]
    fn custom_budget_compositor_evicts_to_limit() {
        let ctx = GpuContext::headless();
        let mut doc = Document::new(512, 64);
        for i in 0..8 {
            let id = doc.add_raster_layer(&format!("l{i}"));
            let x = i * 64;
            fill_rect(
                doc.layer_mut(id).unwrap().buffer_mut().unwrap(),
                Rect::new(x, 0, 64, 64),
                Rgba32F::new(1.0, 0.0, 0.0, 1.0),
            );
        }

        let mut compositor = Compositor::with_budget(&ctx, CacheBudget::new(4));
        compositor
            .composite(&ctx, &doc, Rect::new(0, 0, 512, 64))
            .unwrap();
        assert!(compositor.tiles.len() <= 4);
    }

    fn fill_rect(buffer: &mut TiledBuffer, rect: Rect, color: Rgba32F) {
        let area = (rect.w as usize)
            .checked_mul(rect.h as usize)
            .expect("rect too large");
        let data = vec![color; area];
        buffer.blit_rect(rect, &data);
    }

    fn read_region(compositor: &Compositor, ctx: &GpuContext, region: Rect) -> Vec<Rgba32F> {
        let w = region.w as usize;
        let h = region.h as usize;
        let mut out = vec![Rgba32F::TRANSPARENT; w * h];
        for tile in region.tiles_covered() {
            let tile_pixels = compositor.read_result_tile(ctx, tile);
            let tile_origin_x = (tile.x as i64) * (TILE_SIZE as i64);
            let tile_origin_y = (tile.y as i64) * (TILE_SIZE as i64);
            let tile_rect = Rect::new(
                tile_origin_x as i32,
                tile_origin_y as i32,
                TILE_SIZE as u32,
                TILE_SIZE as u32,
            );
            let Some(inter) = tile_rect.intersect(region) else {
                continue;
            };
            let Some(pixels) = tile_pixels else {
                continue;
            };
            for y in inter.y..inter.bottom() as i32 {
                for x in inter.x..inter.right() as i32 {
                    let lx = (x as i64 - tile_origin_x) as usize;
                    let ly = (y as i64 - tile_origin_y) as usize;
                    let out_idx = ((y - region.y) as usize) * w + ((x - region.x) as usize);
                    out[out_idx] = pixels[ly * TILE_SIZE + lx];
                }
            }
        }
        out
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

    fn assert_pixels_approx_eq(actual: Rgba32F, expected: Rgba32F, eps: f32) {
        assert!(
            (actual.r - expected.r).abs() < eps
                && (actual.g - expected.g).abs() < eps
                && (actual.b - expected.b).abs() < eps
                && (actual.a - expected.a).abs() < eps,
            "{:?} != {:?} within {}",
            actual,
            expected,
            eps
        );
    }

    #[test]
    fn run_exceeding_max_batch_slices_matches_cpu_reference() {
        // A single output tile fed by more than MAX_BATCH_SLICES raster
        // contributions forces the batched run to be split into multiple
        // chunks, where each chunk reads the accumulator the previous chunk
        // stored. This exercises the cross-submit read-after-write on the
        // result tile that no other test reaches (all others have <= 64
        // contributions per tile). The composite must still match the CPU
        // reference exactly.
        let ctx = GpuContext::headless();
        let mut doc = Document::new(64, 64);
        let region = Rect::new(0, 0, 64, 64);

        let layer_count = MAX_BATCH_SLICES + 17;
        let blends = [
            BlendMode::Normal,
            BlendMode::Multiply,
            BlendMode::Screen,
            BlendMode::Overlay,
            BlendMode::SoftLight,
        ];
        for i in 0..layer_count {
            let id = doc.add_raster_layer(&format!("l{i}"));
            // Semi-transparent, varied colours so every layer contributes to
            // the accumulated result rather than being hidden.
            let t = i as f32 / layer_count as f32;
            let color = Rgba32F::new(t, 1.0 - t, 0.5 * t, 0.35);
            fill_rect(
                doc.layer_mut(id).unwrap().buffer_mut().unwrap(),
                region,
                color,
            );
            doc.layer_mut(id).unwrap().blend = blends[i % blends.len()];
            doc.layer_mut(id).unwrap().opacity = 0.8;
        }

        let mut compositor = Compositor::new(&ctx);
        compositor.composite(&ctx, &doc, region).unwrap();
        // The single output tile must have needed more than one batched chunk.
        assert!(
            compositor.dispatch_count() >= 2,
            "expected the run to be split across >= 2 chunks, got {} dispatches",
            compositor.dispatch_count()
        );

        let gpu = read_region(&compositor, &ctx, region);
        let cpu = ogre_core::compositor::composite_document(&doc, region).unwrap();
        assert_region_approx_eq(&gpu, &cpu, EPSILON);
    }

    #[test]
    fn three_layer_stack_matches_cpu_reference() {
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

        let gpu = read_region(&compositor, &ctx, Rect::new(0, 0, 512, 512));
        let cpu =
            ogre_core::compositor::composite_document(&doc, Rect::new(0, 0, 512, 512)).unwrap();
        assert_region_approx_eq(&gpu, &cpu, EPSILON);
    }

    #[test]
    fn dirty_recomposites_only_affected_output_tiles() {
        let ctx = GpuContext::headless();
        let mut doc = Document::new(512, 512);

        let bg = doc.add_raster_layer("bg");
        fill_rect(
            doc.layer_mut(bg).unwrap().buffer_mut().unwrap(),
            Rect::new(0, 0, 512, 512),
            Rgba32F::new(1.0, 0.0, 0.0, 1.0),
        );

        let fg = doc.add_raster_layer("fg");
        fill_rect(
            doc.layer_mut(fg).unwrap().buffer_mut().unwrap(),
            Rect::new(0, 0, 256, 256),
            Rgba32F::new(0.0, 0.0, 1.0, 1.0),
        );

        let mut compositor = Compositor::new(&ctx);
        compositor
            .composite(&ctx, &doc, Rect::new(0, 0, 512, 512))
            .unwrap();
        let first = read_region(&compositor, &ctx, Rect::new(0, 0, 512, 512));

        // Edit a single pixel inside the foreground's only occupied tile.
        doc.layer_mut(fg)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(10, 10), Rgba32F::new(0.0, 1.0, 0.0, 1.0));

        compositor
            .composite(&ctx, &doc, Rect::new(0, 0, 512, 512))
            .unwrap();
        let second = read_region(&compositor, &ctx, Rect::new(0, 0, 512, 512));
        let cpu =
            ogre_core::compositor::composite_document(&doc, Rect::new(0, 0, 512, 512)).unwrap();
        assert_region_approx_eq(&second, &cpu, EPSILON);

        // Only the top-left output tile should have been recomputed.
        let expected_dirty: AHashSet<TileCoord> = AHashSet::from_iter([TileCoord { x: 0, y: 0 }]);
        assert_eq!(*compositor.last_dirty_keys(), expected_dirty);

        // Everywhere outside the dirty tile must be pixel-identical to the
        // first composite.
        let mut unchanged = Vec::new();
        let mut first_unchanged = Vec::new();
        for y in 0..512 {
            for x in 0..512 {
                if x < 256 && y < 256 {
                    continue;
                }
                let idx = (y as usize) * 512 + (x as usize);
                unchanged.push(second[idx]);
                first_unchanged.push(first[idx]);
            }
        }
        assert_eq!(unchanged, first_unchanged);
    }

    #[test]
    fn opacity_hidden_and_mask_match_cpu_reference() {
        let ctx = GpuContext::headless();
        let mut doc = Document::new(256, 256);

        let bg = doc.add_raster_layer("bg");
        fill_rect(
            doc.layer_mut(bg).unwrap().buffer_mut().unwrap(),
            Rect::new(0, 0, 256, 256),
            Rgba32F::new(1.0, 0.0, 0.0, 1.0),
        );

        let opacity_layer = doc.add_raster_layer("opacity");
        fill_rect(
            doc.layer_mut(opacity_layer).unwrap().buffer_mut().unwrap(),
            Rect::new(0, 0, 256, 256),
            Rgba32F::new(0.0, 0.0, 1.0, 1.0),
        );
        doc.layer_mut(opacity_layer).unwrap().opacity = 0.25;

        let hidden = doc.add_raster_layer("hidden");
        fill_rect(
            doc.layer_mut(hidden).unwrap().buffer_mut().unwrap(),
            Rect::new(0, 0, 256, 256),
            Rgba32F::new(0.0, 1.0, 0.0, 1.0),
        );
        doc.layer_mut(hidden).unwrap().visible = false;

        // Build a masked layer by constructing the layer content directly.
        let mut masked = Layer::new_raster("masked");
        let mut buffer = TiledBuffer::new();
        fill_rect(
            &mut buffer,
            Rect::new(0, 0, 256, 256),
            Rgba32F::new(1.0, 1.0, 0.0, 1.0),
        );
        let mut mask = TiledBuffer::new();
        fill_rect(
            &mut mask,
            Rect::new(0, 0, 128, 256),
            Rgba32F::new(0.5, 0.0, 0.0, 1.0),
        );
        masked.content = LayerContent::Raster {
            buffer,
            mask: Some(mask),
        };
        let masked_id = doc.insert_layer_above(masked, hidden).unwrap();
        doc.layer_mut(masked_id).unwrap().blend = BlendMode::Normal;

        let mut compositor = Compositor::new(&ctx);
        compositor
            .composite(&ctx, &doc, Rect::new(0, 0, 256, 256))
            .unwrap();

        let gpu = read_region(&compositor, &ctx, Rect::new(0, 0, 256, 256));
        let cpu =
            ogre_core::compositor::composite_document(&doc, Rect::new(0, 0, 256, 256)).unwrap();
        assert_region_approx_eq(&gpu, &cpu, EPSILON);
    }

    #[test]
    fn sparse_mask_missing_tiles_are_transparent() {
        let ctx = GpuContext::headless();
        let mut doc = Document::new(512, 512);

        // Dark background so any unmasked contribution is obvious.
        let bg = doc.add_raster_layer("bg");
        fill_rect(
            doc.layer_mut(bg).unwrap().buffer_mut().unwrap(),
            Rect::new(0, 0, 512, 512),
            Rgba32F::new(0.1, 0.1, 0.1, 1.0),
        );

        // Build a 2x2 tile layer whose mask only covers the top-left tile.
        // The CPU reference treats absent mask tiles as transparent coverage,
        // so the right and bottom tiles of the layer must be fully masked out.
        let mut masked = Layer::new_raster("masked");
        let mut buffer = TiledBuffer::new();
        fill_rect(
            &mut buffer,
            Rect::new(0, 0, 512, 512),
            Rgba32F::new(1.0, 0.0, 0.0, 1.0),
        );
        let mut mask = TiledBuffer::new();
        fill_rect(
            &mut mask,
            Rect::new(0, 0, 256, 256),
            Rgba32F::new(0.5, 0.0, 0.0, 1.0),
        );
        masked.content = LayerContent::Raster {
            buffer,
            mask: Some(mask),
        };
        let masked_id = doc.insert_layer_above(masked, bg).unwrap();
        doc.layer_mut(masked_id).unwrap().blend = BlendMode::Normal;

        let mut compositor = Compositor::new(&ctx);
        compositor
            .composite(&ctx, &doc, Rect::new(0, 0, 512, 512))
            .unwrap();

        let gpu = read_region(&compositor, &ctx, Rect::new(0, 0, 512, 512));
        let cpu =
            ogre_core::compositor::composite_document(&doc, Rect::new(0, 0, 512, 512)).unwrap();
        assert_region_approx_eq(&gpu, &cpu, EPSILON);

        // Explicit sanity check: outside the masked top-left tile the layer
        // contributes nothing, so the output equals the background.
        for y in 0..512 {
            for x in 256..512 {
                let idx = (y as usize) * 512 + (x as usize);
                assert_pixels_approx_eq(gpu[idx], Rgba32F::new(0.1, 0.1, 0.1, 1.0), EPSILON);
            }
        }
        for y in 256..512 {
            for x in 0..256 {
                let idx = (y as usize) * 512 + (x as usize);
                assert_pixels_approx_eq(gpu[idx], Rgba32F::new(0.1, 0.1, 0.1, 1.0), EPSILON);
            }
        }
    }

    #[test]
    fn group_isolation_matches_cpu_reference() {
        let ctx = GpuContext::headless();
        let mut doc = Document::new(256, 256);

        let bg = doc.add_raster_layer("bg");
        fill_rect(
            doc.layer_mut(bg).unwrap().buffer_mut().unwrap(),
            Rect::new(0, 0, 256, 256),
            Rgba32F::new(1.0, 0.0, 0.0, 1.0),
        );

        let group = Layer::new_group("group");
        let group_id = doc.insert_layer_above(group, bg).unwrap();
        doc.layer_mut(group_id).unwrap().opacity = 0.5;
        doc.layer_mut(group_id).unwrap().blend = BlendMode::Multiply;

        let child = doc.add_raster_layer("child");
        doc.move_into_group(child, group_id, 0).unwrap();
        fill_rect(
            doc.layer_mut(child).unwrap().buffer_mut().unwrap(),
            Rect::new(0, 0, 256, 256),
            Rgba32F::new(0.0, 1.0, 0.0, 1.0),
        );

        let mut compositor = Compositor::new(&ctx);
        compositor
            .composite(&ctx, &doc, Rect::new(0, 0, 256, 256))
            .unwrap();

        let gpu = read_region(&compositor, &ctx, Rect::new(0, 0, 256, 256));
        let cpu =
            ogre_core::compositor::composite_document(&doc, Rect::new(0, 0, 256, 256)).unwrap();
        assert_region_approx_eq(&gpu, &cpu, EPSILON);
    }

    #[test]
    fn group_pool_growth_is_capped() {
        let ctx = GpuContext::headless();
        let mut doc = Document::new(16, 16);

        let bg = doc.add_raster_layer("bg");
        doc.layer_mut(bg)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        // Nest a raster layer inside MAX_GROUP_DEPTH + 5 groups.
        let mut parent = bg;
        for i in 0..(MAX_GROUP_DEPTH + 5) {
            let group = Layer::new_group(format!("g{i}"));
            let group_id = doc.insert_layer_above(group, parent).unwrap();
            doc.move_into_group(parent, group_id, 0).unwrap();
            parent = group_id;
        }

        let mut compositor = Compositor::new(&ctx);
        let result = compositor.composite(&ctx, &doc, Rect::new(0, 0, 16, 16));
        assert!(
            result.is_err(),
            "expected an error for excessive group nesting"
        );
        assert!(
            compositor.group_pool.len() <= MAX_GROUP_DEPTH,
            "group_pool grew beyond the cap: {}",
            compositor.group_pool.len()
        );
    }

    #[test]
    fn extreme_group_offset_skips_subtree() {
        let ctx = GpuContext::headless();
        let mut doc = Document::new(10, 10);

        let bg = doc.add_raster_layer("bg");
        doc.layer_mut(bg)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        let group = Layer::new_group("group");
        let group_id = doc.insert_layer_above(group, bg).unwrap();
        doc.layer_mut(group_id).unwrap().offset = IVec2::new(i32::MAX, i32::MAX);

        let child = doc.add_raster_layer("child");
        doc.move_into_group(child, group_id, 0).unwrap();
        doc.layer_mut(child)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(0.0, 1.0, 0.0, 1.0));

        // The CPU reference treats an overflowing group offset as transparent,
        // so the child must not be composited at its un-offset local
        // coordinate (5, 5). The background red pixel should remain visible.
        let mut compositor = Compositor::new(&ctx);
        compositor
            .composite(&ctx, &doc, Rect::new(5, 5, 1, 1))
            .unwrap();

        let gpu = read_region(&compositor, &ctx, Rect::new(5, 5, 1, 1));
        assert_eq!(gpu[0], Rgba32F::new(1.0, 0.0, 0.0, 1.0));
    }

    #[test]
    fn mask_cache_cleared_when_layer_becomes_group() {
        let ctx = GpuContext::headless();
        let mut doc = Document::new(256, 256);

        let bg = doc.add_raster_layer("bg");
        fill_rect(
            doc.layer_mut(bg).unwrap().buffer_mut().unwrap(),
            Rect::new(0, 0, 256, 256),
            Rgba32F::new(0.1, 0.1, 0.1, 1.0),
        );

        let mut masked = Layer::new_raster("masked");
        let mut buffer = TiledBuffer::new();
        fill_rect(
            &mut buffer,
            Rect::new(0, 0, 256, 256),
            Rgba32F::new(1.0, 0.0, 0.0, 1.0),
        );
        let mut mask = TiledBuffer::new();
        fill_rect(
            &mut mask,
            Rect::new(0, 0, 256, 256),
            Rgba32F::new(0.5, 0.0, 0.0, 1.0),
        );
        masked.content = LayerContent::Raster {
            buffer,
            mask: Some(mask),
        };
        let masked_id = doc.insert_layer_above(masked, bg).unwrap();

        let mut compositor = Compositor::new(&ctx);
        compositor
            .composite(&ctx, &doc, Rect::new(0, 0, 256, 256))
            .unwrap();
        assert!(compositor.masks.contains_layer(masked_id));

        // Turn the raster layer into a group. Its cached mask textures must be
        // evicted, because groups own no mask.
        doc.layer_mut(masked_id).unwrap().content = LayerContent::Group { children: vec![] };

        compositor
            .composite(&ctx, &doc, Rect::new(0, 0, 256, 256))
            .unwrap();
        assert!(!compositor.masks.contains_layer(masked_id));
    }

    #[test]
    fn adjustment_layer_invert_matches_cpu_reference() {
        let ctx = GpuContext::headless();
        let mut doc = Document::new(10, 10);

        let bg = doc.add_raster_layer("bg");
        doc.layer_mut(bg)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        doc.add_adjustment_layer("invert", AdjustmentKind::Invert);

        let mut compositor = Compositor::new(&ctx);
        compositor
            .composite(&ctx, &doc, Rect::new(5, 5, 1, 1))
            .unwrap();

        let gpu = read_region(&compositor, &ctx, Rect::new(5, 5, 1, 1));
        let cpu = ogre_core::compositor::composite_document(&doc, Rect::new(5, 5, 1, 1)).unwrap();
        assert_region_approx_eq(&gpu, &cpu, EPSILON);
    }

    #[test]
    fn adjustment_layer_all_kinds_matches_cpu_reference() {
        let ctx = GpuContext::headless();
        let mut doc = Document::new(256, 256);

        let bg = doc.add_raster_layer("bg");
        fill_rect(
            doc.layer_mut(bg).unwrap().buffer_mut().unwrap(),
            Rect::new(0, 0, 256, 256),
            Rgba32F::new(0.5, 0.25, 0.75, 1.0),
        );

        doc.add_adjustment_layer("invert", AdjustmentKind::Invert);
        doc.add_adjustment_layer("desaturate", AdjustmentKind::Desaturate);
        doc.add_adjustment_layer(
            "brightness_contrast",
            AdjustmentKind::BrightnessContrast {
                brightness: 0.1,
                contrast: 1.2,
            },
        );
        doc.add_adjustment_layer(
            "hue_sat",
            AdjustmentKind::HueSat {
                hue: 45.0,
                saturation: 1.5,
                lightness: 0.05,
            },
        );
        doc.add_adjustment_layer(
            "levels",
            AdjustmentKind::Levels {
                input_black: 0.1,
                input_white: 0.9,
                output_black: 0.0,
                output_white: 1.0,
                gamma: 1.2,
            },
        );
        doc.add_adjustment_layer(
            "curves",
            AdjustmentKind::curves([(0.0, 0.0), (0.25, 0.1), (0.75, 0.9), (1.0, 1.0)]),
        );

        let mut compositor = Compositor::new(&ctx);
        compositor
            .composite(&ctx, &doc, Rect::new(0, 0, 256, 256))
            .unwrap();

        let gpu = read_region(&compositor, &ctx, Rect::new(0, 0, 256, 256));
        let cpu =
            ogre_core::compositor::composite_document(&doc, Rect::new(0, 0, 256, 256)).unwrap();
        assert_region_approx_eq(&gpu, &cpu, EPSILON);
    }

    #[test]
    fn adjustment_layer_inside_group_matches_cpu_reference() {
        let ctx = GpuContext::headless();
        let mut doc = Document::new(256, 256);

        let bg = doc.add_raster_layer("bg");
        fill_rect(
            doc.layer_mut(bg).unwrap().buffer_mut().unwrap(),
            Rect::new(0, 0, 256, 256),
            Rgba32F::new(1.0, 0.0, 0.0, 1.0),
        );

        let group = Layer::new_group("group");
        let group_id = doc.insert_layer_above(group, bg).unwrap();
        doc.layer_mut(group_id).unwrap().opacity = 0.5;
        doc.layer_mut(group_id).unwrap().blend = BlendMode::Multiply;

        let child = doc.add_raster_layer("child");
        doc.move_into_group(child, group_id, 0).unwrap();
        fill_rect(
            doc.layer_mut(child).unwrap().buffer_mut().unwrap(),
            Rect::new(0, 0, 256, 256),
            Rgba32F::new(0.0, 1.0, 0.0, 1.0),
        );

        let adj = Layer::new_adjustment("invert", AdjustmentKind::Invert);
        let adj_id = doc.insert_layer_above(adj, child).unwrap();
        doc.move_into_group(adj_id, group_id, 1).unwrap();

        let mut compositor = Compositor::new(&ctx);
        compositor
            .composite(&ctx, &doc, Rect::new(0, 0, 256, 256))
            .unwrap();

        let gpu = read_region(&compositor, &ctx, Rect::new(0, 0, 256, 256));
        let cpu =
            ogre_core::compositor::composite_document(&doc, Rect::new(0, 0, 256, 256)).unwrap();
        assert_region_approx_eq(&gpu, &cpu, EPSILON);
    }

    #[test]
    fn adjustment_layer_change_invalidates_tile_signature() {
        let ctx = GpuContext::headless();
        let mut doc = Document::new(10, 10);

        let bg = doc.add_raster_layer("bg");
        doc.layer_mut(bg)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        let adj = doc.add_adjustment_layer("adj", AdjustmentKind::Invert);

        let mut compositor = Compositor::new(&ctx);
        compositor
            .composite(&ctx, &doc, Rect::new(5, 5, 1, 1))
            .unwrap();
        let first = compositor
            .result
            .signature(TileCoord { x: 0, y: 0 })
            .unwrap()
            .to_vec();

        doc.layer_mut(adj).unwrap().content = LayerContent::Adjustment(AdjustmentKind::Desaturate);

        compositor
            .composite(&ctx, &doc, Rect::new(5, 5, 1, 1))
            .unwrap();
        let second = compositor
            .result
            .signature(TileCoord { x: 0, y: 0 })
            .unwrap()
            .to_vec();

        assert_ne!(first, second);
    }

    #[test]
    fn panning_within_margin_reuses_cached_tiles() {
        let ctx = GpuContext::headless();
        let mut doc = Document::new(512, 512);

        let bg = doc.add_raster_layer("bg");
        fill_rect(
            doc.layer_mut(bg).unwrap().buffer_mut().unwrap(),
            Rect::new(0, 0, 512, 512),
            Rgba32F::new(1.0, 0.0, 0.0, 1.0),
        );

        let mut compositor = Compositor::new(&ctx);
        compositor
            .composite(&ctx, &doc, Rect::new(0, 0, 256, 256))
            .unwrap();
        let first_dirty = compositor.last_dirty_keys().len();
        assert!(first_dirty > 0, "first composite should dirty tiles");

        // Pan by one tile (256 px). With a 2-tile margin, the new visible region
        // is still inside the previously cached expanded region, so the newly
        // visible tile should not be recomputed.
        compositor
            .composite(&ctx, &doc, Rect::new(256, 0, 256, 256))
            .unwrap();
        assert!(
            !compositor
                .last_dirty_keys()
                .contains(&TileCoord { x: 1, y: 0 }),
            "pan inside margin should not recompute the newly visible tile, got {:?}",
            compositor.last_dirty_keys()
        );
    }
}
