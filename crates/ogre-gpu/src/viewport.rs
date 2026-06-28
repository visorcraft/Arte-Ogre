// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Viewport transform, readback, and on-screen presentation pass.
//!
//! This module provides the bridge between document-space pixels and screen-space
//! coordinates, a helper to read back an arbitrary rectangular region from the GPU
//! compositor, and a render pass that draws the composited result over a
//! checkerboard background.

use bytemuck::{Pod, Zeroable};
use glam::{UVec2, Vec2};
use ogre_core::{Rect, Rgba32F, TILE_SIZE};

use crate::compositor::Compositor;
use crate::context::GpuContext;

const PRESENT_WGSL: &str = include_str!("shaders/present.wgsl");

/// Viewport transform mapping document-space pixels to screen-space pixels.
///
/// `pan` is the document coordinate currently displayed at the top-left corner
/// of the canvas. `zoom` scales document pixels to screen pixels: a zoom of
/// `1.0` maps one document pixel to one screen pixel; a zoom of `2.0` maps one
/// document pixel to a `2×2` screen-pixel block.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Viewport {
    /// Document-space point at the top-left of the canvas.
    pub pan: Vec2,
    /// Scale factor from document pixels to screen pixels.
    pub zoom: f32,
}

impl Viewport {
    /// Minimum positive zoom value.
    ///
    /// The value is large enough that, combined with
    /// [`Self::MAX_VISIBLE_DOC_DIMENSION`], an extreme zoom-out cannot enumerate an
    /// OOM-scale number of tiles.
    pub const MIN_ZOOM: f32 = 1e-3;

    /// Maximum width or height (in document pixels) returned by
    /// [`Self::visible_doc_region`].
    ///
    /// This bounds the number of tiles the GPU compositor is asked to consider
    /// on a single frame, preventing an accidental extreme zoom-out from
    /// exhausting memory. The value is chosen to preserve normal interactive
    /// zoom ranges while still covering a large canvas area.
    pub const MAX_VISIBLE_DOC_DIMENSION: u32 = 1 << 16; // 65536 px

    /// Create a new viewport.
    ///
    /// Negative or zero zoom values are clamped to [`Self::MIN_ZOOM`] so that
    /// `screen_to_doc` is always well-defined and the visible document region
    /// cannot grow without bound.
    pub fn new(pan: Vec2, zoom: f32) -> Self {
        Self {
            pan,
            zoom: zoom.max(Self::MIN_ZOOM),
        }
    }

    /// Map a document coordinate (in pixels) to screen coordinates relative to
    /// the canvas.
    ///
    /// `canvas_origin` is the screen-space top-left corner of the canvas;
    /// `canvas_size` is its size in pixels. `canvas_size` is not used by the
    /// current top-left anchoring convention but is included in the signature
    /// so callers can switch to a center-anchored convention later without
    /// breaking the API.
    pub fn doc_to_screen(&self, doc_pos: Vec2, canvas_origin: Vec2, _canvas_size: Vec2) -> Vec2 {
        canvas_origin + (doc_pos - self.pan) * self.zoom
    }

    /// Map a screen coordinate to a document coordinate.
    ///
    /// `canvas_origin` is the screen-space top-left corner of the canvas;
    /// `canvas_size` is its size in pixels. This is the inverse of
    /// [`Self::doc_to_screen`].
    pub fn screen_to_doc(&self, screen_pos: Vec2, canvas_origin: Vec2, _canvas_size: Vec2) -> Vec2 {
        self.pan + (screen_pos - canvas_origin) / self.zoom
    }

    /// Compute the document region (in pixels) visible through a viewport of the
    /// given screen size.
    ///
    /// The returned rectangle is rounded **out** to integer pixel boundaries so
    /// that every screen fragment that could sample the document is covered. It
    /// is not clipped to the document canvas; callers should intersect with
    /// [`Document::canvas`](ogre_core::Document::canvas) before compositing.
    pub fn visible_doc_region(&self, screen_size: UVec2) -> Rect {
        let origin = Vec2::ZERO;
        let size = screen_size.as_vec2();
        let min_doc = self.screen_to_doc(origin, origin, size);
        let max_doc = self.screen_to_doc(size, origin, size);

        let min_x = min_doc.x.floor() as i64;
        let min_y = min_doc.y.floor() as i64;
        let max_x = max_doc.x.ceil() as i64;
        let max_y = max_doc.y.ceil() as i64;

        let mut x = min_x.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
        let mut y = min_y.clamp(i32::MIN as i64, i32::MAX as i64) as i32;

        let raw_w = (max_x - min_x).max(0) as u32;
        let raw_h = (max_y - min_y).max(0) as u32;

        let w = raw_w.min(Self::MAX_VISIBLE_DOC_DIMENSION);
        let h = raw_h.min(Self::MAX_VISIBLE_DOC_DIMENSION);

        // When clamping, keep the visible region centered on the un-clamped
        // area so pan/zoom behavior stays intuitive at extreme zoom-outs.
        if raw_w > Self::MAX_VISIBLE_DOC_DIMENSION {
            let shift = (raw_w as i64 - Self::MAX_VISIBLE_DOC_DIMENSION as i64) / 2;
            x = ((x as i64) + shift).clamp(i32::MIN as i64, i32::MAX as i64) as i32;
        }
        if raw_h > Self::MAX_VISIBLE_DOC_DIMENSION {
            let shift = (raw_h as i64 - Self::MAX_VISIBLE_DOC_DIMENSION as i64) / 2;
            y = ((y as i64) + shift).clamp(i32::MIN as i64, i32::MAX as i64) as i32;
        }

        Rect::new(x, y, w, h)
    }
}

impl Compositor {
    /// Read back an arbitrary region from the cached compositor result.
    ///
    /// The returned vector has length `region.w * region.h` in row-major order;
    /// the pixel at document coordinate `(x, y)` is at index
    /// `(y - region.y) * region.w + (x - region.x)`. Tiles that have not been
    /// composited are treated as fully transparent.
    ///
    /// This helper blocks on the GPU and is intended for tests, export, and
    /// headless rendering.
    pub fn read_region(&self, ctx: &GpuContext, region: Rect) -> Vec<Rgba32F> {
        let width = region.w as usize;
        let height = region.h as usize;
        let mut out = vec![Rgba32F::TRANSPARENT; width * height];

        for tile in region.tiles_covered() {
            let Some(tile_pixels) = self.read_result_tile(ctx, tile) else {
                continue;
            };

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

            for y in inter.y..inter.bottom() as i32 {
                for x in inter.x..inter.right() as i32 {
                    let local_x = (x as i64 - tile_origin_x) as usize;
                    let local_y = (y as i64 - tile_origin_y) as usize;
                    let out_x = (x - region.x) as usize;
                    let out_y = (y - region.y) as usize;
                    out[out_y * width + out_x] = tile_pixels[local_y * TILE_SIZE + local_x];
                }
            }
        }

        out
    }
}

/// Uniform data sent to the present shader.
///
/// Matches the WGSL `ViewportUniform` struct exactly. Fields are arranged to
/// satisfy WGSL's 16-byte alignment rules.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
struct PresentUniform {
    pan: Vec2,
    source_origin: Vec2,
    canvas_origin: Vec2,
    canvas_size: Vec2,
    zoom: f32,
    // Pad to 48 bytes so the Rust size matches the WGSL uniform's 16-byte
    // rounded size (the buffer binding must be at least that large).
    _padding: [f32; 3],
}

/// Render pipeline that draws a composited result texture to a viewport-sized
/// output, compositing it over a checkerboard background.
#[derive(Debug)]
pub struct PresentPipeline {
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    uniform_buffer: wgpu::Buffer,
    nearest_sampler: wgpu::Sampler,
    linear_sampler: Option<wgpu::Sampler>,
    filterable: bool,
}

impl PresentPipeline {
    /// Create the cached present pipeline on `ctx`.
    ///
    /// The pipeline samples from an `Rgba32Float` source texture and writes to
    /// an `Rgba8Unorm` target. sRGB encoding is performed in the fragment
    /// shader so the resulting texture can be registered as an `egui_wgpu` user
    /// texture (`egui_wgpu` requires `Rgba8Unorm`).
    ///
    /// Linear filtering is only enabled if the device supports
    /// [`wgpu::Features::FLOAT32_FILTERABLE`]; otherwise nearest-neighbor
    /// sampling is used for all zoom levels.
    pub fn new(ctx: &GpuContext) -> Self {
        let device = &ctx.device;
        let shader_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ogre present shader"),
            source: wgpu::ShaderSource::Wgsl(PRESENT_WGSL.into()),
        });

        let filterable = device
            .features()
            .contains(wgpu::Features::FLOAT32_FILTERABLE);

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ogre present bind group layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(if filterable {
                        wgpu::SamplerBindingType::Filtering
                    } else {
                        wgpu::SamplerBindingType::NonFiltering
                    }),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
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
            label: Some("ogre present pipeline layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("ogre present pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader_module,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader_module,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ogre present uniform"),
            size: std::mem::size_of::<PresentUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let nearest_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("ogre present nearest sampler"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let linear_sampler = filterable.then(|| {
            device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("ogre present linear sampler"),
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            })
        });

        Self {
            pipeline,
            bind_group_layout,
            uniform_buffer,
            nearest_sampler,
            linear_sampler,
            filterable,
        }
    }

    /// Render `source` into `output` using `viewport`.
    ///
    /// The `source` view must be an `Rgba32Float` 2D texture covering
    /// `source_region` in document coordinates. `canvas` is the document canvas
    /// rectangle in document coordinates; pixels outside it are filled with a
    /// flat pasteboard color instead of the transparency checkerboard. The
    /// `output` view must use `Rgba8Unorm` (sRGB
    /// encoding is applied in the shader). The draw is a single fullscreen
    /// triangle and the work is submitted to `ctx.queue` before this function
    /// returns.
    ///
    /// Nearest-neighbor sampling is used when `viewport.zoom >= 1.0` for crisp
    /// pixels; linear sampling is used when `zoom < 1.0` and the device
    /// supports filtering `Rgba32Float` textures.
    pub fn render(
        &self,
        ctx: &GpuContext,
        source: &wgpu::TextureView,
        viewport: &Viewport,
        source_region: Rect,
        canvas: Rect,
        output: &wgpu::TextureView,
    ) {
        let uniform = PresentUniform {
            pan: viewport.pan,
            source_origin: Vec2::new(source_region.x as f32, source_region.y as f32),
            canvas_origin: Vec2::new(canvas.x as f32, canvas.y as f32),
            canvas_size: Vec2::new(canvas.w as f32, canvas.h as f32),
            zoom: viewport.zoom,
            _padding: [0.0; 3],
        };
        ctx.queue
            .write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniform));

        let sampler = if self.filterable && viewport.zoom < 1.0 {
            self.linear_sampler
                .as_ref()
                .expect("linear sampler exists when filterable")
        } else {
            &self.nearest_sampler
        };

        let bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ogre present bind group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(source),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: self.uniform_buffer.as_entire_binding(),
                },
            ],
        });

        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("ogre present pass"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("ogre present pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: output,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                ..Default::default()
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        ctx.queue.submit(Some(encoder.finish()));
    }
}

/// Read back the raw bytes of an `Rgba8UnormSrgb` texture.
#[cfg(test)]
fn read_rgba8_texture_to_vec(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
) -> Vec<u8> {
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
        label: Some("present readback"),
        size: buffer_size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("present readback"),
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
    let mut out = Vec::with_capacity(width * height * bytes_per_pixel);
    for row in 0..height {
        let start = row * padded_bytes_per_row;
        let row_bytes = &data[start..start + unpadded_bytes_per_row];
        out.extend_from_slice(row_bytes);
    }
    drop(data);
    buffer.unmap();

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::UVec2;
    use ogre_core::{BlendMode, Document, IVec2};

    use crate::context::GpuContext;

    const EPSILON: f32 = 1e-4;

    fn fill_rect(buffer: &mut ogre_core::TiledBuffer, rect: Rect, color: Rgba32F) {
        let area = (rect.w as usize)
            .checked_mul(rect.h as usize)
            .expect("rect too large");
        let data = vec![color; area];
        buffer.blit_rect(rect, &data);
    }

    fn create_rgba32float_texture(ctx: &GpuContext, size: UVec2, fill: Rgba32F) -> wgpu::Texture {
        let extent = wgpu::Extent3d {
            width: size.x,
            height: size.y,
            depth_or_array_layers: 1,
        };
        let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("present source"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba32Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let pixels = vec![fill; (size.x * size.y) as usize];
        ctx.queue.write_texture(
            texture.as_image_copy(),
            bytemuck::cast_slice(&pixels),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(size.x * 16),
                rows_per_image: Some(size.y),
            },
            extent,
        );
        texture
    }

    fn create_rgba8_output_texture(ctx: &GpuContext, size: UVec2) -> wgpu::Texture {
        ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("present output"),
            size: wgpu::Extent3d {
                width: size.x,
                height: size.y,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        })
    }

    #[test]
    fn read_region_matches_cpu_reference() {
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
            Rgba32F::new(0.0, 0.0, 1.0, 0.5),
        );
        doc.layer_mut(fg).unwrap().offset = IVec2::new(30, 30);
        doc.layer_mut(fg).unwrap().blend = BlendMode::Multiply;
        doc.layer_mut(fg).unwrap().opacity = 0.75;

        let mut compositor = Compositor::new(&ctx);
        compositor
            .composite(&ctx, &doc, Rect::new(0, 0, 512, 512))
            .unwrap();

        let gpu = compositor.read_region(&ctx, Rect::new(0, 0, 512, 512));
        let cpu =
            ogre_core::compositor::composite_document(&doc, Rect::new(0, 0, 512, 512)).unwrap();

        assert_eq!(gpu.len(), cpu.len());
        for (i, (a, b)) in gpu.iter().zip(cpu.iter()).enumerate() {
            assert!(
                (a.r - b.r).abs() < EPSILON
                    && (a.g - b.g).abs() < EPSILON
                    && (a.b - b.b).abs() < EPSILON
                    && (a.a - b.a).abs() < EPSILON,
                "pixel {} differs: got {:?}, expected {:?}",
                i,
                a,
                b
            );
        }
    }

    #[test]
    fn screen_doc_roundtrip() {
        let vp = Viewport::new(Vec2::new(12.5, -7.0), 2.5);
        let origin = Vec2::new(100.0, 200.0);
        let size = Vec2::new(640.0, 480.0);
        let doc = Vec2::new(55.0, 33.0);
        let screen = vp.doc_to_screen(doc, origin, size);
        let doc2 = vp.screen_to_doc(screen, origin, size);
        assert!(
            (doc - doc2).length() < 1e-4,
            "round-trip failed: {:?} -> {:?}",
            doc,
            doc2
        );
    }

    #[test]
    fn top_left_maps_to_origin() {
        let vp = Viewport::new(Vec2::ZERO, 1.0);
        let origin = Vec2::new(100.0, 200.0);
        let size = Vec2::new(640.0, 480.0);
        let doc = vp.screen_to_doc(origin, origin, size);
        assert!((doc - Vec2::ZERO).length() < 1e-4);
    }

    #[test]
    fn zoom_scales_screen_to_doc() {
        let vp = Viewport::new(Vec2::ZERO, 2.0);
        let origin = Vec2::ZERO;
        let size = Vec2::new(100.0, 100.0);
        let screen = Vec2::new(20.0, 30.0);
        let doc = vp.screen_to_doc(screen, origin, size);
        let expected = Vec2::new(10.0, 15.0);
        assert!(
            (doc - expected).length() < 1e-4,
            "got {:?}, expected {:?}",
            doc,
            expected
        );
    }

    #[test]
    fn visible_doc_region_rounds_out_and_accounts_for_zoom() {
        // Pan at a fractional coordinate; zoom 0.5 means each screen pixel covers
        // two document pixels, so a 100x100 screen shows a 200x200 doc region.
        let vp = Viewport::new(Vec2::new(10.3, 20.7), 0.5);
        let region = vp.visible_doc_region(UVec2::new(100, 100));
        assert_eq!(region.x, 10);
        assert_eq!(region.y, 20);
        // ceil(10.3 + 100 / 0.5) = ceil(210.3) = 211, width = 211 - 10 = 201.
        assert_eq!(region.w, 201);
        // ceil(20.7 + 100 / 0.5) = ceil(220.7) = 221, height = 221 - 20 = 201.
        assert_eq!(region.h, 201);
    }

    #[test]
    fn visible_doc_region_with_zoom_greater_than_one() {
        // Zoom 2.0 means a 100x100 screen shows a 50x50 doc region.
        let vp = Viewport::new(Vec2::new(5.0, 5.0), 2.0);
        let region = vp.visible_doc_region(UVec2::new(100, 100));
        assert_eq!(region, Rect::new(5, 5, 50, 50));
    }

    #[test]
    fn extreme_zoom_out_clamps_visible_region_dimensions() {
        // A tiny zoom value combined with a non-trivial screen size would
        // otherwise enumerate an OOM-scale number of tiles. The viewport must
        // bound the returned document region.
        let vp = Viewport::new(Vec2::new(0.0, 0.0), 1e-6);
        let region = vp.visible_doc_region(UVec2::new(1024, 1024));
        assert!(
            region.w <= 100_000,
            "width {} should be clamped to a reasonable maximum",
            region.w
        );
        assert!(
            region.h <= 100_000,
            "height {} should be clamped to a reasonable maximum",
            region.h
        );
        assert!(!region.is_empty(), "clamped region must still be non-empty");
    }

    #[test]
    fn transparent_source_renders_checkerboard() {
        let ctx = GpuContext::headless();
        let pipeline = PresentPipeline::new(&ctx);

        let size = UVec2::new(64, 64);
        let source = create_rgba32float_texture(&ctx, size, Rgba32F::TRANSPARENT);
        let output = create_rgba8_output_texture(&ctx, size);

        pipeline.render(
            &ctx,
            &source.create_view(&wgpu::TextureViewDescriptor::default()),
            &Viewport::new(Vec2::ZERO, 1.0),
            Rect::new(0, 0, 64, 64),
            Rect::new(0, 0, 64, 64),
            &output.create_view(&wgpu::TextureViewDescriptor::default()),
        );

        // Verify the source is actually transparent before presenting.
        let source_pixels =
            crate::tile_cache::read_texture_to_vec(&ctx.device, &ctx.queue, &source);
        assert_eq!(source_pixels[0], Rgba32F::TRANSPARENT);

        let bytes = read_rgba8_texture_to_vec(&ctx.device, &ctx.queue, &output);
        assert_eq!(bytes.len(), 64 * 64 * 4);

        // The checkerboard alternates every CHECKER_SIZE (16) pixels between
        // white and a LIGHT GREY (linear 0.8 → sRGB ≈ 231), so a transparent
        // hole reads as "empty", never as a black fill.
        let cell = |x: usize, y: usize| {
            let i = (y * 64 + x) * 4;
            [bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]
        };
        // (0, 0) and (16, 16): even parity -> white.
        assert_eq!(cell(0, 0), [255, 255, 255, 255]);
        assert_eq!(cell(16, 16), [255, 255, 255, 255]);
        // (16, 0) and (0, 16): odd parity -> a light grey (never black).
        for (x, y) in [(16, 0), (0, 16)] {
            let [r, g, b, a] = cell(x, y);
            assert_eq!(a, 255);
            assert_eq!(r, g);
            assert_eq!(g, b);
            assert!(
                (215..=245).contains(&r),
                "checker grey square at ({x},{y}) should be a light grey, got {r}"
            );
        }
    }

    #[test]
    fn opaque_pixel_renders_unchanged() {
        let ctx = GpuContext::headless();
        let pipeline = PresentPipeline::new(&ctx);

        let size = UVec2::new(64, 64);
        let source = create_rgba32float_texture(&ctx, size, Rgba32F::TRANSPARENT);
        let output = create_rgba8_output_texture(&ctx, size);

        // Paint an opaque red pixel at (10, 15).
        let red = Rgba32F::new(1.0, 0.0, 0.0, 1.0);
        ctx.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &source,
                mip_level: 0,
                origin: wgpu::Origin3d { x: 10, y: 15, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::bytes_of(&red),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(size.x * 16),
                rows_per_image: Some(size.y),
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );

        pipeline.render(
            &ctx,
            &source.create_view(&wgpu::TextureViewDescriptor::default()),
            &Viewport::new(Vec2::ZERO, 1.0),
            Rect::new(0, 0, 64, 64),
            Rect::new(0, 0, 64, 64),
            &output.create_view(&wgpu::TextureViewDescriptor::default()),
        );

        let bytes = read_rgba8_texture_to_vec(&ctx.device, &ctx.queue, &output);
        let idx = (15 * 64 + 10) * 4;
        assert_eq!(&bytes[idx..idx + 4], &[255, 0, 0, 255]);
    }

    #[test]
    fn half_transparent_red_composites_correctly_over_checkerboard() {
        let ctx = GpuContext::headless();
        let pipeline = PresentPipeline::new(&ctx);

        let size = UVec2::new(64, 64);
        // Source is straight-alpha 50% opaque red with RGB = 0.5. Over the light
        // grey checker square (linear 0.8) at (16, 0), the straight-alpha result
        // is 0.5*0.5 + 0.8*0.5 = 0.65 red and 0.0*0.5 + 0.8*0.5 = 0.4 green/blue.
        // The buggy premultiplied formulation would add the full source red (0.5)
        // and produce 0.9 red, distinguishing the two paths.
        let src = Rgba32F::new(0.5, 0.0, 0.0, 0.5);
        let source = create_rgba32float_texture(&ctx, size, src);
        let output = create_rgba8_output_texture(&ctx, size);

        pipeline.render(
            &ctx,
            &source.create_view(&wgpu::TextureViewDescriptor::default()),
            &Viewport::new(Vec2::ZERO, 1.0),
            Rect::new(0, 0, 64, 64),
            Rect::new(0, 0, 64, 64),
            &output.create_view(&wgpu::TextureViewDescriptor::default()),
        );

        let bytes = read_rgba8_texture_to_vec(&ctx.device, &ctx.queue, &output);
        // (16, 0) is an odd parity -> light-grey checker square.
        let idx = 16usize * 4;
        let r = bytes[idx];
        let g = bytes[idx + 1];
        let b = bytes[idx + 2];

        // Linear 0.65 → sRGB ≈ 211 (straight alpha). The buggy premultiplied path
        // would give linear 0.9 → sRGB ≈ 244, so the range excludes it.
        assert!(
            (201..=225).contains(&r),
            "expected red channel ~211 (sRGB of linear 0.65), got {}",
            r
        );
        // Green/blue come purely from the grey checker (linear 0.4 → sRGB ≈ 169).
        assert!(
            (159..=179).contains(&g),
            "expected green channel ~169 (sRGB of linear 0.4), got {}",
            g
        );
        assert_eq!(g, b, "green and blue should match (grey checker)");
        assert_eq!(bytes[idx + 3], 255);
    }

    #[test]
    fn outside_canvas_is_pasteboard_not_checkerboard() {
        // Regression: when the view is larger than the document canvas, pixels
        // beyond the canvas must be flat pasteboard, not an extended
        // checkerboard (which made the canvas look infinitely large).
        let ctx = GpuContext::headless();
        let pipeline = PresentPipeline::new(&ctx);

        let view = UVec2::new(64, 64);
        // A 16x16 canvas at the origin, viewed at zoom 1 with no pan: only the
        // top-left 16x16 of the 64x64 output is canvas; the rest is pasteboard.
        let canvas = Rect::new(0, 0, 16, 16);
        let source = create_rgba32float_texture(&ctx, UVec2::new(16, 16), Rgba32F::TRANSPARENT);
        let output = create_rgba8_output_texture(&ctx, view);

        pipeline.render(
            &ctx,
            &source.create_view(&wgpu::TextureViewDescriptor::default()),
            &Viewport::new(Vec2::ZERO, 1.0),
            canvas,
            canvas,
            &output.create_view(&wgpu::TextureViewDescriptor::default()),
        );

        let bytes = read_rgba8_texture_to_vec(&ctx.device, &ctx.queue, &output);
        let px = |x: usize, y: usize| {
            let i = (y * view.x as usize + x) * 4;
            [bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]
        };
        // sRGB byte value of PASTEBOARD (0.15) written straight to the target.
        let pb = (0.15f32 * 255.0).round() as u8;

        // Several points well outside the 16x16 canvas: all flat pasteboard.
        for (x, y) in [(40, 5), (5, 40), (40, 40), (63, 63)] {
            assert_eq!(
                px(x, y),
                [pb, pb, pb, 255],
                "({x},{y}) should be pasteboard {pb}, got {:?}",
                px(x, y)
            );
        }
        // Inside the canvas over a transparent source: still the checkerboard
        // (top-left 16x16 cell is white).
        assert_eq!(px(0, 0), [255, 255, 255, 255], "inside canvas → checker");
    }
}
