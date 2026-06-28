// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC
//! CPU thumbnail generation for Bird's Eye View.
//!
//! Each thumbnail shows a single root layer in isolation, composited over a
//! checkerboard and downsampled into a fixed-size square tile. Generation is
//! pure (it clones the document and never mutates the live one) and uses the
//! `ogre_core` CPU compositor — no GPU state is involved.
//!
//! Color handling follows the straight-alpha contract: linear-light layer
//! pixels are blended over the checkerboard in linear space, then encoded to
//! sRGB bytes with alpha kept unpremultiplied for
//! [`egui::ColorImage::from_rgba_unmultiplied`].

use ogre_core::{
    rasterize_vector_data, sample_document_pixel, srgb_encode, Document, IVec2, LayerContent,
    LayerId, Rect, Rgba32F,
};

/// Side length, in pixels, of a generated square thumbnail tile.
pub const THUMB_SIZE: usize = 128;

/// Edge of a single checkerboard cell, in thumbnail pixels.
const CHECKER_CELL: usize = 8;
/// Linear-light grey of the lighter checkerboard cell.
const CHECKER_LIGHT: f32 = 0.20;
/// Linear-light grey of the darker checkerboard cell.
const CHECKER_DARK: f32 = 0.10;

/// Clamp to `[0, 1]`, mapping NaN to `0.0`.
fn clamp01(v: f32) -> f32 {
    if v.is_nan() {
        0.0
    } else {
        v.clamp(0.0, 1.0)
    }
}

/// Encode a linear-light channel value to an sRGB byte.
fn srgb_byte(linear: f32) -> u8 {
    (srgb_encode(linear) * 255.0).round().clamp(0.0, 255.0) as u8
}

/// Convert a straight-alpha, linear-light pixel to sRGB RGBA bytes **without**
/// premultiplying: RGB is gamma-encoded and alpha is preserved as-is. This is
/// the correct input for [`egui::ColorImage::from_rgba_unmultiplied`].
fn to_srgb_unmultiplied(px: Rgba32F) -> [u8; 4] {
    [
        srgb_byte(px.r),
        srgb_byte(px.g),
        srgb_byte(px.b),
        (clamp01(px.a) * 255.0).round() as u8,
    ]
}

/// The opaque checkerboard pixel for thumbnail coordinate `(x, y)`.
fn checker_pixel(x: usize, y: usize) -> Rgba32F {
    let v = if (x / CHECKER_CELL + y / CHECKER_CELL).is_multiple_of(2) {
        CHECKER_LIGHT
    } else {
        CHECKER_DARK
    };
    Rgba32F::new(v, v, v, 1.0)
}

/// Straight-alpha "over": composite `top` onto an opaque `bottom`. The result
/// is opaque.
fn over_opaque(top: Rgba32F, bottom: Rgba32F) -> Rgba32F {
    let a = clamp01(top.a);
    Rgba32F::new(
        top.r * a + bottom.r * (1.0 - a),
        top.g * a + bottom.g * (1.0 - a),
        top.b * a + bottom.b * (1.0 - a),
        1.0,
    )
}

/// Sample one destination cell of the layer by averaging an `ss_x` by `ss_y`
/// grid of document samples taken from the isolated clone. Averaging is done in
/// premultiplied space so transparent samples do not pull color toward black.
///
/// Per-pixel document sampling (rather than compositing the whole source rect
/// into a buffer) keeps thumbnail memory bounded to the output size even when
/// the source layer covers a very large canvas.
#[allow(clippy::too_many_arguments)]
fn sample_cell(
    doc: &Document,
    rect: Rect,
    dx: usize,
    dy: usize,
    dst_w: usize,
    dst_h: usize,
    ss_x: usize,
    ss_y: usize,
) -> Rgba32F {
    let mut pr = 0.0f32;
    let mut pg = 0.0f32;
    let mut pb = 0.0f32;
    let mut pa = 0.0f32;
    let mut count = 0u32;
    let max_x = rect.x + rect.w as i32 - 1;
    let max_y = rect.y + rect.h as i32 - 1;
    for sj in 0..ss_y {
        for si in 0..ss_x {
            let fx = (dx as f32 + (si as f32 + 0.5) / ss_x as f32) / dst_w as f32;
            let fy = (dy as f32 + (sj as f32 + 0.5) / ss_y as f32) / dst_h as f32;
            let sx = (rect.x as f32 + fx * rect.w as f32).floor() as i32;
            let sy = (rect.y as f32 + fy * rect.h as f32).floor() as i32;
            let pos = IVec2::new(sx.clamp(rect.x, max_x), sy.clamp(rect.y, max_y));
            let p = sample_document_pixel(doc, pos).unwrap_or(Rgba32F::TRANSPARENT);
            let a = clamp01(p.a);
            pr += p.r * a;
            pg += p.g * a;
            pb += p.b * a;
            pa += a;
            count += 1;
        }
    }
    if count == 0 || pa <= 0.0 {
        return Rgba32F::TRANSPARENT;
    }
    Rgba32F::new(pr / pa, pg / pa, pb / pa, pa / count as f32)
}

/// Translate a local-space bounds rectangle into document space.
fn offset_rect(rect: Rect, offset: IVec2) -> Option<Rect> {
    Some(Rect::new(
        rect.x.checked_add(offset.x)?,
        rect.y.checked_add(offset.y)?,
        rect.w,
        rect.h,
    ))
}

/// Exact visual bounds for a layer in document space, including group offsets.
fn layer_visual_bounds(doc: &Document, id: LayerId, parent_offset: IVec2) -> Option<Rect> {
    let layer = doc.layer(id).ok()?;
    if !layer.visible {
        return None;
    }
    let offset = IVec2::new(
        parent_offset.x.checked_add(layer.offset.x)?,
        parent_offset.y.checked_add(layer.offset.y)?,
    );
    match &layer.content {
        LayerContent::Raster { buffer, .. } => {
            let bounds = buffer.exact_bounds().or_else(|| buffer.bounds())?;
            offset_rect(bounds, offset)
        }
        LayerContent::Vector(data) => {
            let rasterized = rasterize_vector_data(data);
            let bounds = rasterized.exact_bounds().or_else(|| rasterized.bounds())?;
            offset_rect(bounds, offset)
        }
        LayerContent::Group { children } => children
            .iter()
            .filter_map(|&child| layer_visual_bounds(doc, child, offset))
            .fold(None, |acc, rect| {
                Some(acc.map(|prev: Rect| prev.union(rect)).unwrap_or(rect))
            }),
        LayerContent::Adjustment(_) => None,
    }
}

/// Compute the source rectangle (in document space) used to sample a layer's
/// pixels: tight visual bounds when available, otherwise the full canvas.
fn layer_source_rect(doc: &Document, id: LayerId) -> Rect {
    layer_visual_bounds(doc, id, IVec2::ZERO).unwrap_or(doc.canvas)
}

/// Build a document clone with only the target root layer visible and return it
/// alongside the source rectangle to sample, or `None` if the layer contributes
/// nothing inside the canvas.
///
/// Cloning is cheap: tile buffers are copy-on-write through `Arc`, and only the
/// per-layer visibility flags are changed — the live document is never mutated.
fn isolate_layer(doc: &Document, layer_id: LayerId) -> Option<(Document, Rect)> {
    let mut clone = doc.clone();
    // Show only the target root layer; preserve the visibility of any children
    // it owns so a group thumbnail matches its internal state.
    let roots = clone.order.clone();
    for id in roots {
        if let Ok(layer) = clone.layer_mut(id) {
            layer.visible = id == layer_id;
        }
    }
    let rect = layer_source_rect(&clone, layer_id);
    let rect = rect.intersect(clone.canvas).filter(|r| !r.is_empty())?;
    Some((clone, rect))
}

/// Fixed thumbnail-square layout for a source rectangle: destination size and
/// top-left offset that preserves aspect ratio and centers the image.
fn fit_layout(rect: Rect) -> (usize, usize, usize, usize) {
    let rw = rect.w.max(1) as f32;
    let rh = rect.h.max(1) as f32;
    let scale = (THUMB_SIZE as f32 / rw).min(THUMB_SIZE as f32 / rh);
    let dst_w = ((rw * scale).round() as usize).clamp(1, THUMB_SIZE);
    let dst_h = ((rh * scale).round() as usize).clamp(1, THUMB_SIZE);
    let off_x = (THUMB_SIZE - dst_w) / 2;
    let off_y = (THUMB_SIZE - dst_h) / 2;
    (dst_w, dst_h, off_x, off_y)
}

/// Build a thumbnail `ColorImage` by sampling the isolated layer over the
/// checkerboard. The supersample grid adapts to the downscale factor (capped)
/// to reduce aliasing without unbounded cost.
fn pixel_thumbnail(doc: &Document, rect: Rect) -> egui::ColorImage {
    let (dst_w, dst_h, off_x, off_y) = fit_layout(rect);
    let ss_x = (rect.w as usize).div_ceil(dst_w).clamp(1, 4);
    let ss_y = (rect.h as usize).div_ceil(dst_h).clamp(1, 4);
    let mut bytes = Vec::with_capacity(THUMB_SIZE * THUMB_SIZE * 4);
    for oy in 0..THUMB_SIZE {
        for ox in 0..THUMB_SIZE {
            let mut px = checker_pixel(ox, oy);
            if ox >= off_x && ox < off_x + dst_w && oy >= off_y && oy < off_y + dst_h {
                let layer_px =
                    sample_cell(doc, rect, ox - off_x, oy - off_y, dst_w, dst_h, ss_x, ss_y);
                px = over_opaque(layer_px, px);
            }
            bytes.extend_from_slice(&to_srgb_unmultiplied(px));
        }
    }
    egui::ColorImage::from_rgba_unmultiplied([THUMB_SIZE, THUMB_SIZE], &bytes)
}

/// A bare checkerboard tile, used for empty or fully transparent layers.
fn checker_only_thumbnail() -> egui::ColorImage {
    let mut bytes = Vec::with_capacity(THUMB_SIZE * THUMB_SIZE * 4);
    for oy in 0..THUMB_SIZE {
        for ox in 0..THUMB_SIZE {
            bytes.extend_from_slice(&to_srgb_unmultiplied(checker_pixel(ox, oy)));
        }
    }
    egui::ColorImage::from_rgba_unmultiplied([THUMB_SIZE, THUMB_SIZE], &bytes)
}

/// Build a generated thumbnail for an adjustment layer, which has no pixels of
/// its own in isolation. A horizontal black-to-white ramp clearly communicates
/// "tonal adjustment" instead of showing an empty tile.
fn adjustment_thumbnail() -> egui::ColorImage {
    let mut bytes = Vec::with_capacity(THUMB_SIZE * THUMB_SIZE * 4);
    for _oy in 0..THUMB_SIZE {
        for ox in 0..THUMB_SIZE {
            // sRGB ramp directly (perceptually even), full opacity.
            let v = (ox as f32 / (THUMB_SIZE - 1) as f32 * 255.0).round() as u8;
            bytes.extend_from_slice(&[v, v, v, 255]);
        }
    }
    egui::ColorImage::from_rgba_unmultiplied([THUMB_SIZE, THUMB_SIZE], &bytes)
}

/// Generate a `THUMB_SIZE` square thumbnail for the given root layer.
///
/// Raster, vector, and group layers are composited in isolation over a
/// checkerboard. Adjustment layers (which have no isolated pixels) get a
/// generated ramp swatch. Empty or fully transparent layers return a
/// checkerboard-only tile.
pub fn layer_thumbnail(doc: &Document, layer_id: LayerId) -> egui::ColorImage {
    let is_adjustment = matches!(
        doc.layer(layer_id).map(|l| &l.content),
        Ok(LayerContent::Adjustment(_))
    );
    if is_adjustment {
        return adjustment_thumbnail();
    }
    match isolate_layer(doc, layer_id) {
        Some((clone, rect)) => pixel_thumbnail(&clone, rect),
        // Empty / off-canvas layer: a bare checkerboard with no content.
        None => checker_only_thumbnail(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ogre_core::{IVec2, Layer, TiledBuffer};

    fn solid_buffer(w: u32, h: u32, color: Rgba32F) -> TiledBuffer {
        let mut buf = TiledBuffer::new();
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                buf.set_pixel(IVec2::new(x, y), color);
            }
        }
        buf
    }

    #[test]
    fn to_srgb_unmultiplied_keeps_alpha_straight() {
        // A half-transparent white pixel must keep full-white RGB and a ~50%
        // alpha — proving the conversion does not premultiply (which would
        // darken RGB toward black).
        let bytes = to_srgb_unmultiplied(Rgba32F::new(1.0, 1.0, 1.0, 0.5));
        assert_eq!(bytes[0], 255);
        assert_eq!(bytes[1], 255);
        assert_eq!(bytes[2], 255);
        assert!(
            (bytes[3] as i32 - 128).abs() <= 1,
            "alpha must stay ~50%, got {}",
            bytes[3]
        );
    }

    #[test]
    fn large_source_is_downsampled_to_fixed_size() {
        let mut doc = Document::new(2000, 1500);
        let mut layer = Layer::new_raster("big");
        layer.content = LayerContent::Raster {
            buffer: solid_buffer(2000, 1500, Rgba32F::new(0.2, 0.4, 0.8, 1.0)),
            mask: None,
        };
        let id = doc.add_layer(layer);
        doc.order.push(id);
        doc.active = Some(id);

        let img = layer_thumbnail(&doc, id);
        assert_eq!(img.size, [THUMB_SIZE, THUMB_SIZE]);
    }

    #[test]
    fn sparse_raster_source_uses_exact_pixel_bounds() {
        let mut doc = Document::new(512, 512);
        let mut layer = Layer::new_raster("sparse");
        let mut buffer = TiledBuffer::new();
        buffer.set_pixel(IVec2::new(20, 30), Rgba32F::new(0.2, 0.4, 0.8, 1.0));
        layer.offset = IVec2::new(7, 9);
        layer.content = LayerContent::Raster { buffer, mask: None };
        let id = doc.add_layer(layer);
        doc.order.push(id);

        assert_eq!(layer_source_rect(&doc, id), Rect::new(27, 39, 1, 1));
    }

    #[test]
    fn group_source_bounds_union_visible_children_with_offsets() {
        let mut doc = Document::new(512, 512);
        let mut group = Layer::new_group("group");
        group.offset = IVec2::new(5, 7);
        let group_id = doc.add_layer(group);
        doc.order.push(group_id);

        let mut child_a = Layer::new_raster("child-a");
        child_a.offset = IVec2::new(20, 30);
        child_a.content = LayerContent::Raster {
            buffer: solid_buffer(4, 5, Rgba32F::new(1.0, 0.0, 0.0, 1.0)),
            mask: None,
        };
        let child_a_id = doc.add_layer(child_a);

        let mut child_b = Layer::new_raster("child-b");
        child_b.offset = IVec2::new(90, 110);
        child_b.content = LayerContent::Raster {
            buffer: solid_buffer(2, 3, Rgba32F::new(0.0, 1.0, 0.0, 1.0)),
            mask: None,
        };
        let child_b_id = doc.add_layer(child_b);

        if let LayerContent::Group { children } = &mut doc.layer_mut(group_id).unwrap().content {
            children.extend([child_a_id, child_b_id]);
        }

        assert_eq!(layer_source_rect(&doc, group_id), Rect::new(25, 37, 72, 83));
    }

    #[test]
    fn empty_layer_returns_checkerboard_tile() {
        let mut doc = Document::new(256, 256);
        let id = doc.add_layer(Layer::new_raster("empty"));
        doc.order.push(id);
        doc.active = Some(id);

        let img = layer_thumbnail(&doc, id);
        assert_eq!(img.size, [THUMB_SIZE, THUMB_SIZE]);
        // The top-left cell is the lighter checker grey, never transparent.
        let top_left = img.pixels[0];
        assert_eq!(top_left.a(), 255, "thumbnail must be opaque (checkerboard)");
        let expected = srgb_byte(CHECKER_LIGHT);
        assert_eq!(top_left.r(), expected);
    }

    #[test]
    fn adjustment_layer_renders_nonblank_ramp() {
        let mut doc = Document::new(128, 128);
        let id = doc.add_layer(Layer::new_adjustment(
            "curves",
            ogre_core::AdjustmentKind::Invert,
        ));
        doc.order.push(id);
        doc.active = Some(id);

        let img = layer_thumbnail(&doc, id);
        assert_eq!(img.size, [THUMB_SIZE, THUMB_SIZE]);
        // The ramp's left edge is dark and right edge is bright.
        let left = img.pixels[0].r();
        let right = img.pixels[THUMB_SIZE - 1].r();
        assert!(right > left, "ramp must increase left-to-right");
    }
}
