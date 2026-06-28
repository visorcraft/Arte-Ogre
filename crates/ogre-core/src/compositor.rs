// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! CPU reference compositor (ground truth for the GPU implementation).
//!
//! This module composites a [`Document`] into a flat row-major buffer of
//! straight-alpha [`Rgba32F`] pixels. Blending is straight-alpha in and out,
//! using the Porter-Duff "over" operator with separable blend functions.
//!
//! Groups are composited by first merging their children into a temporary
//! buffer and then blending that buffer over the accumulated result using the
//! group's own blend mode, opacity, and offset.

use rayon::prelude::*;

use crate::buffer::TiledBuffer;
use crate::coord::{IVec2, Rect};
use crate::document::Document;
use crate::error::OgreError;
use crate::layer::{AdjustmentKind, BlendMode, Layer, LayerContent, VectorData, VectorFill};
use crate::pixel::clamp01_nan0 as sanitize_coverage;
use crate::pixel::clamp01_nan0 as sanitize_opacity;
use crate::pixel::Rgba32F;

/// Composite a region of `doc` into a row-major straight-alpha buffer.
///
/// The returned vector contains `region.w * region.h` pixels; the pixel at
/// document coordinate `(x, y)` is at index `(y - region.y) * region.w +
/// (x - region.x)`. Pixels outside the region are ignored. If `region` is
/// empty, an empty vector is returned.
///
/// Layers are walked in bottom-to-top [`Document::render_order`]. Groups are
/// flattened into temporary buffers before being blended with their own blend
/// mode and opacity.
///
/// # Errors
///
/// Returns [`OgreError::InvalidOperation`] if the layer tree contains a cycle.
pub fn composite_document(doc: &Document, region: Rect) -> Result<Vec<Rgba32F>, OgreError> {
    let area: usize = match region.area().try_into() {
        Ok(a) => a,
        Err(_) => return Ok(Vec::new()),
    };
    if area == 0 {
        return Ok(Vec::new());
    }

    let order = doc.render_order()?;
    let mut accum: Vec<Rgba32F> = vec![Rgba32F::TRANSPARENT; area];
    let mut group_stack: Vec<(Vec<Rgba32F>, GroupState)> = Vec::new();

    let mut i = 0;
    while i < order.len() {
        // Close any groups that have finished rendering before processing the
        // next layer.
        while let Some(top) = group_stack.last() {
            if order[i].1 <= top.1.depth {
                close_group(&mut accum, &mut group_stack);
            } else {
                break;
            }
        }

        let (id, depth) = order[i];
        let layer = match doc.layer(id) {
            Ok(l) => l,
            Err(_) => {
                i += 1;
                continue;
            }
        };

        let group_offset = group_stack
            .last()
            .map(|(_, state)| state.offset)
            .unwrap_or(IVec2::ZERO);
        let group_valid = group_stack
            .last()
            .map(|(_, state)| state.valid)
            .unwrap_or(true);

        match &layer.content {
            LayerContent::Group { .. } => {
                if !layer.visible {
                    // Skip the entire subtree.
                    i += 1;
                    while i < order.len() && order[i].1 > depth {
                        i += 1;
                    }
                    continue;
                }

                let saved = std::mem::take(&mut accum);
                accum = vec![Rgba32F::TRANSPARENT; saved.len()];
                let parent_offset = group_stack
                    .last()
                    .map(|(_, state)| state.offset)
                    .unwrap_or(IVec2::ZERO);
                let parent_valid = group_stack
                    .last()
                    .map(|(_, state)| state.valid)
                    .unwrap_or(true);
                group_stack.push((
                    saved,
                    GroupState::from_layer(layer, depth, parent_offset, parent_valid),
                ));
                i += 1;
                continue;
            }
            LayerContent::Adjustment(kind) => {
                let opacity = sanitize_opacity(layer.opacity);
                if layer.visible && opacity > 0.0 && group_valid {
                    apply_adjustment(&mut accum, kind, opacity);
                }
                i += 1;
                continue;
            }
            LayerContent::Raster { .. } => {}
            LayerContent::Vector(data) => {
                let opacity = sanitize_opacity(layer.opacity);
                if !layer.visible || opacity == 0.0 || !group_valid {
                    i += 1;
                    continue;
                }
                let rasterized = rasterize_vector_data(data);
                composite_explicit_buffer(
                    &rasterized,
                    None,
                    layer,
                    region,
                    &mut accum,
                    group_offset,
                );
                i += 1;
                continue;
            }
        }

        if group_valid {
            composite_raster_layer(layer, region, &mut accum, group_offset);
        }
        i += 1;
    }

    while !group_stack.is_empty() {
        close_group(&mut accum, &mut group_stack);
    }

    Ok(accum)
}

/// Composite a region of `doc` into a sparse [`TiledBuffer`] in document space.
///
/// Unlike [`composite_document`] (which returns a flat row-major `Vec`), this
/// returns the result as a tiled buffer keyed by document coordinates, which is
/// what the brush-engine tools (Healing, Blur/Sharpen/Smudge, Color
/// Replacement) and Quick Selection / Color Range need for a "frozen backdrop"
/// or "sample-all-layers" snapshot.
///
/// The region is clipped to `doc.canvas`; an empty intersection (or zero-area
/// region) yields an empty buffer. Pixel `(x, y)` of the result equals
/// `composite_document(doc, clipped_region)` at the same coordinate.
pub fn composite_region(doc: &Document, region: Rect) -> Result<TiledBuffer, OgreError> {
    let clipped = region.intersect(doc.canvas);
    let Some(region) = clipped else {
        return Ok(TiledBuffer::new());
    };
    if region.is_empty() {
        return Ok(TiledBuffer::new());
    }
    let pixels = composite_document(doc, region)?;
    let w = region.w as i32;
    let mut buffer = TiledBuffer::new();
    for py in 0..region.h as i32 {
        for px in 0..w {
            let idx = (py * w + px) as usize;
            buffer.set_pixel(IVec2::new(region.x + px, region.y + py), pixels[idx]);
        }
    }
    Ok(buffer)
}

/// State saved while compositing a group.
struct GroupState {
    /// The nesting depth of the group in [`Document::render_order`].
    depth: u8,
    /// Blend mode applied to the merged group result.
    blend: BlendMode,
    /// Opacity applied to the merged group result.
    opacity: f32,
    /// Cumulative offset of this group and all enclosing groups.
    offset: IVec2,
    /// `false` when the cumulative group offset overflowed `i32`; children are
    /// treated as transparent.
    valid: bool,
}

impl GroupState {
    fn from_layer(layer: &Layer, depth: u8, parent_offset: IVec2, parent_valid: bool) -> Self {
        let (offset, valid) = if parent_valid {
            match (
                parent_offset.x.checked_add(layer.offset.x),
                parent_offset.y.checked_add(layer.offset.y),
            ) {
                (Some(x), Some(y)) => (IVec2::new(x, y), true),
                _ => (IVec2::ZERO, false),
            }
        } else {
            (IVec2::ZERO, false)
        };
        Self {
            depth,
            blend: layer.blend,
            opacity: sanitize_opacity(layer.opacity),
            offset,
            valid,
        }
    }
}

/// Close the innermost group, blending its temporary buffer over the saved
/// accumulator.
fn close_group(accum: &mut [Rgba32F], group_stack: &mut Vec<(Vec<Rgba32F>, GroupState)>) {
    let (saved, state) = group_stack.pop().expect("group stack is not empty");
    let blend = state.blend;
    let opacity = state.opacity;
    accum
        .par_iter_mut()
        .zip(saved.par_iter())
        .for_each(|(a, s)| {
            *a = blend_pixel(blend, *s, *a, opacity);
        });
}

/// Sample the blended color at a single document pixel without allocating a
/// full output buffer.
///
/// Returns `None` when `pos` is outside [`Document::canvas`]. The returned
/// color matches [`composite_document`] for a 1×1 region at the same position.
pub fn sample_document_pixel(doc: &Document, pos: IVec2) -> Option<Rgba32F> {
    if !doc.canvas.contains(pos) {
        return None;
    }
    let order = doc.render_order().ok()?;
    let mut accum = Rgba32F::TRANSPARENT;
    let mut group_stack: Vec<(Rgba32F, GroupState)> = Vec::new();

    let mut i = 0;
    while i < order.len() {
        while let Some(top) = group_stack.last() {
            if order[i].1 <= top.1.depth {
                close_group_pixel(&mut accum, &mut group_stack);
            } else {
                break;
            }
        }

        let (id, depth) = order[i];
        let layer = match doc.layer(id) {
            Ok(l) => l,
            Err(_) => {
                i += 1;
                continue;
            }
        };

        let group_offset = group_stack
            .last()
            .map(|(_, state)| state.offset)
            .unwrap_or(IVec2::ZERO);
        let group_valid = group_stack
            .last()
            .map(|(_, state)| state.valid)
            .unwrap_or(true);

        match &layer.content {
            LayerContent::Group { .. } => {
                if !layer.visible {
                    // Skip the entire subtree.
                    i += 1;
                    while i < order.len() && order[i].1 > depth {
                        i += 1;
                    }
                    continue;
                }

                let saved = accum;
                accum = Rgba32F::TRANSPARENT;
                let parent_offset = group_offset;
                let parent_valid = group_valid;
                group_stack.push((
                    saved,
                    GroupState::from_layer(layer, depth, parent_offset, parent_valid),
                ));
                i += 1;
                continue;
            }
            LayerContent::Adjustment(kind) => {
                let opacity = sanitize_opacity(layer.opacity);
                if layer.visible && opacity > 0.0 && group_valid {
                    let kind = sanitize_adjustment(kind);
                    accum = apply_adjustment_pixel(&kind, opacity, accum);
                }
                i += 1;
                continue;
            }
            LayerContent::Raster { .. } => {}
            LayerContent::Vector(data) => {
                let opacity = sanitize_opacity(layer.opacity);
                if !layer.visible || opacity == 0.0 || !group_valid {
                    i += 1;
                    continue;
                }
                let rasterized = rasterize_vector_data(data);
                let buffer = &rasterized;
                let offset = match (
                    layer.offset.x.checked_add(group_offset.x),
                    layer.offset.y.checked_add(group_offset.y),
                ) {
                    (Some(x), Some(y)) => IVec2::new(x, y),
                    _ => {
                        i += 1;
                        continue;
                    }
                };
                let local_p = match (pos.x.checked_sub(offset.x), pos.y.checked_sub(offset.y)) {
                    (Some(lx), Some(ly)) => IVec2::new(lx, ly),
                    _ => {
                        i += 1;
                        continue;
                    }
                };
                let px = buffer.get_pixel(local_p);
                if px.a == 0.0 {
                    i += 1;
                    continue;
                }
                accum = blend_pixel(layer.blend, accum, px, opacity);
                i += 1;
                continue;
            }
        }

        if group_valid {
            accum = sample_raster_layer_pixel(layer, pos, group_offset, accum);
        }
        i += 1;
    }

    while !group_stack.is_empty() {
        close_group_pixel(&mut accum, &mut group_stack);
    }

    Some(accum)
}

/// Close the innermost group for a single-pixel sample.
fn close_group_pixel(accum: &mut Rgba32F, group_stack: &mut Vec<(Rgba32F, GroupState)>) {
    let (saved, state) = group_stack.pop().expect("group stack is not empty");
    *accum = blend_pixel(state.blend, saved, *accum, state.opacity);
}

/// Composite a single raster layer's contribution at `doc_p` onto `dst`.
fn sample_raster_layer_pixel(
    layer: &Layer,
    doc_p: IVec2,
    group_offset: IVec2,
    dst: Rgba32F,
) -> Rgba32F {
    let opacity = sanitize_opacity(layer.opacity);
    if !layer.visible || opacity == 0.0 {
        return dst;
    }

    let (buffer, mask) = match &layer.content {
        LayerContent::Raster { buffer, mask } => (buffer, mask.as_ref()),
        LayerContent::Group { .. } | LayerContent::Adjustment(_) => return dst,
        LayerContent::Vector(_) => return dst, // handled in the caller loop
    };

    let offset = match (
        layer.offset.x.checked_add(group_offset.x),
        layer.offset.y.checked_add(group_offset.y),
    ) {
        (Some(x), Some(y)) => IVec2::new(x, y),
        _ => return dst,
    };

    let local_p = match (doc_p.x.checked_sub(offset.x), doc_p.y.checked_sub(offset.y)) {
        (Some(lx), Some(ly)) => IVec2::new(lx, ly),
        _ => return dst,
    };

    let mut px = buffer.get_pixel(local_p);

    if let Some(m) = mask {
        let cov = m.get_pixel(local_p).r;
        px.a *= sanitize_coverage(cov);
    }

    if px.a == 0.0 {
        return dst;
    }

    blend_pixel(layer.blend, dst, px, opacity)
}

/// Composite a single raster layer onto an accumulator buffer.
fn composite_raster_layer(layer: &Layer, region: Rect, accum: &mut [Rgba32F], group_offset: IVec2) {
    let (buffer, mask) = match &layer.content {
        LayerContent::Raster { buffer, mask } => (buffer, mask.as_ref()),
        LayerContent::Group { .. } | LayerContent::Adjustment(_) | LayerContent::Vector(_) => {
            return
        }
    };
    composite_explicit_buffer(buffer, mask, layer, region, accum, group_offset);
}

/// Composite an explicit buffer (used for both raster layers and the on-the-fly
/// rasterized vector layers).
fn composite_explicit_buffer(
    buffer: &TiledBuffer,
    mask: Option<&TiledBuffer>,
    layer: &Layer,
    region: Rect,
    accum: &mut [Rgba32F],
    group_offset: IVec2,
) {
    let opacity = sanitize_opacity(layer.opacity);
    if !layer.visible || opacity == 0.0 {
        return;
    }

    let w = region.w as usize;
    let rx = region.x as i64;
    let ry = region.y as i64;
    let offset = match (
        layer.offset.x.checked_add(group_offset.x),
        layer.offset.y.checked_add(group_offset.y),
    ) {
        (Some(x), Some(y)) => IVec2::new(x, y),
        _ => return,
    };

    let blend = layer.blend;
    accum.par_chunks_mut(w).enumerate().for_each(|(dy, row)| {
        let y = ry + dy as i64;
        for (dx, out) in row.iter_mut().enumerate() {
            let x = rx + dx as i64;
            let doc_p = IVec2::new(x as i32, y as i32);
            let local_p = match (doc_p.x.checked_sub(offset.x), doc_p.y.checked_sub(offset.y)) {
                (Some(lx), Some(ly)) => IVec2::new(lx, ly),
                _ => continue,
            };
            let mut px = buffer.get_pixel(local_p);

            if let Some(m) = mask {
                let cov = m.get_pixel(local_p).r;
                px.a *= sanitize_coverage(cov);
            }

            if px.a == 0.0 {
                continue;
            }

            *out = blend_pixel(blend, *out, px, opacity);
        }
    });
}

/// Rasterize vector paths into a layer-local [`TiledBuffer`] (scanline
/// even-odd fill + outline stroke).
///
/// This ignores any pre-rasterized cache, so it is useful for rebuilding the
/// authoritative cache from the current editable paths.
pub fn rasterize_vector_paths(data: &VectorData) -> TiledBuffer {
    let mut buf = TiledBuffer::new();
    for path in &data.paths {
        if path.vertices.len() < 2 {
            continue;
        }
        if let VectorFill::Solid(fill) = path.fill {
            if fill.a > 0.0 && path.vertices.len() >= 3 {
                for (p, c) in scanline_fill(&path.vertices, fill) {
                    buf.set_pixel(p, c);
                }
            }
        }
        if path.stroke.color.a > 0.0 && path.stroke.width > 0.0 {
            let n = path.vertices.len();
            let edges = if path.closed { n } else { n - 1 };
            let w = path.stroke.width.round() as i32;
            for i in 0..edges {
                let a = path.vertices[i];
                let b = path.vertices[(i + 1) % n];
                // Widen the stroke by drawing parallel bresenham lines offset
                // perpendicular to the segment direction. Offsets: 0, +1, -1,
                // +2, -2, … so the line is centered on the original path.
                let dx = (b.x - a.x) as f32;
                let dy = (b.y - a.y) as f32;
                let len = (dx * dx + dy * dy).sqrt().max(1.0);
                let perp_x = (-dy / len).round() as i32;
                let perp_y = (dx / len).round() as i32;
                for offset in 0..w {
                    let side = if offset == 0 {
                        0
                    } else {
                        (offset + 1) / 2 * if offset % 2 == 1 { 1 } else { -1 }
                    };
                    let ox = perp_x * side;
                    let oy = perp_y * side;
                    for (p, c) in bresenham_line(
                        IVec2::new(a.x + ox, a.y + oy),
                        IVec2::new(b.x + ox, b.y + oy),
                        path.stroke.color,
                    ) {
                        buf.set_pixel(p, c);
                    }
                }
            }
        }
    }
    buf
}

/// Resolve a vector layer's pixel content.
///
/// If a pre-rasterized cache is present (text glyphs, an imported SVG, or a
/// cache rebuilt after an edit) it is treated as the authoritative pixel source
/// and the editable paths are not drawn again. Otherwise the paths are
/// rasterized on demand.
pub fn rasterize_vector_data(data: &VectorData) -> TiledBuffer {
    if let Some(rasterized) = &data.rasterized {
        return rasterized.clone();
    }
    rasterize_vector_paths(data)
}

/// Scanline even-odd polygon fill: returns `(pixel, color)` pairs.
fn scanline_fill(polygon: &[IVec2], fill: Rgba32F) -> Vec<(IVec2, Rgba32F)> {
    if polygon.len() < 3 {
        return Vec::new();
    }
    let min_x = polygon.iter().map(|p| p.x).min().unwrap_or(0);
    let max_x = polygon.iter().map(|p| p.x).max().unwrap_or(0);
    let min_y = polygon.iter().map(|p| p.y).min().unwrap_or(0);
    let max_y = polygon.iter().map(|p| p.y).max().unwrap_or(0);
    let n = polygon.len();
    let mut out = Vec::new();
    for y in min_y..=max_y {
        let yc = y as f32 + 0.5;
        let mut xs: Vec<f32> = Vec::new();
        for i in 0..n {
            let a = polygon[i];
            let b = polygon[(i + 1) % n];
            let (ay, by) = (a.y as f32, b.y as f32);
            if (ay <= yc) == (by <= yc) {
                continue;
            }
            let t = (yc - ay) / (by - ay);
            let x = a.x as f32 + t * (b.x as f32 - a.x as f32);
            xs.push(x);
        }
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mut i = 0;
        while i + 1 < xs.len() {
            let x0 = xs[i].ceil() as i32;
            let x1 = (xs[i + 1] - 1.0).ceil() as i32;
            for x in x0..=x1.max(x0) {
                if x >= min_x && x <= max_x {
                    out.push((IVec2::new(x, y), fill));
                }
            }
            i += 2;
        }
    }
    out
}

/// Bresenham line algorithm returning `(pixel, color)` pairs.
fn bresenham_line(a: IVec2, b: IVec2, color: Rgba32F) -> Vec<(IVec2, Rgba32F)> {
    let (mut x0, mut y0) = (a.x, a.y);
    let (x1, y1) = (b.x, b.y);
    let dx = (x1 - x0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let dy = -(y1 - y0).abs();
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let mut out = Vec::new();
    loop {
        out.push((IVec2::new(x0, y0), color));
        if x0 == x1 && y0 == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x0 += sx;
        }
        if e2 <= dx {
            err += dx;
            y0 += sy;
        }
    }
    out
}

/// Blend `src` over `dst` using `mode` and an overall `opacity`.
///
/// All inputs and outputs are in **straight alpha**, using the Porter-Duff
/// "over" operator with a separable blend function.
///
/// `opacity` is clamped to `[0.0, 1.0]` and treated as zero if NaN. If the
/// resulting alpha is zero, a fully transparent pixel is returned.
pub fn blend_pixel(mode: BlendMode, dst: Rgba32F, src: Rgba32F, opacity: f32) -> Rgba32F {
    let opacity = sanitize_opacity(opacity);
    if opacity == 0.0 || src.a == 0.0 || src.a.is_nan() {
        return dst;
    }

    let src_a = (src.a * opacity).min(1.0);
    if src_a <= 0.0 {
        return dst;
    }

    let dst_a = if dst.a.is_nan() {
        0.0
    } else {
        dst.a.clamp(0.0, 1.0)
    };
    let out_a = (dst_a + src_a * (1.0 - dst_a)).min(1.0);
    if out_a <= 0.0 || out_a.is_nan() {
        return Rgba32F::TRANSPARENT;
    }

    let blend = |d: f32, s: f32| blend_channel(mode, d, s);

    let out_r = blended_channel(dst.r, dst_a, src.r, src_a, out_a, blend);
    let out_g = blended_channel(dst.g, dst_a, src.g, src_a, out_a, blend);
    let out_b = blended_channel(dst.b, dst_a, src.b, src_a, out_a, blend);

    Rgba32F::new(out_r, out_g, out_b, out_a)
}

/// Compute a single output channel using the separable blend formula.
fn blended_channel<F>(dst_c: f32, dst_a: f32, src_c: f32, src_a: f32, out_a: f32, blend: F) -> f32
where
    F: Fn(f32, f32) -> f32,
{
    if out_a <= 0.0 {
        return 0.0;
    }
    let dst_term = dst_c * dst_a * (1.0 - src_a);
    let src_term = src_c * src_a * (1.0 - dst_a);
    let blend_term = src_a * dst_a * blend(dst_c, src_c);
    (dst_term + src_term + blend_term) / out_a
}

/// Separable blend function for one channel.
fn blend_channel(mode: BlendMode, dst: f32, src: f32) -> f32 {
    match mode {
        BlendMode::Normal => src,
        BlendMode::Multiply => dst * src,
        BlendMode::Screen => dst + src - dst * src,
        BlendMode::Overlay => {
            if dst <= 0.5 {
                2.0 * dst * src
            } else {
                1.0 - 2.0 * (1.0 - dst) * (1.0 - src)
            }
        }
        BlendMode::Darken => dst.min(src),
        BlendMode::Lighten => dst.max(src),
        BlendMode::ColorDodge => {
            if src >= 1.0 {
                1.0
            } else {
                (dst / (1.0 - src)).clamp(0.0, 1.0)
            }
        }
        BlendMode::ColorBurn => {
            if src <= 0.0 {
                0.0
            } else {
                let t = (1.0 - dst) / src;
                (1.0 - t).clamp(0.0, 1.0)
            }
        }
        BlendMode::HardLight => {
            if src <= 0.5 {
                2.0 * dst * src
            } else {
                1.0 - 2.0 * (1.0 - dst) * (1.0 - src)
            }
        }
        BlendMode::SoftLight => soft_light(dst, src),
        BlendMode::Difference => (dst - src).abs(),
        BlendMode::Exclusion => dst + src - 2.0 * dst * src,
        BlendMode::Add => dst + src,
    }
}

/// W3C SVG soft-light blend.
fn soft_light(dst: f32, src: f32) -> f32 {
    // Blend modes are defined for [0, 1]; clamp HDR values to avoid NaN from
    // sqrt on negative inputs.
    let d = dst.clamp(0.0, 1.0);
    if src <= 0.5 {
        d - (1.0 - 2.0 * src) * d * (1.0 - d)
    } else {
        d + (2.0 * src - 1.0) * (d.sqrt() - d)
    }
}

/// Clamp an RGB channel to `[0.0, 1.0]`, treating NaN as zero.
fn clamp_channel(v: f32) -> f32 {
    if v.is_nan() {
        0.0
    } else {
        v.clamp(0.0, 1.0)
    }
}

/// Replace a non-finite value (NaN/inf) with `default`.
fn finite_or(v: f32, default: f32) -> f32 {
    if v.is_finite() {
        v
    } else {
        default
    }
}

/// Clamp each channel of a colour to `[0, 1]`, mapping NaN to 0.
fn sanitize_color(c: Rgba32F) -> Rgba32F {
    Rgba32F::new(
        clamp_channel(c.r),
        clamp_channel(c.g),
        clamp_channel(c.b),
        clamp_channel(c.a),
    )
}

/// Replace any non-finite adjustment parameters with safe defaults.
///
/// Adjustment parameters are serialized in `.ogre` files, so a corrupt or
/// hostile value (e.g. `Levels { gamma: NaN }`) could otherwise propagate
/// NaN/inf into every composited pixel. Sanitizing once per adjustment keeps
/// the per-pixel loop branch-free.
fn sanitize_adjustment(kind: &AdjustmentKind) -> AdjustmentKind {
    match *kind {
        AdjustmentKind::BrightnessContrast {
            brightness,
            contrast,
        } => AdjustmentKind::BrightnessContrast {
            brightness: finite_or(brightness, 0.0),
            contrast: finite_or(contrast, 1.0),
        },
        AdjustmentKind::Levels {
            input_black,
            input_white,
            output_black,
            output_white,
            gamma,
        } => AdjustmentKind::Levels {
            input_black: finite_or(input_black, 0.0),
            input_white: finite_or(input_white, 1.0),
            output_black: finite_or(output_black, 0.0),
            output_white: finite_or(output_white, 1.0),
            gamma: finite_or(gamma, 1.0),
        },
        AdjustmentKind::HueSat {
            hue,
            saturation,
            lightness,
        } => AdjustmentKind::HueSat {
            hue: finite_or(hue, 0.0),
            saturation: finite_or(saturation, 1.0),
            lightness: finite_or(lightness, 0.0),
        },
        AdjustmentKind::Curves { mut points } => {
            for p in points.iter_mut() {
                p.input = finite_or(p.input, 0.0);
                p.output = finite_or(p.output, 0.0);
            }
            AdjustmentKind::Curves { points }
        }
        AdjustmentKind::Posterize { levels } => AdjustmentKind::Posterize {
            levels: levels.max(2),
        },
        AdjustmentKind::Threshold { level } => AdjustmentKind::Threshold {
            level: finite_or(level, 0.5).clamp(0.0, 1.0),
        },
        AdjustmentKind::GradientMap { fg, bg } => AdjustmentKind::GradientMap {
            fg: sanitize_color(fg),
            bg: sanitize_color(bg),
        },
        other => other, // Invert, Desaturate — no parameters
    }
}

/// Convert an RGB triple to HSL (all components in `[0, 1]`).
fn rgb_to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) * 0.5;
    if max == min {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if max == r {
        ((g - b) / d + if g < b { 6.0 } else { 0.0 }) / 6.0
    } else if max == g {
        ((b - r) / d + 2.0) / 6.0
    } else {
        ((r - g) / d + 4.0) / 6.0
    };
    (h, s, l)
}

/// Convert an HSL triple to RGB (all components in `[0, 1]`).
fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
    if s == 0.0 {
        return (l, l, l);
    }
    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let p = 2.0 * l - q;
    let r = hue_to_rgb(p, q, h + 1.0 / 3.0);
    let g = hue_to_rgb(p, q, h);
    let b = hue_to_rgb(p, q, h - 1.0 / 3.0);
    (r, g, b)
}

fn hue_to_rgb(p: f32, q: f32, mut t: f32) -> f32 {
    if t < 0.0 {
        t += 1.0;
    }
    if t > 1.0 {
        t -= 1.0;
    }
    if t < 1.0 / 6.0 {
        return p + (q - p) * 6.0 * t;
    }
    if t < 0.5 {
        return q;
    }
    if t < 2.0 / 3.0 {
        return p + (q - p) * (2.0 / 3.0 - t) * 6.0;
    }
    p
}

/// Apply a single adjustment to one pixel.
///
/// `kind` is assumed to have already been sanitized with
/// [`sanitize_adjustment`]. The alpha channel is sanitized but otherwise left
/// unchanged; RGB channels are clamped to `[0.0, 1.0]` for visible pixels.
/// Fully transparent pixels keep their hidden RGB data unmodified so the CPU
/// reference matches the GPU adjustment shader.
fn apply_adjustment_pixel(kind: &AdjustmentKind, opacity: f32, px: Rgba32F) -> Rgba32F {
    let opacity = sanitize_opacity(opacity);
    if opacity == 0.0 {
        return px;
    }

    let a = if px.a.is_nan() {
        0.0
    } else {
        px.a.clamp(0.0, 1.0)
    };
    if a == 0.0 {
        return Rgba32F::new(px.r, px.g, px.b, 0.0);
    }

    let r = clamp_channel(px.r);
    let g = clamp_channel(px.g);
    let b = clamp_channel(px.b);

    let adjusted = match kind {
        AdjustmentKind::Invert => Rgba32F::new(1.0 - r, 1.0 - g, 1.0 - b, a),
        AdjustmentKind::Desaturate => {
            let l = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            let l = clamp_channel(l);
            Rgba32F::new(l, l, l, a)
        }
        AdjustmentKind::BrightnessContrast {
            brightness,
            contrast,
        } => {
            let f = |c: f32| clamp_channel((c - 0.5) * contrast + 0.5 + brightness);
            Rgba32F::new(f(r), f(g), f(b), a)
        }
        AdjustmentKind::HueSat {
            hue,
            saturation,
            lightness,
        } => {
            let (h, s, l) = rgb_to_hsl(r, g, b);
            let h = (h + hue / 360.0).fract();
            let s = (s * saturation).clamp(0.0, 1.0);
            let l = (l + lightness).clamp(0.0, 1.0);
            let (rr, gg, bb) = hsl_to_rgb(h, s, l);
            Rgba32F::new(rr, gg, bb, a)
        }
        AdjustmentKind::Levels {
            input_black,
            input_white,
            output_black,
            output_white,
            gamma,
        } => {
            let in_range = (input_white - input_black).max(1e-6);
            let out_range = output_white - output_black;
            let gamma = gamma.max(1e-6);
            let f = |c: f32| {
                let t = ((c - input_black) / in_range).clamp(0.0, 1.0);
                let t = t.powf(1.0 / gamma);
                (output_black + t * out_range).clamp(0.0, 1.0)
            };
            Rgba32F::new(f(r), f(g), f(b), a)
        }
        AdjustmentKind::Curves { points } => {
            let map = |v: f32| {
                if v <= points[0].input {
                    return points[0].output;
                }
                for i in 0..points.len() - 1 {
                    let a = points[i];
                    let b = points[i + 1];
                    if v <= b.input {
                        let t = (v - a.input) / (b.input - a.input).max(1e-6);
                        return a.output + (b.output - a.output) * t.clamp(0.0, 1.0);
                    }
                }
                points[3].output
            };
            Rgba32F::new(map(r), map(g), map(b), a)
        }
        AdjustmentKind::Posterize { levels } => {
            let n = ((*levels).max(2) as f32) - 1.0;
            let f = |c: f32| (c * n).round() / n;
            Rgba32F::new(f(r), f(g), f(b), a)
        }
        AdjustmentKind::Threshold { level } => {
            // Rec.709 luminance (matches the Desaturate weights).
            let l = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            let v = if l >= *level { 1.0 } else { 0.0 };
            Rgba32F::new(v, v, v, a)
        }
        AdjustmentKind::GradientMap { fg, bg } => {
            // Map luminance onto the fg→bg ramp (duotone); preserve original alpha.
            let l = clamp_channel(0.2126 * r + 0.7152 * g + 0.0722 * b);
            let mapped = fg.lerp(*bg, l);
            Rgba32F::new(mapped.r, mapped.g, mapped.b, a)
        }
    };

    Rgba32F::new(
        r + (adjusted.r - r) * opacity,
        g + (adjusted.g - g) * opacity,
        b + (adjusted.b - b) * opacity,
        a,
    )
}

/// Apply an adjustment to every pixel in the current accumulator.
///
/// The alpha channel is sanitized but otherwise left unchanged. RGB channels
/// are clamped to `[0.0, 1.0]`. The adjustment is blended by the layer's
/// opacity: opacity `0` leaves the accumulator unchanged; opacity `1` applies
/// the adjustment fully.
fn apply_adjustment(accum: &mut [Rgba32F], kind: &AdjustmentKind, opacity: f32) {
    // Sanitize non-finite parameters once so a corrupt adjustment cannot inject
    // NaN/inf into the accumulator.
    let kind = sanitize_adjustment(kind);
    accum
        .par_iter_mut()
        .for_each(|px| *px = apply_adjustment_pixel(&kind, opacity, *px));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::TiledBuffer;
    use crate::document::Document;
    use crate::history::{AddAdjustmentLayerCmd, History, SetAdjustmentCmd};
    use crate::layer::{AdjustmentKind, BlendMode, Layer};
    use crate::pixel::Rgba32F;

    fn assert_pixel_approx_eq(a: Rgba32F, b: Rgba32F, eps: f32) {
        assert!(
            (a.r - b.r).abs() < eps
                && (a.g - b.g).abs() < eps
                && (a.b - b.b).abs() < eps
                && (a.a - b.a).abs() < eps,
            "{:?} != {:?} within {}",
            a,
            b,
            eps
        );
    }

    #[test]
    fn adjustment_preserves_hidden_rgb_of_fully_transparent_pixel() {
        // The GPU adjustment shader leaves RGB data unchanged when alpha is zero.
        // The CPU reference must match so golden tests remain invariant for all
        // inputs, including pixels with hidden RGB data.
        let hidden = Rgba32F::new(0.2, 0.4, 0.6, 0.0);
        let out = apply_adjustment_pixel(&AdjustmentKind::Invert, 1.0, hidden);
        assert_pixel_approx_eq(out, hidden, 1e-5);
    }

    #[test]
    fn red_opaque_under_half_blue_is_porter_duff_over() {
        let red = Rgba32F::new(1.0, 0.0, 0.0, 1.0);
        let blue = Rgba32F::new(0.0, 0.0, 1.0, 0.5);

        // Blue over an opaque red background: Porter-Duff "over" mixes the
        // source with the destination according to the source alpha.
        let over_red = blend_pixel(BlendMode::Normal, red, blue, 1.0);
        assert_pixel_approx_eq(over_red, Rgba32F::new(0.5, 0.0, 0.5, 1.0), 1e-5);

        // Blue over a transparent background: the result is just the source.
        let over_clear = blend_pixel(BlendMode::Normal, Rgba32F::TRANSPARENT, blue, 1.0);
        assert_pixel_approx_eq(over_clear, Rgba32F::new(0.0, 0.0, 1.0, 0.5), 1e-5);
    }

    #[test]
    fn each_blend_mode_matches_hand_computed_value() {
        let dst = Rgba32F::new(0.5, 0.25, 0.75, 1.0);
        let src = Rgba32F::new(0.25, 0.75, 0.5, 0.5);

        // With dst_alpha == 1.0, the over formula reduces to:
        // result = dst * (1 - src_a) + src_a * blend(dst, src)
        let cases: &[(BlendMode, Rgba32F)] = &[
            (BlendMode::Normal, Rgba32F::new(0.375, 0.5, 0.625, 1.0)),
            (
                BlendMode::Multiply,
                Rgba32F::new(0.3125, 0.21875, 0.5625, 1.0),
            ),
            (
                BlendMode::Screen,
                Rgba32F::new(0.5625, 0.53125, 0.8125, 1.0),
            ),
            (BlendMode::Overlay, Rgba32F::new(0.375, 0.3125, 0.75, 1.0)),
            (BlendMode::Darken, Rgba32F::new(0.375, 0.25, 0.625, 1.0)),
            (BlendMode::Lighten, Rgba32F::new(0.5, 0.5, 0.75, 1.0)),
            (
                BlendMode::ColorDodge,
                Rgba32F::new(0.5833333, 0.625, 0.875, 1.0),
            ),
            (BlendMode::ColorBurn, Rgba32F::new(0.25, 0.125, 0.625, 1.0)),
            (BlendMode::HardLight, Rgba32F::new(0.375, 0.4375, 0.75, 1.0)),
            (
                BlendMode::SoftLight,
                Rgba32F::new(0.4375, 0.3125, 0.75, 1.0),
            ),
            (BlendMode::Difference, Rgba32F::new(0.375, 0.375, 0.5, 1.0)),
            (BlendMode::Exclusion, Rgba32F::new(0.5, 0.4375, 0.625, 1.0)),
            (BlendMode::Add, Rgba32F::new(0.625, 0.625, 1.0, 1.0)),
        ];

        for (mode, expected) in cases {
            let actual = blend_pixel(*mode, dst, src, 1.0);
            assert_pixel_approx_eq(actual, *expected, 1e-5);
        }
    }

    #[test]
    fn invisible_and_zero_opacity_layers_contribute_nothing() {
        let mut doc = Document::new(10, 10);
        let bg = doc.add_raster_layer("bg");
        doc.layer_mut(bg)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        let hidden = doc.add_raster_layer("hidden");
        doc.layer_mut(hidden).unwrap().visible = false;
        doc.layer_mut(hidden)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(0.0, 1.0, 0.0, 1.0));

        let transparent = doc.add_raster_layer("transparent");
        doc.layer_mut(transparent).unwrap().opacity = 0.0;
        doc.layer_mut(transparent)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(0.0, 0.0, 1.0, 1.0));

        let result = composite_document(&doc, Rect::new(5, 5, 1, 1)).unwrap();
        assert_eq!(result[0], Rgba32F::new(1.0, 0.0, 0.0, 1.0));
    }

    #[test]
    fn composite_document_respects_layer_offset() {
        let mut doc = Document::new(20, 20);
        let id = doc.add_raster_layer("L");
        doc.layer_mut(id).unwrap().offset = IVec2::new(5, 5);
        doc.layer_mut(id)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(0, 0), Rgba32F::new(0.0, 1.0, 0.0, 1.0));

        let result = composite_document(&doc, Rect::new(5, 5, 1, 1)).unwrap();
        assert_eq!(result[0], Rgba32F::new(0.0, 1.0, 0.0, 1.0));

        let outside = composite_document(&doc, Rect::new(0, 0, 1, 1)).unwrap();
        assert_eq!(outside[0], Rgba32F::TRANSPARENT);
    }

    #[test]
    fn composite_document_respects_layer_opacity() {
        let mut doc = Document::new(10, 10);
        let bg = doc.add_raster_layer("bg");
        doc.layer_mut(bg)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(3, 3), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        let fg = doc.add_raster_layer("fg");
        doc.layer_mut(fg).unwrap().opacity = 0.5;
        doc.layer_mut(fg)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(3, 3), Rgba32F::new(0.0, 0.0, 1.0, 1.0));

        let result = composite_document(&doc, Rect::new(3, 3, 1, 1)).unwrap()[0];
        let expected = blend_pixel(
            BlendMode::Normal,
            Rgba32F::new(1.0, 0.0, 0.0, 1.0),
            Rgba32F::new(0.0, 0.0, 1.0, 1.0),
            0.5,
        );
        assert_pixel_approx_eq(result, expected, 1e-5);
    }

    #[test]
    fn group_composites_children_then_applies_group_opacity() {
        let mut doc = Document::new(10, 10);
        let bg = doc.add_raster_layer("bg");
        doc.layer_mut(bg)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        let group = Layer::new_group("group");
        let group_id = doc.insert_layer_above(group, bg).unwrap();
        doc.layer_mut(group_id).unwrap().opacity = 0.5;

        let child = doc.add_raster_layer("child");
        doc.move_into_group(child, group_id, 0).unwrap();
        doc.layer_mut(child)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(0.0, 0.0, 1.0, 1.0));

        // Without the group opacity, the child blue would fully replace the red
        // background at this pixel. With group opacity 0.5 the result is 50%
        // blended.
        let result = composite_document(&doc, Rect::new(5, 5, 1, 1)).unwrap()[0];
        let expected = blend_pixel(
            BlendMode::Normal,
            Rgba32F::new(1.0, 0.0, 0.0, 1.0),
            Rgba32F::new(0.0, 0.0, 1.0, 1.0),
            0.5,
        );
        assert_pixel_approx_eq(result, expected, 1e-5);
    }

    #[test]
    fn invisible_group_skips_children() {
        let mut doc = Document::new(10, 10);
        let bg = doc.add_raster_layer("bg");
        doc.layer_mut(bg)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        let group = Layer::new_group("group");
        let group_id = doc.insert_layer_above(group, bg).unwrap();
        doc.layer_mut(group_id).unwrap().visible = false;

        let child = doc.add_raster_layer("child");
        doc.move_into_group(child, group_id, 0).unwrap();
        doc.layer_mut(child)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(0.0, 1.0, 0.0, 1.0));

        let result = composite_document(&doc, Rect::new(5, 5, 1, 1)).unwrap();
        assert_eq!(result[0], Rgba32F::new(1.0, 0.0, 0.0, 1.0));
    }

    #[test]
    fn empty_region_returns_empty_buffer() {
        let doc = Document::new(10, 10);
        assert!(composite_document(&doc, Rect::new(0, 0, 0, 10))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn blend_pixel_with_nan_opacity_treats_as_zero() {
        let result = blend_pixel(
            BlendMode::Normal,
            Rgba32F::new(1.0, 0.0, 0.0, 1.0),
            Rgba32F::new(0.0, 0.0, 1.0, 1.0),
            f32::NAN,
        );
        assert_eq!(result, Rgba32F::new(1.0, 0.0, 0.0, 1.0));
    }

    #[test]
    fn blend_pixel_with_zero_alpha_dst_returns_source_scaled() {
        let result = blend_pixel(
            BlendMode::Normal,
            Rgba32F::TRANSPARENT,
            Rgba32F::new(0.2, 0.4, 0.6, 0.5),
            1.0,
        );
        assert_pixel_approx_eq(result, Rgba32F::new(0.2, 0.4, 0.6, 0.5), 1e-5);
    }

    #[test]
    fn group_with_non_normal_blend_mode() {
        let mut doc = Document::new(10, 10);
        let bg = doc.add_raster_layer("bg");
        doc.layer_mut(bg)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        let group = Layer::new_group("group");
        let group_id = doc.insert_layer_above(group, bg).unwrap();
        doc.layer_mut(group_id).unwrap().blend = BlendMode::Multiply;

        let child = doc.add_raster_layer("child");
        doc.move_into_group(child, group_id, 0).unwrap();
        doc.layer_mut(child)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(0.0, 1.0, 0.0, 1.0));

        let result = composite_document(&doc, Rect::new(5, 5, 1, 1)).unwrap()[0];
        let expected = blend_pixel(
            BlendMode::Multiply,
            Rgba32F::new(1.0, 0.0, 0.0, 1.0),
            Rgba32F::new(0.0, 1.0, 0.0, 1.0),
            1.0,
        );
        assert_pixel_approx_eq(result, expected, 1e-5);
    }

    #[test]
    fn nested_group_composites_children() {
        let mut doc = Document::new(10, 10);
        let bg = doc.add_raster_layer("bg");
        doc.layer_mut(bg)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        let outer = Layer::new_group("outer");
        let outer_id = doc.insert_layer_above(outer, bg).unwrap();
        doc.layer_mut(outer_id).unwrap().opacity = 0.5;

        let inner = Layer::new_group("inner");
        let inner_id = doc.insert_layer_above(inner, outer_id).unwrap();
        doc.move_into_group(inner_id, outer_id, 0).unwrap();

        let child = doc.add_raster_layer("child");
        doc.move_into_group(child, inner_id, 0).unwrap();
        doc.layer_mut(child)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(0.0, 0.0, 1.0, 1.0));

        let result = composite_document(&doc, Rect::new(5, 5, 1, 1)).unwrap()[0];
        let expected = blend_pixel(
            BlendMode::Normal,
            Rgba32F::new(1.0, 0.0, 0.0, 1.0),
            Rgba32F::new(0.0, 0.0, 1.0, 1.0),
            0.5,
        );
        assert_pixel_approx_eq(result, expected, 1e-5);
    }

    #[test]
    fn zero_opacity_group_contributes_nothing() {
        let mut doc = Document::new(10, 10);
        let bg = doc.add_raster_layer("bg");
        doc.layer_mut(bg)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        let group = Layer::new_group("group");
        let group_id = doc.insert_layer_above(group, bg).unwrap();
        doc.layer_mut(group_id).unwrap().opacity = 0.0;

        let child = doc.add_raster_layer("child");
        doc.move_into_group(child, group_id, 0).unwrap();
        doc.layer_mut(child)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(0.0, 1.0, 0.0, 1.0));

        let result = composite_document(&doc, Rect::new(5, 5, 1, 1)).unwrap();
        assert_eq!(result[0], Rgba32F::new(1.0, 0.0, 0.0, 1.0));
    }

    #[test]
    fn move_layer_by_on_group_shifts_composited_result() {
        let mut doc = Document::new(50, 50);
        let bg = doc.add_raster_layer("bg");
        doc.layer_mut(bg)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        let group = Layer::new_group("group");
        let group_id = doc.insert_layer_above(group, bg).unwrap();
        doc.layer_mut(group_id).unwrap().offset = IVec2::new(5, 5);

        let child = doc.add_raster_layer("child");
        doc.move_into_group(child, group_id, 0).unwrap();
        doc.layer_mut(child)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(0, 0), Rgba32F::new(0.0, 1.0, 0.0, 1.0));

        // Child local (0,0) + group offset (5,5) lands at document (5,5).
        let before = composite_document(&doc, Rect::new(5, 5, 1, 1)).unwrap();
        assert_eq!(before[0], Rgba32F::new(0.0, 1.0, 0.0, 1.0));

        crate::ops::move_layer_by(&mut doc, group_id, IVec2::new(3, 7)).unwrap();
        assert_eq!(doc.layer(group_id).unwrap().offset, IVec2::new(8, 12));

        let after = composite_document(&doc, Rect::new(8, 12, 1, 1)).unwrap();
        assert_eq!(after[0], Rgba32F::new(0.0, 1.0, 0.0, 1.0));

        let old_spot = composite_document(&doc, Rect::new(5, 5, 1, 1)).unwrap();
        assert_eq!(old_spot[0], Rgba32F::new(1.0, 0.0, 0.0, 1.0));
    }

    #[test]
    fn composite_document_extreme_layer_offset_returns_transparent() {
        let mut doc = Document::new(10, 10);
        let id = doc.add_raster_layer("L");
        doc.layer_mut(id).unwrap().offset = IVec2::new(i32::MAX, i32::MAX);
        doc.layer_mut(id)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(0, 0), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        let result = composite_document(&doc, Rect::new(0, 0, 10, 10)).unwrap();
        assert!(result.iter().all(|p| *p == Rgba32F::TRANSPARENT));
    }

    #[test]
    fn composite_document_extreme_group_offset_returns_transparent() {
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
            .set_pixel(IVec2::new(0, 0), Rgba32F::new(0.0, 1.0, 0.0, 1.0));

        // The group's children should be transparent, leaving only the background.
        let result = composite_document(&doc, Rect::new(5, 5, 1, 1)).unwrap();
        assert_eq!(result[0], Rgba32F::new(1.0, 0.0, 0.0, 1.0));
    }

    #[test]
    fn composite_document_propagates_cycle_error() {
        let mut doc = Document::new(10, 10);
        let bg = doc.add_raster_layer("bg");
        let parent = Layer::new_group("parent");
        let parent_id = doc.insert_layer_above(parent, bg).unwrap();
        let child = Layer::new_group("child");
        let child_id = doc.insert_layer_above(child, parent_id).unwrap();
        doc.move_into_group(child_id, parent_id, 0).unwrap();

        // Intentionally corrupt the tree to create p -> c -> p.
        doc.layer_mut(child_id).unwrap().content = LayerContent::Group {
            children: vec![parent_id],
        };

        assert!(matches!(
            composite_document(&doc, Rect::new(0, 0, 10, 10)),
            Err(OgreError::InvalidOperation("cycle in layer tree"))
        ));
    }

    // ------------------------------------------------------------------
    // Adjustment layers
    // ------------------------------------------------------------------

    #[test]
    fn adjustment_layer_invert_turns_red_to_cyan() {
        let mut doc = Document::new(10, 10);
        let bg = doc.add_raster_layer("bg");
        doc.layer_mut(bg)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        doc.add_adjustment_layer("invert", AdjustmentKind::Invert);

        let result = composite_document(&doc, Rect::new(5, 5, 1, 1)).unwrap();
        assert_eq!(result[0], Rgba32F::new(0.0, 1.0, 1.0, 1.0));
    }

    #[test]
    fn adjustment_with_nan_params_does_not_produce_nan() {
        // Adjustment parameters can arrive from a corrupt `.ogre` file. A NaN
        // must not propagate into composited pixels.
        let mut doc = Document::new(4, 4);
        let bg = doc.add_raster_layer("bg");
        for y in 0..4 {
            for x in 0..4 {
                doc.layer_mut(bg)
                    .unwrap()
                    .buffer_mut()
                    .unwrap()
                    .set_pixel(IVec2::new(x, y), Rgba32F::new(0.5, 0.5, 0.5, 1.0));
            }
        }
        doc.add_adjustment_layer(
            "levels",
            AdjustmentKind::Levels {
                input_black: f32::NAN,
                input_white: 1.0,
                output_black: 0.0,
                output_white: 1.0,
                gamma: f32::NAN,
            },
        );

        let out = composite_document(&doc, Rect::new(0, 0, 4, 4)).unwrap();
        for px in &out {
            assert!(
                px.r.is_finite() && px.g.is_finite() && px.b.is_finite() && px.a.is_finite(),
                "adjustment must not produce NaN/inf pixels: {px:?}"
            );
        }
    }

    #[test]
    fn adjustment_layer_desaturate_turns_red_to_gray() {
        let mut doc = Document::new(10, 10);
        let bg = doc.add_raster_layer("bg");
        doc.layer_mut(bg)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        doc.add_adjustment_layer("desaturate", AdjustmentKind::Desaturate);

        let result = composite_document(&doc, Rect::new(5, 5, 1, 1)).unwrap()[0];
        let expected_luma = 0.2126;
        assert_pixel_approx_eq(
            result,
            Rgba32F::new(expected_luma, expected_luma, expected_luma, 1.0),
            1e-5,
        );
    }

    #[test]
    fn adjustment_layer_brightness_contrast_identity_and_max_brightness() {
        let mut doc = Document::new(10, 10);
        let bg = doc.add_raster_layer("bg");
        doc.layer_mut(bg)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(1.0, 0.25, 0.5, 1.0));

        doc.add_adjustment_layer(
            "bc identity",
            AdjustmentKind::BrightnessContrast {
                brightness: 0.0,
                contrast: 1.0,
            },
        );

        let identity = composite_document(&doc, Rect::new(5, 5, 1, 1)).unwrap()[0];
        assert_pixel_approx_eq(identity, Rgba32F::new(1.0, 0.25, 0.5, 1.0), 1e-5);

        // Change the adjustment to maximum brightness; everything becomes white.
        let adjustment = doc.active.unwrap();
        doc.layer_mut(adjustment).unwrap().content =
            LayerContent::Adjustment(AdjustmentKind::BrightnessContrast {
                brightness: 1.0,
                contrast: 1.0,
            });

        let white = composite_document(&doc, Rect::new(5, 5, 1, 1)).unwrap()[0];
        assert_pixel_approx_eq(white, Rgba32F::new(1.0, 1.0, 1.0, 1.0), 1e-5);
    }

    #[test]
    fn add_adjustment_layer_cmd_undo_restores_original_composite() {
        let mut doc = Document::new(10, 10);
        let bg = doc.add_raster_layer("bg");
        doc.layer_mut(bg)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        let before = composite_document(&doc, Rect::new(5, 5, 1, 1)).unwrap();

        let mut history = History::new(0);
        history
            .do_command(
                &mut doc,
                Box::new(AddAdjustmentLayerCmd::new("invert", AdjustmentKind::Invert)),
            )
            .unwrap();

        let with_adjustment = composite_document(&doc, Rect::new(5, 5, 1, 1)).unwrap();
        assert_eq!(with_adjustment[0], Rgba32F::new(0.0, 1.0, 1.0, 1.0));

        history.undo(&mut doc);
        let after_undo = composite_document(&doc, Rect::new(5, 5, 1, 1)).unwrap();
        assert_eq!(before, after_undo);
    }

    #[test]
    fn set_adjustment_cmd_undo_restores_previous_kind() {
        let mut doc = Document::new(10, 10);
        let bg = doc.add_raster_layer("bg");
        doc.layer_mut(bg)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        let adj = doc.add_adjustment_layer("adj", AdjustmentKind::Invert);
        let inverted = composite_document(&doc, Rect::new(5, 5, 1, 1)).unwrap();

        let mut history = History::new(0);
        history
            .do_command(
                &mut doc,
                Box::new(SetAdjustmentCmd::new(adj, AdjustmentKind::Desaturate)),
            )
            .unwrap();

        let desaturated = composite_document(&doc, Rect::new(5, 5, 1, 1)).unwrap();
        assert!(
            (desaturated[0].r - desaturated[0].g).abs() < 1e-5
                && (desaturated[0].g - desaturated[0].b).abs() < 1e-5
        );
        assert_ne!(inverted, desaturated);

        history.undo(&mut doc);
        let restored = composite_document(&doc, Rect::new(5, 5, 1, 1)).unwrap();
        assert_eq!(inverted, restored);
    }

    // ------------------------------------------------------------------
    // sample_document_pixel regression tests
    // ------------------------------------------------------------------

    fn assert_sample_matches_composite(doc: &Document, pos: IVec2) {
        let sample = sample_document_pixel(doc, pos);
        if !doc.canvas.contains(pos) {
            assert!(sample.is_none(), "expected None for {pos:?} outside canvas");
            return;
        }
        let composite = composite_document(doc, Rect::new(pos.x, pos.y, 1, 1)).unwrap();
        assert_eq!(composite.len(), 1);
        assert_pixel_approx_eq(sample.unwrap(), composite[0], 1e-5);
    }

    #[test]
    fn sample_document_pixel_opaque_layer() {
        let mut doc = Document::new(10, 10);
        let bg = doc.add_raster_layer("bg");
        doc.layer_mut(bg)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        assert_sample_matches_composite(&doc, IVec2::new(5, 5));
    }

    #[test]
    fn sample_document_pixel_opacity_and_blend_mode() {
        let mut doc = Document::new(10, 10);
        let bg = doc.add_raster_layer("bg");
        doc.layer_mut(bg)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(3, 3), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        let fg = doc.add_raster_layer("fg");
        doc.layer_mut(fg).unwrap().opacity = 0.5;
        doc.layer_mut(fg).unwrap().blend = BlendMode::Multiply;
        doc.layer_mut(fg)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(3, 3), Rgba32F::new(0.0, 0.0, 1.0, 1.0));

        assert_sample_matches_composite(&doc, IVec2::new(3, 3));
    }

    #[test]
    fn sample_document_pixel_layer_offset() {
        let mut doc = Document::new(20, 20);
        let id = doc.add_raster_layer("L");
        doc.layer_mut(id).unwrap().offset = IVec2::new(5, 5);
        doc.layer_mut(id)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(0, 0), Rgba32F::new(0.0, 1.0, 0.0, 1.0));

        assert_sample_matches_composite(&doc, IVec2::new(5, 5));
        assert_sample_matches_composite(&doc, IVec2::new(0, 0));
    }

    #[test]
    fn sample_document_pixel_raster_mask() {
        let mut doc = Document::new(10, 10);
        let id = doc.add_raster_layer("L");
        doc.layer_mut(id)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(3, 3), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        let mut mask = TiledBuffer::new();
        mask.set_pixel(IVec2::new(3, 3), Rgba32F::new(0.25, 0.0, 0.0, 1.0));
        if let LayerContent::Raster {
            mask: ref mut m, ..
        } = doc.layer_mut(id).unwrap().content
        {
            *m = Some(mask);
        }

        assert_sample_matches_composite(&doc, IVec2::new(3, 3));
    }

    #[test]
    fn sample_document_pixel_group_with_opacity() {
        let mut doc = Document::new(10, 10);
        let bg = doc.add_raster_layer("bg");
        doc.layer_mut(bg)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        let group = Layer::new_group("group");
        let group_id = doc.insert_layer_above(group, bg).unwrap();
        doc.layer_mut(group_id).unwrap().opacity = 0.5;

        let child = doc.add_raster_layer("child");
        doc.move_into_group(child, group_id, 0).unwrap();
        doc.layer_mut(child)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(0.0, 0.0, 1.0, 1.0));

        assert_sample_matches_composite(&doc, IVec2::new(5, 5));
    }

    #[test]
    fn sample_document_pixel_adjustment_layer() {
        let mut doc = Document::new(10, 10);
        let bg = doc.add_raster_layer("bg");
        doc.layer_mut(bg)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        doc.add_adjustment_layer("invert", AdjustmentKind::Invert);
        assert_sample_matches_composite(&doc, IVec2::new(5, 5));
    }

    #[test]
    fn sample_document_pixel_outside_canvas_is_none() {
        let doc = Document::new(10, 10);
        assert!(sample_document_pixel(&doc, IVec2::new(-1, 0)).is_none());
        assert!(sample_document_pixel(&doc, IVec2::new(0, -1)).is_none());
        assert!(sample_document_pixel(&doc, IVec2::new(10, 0)).is_none());
        assert!(sample_document_pixel(&doc, IVec2::new(0, 10)).is_none());
    }

    fn two_layer_doc() -> Document {
        use crate::coord::IVec2;
        let mut doc = Document::new(8, 6);
        let bg = doc.add_raster_layer("bg");
        let buf = doc.layer_mut(bg).unwrap().buffer_mut().unwrap();
        for y in 0..6 {
            for x in 0..8 {
                buf.set_pixel(IVec2::new(x, y), Rgba32F::new(0.0, 0.0, 1.0, 1.0));
            }
        }
        let top = doc.add_raster_layer("top");
        doc.layer_mut(top).unwrap().opacity = 0.5;
        let buf = doc.layer_mut(top).unwrap().buffer_mut().unwrap();
        for y in 0..3 {
            for x in 0..4 {
                buf.set_pixel(IVec2::new(x, y), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
            }
        }
        doc
    }

    #[test]
    fn composite_region_matches_composite_document() {
        let doc = two_layer_doc();
        let region = Rect::new(0, 0, 4, 3);
        let tiled = composite_region(&doc, region).unwrap();
        let vec_form = composite_document(&doc, region).unwrap();
        assert_eq!(vec_form.len(), 12);
        for py in 0..3 {
            for px in 0..4 {
                let idx = (py * 4 + px) as usize;
                assert_eq!(
                    tiled.get_pixel(IVec2::new(px, py)),
                    vec_form[idx],
                    "mismatch at ({px},{py})"
                );
            }
        }
        // Independent blend check: red@50% over blue = (0.5, 0, 0.5, 1).
        let expected = blend_pixel(
            BlendMode::Normal,
            Rgba32F::new(0.0, 0.0, 1.0, 1.0),
            Rgba32F::new(1.0, 0.0, 0.0, 1.0),
            0.5,
        );
        assert_eq!(tiled.get_pixel(IVec2::new(0, 0)), expected);
    }

    #[test]
    fn composite_region_empty_for_out_of_canvas() {
        let doc = Document::new(4, 4);
        let tiled = composite_region(&doc, Rect::new(100, 100, 4, 4)).unwrap();
        assert!(tiled.is_empty());
    }

    #[test]
    fn composite_region_zero_area_is_empty() {
        let doc = Document::new(4, 4);
        let tiled = composite_region(&doc, Rect::new(0, 0, 0, 5)).unwrap();
        assert!(tiled.is_empty());
    }

    #[test]
    fn composite_region_clips_to_canvas() {
        let doc = two_layer_doc();
        // 10×10 region over an 8×6 canvas clips to the full canvas.
        let tiled = composite_region(&doc, Rect::new(0, 0, 10, 10)).unwrap();
        // Matches composite_document over the clipped (8×6) region.
        let vec_form = composite_document(&doc, Rect::new(0, 0, 8, 6)).unwrap();
        for py in 0..6 {
            for px in 0..8 {
                let idx = (py * 8 + px) as usize;
                assert_eq!(tiled.get_pixel(IVec2::new(px, py)), vec_form[idx]);
            }
        }
    }

    /// Spec test #3: a vector layer composites identically to an equivalent
    /// raster layer. The vector layer's scanline fill of an axis-aligned
    /// integer rectangle must produce the same pixels as a raster layer with
    /// the same rectangle filled directly.
    #[test]
    fn composite_document_vector_layer_matches_equivalent_raster() {
        let fill = Rgba32F::new(0.2, 0.4, 0.6, 1.0);
        let offset = IVec2::new(3, 4);
        let region = Rect::new(0, 0, 20, 20);

        // Vector document: one vector layer with a filled 8×6 rectangle path
        // (vertices in layer-local space).
        let mut vdoc = Document::new(20, 20);
        let vdata = VectorData {
            paths: vec![crate::layer::VectorPath {
                vertices: vec![
                    IVec2::new(0, 0),
                    IVec2::new(8, 0),
                    IVec2::new(8, 6),
                    IVec2::new(0, 6),
                ],
                fill: crate::layer::VectorFill::Solid(fill),
                stroke: crate::layer::VectorStroke {
                    color: Rgba32F::TRANSPARENT,
                    width: 0.0,
                    dash: Vec::new(),
                    cap: crate::layer::StrokeCap::Butt,
                    join: crate::layer::StrokeJoin::Miter,
                },
                closed: true,
            }],
            ..Default::default()
        };
        let vid = vdoc.add_vector_layer("Rect", vdata);
        vdoc.layer_mut(vid).unwrap().offset = offset;

        // Raster document: one raster layer with the same rectangle filled.
        let mut rdoc = Document::new(20, 20);
        let rid = rdoc.add_raster_layer("Rect");
        rdoc.layer_mut(rid).unwrap().offset = offset;
        for y in 0..6 {
            for x in 0..8 {
                rdoc.layer_mut(rid)
                    .unwrap()
                    .buffer_mut()
                    .unwrap()
                    .set_pixel(IVec2::new(x, y), fill);
            }
        }

        let v = composite_document(&vdoc, region).unwrap();
        let r = composite_document(&rdoc, region).unwrap();
        // Every pixel must match within the standard tolerance.
        let mut max_diff = 0.0f32;
        let mut mismatches = 0usize;
        for (vp, rp) in v.iter().zip(r.iter()) {
            let d = (vp.r - rp.r)
                .abs()
                .max((vp.g - rp.g).abs())
                .max((vp.b - rp.b).abs())
                .max((vp.a - rp.a).abs());
            if d > 1e-4 {
                mismatches += 1;
            }
            max_diff = max_diff.max(d);
        }
        assert_eq!(
            mismatches, 0,
            "vector composite must match raster composite within 1e-4 \
             (max per-channel diff was {max_diff})"
        );

        // Spot-check: the filled interior at the offset is the fill color,
        // and outside the rectangle is transparent.
        let at = |x: i32, y: i32| v[(y * 20 + x) as usize];
        assert_pixel_approx_eq(at(3, 4), fill, 1e-5);
        assert_pixel_approx_eq(at(10, 9), fill, 1e-5);
        assert_eq!(at(11, 10), Rgba32F::TRANSPARENT);
        assert_eq!(at(2, 3), Rgba32F::TRANSPARENT);
    }

    /// A stroked vector layer composites with the stroke color along its outline.
    #[test]
    fn composite_document_vector_layer_renders_stroke() {
        let stroke = Rgba32F::new(1.0, 0.0, 0.0, 1.0);
        let mut doc = Document::new(12, 12);
        let vdata = VectorData {
            paths: vec![crate::layer::VectorPath {
                vertices: vec![
                    IVec2::new(2, 2),
                    IVec2::new(9, 2),
                    IVec2::new(9, 9),
                    IVec2::new(2, 9),
                ],
                fill: crate::layer::VectorFill::None,
                stroke: crate::layer::VectorStroke {
                    color: stroke,
                    width: 1.0,
                    dash: Vec::new(),
                    cap: crate::layer::StrokeCap::Butt,
                    join: crate::layer::StrokeJoin::Miter,
                },
                closed: true,
            }],
            ..Default::default()
        };
        let vid = doc.add_vector_layer("Frame", vdata);
        let _ = vid;
        let result = composite_document(&doc, Rect::new(0, 0, 12, 12)).unwrap();
        let at = |x: i32, y: i32| result[(y * 12 + x) as usize];
        // Top edge of the frame is stroked.
        assert_pixel_approx_eq(at(2, 2), stroke, 1e-5);
        assert_pixel_approx_eq(at(9, 2), stroke, 1e-5);
        // Interior is empty (transparent fill, no stroke reaches center).
        assert_eq!(at(5, 5), Rgba32F::TRANSPARENT);
    }
}
