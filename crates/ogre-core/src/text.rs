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
    #[serde(default)]
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
    /// Prefer a bold face (weight 700) when shaping. Missing from older
    /// documents; defaults to `false` via serde.
    #[serde(default)]
    pub bold: bool,
    /// Prefer an italic face when shaping. Missing from older documents;
    /// defaults to `false` via serde.
    #[serde(default)]
    pub italic: bool,
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
            bold: false,
            italic: false,
        }
    }
}

impl TextBlock {
    /// Line height in document pixels.
    pub fn line_height_px(&self) -> f32 {
        self.font_size * self.line_height
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bold_italic_default_false() {
        let block = TextBlock::default();
        assert!(!block.bold);
        assert!(!block.italic);
        assert!(block.font_family.is_empty());
    }

    #[test]
    fn old_document_without_bold_italic_deserializes() {
        // Pre-bold/italic documents omit the fields; serde defaults must fill in.
        // Native `.ogre` uses named field maps (rmp-serde).
        #[derive(serde::Serialize)]
        struct OldTextBlock {
            text: String,
            font_family: String,
            font_size: f32,
            color: Rgba32F,
            line_height: f32,
            align: TextAlign,
            wrap_width: f32,
        }
        let old = OldTextBlock {
            text: "Hi".into(),
            font_family: "DejaVu Sans".into(),
            font_size: 18.0,
            color: Rgba32F::new(0.0, 0.0, 0.0, 1.0),
            line_height: 1.2,
            align: TextAlign::Left,
            wrap_width: 0.0,
        };
        let encoded = rmp_serde::to_vec_named(&old).unwrap();
        let restored: TextBlock = rmp_serde::from_slice(&encoded).unwrap();
        assert_eq!(restored.text, "Hi");
        assert_eq!(restored.font_family, "DejaVu Sans");
        assert_eq!(restored.font_size, 18.0);
        assert!(!restored.bold, "missing bold must default to false");
        assert!(!restored.italic, "missing italic must default to false");
    }

    #[test]
    fn style_fields_round_trip_msgpack() {
        let block = TextBlock {
            text: "Styled".into(),
            font_family: "Liberation Sans".into(),
            font_size: 32.0,
            bold: true,
            italic: true,
            ..Default::default()
        };
        let encoded = rmp_serde::to_vec_named(&block).unwrap();
        let back: TextBlock = rmp_serde::from_slice(&encoded).unwrap();
        assert_eq!(back, block);
    }
}
