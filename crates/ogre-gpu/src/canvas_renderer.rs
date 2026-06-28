// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Public `CanvasRenderer` API and optional `egui` interop.
//!
//! [`CanvasRenderer`] is the bridge between `ogre-core` documents and the GPU
//! display. It composites a document into a viewport-sized output texture that
//! can be read back for tests/export or registered with `egui_wgpu` for
//! zero-copy display.

#[cfg(feature = "egui")]
use std::cell::Cell;

use glam::UVec2;
use ogre_core::{Document, Rect, Rgba32F, TileCoord, TILE_SIZE};

use crate::compositor::Compositor;
use crate::context::GpuContext;
use crate::tile_cache::read_texture_to_vec;
use crate::viewport::{PresentPipeline, Viewport};

/// GPU renderer that turns an `ogre-core` [`Document`] into a viewport-sized
/// display texture.
///
/// `CanvasRenderer` is designed to live on a *shared* `wgpu::Device`/`Queue`
/// (e.g. the one owned by `eframe`). Construction through
/// [`CanvasRenderer::new`] wraps the shared device via
/// [`GpuContext::from_shared`], validating that the device supports the format
/// capabilities the compositor needs.
///
/// The renderer composites only the tiles visible through the current
/// [`Viewport`], assembles the result tiles into a single linear `Rgba32Float`
/// source texture, and then runs the present pass to produce an `Rgba8Unorm`
/// output texture. sRGB encoding is performed in the present shader so the
/// output texture can be registered directly with `egui_wgpu`.
pub struct CanvasRenderer {
    ctx: GpuContext,
    compositor: Compositor,
    viewport: Viewport,
    present: PresentPipeline,
    /// Assembled linear-space composited result covering the last rendered
    /// viewport-visible region.
    source: Option<wgpu::Texture>,
    /// Region that `source` covers, in document coordinates.
    source_region: Rect,
    /// Viewport-sized output texture in `Rgba8Unorm`.
    output: Option<wgpu::Texture>,
    /// Size of the current output texture, in pixels.
    output_size: UVec2,
    /// Output texture view, recreated whenever `output` is resized.
    output_view: Option<wgpu::TextureView>,
    #[cfg(feature = "egui")]
    egui_texture_id: Cell<Option<egui::TextureId>>,
}

impl CanvasRenderer {
    /// Construct a renderer on a shared device/queue supplied by `eframe`.
    ///
    /// # Panics
    ///
    /// Panics if `device` does not have the format capabilities required by the
    /// compositor. See [`GpuContext::from_shared`] for details.
    pub fn new(device: wgpu::Device, queue: wgpu::Queue, info: wgpu::AdapterInfo) -> Self {
        let ctx = GpuContext::from_shared(device, queue, info);
        let compositor = Compositor::new(&ctx);
        let present = PresentPipeline::new(&ctx);

        Self {
            ctx,
            compositor,
            viewport: Viewport::new(glam::Vec2::ZERO, 1.0),
            present,
            source: None,
            source_region: Rect::new(0, 0, 0, 0),
            output: None,
            output_size: UVec2::ZERO,
            output_view: None,
            #[cfg(feature = "egui")]
            egui_texture_id: Cell::new(None),
        }
    }

    /// Return the shared `wgpu::Device` used by this renderer.
    ///
    /// This is the same device that was passed to [`CanvasRenderer::new`]. It is
    /// exposed so callers can pass it to `CanvasRenderer::register_with_egui`.
    pub fn device(&self) -> &wgpu::Device {
        &self.ctx.device
    }

    /// Return the GPU context used by this renderer.
    pub fn context(&self) -> &GpuContext {
        &self.ctx
    }

    /// Replace the current viewport transform.
    pub fn set_viewport(&mut self, vp: Viewport) {
        self.viewport = vp;
    }

    /// Drop all cached compositor tiles and the assembled source/output textures.
    ///
    /// Call this whenever the document is replaced so GPU memory from the
    /// previous document is not retained. The egui texture id is left stable;
    /// [`register_with_egui`](Self::register_with_egui) will rebind it when a
    /// new output texture is rendered.
    pub fn clear_caches(&mut self) {
        self.compositor.clear_caches();
        self.source = None;
        self.source_region = Rect::new(0, 0, 0, 0);
        self.output = None;
        self.output_size = UVec2::ZERO;
        self.output_view = None;
    }

    /// Composite `doc` for the current viewport into an internal output texture
    /// and return its view.
    ///
    /// The output texture is resized to `view_size_px` whenever it changes. The
    /// returned view remains valid until the next call to `render` (or until
    /// this `CanvasRenderer` is dropped).
    ///
    /// Only the document region visible through the current [`Viewport`] is
    /// composited; the present pass then maps document coordinates to screen
    /// coordinates. The composited region is available from
    /// [`composite_region`](Self::composite_region).
    ///
    /// # Panics
    ///
    /// Panics if the document's layer tree contains a cycle, which is treated
    /// as an unrecoverable error for the renderer.
    pub fn render(&mut self, doc: &Document, view_size_px: UVec2) -> &wgpu::TextureView {
        let visible_region = self.viewport.visible_doc_region(view_size_px);
        let region = visible_region
            .intersect(doc.canvas)
            .unwrap_or(Rect::new(0, 0, 0, 0));

        self.compositor
            .composite(&self.ctx, doc, region)
            .expect("layer tree contains a cycle");

        self.ensure_source_texture(region);
        self.source_region = region;
        {
            let source = self.source.as_ref().unwrap();
            self.copy_result_tiles_to_source(region, source);
        }

        self.ensure_output_texture(view_size_px);
        let output = self.output.as_ref().unwrap();
        let output_view = output.create_view(&wgpu::TextureViewDescriptor::default());
        let source_view = self
            .source
            .as_ref()
            .unwrap()
            .create_view(&wgpu::TextureViewDescriptor::default());

        self.present.render(
            &self.ctx,
            &source_view,
            &self.viewport,
            self.source_region,
            doc.canvas,
            &output_view,
        );

        self.output_view = Some(output_view);
        self.output_view.as_ref().unwrap()
    }

    /// Read back the linear-space composited result for the region rendered
    /// during the last `render` call.
    ///
    /// This returns the assembled `Rgba32Float` source texture, **not** the
    /// displayed `Rgba8Unorm` output. Reading the source preserves float
    /// precision, which is required for golden tests against the CPU reference
    /// compositor. The region covered by the returned pixels is available from
    /// [`composite_region`](Self::composite_region).
    ///
    /// The returned vector has length `composite_region().w * composite_region().h`
    /// in row-major order. If `render` has never been called, an empty vector is
    /// returned.
    pub fn read_output(&self) -> Vec<Rgba32F> {
        if self.source_region.is_empty() {
            return Vec::new();
        }
        let Some(source) = &self.source else {
            return Vec::new();
        };
        read_texture_to_vec(&self.ctx.device, &self.ctx.queue, source)
    }

    /// Return the document region that was composited during the last `render`
    /// call.
    ///
    /// This is primarily useful for tests and debug tooling.
    pub fn composite_region(&self) -> Rect {
        self.source_region
    }

    /// Ensure the `source` texture exists and matches `region`.
    fn ensure_source_texture(&mut self, region: Rect) {
        let needs_create = match &self.source {
            None => true,
            Some(t) => t.size().width != region.w.max(1) || t.size().height != region.h.max(1),
        };

        if needs_create {
            let extent = wgpu::Extent3d {
                width: region.w.max(1),
                height: region.h.max(1),
                depth_or_array_layers: 1,
            };
            self.source = Some(self.ctx.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("ogre canvas source"),
                size: extent,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba32Float,
                usage: wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_DST
                    | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            }));
        }
    }

    /// Copy the compositor's result tiles into the assembled source texture.
    fn copy_result_tiles_to_source(&self, region: Rect, source: &wgpu::Texture) {
        if region.is_empty() {
            return;
        }

        let mut encoder = self
            .ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("ogre canvas assemble"),
            });

        let tile_size = TILE_SIZE as i64;
        for TileCoord { x: tx, y: ty } in region.tiles_covered() {
            let Some(tile_texture) = self.compositor.result_texture(TileCoord { x: tx, y: ty })
            else {
                continue;
            };

            let tile_rect = Rect::new(
                (tx as i64 * tile_size) as i32,
                (ty as i64 * tile_size) as i32,
                TILE_SIZE as u32,
                TILE_SIZE as u32,
            );
            let Some(inter) = tile_rect.intersect(region) else {
                continue;
            };

            let src_origin = wgpu::Origin3d {
                x: (inter.x - tile_rect.x) as u32,
                y: (inter.y - tile_rect.y) as u32,
                z: 0,
            };
            let dst_origin = wgpu::Origin3d {
                x: (inter.x - region.x) as u32,
                y: (inter.y - region.y) as u32,
                z: 0,
            };
            let extent = wgpu::Extent3d {
                width: inter.w,
                height: inter.h,
                depth_or_array_layers: 1,
            };

            encoder.copy_texture_to_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: tile_texture,
                    mip_level: 0,
                    origin: src_origin,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyTextureInfo {
                    texture: source,
                    mip_level: 0,
                    origin: dst_origin,
                    aspect: wgpu::TextureAspect::All,
                },
                extent,
            );
        }

        self.ctx.queue.submit(Some(encoder.finish()));
    }

    /// Ensure the `output` texture exists and matches `view_size_px`.
    fn ensure_output_texture(&mut self, view_size_px: UVec2) {
        let size = UVec2::new(view_size_px.x.max(1), view_size_px.y.max(1));
        let needs_create = match &self.output {
            None => true,
            Some(t) => t.size().width != size.x || t.size().height != size.y,
        };

        if needs_create {
            self.output = Some(self.ctx.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("ogre canvas output"),
                size: wgpu::Extent3d {
                    width: size.x,
                    height: size.y,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::COPY_SRC
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            }));
            self.output_size = size;
            // A new output texture invalidates any previously registered egui
            // binding; the next `register_with_egui` call will update it.
        }
    }
}

#[cfg(feature = "egui")]
impl CanvasRenderer {
    /// Register the current output texture with an `egui_wgpu::Renderer`.
    ///
    /// Returns a stable [`egui::TextureId`]. The first call registers a new
    /// user texture; subsequent calls update the existing binding, so the id
    /// does not change when the output texture is resized.
    ///
    /// `device` must be the same `wgpu::Device` that was passed to
    /// [`CanvasRenderer::new`]; passing a different device will create resources
    /// on the wrong device and likely panic later when egui tries to render.
    ///
    /// # Panics
    ///
    /// Panics if `render` has not yet been called and therefore no output
    /// texture exists.
    pub fn register_with_egui(
        &self,
        r: &mut egui_wgpu::Renderer,
        device: &wgpu::Device,
    ) -> egui::TextureId {
        assert!(
            device == &self.ctx.device,
            "register_with_egui must be called with the same wgpu::Device passed to CanvasRenderer::new"
        );
        let output = self
            .output
            .as_ref()
            .expect("render() must be called before register_with_egui()");
        let view = output.create_view(&wgpu::TextureViewDescriptor::default());

        match self.egui_texture_id.get() {
            Some(id) => {
                r.update_egui_texture_from_wgpu_texture(
                    device,
                    &view,
                    wgpu::FilterMode::Nearest,
                    id,
                );
                id
            }
            None => {
                let id = r.register_native_texture(device, &view, wgpu::FilterMode::Nearest);
                self.egui_texture_id.set(Some(id));
                id
            }
        }
    }

    /// Unregister the renderer's output texture from `egui_wgpu`.
    ///
    /// Call this before dropping the [`CanvasRenderer`] to free the
    /// [`egui::TextureId`] binding in the egui renderer. After unregistering,
    /// subsequent calls to [`register_with_egui`](Self::register_with_egui) will
    /// allocate a new id.
    pub fn unregister_with_egui(&self, r: &mut egui_wgpu::Renderer) {
        if let Some(id) = self.egui_texture_id.take() {
            r.free_texture(&id);
        }
    }
}

/// Render a `kurbo::BezPath` into an `Rgba8Unorm` texture using `vello`.
///
/// GPU vector rendering test utility. The returned texture has `COPY_SRC`
/// usage so it can be read back for golden comparison against
/// [`ogre_vector::rasterize_bezpath`].
///
/// # Panics
///
/// Panics if Vello fails to initialize or render.
#[cfg(test)]
pub fn render_bezpath_gpu(
    ctx: &GpuContext,
    path: &kurbo::BezPath,
    fill: ogre_vector::Fill,
    stroke: ogre_vector::Stroke,
    size: UVec2,
) -> wgpu::Texture {
    let width = size.x.max(1);
    let height = size.y.max(1);

    let mut renderer = vello::Renderer::new(
        &ctx.device,
        vello::RendererOptions {
            antialiasing_support: vello::AaSupport::area_only(),
            ..Default::default()
        },
    )
    .expect("failed to create vello renderer");

    let mut scene = vello::Scene::new();
    let transform = vello::kurbo::Affine::IDENTITY;

    if let ogre_vector::Fill::Solid(color) = fill {
        scene.fill(
            vello::peniko::Fill::NonZero,
            transform,
            to_peniko_color(color),
            None,
            path,
        );
    }

    if stroke.width > 0.0 && stroke.color.a > 0.0 {
        let vello_stroke = vello::kurbo::Stroke::new(stroke.width as f64);
        scene.stroke(
            &vello_stroke,
            transform,
            to_peniko_color(stroke.color),
            None,
            path,
        );
    }

    let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("vello vector render"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

    renderer
        .render_to_texture(
            &ctx.device,
            &ctx.queue,
            &scene,
            &view,
            &vello::RenderParams {
                base_color: vello::peniko::Color::TRANSPARENT,
                width,
                height,
                antialiasing_method: vello::AaConfig::Area,
            },
        )
        .expect("failed to render vector path with vello");

    texture
}

/// Convert a linear-space [`Rgba32F`] to a `vello`/`peniko` sRGB color.
#[cfg(test)]
fn to_peniko_color(c: Rgba32F) -> vello::peniko::Color {
    use ogre_core::srgb_encode;
    vello::peniko::Color::new([srgb_encode(c.r), srgb_encode(c.g), srgb_encode(c.b), c.a])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tile_cache::read_rgba8_texture_to_vec;
    use glam::Vec2;
    use ogre_core::{BlendMode, Document, IVec2};
    use ogre_vector::{rasterize_bezpath, Fill, Stroke};

    const EPSILON: f32 = 1e-4;

    fn fill_rect(buffer: &mut ogre_core::TiledBuffer, rect: Rect, color: Rgba32F) {
        let area = (rect.w as usize)
            .checked_mul(rect.h as usize)
            .expect("rect too large");
        let data = vec![color; area];
        buffer.blit_rect(rect, &data);
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

    #[test]
    fn canvas_renderer_matches_cpu_reference() {
        let ctx = GpuContext::headless();
        let mut renderer = CanvasRenderer::new(ctx.device, ctx.queue, ctx.adapter_info);
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

        renderer.set_viewport(Viewport::new(Vec2::ZERO, 1.0));
        renderer.render(&doc, UVec2::new(512, 512));

        let gpu = renderer.read_output();
        let cpu = ogre_core::compositor::composite_document(&doc, doc.canvas).unwrap();
        assert_region_approx_eq(&gpu, &cpu, EPSILON);
    }

    #[test]
    fn render_culls_to_viewport_visible_region() {
        let ctx = GpuContext::headless();
        let mut renderer = CanvasRenderer::new(ctx.device, ctx.queue, ctx.adapter_info);
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

        // Show only the bottom-right quadrant of the document.
        renderer.set_viewport(Viewport::new(Vec2::new(256.0, 256.0), 1.0));
        renderer.render(&doc, UVec2::new(256, 256));

        let expected_region = Rect::new(256, 256, 256, 256);
        assert_eq!(renderer.composite_region(), expected_region);

        let gpu = renderer.read_output();
        let cpu = ogre_core::compositor::composite_document(&doc, expected_region).unwrap();
        assert_region_approx_eq(&gpu, &cpu, EPSILON);
    }

    #[test]
    fn read_output_before_render_is_empty() {
        let ctx = GpuContext::headless();
        let renderer = CanvasRenderer::new(ctx.device, ctx.queue, ctx.adapter_info);
        assert!(renderer.read_output().is_empty());
    }

    #[test]
    fn output_resizes_with_view_size() {
        let ctx = GpuContext::headless();
        let mut renderer = CanvasRenderer::new(ctx.device, ctx.queue, ctx.adapter_info);
        let doc = Document::new(64, 64);

        renderer.render(&doc, UVec2::new(64, 64));
        assert_eq!(renderer.output_size, UVec2::new(64, 64));

        renderer.render(&doc, UVec2::new(128, 128));
        assert_eq!(renderer.output_size, UVec2::new(128, 128));
    }

    #[test]
    fn clear_caches_drops_output_and_allows_rerender() {
        let ctx = GpuContext::headless();
        let mut renderer = CanvasRenderer::new(ctx.device, ctx.queue, ctx.adapter_info);
        let mut doc = Document::new(64, 64);

        let bg = doc.add_raster_layer("bg");
        fill_rect(
            doc.layer_mut(bg).unwrap().buffer_mut().unwrap(),
            Rect::new(0, 0, 64, 64),
            Rgba32F::new(0.0, 1.0, 0.0, 1.0),
        );

        renderer.set_viewport(Viewport::new(Vec2::ZERO, 1.0));
        renderer.render(&doc, UVec2::new(64, 64));
        let before = renderer.read_output();
        assert!(!before.is_empty());
        assert_eq!(renderer.composite_region(), Rect::new(0, 0, 64, 64));

        renderer.clear_caches();
        assert!(renderer.read_output().is_empty());
        assert!(renderer.composite_region().is_empty());

        renderer.render(&doc, UVec2::new(64, 64));
        let after = renderer.read_output();
        assert_eq!(after.len(), before.len());
        assert_region_approx_eq(&after, &before, EPSILON);
    }

    #[cfg(feature = "egui")]
    #[test]
    fn register_with_egui_returns_stable_id() {
        use egui_wgpu::Renderer;

        let ctx = GpuContext::headless();
        let mut renderer = CanvasRenderer::new(ctx.device, ctx.queue, ctx.adapter_info);
        let doc = Document::new(64, 64);

        renderer.render(&doc, UVec2::new(64, 64));

        let mut egui_renderer = Renderer::new(
            &renderer.ctx.device,
            wgpu::TextureFormat::Rgba8Unorm,
            egui_wgpu::RendererOptions::default(),
        );

        let id1 = renderer.register_with_egui(&mut egui_renderer, &renderer.ctx.device);
        let id2 = renderer.register_with_egui(&mut egui_renderer, &renderer.ctx.device);
        assert_eq!(id1, id2, "TextureId must stay stable across frames");
        assert!(
            egui_renderer.texture(&id1).is_some(),
            "texture must be registered"
        );

        // Resize the output and verify the same id is reused.
        renderer.render(&doc, UVec2::new(128, 128));
        let id3 = renderer.register_with_egui(&mut egui_renderer, &renderer.ctx.device);
        assert_eq!(id1, id3, "TextureId must stay stable after resize");
        assert!(
            egui_renderer.texture(&id3).is_some(),
            "texture must remain registered after resize"
        );

        // Unregistering frees the id and allows a new one to be allocated.
        renderer.unregister_with_egui(&mut egui_renderer);
        assert!(
            egui_renderer.texture(&id1).is_none(),
            "texture must be freed after unregister"
        );
        let id4 = renderer.register_with_egui(&mut egui_renderer, &renderer.ctx.device);
        assert_ne!(
            id1, id4,
            "a new TextureId must be allocated after unregister"
        );
        assert!(
            egui_renderer.texture(&id4).is_some(),
            "new texture must be registered"
        );
    }

    #[test]
    fn vello_path_matches_lyon_reference() {
        let ctx = GpuContext::headless();
        let size = UVec2::new(64, 64);

        let mut path = kurbo::BezPath::new();
        path.move_to((10.0, 10.0));
        path.line_to((54.0, 10.0));
        path.line_to((32.0, 54.0));
        path.close_path();

        let fill = Fill::Solid(Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        let stroke = Stroke::NONE;

        let gpu_tex = render_bezpath_gpu(&ctx, &path, fill, stroke, size);
        let gpu = read_rgba8_texture_to_vec(&ctx.device, &ctx.queue, &gpu_tex);

        let cpu_buf = rasterize_bezpath(&path, fill, stroke);
        let cpu = cpu_buf.read_rect(Rect::new(0, 0, size.x, size.y));

        assert_eq!(gpu.len(), cpu.len(), "pixel counts differ");

        // Vello writes an 8-bit UNORM target, so a one-byte quantization step
        // is the finest meaningful per-channel difference.
        const TOLERANCE: f32 = 1.0 / 255.0 + 1e-4;
        for (i, (a, b)) in gpu.iter().zip(cpu.iter()).enumerate() {
            assert!(
                (a.r - b.r).abs() < TOLERANCE
                    && (a.g - b.g).abs() < TOLERANCE
                    && (a.b - b.b).abs() < TOLERANCE
                    && (a.a - b.a).abs() < TOLERANCE,
                "pixel {} differs: gpu={:?}, cpu={:?}",
                i,
                a,
                b
            );
        }
    }
}
