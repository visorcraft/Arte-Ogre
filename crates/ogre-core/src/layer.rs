// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Layer identifiers, blend modes, and the [`Layer`] type.

use crate::buffer::TiledBuffer;
use crate::coord::IVec2;
use crate::pixel::Rgba32F;

slotmap::new_key_type! {
    /// Opaque identifier for a layer in a [`Document`](crate::document::Document).
    pub struct LayerId;
}

/// How a layer's pixels combine with the pixels underneath it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum BlendMode {
    /// Source pixels replace the destination.
    #[default]
    Normal,
    /// Multiply source and destination channels.
    Multiply,
    /// Invert-multiply: `1 - (1-src)*(1-dst)`.
    Screen,
    /// Multiply or screen depending on destination.
    Overlay,
    /// Keep the darker of source and destination.
    Darken,
    /// Keep the lighter of source and destination.
    Lighten,
    /// Brighten the destination by the source.
    ColorDodge,
    /// Darken the destination by the source.
    ColorBurn,
    /// Hard light: like overlay but with source as the test.
    HardLight,
    /// Soft light: a gentler lighting effect.
    SoftLight,
    /// Absolute difference between source and destination.
    Difference,
    /// Lower-contrast difference.
    Exclusion,
    /// Add source and destination channels.
    Add,
}

impl BlendMode {
    /// Serialize the mode to a stable UI/serialization name.
    pub fn name(self) -> &'static str {
        match self {
            BlendMode::Normal => "normal",
            BlendMode::Multiply => "multiply",
            BlendMode::Screen => "screen",
            BlendMode::Overlay => "overlay",
            BlendMode::Darken => "darken",
            BlendMode::Lighten => "lighten",
            BlendMode::ColorDodge => "color-dodge",
            BlendMode::ColorBurn => "color-burn",
            BlendMode::HardLight => "hard-light",
            BlendMode::SoftLight => "soft-light",
            BlendMode::Difference => "difference",
            BlendMode::Exclusion => "exclusion",
            BlendMode::Add => "add",
        }
    }

    /// Parse a mode from its [`BlendMode::name`].
    ///
    /// Matching is case-insensitive and accepts both kebab-case and the
    /// original Rust variant spelling.
    pub fn from_name(s: &str) -> Option<Self> {
        let norm = s.to_ascii_lowercase();
        Some(match norm.as_str() {
            "normal" => BlendMode::Normal,
            "multiply" => BlendMode::Multiply,
            "screen" => BlendMode::Screen,
            "overlay" => BlendMode::Overlay,
            "darken" => BlendMode::Darken,
            "lighten" => BlendMode::Lighten,
            "color-dodge" | "colordodge" => BlendMode::ColorDodge,
            "color-burn" | "colorburn" => BlendMode::ColorBurn,
            "hard-light" | "hardlight" => BlendMode::HardLight,
            "soft-light" | "softlight" => BlendMode::SoftLight,
            "difference" => BlendMode::Difference,
            "exclusion" => BlendMode::Exclusion,
            "add" => BlendMode::Add,
            _ => return None,
        })
    }
}

/// A single input/output point on a curves curve.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CurvePoint {
    /// Input channel value in [0, 1].
    pub input: f32,
    /// Output channel value in [0, 1].
    pub output: f32,
}

impl CurvePoint {
    /// Create a new curve point.
    ///
    /// NaN components are treated as `0.0` before clamping to `[0, 1]`.
    pub fn new(input: f32, output: f32) -> Self {
        let sanitize = |v: f32| if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) };
        Self {
            input: sanitize(input),
            output: sanitize(output),
        }
    }
}

/// A non-destructive adjustment applied to the accumulated backdrop.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum AdjustmentKind {
    /// Shift brightness and scale contrast.
    BrightnessContrast {
        /// Brightness offset applied after contrast scaling.
        brightness: f32,
        /// Contrast multiplier applied around the mid-tone.
        contrast: f32,
    },
    /// Invert all RGB channels.
    Invert,
    /// Convert RGB channels to a luminance grayscale.
    Desaturate,
    /// Levels adjustment: remap input black/white points to output points with
    /// a gamma curve on the midtones.
    Levels {
        /// Input black point.
        input_black: f32,
        /// Input white point.
        input_white: f32,
        /// Output black point.
        output_black: f32,
        /// Output white point.
        output_white: f32,
        /// Gamma correction applied to the midtones.
        gamma: f32,
    },
    /// Curves adjustment defined by four sorted control points.
    ///
    /// The first point is anchored at `(0, 0)` and the last at `(1, 1)`;
    /// the two middle points define an S-curve or other tonal remap.
    Curves {
        /// Four control points, sorted by increasing `input`.
        points: [CurvePoint; 4],
    },
    /// Hue/saturation/lightness adjustment.
    HueSat {
        /// Hue shift in degrees.
        hue: f32,
        /// Saturation multiplier.
        saturation: f32,
        /// Lightness offset.
        lightness: f32,
    },
    /// Posterize: quantise each channel to a fixed number of levels.
    Posterize {
        /// Number of discrete levels per channel (clamped to ≥ 2).
        levels: u32,
    },
    /// Threshold: convert to a binary black/white image at a luminance cutoff.
    Threshold {
        /// Luminance cutoff in `0.0..=1.0`; pixels at or above are white.
        level: f32,
    },
    /// Gradient map: remap each pixel's luminance to a position along a
    /// two-colour foreground→background ramp (duotone).
    GradientMap {
        /// Colour at luminance 0 (shadow).
        fg: crate::pixel::Rgba32F,
        /// Colour at luminance 1 (highlight).
        bg: crate::pixel::Rgba32F,
    },
}

impl AdjustmentKind {
    /// Build a curves adjustment from four control points, sorting and clamping
    /// them automatically.
    pub fn curves(points: [(f32, f32); 4]) -> Self {
        let mut pts: [CurvePoint; 4] = points.map(|(i, o)| CurvePoint::new(i, o));
        pts.sort_by(|a, b| {
            a.input
                .partial_cmp(&b.input)
                .unwrap_or(core::cmp::Ordering::Equal)
        });
        // Anchor the endpoints so the curve always maps 0->0 and 1->1.
        pts[0].input = 0.0;
        pts[0].output = pts[0].output.clamp(0.0, 1.0);
        pts[3].input = 1.0;
        pts[3].output = pts[3].output.clamp(0.0, 1.0);
        Self::Curves { points: pts }
    }
}

/// Flattened vector path for non-destructive vector layers.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(from = "VectorPathRepr")]
pub struct VectorPath {
    /// Flattened polygon vertices (layer-local space).
    pub vertices: Vec<IVec2>,
    /// Fill style.
    pub fill: VectorFill,
    /// Stroke style.
    pub stroke: VectorStroke,
    /// Whether the path is closed (last vertex connects to first).
    pub closed: bool,
}

impl From<VectorPathRepr> for VectorPath {
    fn from(repr: VectorPathRepr) -> Self {
        match repr {
            VectorPathRepr::V1(v1) => Self::from(v1),
            VectorPathRepr::V2(v2) => Self::from(v2),
        }
    }
}

impl From<VectorPathV1> for VectorPath {
    fn from(v1: VectorPathV1) -> Self {
        Self {
            vertices: v1.vertices,
            fill: VectorFill::Solid(v1.fill),
            stroke: VectorStroke {
                color: v1.stroke,
                width: v1.stroke_width as f32,
                dash: Vec::new(),
                cap: StrokeCap::Butt,
                join: StrokeJoin::Miter,
            },
            closed: v1.closed,
        }
    }
}

impl From<VectorPathV2> for VectorPath {
    fn from(v2: VectorPathV2) -> Self {
        Self {
            vertices: v2.vertices,
            fill: v2.fill,
            stroke: v2.stroke,
            closed: v2.closed,
        }
    }
}

/// On-disk schema for `VectorPath` before the SVG refactor.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct VectorPathV1 {
    vertices: Vec<IVec2>,
    fill: Rgba32F,
    stroke: Rgba32F,
    stroke_width: u32,
    closed: bool,
}

/// On-disk schema for `VectorPath` after the SVG refactor.
#[derive(Clone, Debug, serde::Deserialize)]
struct VectorPathV2 {
    vertices: Vec<IVec2>,
    fill: VectorFill,
    stroke: VectorStroke,
    closed: bool,
}

/// Deserialization union that migrates legacy V1 paths and accepts V2 paths.
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(untagged)]
enum VectorPathRepr {
    V1(VectorPathV1),
    V2(VectorPathV2),
}

/// Fill style for a [`VectorPath`].
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum VectorFill {
    /// No fill.
    None,
    /// Solid color fill.
    Solid(Rgba32F),
}

/// Stroke style for a [`VectorPath`].
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct VectorStroke {
    /// Stroke color.
    pub color: Rgba32F,
    /// Stroke width in pixels.
    pub width: f32,
    /// Dash pattern as alternating on/off lengths in pixels.
    pub dash: Vec<f32>,
    /// Line cap style.
    pub cap: StrokeCap,
    /// Line join style.
    pub join: StrokeJoin,
}

/// Line cap style for a vector stroke.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum StrokeCap {
    /// Flat cap at the line endpoint.
    Butt,
    /// Rounded cap.
    Round,
    /// Square cap extending past the endpoint.
    Square,
}

/// Line join style for a vector stroke.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum StrokeJoin {
    /// Sharp mitered join.
    Miter,
    /// Rounded join.
    Round,
    /// Beveled join.
    Bevel,
}

/// Original SVG document fragment preserved for re-rasterization of SVG-backed
/// vector layers.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SvgSource {
    /// Full SVG document bytes (UTF-8 XML).
    pub source_bytes: Vec<u8>,
    /// Optional element id that this layer represents. Empty means the whole
    /// document.
    pub element_id: String,
}

/// Non-destructive vector layer content: flattened paths, optional
/// pre-rasterized content (text glyphs or imported SVG pixels), and an optional
/// preserved SVG source for re-rasterization. The `version` field is the dirty
/// flag for the compositor's on-demand rasterization cache.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct VectorData {
    /// Flattened vector paths.
    pub paths: Vec<VectorPath>,
    /// Pre-rasterized content (e.g., text glyphs) in layer-local space.
    #[serde(default)]
    pub rasterized: Option<TiledBuffer>,
    /// Source text block, preserved so the Type tool can re-edit the layer.
    #[serde(default)]
    pub text: Option<crate::text::TextBlock>,
    /// Optional SVG source preserved for re-rasterization.
    #[serde(default)]
    pub svg_source: Option<SvgSource>,
    /// Monotonic version counter — bump to force re-rasterization.
    #[serde(default)]
    pub version: u64,
}

impl VectorData {
    /// Create empty vector data.
    pub fn new() -> Self {
        Self::default()
    }

    /// Bump the version, marking the data as dirty for the compositor cache.
    pub fn mark_dirty(&mut self) {
        self.version = self.version.wrapping_add(1);
    }
}

/// The concrete contents of a layer.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum LayerContent {
    /// A raster layer owns a pixel buffer and an optional layer mask.
    ///
    /// The mask, if present, stores coverage in its red channel.
    Raster {
        /// The pixel data for this layer, in layer-local space.
        buffer: TiledBuffer,
        /// Optional per-pixel mask coverage.
        mask: Option<TiledBuffer>,
    },
    /// A group layer owns an ordered list of child layers.
    ///
    /// Children are stored bottom-to-top within the group.
    Group {
        /// Child layer identifiers, bottom-to-top.
        children: Vec<LayerId>,
    },
    /// An adjustment layer transforms the backdrop already accumulated below
    /// it, rather than holding any pixels of its own.
    Adjustment(AdjustmentKind),
    /// A non-destructive vector layer holds flattened paths that the compositor
    /// rasterizes on the fly.
    Vector(Box<VectorData>),
}

/// A single layer in an Arte Ogre document.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Layer {
    /// Stable identifier assigned by the document's [`SlotMap`](slotmap::SlotMap).
    pub id: LayerId,
    /// Human-readable layer name.
    pub name: String,
    /// Offset from document origin to layer-local origin.
    ///
    /// Document coordinates map to layer-local coordinates as
    /// `local = doc - offset`.
    pub offset: IVec2,
    /// Layer opacity, nominally in the range `0.0..=1.0`.
    pub opacity: f32,
    /// Blend mode used when compositing this layer over the result below it.
    pub blend: BlendMode,
    /// Whether the layer is visible in the canvas.
    pub visible: bool,
    /// Whether the layer is locked against edits.
    pub locked: bool,
    /// The actual content of this layer.
    pub content: LayerContent,
}

impl Layer {
    /// Create a new empty raster layer with the given name.
    ///
    /// The new layer has a placeholder [`LayerId`]; the real id is assigned by
    /// [`Document`](crate::document::Document) when the layer is inserted. It
    /// also has zero offset, full opacity, normal blending, is visible,
    /// unlocked, and holds an empty buffer with no mask.
    pub fn new_raster(name: impl Into<String>) -> Self {
        Self {
            id: LayerId::default(),
            name: name.into(),
            offset: IVec2::ZERO,
            opacity: 1.0,
            blend: BlendMode::Normal,
            visible: true,
            locked: false,
            content: LayerContent::Raster {
                buffer: TiledBuffer::new(),
                mask: None,
            },
        }
    }

    /// Create a new empty group layer with the given name.
    ///
    /// As with [`Layer::new_raster`], the layer's [`LayerId`] is overwritten
    /// by the document on insertion.
    pub fn new_group(name: impl Into<String>) -> Self {
        Self {
            id: LayerId::default(),
            name: name.into(),
            offset: IVec2::ZERO,
            opacity: 1.0,
            blend: BlendMode::Normal,
            visible: true,
            locked: false,
            content: LayerContent::Group {
                children: Vec::new(),
            },
        }
    }

    /// Create a new adjustment layer with the given name and kind.
    ///
    /// As with [`Layer::new_raster`], the layer's [`LayerId`] is overwritten
    /// by the document on insertion.
    pub fn new_adjustment(name: impl Into<String>, kind: AdjustmentKind) -> Self {
        Self {
            id: LayerId::default(),
            name: name.into(),
            offset: IVec2::ZERO,
            opacity: 1.0,
            blend: BlendMode::Normal,
            visible: true,
            locked: false,
            content: LayerContent::Adjustment(kind),
        }
    }

    /// Create a new vector layer with the given name and data.
    pub fn new_vector(name: impl Into<String>, data: VectorData) -> Self {
        Self {
            id: LayerId::default(),
            name: name.into(),
            offset: IVec2::ZERO,
            opacity: 1.0,
            blend: BlendMode::Normal,
            visible: true,
            locked: false,
            content: LayerContent::Vector(Box::new(data)),
        }
    }

    /// Access the vector data if this is a vector layer.
    pub fn vector_data(&self) -> Option<&VectorData> {
        match &self.content {
            LayerContent::Vector(data) => Some(data),
            _ => None,
        }
    }

    /// Mutably access the vector data if this is a vector layer.
    pub fn vector_data_mut(&mut self) -> Option<&mut VectorData> {
        match &mut self.content {
            LayerContent::Vector(data) => Some(data),
            _ => None,
        }
    }

    /// Access the raster buffer if this is a raster layer.
    pub fn buffer(&self) -> Option<&TiledBuffer> {
        match &self.content {
            LayerContent::Raster { buffer, .. } => Some(buffer),
            LayerContent::Group { .. } | LayerContent::Adjustment(_) | LayerContent::Vector(_) => {
                None
            }
        }
    }

    /// Mutably access the raster buffer if this is a raster layer.
    pub fn buffer_mut(&mut self) -> Option<&mut TiledBuffer> {
        match &mut self.content {
            LayerContent::Raster { buffer, .. } => Some(buffer),
            LayerContent::Group { .. } | LayerContent::Adjustment(_) | LayerContent::Vector(_) => {
                None
            }
        }
    }

    /// Access the raster mask if this is a raster layer with a mask.
    pub fn mask(&self) -> Option<&TiledBuffer> {
        match &self.content {
            LayerContent::Raster { mask, .. } => mask.as_ref(),
            LayerContent::Group { .. } | LayerContent::Adjustment(_) | LayerContent::Vector(_) => {
                None
            }
        }
    }

    /// Mutably access the raster mask if this is a raster layer with a mask.
    pub fn mask_mut(&mut self) -> Option<&mut TiledBuffer> {
        match &mut self.content {
            LayerContent::Raster { mask, .. } => mask.as_mut(),
            LayerContent::Group { .. } | LayerContent::Adjustment(_) | LayerContent::Vector(_) => {
                None
            }
        }
    }

    /// Set or clear the raster mask. No effect on non-raster layers.
    pub fn set_mask(&mut self, mask: Option<TiledBuffer>) {
        if let LayerContent::Raster { mask: slot, .. } = &mut self.content {
            *slot = mask;
        }
    }

    /// True if this layer holds raster pixels.
    pub fn is_raster(&self) -> bool {
        matches!(self.content, LayerContent::Raster { .. })
    }

    /// True if this layer is a non-destructive vector layer.
    pub fn is_vector(&self) -> bool {
        matches!(self.content, LayerContent::Vector(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::TiledBuffer;
    use crate::pixel::Rgba32F;

    #[test]
    fn blend_mode_default_is_normal() {
        assert_eq!(BlendMode::default(), BlendMode::Normal);
    }

    #[test]
    fn all_blend_modes_round_trip_through_name() {
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
            assert_eq!(BlendMode::from_name(mode.name()), Some(mode));
        }
    }

    #[test]
    fn blend_mode_from_name_case_insensitive() {
        assert_eq!(BlendMode::from_name("Normal"), Some(BlendMode::Normal));
        assert_eq!(
            BlendMode::from_name("HARD-LIGHT"),
            Some(BlendMode::HardLight)
        );
    }

    #[test]
    fn blend_mode_from_name_unknown_returns_none() {
        assert_eq!(BlendMode::from_name("not-a-mode"), None);
    }

    #[test]
    fn vector_path_v1_deserializes_to_new_model() {
        let v1 = VectorPathV1 {
            vertices: vec![IVec2::new(0, 0), IVec2::new(10, 0), IVec2::new(10, 10)],
            fill: Rgba32F::new(1.0, 0.0, 0.0, 1.0),
            stroke: Rgba32F::new(0.0, 1.0, 0.0, 1.0),
            stroke_width: 3,
            closed: true,
        };
        let encoded = rmp_serde::to_vec(&v1).unwrap();
        let decoded: VectorPath = rmp_serde::from_slice(&encoded).unwrap();
        assert_eq!(
            decoded.fill,
            VectorFill::Solid(Rgba32F::new(1.0, 0.0, 0.0, 1.0))
        );
        assert_eq!(decoded.stroke.color, Rgba32F::new(0.0, 1.0, 0.0, 1.0));
        assert_eq!(decoded.stroke.width, 3.0);
        assert!(decoded.stroke.dash.is_empty());
    }

    #[test]
    fn vector_path_v2_round_trips_through_msgpack() {
        let original = VectorPath {
            vertices: vec![IVec2::new(0, 0), IVec2::new(10, 0), IVec2::new(10, 10)],
            fill: VectorFill::Solid(Rgba32F::new(1.0, 0.0, 0.0, 1.0)),
            stroke: VectorStroke {
                color: Rgba32F::new(0.0, 1.0, 0.0, 1.0),
                width: 3.0,
                dash: vec![2.0, 1.0],
                cap: StrokeCap::Round,
                join: StrokeJoin::Bevel,
            },
            closed: true,
        };
        let encoded = rmp_serde::to_vec(&original).unwrap();
        let decoded: VectorPath = rmp_serde::from_slice(&encoded).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn new_raster_layer_has_expected_defaults() {
        let layer = Layer::new_raster("Background");
        assert_eq!(layer.name, "Background");
        assert_eq!(layer.offset, IVec2::ZERO);
        assert_eq!(layer.opacity, 1.0);
        assert_eq!(layer.blend, BlendMode::Normal);
        assert!(layer.visible);
        assert!(!layer.locked);
        assert!(layer.is_raster());
        assert!(layer.buffer().unwrap().is_empty());
    }

    #[test]
    fn raster_buffer_accessor_works() {
        let mut layer = Layer::new_raster("Paint");
        layer
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        assert_eq!(
            layer.buffer().unwrap().get_pixel(IVec2::new(5, 5)),
            Rgba32F::new(1.0, 0.0, 0.0, 1.0)
        );
    }

    #[test]
    fn group_layer_returns_none_from_buffer() {
        let mut group = Layer::new_group("Group");
        assert!(!group.is_raster());
        assert!(group.buffer().is_none());
        assert!(group.buffer_mut().is_none());
    }

    #[test]
    fn adjustment_layer_defaults_and_accessors() {
        let mut layer = Layer::new_adjustment("Invert", AdjustmentKind::Invert);
        assert_eq!(layer.name, "Invert");
        assert_eq!(layer.opacity, 1.0);
        assert!(layer.visible);
        assert!(!layer.is_raster());
        assert!(layer.buffer().is_none());
        assert!(layer.buffer_mut().is_none());
        assert!(layer.mask().is_none());
        assert_eq!(
            layer.content,
            LayerContent::Adjustment(AdjustmentKind::Invert)
        );
    }

    #[test]
    fn vector_data_can_carry_svg_source() {
        let svg =
            br#"<svg xmlns="http://www.w3.org/2000/svg"><rect width="10" height="10"/></svg>"#
                .to_vec();
        let data = VectorData {
            paths: Vec::new(),
            rasterized: None,
            text: None,
            svg_source: Some(SvgSource {
                source_bytes: svg.clone(),
                element_id: "rect1".to_string(),
            }),
            version: 0,
        };
        assert!(data.svg_source.is_some());
        let src = data.svg_source.unwrap();
        assert_eq!(src.source_bytes, svg);
        assert_eq!(src.element_id, "rect1");
    }

    #[test]
    fn vector_data_with_svg_source_round_trips_through_named_msgpack() {
        let data = VectorData {
            paths: Vec::new(),
            rasterized: None,
            text: None,
            svg_source: Some(SvgSource {
                source_bytes: b"<svg/>".to_vec(),
                element_id: "layer-1".to_string(),
            }),
            version: 7,
        };
        let encoded = rmp_serde::to_vec_named(&data).unwrap();
        let decoded: VectorData = rmp_serde::from_slice(&encoded).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn vector_data_without_svg_source_deserializes_from_old_msgpack() {
        // Simulate a msgpack payload encoded before `svg_source` existed. The
        // native `.ogre` format uses named field maps, so a missing
        // `svg_source` entry should deserialize to `None` via `#[serde(default)]`.
        #[derive(serde::Serialize)]
        struct OldVectorData {
            paths: Vec<VectorPath>,
            rasterized: Option<TiledBuffer>,
            text: Option<crate::text::TextBlock>,
            version: u64,
        }
        let old = OldVectorData {
            paths: Vec::new(),
            rasterized: None,
            text: None,
            version: 3,
        };
        let encoded = rmp_serde::to_vec_named(&old).unwrap();
        let decoded: VectorData = rmp_serde::from_slice(&encoded).unwrap();
        assert!(decoded.svg_source.is_none());
        assert_eq!(decoded.version, 3);
    }
}
