// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! High-level document operations.

pub mod bucket;
pub mod extract;
pub mod finger_ops;
pub mod flip;
pub mod gradient;
pub mod layer_ops;
pub mod magic_wand;
pub mod magnetic;
pub mod mask;
pub mod matte;

pub use bucket::{fill_region, fill_selection, stroke_selection, DEFAULT_FILL_TOLERANCE};
pub use extract::{
    copy_selection_to_new_layer, cut_selection_to_new_layer, erase_selection_from_buffer,
    extract_selection,
};
pub use finger_ops::{
    blur_target, burn_target, dodge_target, heal_target, luma, range_mask, recolor_target,
    sharpen_target, sponge_target, Range,
};
pub use flip::{flip_buffer, FlipAxis};
pub use gradient::{fill_gradient, gradient_color, gradient_t, GradientKind, WrapMode};
pub use layer_ops::{
    delete_layer, duplicate_layer, merge_down, move_layer_by, move_layer_into_group, rename_layer,
    reorder_layer, set_layer_blend, set_layer_locked, set_layer_opacity, set_layer_visible,
};
pub use magic_wand::{magic_wand, quick_select, region_grow, select_by_color};
pub use magnetic::{edge_magnitude, simplify_path, snap_to_edge};
pub use mask::{apply_mask_to_buffer, MaskInit};
pub use matte::{
    remove_matte, remove_matte_with, MatteOptions, DEFAULT_EDGE_SOFTNESS, DEFAULT_MATTE_TOLERANCE,
};
