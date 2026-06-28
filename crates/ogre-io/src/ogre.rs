// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Native `.ogre` document format.
//!
//! The file layout is:
//!
//! ```text
//! 4 bytes  magic = b"OGRE"
//! 4 bytes  version (little-endian u32)
//! 8 bytes  manifest length (little-endian u64)
//! N bytes  manifest encoded with rmp-serde
//! ```
//!
//! The manifest contains document metadata and, for every occupied raster/mask
//! tile, a zstd-compressed blob of pixel data. Layer ids are stored as stable
//! 64-bit FFI values and remapped when the document is loaded into a fresh
//! [`Document`].

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;

use ahash::{AHashMap, AHashSet};
use ogre_core::{
    AdjustmentKind, BlendMode, ColorSpace, Document, IVec2, Layer, LayerContent, LayerId, Rect,
    Rgba32F, Selection, SelectionKind, Tile, TileCoord, TiledBuffer, VectorData, VectorPath,
    TILE_SIZE,
};
use serde::{Deserialize, Serialize};
use slotmap::Key;

const MAGIC: &[u8; 4] = b"OGRE";
const VERSION: u32 = 2;
const COMPRESSION_LEVEL: i32 = 3;
const MAX_NATIVE_CANVAS_DIMENSION: u32 = 8192;
const MAX_NATIVE_CANVAS_PIXELS: u64 =
    MAX_NATIVE_CANVAS_DIMENSION as u64 * MAX_NATIVE_CANVAS_DIMENSION as u64;
const MAX_NATIVE_VECTOR_VERTICES: usize = 100_000;

use crate::error::IoError;

#[derive(Serialize, Deserialize)]
struct Manifest {
    version: u32,
    canvas: Rect,
    color_space: ColorSpace,
    #[serde(default)]
    icc_profile: Option<Vec<u8>>,
    active: Option<u64>,
    root_order: Vec<u64>,
    selection: SelectionManifest,
    layers: Vec<LayerManifest>,
    removed: Vec<u64>,
}

#[derive(Serialize, Deserialize)]
enum SelectionManifest {
    None,
    Rect(Rect),
    InvertedRect {
        canvas: Rect,
        rect: Rect,
    },
    Mask {
        tiles: Vec<TileManifest>,
        bounds: Rect,
    },
}

#[derive(Serialize, Deserialize)]
struct LayerManifest {
    stable_id: u64,
    name: String,
    offset: IVec2,
    opacity: f32,
    blend: BlendMode,
    visible: bool,
    locked: bool,
    content: ContentManifest,
}

#[derive(Serialize, Deserialize)]
enum ContentManifest {
    Raster {
        tiles: Vec<TileManifest>,
        mask: Option<Vec<TileManifest>>,
    },
    Group {
        children: Vec<u64>,
    },
    Adjustment(AdjustmentKind),
    Vector(Box<ogre_core::VectorData>),
}

#[derive(Clone, Serialize, Deserialize)]
struct TileManifest {
    coord: TileCoord,
    data: Vec<u8>,
}

/// Save `doc` to `path` in the native `.ogre` format.
pub fn save(doc: &Document, path: impl AsRef<Path>) -> Result<(), IoError> {
    let manifest = build_manifest(doc)?;
    let manifest_bytes = rmp_serde::to_vec_named(&manifest)?;

    let mut file = File::create(path)?;
    file.write_all(MAGIC)?;
    file.write_all(&VERSION.to_le_bytes())?;
    file.write_all(&(manifest_bytes.len() as u64).to_le_bytes())?;
    file.write_all(&manifest_bytes)?;
    Ok(())
}

/// Load a document from `path` in the native `.ogre` format.
pub fn load(path: impl AsRef<Path>) -> Result<Document, IoError> {
    let mut file = File::open(path)?;

    let mut magic = [0u8; 4];
    file.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(IoError::BadMagic);
    }

    let mut version_bytes = [0u8; 4];
    file.read_exact(&mut version_bytes)?;
    let version = u32::from_le_bytes(version_bytes);
    if version != 1 && version != 2 {
        return Err(IoError::UnsupportedVersion(version));
    }

    let mut len_bytes = [0u8; 8];
    file.read_exact(&mut len_bytes)?;
    let len = u64::from_le_bytes(len_bytes);

    // Reject a declared manifest length larger than the file itself: a corrupt
    // or hostile header must not trigger a multi-gigabyte allocation before the
    // read fails.
    let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if len > file_len {
        return Err(IoError::CorruptManifest(
            "manifest length exceeds file size",
        ));
    }

    let mut manifest_bytes = vec![0u8; len as usize];
    file.read_exact(&mut manifest_bytes)?;

    let manifest: Manifest = rmp_serde::from_slice(&manifest_bytes)?;
    rebuild_document(manifest)
}

fn build_manifest(doc: &Document) -> Result<Manifest, IoError> {
    let mut layers = Vec::new();
    for (id, layer) in doc.all_layers().iter() {
        let stable_id = id.data().as_ffi();
        let content = match &layer.content {
            LayerContent::Raster { buffer, mask } => {
                let mut tiles = Vec::new();
                for (coord, tile) in buffer.occupied_tiles() {
                    tiles.push(tile_to_manifest(&tile, coord)?);
                }
                let mask = mask.as_ref().map(|m| {
                    m.occupied_tiles()
                        .into_iter()
                        .map(|(coord, tile)| tile_to_manifest(&tile, coord))
                        .collect::<Result<Vec<_>, _>>()
                });
                ContentManifest::Raster {
                    tiles,
                    mask: mask.transpose()?,
                }
            }
            LayerContent::Group { children } => ContentManifest::Group {
                children: children.iter().map(|id| id.data().as_ffi()).collect(),
            },
            LayerContent::Adjustment(kind) => ContentManifest::Adjustment(*kind),
            LayerContent::Vector(data) => ContentManifest::Vector(data.clone()),
        };

        layers.push(LayerManifest {
            stable_id,
            name: layer.name.clone(),
            offset: layer.offset,
            opacity: layer.opacity,
            blend: layer.blend,
            visible: layer.visible,
            locked: layer.locked,
            content,
        });
    }

    // Deterministic output: sort layers by their stable id.
    layers.sort_by_key(|l| l.stable_id);

    Ok(Manifest {
        version: VERSION,
        canvas: doc.canvas,
        color_space: doc.color_space,
        icc_profile: doc.icc_profile.clone(),
        active: doc.active.map(|id| id.data().as_ffi()),
        root_order: doc.order.iter().map(|id| id.data().as_ffi()).collect(),
        selection: selection_to_manifest(&doc.selection)?,
        layers,
        removed: doc
            .removed_layers()
            .iter()
            .map(|id| id.data().as_ffi())
            .collect(),
    })
}

fn selection_to_manifest(sel: &Selection) -> Result<SelectionManifest, IoError> {
    Ok(match &sel.kind {
        SelectionKind::None => SelectionManifest::None,
        SelectionKind::Rect(r) => SelectionManifest::Rect(*r),
        SelectionKind::InvertedRect { canvas, rect } => SelectionManifest::InvertedRect {
            canvas: *canvas,
            rect: *rect,
        },
        SelectionKind::Mask { coverage, bounds } => {
            let mut tiles = Vec::new();
            for (coord, tile) in coverage.occupied_tiles() {
                tiles.push(tile_to_manifest(&tile, coord)?);
            }
            SelectionManifest::Mask {
                tiles,
                bounds: *bounds,
            }
        }
    })
}

fn tile_to_manifest(tile: &Tile, coord: TileCoord) -> Result<TileManifest, IoError> {
    let bytes = bytemuck::cast_slice(tile.as_slice());
    let data = zstd::encode_all(bytes, COMPRESSION_LEVEL)?;
    Ok(TileManifest { coord, data })
}

fn rebuild_document(manifest: Manifest) -> Result<Document, IoError> {
    if manifest.version != 1 && manifest.version != 2 {
        return Err(IoError::UnsupportedVersion(manifest.version));
    }

    let canvas = validate_canvas(manifest.canvas)?;
    let mut doc = Document::with_canvas(canvas);
    doc.color_space = manifest.color_space;
    doc.icc_profile = manifest.icc_profile;
    doc.selection = manifest_to_selection(&manifest.selection, doc.canvas)?;

    let mut stable_to_id: AHashMap<u64, LayerId> = AHashMap::new();
    let mut group_children: AHashMap<LayerId, Vec<u64>> = AHashMap::new();

    for lm in manifest.layers {
        let stable_children = match &lm.content {
            ContentManifest::Group { children } => Some(children.clone()),
            _ => None,
        };

        let content = match lm.content {
            ContentManifest::Raster { tiles, mask } => {
                let mut map = AHashMap::new();
                for tm in tiles {
                    let (coord, tile) = manifest_to_tile(tm)?;
                    map.insert(coord, tile);
                }
                let buffer = TiledBuffer::from_tiles(map);
                validate_tiled_buffer_bounds(
                    &buffer,
                    "raster layer bounds exceed the maximum supported dimensions",
                )?;
                let mask = match mask {
                    None => None,
                    Some(mask_tiles) => {
                        let mut map = AHashMap::new();
                        for tm in mask_tiles {
                            let (coord, tile) = manifest_to_tile(tm)?;
                            map.insert(coord, tile);
                        }
                        let mask = TiledBuffer::from_tiles(map);
                        validate_tiled_buffer_bounds(
                            &mask,
                            "raster mask bounds exceed the maximum supported dimensions",
                        )?;
                        Some(mask)
                    }
                };
                LayerContent::Raster { buffer, mask }
            }
            ContentManifest::Group { .. } => LayerContent::Group {
                children: Vec::new(),
            },
            ContentManifest::Adjustment(kind) => LayerContent::Adjustment(kind),
            ContentManifest::Vector(data) => {
                validate_vector_data(&data)?;
                LayerContent::Vector(data)
            }
        };

        let layer = Layer {
            id: LayerId::default(),
            name: lm.name,
            offset: lm.offset,
            opacity: lm.opacity,
            blend: lm.blend,
            visible: lm.visible,
            locked: lm.locked,
            content,
        };

        let id = doc.add_layer(layer);
        if stable_to_id.insert(lm.stable_id, id).is_some() {
            return Err(IoError::CorruptManifest("duplicate layer stable id"));
        }
        if let Some(children) = stable_children {
            group_children.insert(id, children);
        }
    }

    let resolve = |sid: u64| -> Result<LayerId, IoError> {
        stable_to_id
            .get(&sid)
            .copied()
            .ok_or(IoError::CorruptManifest("dangling layer reference"))
    };

    // Resolve group children from stable ids to loaded ids.
    for (id, children) in group_children {
        let resolved: Vec<LayerId> = children
            .iter()
            .copied()
            .map(resolve)
            .collect::<Result<_, _>>()?;
        if let Ok(layer) = doc.layer_mut(id) {
            if let LayerContent::Group { children } = &mut layer.content {
                *children = resolved;
            }
        }
    }

    doc.order = manifest
        .root_order
        .iter()
        .copied()
        .map(resolve)
        .collect::<Result<_, _>>()?;
    doc.active = manifest.active.map(resolve).transpose()?;

    let removed: AHashSet<LayerId> = manifest
        .removed
        .iter()
        .copied()
        .map(resolve)
        .collect::<Result<_, _>>()?;
    doc.set_removed(removed);

    Ok(doc)
}

fn validate_canvas(canvas: Rect) -> Result<Rect, IoError> {
    if canvas.w == 0 || canvas.h == 0 {
        return Err(IoError::CorruptManifest("canvas must not be empty"));
    }
    validate_rect_budget(canvas, "canvas exceeds the maximum supported dimensions")?;
    Ok(canvas)
}

fn validate_rect_budget(rect: Rect, error: &'static str) -> Result<(), IoError> {
    if rect.w > MAX_NATIVE_CANVAS_DIMENSION
        || rect.h > MAX_NATIVE_CANVAS_DIMENSION
        || rect.area() > MAX_NATIVE_CANVAS_PIXELS
    {
        return Err(IoError::CorruptManifest(error));
    }
    Ok(())
}

fn clip_to_canvas(rect: Rect, canvas: Rect) -> Rect {
    rect.intersect(canvas)
        .unwrap_or_else(|| Rect::new(canvas.x, canvas.y, 0, 0))
}

fn manifest_to_selection(m: &SelectionManifest, canvas: Rect) -> Result<Selection, IoError> {
    let kind = match m {
        SelectionManifest::None => SelectionKind::None,
        SelectionManifest::Rect(r) => SelectionKind::Rect(clip_to_canvas(*r, canvas)),
        SelectionManifest::InvertedRect { rect, .. } => SelectionKind::InvertedRect {
            canvas,
            rect: clip_to_canvas(*rect, canvas),
        },
        SelectionManifest::Mask { tiles, bounds } => {
            let mut map = AHashMap::new();
            for tm in tiles {
                let (coord, tile) = manifest_to_tile(tm.clone())?;
                map.insert(coord, tile);
            }
            let coverage = TiledBuffer::from_tiles(map);
            validate_tiled_buffer_bounds(
                &coverage,
                "selection mask bounds exceed the maximum supported dimensions",
            )?;
            SelectionKind::Mask {
                coverage,
                bounds: clip_to_canvas(*bounds, canvas),
            }
        }
    };
    Ok(Selection { kind })
}

fn validate_vector_data(data: &VectorData) -> Result<(), IoError> {
    if let Some(rasterized) = &data.rasterized {
        validate_tiled_buffer_bounds(
            rasterized,
            "vector raster cache exceeds the maximum supported dimensions",
        )?;
    }
    for path in &data.paths {
        validate_vector_path(path)?;
    }
    Ok(())
}

fn validate_tiled_buffer_bounds(buffer: &TiledBuffer, error: &'static str) -> Result<(), IoError> {
    if let Some(bounds) = buffer.exact_bounds() {
        validate_rect_budget(bounds, error)?;
    }
    Ok(())
}

fn validate_vector_path(path: &VectorPath) -> Result<(), IoError> {
    if path.vertices.len() > MAX_NATIVE_VECTOR_VERTICES {
        return Err(IoError::CorruptManifest(
            "vector path has too many vertices",
        ));
    }
    if path.vertices.is_empty() {
        return Ok(());
    }
    if path.stroke.width > MAX_NATIVE_CANVAS_DIMENSION as f32 {
        return Err(IoError::CorruptManifest(
            "vector stroke width exceeds the maximum supported dimensions",
        ));
    }

    let min_x = path.vertices.iter().map(|p| p.x as i64).min().unwrap_or(0);
    let max_x = path.vertices.iter().map(|p| p.x as i64).max().unwrap_or(0);
    let min_y = path.vertices.iter().map(|p| p.y as i64).min().unwrap_or(0);
    let max_y = path.vertices.iter().map(|p| p.y as i64).max().unwrap_or(0);
    let stroke_pad = path.stroke.width as i64;
    let w = max_x
        .checked_sub(min_x)
        .and_then(|v| v.checked_add(1))
        .and_then(|v| v.checked_add(stroke_pad.saturating_mul(2)))
        .ok_or(IoError::CorruptManifest(
            "vector path bounds exceed the maximum supported dimensions",
        ))?;
    let h = max_y
        .checked_sub(min_y)
        .and_then(|v| v.checked_add(1))
        .and_then(|v| v.checked_add(stroke_pad.saturating_mul(2)))
        .ok_or(IoError::CorruptManifest(
            "vector path bounds exceed the maximum supported dimensions",
        ))?;

    if w <= 0
        || h <= 0
        || w as u64 > MAX_NATIVE_CANVAS_DIMENSION as u64
        || h as u64 > MAX_NATIVE_CANVAS_DIMENSION as u64
        || (w as u64).saturating_mul(h as u64) > MAX_NATIVE_CANVAS_PIXELS
    {
        return Err(IoError::CorruptManifest(
            "vector path bounds exceed the maximum supported dimensions",
        ));
    }
    Ok(())
}

fn manifest_to_tile(m: TileManifest) -> Result<(TileCoord, Arc<Tile>), IoError> {
    let expected_bytes = TILE_SIZE * TILE_SIZE * std::mem::size_of::<Rgba32F>();
    let raw = decode_tile_capped(&m.data, expected_bytes)?;
    if raw.len() != expected_bytes {
        return Err(IoError::CorruptTile);
    }
    let pixels: Vec<Rgba32F> = bytemuck::cast_slice(&raw).to_vec();
    let tile = Tile::from_boxed_slice(pixels.into_boxed_slice());
    Ok((m.coord, Arc::new(tile)))
}

/// Decompress a zstd tile blob, refusing to inflate beyond a single tile so a
/// crafted blob cannot exhaust memory (a decompression bomb).
fn decode_tile_capped(data: &[u8], expected: usize) -> Result<Vec<u8>, IoError> {
    let mut decoder = zstd::Decoder::new(data)?;
    let mut out = Vec::with_capacity(expected);
    // Read at most one byte past the expected tile size; anything larger is
    // rejected without materializing the full (possibly enormous) output.
    decoder
        .by_ref()
        .take(expected as u64 + 1)
        .read_to_end(&mut out)?;
    if out.len() > expected {
        return Err(IoError::CorruptTile);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ogre_core::{
        compositor::composite_document, AdjustmentKind, Document, IVec2, LayerContent, Rgba32F,
        Selection, SelectionKind, TiledBuffer,
    };

    #[test]
    fn round_trip_document_matches_original_composite() {
        let mut doc = Document::new(64, 64);

        let bg = doc.add_raster_layer("bg");
        doc.layer_mut(bg)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(10, 10), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        let group = Layer::new_group("group");
        let group_id = doc.insert_layer_above(group, bg).unwrap();
        let child = doc.add_raster_layer("child");
        doc.move_into_group(child, group_id, 0).unwrap();
        doc.layer_mut(child)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(20, 20), Rgba32F::new(0.0, 1.0, 0.0, 1.0));

        doc.add_adjustment_layer("invert", AdjustmentKind::Invert);

        let path = std::env::temp_dir().join("ogre_rt_test.ogre");
        save(&doc, &path).unwrap();
        let loaded = load(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        let region = Rect::new(0, 0, 64, 64);
        let original = composite_document(&doc, region).unwrap();
        let restored = composite_document(&loaded, region).unwrap();
        assert_eq!(original, restored);
    }

    #[test]
    fn round_trip_preserves_icc_profile() {
        let mut doc = Document::new(8, 8);
        doc.icc_profile = Some(
            crate::color::IccProfile::new(lcms2::Profile::new_srgb().icc().unwrap()).into_bytes(),
        );
        let id = doc.add_raster_layer("a");
        doc.layer_mut(id)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(1, 1), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        let path = std::env::temp_dir().join("ogre_icc_profile_test.ogre");
        save(&doc, &path).unwrap();
        let loaded = load(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert_eq!(loaded.icc_profile, doc.icc_profile);
    }

    #[test]
    fn round_trip_preserves_metadata() {
        let mut doc = Document::new(128, 96);
        doc.color_space = ColorSpace::Srgb;
        let a = doc.add_raster_layer("a");
        doc.layer_mut(a).unwrap().offset = IVec2::new(5, -3);
        doc.layer_mut(a).unwrap().opacity = 0.5;
        doc.layer_mut(a).unwrap().blend = BlendMode::Multiply;
        doc.layer_mut(a).unwrap().visible = false;
        doc.layer_mut(a).unwrap().locked = true;

        let path = std::env::temp_dir().join("ogre_meta_test.ogre");
        save(&doc, &path).unwrap();
        let loaded = load(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert_eq!(loaded.canvas, doc.canvas);
        assert_eq!(loaded.color_space, doc.color_space);
        let layer = loaded.layer(loaded.order[0]).unwrap();
        assert_eq!(layer.name, "a");
        assert_eq!(layer.offset, IVec2::new(5, -3));
        assert_eq!(layer.opacity, 0.5);
        assert_eq!(layer.blend, BlendMode::Multiply);
        assert!(!layer.visible);
        assert!(layer.locked);
    }

    #[test]
    fn round_trip_preserves_raster_mask() {
        let mut doc = Document::new(64, 64);
        let bg = doc.add_raster_layer("bg");
        doc.layer_mut(bg)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        let mut masked = ogre_core::Layer::new_raster("masked");
        let mut buffer = TiledBuffer::new();
        buffer.set_pixel(IVec2::new(10, 10), Rgba32F::new(0.0, 0.0, 1.0, 1.0));
        let mut mask = TiledBuffer::new();
        mask.set_pixel(IVec2::new(10, 10), Rgba32F::new(0.5, 0.0, 0.0, 1.0));
        masked.content = LayerContent::Raster {
            buffer,
            mask: Some(mask),
        };
        doc.insert_layer_above(masked, bg).unwrap();

        let path = std::env::temp_dir().join("ogre_mask_test.ogre");
        save(&doc, &path).unwrap();
        let loaded = load(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        let region = Rect::new(0, 0, 64, 64);
        let original = composite_document(&doc, region).unwrap();
        let restored = composite_document(&loaded, region).unwrap();
        assert_eq!(original, restored);
    }

    #[test]
    fn round_trip_preserves_selection_mask() {
        let mut doc = Document::new(32, 32);
        let mut coverage = TiledBuffer::new();
        coverage.set_pixel(IVec2::new(5, 5), Rgba32F::new(0.75, 0.0, 0.0, 0.0));
        doc.selection = Selection {
            kind: SelectionKind::Mask {
                coverage,
                bounds: Rect::new(0, 0, 10, 10),
            },
        };

        let path = std::env::temp_dir().join("ogre_sel_test.ogre");
        save(&doc, &path).unwrap();
        let loaded = load(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert_eq!(loaded.selection, doc.selection);
    }

    #[test]
    fn round_trip_preserves_active_and_removed_layers() {
        let mut doc = Document::new(32, 32);
        let a = doc.add_raster_layer("a");
        let b = doc.add_raster_layer("b");
        doc.active = Some(b);
        // Soft-delete layer `a` by removing it from the root order but keeping
        // it in the removed set (simulating a delete + undo history state).
        doc.order.retain(|id| *id != a);
        let mut removed = ahash::AHashSet::new();
        removed.insert(a);
        doc.set_removed(removed);

        let path = std::env::temp_dir().join("ogre_removed_test.ogre");
        save(&doc, &path).unwrap();
        let loaded = load(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert!(loaded.active.is_some());
        assert_eq!(loaded.layer(loaded.active.unwrap()).unwrap().name, "b");
        assert_eq!(loaded.order.len(), 1);
        assert!(loaded.all_layers().iter().any(|(_, l)| l.name == "a"));
        assert_eq!(loaded.removed_layers().len(), 1);
    }

    #[test]
    fn round_trip_preserves_nested_groups() {
        let mut doc = Document::new(32, 32);
        let bg = doc.add_raster_layer("bg");
        let outer = Layer::new_group("outer");
        let outer_id = doc.insert_layer_above(outer, bg).unwrap();
        let inner = Layer::new_group("inner");
        let inner_id = doc.insert_layer_above(inner, outer_id).unwrap();
        doc.move_into_group(inner_id, outer_id, 0).unwrap();
        let child = doc.add_raster_layer("child");
        doc.layer_mut(child)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(8, 8), Rgba32F::new(0.0, 1.0, 0.0, 1.0));
        doc.move_into_group(child, inner_id, 0).unwrap();

        let path = std::env::temp_dir().join("ogre_nested_test.ogre");
        save(&doc, &path).unwrap();
        let loaded = load(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        let region = Rect::new(0, 0, 32, 32);
        let original = composite_document(&doc, region).unwrap();
        let restored = composite_document(&loaded, region).unwrap();
        assert_eq!(original, restored);
    }

    #[test]
    fn load_rejects_bad_magic() {
        let path = std::env::temp_dir().join("ogre_bad_magic.ogre");
        std::fs::write(
            &path,
            [b'X', b'X', b'X', b'X', 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        )
        .unwrap();
        let res = load(&path);
        let _ = std::fs::remove_file(&path);
        assert!(matches!(res, Err(IoError::BadMagic)), "got {res:?}");
    }

    #[test]
    fn load_rejects_unknown_version() {
        let path = std::env::temp_dir().join("ogre_bad_version.ogre");
        let mut data = Vec::new();
        data.extend_from_slice(MAGIC);
        data.extend_from_slice(&999u32.to_le_bytes());
        data.extend_from_slice(&0u64.to_le_bytes());
        std::fs::write(&path, &data).unwrap();
        let res = load(&path);
        let _ = std::fs::remove_file(&path);
        assert!(
            matches!(res, Err(IoError::UnsupportedVersion(999))),
            "got {res:?}"
        );
    }

    #[test]
    fn load_accepts_version_1_header() {
        let mut doc = Document::new(16, 16);
        let id = doc.add_raster_layer("bg");
        doc.layer_mut(id)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(4, 4), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        let path = std::env::temp_dir().join("ogre_v1_compat.ogre");
        save(&doc, &path).unwrap();

        let mut bytes = std::fs::read(&path).unwrap();
        // Version is the little-endian u32 at bytes 4..8.
        assert_eq!(bytes[4..8], 2u32.to_le_bytes());
        bytes[4..8].copy_from_slice(&1u32.to_le_bytes());

        // Patch the manifest version field as well so rebuild_document accepts it.
        let manifest_len = u64::from_le_bytes(bytes[8..16].try_into().unwrap()) as usize;
        let mut manifest: Manifest = rmp_serde::from_slice(&bytes[16..16 + manifest_len]).unwrap();
        manifest.version = 1;
        let manifest_bytes = rmp_serde::to_vec_named(&manifest).unwrap();
        let mut patched = Vec::new();
        patched.extend_from_slice(MAGIC);
        patched.extend_from_slice(&1u32.to_le_bytes());
        patched.extend_from_slice(&(manifest_bytes.len() as u64).to_le_bytes());
        patched.extend_from_slice(&manifest_bytes);
        std::fs::write(&path, &patched).unwrap();

        let loaded = load(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        let region = Rect::new(0, 0, 16, 16);
        let original = composite_document(&doc, region).unwrap();
        let restored = composite_document(&loaded, region).unwrap();
        assert_eq!(original, restored);
    }

    #[test]
    fn load_rejects_manifest_length_larger_than_file() {
        // A header that claims a 100 MB manifest in a 24-byte file must be
        // rejected before allocating, not after a giant `vec![0; len]`.
        let path = std::env::temp_dir().join("ogre_oversized_len.ogre");
        let mut data = Vec::new();
        data.extend_from_slice(MAGIC);
        data.extend_from_slice(&VERSION.to_le_bytes());
        data.extend_from_slice(&100_000_000u64.to_le_bytes());
        data.extend_from_slice(&[0u8; 8]);
        std::fs::write(&path, &data).unwrap();
        let res = load(&path);
        let _ = std::fs::remove_file(&path);
        assert!(
            matches!(res, Err(IoError::CorruptManifest(_))),
            "got {res:?}"
        );
    }

    fn write_manifest_fixture(name: &str, manifest: &Manifest) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(name);
        let manifest_bytes = rmp_serde::to_vec_named(manifest).unwrap();
        let mut data = Vec::new();
        data.extend_from_slice(MAGIC);
        data.extend_from_slice(&VERSION.to_le_bytes());
        data.extend_from_slice(&(manifest_bytes.len() as u64).to_le_bytes());
        data.extend_from_slice(&manifest_bytes);
        std::fs::write(&path, data).unwrap();
        path
    }

    fn empty_manifest(canvas: Rect) -> Manifest {
        Manifest {
            version: VERSION,
            canvas,
            color_space: ColorSpace::LinearSrgb,
            icc_profile: None,
            active: None,
            root_order: Vec::new(),
            selection: SelectionManifest::None,
            layers: Vec::new(),
            removed: Vec::new(),
        }
    }

    #[test]
    fn load_rejects_oversized_native_canvas() {
        let manifest = empty_manifest(Rect::new(0, 0, 8193, 16));
        let path = write_manifest_fixture("ogre_oversized_canvas.ogre", &manifest);
        let res = load(&path);
        let _ = std::fs::remove_file(&path);
        assert!(
            matches!(res, Err(IoError::CorruptManifest(_))),
            "got {res:?}"
        );
    }

    #[test]
    fn load_clips_persisted_rect_selection_to_canvas() {
        let mut manifest = empty_manifest(Rect::new(0, 0, 64, 32));
        manifest.selection = SelectionManifest::Rect(Rect::new(-10, -10, 100, 100));
        let path = write_manifest_fixture("ogre_clips_selection.ogre", &manifest);
        let loaded = load(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(loaded.selection, Selection::rect(Rect::new(0, 0, 64, 32)));
    }

    #[test]
    fn load_rejects_oversized_raster_layer_bounds() {
        let mut doc = Document::new(64, 64);
        let id = doc.add_raster_layer("wide");
        let buffer = doc.layer_mut(id).unwrap().buffer_mut().unwrap();
        buffer.set_pixel(IVec2::new(0, 0), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        buffer.set_pixel(IVec2::new(8193, 0), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        let path = std::env::temp_dir().join("ogre_bad_raster_bounds.ogre");
        save(&doc, &path).unwrap();
        let res = load(&path);
        let _ = std::fs::remove_file(&path);
        assert!(
            matches!(res, Err(IoError::CorruptManifest(_))),
            "got {res:?}"
        );
    }

    #[test]
    fn load_rejects_oversized_selection_mask_bounds() {
        let mut doc = Document::new(64, 64);
        let mut coverage = TiledBuffer::new();
        coverage.set_pixel(IVec2::new(0, 0), Rgba32F::new(1.0, 0.0, 0.0, 0.0));
        coverage.set_pixel(IVec2::new(8193, 0), Rgba32F::new(1.0, 0.0, 0.0, 0.0));
        doc.selection = Selection {
            kind: SelectionKind::Mask {
                coverage,
                bounds: Rect::new(0, 0, 64, 64),
            },
        };

        let path = std::env::temp_dir().join("ogre_bad_selection_mask_bounds.ogre");
        save(&doc, &path).unwrap();
        let res = load(&path);
        let _ = std::fs::remove_file(&path);
        assert!(
            matches!(res, Err(IoError::CorruptManifest(_))),
            "got {res:?}"
        );
    }

    #[test]
    fn load_rejects_oversized_vector_raster_cache() {
        let mut doc = Document::new(64, 64);
        let mut rasterized = TiledBuffer::new();
        rasterized.set_pixel(IVec2::new(0, 0), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        rasterized.set_pixel(IVec2::new(8193, 0), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        let data = ogre_core::VectorData {
            rasterized: Some(rasterized),
            ..Default::default()
        };
        doc.add_vector_layer("text", data);

        let path = std::env::temp_dir().join("ogre_bad_vector_cache.ogre");
        save(&doc, &path).unwrap();
        let res = load(&path);
        let _ = std::fs::remove_file(&path);
        assert!(
            matches!(res, Err(IoError::CorruptManifest(_))),
            "got {res:?}"
        );
    }

    #[test]
    fn load_rejects_oversized_vector_path_bounds() {
        let mut doc = Document::new(64, 64);
        let data = ogre_core::VectorData {
            paths: vec![ogre_core::VectorPath {
                vertices: vec![IVec2::new(0, 0), IVec2::new(8193, 0), IVec2::new(8193, 1)],
                fill: ogre_core::VectorFill::Solid(Rgba32F::new(1.0, 0.0, 0.0, 1.0)),
                stroke: ogre_core::VectorStroke {
                    color: Rgba32F::TRANSPARENT,
                    width: 0.0,
                    dash: Vec::new(),
                    cap: ogre_core::StrokeCap::Butt,
                    join: ogre_core::StrokeJoin::Miter,
                },
                closed: true,
            }],
            ..Default::default()
        };
        doc.add_vector_layer("wide", data);

        let path = std::env::temp_dir().join("ogre_bad_vector_path.ogre");
        save(&doc, &path).unwrap();
        let res = load(&path);
        let _ = std::fs::remove_file(&path);
        assert!(
            matches!(res, Err(IoError::CorruptManifest(_))),
            "got {res:?}"
        );
    }

    #[test]
    fn manifest_to_tile_rejects_oversized_blob() {
        // A zstd blob that inflates well beyond one tile must be rejected
        // without materializing the full decompressed output.
        let expected = TILE_SIZE * TILE_SIZE * std::mem::size_of::<Rgba32F>();
        let oversized = zstd::encode_all(vec![0u8; expected * 4].as_slice(), 3).unwrap();
        let res = manifest_to_tile(TileManifest {
            coord: TileCoord { x: 0, y: 0 },
            data: oversized,
        });
        assert!(matches!(res, Err(IoError::CorruptTile)), "got {res:?}");
    }
}
