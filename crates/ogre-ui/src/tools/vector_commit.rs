// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Shared helpers for the Shape / Pen / Type vector-commit path.
//!
//! These convert document-space geometry and rasterized buffers into the
//! layer-local representation expected by [`ogre_core::VectorData`] and
//! [`ogre_core::BrushStrokeCmd`]. Vector layers keep their path vertices in
//! layer-local space and store the document-space origin in [`ogre_core::Layer::offset`],
//! so `doc_coord = local + offset` round-trips exactly.

use glam::IVec2;
use ogre_core::{Rgba32F, TiledBuffer, VectorPath};

/// Shift `path`'s vertices so their bounding-box top-left sits at the local
/// origin, and return that top-left as the layer [`offset`](ogre_core::Layer::offset).
///
/// This preserves the path's document-space position: after localizing,
/// `local_vertex + offset == original_document_vertex`.
pub(crate) fn localize_path(path: &mut VectorPath) -> IVec2 {
    let min = path
        .vertices
        .iter()
        .copied()
        .reduce(|a, b| IVec2::new(a.x.min(b.x), a.y.min(b.y)))
        .unwrap_or(IVec2::ZERO);
    for p in &mut path.vertices {
        *p -= min;
    }
    min
}

/// Collect non-transparent pixels from `buf` into layer-local `(pos, color)`
/// edits. `doc_origin` is the document-space position of the buffer's `(0, 0)`
/// pixel (the anchor for text, zero for shape/pen rasterization);
/// `layer_offset` is the target layer's offset. The resulting local position is
/// `(doc_pixel + doc_origin) - layer_offset`.
pub(crate) fn buffer_to_edits(
    buf: &TiledBuffer,
    doc_origin: IVec2,
    layer_offset: IVec2,
) -> Vec<(IVec2, Rgba32F)> {
    let Some(bounds) = buf.exact_bounds() else {
        return Vec::new();
    };
    let mut edits = Vec::new();
    for y in bounds.y..bounds.bottom() as i32 {
        for x in bounds.x..bounds.right() as i32 {
            let p = IVec2::new(x, y);
            let px = buf.get_pixel(p);
            if px.a > 0.0 {
                edits.push(((p + doc_origin) - layer_offset, px));
            }
        }
    }
    edits
}

/// An undoable edit to an SVG-derived vector layer that re-rasterizes the
/// authoritative cache after updating paths.
#[derive(Debug)]
pub(crate) struct EditSvgVectorCmd {
    layer: ogre_core::LayerId,
    old_data: ogre_core::VectorData,
    new_data: ogre_core::VectorData,
}

impl EditSvgVectorCmd {
    pub(crate) fn new(
        layer: ogre_core::LayerId,
        old_data: ogre_core::VectorData,
        mut new_data: ogre_core::VectorData,
    ) -> Self {
        // Re-rasterize from the current paths only if the caller has not
        // supplied a fresh rasterized buffer (e.g., the Type tool's glyph cache).
        let caller_supplied_cache = new_data
            .rasterized
            .as_ref()
            .is_some_and(|r| old_data.rasterized.as_ref() != Some(r));
        if old_data.svg_source.is_some() && !caller_supplied_cache {
            if let Err(e) = ogre_io::svg::rerasterize_vector_data(&mut new_data, 96.0) {
                eprintln!("SVG re-rasterization failed: {e}; preserving path edit");
            }
        }
        Self {
            layer,
            old_data,
            new_data,
        }
    }
}

impl ogre_core::Command for EditSvgVectorCmd {
    fn label(&self) -> &'static str {
        "Edit SVG vector"
    }

    fn apply(&mut self, doc: &mut ogre_core::Document) -> ogre_core::Result<()> {
        let layer = doc.layer_mut(self.layer)?;
        let data = layer
            .vector_data_mut()
            .ok_or(ogre_core::OgreError::InvalidOperation("not a vector layer"))?;
        *data = self.new_data.clone();
        Ok(())
    }

    fn undo(&mut self, doc: &mut ogre_core::Document) {
        if let Ok(layer) = doc.layer_mut(self.layer) {
            if let Some(data) = layer.vector_data_mut() {
                *data = self.old_data.clone();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ogre_core::pixel::Rgba32F;

    #[test]
    fn localize_shifts_vertices_and_returns_offset() {
        let mut path = VectorPath {
            vertices: vec![IVec2::new(10, 20), IVec2::new(40, 20), IVec2::new(40, 50)],
            fill: ogre_core::VectorFill::None,
            stroke: ogre_core::VectorStroke {
                color: Rgba32F::TRANSPARENT,
                width: 0.0,
                dash: Vec::new(),
                cap: ogre_core::StrokeCap::Butt,
                join: ogre_core::StrokeJoin::Miter,
            },
            closed: true,
        };
        let offset = localize_path(&mut path);
        assert_eq!(offset, IVec2::new(10, 20));
        assert_eq!(path.vertices[0], IVec2::ZERO);
        assert_eq!(path.vertices[1], IVec2::new(30, 0));
        assert_eq!(path.vertices[2], IVec2::new(30, 30));
    }

    #[test]
    fn buffer_to_edits_applies_origin_and_offset() {
        // A 2x1 buffer at doc pixel (5, 6) and (6, 6).
        let mut buf = TiledBuffer::new();
        buf.set_pixel(IVec2::new(5, 6), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        buf.set_pixel(IVec2::new(6, 6), Rgba32F::new(0.0, 1.0, 0.0, 0.5));
        // doc_origin = (2, 1), layer_offset = (1, 1) → local = doc + (2,1) - (1,1).
        let edits = buffer_to_edits(&buf, IVec2::new(2, 1), IVec2::new(1, 1));
        assert_eq!(edits.len(), 2);
        assert!(edits.iter().any(|&(p, _)| p == IVec2::new(6, 6)));
        assert!(edits.iter().any(|&(p, _)| p == IVec2::new(7, 6)));
    }

    fn red_rect_path() -> ogre_core::VectorPath {
        ogre_core::VectorPath {
            vertices: vec![
                IVec2::new(0, 0),
                IVec2::new(10, 0),
                IVec2::new(10, 10),
                IVec2::new(0, 10),
            ],
            fill: ogre_core::VectorFill::Solid(Rgba32F::new(1.0, 0.0, 0.0, 1.0)),
            stroke: ogre_core::VectorStroke {
                color: Rgba32F::TRANSPARENT,
                width: 0.0,
                dash: Vec::new(),
                cap: ogre_core::StrokeCap::Butt,
                join: ogre_core::StrokeJoin::Miter,
            },
            closed: true,
        }
    }

    fn svg_source() -> ogre_core::SvgSource {
        ogre_core::SvgSource {
            source_bytes:
                br#"<svg xmlns="http://www.w3.org/2000/svg"><rect width="10" height="10"/></svg>"#
                    .to_vec(),
            element_id: String::new(),
        }
    }

    #[test]
    fn edit_svg_vector_cmd_rerasterizes_stale_cache() {
        let mut old_raster = TiledBuffer::new();
        old_raster.set_pixel(IVec2::new(2, 2), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        let old_data = ogre_core::VectorData {
            paths: vec![red_rect_path()],
            rasterized: Some(old_raster),
            svg_source: Some(svg_source()),
            ..Default::default()
        };

        let mut new_data = old_data.clone();
        // Move the path; keep the stale cache.
        for v in &mut new_data.paths[0].vertices {
            v.x += 5;
        }

        let cmd = EditSvgVectorCmd::new(ogre_core::LayerId::default(), old_data, new_data);
        // The stale red pixel at (2,2) should no longer exist after re-rasterization.
        let new_raster = cmd.new_data.rasterized.unwrap();
        assert_eq!(
            new_raster.get_pixel(IVec2::new(2, 2)),
            Rgba32F::TRANSPARENT,
            "stale cache must be replaced by current-path rasterization"
        );
    }

    #[test]
    fn edit_svg_vector_cmd_preserves_caller_supplied_rasterized_buffer() {
        let mut old_raster = TiledBuffer::new();
        old_raster.set_pixel(IVec2::new(0, 0), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        let mut new_raster = TiledBuffer::new();
        new_raster.set_pixel(IVec2::new(0, 0), Rgba32F::new(0.0, 1.0, 0.0, 1.0));

        let old_data = ogre_core::VectorData {
            paths: Vec::new(),
            rasterized: Some(old_raster),
            svg_source: Some(svg_source()),
            ..Default::default()
        };

        let new_data = ogre_core::VectorData {
            paths: Vec::new(),
            rasterized: Some(new_raster.clone()),
            svg_source: Some(svg_source()),
            ..Default::default()
        };

        let cmd = EditSvgVectorCmd::new(ogre_core::LayerId::default(), old_data, new_data);
        assert_eq!(
            cmd.new_data.rasterized.unwrap().get_pixel(IVec2::new(0, 0)),
            Rgba32F::new(0.0, 1.0, 0.0, 1.0),
            "caller-supplied rasterized buffer must not be overwritten"
        );
    }
}
