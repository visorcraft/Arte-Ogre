// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Vector/brush rendering crate for Arte Ogre.
//!
//! `ogre-vector` turns raw pointer input into smoothed, variable-width stroke
//! geometry (see [`stroke`]) and provides CPU path rasterization via `lyon`
//! plus text shaping via `cosmic-text` for the vector tools.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod rasterize;
pub mod stroke;
pub mod text;

pub use cosmic_text::{FontSystem, SwashCache};
pub use rasterize::{
    bezpath_to_polygon, ellipse_bezpath, rasterize_bezpath, rounded_rect_bezpath, writes_to_buffer,
    Fill, Stroke,
};
pub use text::{layout_and_rasterize, load_system_font_system};
// TextBlock/TextAlign live in ogre-core because they are document data.
pub use ogre_core::text::{TextAlign, TextBlock};
