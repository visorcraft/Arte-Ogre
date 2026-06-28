// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Golden tests: GPU single-layer composite must match the CPU reference
//! compositor for every [`BlendMode`] across representative colors and
//! opacities.
//!
//! On failure both the GPU result and the CPU reference are written as PNGs to
//! `target/golden/<case_name>.gpu.png` and `target/golden/<case_name>.cpu.png`
//! for visual inspection.

use std::path::PathBuf;

use image::{ImageBuffer, Rgba};
use ogre_core::{blend_pixel, BlendMode, IVec2, Rgba32F, Tile, TILE_SIZE};
use ogre_gpu::{
    blend::BlendPipelines,
    context::GpuContext,
    tile_cache::{read_texture_to_vec, upload_tile},
};

const EPSILON: f32 = 1e-4;

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
        label: Some("golden blend accumulator"),
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

fn dump_png(path: PathBuf, pixels: &[Rgba32F], width: u32, height: u32) {
    let u8_pixels: Vec<u8> = pixels
        .iter()
        .flat_map(|p| {
            let clamp = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
            [clamp(p.r), clamp(p.g), clamp(p.b), clamp(p.a)]
        })
        .collect();
    let buffer = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(width, height, u8_pixels)
        .expect("pixel vector had expected dimensions");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    buffer.save(&path).unwrap_or_else(|e| {
        eprintln!("failed to write {}: {}", path.display(), e);
    });
}

fn case_name(mode: BlendMode, src: Rgba32F, dst: Rgba32F, opacity: f32) -> String {
    let fmt = |p: Rgba32F| {
        format!(
            "r{}g{}b{}a{}",
            (p.r * 1000.0) as i32,
            (p.g * 1000.0) as i32,
            (p.b * 1000.0) as i32,
            (p.a * 1000.0) as i32
        )
    };
    format!(
        "{}_src{}_dst{}_op{:.2}",
        mode.name(),
        fmt(src),
        fmt(dst),
        opacity
    )
    .replace(['.', ',', ' '], "_")
}

fn run_case(
    ctx: &GpuContext,
    pipelines: &BlendPipelines,
    mode: BlendMode,
    src: Rgba32F,
    dst: Rgba32F,
    opacity: f32,
) -> Result<(), String> {
    let src_texture = upload_tile(ctx, &solid_tile(src));
    let dst_texture = solid_storage_texture(ctx, dst);

    pipelines.composite_tile_over(
        ctx,
        &src_texture,
        &dst_texture,
        mode,
        opacity,
        IVec2::ZERO,
        None,
    );

    let gpu_pixels = read_texture_to_vec(&ctx.device, &ctx.queue, &dst_texture);
    let expected = blend_pixel(mode, dst, src, opacity);

    if let Some(bad) = gpu_pixels
        .iter()
        .find(|p| !pixel_approx_eq(**p, expected, EPSILON))
    {
        // Dump failure artefacts into the workspace target/ directory so they are
        // covered by the top-level .gitignore.
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let root = manifest.parent().unwrap().parent().unwrap();
        let name = case_name(mode, src, dst, opacity);
        dump_png(
            root.join("target")
                .join("golden")
                .join(format!("{}.gpu.png", name)),
            &gpu_pixels,
            TILE_SIZE as u32,
            TILE_SIZE as u32,
        );
        let cpu_pixels = vec![expected; TILE_SIZE * TILE_SIZE];
        dump_png(
            root.join("target")
                .join("golden")
                .join(format!("{}.cpu.png", name)),
            &cpu_pixels,
            TILE_SIZE as u32,
            TILE_SIZE as u32,
        );
        return Err(format!(
            "mode={:?} src={:?} dst={:?} opacity={}: got {:?}, expected {:?}",
            mode, src, dst, opacity, bad, expected
        ));
    }
    Ok(())
}

fn pixel_approx_eq(a: Rgba32F, b: Rgba32F, eps: f32) -> bool {
    (a.r - b.r).abs() < eps
        && (a.g - b.g).abs() < eps
        && (a.b - b.b).abs() < eps
        && (a.a - b.a).abs() < eps
}

#[test]
fn golden_blend_matches_cpu_reference_for_all_modes() {
    let ctx = GpuContext::headless();
    let pipelines = BlendPipelines::new(&ctx);

    let colors: &[Rgba32F] = &[
        Rgba32F::new(1.0, 0.0, 0.0, 1.0),
        Rgba32F::new(0.0, 1.0, 0.0, 1.0),
        Rgba32F::new(0.0, 0.0, 1.0, 1.0),
        Rgba32F::new(1.0, 1.0, 1.0, 1.0),
        Rgba32F::new(0.0, 0.0, 0.0, 1.0),
        Rgba32F::new(0.5, 0.25, 0.75, 1.0),
    ];
    let alphas: &[f32] = &[1.0, 0.5, 0.25];
    let opacities: &[f32] = &[1.0, 0.5, 0.0];

    let mut failures: Vec<String> = Vec::new();

    let modes = [
        BlendMode::Normal,
        BlendMode::Multiply,
        BlendMode::Screen,
        BlendMode::Overlay,
        BlendMode::Darken,
        BlendMode::Lighten,
        BlendMode::ColorDodge,
        BlendMode::ColorBurn,
        BlendMode::HardLight,
        BlendMode::SoftLight,
        BlendMode::Difference,
        BlendMode::Exclusion,
        BlendMode::Add,
    ];

    for mode in modes {
        for src_base in colors {
            for src_a in alphas {
                for dst_base in colors {
                    for dst_a in alphas {
                        for opacity in opacities {
                            let src = Rgba32F::new(src_base.r, src_base.g, src_base.b, *src_a);
                            let dst = Rgba32F::new(dst_base.r, dst_base.g, dst_base.b, *dst_a);
                            if let Err(msg) = run_case(&ctx, &pipelines, mode, src, dst, *opacity) {
                                failures.push(msg);
                            }
                        }
                    }
                }
            }
        }
    }

    if !failures.is_empty() {
        for msg in &failures[..(failures.len().min(10))] {
            eprintln!("FAIL: {}", msg);
        }
        panic!("{} golden blend cases failed", failures.len());
    }
}
