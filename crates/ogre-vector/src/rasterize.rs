// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Vector path rasterization into a [`TiledBuffer`] for the Shape and Pen tools
//! (§3.3.2, §3.3.3).
//!
//! Uses `lyon::tessellation` for the CPU ground-truth rasterizer, with
//! `kurbo::BezPath` as the input geometry. This is the C4 oracle for any future
//! GPU vector rendering.

use kurbo::Shape;
use ogre_core::{coord::IVec2, pixel::Rgba32F, TiledBuffer};

/// Fill style for [`rasterize_bezpath`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Fill {
    /// No fill.
    None,
    /// Solid color fill.
    Solid(Rgba32F),
}

/// Stroke style for [`rasterize_bezpath`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stroke {
    /// Stroke color.
    pub color: Rgba32F,
    /// Stroke width in pixels.
    pub width: f32,
}

impl Stroke {
    /// No stroke.
    pub const NONE: Self = Self {
        color: Rgba32F::TRANSPARENT,
        width: 0.0,
    };
}

/// Supersampling rate for anti-aliased CPU rasterization.
///
/// 32×32 gives 1024 sub-pixel samples, which approximates area coverage
/// closely enough to match an 8-bit GPU target on typical vector shapes.
const SS: usize = 32;

/// A rasterizable vertex produced by `lyon` tessellation.
#[derive(Clone, Copy, Debug)]
struct Vertex {
    position: [f32; 2],
}

struct VertexCtor;

impl lyon::tessellation::FillVertexConstructor<Vertex> for VertexCtor {
    fn new_vertex(&mut self, vertex: lyon::tessellation::FillVertex<'_>) -> Vertex {
        let p = vertex.position();
        Vertex {
            position: [p.x, p.y],
        }
    }
}

impl lyon::tessellation::StrokeVertexConstructor<Vertex> for VertexCtor {
    fn new_vertex(&mut self, vertex: lyon::tessellation::StrokeVertex<'_, '_>) -> Vertex {
        let p = vertex.position();
        Vertex {
            position: [p.x, p.y],
        }
    }
}

/// Rasterize a `kurbo::BezPath` with the given fill and stroke into a
/// document-space [`TiledBuffer`].
///
/// The returned buffer uses document coordinates; callers that need layer-local
/// pixels can shift by the layer offset after compositing.
pub fn rasterize_bezpath(path: &kurbo::BezPath, fill: Fill, stroke: Stroke) -> TiledBuffer {
    let lyon_path = kurbo_to_lyon(path);

    let mut fill_tris: Vec<(Vertex, Vertex, Vertex)> = Vec::new();
    let mut stroke_tris: Vec<(Vertex, Vertex, Vertex)> = Vec::new();

    if matches!(fill, Fill::Solid(_)) {
        let mut tess = lyon::tessellation::FillTessellator::new();
        let mut buffers: lyon::tessellation::VertexBuffers<Vertex, u32> =
            lyon::tessellation::VertexBuffers::new();
        let mut b = lyon::tessellation::BuffersBuilder::new(&mut buffers, VertexCtor);
        if tess
            .tessellate_path(
                &lyon_path,
                &lyon::tessellation::FillOptions::default(),
                &mut b,
            )
            .is_ok()
        {
            triangulate(&buffers.vertices, &buffers.indices, &mut fill_tris);
        }
    }

    if stroke.width > 0.0 && stroke.color.a > 0.0 {
        let mut tess = lyon::tessellation::StrokeTessellator::new();
        let mut buffers: lyon::tessellation::VertexBuffers<Vertex, u32> =
            lyon::tessellation::VertexBuffers::new();
        let mut b = lyon::tessellation::BuffersBuilder::new(&mut buffers, VertexCtor);
        let options = lyon::tessellation::StrokeOptions::default().with_line_width(stroke.width);
        if tess.tessellate_path(&lyon_path, &options, &mut b).is_ok() {
            triangulate(&buffers.vertices, &buffers.indices, &mut stroke_tris);
        }
    }

    if fill_tris.is_empty() && stroke_tris.is_empty() {
        return TiledBuffer::new();
    }

    let (min_x, min_y, max_x, max_y) = bounds(&fill_tris, &stroke_tris);
    let pad = 1;
    let min_x = (min_x.floor() as i32).saturating_sub(pad);
    let min_y = (min_y.floor() as i32).saturating_sub(pad);
    let max_x = (max_x.ceil() as i32).saturating_add(pad);
    let max_y = (max_y.ceil() as i32).saturating_add(pad);

    let w = (max_x - min_x) as usize;
    let h = (max_y - min_y) as usize;
    if w == 0 || h == 0 {
        return TiledBuffer::new();
    }

    let sw = w * SS;
    let sh = h * SS;
    let mut fill_counts = vec![0u32; sw * sh];
    let mut stroke_counts = vec![0u32; sw * sh];

    for tri in &fill_tris {
        rasterize_triangle_samples(&mut fill_counts, sw, sh, min_x, min_y, *tri);
    }
    for tri in &stroke_tris {
        rasterize_triangle_samples(&mut stroke_counts, sw, sh, min_x, min_y, *tri);
    }

    let mut writes: Vec<(IVec2, Rgba32F)> = Vec::new();
    let total = (SS * SS) as f32;

    for py in 0..h {
        for px in 0..w {
            let mut fill_hits = 0u32;
            let mut stroke_hits = 0u32;
            for sy in 0..SS {
                for sx in 0..SS {
                    let idx = (py * SS + sy) * sw + (px * SS + sx);
                    fill_hits += fill_counts[idx];
                    stroke_hits += stroke_counts[idx];
                }
            }
            let fill_alpha = (fill_hits as f32 / total).clamp(0.0, 1.0);
            let stroke_alpha = (stroke_hits as f32 / total).clamp(0.0, 1.0);
            if fill_alpha <= 0.0 && stroke_alpha <= 0.0 {
                continue;
            }
            let color = composite(fill, stroke, fill_alpha, stroke_alpha);
            writes.push((IVec2::new(min_x + px as i32, min_y + py as i32), color));
        }
    }

    writes_to_buffer(&writes)
}

/// Composite fill and stroke coverages into a single straight-alpha pixel.
fn composite(fill: Fill, stroke: Stroke, fill_alpha: f32, stroke_alpha: f32) -> Rgba32F {
    let fill_color = match fill {
        Fill::Solid(c) => c,
        Fill::None => Rgba32F::TRANSPARENT,
    };
    let one_minus_sa = 1.0 - stroke_alpha;
    let alpha = stroke_alpha + fill_alpha * one_minus_sa;
    if alpha <= 0.0 {
        return Rgba32F::TRANSPARENT;
    }
    Rgba32F::new(
        (stroke.color.r * stroke_alpha + fill_color.r * fill_alpha * one_minus_sa) / alpha,
        (stroke.color.g * stroke_alpha + fill_color.g * fill_alpha * one_minus_sa) / alpha,
        (stroke.color.b * stroke_alpha + fill_color.b * fill_alpha * one_minus_sa) / alpha,
        alpha,
    )
}

/// Convert a `kurbo::BezPath` into a `lyon::path::Path`.
fn kurbo_to_lyon(path: &kurbo::BezPath) -> lyon::path::Path {
    let mut builder = lyon::path::Path::builder();
    let mut open = false;
    for el in path.elements() {
        use kurbo::PathEl;
        match *el {
            PathEl::MoveTo(p) => {
                if open {
                    builder.end(false);
                }
                builder.begin(lyon::math::point(p.x as f32, p.y as f32));
                open = true;
            }
            PathEl::LineTo(p) => {
                builder.line_to(lyon::math::point(p.x as f32, p.y as f32));
            }
            PathEl::QuadTo(q, p) => {
                builder.quadratic_bezier_to(
                    lyon::math::point(q.x as f32, q.y as f32),
                    lyon::math::point(p.x as f32, p.y as f32),
                );
            }
            PathEl::CurveTo(c0, c1, p) => {
                builder.cubic_bezier_to(
                    lyon::math::point(c0.x as f32, c0.y as f32),
                    lyon::math::point(c1.x as f32, c1.y as f32),
                    lyon::math::point(p.x as f32, p.y as f32),
                );
            }
            PathEl::ClosePath => {
                builder.close();
                open = false;
            }
        }
    }
    if open {
        builder.end(false);
    }
    builder.build()
}

/// Build a list of triangles from indexed vertex buffers.
fn triangulate(vertices: &[Vertex], indices: &[u32], out: &mut Vec<(Vertex, Vertex, Vertex)>) {
    for tri in indices.chunks(3) {
        if tri.len() != 3 {
            continue;
        }
        out.push((
            vertices[tri[0] as usize],
            vertices[tri[1] as usize],
            vertices[tri[2] as usize],
        ));
    }
}

/// Compute the document-space bounding box of a set of triangles.
fn bounds(
    fill_tris: &[(Vertex, Vertex, Vertex)],
    stroke_tris: &[(Vertex, Vertex, Vertex)],
) -> (f32, f32, f32, f32) {
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for tri in fill_tris.iter().chain(stroke_tris.iter()) {
        for v in [tri.0, tri.1, tri.2] {
            min_x = min_x.min(v.position[0]);
            min_y = min_y.min(v.position[1]);
            max_x = max_x.max(v.position[0]);
            max_y = max_y.max(v.position[1]);
        }
    }
    (min_x, min_y, max_x, max_y)
}

/// Rasterize one triangle into a supersampled coverage count buffer.
fn rasterize_triangle_samples(
    counts: &mut [u32],
    sw: usize,
    sh: usize,
    min_x: i32,
    min_y: i32,
    tri: (Vertex, Vertex, Vertex),
) {
    let s = SS as f32;
    let offset_x = min_x as f32 * s;
    let offset_y = min_y as f32 * s;
    let a = scale(tri.0, s);
    let b = scale(tri.1, s);
    let c = scale(tri.2, s);

    let tri_min_x = a.position[0].min(b.position[0]).min(c.position[0]).floor() as i32;
    let tri_max_x = a.position[0].max(b.position[0]).max(c.position[0]).ceil() as i32;
    let tri_min_y = a.position[1].min(b.position[1]).min(c.position[1]).floor() as i32;
    let tri_max_y = a.position[1].max(b.position[1]).max(c.position[1]).ceil() as i32;

    for y in tri_min_y..=tri_max_y {
        let by = y - (offset_y as i32);
        if by < 0 || by >= sh as i32 {
            continue;
        }
        let py = y as f32 + 0.5;
        for x in tri_min_x..=tri_max_x {
            let bx = x - (offset_x as i32);
            if bx < 0 || bx >= sw as i32 {
                continue;
            }
            let px = x as f32 + 0.5;
            if point_in_triangle(px, py, a.position, b.position, c.position) {
                counts[by as usize * sw + bx as usize] += 1;
            }
        }
    }
}

fn scale(v: Vertex, s: f32) -> Vertex {
    Vertex {
        position: [v.position[0] * s, v.position[1] * s],
    }
}

/// Barycentric point-in-triangle test.
fn point_in_triangle(px: f32, py: f32, a: [f32; 2], b: [f32; 2], c: [f32; 2]) -> bool {
    let denom = (b[1] - c[1]) * (a[0] - c[0]) + (c[0] - b[0]) * (a[1] - c[1]);
    if denom.abs() < 1e-12 {
        return false;
    }
    let u = ((b[1] - c[1]) * (px - c[0]) + (c[0] - b[0]) * (py - c[1])) / denom;
    let v = ((c[1] - a[1]) * (px - c[0]) + (a[0] - c[0]) * (py - c[1])) / denom;
    let w = 1.0 - u - v;
    u >= -1e-6 && v >= -1e-6 && w >= -1e-6
}

/// Flatten a `kurbo::BezPath` into a polygon of document pixels.
///
/// Useful for previews and for converting a closed path to a selection mask.
pub fn bezpath_to_polygon(path: &kurbo::BezPath, tolerance: f64) -> Vec<IVec2> {
    let mut pts = Vec::new();
    kurbo::flatten(path.iter(), tolerance, |el| match el {
        kurbo::PathEl::MoveTo(p) | kurbo::PathEl::LineTo(p) => {
            pts.push(IVec2::new(p.x as i32, p.y as i32));
        }
        kurbo::PathEl::ClosePath => {}
        _ => {}
    });
    pts
}

/// Approximate an ellipse as a `kurbo::BezPath`.
pub fn ellipse_bezpath(cx: f64, cy: f64, rx: f64, ry: f64) -> kurbo::BezPath {
    kurbo::Ellipse::new((cx, cy), (rx, ry), 0.0).into_path(1e-3)
}

/// Build a rounded rectangle as a `kurbo::BezPath`.
pub fn rounded_rect_bezpath(x: f64, y: f64, w: f64, h: f64, radius: f64) -> kurbo::BezPath {
    kurbo::RoundedRect::new(x, y, x + w, y + h, radius).to_path(1e-3)
}

/// Pack a list of `(document pixel, color)` writes into a document-space
/// [`TiledBuffer`].
pub fn writes_to_buffer(writes: &[(IVec2, Rgba32F)]) -> TiledBuffer {
    let mut buf = TiledBuffer::new();
    for (p, c) in writes {
        buf.set_pixel(*p, *c);
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit_square_path() -> kurbo::BezPath {
        let mut path = kurbo::BezPath::new();
        path.move_to((0.0, 0.0));
        path.line_to((1.0, 0.0));
        path.line_to((1.0, 1.0));
        path.line_to((0.0, 1.0));
        path.close_path();
        path
    }

    #[test]
    fn fill_unit_square_writes_one_pixel() {
        let buf = rasterize_bezpath(
            &unit_square_path(),
            Fill::Solid(Rgba32F::new(1.0, 0.0, 0.0, 1.0)),
            Stroke::NONE,
        );
        assert!(!buf.is_empty());
        assert_eq!(
            buf.get_pixel(IVec2::new(0, 0)),
            Rgba32F::new(1.0, 0.0, 0.0, 1.0)
        );
    }

    #[test]
    fn fill_polygon_to_buffer_round_trips() {
        let mut path = kurbo::BezPath::new();
        path.move_to((0.0, 0.0));
        path.line_to((4.0, 0.0));
        path.line_to((4.0, 4.0));
        path.line_to((0.0, 4.0));
        path.close_path();
        let buf = rasterize_bezpath(
            &path,
            Fill::Solid(Rgba32F::new(0.0, 1.0, 0.0, 1.0)),
            Stroke::NONE,
        );
        // Interior pixel is green.
        assert_eq!(
            buf.get_pixel(IVec2::new(2, 2)),
            Rgba32F::new(0.0, 1.0, 0.0, 1.0)
        );
    }

    #[test]
    fn ellipse_bezpath_is_non_empty() {
        let path = ellipse_bezpath(10.0, 10.0, 5.0, 5.0);
        let buf = rasterize_bezpath(
            &path,
            Fill::Solid(Rgba32F::new(1.0, 0.0, 0.0, 1.0)),
            Stroke::NONE,
        );
        assert!(!buf.is_empty());
    }

    #[test]
    fn stroke_adds_outline() {
        let buf = rasterize_bezpath(
            &unit_square_path(),
            Fill::None,
            Stroke {
                color: Rgba32F::new(1.0, 0.0, 0.0, 1.0),
                width: 1.0,
            },
        );
        assert!(!buf.is_empty());
    }

    #[test]
    fn bezpath_to_polygon_flattens_curves() {
        let path = ellipse_bezpath(0.0, 0.0, 10.0, 10.0);
        let poly = bezpath_to_polygon(&path, 0.5);
        assert!(poly.len() >= 4);
    }
}
