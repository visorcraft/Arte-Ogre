// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Interchange format support: OpenRaster (`.ora`) and PSD import.

use std::fs::File;
use std::io::{Cursor, Read, Write};
use std::path::Path;

use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageEncoder};
use ogre_core::{BlendMode, Document, IVec2, Layer, LayerContent, LayerId, Rgba32F, TiledBuffer};
use psd::Psd;
use quick_xml::events::Event;
use quick_xml::Reader;
use zip::write::{FileOptions, ZipWriter};
use zip::CompressionMethod;
use zip::ZipArchive;

// Pins the `()` extension type; bare `FileOptions::default()` is ambiguous
// because zip implements `FileOptionExtension` for more than one type.
fn file_options() -> FileOptions<'static, ()> {
    FileOptions::default()
}

use crate::color::{rgba32_to_srgb_u8, srgb_u8_to_rgba32};
use crate::error::IoError;

const ORA_STACK_XML: &str = "stack.xml";
const ORA_MIMETYPE: &str = "mimetype";
const ORA_MIMETYPE_VALUE: &str = "image/openraster";
const ORA_DATA_DIR: &str = "data";

/// Import an OpenRaster (`.ora`) file into a `Document`.
pub fn import_ora(path: impl AsRef<Path>) -> Result<Document, IoError> {
    let file = File::open(path)?;
    let mut archive = ZipArchive::new(file)?;

    let mut stack_xml = String::new();
    {
        let mut entry = archive.by_name(ORA_STACK_XML)?;
        entry.read_to_string(&mut stack_xml)?;
    }

    let mut reader = Reader::from_str(&stack_xml);

    let mut doc: Option<Document> = None;
    let mut stack: Vec<Vec<LayerId>> = Vec::new();
    let mut parent_stack: Vec<Option<LayerId>> = Vec::new();

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                let tag_name = e.name();
                let name = tag_name.as_ref();
                if name == b"image" {
                    let attrs = attrs_to_map(e)?;
                    let w = attrs
                        .get("w")
                        .and_then(|s| s.parse::<u32>().ok())
                        .unwrap_or(64);
                    let h = attrs
                        .get("h")
                        .and_then(|s| s.parse::<u32>().ok())
                        .unwrap_or(64);
                    doc = Some(Document::new(w, h));
                } else if name == b"stack" {
                    if let Some(ref mut doc) = doc {
                        let attrs = attrs_to_map(e)?;
                        let layer_name = attrs.get("name").cloned().unwrap_or_default();
                        let (opacity, visible, blend) = extract_common_attrs(&attrs);
                        let group = if parent_stack.is_empty() {
                            // Root stack: we don't create a synthetic group.
                            None
                        } else {
                            let mut group = Layer::new_group(xml_unescape(&layer_name));
                            group.opacity = opacity;
                            group.visible = visible;
                            group.blend = blend;
                            let group_id = doc.add_layer(group);
                            // Add this new group as a child of its own parent.
                            stack.last_mut().unwrap().push(group_id);
                            Some(group_id)
                        };
                        parent_stack.push(group);
                        stack.push(Vec::new());
                    }
                } else if name == b"layer" {
                    if let Some(ref mut doc) = doc {
                        let attrs = attrs_to_map(e)?;
                        let src = attrs
                            .get("src")
                            .cloned()
                            .ok_or(IoError::CorruptManifest("missing layer src"))?;
                        let layer_name = attrs.get("name").cloned().unwrap_or_default();
                        let x = attrs
                            .get("x")
                            .and_then(|s| s.parse::<i32>().ok())
                            .unwrap_or(0);
                        let y = attrs
                            .get("y")
                            .and_then(|s| s.parse::<i32>().ok())
                            .unwrap_or(0);
                        let (opacity, visible, blend) = extract_common_attrs(&attrs);

                        let mut entry = archive.by_name(&src)?;
                        let mut bytes = Vec::new();
                        entry.read_to_end(&mut bytes)?;
                        let image = image::load_from_memory(&bytes)?.to_rgba8();
                        let width = image.width();
                        let height = image.height();
                        let rgba = srgb_u8_to_rgba32(image.as_raw());

                        let mut layer = Layer::new_raster(xml_unescape(&layer_name));
                        layer.offset = IVec2::new(x, y);
                        layer.opacity = opacity;
                        layer.visible = visible;
                        layer.blend = blend;

                        {
                            let buffer = layer.buffer_mut().expect("raster layer");
                            for iy in 0..height {
                                for ix in 0..width {
                                    let p = rgba[(iy * width + ix) as usize];
                                    buffer.set_pixel(IVec2::new(ix as i32, iy as i32), p);
                                }
                            }
                        }

                        let id = doc.add_layer(layer);
                        stack.last_mut().unwrap().push(id);
                    }
                }
            }
            Ok(Event::End(ref e)) => {
                let tag_name = e.name();
                let name = tag_name.as_ref();
                if name == b"stack" {
                    let ids = stack.pop().unwrap();
                    let parent = parent_stack.pop().flatten();
                    if let Some(ref mut doc) = doc {
                        // `ids` was collected in file order (top-to-bottom);
                        // store it bottom-to-top to match `doc.order`/children.
                        let bottom_to_top: Vec<LayerId> = ids.into_iter().rev().collect();
                        if let Some(parent) = parent {
                            if let LayerContent::Group { children } =
                                &mut doc.layer_mut(parent)?.content
                            {
                                *children = bottom_to_top;
                            }
                        } else {
                            doc.order = bottom_to_top;
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(IoError::Xml(e)),
            _ => {}
        }
        buf.clear();
    }

    doc.ok_or(IoError::CorruptManifest("no <image> element in stack.xml"))
}

fn attrs_to_map(
    e: &quick_xml::events::BytesStart<'_>,
) -> Result<std::collections::HashMap<String, String>, IoError> {
    let mut map = std::collections::HashMap::new();
    for attr in e.attributes() {
        let attr = attr?;
        let key = String::from_utf8_lossy(attr.key.as_ref()).into_owned();
        let value = String::from_utf8_lossy(&attr.value).into_owned();
        map.insert(key, value);
    }
    Ok(map)
}

fn extract_common_attrs(
    attrs: &std::collections::HashMap<String, String>,
) -> (f32, bool, BlendMode) {
    let opacity = attrs
        .get("opacity")
        .and_then(|s| s.parse::<f32>().ok())
        .unwrap_or(1.0)
        .clamp(0.0, 1.0);
    let visible = attrs.get("visibility").map(|s| s.as_str()) != Some("hidden");
    let blend = attrs
        .get("composite-op")
        .map(|s| parse_ora_blend(s))
        .unwrap_or(BlendMode::Normal);
    (opacity, visible, blend)
}

fn parse_ora_blend(s: &str) -> BlendMode {
    match s {
        "svg:multiply" => BlendMode::Multiply,
        "svg:screen" => BlendMode::Screen,
        "svg:overlay" => BlendMode::Overlay,
        "svg:darken" => BlendMode::Darken,
        "svg:lighten" => BlendMode::Lighten,
        "svg:color-dodge" => BlendMode::ColorDodge,
        "svg:color-burn" => BlendMode::ColorBurn,
        "svg:hard-light" => BlendMode::HardLight,
        "svg:soft-light" => BlendMode::SoftLight,
        "svg:difference" => BlendMode::Difference,
        "svg:exclusion" => BlendMode::Exclusion,
        "svg:plus" => BlendMode::Add,
        _ => BlendMode::Normal,
    }
}

fn ora_blend_name(mode: BlendMode) -> &'static str {
    match mode {
        BlendMode::Normal => "svg:src-over",
        BlendMode::Multiply => "svg:multiply",
        BlendMode::Screen => "svg:screen",
        BlendMode::Overlay => "svg:overlay",
        BlendMode::Darken => "svg:darken",
        BlendMode::Lighten => "svg:lighten",
        BlendMode::ColorDodge => "svg:color-dodge",
        BlendMode::ColorBurn => "svg:color-burn",
        BlendMode::HardLight => "svg:hard-light",
        BlendMode::SoftLight => "svg:soft-light",
        BlendMode::Difference => "svg:difference",
        BlendMode::Exclusion => "svg:exclusion",
        BlendMode::Add => "svg:plus",
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Export a `Document` to an OpenRaster (`.ora`) file.
pub fn export_ora(doc: &Document, path: impl AsRef<Path>) -> Result<(), IoError> {
    let file = File::create(path)?;
    let mut zip = ZipWriter::new(file);

    // mimetype must be first and uncompressed per OpenRaster spec.
    zip.start_file(
        ORA_MIMETYPE,
        file_options().compression_method(CompressionMethod::Stored),
    )?;
    zip.write_all(ORA_MIMETYPE_VALUE.as_bytes())?;

    let mut layer_index: usize = 0;
    let mut xml = String::new();
    xml.push_str(&format!(
        "<image w=\"{}\" h=\"{}\">\n<stack>\n",
        doc.canvas.w, doc.canvas.h
    ));
    // OpenRaster lists children top-to-bottom (first child = topmost), but
    // `doc.order` is bottom-to-top, so emit it reversed.
    for &id in doc.order.iter().rev() {
        write_layer_or_stack(doc, id, &mut xml, &mut zip, &mut layer_index, 1)?;
    }
    xml.push_str("</stack>\n</image>\n");

    zip.start_file(ORA_STACK_XML, file_options())?;
    zip.write_all(xml.as_bytes())?;

    zip.finish()?;
    Ok(())
}

fn write_layer_or_stack(
    doc: &Document,
    id: LayerId,
    xml: &mut String,
    zip: &mut ZipWriter<File>,
    layer_index: &mut usize,
    indent: usize,
) -> Result<(), IoError> {
    let layer = doc.layer(id)?;
    let indent_str = "  ".repeat(indent);
    match &layer.content {
        LayerContent::Raster { buffer, .. } => {
            let src = format!("{}/layer{}.png", ORA_DATA_DIR, layer_index);
            *layer_index += 1;

            let name = xml_escape(&layer.name);
            let (x, y) = if let Some(bounds) = buffer.exact_bounds() {
                (layer.offset.x + bounds.x, layer.offset.y + bounds.y)
            } else {
                (layer.offset.x, layer.offset.y)
            };
            let opacity = layer.opacity;
            let visibility = if layer.visible { "visible" } else { "hidden" };
            let composite_op = ora_blend_name(layer.blend);

            xml.push_str(&format!(
                "{}<layer name=\"{}\" x=\"{}\" y=\"{}\" opacity=\"{}\" visibility=\"{}\" composite-op=\"{}\" src=\"{}\" />\n",
                indent_str, name, x, y, opacity, visibility, composite_op, src
            ));

            if let Some(bounds) = buffer.exact_bounds() {
                let mut pixels = Vec::with_capacity((bounds.w * bounds.h) as usize);
                for y in bounds.y..bounds.y + bounds.h as i32 {
                    for x in bounds.x..bounds.x + bounds.w as i32 {
                        pixels.push(buffer.get_pixel(IVec2::new(x, y)));
                    }
                }
                let bytes = rgba32_to_srgb_u8(&pixels);
                let mut encoded = Vec::new();
                let encoder = PngEncoder::new(Cursor::new(&mut encoded));
                encoder.write_image(&bytes, bounds.w, bounds.h, ExtendedColorType::Rgba8)?;

                zip.start_file(&src, file_options())?;
                zip.write_all(&encoded)?;
            } else {
                // Empty layer: write a 1x1 transparent PNG placeholder.
                let bytes = vec![0u8; 4];
                let mut encoded = Vec::new();
                let encoder = PngEncoder::new(Cursor::new(&mut encoded));
                encoder.write_image(&bytes, 1, 1, ExtendedColorType::Rgba8)?;
                zip.start_file(&src, file_options())?;
                zip.write_all(&encoded)?;
            }
        }
        LayerContent::Group { children } => {
            let name = xml_escape(&layer.name);
            let opacity = layer.opacity;
            let visibility = if layer.visible { "visible" } else { "hidden" };
            let composite_op = ora_blend_name(layer.blend);
            xml.push_str(&format!(
                "{}<stack name=\"{}\" opacity=\"{}\" visibility=\"{}\" composite-op=\"{}\">\n",
                indent_str, name, opacity, visibility, composite_op
            ));
            // Children are bottom-to-top in our model; OpenRaster wants
            // top-to-bottom, so emit them reversed.
            for &child in children.iter().rev() {
                write_layer_or_stack(doc, child, xml, zip, layer_index, indent + 1)?;
            }
            xml.push_str(&format!("{}</stack>\n", indent_str));
        }
        LayerContent::Adjustment(_) => {
            // OpenRaster does not support adjustment layers; skip them.
        }
        LayerContent::Vector(_) => {
            // OpenRaster does not support vector layers; skip them.
        }
    }
    Ok(())
}

/// Import a PSD file into a `Document`.
pub fn import_psd(path: impl AsRef<Path>) -> Result<Document, IoError> {
    let bytes = std::fs::read(path)?;
    let psd = Psd::from_bytes(&bytes)?;

    let mut doc = Document::new(psd.width(), psd.height());

    let psd_w = psd.width();
    let psd_h = psd.height();

    for layer in psd.layers() {
        // Skip group divider layers that the psd crate still exposes as layers.
        if layer.name().starts_with('<') || layer.name() == "</Layer group>" {
            continue;
        }

        let name = layer.name().to_string();
        // `PsdLayer::rgba()` returns a buffer sized to the WHOLE PSD canvas
        // (`psd_width * psd_height * 4`) with the layer's pixels already placed
        // at their document position (transparent elsewhere). It must therefore
        // be read with the canvas stride, and the layer offset stays zero —
        // the previous code used the layer's own width as the stride and added
        // `(left, top)`, which scrambled pixels and could index out of bounds.
        let rgba = layer.rgba();
        let buffer = psd_rgba_to_buffer(&rgba, psd_w, psd_h);

        let mut ogre_layer = Layer::new_raster(&name);
        ogre_layer.offset = IVec2::ZERO;
        ogre_layer.opacity = (layer.opacity() as f32 / 255.0).clamp(0.0, 1.0);
        ogre_layer.visible = layer.visible();
        // The `psd` crate does not publicly expose the layer blend mode enum, so we
        // fall back to Normal and leave best-effort mapping to a future improvement.
        ogre_layer.blend = BlendMode::Normal;
        *ogre_layer.buffer_mut().expect("raster layer") = buffer;

        let id = doc.add_layer(ogre_layer);
        doc.order.push(id);
    }

    Ok(doc)
}

/// Convert a canvas-sized PSD RGBA8 buffer (sRGB, straight alpha) into a
/// document-space [`TiledBuffer`]. Pixels are positioned at their canvas
/// coordinates using the canvas width as the row stride.
fn psd_rgba_to_buffer(rgba: &[u8], psd_w: u32, psd_h: u32) -> TiledBuffer {
    let mut buffer = TiledBuffer::new();
    let w = psd_w as usize;
    for y in 0..psd_h as usize {
        for x in 0..w {
            let idx = (y * w + x) * 4;
            if idx + 3 >= rgba.len() {
                continue;
            }
            let a = rgba[idx + 3] as f32 / 255.0;
            if a == 0.0 {
                // Keep the buffer sparse; fully transparent pixels add nothing.
                continue;
            }
            let p = Rgba32F::new(
                crate::color::srgb_to_linear(rgba[idx] as f32 / 255.0),
                crate::color::srgb_to_linear(rgba[idx + 1] as f32 / 255.0),
                crate::color::srgb_to_linear(rgba[idx + 2] as f32 / 255.0),
                a,
            );
            buffer.set_pixel(IVec2::new(x as i32, y as i32), p);
        }
    }
    buffer
}

#[cfg(test)]
mod tests {
    use super::*;
    use ogre_core::{IVec2, Rect, Rgba32F};
    use std::env::temp_dir;

    fn ora_round_trip_document() -> Document {
        let mut doc = Document::new(32, 32);
        let bg = doc.add_raster_layer("bg");
        doc.layer_mut(bg)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(4, 4), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        let group = Layer::new_group("group");
        let group_id = doc.insert_layer_above(group, bg).unwrap();
        let child = doc.add_raster_layer("child");
        doc.layer_mut(child)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(8, 8), Rgba32F::new(0.0, 1.0, 0.0, 0.75));
        doc.move_into_group(child, group_id, 0).unwrap();

        let top = doc.add_raster_layer("top");
        doc.layer_mut(top)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(12, 12), Rgba32F::new(0.0, 0.0, 1.0, 0.5));
        doc
    }

    #[test]
    fn ora_round_trip_preserves_composite() {
        let doc = ora_round_trip_document();
        let path = temp_dir().join("ogre_ora_rt.ora");
        export_ora(&doc, &path).unwrap();
        let loaded = import_ora(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert_eq!(loaded.canvas, doc.canvas);
        let region = Rect::new(0, 0, 32, 32);
        let original = ogre_core::compositor::composite_document(&doc, region).unwrap();
        let restored = ogre_core::compositor::composite_document(&loaded, region).unwrap();
        assert_eq!(original.len(), restored.len());
        for (a, b) in original.iter().zip(restored.iter()) {
            assert!((a.r - b.r).abs() < 2.0 / 255.0);
            assert!((a.g - b.g).abs() < 2.0 / 255.0);
            assert!((a.b - b.b).abs() < 2.0 / 255.0);
            assert!((a.a - b.a).abs() < 2.0 / 255.0);
        }
    }

    #[test]
    fn psd_import_reads_green_1x1() {
        let bytes = include_bytes!("../tests/fixtures/green-1x1.psd");
        let file = temp_dir().join("ogre_psd_green_1x1.psd");
        std::fs::write(&file, bytes).unwrap();
        let doc = import_psd(&file).unwrap();
        let _ = std::fs::remove_file(&file);

        assert_eq!(doc.canvas.w, 1);
        assert_eq!(doc.canvas.h, 1);
        assert_eq!(doc.order.len(), 1);
    }

    #[test]
    fn psd_import_reads_rle_3_layer_8x8() {
        let bytes = include_bytes!("../tests/fixtures/rle-3-layer-8x8.psd");
        let file = temp_dir().join("ogre_psd_3layer_8x8.psd");
        std::fs::write(&file, bytes).unwrap();
        let doc = import_psd(&file).unwrap();
        let _ = std::fs::remove_file(&file);

        assert_eq!(doc.canvas.w, 8);
        assert_eq!(doc.canvas.h, 8);
        assert!(!doc.order.is_empty());
    }

    #[test]
    fn psd_rgba_to_buffer_uses_canvas_stride_and_position() {
        // 4x2 canvas-sized RGBA buffer with a single green pixel at (3, 1).
        let (w, h) = (4u32, 2u32);
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        let idx = ((w + 3) * 4) as usize; // (x=3, y=1) with canvas stride (row 1)
        rgba[idx + 1] = 255; // green
        rgba[idx + 3] = 255; // opaque

        let buffer = psd_rgba_to_buffer(&rgba, w, h);

        let green = buffer.get_pixel(IVec2::new(3, 1));
        assert!(
            green.g > 0.5 && green.a == 1.0,
            "expected green at (3,1): {green:?}"
        );
        // Everything else is transparent and must not be misplaced.
        assert_eq!(buffer.get_pixel(IVec2::new(0, 0)), Rgba32F::TRANSPARENT);
        assert_eq!(buffer.get_pixel(IVec2::new(3, 0)), Rgba32F::TRANSPARENT);
        assert_eq!(buffer.get_pixel(IVec2::new(1, 1)), Rgba32F::TRANSPARENT);
    }

    #[test]
    fn ora_export_lists_top_layer_first() {
        // `doc.order` is bottom-to-top; OpenRaster lists top-to-bottom.
        let mut doc = Document::new(8, 8);
        let bottom = doc.add_raster_layer("bottom");
        doc.layer_mut(bottom)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(0, 0), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        let top = doc.add_raster_layer("top");
        doc.layer_mut(top)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(1, 1), Rgba32F::new(0.0, 1.0, 0.0, 1.0));

        let path = temp_dir().join("ogre_ora_order.ora");
        export_ora(&doc, &path).unwrap();

        let mut archive = ZipArchive::new(File::open(&path).unwrap()).unwrap();
        let mut xml = String::new();
        archive
            .by_name(ORA_STACK_XML)
            .unwrap()
            .read_to_string(&mut xml)
            .unwrap();
        let _ = std::fs::remove_file(&path);

        let top_pos = xml.find("name=\"top\"").expect("top layer present");
        let bottom_pos = xml.find("name=\"bottom\"").expect("bottom layer present");
        assert!(
            top_pos < bottom_pos,
            "OpenRaster must list the top layer first:\n{xml}"
        );
    }
}
