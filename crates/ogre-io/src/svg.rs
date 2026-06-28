// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! SVG import/export.

use ogre_core::{
    coord::IVec2, pixel::Rgba32F, Document, StrokeCap, StrokeJoin, SvgSource, TiledBuffer,
    VectorData, VectorFill, VectorPath, VectorStroke,
};

use base64::Engine as _;
use image::ImageEncoder as _;

use crate::error::IoError;

/// How to import an SVG document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SvgImportMode {
    /// Rasterize the SVG to a single pixel layer.
    Rasterize,
    /// Import extractable vector paths as an ogre vector layer.
    Vector,
    /// Import as a vector layer and pre-rasterize the SVG into the layer cache
    /// so the document looks correct even for unsupported SVG features.
    #[default]
    Both,
}

/// Options controlling SVG import.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SvgImportOptions {
    /// Import mode.
    pub mode: SvgImportMode,
    /// Target DPI for rasterization. The SVG user-unit size is rendered at this
    /// resolution.
    pub dpi: f32,
}

impl Default for SvgImportOptions {
    fn default() -> Self {
        Self {
            mode: SvgImportMode::default(),
            dpi: 96.0,
        }
    }
}

/// Import an SVG document from disk, transparently decompressing `.svgz` files.
///
/// The returned document's canvas matches the SVG's natural size in pixels at
/// the requested DPI. Depending on [`SvgImportOptions::mode`] the layer is
/// raster, vector, or a vector layer with a pre-rasterized cache.
pub fn import_svg_file(
    path: impl AsRef<std::path::Path>,
    opts: SvgImportOptions,
) -> Result<Document, IoError> {
    let path = path.as_ref();
    let bytes = std::fs::read(path)?;
    let is_svgz = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("svgz"))
        .unwrap_or(false);
    if is_svgz {
        let mut decoder = flate2::read::GzDecoder::new(&bytes[..]);
        let mut decoded = Vec::new();
        std::io::Read::read_to_end(&mut decoder, &mut decoded)?;
        import_svg(&decoded, opts)
    } else {
        import_svg(&bytes, opts)
    }
}

/// Import an SVG document into a single-layer [`Document`].
pub fn import_svg(source: &[u8], opts: SvgImportOptions) -> Result<Document, IoError> {
    if !opts.dpi.is_finite() || opts.dpi <= 0.0 || opts.dpi > 2400.0 {
        return Err(IoError::SvgParse(
            "DPI must be a finite positive value no greater than 2400".to_string(),
        ));
    }

    let xml = std::str::from_utf8(source)
        .map_err(|e| IoError::SvgParse(format!("invalid UTF-8: {e}")))?;

    let parse_opts = usvg::Options {
        dpi: opts.dpi,
        ..Default::default()
    };
    let tree = usvg::Tree::from_str(xml, &parse_opts)
        .map_err(|e| IoError::SvgParse(format!("usvg parse: {e}")))?;

    let size = tree.size();
    let width = size.width().round() as u32;
    let height = size.height().round() as u32;
    if width > i32::MAX as u32 || height > i32::MAX as u32 {
        return Err(IoError::Unsupported("SVG dimensions exceed the maximum"));
    }
    if width == 0 || height == 0 {
        return Err(IoError::SvgParse("SVG has zero size".to_string()));
    }

    let mut doc = Document::new(width, height);
    let name = "SVG";

    match opts.mode {
        SvgImportMode::Rasterize => {
            let buffer = render_tree_to_buffer(&tree)?;
            let layer_id = doc.add_raster_layer(name);
            let layer = doc.layer_mut(layer_id).expect("just inserted");
            *layer.buffer_mut().expect("raster layer") = buffer;
        }
        SvgImportMode::Vector | SvgImportMode::Both => {
            let paths = extract_paths(&tree);
            let mut data = VectorData {
                paths,
                ..Default::default()
            };
            if opts.mode == SvgImportMode::Both {
                data.rasterized = Some(render_tree_to_buffer(&tree)?);
            }
            // Preserve the original SVG so the layer can be re-rasterized after
            // vector edits.
            data.svg_source = Some(SvgSource {
                source_bytes: source.to_vec(),
                element_id: String::new(),
            });
            data.mark_dirty();
            doc.add_vector_layer(name, data);
        }
    }

    Ok(doc)
}

/// Render a parsed SVG tree into a document-space [`TiledBuffer`].
fn render_tree_to_buffer(tree: &usvg::Tree) -> Result<TiledBuffer, IoError> {
    let size = tree.size();
    let width = size.width().round() as u32;
    let height = size.height().round() as u32;
    if width == 0 || height == 0 {
        return Ok(TiledBuffer::new());
    }

    let mut pixels = vec![0u8; (width * height * 4) as usize];
    {
        let mut pixmap = resvg::tiny_skia::PixmapMut::from_bytes(&mut pixels, width, height)
            .ok_or_else(|| IoError::SvgRender("failed to create SVG pixmap".to_string()))?;
        let transform = resvg::tiny_skia::Transform::identity();
        resvg::render(tree, transform, &mut pixmap);
    }

    let mut buffer = TiledBuffer::new();
    // resvg writes premultiplied sRGB RGBA; demultiply and convert to linear
    // Rgba32F.
    for y in 0..height {
        for x in 0..width {
            let idx = ((y * width + x) * 4) as usize;
            let premul = resvg::tiny_skia::PremultipliedColorU8::from_rgba(
                pixels[idx],
                pixels[idx + 1],
                pixels[idx + 2],
                pixels[idx + 3],
            )
            .unwrap_or(resvg::tiny_skia::PremultipliedColorU8::TRANSPARENT);
            let c = premul.demultiply();
            let a = c.alpha() as f32 / 255.0;
            if a == 0.0 {
                continue;
            }
            buffer.set_pixel(
                IVec2::new(x as i32, y as i32),
                Rgba32F::new(
                    srgb_to_linear(c.red() as f32 / 255.0),
                    srgb_to_linear(c.green() as f32 / 255.0),
                    srgb_to_linear(c.blue() as f32 / 255.0),
                    a,
                ),
            );
        }
    }
    Ok(buffer)
}

fn srgb_to_linear(v: f32) -> f32 {
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// Extract visible vector paths from a parsed SVG tree.
fn extract_paths(tree: &usvg::Tree) -> Vec<VectorPath> {
    let mut out = Vec::new();
    for node in tree.root().children() {
        if let usvg::Node::Path(path) = node {
            if path.is_visible() {
                if let Some(vp) = convert_path(path) {
                    out.push(vp);
                }
            }
        }
    }
    out
}

fn convert_path(path: &usvg::Path) -> Option<VectorPath> {
    let mut bez = kurbo::BezPath::new();
    use usvg::tiny_skia_path::PathSegment;
    for seg in path.data().segments() {
        match seg {
            PathSegment::MoveTo(p) => {
                bez.move_to((p.x as f64, p.y as f64));
            }
            PathSegment::LineTo(p) => {
                bez.line_to((p.x as f64, p.y as f64));
            }
            PathSegment::QuadTo(q, p) => {
                bez.quad_to((q.x as f64, q.y as f64), (p.x as f64, p.y as f64));
            }
            PathSegment::CubicTo(c1, c2, p) => {
                bez.curve_to(
                    (c1.x as f64, c1.y as f64),
                    (c2.x as f64, c2.y as f64),
                    (p.x as f64, p.y as f64),
                );
            }
            PathSegment::Close => {
                bez.close_path();
            }
        }
    }

    let tolerance = 0.5;
    let vertices = ogre_vector::bezpath_to_polygon(&bez, tolerance);
    if vertices.len() < 2 {
        return None;
    }

    let fill = path
        .fill()
        .and_then(|f| match f.paint() {
            usvg::Paint::Color(c) => {
                let a = f.opacity().get();
                Some(VectorFill::Solid(color_from(*c, a)))
            }
            _ => None,
        })
        .unwrap_or(VectorFill::None);

    let stroke = path.stroke().map_or(
        VectorStroke {
            color: Rgba32F::TRANSPARENT,
            width: 0.0,
            dash: Vec::new(),
            cap: StrokeCap::Butt,
            join: StrokeJoin::Miter,
        },
        |s| {
            let color = match s.paint() {
                usvg::Paint::Color(c) => color_from(*c, s.opacity().get()),
                _ => Rgba32F::TRANSPARENT,
            };
            VectorStroke {
                color,
                width: s.width().get(),
                dash: s.dasharray().map(|d| d.to_vec()).unwrap_or_default(),
                cap: cap_from(s.linecap()),
                join: join_from(s.linejoin()),
            }
        },
    );

    // A path is closed when its final segment is Close. tiny-skia-path also
    // emits Close for implicitly closed subpaths.
    let mut closed = false;
    for seg in path.data().segments() {
        if matches!(seg, PathSegment::Close) {
            closed = true;
        }
    }

    Some(VectorPath {
        vertices,
        fill,
        stroke,
        closed,
    })
}

fn color_from(c: usvg::Color, alpha: f32) -> Rgba32F {
    // SVG color values are sRGB-encoded; the working colour space is linear.
    let a = if alpha.is_finite() {
        alpha.clamp(0.0, 1.0)
    } else {
        1.0
    };
    Rgba32F::new(
        srgb_to_linear(c.red as f32 / 255.0),
        srgb_to_linear(c.green as f32 / 255.0),
        srgb_to_linear(c.blue as f32 / 255.0),
        a,
    )
}

fn cap_from(cap: usvg::LineCap) -> StrokeCap {
    match cap {
        usvg::LineCap::Butt => StrokeCap::Butt,
        usvg::LineCap::Round => StrokeCap::Round,
        usvg::LineCap::Square => StrokeCap::Square,
    }
}

fn join_from(join: usvg::LineJoin) -> StrokeJoin {
    match join {
        usvg::LineJoin::Miter | usvg::LineJoin::MiterClip => StrokeJoin::Miter,
        usvg::LineJoin::Round => StrokeJoin::Round,
        usvg::LineJoin::Bevel => StrokeJoin::Bevel,
    }
}

/// Re-render the authoritative `rasterized` cache for an SVG-derived vector
/// layer from the current editable paths.
///
/// This is called after vector path edits so the cached pixels reflect the
/// current geometry. Any SVG features that are not represented as editable
/// paths (complex filters, text, etc.) are dropped once the layer is edited;
/// the editable vector paths become the authoritative content.
pub fn rerasterize_vector_data(data: &mut VectorData, _dpi: f32) -> Result<(), IoError> {
    if data.svg_source.is_none() && data.rasterized.is_none() {
        return Ok(());
    }
    data.rasterized = Some(ogre_core::rasterize_vector_paths(data));
    data.mark_dirty();
    Ok(())
}

/// Export a document to a flat SVG file.
///
/// The result is an SVG 1.1 document whose contents are a single `<image>`
/// element referencing the composited canvas as a base64-encoded PNG. This
/// preserves the full visual result (including groups, adjustments, and any
/// vector layers) without requiring the consumer to understand Arte Ogre's
/// native layer model.
pub fn export_svg(
    doc: &ogre_core::Document,
    path: impl AsRef<std::path::Path>,
) -> Result<(), IoError> {
    let region = doc.canvas;
    let pixels = ogre_core::composite_document(doc, region)?;

    let mut rgba_u8 = Vec::with_capacity(
        (region.w as usize)
            .checked_mul(region.h as usize)
            .and_then(|n| n.checked_mul(4))
            .expect("export region too large"),
    );
    for px in &pixels {
        rgba_u8.push((ogre_core::srgb_encode(px.r) * 255.0).round() as u8);
        rgba_u8.push((ogre_core::srgb_encode(px.g) * 255.0).round() as u8);
        rgba_u8.push((ogre_core::srgb_encode(px.b) * 255.0).round() as u8);
        rgba_u8.push((px.a.clamp(0.0, 1.0) * 255.0).round() as u8);
    }

    let mut png_bytes = Vec::new();
    {
        let encoder = image::codecs::png::PngEncoder::new(&mut png_bytes);
        encoder.write_image(
            &rgba_u8,
            region.w,
            region.h,
            image::ExtendedColorType::Rgba8,
        )?;
    }

    let b64 = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
    let svg = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="{}" height="{}"><image width="{}" height="{}" href="data:image/png;base64,{}"/></svg>"##,
        region.w, region.h, region.w, region.h, b64
    );

    std::fs::write(path, svg)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ogre_core::LayerContent;

    fn red_rect_svg() -> &'static [u8] {
        br##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10">
            <rect width="10" height="10" fill="#ff0000"/>
        </svg>"##
    }

    #[test]
    fn import_svg_rasterize_creates_red_raster_layer() {
        let doc = import_svg(
            red_rect_svg(),
            SvgImportOptions {
                mode: SvgImportMode::Rasterize,
                dpi: 96.0,
            },
        )
        .unwrap();
        assert_eq!(doc.canvas.w, 10);
        assert_eq!(doc.canvas.h, 10);
        let layer = doc.layer(doc.order[0]).unwrap();
        assert!(layer.is_raster());
        let buffer = layer.buffer().unwrap();
        assert_eq!(
            buffer.get_pixel(IVec2::new(5, 5)),
            Rgba32F::new(1.0, 0.0, 0.0, 1.0)
        );
    }

    #[test]
    fn import_svg_both_creates_vector_layer_with_rasterized_cache() {
        let doc = import_svg(red_rect_svg(), SvgImportOptions::default()).unwrap();
        assert_eq!(doc.canvas.w, 10);
        assert_eq!(doc.canvas.h, 10);
        let layer = doc.layer(doc.order[0]).unwrap();
        assert!(layer.is_vector());
        let LayerContent::Vector(data) = &layer.content else {
            panic!("expected vector layer");
        };
        assert!(!data.paths.is_empty(), "should extract vector paths");
        assert!(
            data.rasterized.is_some(),
            "Both mode should pre-rasterize the SVG"
        );
        assert!(data.svg_source.is_some());
        assert_ne!(data.version, 0);
    }

    #[test]
    fn import_svg_vector_mode_preserves_source_and_skips_raster_cache() {
        let doc = import_svg(
            red_rect_svg(),
            SvgImportOptions {
                mode: SvgImportMode::Vector,
                dpi: 96.0,
            },
        )
        .unwrap();
        let layer = doc.layer(doc.order[0]).unwrap();
        let LayerContent::Vector(data) = &layer.content else {
            panic!("expected vector layer");
        };
        assert!(!data.paths.is_empty());
        assert!(data.rasterized.is_none());
        assert!(data.svg_source.is_some());
    }

    #[test]
    fn import_svg_file_loads_svgz() {
        let tmp = std::env::temp_dir().join(format!("ogre_svgz_test_{}.svgz", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let plain = red_rect_svg();
        let file = std::fs::File::create(&tmp).unwrap();
        let mut encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, plain).unwrap();
        encoder.finish().unwrap();

        let doc = import_svg_file(&tmp, SvgImportOptions::default()).unwrap();
        assert_eq!(doc.canvas.w, 10);
        assert_eq!(doc.canvas.h, 10);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn import_svg_rejects_malformed_xml() {
        let err = import_svg(b"<svg><unclosed", SvgImportOptions::default()).unwrap_err();
        assert!(matches!(err, IoError::SvgParse(_)));
    }

    fn stroke_svg() -> &'static [u8] {
        br##"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20">
            <line x1="0" y1="10" x2="20" y2="10" stroke="#008080" stroke-width="2"/>
        </svg>"##
    }

    fn gray_rect_svg() -> &'static [u8] {
        br##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10">
            <rect width="10" height="10" fill="#808080"/>
        </svg>"##
    }

    #[test]
    fn import_svg_converts_srgb_fill_to_linear() {
        let doc = import_svg(
            gray_rect_svg(),
            SvgImportOptions {
                mode: SvgImportMode::Vector,
                dpi: 96.0,
            },
        )
        .unwrap();
        let output =
            ogre_core::composite_document(&doc, ogre_core::Rect::new(0, 0, 10, 10)).unwrap();
        // #808080 is 128/255 in each sRGB channel; the working space stores it
        // as linear light.
        let expected = srgb_to_linear(128.0 / 255.0);
        assert!(
            (output[0].r - expected).abs() < 1e-4,
            "#808080 sRGB should decode to linear ~{expected}, got {}",
            output[0].r
        );
    }

    #[test]
    fn import_svg_extracts_stroke_attributes() {
        let doc = import_svg(stroke_svg(), SvgImportOptions::default()).unwrap();
        let layer = doc.layer(doc.order[0]).unwrap();
        let LayerContent::Vector(data) = &layer.content else {
            panic!("expected vector layer");
        };
        let path = data.paths.first().expect("should extract line path");
        // #008080 is sRGB (0, 128/255, 128/255); storage is linear.
        let expected_stroke = Rgba32F::new(
            0.0,
            srgb_to_linear(128.0 / 255.0),
            srgb_to_linear(128.0 / 255.0),
            1.0,
        );
        assert!(
            (path.stroke.color.r - expected_stroke.r).abs() < 1e-4
                && (path.stroke.color.g - expected_stroke.g).abs() < 1e-4
                && (path.stroke.color.b - expected_stroke.b).abs() < 1e-4
                && (path.stroke.color.a - expected_stroke.a).abs() < 1e-4,
            "stroke color should be linear teal, got {:?}",
            path.stroke.color
        );
        assert_eq!(path.stroke.width, 2.0);
    }

    #[test]
    fn export_svg_writes_flat_image_embedded_in_svg() {
        let mut doc = Document::new(10, 10);
        let id = doc.add_raster_layer("bg");
        let layer = doc.layer_mut(id).unwrap();
        for y in 0..10 {
            for x in 0..10 {
                layer
                    .buffer_mut()
                    .unwrap()
                    .set_pixel(IVec2::new(x, y), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
            }
        }

        let tmp = std::env::temp_dir().join(format!("ogre_svg_export_{}.svg", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        export_svg(&doc, &tmp).unwrap();

        let svg = std::fs::read_to_string(&tmp).unwrap();
        assert!(svg.starts_with("<svg"), "output should be an SVG document");
        assert!(
            svg.contains("data:image/png;base64,"),
            "SVG should embed a base64 PNG"
        );

        // Re-importing the exported SVG should recover the same canvas size.
        let imported = import_svg_file(&tmp, SvgImportOptions::default()).unwrap();
        assert_eq!(imported.canvas.w, 10);
        assert_eq!(imported.canvas.h, 10);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn import_svg_rejects_zero_size() {
        let err = import_svg(
            br##"<svg xmlns="http://www.w3.org/2000/svg" width="0" height="10"/>"##,
            SvgImportOptions::default(),
        )
        .unwrap_err();
        assert!(
            matches!(err, IoError::SvgParse(_)),
            "zero width should be rejected"
        );
    }

    #[test]
    fn import_svg_rejects_invalid_dpi() {
        for dpi in [0.0, -96.0, f32::NAN, f32::INFINITY, 3000.0] {
            let err = import_svg(
                br##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"/>"##,
                SvgImportOptions {
                    mode: SvgImportMode::Vector,
                    dpi,
                },
            )
            .unwrap_err();
            assert!(
                matches!(err, IoError::SvgParse(_)),
                "dpi {dpi} should be rejected"
            );
        }
    }

    #[test]
    fn import_svg_vector_mode_extracts_no_paths_from_empty_drawing() {
        let doc = import_svg(
            br##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10">
                <text x="0" y="5">hello</text>
            </svg>"##,
            SvgImportOptions::default(),
        )
        .unwrap();
        let layer = doc.layer(doc.order[0]).unwrap();
        let LayerContent::Vector(data) = &layer.content else {
            panic!("expected vector layer");
        };
        assert!(data.paths.is_empty(), "text is not extractable as a path");
        assert!(data.svg_source.is_some());
    }

    #[test]
    fn import_svg_file_rejects_invalid_gzip() {
        let tmp = std::env::temp_dir().join(format!("ogre_bad_svgz_{}.svgz", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        std::fs::write(&tmp, b"not gzip").unwrap();
        let err = import_svg_file(&tmp, SvgImportOptions::default()).unwrap_err();
        assert!(
            matches!(err, IoError::Io(_) | IoError::SvgParse(_)),
            "invalid gzip should fail: {err:?}"
        );
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn export_svg_round_trips_pixel_content() {
        let mut doc = Document::new(10, 10);
        let id = doc.add_raster_layer("bg");
        let layer = doc.layer_mut(id).unwrap();
        let color = Rgba32F::new(
            ogre_core::srgb_encode(0.25),
            ogre_core::srgb_encode(0.5),
            ogre_core::srgb_encode(0.75),
            1.0,
        );
        for y in 0..10 {
            for x in 0..10 {
                layer
                    .buffer_mut()
                    .unwrap()
                    .set_pixel(IVec2::new(x, y), color);
            }
        }

        let tmp =
            std::env::temp_dir().join(format!("ogre_svg_export_pixel_{}.svg", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        export_svg(&doc, &tmp).unwrap();

        let imported = import_svg_file(
            &tmp,
            SvgImportOptions {
                mode: SvgImportMode::Rasterize,
                dpi: 96.0,
            },
        )
        .unwrap();
        let imported_layer = imported.layer(imported.order[0]).unwrap();
        let sample = imported_layer.buffer().unwrap().get_pixel(IVec2::new(5, 5));
        // 8-bit sRGB quantization allows up to ~1/255 error per channel.
        let eps = 2.0 / 255.0;
        assert!(
            (sample.r - color.r).abs() < eps
                && (sample.g - color.g).abs() < eps
                && (sample.b - color.b).abs() < eps
                && (sample.a - color.a).abs() < eps,
            "exported→imported pixel should round-trip, got {:?} expected {:?}",
            sample,
            color
        );
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn rerasterize_updates_cache_after_path_edit() {
        let mut doc = import_svg(red_rect_svg(), SvgImportOptions::default()).unwrap();
        let layer_id = doc.order[0];
        let layer = doc.layer_mut(layer_id).unwrap();
        let LayerContent::Vector(data) = &mut layer.content else {
            panic!("expected vector layer");
        };
        // Move the imported rectangle 5 pixels to the right.
        for path in &mut data.paths {
            for v in &mut path.vertices {
                v.x += 5;
            }
        }
        rerasterize_vector_data(data, 96.0).unwrap();

        // The cache is now authoritative: pixels at the new location are red,
        // and the old location is transparent.
        let output =
            ogre_core::composite_document(&doc, ogre_core::Rect::new(0, 0, 10, 10)).unwrap();
        let red = Rgba32F::new(ogre_core::srgb_encode(1.0), 0.0, 0.0, 1.0);
        let got = output[7 * 10 + 7];
        assert!(
            (got.r - red.r).abs() < 1e-4
                && (got.g - red.g).abs() < 1e-4
                && (got.b - red.b).abs() < 1e-4
                && (got.a - red.a).abs() < 1e-4,
            "pixel inside moved rect should be red, got {:?}",
            got
        );
        assert_eq!(
            output[5 * 10 + 2],
            Rgba32F::TRANSPARENT,
            "pixel at original location should be transparent"
        );
    }
}
