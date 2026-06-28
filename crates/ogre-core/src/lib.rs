// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! `ogre-core` — headless layer engine for Arte Ogre.
//!
//! This crate provides the ground-truth CPU representation of a layered image:
//! sparse tiled copy-on-write pixel buffers, layers, selections, undo history,
//! and the reference compositor against which the GPU implementation is tested.
//!
//! All coordinates use a top-left origin with +y pointing down. Tiles are
//! 256×256 pixels and live in layer-local space; document coordinates are
//! `doc = local + layer.offset`.
//!
//! # Stable public surface
//!
//! The items re-exported at the crate root are the intended API for downstream
//! crates (`ogre-gpu`, `ogre-ui`, and the `ogre` binary). Sub-modules remain
//! public for advanced use, but most callers should import from here.

#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod brush;
pub mod buffer;
pub mod commands;
pub mod compositor;
pub mod coord;
pub mod document;
pub mod error;
pub mod history;
pub mod layer;
pub mod ops;
pub mod pixel;
pub mod resample;
pub mod selection;
pub mod text;
pub mod tile;

// ------------------------------------------------------------------
// Stable re-exports
// ------------------------------------------------------------------

/// Brush input sample, settings, paint mode, and rasterization.
pub use crate::brush::{rasterize_stroke, BrushSettings, InputSample, PaintMode};

/// Sparse tiled copy-on-write pixel buffer.
pub use crate::buffer::TiledBuffer;

/// CPU reference compositor and pixel blending.
pub use crate::compositor::{
    blend_pixel, composite_document, composite_region, rasterize_vector_data,
    rasterize_vector_paths, sample_document_pixel,
};

/// Coordinate types and helpers.
pub use crate::coord::{tile_rect, tile_to_pixel, IVec2, Rect, TileCoord};

/// Document model and color space.
pub use crate::document::{ColorSpace, Document, SliceRect};

/// Error type and result alias.
pub use crate::error::{OgreError, Result};

/// Undo/redo history and built-in commands.
pub use crate::commands::{
    AddRasterLayerCmd, AddVectorLayerCmd, BatchCmd, CanvasAnchor, CropCmd, DuplicateLayerCmd,
    EditVectorCmd, PasteImageLayerCmd, RenameCmd, ResizeCanvasCmd, SetBlendCmd, SetOpacityCmd,
    SetVisibleCmd,
};
pub use crate::history::{
    AddAdjustmentLayerCmd, AddLayerMaskCmd, ApplyLayerMaskCmd, BrushStrokeCmd, Command,
    CopyToNewLayerCmd, CutToNewLayerCmd, DeleteLayerCmd, DeleteLayerMaskCmd, FeatherSelectionCmd,
    FillSelectionCmd, FlipLayerCmd, GradientFillCmd, GrowSelectionCmd, History,
    InsertLayerSubtreeCmd, InvertSelectionCmd, MagicWandCmd, MergeDownCmd, MoveLayerByCmd,
    PaintBucketCmd, PaintCmd, PaintStrokeCmd, RemoveBackgroundCmd, ReorderCmd, SetAdjustmentCmd,
    SetLayerBufferCmd, SetLayerLockedCmd, SetLayerMaskCmd, SetSelectionCmd, ShrinkSelectionCmd,
    StrokeSelectionCmd, TransformLayerCmd,
};
/// Layer identifiers, blend modes, adjustment kinds, curve points, SVG source
/// payloads, and the layer type.
pub use crate::layer::{
    AdjustmentKind, BlendMode, CurvePoint, Layer, LayerContent, LayerId, StrokeCap, StrokeJoin,
    SvgSource, VectorData, VectorFill, VectorPath, VectorStroke,
};

/// High-level document operations.
pub use crate::ops::{
    apply_mask_to_buffer, blur_target, burn_target, copy_selection_to_new_layer,
    cut_selection_to_new_layer, delete_layer, dodge_target, duplicate_layer, edge_magnitude,
    erase_selection_from_buffer, extract_selection, fill_gradient, fill_region, fill_selection,
    flip_buffer, gradient_color, gradient_t, heal_target, luma, magic_wand, merge_down,
    move_layer_by, move_layer_into_group, quick_select, range_mask, recolor_target, region_grow,
    remove_matte, remove_matte_with, rename_layer, reorder_layer, select_by_color, set_layer_blend,
    set_layer_locked, set_layer_opacity, set_layer_visible, sharpen_target, simplify_path,
    snap_to_edge, sponge_target, stroke_selection, FlipAxis, GradientKind, MaskInit, MatteOptions,
    Range, WrapMode, DEFAULT_EDGE_SOFTNESS, DEFAULT_FILL_TOLERANCE, DEFAULT_MATTE_TOLERANCE,
};
/// 32-bit floating-point RGBA pixel type, sRGB encode, and perceptual distance.
pub use crate::pixel::{perceptual_distance_sq, srgb_encode, Rgba32F};

/// Image resampling for transform tools.
pub use crate::resample::{bezier_warp, projective_warp, Filter, WarpGrid};

/// Selection model.
pub use crate::selection::{Selection, SelectionKind, SelectionMode};

/// Text block stored inside vector layers.
pub use crate::text::{TextAlign, TextBlock};

/// A single 256×256 pixel tile and the tile side length.
pub use crate::tile::{Tile, TILE_SIZE};
