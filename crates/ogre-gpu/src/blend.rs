// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Single-layer blend compute pipeline.
//!
//! This module owns the compute shader that composites one source tile over the
//! current accumulator tile. The pipeline and a single uniform buffer are
//! created once and reused; per-tile work only allocates a bind group.

use std::num::NonZeroU64;

use bytemuck::{Pod, Zeroable};
use ogre_core::{AdjustmentKind, BlendMode, IVec2, Rgba32F, Tile, TILE_SIZE};

use crate::compositor::sanitize_opacity;
use crate::context::GpuContext;
use crate::tile_cache::upload_tile;

const BLEND_WGSL: &str = include_str!("shaders/blend.wgsl");
const BLEND_BATCH_WGSL: &str = include_str!("shaders/blend_batch.wgsl");

/// Uniform data sent to the blend compute shader.
///
/// The layout is `#[repr(C)]` and matches the WGSL `Params` struct exactly:
/// `mode` (4 bytes), `opacity` (4 bytes), `src_offset` as `vec2<i32>` (8 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct BlendUniform {
    /// Shader blend-mode index, matching [`shader_index`].
    pub mode: u32,
    /// Opacity in the range `[0.0, 1.0]`.
    pub opacity: f32,
    /// Horizontal pixel offset from destination to source coordinates.
    pub src_offset_x: i32,
    /// Vertical pixel offset from destination to source coordinates.
    pub src_offset_y: i32,
}

impl BlendUniform {
    /// Create a uniform value from a [`BlendMode`], opacity, and source offset.
    ///
    /// NaN opacity is sanitized to `0.0` to match the CPU reference compositor.
    pub fn new(mode: BlendMode, opacity: f32, src_offset: IVec2) -> Self {
        let opacity = sanitize_opacity(opacity);
        Self {
            mode: shader_index(mode),
            opacity,
            src_offset_x: src_offset.x,
            src_offset_y: src_offset.y,
        }
    }
}

/// Map a [`BlendMode`] to the `mode: u32` value consumed by the WGSL shader.
///
/// The indices match the `MODE_*` constants declared in `blend.wgsl`.
pub fn shader_index(mode: BlendMode) -> u32 {
    match mode {
        BlendMode::Normal => 0,
        BlendMode::Multiply => 1,
        BlendMode::Screen => 2,
        BlendMode::Overlay => 3,
        BlendMode::Darken => 4,
        BlendMode::Lighten => 5,
        BlendMode::ColorDodge => 6,
        BlendMode::ColorBurn => 7,
        BlendMode::HardLight => 8,
        BlendMode::SoftLight => 9,
        BlendMode::Difference => 10,
        BlendMode::Exclusion => 11,
        BlendMode::Add => 12,
    }
}

/// Cached compute pipeline and bind-group layout for single-layer compositing.
#[derive(Debug)]
pub struct BlendPipelines {
    pipeline: wgpu::ComputePipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    uniform_buffer: wgpu::Buffer,
    /// Tile-sized white mask used when a layer has no mask at all.
    _fallback_white_mask: wgpu::Texture,
    fallback_white_mask_view: wgpu::TextureView,
}

impl BlendPipelines {
    /// Create the cached blend pipeline on `ctx`.
    pub fn new(ctx: &GpuContext) -> Self {
        let device = &ctx.device;
        let shader_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ogre blend shader"),
            source: wgpu::ShaderSource::Wgsl(BLEND_WGSL.into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ogre blend bind group layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::ReadWrite,
                        format: wgpu::TextureFormat::Rgba32Float,
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ogre blend pipeline layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("ogre blend pipeline"),
            layout: Some(&pipeline_layout),
            module: &shader_module,
            entry_point: Some("composite"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            cache: None,
        });

        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ogre blend uniform"),
            size: std::mem::size_of::<BlendUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // A tile-sized white mask so that `textureLoad` returns `mask.r == 1.0`
        // for every pixel when a layer has no mask.
        let white = Rgba32F::new(1.0, 0.0, 0.0, 1.0);
        let fallback_white_mask = upload_tile(ctx, &Tile::filled(white));
        let fallback_white_mask_view =
            fallback_white_mask.create_view(&wgpu::TextureViewDescriptor::default());

        Self {
            pipeline,
            bind_group_layout,
            uniform_buffer,
            _fallback_white_mask: fallback_white_mask,
            fallback_white_mask_view,
        }
    }

    /// Composite `src` over `dst` in-place.
    ///
    /// `src` is read through a sampled `texture_2d<f32>`. `dst` must be a
    /// `Rgba32Float` texture created with `wgpu::TextureUsages::STORAGE_BINDING`
    /// and bound read/write. The work is submitted to `ctx.queue` before this
    /// function returns.
    ///
    /// The source pixel at `src_coord` is mapped to destination pixel
    /// `src_coord + src_offset`. This lets a single source tile be positioned
    /// anywhere relative to the destination accumulator tile, which is required
    /// for layer offsets that are not multiples of the tile size.
    ///
    /// `mask`, if provided, must be the same size as `src`. The shader scales
    /// the source alpha by `mask.r` clamped to `[0, 1]`. Pass `None` when the
    /// layer has no mask; pass the tile-sized transparent fallback texture when
    /// the layer has a mask but the current source tile has no corresponding
    /// mask tile.
    ///
    /// This is a low-level dispatch helper: it does **not** call
    /// `device.poll`. Callers that need to read the result on the CPU must
    /// flush the queue themselves (the readback helper in [`crate::tile_cache`]
    /// does this).
    ///
    /// # Panics
    ///
    /// Panics if `src` and `dst` do not have the same dimensions, or if `mask`
    /// is provided and does not have the same dimensions as `src`.
    #[allow(clippy::too_many_arguments)]
    pub fn composite_tile_over(
        &self,
        ctx: &GpuContext,
        src: &wgpu::Texture,
        dst: &wgpu::Texture,
        mode: BlendMode,
        opacity: f32,
        src_offset: IVec2,
        mask: Option<&wgpu::Texture>,
    ) {
        assert_eq!(
            src.size(),
            dst.size(),
            "source and destination textures must have the same dimensions"
        );
        if let Some(m) = mask {
            assert_eq!(
                m.size(),
                src.size(),
                "mask and source textures must have the same dimensions"
            );
        }

        let uniform = BlendUniform::new(mode, opacity, src_offset);
        ctx.queue
            .write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniform));

        let src_view = src.create_view(&wgpu::TextureViewDescriptor::default());
        let dst_view = dst.create_view(&wgpu::TextureViewDescriptor::default());
        let mask_view = mask
            .map(|m| m.create_view(&wgpu::TextureViewDescriptor::default()))
            .unwrap_or_else(|| self.fallback_white_mask_view.clone());

        let bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ogre blend bind group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&src_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&dst_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: self.uniform_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&mask_view),
                },
            ],
        });

        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("ogre blend dispatch"),
            });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("ogre blend pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.dispatch_workgroups(
                (dst.size().width).div_ceil(16),
                (dst.size().height).div_ceil(16),
                1,
            );
        }
        ctx.queue.submit(Some(encoder.finish()));
    }
}

/// Maximum number of source slices composited in a single batched dispatch.
///
/// A run of consecutive raster contributions longer than this is split into
/// chunks; each chunk reads the accumulator the previous chunk stored, so the
/// result is independent of the split. The cap bounds the size of the array
/// textures: `MAX_BATCH_SLICES` × `TILE_SIZE²` × 16 bytes each.
pub const MAX_BATCH_SLICES: usize = 64;

/// Per-contribution parameters for the batched blend shader.
///
/// The layout is `#[repr(C)]` and matches the WGSL `LayerParam` struct exactly
/// (eight 32-bit words, 32 bytes). The three trailing words are padding so the
/// storage-buffer array stride is a clean 32 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct LayerParam {
    mode: u32,
    opacity: f32,
    offset_x: i32,
    offset_y: i32,
    has_mask: u32,
    _pad: [u32; 3],
}

impl LayerParam {
    /// Build a contribution parameter.
    ///
    /// `src_offset` matches [`BlendUniform`]: the source pixel at `c` maps to
    /// destination pixel `c + src_offset`. NaN opacity is sanitized to `0.0`.
    pub fn new(mode: BlendMode, opacity: f32, src_offset: IVec2, has_mask: bool) -> Self {
        let opacity = sanitize_opacity(opacity);
        Self {
            mode: shader_index(mode),
            opacity,
            offset_x: src_offset.x,
            offset_y: src_offset.y,
            has_mask: has_mask as u32,
            _pad: [0; 3],
        }
    }
}

/// Create an empty `TILE_SIZE × TILE_SIZE × MAX_BATCH_SLICES` array texture used
/// to feed source (or mask) tiles to the batched blend shader.
fn create_batch_array(ctx: &GpuContext, label: &str) -> wgpu::Texture {
    ctx.device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: TILE_SIZE as u32,
            height: TILE_SIZE as u32,
            depth_or_array_layers: MAX_BATCH_SLICES as u32,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba32Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

/// Record a full-tile copy of `src` into array layer `slice` of `array`.
fn copy_tile_into_slice(
    encoder: &mut wgpu::CommandEncoder,
    src: &wgpu::Texture,
    array: &wgpu::Texture,
    slice: u32,
) {
    encoder.copy_texture_to_texture(
        src.as_image_copy(),
        wgpu::TexelCopyTextureInfo {
            texture: array,
            mip_level: 0,
            origin: wgpu::Origin3d {
                x: 0,
                y: 0,
                z: slice,
            },
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::Extent3d {
            width: TILE_SIZE as u32,
            height: TILE_SIZE as u32,
            depth_or_array_layers: 1,
        },
    );
}

/// Compute pipeline that composites an ordered run of source tiles over one
/// destination accumulator tile in a single dispatch.
///
/// This is the multi-layer counterpart to [`BlendPipelines::composite_tile_over`]:
/// where that issues one dispatch (and one bind group) per contribution, this
/// copies each contribution's source tile into a slice of a shared array
/// texture and runs a single dispatch that accumulates the whole run in
/// registers. The per-pixel math is identical, so the output matches the
/// single-layer path within the golden tolerance.
///
/// The array textures and the parameter buffer are allocated lazily on first
/// use and reused across calls.
#[derive(Debug)]
pub struct BatchBlendPipeline {
    pipeline: wgpu::ComputePipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    params_buffer: wgpu::Buffer,
    src_array: Option<wgpu::Texture>,
    mask_array: Option<wgpu::Texture>,
}

impl BatchBlendPipeline {
    /// Create the batched blend pipeline on `ctx`.
    pub fn new(ctx: &GpuContext) -> Self {
        let device = &ctx.device;
        let shader_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ogre batch blend shader"),
            source: wgpu::ShaderSource::Wgsl(BLEND_BATCH_WGSL.into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ogre batch blend bind group layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2Array,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::ReadWrite,
                        format: wgpu::TextureFormat::Rgba32Float,
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2Array,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ogre batch blend pipeline layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("ogre batch blend pipeline"),
            layout: Some(&pipeline_layout),
            module: &shader_module,
            entry_point: Some("composite"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            cache: None,
        });

        let params_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ogre batch blend params"),
            size: (MAX_BATCH_SLICES * std::mem::size_of::<LayerParam>()) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            pipeline,
            bind_group_layout,
            params_buffer,
            src_array: None,
            mask_array: None,
        }
    }

    /// Composite an ordered run of source tiles over `dst` in a single dispatch.
    ///
    /// `srcs`, `masks`, and `params` are parallel slices of equal length `n`,
    /// where `1 <= n <= MAX_BATCH_SLICES`. `params[i]` describes how `srcs[i]`
    /// blends over the running accumulator; `masks[i]` is the layer mask tile
    /// for that contribution (must be `Some` exactly when `params[i].has_mask`
    /// is set). `dst` must be an `Rgba32Float` storage texture, and the shader
    /// reads its current contents as the initial accumulator — so clearing it
    /// first yields a from-transparent composite, while leaving prior content
    /// (e.g. a popped group result) composites over it.
    ///
    /// The work is submitted to `ctx.queue` before this function returns.
    ///
    /// # Panics
    ///
    /// Panics if the three slices have different lengths, if `n` is `0` or
    /// exceeds [`MAX_BATCH_SLICES`], or if a `params[i].has_mask` flag does not
    /// agree with whether `masks[i]` is `Some`.
    pub fn composite_run(
        &mut self,
        ctx: &GpuContext,
        dst: &wgpu::Texture,
        srcs: &[&wgpu::Texture],
        masks: &[Option<&wgpu::Texture>],
        params: &[LayerParam],
    ) {
        let n = srcs.len();
        assert!(
            n == masks.len() && n == params.len(),
            "srcs, masks, and params must have equal length"
        );
        assert!(
            (1..=MAX_BATCH_SLICES).contains(&n),
            "batch run length {n} out of range 1..={MAX_BATCH_SLICES}"
        );

        let any_mask = masks.iter().any(Option::is_some);

        if self.src_array.is_none() {
            self.src_array = Some(create_batch_array(ctx, "ogre batch src array"));
        }
        if any_mask && self.mask_array.is_none() {
            self.mask_array = Some(create_batch_array(ctx, "ogre batch mask array"));
        }
        let src_array = self.src_array.as_ref().unwrap();
        // When the run has no masks, bind the source array in the mask slot; the
        // shader never samples it because every `has_mask` flag is zero.
        let mask_array = if any_mask {
            self.mask_array.as_ref().unwrap()
        } else {
            src_array
        };

        ctx.queue
            .write_buffer(&self.params_buffer, 0, bytemuck::cast_slice(params));

        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("ogre batch blend dispatch"),
            });

        for (i, src) in srcs.iter().enumerate() {
            copy_tile_into_slice(&mut encoder, src, src_array, i as u32);
            match (masks[i], params[i].has_mask != 0) {
                (Some(mask), true) => {
                    copy_tile_into_slice(&mut encoder, mask, mask_array, i as u32)
                }
                (None, false) => {}
                _ => panic!("masks[{i}] presence disagrees with params[{i}].has_mask"),
            }
        }

        let array_view_desc = wgpu::TextureViewDescriptor {
            label: Some("ogre batch array view"),
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        };
        let src_view = src_array.create_view(&array_view_desc);
        let mask_view = mask_array.create_view(&array_view_desc);
        let dst_view = dst.create_view(&wgpu::TextureViewDescriptor::default());

        let params_size = (n * std::mem::size_of::<LayerParam>()) as u64;
        let bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ogre batch blend bind group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&src_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&dst_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &self.params_buffer,
                        offset: 0,
                        size: Some(NonZeroU64::new(params_size).unwrap()),
                    }),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&mask_view),
                },
            ],
        });

        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("ogre batch blend pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.dispatch_workgroups(
                (dst.size().width).div_ceil(16),
                (dst.size().height).div_ceil(16),
                1,
            );
        }
        ctx.queue.submit(Some(encoder.finish()));
    }
}

const ADJUSTMENT_WGSL: &str = include_str!("shaders/adjustment.wgsl");

const ADJ_MODE_INVERT: u32 = 0;
const ADJ_MODE_DESATURATE: u32 = 1;
const ADJ_MODE_BRIGHTNESS_CONTRAST: u32 = 2;
const ADJ_MODE_HUE_SAT: u32 = 3;
const ADJ_MODE_LEVELS: u32 = 4;
const ADJ_MODE_CURVES: u32 = 5;
const ADJ_MODE_POSTERIZE: u32 = 6;
const ADJ_MODE_THRESHOLD: u32 = 7;
const ADJ_MODE_GRADIENT_MAP: u32 = 8;

/// Uniform sent to the adjustment compute shader.
///
/// The layout matches the WGSL `Params` struct exactly:
/// `mode` (4 bytes), `opacity` (4 bytes), `_pad` (8 bytes),
/// `param` (2 × vec4<f32> = 32 bytes). Total size is 48 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
struct AdjustmentUniform {
    mode: u32,
    opacity: f32,
    _pad: [f32; 2],
    param: [f32; 8],
}

fn adjustment_mode_and_params(kind: AdjustmentKind) -> (u32, [f32; 8]) {
    let mut param = [0.0f32; 8];
    let mode = match kind {
        AdjustmentKind::Invert => ADJ_MODE_INVERT,
        AdjustmentKind::Desaturate => ADJ_MODE_DESATURATE,
        AdjustmentKind::BrightnessContrast {
            brightness,
            contrast,
        } => {
            param[0] = brightness;
            param[1] = contrast;
            ADJ_MODE_BRIGHTNESS_CONTRAST
        }
        AdjustmentKind::HueSat {
            hue,
            saturation,
            lightness,
        } => {
            param[0] = hue / 360.0;
            param[1] = saturation;
            param[2] = lightness;
            ADJ_MODE_HUE_SAT
        }
        AdjustmentKind::Levels {
            input_black,
            input_white,
            output_black,
            output_white,
            gamma,
        } => {
            param[0] = input_black;
            param[1] = input_white;
            param[2] = output_black;
            param[3] = output_white;
            param[4] = gamma;
            ADJ_MODE_LEVELS
        }
        AdjustmentKind::Curves { points } => {
            param[0] = points[0].input;
            param[1] = points[0].output;
            param[2] = points[1].input;
            param[3] = points[1].output;
            param[4] = points[2].input;
            param[5] = points[2].output;
            param[6] = points[3].input;
            param[7] = points[3].output;
            ADJ_MODE_CURVES
        }
        AdjustmentKind::Posterize { levels } => {
            param[0] = levels.max(2) as f32;
            ADJ_MODE_POSTERIZE
        }
        AdjustmentKind::Threshold { level } => {
            param[0] = level;
            ADJ_MODE_THRESHOLD
        }
        AdjustmentKind::GradientMap { fg, bg } => {
            param[0] = fg.r;
            param[1] = fg.g;
            param[2] = fg.b;
            param[3] = fg.a;
            param[4] = bg.r;
            param[5] = bg.g;
            param[6] = bg.b;
            param[7] = bg.a;
            ADJ_MODE_GRADIENT_MAP
        }
    };
    (mode, param)
}

/// Cached compute pipeline and bind-group layout for in-place adjustment layers.
#[derive(Debug)]
pub struct AdjustmentPipelines {
    pipeline: wgpu::ComputePipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    uniform_buffer: wgpu::Buffer,
}

impl AdjustmentPipelines {
    /// Create the adjustment pipeline on `ctx`.
    pub fn new(ctx: &GpuContext) -> Self {
        let device = &ctx.device;
        let shader_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ogre adjustment shader"),
            source: wgpu::ShaderSource::Wgsl(ADJUSTMENT_WGSL.into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ogre adjustment bind group layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::ReadWrite,
                        format: wgpu::TextureFormat::Rgba32Float,
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ogre adjustment pipeline layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("ogre adjustment pipeline"),
            layout: Some(&pipeline_layout),
            module: &shader_module,
            entry_point: Some("main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            cache: None,
        });

        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ogre adjustment uniform"),
            size: std::mem::size_of::<AdjustmentUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            pipeline,
            bind_group_layout,
            uniform_buffer,
        }
    }

    /// Apply `kind` with `opacity` to `dst` in-place.
    ///
    /// `dst` must be an `Rgba32Float` storage texture with
    /// `wgpu::TextureUsages::STORAGE_BINDING`. The work is submitted to
    /// `ctx.queue` before this function returns.
    pub fn apply_adjustment_inplace(
        &self,
        ctx: &GpuContext,
        dst: &wgpu::Texture,
        kind: AdjustmentKind,
        opacity: f32,
    ) {
        let opacity = sanitize_opacity(opacity);
        let (mode, param) = adjustment_mode_and_params(kind);
        let uniform = AdjustmentUniform {
            mode,
            opacity,
            _pad: [0.0; 2],
            param,
        };
        ctx.queue
            .write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniform));

        let dst_view = dst.create_view(&wgpu::TextureViewDescriptor::default());

        let bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ogre adjustment bind group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&dst_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: self.uniform_buffer.as_entire_binding(),
                },
            ],
        });

        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("ogre adjustment dispatch"),
            });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("ogre adjustment pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.dispatch_workgroups(
                (dst.size().width).div_ceil(16),
                (dst.size().height).div_ceil(16),
                1,
            );
        }
        ctx.queue.submit(Some(encoder.finish()));
    }
}

#[cfg(test)]
mod tests {
    use ogre_core::{BlendMode, Rgba32F, Tile, TILE_SIZE};

    use super::*;
    use crate::tile_cache::{read_texture_to_vec, upload_tile};

    fn solid_tile(color: Rgba32F) -> Tile {
        let mut tile = Tile::transparent();
        for y in 0..TILE_SIZE {
            for x in 0..TILE_SIZE {
                tile.set(x, y, color);
            }
        }
        tile
    }

    fn solid_storage_texture(ctx: &GpuContext, color: Rgba32F) -> wgpu::Texture {
        let size = wgpu::Extent3d {
            width: TILE_SIZE as u32,
            height: TILE_SIZE as u32,
            depth_or_array_layers: 1,
        };
        let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("ogre blend test accumulator"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba32Float,
            usage: wgpu::TextureUsages::STORAGE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let pixels = vec![color; TILE_SIZE * TILE_SIZE];
        ctx.queue.write_texture(
            texture.as_image_copy(),
            bytemuck::cast_slice(&pixels),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(TILE_SIZE as u32 * 16),
                rows_per_image: Some(TILE_SIZE as u32),
            },
            size,
        );
        texture
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
    fn blend_shader_compiles() {
        let ctx = GpuContext::headless();
        let _pipelines = BlendPipelines::new(&ctx);
    }

    #[test]
    fn adjustment_shader_compiles() {
        let ctx = GpuContext::headless();
        let _pipelines = AdjustmentPipelines::new(&ctx);
    }

    #[test]
    fn normal_half_blue_over_red_matches_cpu_reference() {
        let ctx = GpuContext::headless();
        let pipelines = BlendPipelines::new(&ctx);

        let red = Rgba32F::new(1.0, 0.0, 0.0, 1.0);
        let blue = Rgba32F::new(0.0, 0.0, 1.0, 0.5);

        let src_texture = upload_tile(&ctx, &solid_tile(blue));
        let dst_texture = solid_storage_texture(&ctx, red);

        pipelines.composite_tile_over(
            &ctx,
            &src_texture,
            &dst_texture,
            BlendMode::Normal,
            1.0,
            IVec2::ZERO,
            None,
        );

        let gpu_pixels = read_texture_to_vec(&ctx.device, &ctx.queue, &dst_texture);
        let expected = ogre_core::compositor::blend_pixel(BlendMode::Normal, red, blue, 1.0);

        for px in &gpu_pixels {
            assert_pixels_approx_eq(*px, expected, 1e-4);
        }
    }

    #[test]
    fn blend_uniform_nan_opacity_is_sanitized_to_zero() {
        let u = BlendUniform::new(BlendMode::Normal, f32::NAN, IVec2::new(10, -20));
        assert_eq!(u.opacity, 0.0);
    }

    #[test]
    fn nan_source_alpha_leaves_destination_unchanged() {
        let ctx = GpuContext::headless();
        let pipelines = BlendPipelines::new(&ctx);

        let red = Rgba32F::new(1.0, 0.0, 0.0, 1.0);
        let src = Rgba32F::new(0.0, 0.0, 1.0, f32::NAN);

        let src_texture = upload_tile(&ctx, &solid_tile(src));
        let dst_texture = solid_storage_texture(&ctx, red);

        pipelines.composite_tile_over(
            &ctx,
            &src_texture,
            &dst_texture,
            BlendMode::Normal,
            1.0,
            IVec2::ZERO,
            None,
        );

        let gpu_pixels = read_texture_to_vec(&ctx.device, &ctx.queue, &dst_texture);
        for px in &gpu_pixels {
            assert_pixels_approx_eq(*px, red, 1e-4);
        }
    }

    #[test]
    fn nan_destination_alpha_treated_as_zero() {
        let ctx = GpuContext::headless();
        let pipelines = BlendPipelines::new(&ctx);

        let dst = Rgba32F::new(1.0, 0.0, 0.0, f32::NAN);
        let src = Rgba32F::new(0.0, 0.0, 1.0, 1.0);

        let src_texture = upload_tile(&ctx, &solid_tile(src));
        let dst_texture = solid_storage_texture(&ctx, dst);

        pipelines.composite_tile_over(
            &ctx,
            &src_texture,
            &dst_texture,
            BlendMode::Normal,
            1.0,
            IVec2::ZERO,
            None,
        );

        let gpu_pixels = read_texture_to_vec(&ctx.device, &ctx.queue, &dst_texture);
        let expected = ogre_core::compositor::blend_pixel(BlendMode::Normal, dst, src, 1.0);
        for px in &gpu_pixels {
            assert_pixels_approx_eq(*px, expected, 1e-4);
        }
    }

    /// One varied contribution used by the batch-vs-sequential test.
    struct Contribution {
        color: Rgba32F,
        mode: BlendMode,
        opacity: f32,
        offset: IVec2,
        mask: Option<Rgba32F>,
    }

    #[test]
    fn batch_run_matches_sequential_composite() {
        let ctx = GpuContext::headless();
        let single = BlendPipelines::new(&ctx);
        let mut batch = BatchBlendPipeline::new(&ctx);

        // A non-trivial ordered stack: varied blend modes, opacities, offsets,
        // and an interleaved masked contribution. The two paths must agree.
        let contribs = [
            Contribution {
                color: Rgba32F::new(0.0, 1.0, 0.0, 0.5),
                mode: BlendMode::Multiply,
                opacity: 0.75,
                offset: IVec2::new(10, -5),
                mask: None,
            },
            Contribution {
                color: Rgba32F::new(0.0, 0.0, 1.0, 0.5),
                mode: BlendMode::Screen,
                opacity: 0.5,
                offset: IVec2::new(-20, 30),
                mask: Some(Rgba32F::new(0.5, 0.0, 0.0, 1.0)),
            },
            Contribution {
                color: Rgba32F::new(1.0, 1.0, 0.0, 1.0),
                mode: BlendMode::Overlay,
                opacity: 1.0,
                offset: IVec2::ZERO,
                mask: None,
            },
            Contribution {
                color: Rgba32F::new(0.2, 0.4, 0.6, 0.8),
                mode: BlendMode::SoftLight,
                opacity: 0.9,
                offset: IVec2::new(5, 5),
                mask: None,
            },
        ];

        // A non-empty initial accumulator exercises the "compose over existing
        // destination content" case (as after a group pop).
        let bg = Rgba32F::new(1.0, 0.0, 0.0, 1.0);
        let seq_dst = solid_storage_texture(&ctx, bg);
        let batch_dst = solid_storage_texture(&ctx, bg);

        let src_textures: Vec<wgpu::Texture> = contribs
            .iter()
            .map(|c| upload_tile(&ctx, &solid_tile(c.color)))
            .collect();
        let mask_textures: Vec<Option<wgpu::Texture>> = contribs
            .iter()
            .map(|c| c.mask.map(|m| upload_tile(&ctx, &solid_tile(m))))
            .collect();

        for (i, c) in contribs.iter().enumerate() {
            single.composite_tile_over(
                &ctx,
                &src_textures[i],
                &seq_dst,
                c.mode,
                c.opacity,
                c.offset,
                mask_textures[i].as_ref(),
            );
        }

        let srcs: Vec<&wgpu::Texture> = src_textures.iter().collect();
        let masks: Vec<Option<&wgpu::Texture>> = mask_textures.iter().map(Option::as_ref).collect();
        let params: Vec<LayerParam> = contribs
            .iter()
            .map(|c| LayerParam::new(c.mode, c.opacity, c.offset, c.mask.is_some()))
            .collect();
        batch.composite_run(&ctx, &batch_dst, &srcs, &masks, &params);

        let seq = read_texture_to_vec(&ctx.device, &ctx.queue, &seq_dst);
        let bat = read_texture_to_vec(&ctx.device, &ctx.queue, &batch_dst);
        assert_eq!(seq.len(), bat.len());
        for (a, b) in seq.iter().zip(bat.iter()) {
            assert_pixels_approx_eq(*a, *b, 1e-4);
        }
    }
}
