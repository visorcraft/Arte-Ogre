// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Text block types stored in the document model.
//!
//! These types live in `ogre-core` because they are part of a vector layer's
//! serializable source data. The `ogre-vector` crate rasterizes them.

use crate::pixel::Rgba32F;

/// Horizontal alignment within a wrapped text block.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TextAlign {
    /// Left-aligned (default).
    #[default]
    Left,
    /// Centered.
    Center,
    /// Right-aligned.
    Right,
}

/// A block of text to rasterize.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TextBlock {
    /// The text content (may contain newlines for manual line breaks).
    pub text: String,
    /// Font family name (e.g. "sans-serif"). Empty uses the default font.
    pub font_family: String,
    /// Font size in document pixels.
    pub font_size: f32,
    /// Text color (linear RGBA).
    pub color: Rgba32F,
    /// Line height multiplier (1.0 = natural font line height).
    pub line_height: f32,
    /// Horizontal alignment.
    pub align: TextAlign,
    /// Wrapping width in document pixels (`0.0` = no wrapping / point text).
    pub wrap_width: f32,
}

impl Default for TextBlock {
    fn default() -> Self {
        Self {
            text: String::new(),
            font_family: String::new(),
            font_size: 16.0,
            color: Rgba32F::new(0.0, 0.0, 0.0, 1.0),
            line_height: 1.2,
            align: TextAlign::default(),
            wrap_width: 0.0,
        }
    }
}

impl TextBlock {
    /// Line height in document pixels.
    pub fn line_height_px(&self) -> f32 {
        self.font_size * self.line_height
    }
}
