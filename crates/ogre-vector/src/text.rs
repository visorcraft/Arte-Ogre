// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Text rendering for the Type tool (§3.3.1).
//!
//! Uses `cosmic-text` for shaping + layout and rasterizes glyphs into a
//! [`TiledBuffer`]. The caller owns the [`FontSystem`] and [`SwashCache`] so
//! they can be reused across multiple text blocks.
//!
//! The [`TextBlock`] and [`TextAlign`] types live in `ogre-core` because they
//! are part of a vector layer's serializable source data.

use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, SwashCache};
use ogre_core::coord::IVec2;
use ogre_core::pixel::Rgba32F;
use ogre_core::text::{TextAlign, TextBlock};
use ogre_core::TiledBuffer;

/// Layout and rasterize a [`TextBlock`] into a document-space [`TiledBuffer`].
///
/// `box_width` wraps lines to the given pixel width (`None` = point mode, no
/// wrapping). Returns the buffer and the rendered size `(width, height)` in
/// pixels. The text's top-left corner is at the document origin `(0, 0)`; the
/// caller offsets it.
pub fn layout_and_rasterize(
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
    block: &TextBlock,
    box_width: Option<f32>,
) -> (TiledBuffer, (f32, f32)) {
    if block.text.is_empty() {
        return (TiledBuffer::new(), (0.0, 0.0));
    }

    let metrics = Metrics::new(block.font_size, block.line_height_px());
    let mut buffer = Buffer::new(font_system, metrics);

    let family = if block.font_family.is_empty() {
        Family::SansSerif
    } else {
        Family::Name(&block.font_family)
    };
    let attrs = Attrs::new().family(family);
    let align = match block.align {
        TextAlign::Left => cosmic_text::Align::Left,
        TextAlign::Center => cosmic_text::Align::Center,
        TextAlign::Right => cosmic_text::Align::Right,
    };

    // Height is unbounded for point mode; for box mode use a large value.
    buffer.set_size(box_width.map(|w| w.max(0.0)), Some(f32::INFINITY));
    buffer.set_text(&block.text, &attrs, Shaping::Advanced, Some(align));

    buffer.set_redraw(true);
    buffer.shape_until_scroll(font_system, false);

    let mut writes: Vec<(IVec2, Rgba32F)> = Vec::new();
    let mut max_x = 0.0f32;
    let mut max_y = 0.0f32;

    for run in buffer.layout_runs() {
        let line_top = run.line_top;
        for glyph in run.glyphs {
            let physical = glyph.physical((0.0, 0.0), 1.0);
            let Some(image) = swash_cache.get_image(font_system, physical.cache_key) else {
                continue;
            };
            if image.placement.width == 0 || image.placement.height == 0 {
                continue;
            }
            let gx = physical.x;
            let gy = physical.y + line_top as i32;
            let img_w = image.placement.width;
            let img_h = image.placement.height;
            for py in 0..img_h {
                for px in 0..img_w {
                    let idx = (py * img_w + px) as usize;
                    let coverage = image.data.get(idx).copied().unwrap_or(0) as f32 / 255.0;
                    if coverage > 0.0 {
                        let p = IVec2::new(gx + px as i32, gy + py as i32);
                        let c = Rgba32F::new(
                            block.color.r,
                            block.color.g,
                            block.color.b,
                            block.color.a * coverage,
                        );
                        writes.push((p, c));
                    }
                }
            }
            let right = gx as f32 + img_w as f32;
            let bottom = gy as f32 + img_h as f32;
            if right > max_x {
                max_x = right;
            }
            if bottom > max_y {
                max_y = bottom;
            }
        }
    }

    let buf = crate::rasterize::writes_to_buffer(&writes);
    (buf, (max_x, max_y))
}

/// Try to create a FontSystem from system fonts.
///
/// Returns `None` if no fonts could be loaded.
pub fn load_system_font_system() -> Option<FontSystem> {
    let font_system = FontSystem::new();
    // cosmic-text always loads at least the default fallback font if any are
    // available; we just verify the database is non-empty.
    if font_system.db().faces().count() == 0 {
        None
    } else {
        Some(font_system)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn font_system_or_skip() -> (FontSystem, SwashCache) {
        match load_system_font_system() {
            Some(fs) => (fs, SwashCache::new()),
            None => {
                eprintln!("skipping text test — no system fonts found");
                (FontSystem::new(), SwashCache::new())
            }
        }
    }

    #[test]
    fn empty_text_returns_empty() {
        let (mut fs, mut cache) = font_system_or_skip();
        let block = TextBlock {
            text: String::new(),
            ..Default::default()
        };
        let (buf, size) = layout_and_rasterize(&mut fs, &mut cache, &block, None);
        assert!(buf.is_empty());
        assert_eq!(size, (0.0, 0.0));
    }

    #[test]
    fn renders_non_empty_pixels() {
        let (mut fs, mut cache) = font_system_or_skip();
        let block = TextBlock {
            text: "AB".into(),
            font_size: 32.0,
            color: Rgba32F::new(1.0, 0.0, 0.0, 1.0),
            ..Default::default()
        };
        let (buf, (w, h)) = layout_and_rasterize(&mut fs, &mut cache, &block, None);
        assert!(
            !buf.is_empty(),
            "expected non-empty buffer for 'AB' with a valid font"
        );
        assert!(w > 0.0 && h > 0.0, "expected positive rendered size");
    }

    #[test]
    fn word_wrap_breaks_long_lines() {
        let (mut fs, mut cache) = font_system_or_skip();
        let block = TextBlock {
            text: "Hello World Foo Bar".into(),
            font_size: 16.0,
            ..Default::default()
        };
        let (buf_wide, (wide_w, _)) = layout_and_rasterize(&mut fs, &mut cache, &block, None);
        let (buf_narrow, (narrow_w, _)) =
            layout_and_rasterize(&mut fs, &mut cache, &block, Some(wide_w * 0.4));
        assert!(!buf_wide.is_empty());
        assert!(!buf_narrow.is_empty());
        assert!(
            narrow_w <= wide_w * 0.6,
            "wrapped text should be narrower than unwrapped"
        );
    }
}
