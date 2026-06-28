// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Flat raster image import/export.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use image::codecs::{jpeg::JpegEncoder, png::PngEncoder, tiff::TiffEncoder, webp::WebPEncoder};
use image::{ColorType, ExtendedColorType, ImageEncoder, ImageFormat};
use ogre_core::{compositor::composite_document, Document, IVec2, Rect, Rgba32F};

use crate::color::{
    convert_to_profile, detect_embedded_profile, raw_rgb_u16_to_rgba32, raw_rgb_u8_to_rgba32,
    raw_u16_to_rgba32, raw_u8_to_rgba32, rgba32_to_linear_u16, rgba32_to_linear_u8,
    rgba32_to_srgb_u16, rgba32_to_srgb_u8, srgb_u16_to_rgba32, srgb_u8_to_rgba32, IccProfile,
};
use crate::error::IoError;

/// Supported flat raster formats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RasterFormat {
    Png,
    Jpeg,
    Tiff,
    WebP,
    Exr,
}

/// Bit depth for integer raster exports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RasterBitDepth {
    Eight,
    Sixteen,
}

/// Options controlling raster export.
#[derive(Clone, Debug)]
pub struct ExportOptions {
    pub format: RasterFormat,
    /// Quality for lossy codecs (JPEG). Range 0–100.
    pub quality: Option<u8>,
    /// Bit depth for integer formats. EXR is always float.
    pub bit_depth: RasterBitDepth,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            format: RasterFormat::Png,
            quality: None,
            bit_depth: RasterBitDepth::Eight,
        }
    }
}

/// Decode any supported raster file into a single-layer `Document`.
///
/// The new document's canvas equals the decoded image size and its single
/// raster layer is named after the file stem.
pub fn import_image(path: impl AsRef<Path>) -> Result<Document, IoError> {
    let path = path.as_ref();
    let image = image::open(path)?;
    let (width, height) = (image.width(), image.height());
    // Document coordinates are `i32`; reject images whose dimensions would wrap
    // when cast, before allocating buffers sized from them.
    if width > i32::MAX as u32 || height > i32::MAX as u32 {
        return Err(IoError::Unsupported("image dimensions exceed the maximum"));
    }
    let embedded = detect_embedded_profile(path)?;
    let use_profile = embedded.is_some();

    let mut rgba: Vec<Rgba32F> = match image {
        image::DynamicImage::ImageRgba8(buf) => {
            if use_profile {
                raw_u8_to_rgba32(buf.as_raw())
            } else {
                srgb_u8_to_rgba32(buf.as_raw())
            }
        }
        image::DynamicImage::ImageRgb8(buf) => {
            if use_profile {
                raw_rgb_u8_to_rgba32(buf.as_raw())
            } else {
                let mut out = Vec::with_capacity(buf.as_raw().len() / 3);
                for c in buf.as_raw().chunks_exact(3) {
                    out.push(Rgba32F::new(
                        crate::color::srgb_to_linear(c[0] as f32 / 255.0),
                        crate::color::srgb_to_linear(c[1] as f32 / 255.0),
                        crate::color::srgb_to_linear(c[2] as f32 / 255.0),
                        1.0,
                    ));
                }
                out
            }
        }
        image::DynamicImage::ImageRgba16(buf) => {
            if use_profile {
                raw_u16_to_rgba32(buf.as_raw())
            } else {
                srgb_u16_to_rgba32(buf.as_raw())
            }
        }
        image::DynamicImage::ImageRgb16(buf) => {
            if use_profile {
                raw_rgb_u16_to_rgba32(buf.as_raw())
            } else {
                let mut out = Vec::with_capacity(buf.as_raw().len() / 3);
                for c in buf.as_raw().chunks_exact(3) {
                    out.push(Rgba32F::new(
                        crate::color::srgb_to_linear(c[0] as f32 / 65535.0),
                        crate::color::srgb_to_linear(c[1] as f32 / 65535.0),
                        crate::color::srgb_to_linear(c[2] as f32 / 65535.0),
                        1.0,
                    ));
                }
                out
            }
        }
        image::DynamicImage::ImageRgba32F(buf) => buf
            .pixels()
            .map(|p| {
                let [r, g, b, a] = p.0;
                Rgba32F::new(r, g, b, a)
            })
            .collect(),
        other => {
            // Convert any other dynamic image variant to RGBA8 (sRGB) and then
            // back to linear. This covers Luma8, LumaA8, Rgb32F, etc.
            let rgba8 = other.to_rgba8();
            if use_profile {
                raw_u8_to_rgba32(rgba8.as_raw())
            } else {
                srgb_u8_to_rgba32(rgba8.as_raw())
            }
        }
    };

    if let Some(ref profile) = embedded {
        convert_to_profile(&mut rgba, Some(profile), None)?;
    }

    let mut doc = Document::new(width, height);
    doc.icc_profile = embedded.map(IccProfile::into_bytes);
    let layer_id = doc.add_raster_layer(
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("imported"),
    );
    let layer = doc.layer_mut(layer_id).expect("just inserted");
    for y in 0..height {
        for x in 0..width {
            // Index in `usize` so `y * width` cannot overflow for large images.
            let p = rgba[y as usize * width as usize + x as usize];
            layer
                .buffer_mut()
                .expect("raster layer")
                .set_pixel(IVec2::new(x as i32, y as i32), p);
        }
    }
    Ok(doc)
}

/// Composite `region` of `doc` and save it as a flat raster image.
pub fn export_image(
    doc: &Document,
    region: Rect,
    options: &ExportOptions,
    path: impl AsRef<Path>,
) -> Result<(), IoError> {
    let mut pixels = composite_document(doc, region)?;
    let converted = if let Some(ref bytes) = doc.icc_profile {
        let profile = IccProfile::new(bytes.clone());
        convert_to_profile(&mut pixels, None, Some(&profile))?;
        true
    } else {
        false
    };

    match options.format {
        RasterFormat::Png => export_png(
            path,
            &pixels,
            region.w,
            region.h,
            options.bit_depth,
            converted,
        ),
        RasterFormat::Jpeg => export_jpeg(
            path,
            &pixels,
            region.w,
            region.h,
            options.quality,
            converted,
        ),
        RasterFormat::Tiff => export_tiff(
            path,
            &pixels,
            region.w,
            region.h,
            options.bit_depth,
            converted,
        ),
        RasterFormat::WebP => export_webp(path, &pixels, region.w, region.h, converted),
        RasterFormat::Exr => export_exr(path, &pixels, region.w, region.h),
    }
}

/// Export every slice in `doc` as a separate raster image.
///
/// Files are written to `base_dir` using the pattern `{name}_{x}_{y}.{ext}`
/// (e.g. `button_12_34.png`). Empty or degenerate slices are skipped.
/// Returns the list of paths that were actually written.
pub fn export_slices(
    doc: &Document,
    base_dir: impl AsRef<Path>,
    options: &ExportOptions,
) -> Result<Vec<PathBuf>, IoError> {
    let base_dir = base_dir.as_ref();
    std::fs::create_dir_all(base_dir)?;

    let ext = match options.format {
        RasterFormat::Png => "png",
        RasterFormat::Jpeg => "jpg",
        RasterFormat::Tiff => "tiff",
        RasterFormat::WebP => "webp",
        RasterFormat::Exr => "exr",
    };

    let mut written = Vec::new();
    for slice in &doc.slices {
        if slice.rect.is_empty() {
            continue;
        }
        let safe_name: String = slice
            .name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let file_name = format!("{}_{}_{}.{}", safe_name, slice.rect.x, slice.rect.y, ext);
        let path = base_dir.join(file_name);
        export_image(doc, slice.rect, options, &path)?;
        written.push(path);
    }
    Ok(written)
}

fn export_png(
    path: impl AsRef<Path>,
    pixels: &[Rgba32F],
    width: u32,
    height: u32,
    depth: RasterBitDepth,
    converted: bool,
) -> Result<(), IoError> {
    let file = BufWriter::new(File::create(path)?);
    let encoder = PngEncoder::new(file);
    match depth {
        RasterBitDepth::Eight => {
            let bytes = if converted {
                rgba32_to_linear_u8(pixels)
            } else {
                rgba32_to_srgb_u8(pixels)
            };
            encoder.write_image(&bytes, width, height, ExtendedColorType::Rgba8)?;
        }
        RasterBitDepth::Sixteen => {
            let words = if converted {
                rgba32_to_linear_u16(pixels)
            } else {
                rgba32_to_srgb_u16(pixels)
            };
            let bytes = bytemuck::cast_slice(&words);
            encoder.write_image(bytes, width, height, ExtendedColorType::Rgba16)?;
        }
    }
    Ok(())
}

fn export_jpeg(
    path: impl AsRef<Path>,
    pixels: &[Rgba32F],
    width: u32,
    height: u32,
    quality: Option<u8>,
    converted: bool,
) -> Result<(), IoError> {
    // JPEG has no alpha; composite onto white.
    let mut rgb = Vec::with_capacity(pixels.len() * 3);
    for p in pixels {
        let a = p.a.clamp(0.0, 1.0);
        if converted {
            let blend = |c: f32| c * a + (1.0 - a);
            rgb.push((blend(p.r) * 255.0).round() as u8);
            rgb.push((blend(p.g) * 255.0).round() as u8);
            rgb.push((blend(p.b) * 255.0).round() as u8);
        } else {
            let blend = |c: f32| crate::color::linear_to_srgb(c * a + (1.0 - a));
            rgb.push((blend(p.r) * 255.0).round() as u8);
            rgb.push((blend(p.g) * 255.0).round() as u8);
            rgb.push((blend(p.b) * 255.0).round() as u8);
        }
    }
    let file = BufWriter::new(File::create(path)?);
    let quality = quality.unwrap_or(90).clamp(1, 100);
    let encoder = JpegEncoder::new_with_quality(file, quality);
    encoder.write_image(&rgb, width, height, ExtendedColorType::Rgb8)?;
    Ok(())
}

fn export_tiff(
    path: impl AsRef<Path>,
    pixels: &[Rgba32F],
    width: u32,
    height: u32,
    depth: RasterBitDepth,
    converted: bool,
) -> Result<(), IoError> {
    let file = BufWriter::new(File::create(path)?);
    let encoder = TiffEncoder::new(file);
    match depth {
        RasterBitDepth::Eight => {
            let bytes = if converted {
                rgba32_to_linear_u8(pixels)
            } else {
                rgba32_to_srgb_u8(pixels)
            };
            encoder.write_image(&bytes, width, height, ExtendedColorType::Rgba8)?;
        }
        RasterBitDepth::Sixteen => {
            let words = if converted {
                rgba32_to_linear_u16(pixels)
            } else {
                rgba32_to_srgb_u16(pixels)
            };
            let bytes = bytemuck::cast_slice(&words);
            encoder.write_image(bytes, width, height, ExtendedColorType::Rgba16)?;
        }
    }
    Ok(())
}

fn export_webp(
    path: impl AsRef<Path>,
    pixels: &[Rgba32F],
    width: u32,
    height: u32,
    converted: bool,
) -> Result<(), IoError> {
    // The pure-Rust WebP encoder in `image` supports lossless RGB/RGBA only.
    let bytes = if converted {
        rgba32_to_linear_u8(pixels)
    } else {
        rgba32_to_srgb_u8(pixels)
    };
    let file = BufWriter::new(File::create(path)?);
    let encoder = WebPEncoder::new_lossless(file);
    encoder.write_image(&bytes, width, height, ExtendedColorType::Rgba8)?;
    Ok(())
}

fn export_exr(
    path: impl AsRef<Path>,
    pixels: &[Rgba32F],
    width: u32,
    height: u32,
) -> Result<(), IoError> {
    let raw: Vec<f32> = pixels.iter().flat_map(|p| [p.r, p.g, p.b, p.a]).collect();
    image::save_buffer_with_format(
        path,
        bytemuck::cast_slice(&raw),
        width,
        height,
        ColorType::Rgba32F,
        ImageFormat::OpenExr,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ogre_core::{IVec2, Rgba32F};
    use std::env::temp_dir;

    fn test_document() -> Document {
        let mut doc = Document::new(16, 16);
        let id = doc.add_raster_layer("test");
        let layer = doc.layer_mut(id).unwrap();
        for y in 0..16 {
            for x in 0..16 {
                let t = (x + y) as f32 / 30.0;
                layer
                    .buffer_mut()
                    .unwrap()
                    .set_pixel(IVec2::new(x, y), Rgba32F::new(t, 0.5, 1.0 - t, 0.75));
            }
        }
        doc
    }

    fn extension_for(format: RasterFormat) -> &'static str {
        match format {
            RasterFormat::Png => "png",
            RasterFormat::Jpeg => "jpg",
            RasterFormat::Tiff => "tiff",
            RasterFormat::WebP => "webp",
            RasterFormat::Exr => "exr",
        }
    }

    fn assert_round_trips(format: RasterFormat, depth: RasterBitDepth) {
        let doc = test_document();
        let ext = extension_for(format);
        let mut path = temp_dir().join(format!("ogre_raster_{:?}_{:?}_{}", format, depth, ext));
        path.set_extension(ext);
        let options = ExportOptions {
            format,
            quality: None,
            bit_depth: depth,
        };
        export_image(&doc, doc.canvas, &options, &path).unwrap();
        let loaded = import_image(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert_eq!(loaded.canvas, doc.canvas);
        let region = doc.canvas;
        let original = composite_document(&doc, region).unwrap();
        let restored = composite_document(&loaded, region).unwrap();
        assert_eq!(original.len(), restored.len());
        let eps = match depth {
            RasterBitDepth::Eight => 2.0 / 255.0,
            RasterBitDepth::Sixteen => 2.0 / 65535.0,
        };
        for (a, b) in original.iter().zip(restored.iter()) {
            assert!((a.r - b.r).abs() < eps, "r mismatch: {:?} vs {:?}", a, b);
            assert!((a.g - b.g).abs() < eps, "g mismatch: {:?} vs {:?}", a, b);
            assert!((a.b - b.b).abs() < eps, "b mismatch: {:?} vs {:?}", a, b);
            assert!((a.a - b.a).abs() < eps, "a mismatch: {:?} vs {:?}", a, b);
        }
    }

    #[test]
    fn png_round_trip_8bit() {
        assert_round_trips(RasterFormat::Png, RasterBitDepth::Eight);
    }

    #[test]
    fn png_round_trip_16bit() {
        assert_round_trips(RasterFormat::Png, RasterBitDepth::Sixteen);
    }

    #[test]
    fn jpeg_round_trip_no_alpha() {
        // Build an opaque document so alpha blending onto white is exact.
        let mut doc = Document::new(16, 16);
        let id = doc.add_raster_layer("opaque");
        let layer = doc.layer_mut(id).unwrap();
        for y in 0..16 {
            for x in 0..16 {
                layer.buffer_mut().unwrap().set_pixel(
                    IVec2::new(x, y),
                    Rgba32F::new(x as f32 / 15.0, y as f32 / 15.0, 0.25, 1.0),
                );
            }
        }
        let mut path = temp_dir().join("ogre_raster_jpg");
        path.set_extension("jpg");
        let options = ExportOptions {
            format: RasterFormat::Jpeg,
            quality: Some(95),
            bit_depth: RasterBitDepth::Eight,
        };
        export_image(&doc, doc.canvas, &options, &path).unwrap();
        let loaded = import_image(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(loaded.canvas, doc.canvas);
    }

    #[test]
    fn tiff_round_trip_8bit() {
        assert_round_trips(RasterFormat::Tiff, RasterBitDepth::Eight);
    }

    #[test]
    fn webp_round_trip_lossless() {
        assert_round_trips(RasterFormat::WebP, RasterBitDepth::Eight);
    }

    #[test]
    fn exr_round_trip_float() {
        assert_round_trips(RasterFormat::Exr, RasterBitDepth::Eight);
    }

    #[test]
    fn export_slices_writes_one_file_per_slice() {
        use std::env::temp_dir;

        let mut doc = Document::new(64, 64);
        let id = doc.add_raster_layer("L");
        let layer = doc.layer_mut(id).unwrap();
        for y in 0..10 {
            for x in 0..10 {
                layer
                    .buffer_mut()
                    .unwrap()
                    .set_pixel(IVec2::new(x, y), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
            }
        }
        doc.slices.push(ogre_core::SliceRect {
            name: "red-block".to_string(),
            rect: Rect::new(0, 0, 10, 10),
        });
        doc.slices.push(ogre_core::SliceRect {
            name: "empty slice!".to_string(),
            rect: Rect::new(20, 20, 0, 0),
        });

        let out_dir = temp_dir().join("ogre_slice_export_test");
        let _ = std::fs::remove_dir_all(&out_dir);
        let options = ExportOptions {
            format: RasterFormat::Png,
            quality: None,
            bit_depth: RasterBitDepth::Eight,
        };
        let paths = export_slices(&doc, &out_dir, &options).unwrap();

        assert_eq!(paths.len(), 1);
        assert!(paths[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("red-block_0_0"));
        assert!(paths[0].exists());

        let _ = std::fs::remove_dir_all(&out_dir);
    }

    #[test]
    fn export_after_remove_matte_is_valid_png() {
        // Regression: after Remove Background (checkerboard matte + enclosed areas),
        // the resulting sparse buffer must export to a valid PNG rather than a
        // corrupted or unreadable file.
        use ogre_core::{remove_matte_with, MatteOptions};
        use std::env::temp_dir;

        let mut doc = Document::new(64, 64);
        let id = doc.add_raster_layer("subject");
        let layer = doc.layer_mut(id).unwrap();
        let white = Rgba32F::new(1.0, 1.0, 1.0, 1.0);
        let grey = Rgba32F::new(0.78, 0.78, 0.78, 1.0);
        let red = Rgba32F::new(1.0, 0.0, 0.0, 1.0);
        for y in 0..64 {
            for x in 0..64 {
                let even = ((x / 8) + (y / 8)) % 2 == 0;
                let bg = if even { white } else { grey };
                layer.buffer_mut().unwrap().set_pixel(IVec2::new(x, y), bg);
            }
        }
        for y in 20..44 {
            for x in 20..44 {
                layer.buffer_mut().unwrap().set_pixel(IVec2::new(x, y), red);
            }
        }

        let mut removed = remove_matte_with(
            layer.buffer().unwrap(),
            MatteOptions {
                tolerance: 0.2,
                edge_softness: 0.55,
                remove_enclosed: true,
            },
        )
        .expect("matte is detected");

        let mut doc2 = Document::new(64, 64);
        let id2 = doc2.add_raster_layer("removed");
        std::mem::swap(
            doc2.layer_mut(id2).unwrap().buffer_mut().unwrap(),
            &mut removed,
        );

        let path = temp_dir().join("ogre_remove_matte_export.png");
        let _ = std::fs::remove_file(&path);
        export_image(&doc2, doc2.canvas, &ExportOptions::default(), &path).unwrap();

        let loaded = import_image(&path).expect("exported PNG decodes");
        assert_eq!(loaded.canvas, doc2.canvas);

        let _ = std::fs::remove_file(&path);
    }
}
