// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Tool icons: render bundled Breeze SVGs as tinted egui images.

use egui::Color32;

use crate::tools::ToolKind;

const EYE_OPEN_SVG: &[u8] = include_bytes!("../assets/icons/eye-open.svg");
const EYE_CLOSED_SVG: &[u8] = include_bytes!("../assets/icons/eye-closed.svg");
const SWAP_SVG: &[u8] = include_bytes!("../assets/icons/swap.svg");
const DRAG_HANDLE_SVG: &[u8] = include_bytes!("../assets/icons/drag-handle.svg");
const DUPLICATE_SVG: &[u8] = include_bytes!("../assets/icons/duplicate.svg");
const ADD_SVG: &[u8] = include_bytes!("../assets/icons/add.svg");
const TRASH_SVG: &[u8] = include_bytes!("../assets/icons/trash.svg");

/// Inline padlock SVGs (drawn at 24×24, stroke-based, matching the Breeze
/// icon stroke weight). No external file dependency.
const LOCK_CLOSED_SVG: &[u8] = b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"24\" height=\"24\" viewBox=\"0 0 24 24\" fill=\"#ffffff\" stroke=\"#ffffff\" stroke-width=\"2\" stroke-linecap=\"round\" stroke-linejoin=\"round\"><rect x=\"5\" y=\"11\" width=\"14\" height=\"10\" rx=\"1.5\"/><path d=\"M8 11V8a4 4 0 0 1 8 0v3\" fill=\"none\"/></svg>";
const LOCK_OPEN_SVG: &[u8] = b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"24\" height=\"24\" viewBox=\"0 0 24 24\" fill=\"#ffffff\" stroke=\"#ffffff\" stroke-width=\"2\" stroke-linecap=\"round\" stroke-linejoin=\"round\"><rect x=\"5\" y=\"11\" width=\"14\" height=\"10\" rx=\"1.5\"/><path d=\"M8 11V8a4 4 0 0 1 7.5-2\" fill=\"none\"/></svg>";

/// Drag-reorder grip: two vertical bars with a dot between them. (A plain dash
/// read as a "remove" minus; an SVG avoids any font-glyph confusion.)
pub fn drag_handle_image(size: f32, tint: Color32) -> egui::Image<'static> {
    egui::Image::from_bytes("bytes://ogre/icons/drag-handle.svg", DRAG_HANDLE_SVG)
        .tint(tint)
        .fit_to_exact_size(egui::vec2(size, size))
}

/// Duplicate-layer icon: the familiar two-overlapping-sheets "copy" glyph.
/// Clearer than the bare `📄` page emoji it replaces.
pub fn duplicate_image(size: f32, tint: Color32) -> egui::Image<'static> {
    egui::Image::from_bytes("bytes://ogre/icons/duplicate.svg", DUPLICATE_SVG)
        .tint(tint)
        .fit_to_exact_size(egui::vec2(size, size))
}

/// Plus icon for the "new layer" toolbar button.
pub fn add_image(size: f32, tint: Color32) -> egui::Image<'static> {
    egui::Image::from_bytes("bytes://ogre/icons/add.svg", ADD_SVG)
        .tint(tint)
        .fit_to_exact_size(egui::vec2(size, size))
}

/// Trash-can icon for the "delete layer" toolbar button.
pub fn trash_image(size: f32, tint: Color32) -> egui::Image<'static> {
    egui::Image::from_bytes("bytes://ogre/icons/trash.svg", TRASH_SVG)
        .tint(tint)
        .fit_to_exact_size(egui::vec2(size, size))
}

/// Eye icon for a layer's visibility toggle: an open eye when `visible`, a
/// crossed-out eye when hidden.
pub fn eye_image(visible: bool, size: f32, tint: Color32) -> egui::Image<'static> {
    let (uri, bytes) = if visible {
        ("bytes://ogre/icons/eye-open.svg", EYE_OPEN_SVG)
    } else {
        ("bytes://ogre/icons/eye-closed.svg", EYE_CLOSED_SVG)
    };
    egui::Image::from_bytes(uri, bytes)
        .tint(tint)
        .fit_to_exact_size(egui::vec2(size, size))
}

/// Two-arrows "swap" icon (for the foreground/background colour swap).
pub fn swap_image(size: f32, tint: Color32) -> egui::Image<'static> {
    egui::Image::from_bytes("bytes://ogre/icons/swap.svg", SWAP_SVG)
        .tint(tint)
        .fit_to_exact_size(egui::vec2(size, size))
}

/// Padlock icon for a layer's lock toggle: closed when `locked`, open when
/// unlocked.
pub fn lock_image(locked: bool, size: f32, tint: Color32) -> egui::Image<'static> {
    let (uri, bytes) = if locked {
        ("bytes://ogre/icons/lock-closed.svg", LOCK_CLOSED_SVG)
    } else {
        ("bytes://ogre/icons/lock-open.svg", LOCK_OPEN_SVG)
    };
    egui::Image::from_bytes(uri, bytes)
        .tint(tint)
        .fit_to_exact_size(egui::vec2(size, size))
}

/// A tinted, sized egui image for a tool's Breeze icon.
///
/// The SVG bytes are referenced by a stable `bytes://` URI so egui's image
/// loader decodes each icon once and caches the texture; `tint` is applied at
/// draw time and is free to vary per frame (active vs inactive row).
pub fn tool_image(kind: ToolKind, size: f32, tint: Color32) -> egui::Image<'static> {
    let uri = match kind {
        ToolKind::RectSelect => "bytes://ogre/icons/select-rectangular.svg",
        ToolKind::EllipseSelect => "bytes://ogre/icons/draw-ellipse.svg",
        ToolKind::PolygonLasso | ToolKind::FreehandLasso | ToolKind::MagneticLasso => {
            "bytes://ogre/icons/edit-select-lasso.svg"
        }
        ToolKind::MagicWand | ToolKind::Eyedropper => "bytes://ogre/icons/tool_color_picker.svg",
        ToolKind::QuickSelect => "bytes://ogre/icons/quick-select.svg",
        ToolKind::Brush => "bytes://ogre/icons/draw-brush.svg",
        ToolKind::Pencil => "bytes://ogre/icons/tool_pen.svg",
        ToolKind::Eraser => "bytes://ogre/icons/draw-eraser.svg",
        ToolKind::Blur => "bytes://ogre/icons/blur.svg",
        ToolKind::Sharpen => "bytes://ogre/icons/sharpen.svg",
        ToolKind::Smudge => "bytes://ogre/icons/smudge.svg",
        ToolKind::Dodge => "bytes://ogre/icons/dodge.svg",
        ToolKind::Burn => "bytes://ogre/icons/burn.svg",
        ToolKind::Sponge => "bytes://ogre/icons/sponge.svg",
        ToolKind::ColorReplacement => "bytes://ogre/icons/color-replacement.svg",
        ToolKind::CloneStamp => "bytes://ogre/icons/clone-stamp.svg",
        ToolKind::Healing => "bytes://ogre/icons/healing.svg",
        ToolKind::SpotHealing => "bytes://ogre/icons/spot-healing.svg",
        ToolKind::PaintBucket => "bytes://ogre/icons/fill-color.svg",
        ToolKind::Gradient => "bytes://ogre/icons/color-gradient.svg",
        ToolKind::Move => "bytes://ogre/icons/transform-move.svg",
        ToolKind::FreeTransform => "bytes://ogre/icons/transform-scale.svg",
        ToolKind::Crop => "bytes://ogre/icons/transform-crop-and-resize.svg",
        ToolKind::ShapeRect => "bytes://ogre/icons/shape-rect.svg",
        ToolKind::ShapeEllipse => "bytes://ogre/icons/shape-ellipse.svg",
        ToolKind::ShapeLine => "bytes://ogre/icons/shape-line.svg",
        ToolKind::ShapePolygon => "bytes://ogre/icons/shape-polygon.svg",
        ToolKind::Pen => "bytes://ogre/icons/draw-bezier-curves.svg",
        ToolKind::Type => "bytes://ogre/icons/tool_text.svg",
        ToolKind::PathSelect => "bytes://ogre/icons/path-select.svg",
        ToolKind::DirectSelect => "bytes://ogre/icons/direct-select.svg",
        ToolKind::Slice => "bytes://ogre/icons/slice.svg",
        ToolKind::Hand => "bytes://ogre/icons/transform-hand.svg",
        ToolKind::Zoom => "bytes://ogre/icons/zoom-in.svg",
    };
    egui::Image::from_bytes(uri, kind.svg_bytes())
        .tint(tint)
        .fit_to_exact_size(egui::vec2(size, size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_yields_an_image() {
        for k in ToolKind::ALL {
            // Must not panic; produces a sized, tinted image source.
            let _img = tool_image(k, 18.0, Color32::WHITE);
        }
    }
}

#[cfg(test)]
mod icon_svg_check {
    #[test]
    fn new_ui_icons_parse() {
        for (name, bytes) in [
            (
                "eye-open",
                &include_bytes!("../assets/icons/eye-open.svg")[..],
            ),
            (
                "eye-closed",
                &include_bytes!("../assets/icons/eye-closed.svg")[..],
            ),
            ("swap", &include_bytes!("../assets/icons/swap.svg")[..]),
        ] {
            usvg::Tree::from_data(bytes, &usvg::Options::default())
                .unwrap_or_else(|e| panic!("{name} svg failed to parse: {e}"));
        }
    }
}
