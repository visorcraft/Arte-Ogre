// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Headless integration test for the public `CanvasRenderer` API.

use glam::{UVec2, Vec2};
use ogre_core::{
    BlendMode, Document, IVec2, Rect, Rgba32F, StrokeCap, StrokeJoin, VectorData, VectorFill,
    VectorPath, VectorStroke,
};
use ogre_gpu::canvas_renderer::CanvasRenderer;
use ogre_gpu::context::GpuContext;
use ogre_gpu::viewport::Viewport;
use ogre_io::svg::{import_svg, SvgImportMode, SvgImportOptions};

const EPSILON: f32 = 1e-4;

fn fill_rect(buffer: &mut ogre_core::TiledBuffer, rect: Rect, color: Rgba32F) {
    let area = (rect.w as usize)
        .checked_mul(rect.h as usize)
        .expect("rect too large");
    let data = vec![color; area];
    buffer.blit_rect(rect, &data);
}

#[test]
fn new_set_viewport_render_read_matches_cpu_reference() {
    let ctx = GpuContext::headless();
    let mut renderer = CanvasRenderer::new(ctx.device, ctx.queue, ctx.adapter_info);

    let mut doc = Document::new(512, 512);

    let bg = doc.add_raster_layer("bg");
    fill_rect(
        doc.layer_mut(bg).unwrap().buffer_mut().unwrap(),
        Rect::new(0, 0, 512, 512),
        Rgba32F::new(1.0, 0.0, 0.0, 1.0),
    );

    let mid = doc.add_raster_layer("mid");
    fill_rect(
        doc.layer_mut(mid).unwrap().buffer_mut().unwrap(),
        Rect::new(0, 0, 300, 300),
        Rgba32F::new(0.0, 1.0, 0.0, 0.5),
    );
    doc.layer_mut(mid).unwrap().offset = IVec2::new(40, 40);
    doc.layer_mut(mid).unwrap().blend = BlendMode::Multiply;
    doc.layer_mut(mid).unwrap().opacity = 0.8;

    let top = doc.add_raster_layer("top");
    fill_rect(
        doc.layer_mut(top).unwrap().buffer_mut().unwrap(),
        Rect::new(0, 0, 200, 200),
        Rgba32F::new(0.0, 0.0, 1.0, 0.5),
    );
    doc.layer_mut(top).unwrap().offset = IVec2::new(120, 120);
    doc.layer_mut(top).unwrap().blend = BlendMode::Screen;
    doc.layer_mut(top).unwrap().opacity = 0.6;

    renderer.set_viewport(Viewport::new(Vec2::new(0.0, 0.0), 1.0));
    renderer.render(&doc, UVec2::new(512, 512));

    let gpu = renderer.read_output();
    let cpu = ogre_core::compositor::composite_document(&doc, doc.canvas).unwrap();

    assert_eq!(gpu.len(), cpu.len(), "pixel count mismatch");
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
fn vector_layer_render_matches_cpu_reference() {
    let ctx = GpuContext::headless();
    let mut renderer = CanvasRenderer::new(ctx.device, ctx.queue, ctx.adapter_info);

    let mut doc = Document::new(100, 100);

    let data = VectorData {
        paths: vec![VectorPath {
            vertices: vec![
                IVec2::new(0, 0),
                IVec2::new(50, 0),
                IVec2::new(50, 50),
                IVec2::new(0, 50),
            ],
            fill: VectorFill::Solid(Rgba32F::new(1.0, 0.0, 0.0, 1.0)),
            stroke: VectorStroke {
                color: Rgba32F::TRANSPARENT,
                width: 0.0,
                dash: Vec::new(),
                cap: StrokeCap::Butt,
                join: StrokeJoin::Miter,
            },
            closed: true,
        }],
        rasterized: None,
        text: None,
        svg_source: None,
        version: 1,
    };
    let vector = doc.add_vector_layer("vector", data);
    doc.layer_mut(vector).unwrap().offset = IVec2::new(10, 10);

    renderer.set_viewport(Viewport::new(Vec2::new(0.0, 0.0), 1.0));
    renderer.render(&doc, UVec2::new(100, 100));

    let gpu = renderer.read_output();
    let cpu = ogre_core::compositor::composite_document(&doc, doc.canvas).unwrap();

    assert_eq!(gpu.len(), cpu.len(), "pixel count mismatch");
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
fn vector_layer_with_rasterized_cache_matches_cpu_reference() {
    let ctx = GpuContext::headless();
    let mut renderer = CanvasRenderer::new(ctx.device, ctx.queue, ctx.adapter_info);

    let mut doc = Document::new(100, 100);

    let mut rasterized = ogre_core::TiledBuffer::new();
    fill_rect(
        &mut rasterized,
        Rect::new(10, 10, 50, 50),
        Rgba32F::new(0.0, 0.0, 1.0, 1.0),
    );

    let data = VectorData {
        paths: Vec::new(),
        rasterized: Some(rasterized),
        text: None,
        svg_source: None,
        version: 1,
    };
    let vector = doc.add_vector_layer("cached", data);
    doc.layer_mut(vector).unwrap().offset = IVec2::new(0, 0);

    renderer.set_viewport(Viewport::new(Vec2::new(0.0, 0.0), 1.0));
    renderer.render(&doc, UVec2::new(100, 100));

    let gpu = renderer.read_output();
    let cpu = ogre_core::compositor::composite_document(&doc, doc.canvas).unwrap();

    assert_eq!(gpu.len(), cpu.len(), "pixel count mismatch");
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
fn svg_import_vector_matches_cpu_reference() {
    let ctx = GpuContext::headless();
    let mut renderer = CanvasRenderer::new(ctx.device, ctx.queue, ctx.adapter_info);

    let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100"><rect width="50" height="50" fill="#ff0000"/></svg>"##;
    let mut doc = import_svg(
        svg,
        SvgImportOptions {
            mode: SvgImportMode::Vector,
            dpi: 96.0,
        },
    )
    .unwrap();

    let layer_id = doc.order[0];
    doc.layer_mut(layer_id).unwrap().offset = IVec2::new(10, 10);

    renderer.set_viewport(Viewport::new(Vec2::new(0.0, 0.0), 1.0));
    renderer.render(&doc, UVec2::new(100, 100));

    let gpu = renderer.read_output();
    let cpu = ogre_core::compositor::composite_document(&doc, doc.canvas).unwrap();

    assert_eq!(gpu.len(), cpu.len(), "pixel count mismatch");
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
fn vector_layer_with_opacity_and_blend_matches_cpu_reference() {
    let ctx = GpuContext::headless();
    let mut renderer = CanvasRenderer::new(ctx.device, ctx.queue, ctx.adapter_info);

    let mut doc = Document::new(100, 100);

    let bg = doc.add_raster_layer("bg");
    fill_rect(
        doc.layer_mut(bg).unwrap().buffer_mut().unwrap(),
        Rect::new(0, 0, 100, 100),
        Rgba32F::new(1.0, 1.0, 0.0, 1.0),
    );

    let data = VectorData {
        paths: vec![VectorPath {
            vertices: vec![
                IVec2::new(0, 0),
                IVec2::new(50, 0),
                IVec2::new(50, 50),
                IVec2::new(0, 50),
            ],
            fill: VectorFill::Solid(Rgba32F::new(1.0, 0.0, 0.0, 1.0)),
            stroke: VectorStroke {
                color: Rgba32F::TRANSPARENT,
                width: 0.0,
                dash: Vec::new(),
                cap: StrokeCap::Butt,
                join: StrokeJoin::Miter,
            },
            closed: true,
        }],
        rasterized: None,
        text: None,
        svg_source: None,
        version: 1,
    };
    let vector = doc.add_vector_layer("vector", data);
    doc.layer_mut(vector).unwrap().offset = IVec2::new(10, 10);
    doc.layer_mut(vector).unwrap().opacity = 0.6;
    doc.layer_mut(vector).unwrap().blend = BlendMode::Multiply;

    renderer.set_viewport(Viewport::new(Vec2::new(0.0, 0.0), 1.0));
    renderer.render(&doc, UVec2::new(100, 100));

    let gpu = renderer.read_output();
    let cpu = ogre_core::compositor::composite_document(&doc, doc.canvas).unwrap();

    assert_eq!(gpu.len(), cpu.len(), "pixel count mismatch");
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
