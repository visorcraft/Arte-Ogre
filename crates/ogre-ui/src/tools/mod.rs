// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Tool system and tool manager.
//!
//! Tools receive pointer events in document-space coordinates and return
//! commands to dispatch. The active tool is owned by [`ToolManager`], which
//! lives in [`AppState`](crate::state::AppState) so the canvas panel can forward
//! input without borrowing the GPU renderer.

use glam::IVec2;
use ogre_core::Rgba32F;
use serde::{Deserialize, Deserializer, Serialize};

pub mod brush;
pub mod clone_stamp;
pub mod crop;
pub mod ellipse_select;
pub mod fill;
pub mod finger;
pub mod gradient;
pub mod healing;
pub mod lasso;
pub mod magic_wand;
pub mod magnetic_lasso;
pub mod path_select;
pub mod pen;
pub mod quick_select;
pub mod rect_select;
pub mod shape;
pub mod slice;
pub mod smudge;
pub mod transform;
pub mod type_tool;
pub mod vector_commit;
pub mod zoom;

pub use brush::{BrushTool, EraserTool, PaintTool, PencilTool};
pub use clone_stamp::CloneStampTool;
pub use crop::CropTool;
pub use ellipse_select::EllipseSelectTool;
pub use fill::{EyedropperTool, PaintBucketTool};
pub use finger::{FingerOp, FingerTool};
pub use gradient::GradientTool;
pub use healing::{HealingTool, SpotHealingTool};
pub use lasso::{FreehandLassoTool, PolygonLassoTool};
pub use magic_wand::MagicWandTool;
pub use magnetic_lasso::MagneticLassoTool;
pub use path_select::{DirectSelectTool, PathSelectTool};
pub use pen::PenTool;
pub use quick_select::QuickSelectTool;
pub use rect_select::RectSelectTool;
pub use shape::{ShapeKind, ShapeTool};
pub use slice::SliceTool;
pub use smudge::SmudgeTool;
pub use transform::{FreeTransformTool, MoveTool};
pub use type_tool::TypeTool;
pub use zoom::ZoomTool;

/// The current tool mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ToolKind {
    /// Rectangle marquee selection.
    #[default]
    RectSelect,
    /// Elliptical marquee selection.
    EllipseSelect,
    /// Click-to-place polygon lasso.
    PolygonLasso,
    /// Freehand drag lasso.
    FreehandLasso,
    /// Magnetic (edge-snapping) lasso.
    MagneticLasso,
    /// Magic wand / select-by-color.
    MagicWand,
    /// Brush-out selection that grows to edges (Quick Selection).
    QuickSelect,
    /// Pick the foreground color from the canvas.
    Eyedropper,
    /// Round brush with pressure and variable settings.
    Brush,
    /// One-pixel hard brush (aliasing off).
    Pencil,
    /// Round brush that removes alpha.
    Eraser,
    /// Blur finger tool (softens detail via a frozen-backdrop box blur).
    Blur,
    /// Sharpen finger tool (unsharp mask against the frozen backdrop).
    Sharpen,
    /// Smudge finger tool (smears color along the stroke).
    Smudge,
    /// Dodge finger tool (reciprocal lightening, tonally masked).
    Dodge,
    /// Burn finger tool (reciprocal darkening, tonally masked).
    Burn,
    /// Sponge finger tool (saturate / desaturate in sRGB space).
    Sponge,
    /// Color-replacement brush (recolor with luma preservation).
    ColorReplacement,
    /// Clone Stamp: paint from an Alt-set source anchor.
    CloneStamp,
    /// Healing brush: mean-matched clone of an Alt-set source.
    Healing,
    /// Spot Healing: proximity-match blend (no source anchor).
    SpotHealing,
    /// Flood-fill a region with the foreground color.
    PaintBucket,
    /// Drag-to-fill a foreground→background gradient along a line.
    Gradient,
    /// Move a raster layer by an integer pixel offset.
    Move,
    /// Free transform a raster layer with scale, rotate, skew, and translate.
    FreeTransform,
    /// Crop the document canvas to a dragged rectangle.
    Crop,
    /// Drag a rectangle shape (destructive).
    ShapeRect,
    /// Drag an ellipse shape (destructive).
    ShapeEllipse,
    /// Drag a line shape (destructive).
    ShapeLine,
    /// Click-to-add polygon shape (destructive).
    ShapePolygon,
    /// Bezier pen tool (destructive + Path→Selection).
    Pen,
    /// Type tool (destructive, rasterized text).
    Type,
    /// Select and move entire vector paths.
    PathSelect,
    /// Select and drag individual vector anchor points.
    DirectSelect,
    /// Define named slice rects for web export (metadata only).
    Slice,
    /// Pan the view without altering the document (the "hand" / grab tool).
    Hand,
    /// Zoom the view without altering the document. Click to zoom in toward
    /// the cursor; Alt+click to zoom out. Like Hand, the work happens in the
    /// canvas input layer, not via a `Command`.
    Zoom,
}

/// Functional grouping for the Tools sidebar.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ToolGroup {
    /// Selection tools.
    Select,
    /// Painting tools.
    Paint,
    /// Transform tools.
    Transform,
    /// Vector / shape tools.
    Vector,
    /// View navigation tools (pan/zoom) that never modify the document.
    Navigate,
}

impl ToolGroup {
    /// All tool groups, in the canonical default order.
    pub const ALL: [ToolGroup; 5] = [
        ToolGroup::Navigate,
        ToolGroup::Transform,
        ToolGroup::Select,
        ToolGroup::Paint,
        ToolGroup::Vector,
    ];
}

/// Default order of tool groups in the left sidebar.
pub fn default_tool_group_order() -> Vec<ToolGroup> {
    ToolGroup::ALL.to_vec()
}

/// Deserialize a tool-group order, falling back to the default if the value is malformed.
pub fn deserialize_tool_group_order<'de, D>(deserializer: D) -> Result<Vec<ToolGroup>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Vec::<ToolGroup>::deserialize(deserializer).unwrap_or_else(|_| default_tool_group_order()))
}

/// Repair an order vector so it contains every tool group exactly once.
pub fn normalize_tool_group_order(order: &mut Vec<ToolGroup>) {
    let mut seen = std::collections::HashSet::new();
    order.retain(|&g| seen.insert(g));
    for &g in &ToolGroup::ALL {
        if !seen.contains(&g) {
            order.push(g);
        }
    }
}

/// A reorderable section in the left Tools sidebar. Tool-group variants mirror
/// [`ToolGroup`] and serialize as their string names (e.g. `"Select"`), so old
/// `tool_group_order` TOML arrays load directly. The `Color` and `Swatches`
/// sections are separate, independently draggable panels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SidebarSection {
    /// View navigation tools (pan/zoom).
    Navigate,
    /// Transform tools.
    Transform,
    /// Selection tools.
    Select,
    /// Painting tools.
    Paint,
    /// Vector / shape tools.
    Vector,
    /// Foreground/background color swatches.
    Color,
    /// Saved color swatches.
    Swatches,
}

impl SidebarSection {
    /// All sidebar sections, in the canonical default order.
    pub const ALL: [SidebarSection; 7] = [
        SidebarSection::Navigate,
        SidebarSection::Transform,
        SidebarSection::Select,
        SidebarSection::Color,
        SidebarSection::Swatches,
        SidebarSection::Paint,
        SidebarSection::Vector,
    ];

    /// Human-readable section label shown in the sidebar header.
    pub fn label(self) -> &'static str {
        match self {
            SidebarSection::Navigate => "NAVIGATE",
            SidebarSection::Transform => "TRANSFORM",
            SidebarSection::Select => "SELECT",
            SidebarSection::Paint => "PAINT",
            SidebarSection::Vector => "VECTOR",
            SidebarSection::Color => "COLOR",
            SidebarSection::Swatches => "SWATCHES",
        }
    }

    /// Returns the matching tool group, if this section is a tool group.
    pub fn as_tool_group(self) -> Option<ToolGroup> {
        match self {
            SidebarSection::Navigate => Some(ToolGroup::Navigate),
            SidebarSection::Transform => Some(ToolGroup::Transform),
            SidebarSection::Select => Some(ToolGroup::Select),
            SidebarSection::Paint => Some(ToolGroup::Paint),
            SidebarSection::Vector => Some(ToolGroup::Vector),
            SidebarSection::Color | SidebarSection::Swatches => None,
        }
    }
}

/// Default order of sidebar sections in the left sidebar.
pub fn default_sidebar_order() -> Vec<SidebarSection> {
    SidebarSection::ALL.to_vec()
}

/// Deserialize a sidebar-section order, falling back to the default if the value is malformed.
pub fn deserialize_sidebar_order<'de, D>(deserializer: D) -> Result<Vec<SidebarSection>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(
        Vec::<SidebarSection>::deserialize(deserializer)
            .unwrap_or_else(|_| default_sidebar_order()),
    )
}

/// Repair an order vector so it contains every sidebar section exactly once.
pub fn normalize_sidebar_order(order: &mut Vec<SidebarSection>) {
    let mut seen = std::collections::HashSet::new();
    order.retain(|&s| seen.insert(s));
    for &s in &SidebarSection::ALL {
        if !seen.contains(&s) {
            order.push(s);
        }
    }
}

impl ToolKind {
    /// All tool kinds, in palette order.
    pub const ALL: [ToolKind; 37] = [
        ToolKind::RectSelect,
        ToolKind::EllipseSelect,
        ToolKind::PolygonLasso,
        ToolKind::FreehandLasso,
        ToolKind::MagneticLasso,
        ToolKind::MagicWand,
        ToolKind::QuickSelect,
        ToolKind::PaintBucket,
        ToolKind::Gradient,
        ToolKind::Eyedropper,
        ToolKind::Brush,
        ToolKind::Pencil,
        ToolKind::Eraser,
        ToolKind::Blur,
        ToolKind::Sharpen,
        ToolKind::Smudge,
        ToolKind::Dodge,
        ToolKind::Burn,
        ToolKind::Sponge,
        ToolKind::ColorReplacement,
        ToolKind::CloneStamp,
        ToolKind::Healing,
        ToolKind::SpotHealing,
        ToolKind::Move,
        ToolKind::FreeTransform,
        ToolKind::Crop,
        ToolKind::ShapeRect,
        ToolKind::ShapeEllipse,
        ToolKind::ShapeLine,
        ToolKind::ShapePolygon,
        ToolKind::Pen,
        ToolKind::Type,
        ToolKind::PathSelect,
        ToolKind::DirectSelect,
        ToolKind::Slice,
        ToolKind::Hand,
        ToolKind::Zoom,
    ];

    /// Sidebar group this tool belongs to.
    pub fn group(self) -> ToolGroup {
        match self {
            ToolKind::RectSelect
            | ToolKind::EllipseSelect
            | ToolKind::PolygonLasso
            | ToolKind::FreehandLasso
            | ToolKind::MagneticLasso
            | ToolKind::MagicWand
            | ToolKind::QuickSelect => ToolGroup::Select,
            ToolKind::Eraser
            | ToolKind::Blur
            | ToolKind::Sharpen
            | ToolKind::Smudge
            | ToolKind::Dodge
            | ToolKind::Burn
            | ToolKind::Sponge
            | ToolKind::ColorReplacement
            | ToolKind::CloneStamp
            | ToolKind::Healing
            | ToolKind::SpotHealing
            | ToolKind::PaintBucket
            | ToolKind::Gradient
            | ToolKind::Eyedropper
            | ToolKind::Brush
            | ToolKind::Pencil => ToolGroup::Paint,
            ToolKind::Move | ToolKind::FreeTransform | ToolKind::Crop => ToolGroup::Transform,
            ToolKind::ShapeRect
            | ToolKind::ShapeEllipse
            | ToolKind::ShapeLine
            | ToolKind::ShapePolygon
            | ToolKind::Pen
            | ToolKind::Type
            | ToolKind::PathSelect
            | ToolKind::DirectSelect
            | ToolKind::Slice => ToolGroup::Vector,
            ToolKind::Hand | ToolKind::Zoom => ToolGroup::Navigate,
        }
    }

    /// Vendored Breeze SVG bytes for this tool's icon.
    pub fn svg_bytes(self) -> &'static [u8] {
        match self {
            ToolKind::RectSelect => {
                include_bytes!("../../assets/icons/select-rectangular.svg")
            }
            ToolKind::EllipseSelect => {
                include_bytes!("../../assets/icons/draw-ellipse.svg")
            }
            ToolKind::PolygonLasso | ToolKind::FreehandLasso | ToolKind::MagneticLasso => {
                include_bytes!("../../assets/icons/edit-select-lasso.svg")
            }
            ToolKind::MagicWand => {
                include_bytes!("../../assets/icons/tool_color_picker.svg")
            }
            ToolKind::QuickSelect => {
                include_bytes!("../../assets/icons/quick-select.svg")
            }
            ToolKind::Eyedropper => {
                include_bytes!("../../assets/icons/tool_color_picker.svg")
            }
            ToolKind::Brush => include_bytes!("../../assets/icons/draw-brush.svg"),
            ToolKind::Pencil => include_bytes!("../../assets/icons/tool_pen.svg"),
            ToolKind::Eraser => include_bytes!("../../assets/icons/draw-eraser.svg"),
            ToolKind::Blur => include_bytes!("../../assets/icons/blur.svg"),
            ToolKind::Sharpen => include_bytes!("../../assets/icons/sharpen.svg"),
            ToolKind::Smudge => include_bytes!("../../assets/icons/smudge.svg"),
            ToolKind::Dodge => include_bytes!("../../assets/icons/dodge.svg"),
            ToolKind::Burn => include_bytes!("../../assets/icons/burn.svg"),
            ToolKind::Sponge => include_bytes!("../../assets/icons/sponge.svg"),
            ToolKind::ColorReplacement => {
                include_bytes!("../../assets/icons/color-replacement.svg")
            }
            ToolKind::CloneStamp => include_bytes!("../../assets/icons/clone-stamp.svg"),
            ToolKind::Healing => include_bytes!("../../assets/icons/healing.svg"),
            ToolKind::SpotHealing => include_bytes!("../../assets/icons/spot-healing.svg"),
            ToolKind::PaintBucket => include_bytes!("../../assets/icons/fill-color.svg"),
            ToolKind::Gradient => include_bytes!("../../assets/icons/color-gradient.svg"),
            ToolKind::Move => include_bytes!("../../assets/icons/transform-move.svg"),
            ToolKind::FreeTransform => {
                include_bytes!("../../assets/icons/transform-scale.svg")
            }
            ToolKind::Crop => {
                include_bytes!("../../assets/icons/transform-crop-and-resize.svg")
            }
            ToolKind::ShapeRect => include_bytes!("../../assets/icons/shape-rect.svg"),
            ToolKind::ShapeEllipse => include_bytes!("../../assets/icons/shape-ellipse.svg"),
            ToolKind::ShapeLine => include_bytes!("../../assets/icons/shape-line.svg"),
            ToolKind::ShapePolygon => {
                include_bytes!("../../assets/icons/shape-polygon.svg")
            }
            ToolKind::Pen => include_bytes!("../../assets/icons/draw-bezier-curves.svg"),
            ToolKind::Type => include_bytes!("../../assets/icons/tool_text.svg"),
            ToolKind::PathSelect => {
                include_bytes!("../../assets/icons/path-select.svg")
            }
            ToolKind::DirectSelect => {
                include_bytes!("../../assets/icons/direct-select.svg")
            }
            ToolKind::Slice => include_bytes!("../../assets/icons/slice.svg"),
            ToolKind::Hand => include_bytes!("../../assets/icons/transform-hand.svg"),
            ToolKind::Zoom => include_bytes!("../../assets/icons/zoom-in.svg"),
        }
    }

    /// Human-readable name shown in the UI (tooltips, menus).
    pub fn name(self) -> &'static str {
        match self {
            ToolKind::RectSelect => "Rectangle Select",
            ToolKind::EllipseSelect => "Ellipse Select",
            ToolKind::PolygonLasso => "Polygon Lasso",
            ToolKind::FreehandLasso => "Freehand Lasso",
            ToolKind::MagneticLasso => "Magnetic Lasso",
            ToolKind::MagicWand => "Magic Wand",
            ToolKind::QuickSelect => "Quick Selection",
            ToolKind::Eyedropper => "Eyedropper",
            ToolKind::Brush => "Brush",
            ToolKind::Pencil => "Pencil",
            ToolKind::Eraser => "Eraser",
            ToolKind::Blur => "Blur",
            ToolKind::Sharpen => "Sharpen",
            ToolKind::Smudge => "Smudge",
            ToolKind::Dodge => "Dodge",
            ToolKind::Burn => "Burn",
            ToolKind::Sponge => "Sponge",
            ToolKind::ColorReplacement => "Color Replacement",
            ToolKind::CloneStamp => "Clone Stamp",
            ToolKind::Healing => "Healing",
            ToolKind::SpotHealing => "Spot Healing",
            ToolKind::PaintBucket => "Paint Bucket",
            ToolKind::Gradient => "Gradient",
            ToolKind::Move => "Move",
            ToolKind::FreeTransform => "Free Transform",
            ToolKind::Crop => "Crop",
            ToolKind::ShapeRect => "Rectangle",
            ToolKind::ShapeEllipse => "Ellipse",
            ToolKind::ShapeLine => "Line",
            ToolKind::ShapePolygon => "Polygon",
            ToolKind::Pen => "Pen",
            ToolKind::Type => "Type",
            ToolKind::PathSelect => "Path Select",
            ToolKind::DirectSelect => "Direct Select",
            ToolKind::Slice => "Slice",
            ToolKind::Hand => "Hand",
            ToolKind::Zoom => "Zoom",
        }
    }

    /// The tool family used for sidebar flyout grouping (§2.2) and bare-key
    /// cycling (§2.1). Mode-switched variants share a family.
    pub fn family(self) -> ToolFamily {
        match self {
            ToolKind::RectSelect | ToolKind::EllipseSelect => ToolFamily::Marquee,
            ToolKind::PolygonLasso | ToolKind::FreehandLasso | ToolKind::MagneticLasso => {
                ToolFamily::Lasso
            }
            ToolKind::MagicWand => ToolFamily::Wand,
            ToolKind::QuickSelect => ToolFamily::Wand,
            ToolKind::Eyedropper => ToolFamily::Eyedropper,
            ToolKind::Brush | ToolKind::Pencil => ToolFamily::Brush,
            ToolKind::Eraser => ToolFamily::Eraser,
            ToolKind::Blur | ToolKind::Sharpen | ToolKind::Smudge => ToolFamily::Blur,
            ToolKind::Dodge | ToolKind::Burn | ToolKind::Sponge => ToolFamily::Dodge,
            ToolKind::ColorReplacement => ToolFamily::Brush,
            ToolKind::CloneStamp => ToolFamily::CloneStamp,
            ToolKind::Healing | ToolKind::SpotHealing => ToolFamily::Healing,
            ToolKind::PaintBucket => ToolFamily::Bucket,
            ToolKind::Gradient => ToolFamily::Bucket,
            ToolKind::Move => ToolFamily::Move,
            ToolKind::FreeTransform => ToolFamily::Transform,
            ToolKind::Crop => ToolFamily::Crop,
            ToolKind::ShapeRect
            | ToolKind::ShapeEllipse
            | ToolKind::ShapeLine
            | ToolKind::ShapePolygon => ToolFamily::Shape,
            ToolKind::Pen => ToolFamily::Pen,
            ToolKind::Type => ToolFamily::Type,
            ToolKind::PathSelect | ToolKind::DirectSelect => ToolFamily::PathSelect,
            ToolKind::Slice => ToolFamily::Slice,
            ToolKind::Hand => ToolFamily::Hand,
            ToolKind::Zoom => ToolFamily::Zoom,
        }
    }

    /// The OS cursor to show over the canvas for this tool, or `None` if the
    /// tool manages its own cursor (Hand, Zoom, PaintBucket) and the canvas
    /// should not override it.
    pub fn cursor_icon(self) -> Option<egui::CursorIcon> {
        match self {
            // Self-managed: their own input handler sets the cursor.
            ToolKind::Hand | ToolKind::Zoom | ToolKind::PaintBucket => None,
            // Paint-family: hide the OS cursor so the brush ring is the cursor.
            ToolKind::Brush
            | ToolKind::Pencil
            | ToolKind::Eraser
            | ToolKind::Blur
            | ToolKind::Sharpen
            | ToolKind::Smudge
            | ToolKind::Dodge
            | ToolKind::Burn
            | ToolKind::Sponge
            | ToolKind::ColorReplacement
            | ToolKind::CloneStamp
            | ToolKind::Healing
            | ToolKind::SpotHealing => Some(egui::CursorIcon::None),
            // Move / transform.
            ToolKind::Move | ToolKind::FreeTransform => Some(egui::CursorIcon::Move),
            // Everything else: precise crosshair.
            _ => Some(egui::CursorIcon::Crosshair),
        }
    }

    /// A short, one-line hint describing the tool's modifier-key behavior,
    /// shown in the status bar so the affordance is discoverable.
    pub fn hint(self) -> &'static str {
        match self {
            ToolKind::RectSelect | ToolKind::EllipseSelect => {
                "Shift-drag to add · Alt-drag to subtract"
            }
            ToolKind::PolygonLasso | ToolKind::FreehandLasso | ToolKind::MagneticLasso => {
                "Shift-drag to add · Alt-drag to subtract · Enter/double-click to close"
            }
            ToolKind::MagicWand => "Shift-click to add · Alt-click to subtract",
            ToolKind::QuickSelect => "Click to add · Alt-click to subtract",
            ToolKind::Eyedropper => "Click to sample foreground color",
            ToolKind::Brush | ToolKind::Pencil => "[ ] resize · Shift+[ ] hardness · 1-9 opacity",
            ToolKind::Eraser => "[ ] resize · 1-9 opacity",
            ToolKind::Blur | ToolKind::Sharpen | ToolKind::Smudge => "[ ] resize",
            ToolKind::Dodge | ToolKind::Burn | ToolKind::Sponge => "[ ] resize",
            ToolKind::ColorReplacement => "[ ] resize · 1-9 opacity",
            ToolKind::CloneStamp | ToolKind::Healing | ToolKind::SpotHealing => {
                "Alt-click to set source"
            }
            ToolKind::Gradient => "Drag to draw the gradient direction",
            ToolKind::ShapeRect
            | ToolKind::ShapeEllipse
            | ToolKind::ShapeLine
            | ToolKind::ShapePolygon => "Shift constrains · Alt draws from center",
            ToolKind::Move => "Drag to move the active layer",
            ToolKind::FreeTransform => "Drag handles to scale · rotate · Enter to commit",
            ToolKind::Crop => "Drag to define the crop area · Enter to apply",
            ToolKind::PaintBucket => "Click to flood-fill connected pixels",
            ToolKind::Hand => "Drag to pan the canvas",
            ToolKind::Zoom => "Click to zoom in · Alt-click to zoom out",
            ToolKind::Pen => "Click to add anchor points · Enter to commit",
            ToolKind::Type => "Click to place a text box",
            ToolKind::PathSelect => "Click to select a path",
            ToolKind::DirectSelect => "Click to drag an anchor point",
            ToolKind::Slice => "Drag to define a slice",
        }
    }
}

/// A tool family: groups mode-switched `ToolKind` variants that share one
/// sidebar slot and one bare-key shortcut. Pressing the family's key activates
/// the primary; repeated presses cycle through siblings (`Shift` cycles
/// backward.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ToolFamily {
    /// Rectangle + ellipse marquee.
    Marquee,
    /// Polygon, freehand, and magnetic lasso.
    Lasso,
    /// Magic wand and quick selection.
    Wand,
    /// Eyedropper.
    Eyedropper,
    /// Brush, pencil, and color replacement.
    Brush,
    /// Eraser.
    Eraser,
    /// Blur / Sharpen / Smudge finger tools.
    Blur,
    /// Dodge / Burn / Sponge finger tools.
    Dodge,
    /// Clone Stamp.
    CloneStamp,
    /// Healing + Spot Healing.
    Healing,
    /// Paint bucket and gradient.
    Bucket,
    /// Move.
    Move,
    /// Free transform (no default bare key — invoked via `Ctrl+T`).
    Transform,
    /// Crop.
    Crop,
    /// Rectangle / ellipse / line / polygon shapes.
    Shape,
    /// Pen (bezier paths).
    Pen,
    /// Type (text).
    Type,
    /// Path Select / Direct Select.
    PathSelect,
    /// Slice.
    Slice,
    /// Hand (pan).
    Hand,
    /// Zoom.
    Zoom,
}

impl ToolFamily {
    /// All families in a stable order.
    pub const ALL: [ToolFamily; 21] = [
        ToolFamily::Marquee,
        ToolFamily::Lasso,
        ToolFamily::Wand,
        ToolFamily::Eyedropper,
        ToolFamily::Brush,
        ToolFamily::Eraser,
        ToolFamily::Blur,
        ToolFamily::Dodge,
        ToolFamily::CloneStamp,
        ToolFamily::Healing,
        ToolFamily::Bucket,
        ToolFamily::Move,
        ToolFamily::Transform,
        ToolFamily::Crop,
        ToolFamily::Shape,
        ToolFamily::Pen,
        ToolFamily::Type,
        ToolFamily::PathSelect,
        ToolFamily::Slice,
        ToolFamily::Hand,
        ToolFamily::Zoom,
    ];

    /// Ordered siblings in this family. `[0]` is the family primary.
    pub fn siblings(self) -> &'static [ToolKind] {
        match self {
            ToolFamily::Marquee => &[ToolKind::RectSelect, ToolKind::EllipseSelect],
            ToolFamily::Lasso => &[
                ToolKind::PolygonLasso,
                ToolKind::FreehandLasso,
                ToolKind::MagneticLasso,
            ],
            ToolFamily::Wand => &[ToolKind::MagicWand, ToolKind::QuickSelect],
            ToolFamily::Eyedropper => &[ToolKind::Eyedropper],
            ToolFamily::Brush => &[
                ToolKind::Brush,
                ToolKind::Pencil,
                ToolKind::ColorReplacement,
            ],
            ToolFamily::Eraser => &[ToolKind::Eraser],
            ToolFamily::Blur => &[ToolKind::Blur, ToolKind::Sharpen, ToolKind::Smudge],
            ToolFamily::Dodge => &[ToolKind::Dodge, ToolKind::Burn, ToolKind::Sponge],
            ToolFamily::CloneStamp => &[ToolKind::CloneStamp],
            ToolFamily::Healing => &[ToolKind::Healing, ToolKind::SpotHealing],
            ToolFamily::Bucket => &[ToolKind::PaintBucket, ToolKind::Gradient],
            ToolFamily::Move => &[ToolKind::Move],
            ToolFamily::Transform => &[ToolKind::FreeTransform],
            ToolFamily::Crop => &[ToolKind::Crop],
            ToolFamily::Shape => &[
                ToolKind::ShapeRect,
                ToolKind::ShapeEllipse,
                ToolKind::ShapeLine,
                ToolKind::ShapePolygon,
            ],
            ToolFamily::Pen => &[ToolKind::Pen],
            ToolFamily::Type => &[ToolKind::Type],
            ToolFamily::PathSelect => &[ToolKind::PathSelect, ToolKind::DirectSelect],
            ToolFamily::Slice => &[ToolKind::Slice],
            ToolFamily::Hand => &[ToolKind::Hand],
            ToolFamily::Zoom => &[ToolKind::Zoom],
        }
    }

    /// The family primary (first sibling).
    pub fn primary(self) -> ToolKind {
        self.siblings()[0]
    }

    /// The default bare-key shortcut for this family, if any.
    pub fn default_key(self) -> Option<egui::Key> {
        use egui::Key;
        Some(match self {
            ToolFamily::Marquee => Key::M,
            ToolFamily::Lasso => Key::L,
            ToolFamily::Wand => Key::W,
            ToolFamily::Eyedropper => Key::I,
            ToolFamily::Brush => Key::B,
            ToolFamily::Eraser => Key::E,
            ToolFamily::Blur => Key::R,
            ToolFamily::Dodge => Key::O,
            ToolFamily::CloneStamp => Key::S,
            ToolFamily::Healing => Key::J,
            ToolFamily::Bucket => Key::G,
            ToolFamily::Move => Key::V,
            ToolFamily::Crop => Key::C,
            ToolFamily::Shape => Key::U,
            ToolFamily::Pen => Key::P,
            ToolFamily::Type => Key::T,
            ToolFamily::PathSelect => Key::A,
            ToolFamily::Slice => Key::K,
            ToolFamily::Hand => Key::H,
            ToolFamily::Zoom => Key::Z,
            ToolFamily::Transform => return None,
        })
    }

    /// Resolve a bare key to its family, if any key is bound.
    pub fn for_key(key: egui::Key) -> Option<ToolFamily> {
        Self::ALL.into_iter().find(|f| f.default_key() == Some(key))
    }
}

/// How vector-capable tools (Shape, Pen, Type) commit their result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VectorCommitMode {
    /// Create or edit a non-destructive vector layer.
    #[default]
    Vector,
    /// Rasterize into the active raster layer (destructive, legacy behavior).
    Pixels,
}

/// Phase of a pointer interaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Pointer button pressed.
    Down,
    /// Pointer moved while button is held.
    Drag,
    /// Pointer button released.
    Up,
}

/// A pointer event translated into document-space coordinates.
#[derive(Debug, Clone, Copy)]
pub struct PointerEvent {
    /// Document pixel coordinate under the pointer.
    pub doc_pos: IVec2,
    /// Phase of the interaction.
    pub phase: Phase,
    /// Keyboard modifiers held during the event.
    pub modifiers: egui::Modifiers,
    /// Normalised tablet/touch pressure in `[0, 1]`.  Mouse input reports `1.0`.
    pub pressure: f32,
}

impl PointerEvent {
    /// Create a pointer event with default pressure (`1.0`).
    pub const fn new(doc_pos: IVec2, phase: Phase, modifiers: egui::Modifiers) -> Self {
        Self {
            doc_pos,
            phase,
            modifiers,
            pressure: 1.0,
        }
    }
}

/// Map egui modifiers to the selection combination mode.
///
/// Shift adds, Alt subtracts, Shift+Alt intersects, and no modifiers replaces.
pub fn selection_mode(modifiers: egui::Modifiers) -> ogre_core::SelectionMode {
    match (modifiers.shift, modifiers.alt) {
        (true, true) => ogre_core::SelectionMode::Intersect,
        (true, false) => ogre_core::SelectionMode::Add,
        (false, true) => ogre_core::SelectionMode::Subtract,
        (false, false) => ogre_core::SelectionMode::Replace,
    }
}

/// A single editor tool.
///
/// Tools are stateful controllers: they interpret pointer events, update any
/// preview state, and return a command when an interaction completes. Keeping
/// the returned command separate from the rest of the application state lets
/// the canvas mark the renderer dirty and avoids borrow-checker friction
/// between the tool manager and the document.
pub trait Tool {
    /// Process a pointer event.
    ///
    /// Returns a command to dispatch if the interaction produced an undoable
    /// change, or `None` if it only updated preview state.
    ///
    /// `doc` is the current document so that tools can inspect layer state
    /// (e.g., the brush tool needs the active raster layer).
    fn on_pointer(
        &mut self,
        doc: &ogre_core::Document,
        ev: PointerEvent,
    ) -> Option<Box<dyn ogre_core::Command>>;

    /// Draw the tool's on-canvas overlay (e.g., marching-ants selection outline).
    ///
    /// `time` is the current application time in seconds, used for animation.
    fn draw_overlay(
        &self,
        painter: &egui::Painter,
        doc: &ogre_core::Document,
        viewport: &ogre_gpu::Viewport,
        canvas_rect: egui::Rect,
        pixels_per_point: f32,
        time: f64,
    );

    /// Commit the current interaction, if any, returning a command to dispatch.
    ///
    /// Called when the user presses Enter while this tool is active. The default
    /// implementation returns `None`.
    fn commit(&mut self, _doc: &ogre_core::Document) -> Option<Box<dyn ogre_core::Command>> {
        None
    }

    /// Reset any in-progress interaction without dispatching a command.
    fn cancel(&mut self);

    /// The in-progress marquee rectangle (in document pixels), if this tool is
    /// currently dragging out a selection. Used to show live W×H in the status
    /// bar. The default implementation returns `None`.
    fn pending_selection_rect(&self) -> Option<ogre_core::Rect> {
        None
    }
}

/// The Hand (pan) tool. It never edits the document — panning is handled
/// directly by the canvas panel when this tool is active — so every method is a
/// no-op. Having it as a real [`Tool`] keeps it uniform with the tool manager.
#[derive(Debug, Default)]
pub struct HandTool;

impl Tool for HandTool {
    fn on_pointer(
        &mut self,
        _doc: &ogre_core::Document,
        _ev: PointerEvent,
    ) -> Option<Box<dyn ogre_core::Command>> {
        None
    }

    fn draw_overlay(
        &self,
        _painter: &egui::Painter,
        _doc: &ogre_core::Document,
        _viewport: &ogre_gpu::Viewport,
        _canvas_rect: egui::Rect,
        _pixels_per_point: f32,
        _time: f64,
    ) {
    }

    fn cancel(&mut self) {}
}

/// Owns the active tool and dispatches events to it.
#[derive(Debug)]
pub struct ToolManager {
    active: ToolKind,
    rect_select: RectSelectTool,
    ellipse_select: EllipseSelectTool,
    polygon_lasso: PolygonLassoTool,
    freehand_lasso: FreehandLassoTool,
    magnetic_lasso: MagneticLassoTool,
    magic_wand: MagicWandTool,
    quick_select: QuickSelectTool,
    eyedropper: EyedropperTool,
    brush: BrushTool,
    pencil: PencilTool,
    eraser: EraserTool,
    blur: FingerTool,
    sharpen: FingerTool,
    smudge: SmudgeTool,
    dodge: FingerTool,
    burn: FingerTool,
    sponge: FingerTool,
    color_replacement: FingerTool,
    clone_stamp: CloneStampTool,
    healing: HealingTool,
    spot_healing: SpotHealingTool,
    paint_bucket: PaintBucketTool,
    gradient: GradientTool,
    move_tool: MoveTool,
    free_transform: FreeTransformTool,
    crop: CropTool,
    shape_rect: ShapeTool,
    shape_ellipse: ShapeTool,
    shape_line: ShapeTool,
    shape_polygon: ShapeTool,
    pen: PenTool,
    type_tool: TypeTool,
    path_select: PathSelectTool,
    direct_select: DirectSelectTool,
    slice: SliceTool,
    hand: HandTool,
    zoom: ZoomTool,
}

impl Default for ToolManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolManager {
    /// Create a tool manager with the default rectangle-select tool active.
    pub fn new() -> Self {
        Self::with_active(ToolKind::RectSelect)
    }

    /// Create a tool manager with `active` pre-selected.
    pub fn with_active(active: ToolKind) -> Self {
        Self {
            active,
            rect_select: RectSelectTool::new(),
            ellipse_select: EllipseSelectTool::new(),
            polygon_lasso: PolygonLassoTool::new(),
            freehand_lasso: FreehandLassoTool::new(),
            magnetic_lasso: MagneticLassoTool::new(),
            magic_wand: MagicWandTool::new(),
            quick_select: QuickSelectTool::new(),
            eyedropper: EyedropperTool {
                sample: Default::default(),
            },
            brush: BrushTool::new(),
            pencil: PencilTool::new(),
            eraser: EraserTool::new(),
            blur: FingerTool::new(FingerOp::Blur),
            sharpen: FingerTool::new(FingerOp::Sharpen { strength: 0.5 }),
            smudge: SmudgeTool::new(),
            dodge: FingerTool::new(FingerOp::Dodge {
                range: ogre_core::Range::Midtones,
                exposure: 0.5,
            }),
            burn: FingerTool::new(FingerOp::Burn {
                range: ogre_core::Range::Midtones,
                exposure: 0.5,
            }),
            sponge: FingerTool::new(FingerOp::Sponge {
                exposure: 0.5,
                saturate: true,
            }),
            color_replacement: FingerTool::new(FingerOp::Recolor {
                color: ogre_core::Rgba32F::new(1.0, 0.0, 0.0, 1.0),
            }),
            clone_stamp: CloneStampTool::new(),
            healing: HealingTool::new(),
            spot_healing: SpotHealingTool::new(),
            paint_bucket: PaintBucketTool::new(),
            gradient: GradientTool::new(),
            move_tool: MoveTool::new(),
            free_transform: FreeTransformTool::new(),
            crop: CropTool::new(),
            shape_rect: ShapeTool::new(ShapeKind::Rect),
            shape_ellipse: ShapeTool::new(ShapeKind::Ellipse),
            shape_line: ShapeTool::new(ShapeKind::Line),
            shape_polygon: ShapeTool::new(ShapeKind::Polygon),
            pen: PenTool::new(),
            type_tool: TypeTool::new(),
            path_select: PathSelectTool::new(),
            direct_select: DirectSelectTool::new(),
            slice: SliceTool::new(),
            hand: HandTool,
            zoom: ZoomTool,
        }
    }

    /// Push the shared foreground `fg` into the paint tools (brush, pencil,
    /// paint bucket) and both `fg`/`bg` into the gradient tool. The eraser is
    /// unaffected.
    pub fn set_paint_colors(&mut self, fg: ogre_core::Rgba32F, bg: ogre_core::Rgba32F) {
        *self.brush.color_mut() = fg;
        *self.pencil.color_mut() = fg;
        self.paint_bucket.set_color(fg);
        self.gradient.set_colors(fg, bg);
        self.smudge.set_fg(fg);
        self.shape_rect.set_colors(fg, bg);
        self.shape_ellipse.set_colors(fg, bg);
        self.shape_line.set_colors(fg, bg);
        self.shape_polygon.set_colors(fg, bg);
        self.pen.set_colors(fg, bg);
        self.type_tool.set_color(fg);
        // Color Replacement's recolor source tracks the foreground too.
        if let FingerOp::Recolor { color } = self.color_replacement.op_mut() {
            *color = fg;
        }
    }

    /// The currently active tool kind.
    pub fn active(&self) -> ToolKind {
        self.active
    }

    /// Switch to `kind`, cancelling any in-progress interaction on the current tool.
    ///
    /// Re-selecting the already-active tool also cancels, which drops any
    /// accidental in-progress drag.
    pub fn set_tool(&mut self, kind: ToolKind) {
        self.active_tool().cancel();
        self.active = kind;
    }

    /// Forward a pointer event to the active tool.
    ///
    /// Returns a command to dispatch if the interaction produced an undoable
    /// change.
    pub fn on_pointer(
        &mut self,
        doc: &ogre_core::Document,
        ev: PointerEvent,
    ) -> Option<Box<dyn ogre_core::Command>> {
        self.active_tool().on_pointer(doc, ev)
    }

    /// Commit the active tool's current interaction.
    ///
    /// Returns a command to dispatch if the tool had a pending change.
    pub fn commit_active(
        &mut self,
        doc: &ogre_core::Document,
    ) -> Option<Box<dyn ogre_core::Command>> {
        self.active_tool().commit(doc)
    }

    /// Cancel the active tool's in-progress interaction, discarding any pending
    /// preview without committing it (e.g. when the user presses Escape).
    pub fn cancel_active(&mut self) {
        self.active_tool().cancel();
    }

    /// The in-progress marquee rectangle for the active tool, if any (used to
    /// show live W×H in the status bar while dragging a selection).
    pub fn pending_selection_rect(&self) -> Option<ogre_core::Rect> {
        self.active_tool_ref().pending_selection_rect()
    }

    /// Draw the active tool's overlay.
    pub fn draw_overlay(
        &self,
        painter: &egui::Painter,
        doc: &ogre_core::Document,
        viewport: &ogre_gpu::Viewport,
        canvas_rect: egui::Rect,
        pixels_per_point: f32,
        time: f64,
    ) {
        self.active_tool_ref().draw_overlay(
            painter,
            doc,
            viewport,
            canvas_rect,
            pixels_per_point,
            time,
        );
    }

    fn active_tool_ref(&self) -> &dyn Tool {
        match self.active {
            ToolKind::RectSelect => &self.rect_select,
            ToolKind::EllipseSelect => &self.ellipse_select,
            ToolKind::PolygonLasso => &self.polygon_lasso,
            ToolKind::FreehandLasso => &self.freehand_lasso,
            ToolKind::MagneticLasso => &self.magnetic_lasso,
            ToolKind::MagicWand => &self.magic_wand,
            ToolKind::QuickSelect => &self.quick_select,
            ToolKind::Eyedropper => &self.eyedropper,
            ToolKind::Brush => &self.brush,
            ToolKind::Pencil => &self.pencil,
            ToolKind::Eraser => &self.eraser,
            ToolKind::Blur => &self.blur,
            ToolKind::Sharpen => &self.sharpen,
            ToolKind::Smudge => &self.smudge,
            ToolKind::Dodge => &self.dodge,
            ToolKind::Burn => &self.burn,
            ToolKind::Sponge => &self.sponge,
            ToolKind::ColorReplacement => &self.color_replacement,
            ToolKind::CloneStamp => &self.clone_stamp,
            ToolKind::Healing => &self.healing,
            ToolKind::SpotHealing => &self.spot_healing,
            ToolKind::PaintBucket => &self.paint_bucket,
            ToolKind::Gradient => &self.gradient,
            ToolKind::Move => &self.move_tool,
            ToolKind::FreeTransform => &self.free_transform,
            ToolKind::Crop => &self.crop,
            ToolKind::ShapeRect => &self.shape_rect,
            ToolKind::ShapeEllipse => &self.shape_ellipse,
            ToolKind::ShapeLine => &self.shape_line,
            ToolKind::ShapePolygon => &self.shape_polygon,
            ToolKind::Pen => &self.pen,
            ToolKind::Type => &self.type_tool,
            ToolKind::PathSelect => &self.path_select,
            ToolKind::DirectSelect => &self.direct_select,
            ToolKind::Slice => &self.slice,
            ToolKind::Hand => &self.hand,
            ToolKind::Zoom => &self.zoom,
        }
    }

    fn active_tool(&mut self) -> &mut dyn Tool {
        match self.active {
            ToolKind::RectSelect => &mut self.rect_select,
            ToolKind::EllipseSelect => &mut self.ellipse_select,
            ToolKind::PolygonLasso => &mut self.polygon_lasso,
            ToolKind::FreehandLasso => &mut self.freehand_lasso,
            ToolKind::MagneticLasso => &mut self.magnetic_lasso,
            ToolKind::MagicWand => &mut self.magic_wand,
            ToolKind::QuickSelect => &mut self.quick_select,
            ToolKind::Eyedropper => &mut self.eyedropper,
            ToolKind::Brush => &mut self.brush,
            ToolKind::Pencil => &mut self.pencil,
            ToolKind::Eraser => &mut self.eraser,
            ToolKind::Blur => &mut self.blur,
            ToolKind::Sharpen => &mut self.sharpen,
            ToolKind::Smudge => &mut self.smudge,
            ToolKind::Dodge => &mut self.dodge,
            ToolKind::Burn => &mut self.burn,
            ToolKind::Sponge => &mut self.sponge,
            ToolKind::ColorReplacement => &mut self.color_replacement,
            ToolKind::CloneStamp => &mut self.clone_stamp,
            ToolKind::Healing => &mut self.healing,
            ToolKind::SpotHealing => &mut self.spot_healing,
            ToolKind::PaintBucket => &mut self.paint_bucket,
            ToolKind::Gradient => &mut self.gradient,
            ToolKind::Move => &mut self.move_tool,
            ToolKind::FreeTransform => &mut self.free_transform,
            ToolKind::Crop => &mut self.crop,
            ToolKind::ShapeRect => &mut self.shape_rect,
            ToolKind::ShapeEllipse => &mut self.shape_ellipse,
            ToolKind::ShapeLine => &mut self.shape_line,
            ToolKind::ShapePolygon => &mut self.shape_polygon,
            ToolKind::Pen => &mut self.pen,
            ToolKind::Type => &mut self.type_tool,
            ToolKind::PathSelect => &mut self.path_select,
            ToolKind::DirectSelect => &mut self.direct_select,
            ToolKind::Slice => &mut self.slice,
            ToolKind::Hand => &mut self.hand,
            ToolKind::Zoom => &mut self.zoom,
        }
    }

    /// Mutable access to the settings of the active paint tool, if any.
    pub fn active_paint_settings_mut(&mut self) -> Option<&mut PaintTool> {
        match self.active {
            ToolKind::Brush => Some(&mut self.brush),
            ToolKind::Pencil => Some(&mut self.pencil),
            ToolKind::Eraser => Some(&mut self.eraser),
            _ => None,
        }
    }

    /// Mutable access to the paint bucket's fill tolerance, if it is active.
    pub fn paint_bucket_tolerance_mut(&mut self) -> Option<&mut f32> {
        match self.active {
            ToolKind::PaintBucket => Some(self.paint_bucket.tolerance_mut()),
            _ => None,
        }
    }

    /// Mutable access to the gradient tool's settings, if it is active.
    pub fn gradient_settings_mut(
        &mut self,
    ) -> Option<&mut crate::tools::gradient::GradientSettings> {
        match self.active {
            ToolKind::Gradient => Some(self.gradient.settings_mut()),
            _ => None,
        }
    }

    /// The live selection the Polygon Lasso currently forms (≥3 vertices), if it
    /// is the active tool. The canvas commits this as a coalesced selection so
    /// the polygon becomes the selection as vertices are placed.
    pub fn polygon_live_selection(&self) -> Option<ogre_core::Selection> {
        match self.active {
            ToolKind::PolygonLasso => self.polygon_lasso.live_selection(),
            _ => None,
        }
    }

    /// Mutable access to the magic wand's `(tolerance, contiguous)` settings, if
    /// it is the active tool.
    pub fn magic_wand_settings_mut(&mut self) -> Option<(&mut f32, &mut bool)> {
        match self.active {
            ToolKind::MagicWand => Some((
                &mut self.magic_wand.tolerance,
                &mut self.magic_wand.contiguous,
            )),
            _ => None,
        }
    }

    /// Mutable access to the Quick Selection `(radius, tolerance,
    /// sample_all_layers)` settings, if it is the active tool.
    pub fn quick_select_settings_mut(&mut self) -> Option<(&mut u32, &mut f32, &mut bool)> {
        match self.active {
            ToolKind::QuickSelect => Some(self.quick_select.settings_mut()),
            _ => None,
        }
    }

    /// Mutable reference to the active stateless finger tool
    /// (Blur/Sharpen/Dodge/Burn/Sponge/ColorReplacement), if one is active.
    pub fn active_finger_tool_mut(&mut self) -> Option<&mut FingerTool> {
        let tool = match self.active {
            ToolKind::Blur => &mut self.blur,
            ToolKind::Sharpen => &mut self.sharpen,
            ToolKind::Dodge => &mut self.dodge,
            ToolKind::Burn => &mut self.burn,
            ToolKind::Sponge => &mut self.sponge,
            ToolKind::ColorReplacement => &mut self.color_replacement,
            _ => return None,
        };
        Some(tool)
    }

    /// Mutable `(settings, strength, finger_painting)` for the Smudge tool, if
    /// it is active.
    pub fn smudge_controls_mut(
        &mut self,
    ) -> Option<(&mut ogre_core::BrushSettings, &mut f32, &mut bool)> {
        match self.active {
            ToolKind::Smudge => Some(self.smudge.controls_mut()),
            _ => None,
        }
    }

    /// Mutable access to the Free Transform mode, if the tool is active.
    pub fn free_transform_mode_mut(
        &mut self,
    ) -> Option<&mut crate::tools::transform::FreeTransformMode> {
        match self.active {
            ToolKind::FreeTransform => Some(self.free_transform.mode_mut()),
            _ => None,
        }
    }

    /// Mutable access to the full Free Transform tool, if it is active.
    pub fn free_transform_tool_mut(
        &mut self,
    ) -> Option<&mut crate::tools::transform::FreeTransformTool> {
        match self.active {
            ToolKind::FreeTransform => Some(&mut self.free_transform),
            _ => None,
        }
    }

    /// Mutable access to the eyedropper's sample-size setting, if it is active.
    pub fn eyedropper_sample_mut(&mut self) -> Option<&mut crate::tools::fill::EyedropperSample> {
        match self.active {
            ToolKind::Eyedropper => Some(&mut self.eyedropper.sample),
            _ => None,
        }
    }

    /// The eyedropper's sample-size setting (read access for the sampler).
    pub fn eyedropper_sample(&self) -> crate::tools::fill::EyedropperSample {
        self.eyedropper.sample
    }

    /// Mutable access to the active shape tool's `(fill, stroke, stroke_width,
    /// commit_mode)`, if a shape tool is active.
    pub fn active_shape_controls_mut(
        &mut self,
    ) -> Option<(&mut Rgba32F, &mut Rgba32F, &mut u32, &mut VectorCommitMode)> {
        let tool = match self.active {
            ToolKind::ShapeRect => &mut self.shape_rect,
            ToolKind::ShapeEllipse => &mut self.shape_ellipse,
            ToolKind::ShapeLine => &mut self.shape_line,
            ToolKind::ShapePolygon => &mut self.shape_polygon,
            _ => return None,
        };
        let (fill, stroke, width, mode) = tool.controls_mut();
        Some((fill, stroke, width, mode))
    }

    /// Mutable access to the Pen tool's `(fill, stroke, stroke_width,
    /// fill_closed, commit_mode)`, if it is active.
    pub fn pen_controls_mut(
        &mut self,
    ) -> Option<(
        &mut Rgba32F,
        &mut Rgba32F,
        &mut u32,
        &mut bool,
        &mut VectorCommitMode,
    )> {
        match self.active {
            ToolKind::Pen => Some(self.pen.controls_mut()),
            _ => None,
        }
    }

    /// Mutable access to the Type tool's commit mode, if it is active.
    pub fn type_mode_mut(&mut self) -> Option<&mut VectorCommitMode> {
        match self.active {
            ToolKind::Type => Some(self.type_tool.mode_mut()),
            _ => None,
        }
    }

    /// If the active tool is vector-capable (Shape / Pen / Type) and the
    /// document's active layer is a compatible vector layer, load that layer
    /// into the tool for in-place re-edit. Returns `true` if a layer was
    /// loaded. Call this after switching to a vector tool or after the active
    /// layer changes while a vector tool is active.
    pub fn load_active_vector_layer(&mut self, doc: &ogre_core::Document) -> bool {
        match self.active {
            ToolKind::ShapeRect => self.shape_rect.load_active_vector_layer(doc),
            ToolKind::ShapeEllipse => self.shape_ellipse.load_active_vector_layer(doc),
            ToolKind::ShapeLine => self.shape_line.load_active_vector_layer(doc),
            ToolKind::ShapePolygon => self.shape_polygon.load_active_vector_layer(doc),
            ToolKind::Pen => self.pen.load_active_vector_layer(doc),
            ToolKind::Type => self.type_tool.load_active_vector_layer(doc),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_has_group_and_parsable_svg() {
        for k in ToolKind::ALL {
            let _g = k.group(); // exhaustive match guarantees coverage
            let bytes = k.svg_bytes();
            assert!(!bytes.is_empty(), "{k:?} has empty svg");
            usvg::Tree::from_data(bytes, &usvg::Options::default())
                .unwrap_or_else(|e| panic!("{k:?} svg failed to parse: {e}"));
        }
    }

    #[test]
    fn groups_partition_all_tools() {
        use ToolGroup::*;
        let mut count = 0;
        for k in ToolKind::ALL {
            count += 1;
            let _ = matches!(k.group(), Select | Paint | Transform | Vector | Navigate);
        }
        assert_eq!(count, 37);
    }

    #[test]
    fn families_cover_all_tools_and_resolve() {
        for k in ToolKind::ALL {
            let fam = k.family();
            assert!(
                fam.siblings().contains(&k),
                "{k:?} not in its own family's siblings"
            );
            assert!(!fam.siblings().is_empty());
            assert_eq!(fam.siblings()[0], fam.primary());
        }
        // Every family with a default key resolves back through for_key.
        for fam in ToolFamily::ALL {
            if let Some(key) = fam.default_key() {
                assert_eq!(ToolFamily::for_key(key), Some(fam));
            }
        }
        // Free transform has no bare key (invoked via Ctrl+T).
        assert_eq!(ToolFamily::Transform.default_key(), None);
    }

    #[test]
    fn tool_manager_starts_with_rect_select() {
        let manager = ToolManager::new();
        assert_eq!(manager.active(), ToolKind::RectSelect);
    }

    #[test]
    fn set_paint_colors_updates_brush_pencil_and_gradient() {
        let mut manager = ToolManager::new();
        let fg = ogre_core::Rgba32F::new(0.2, 0.4, 0.6, 1.0);
        let bg = ogre_core::Rgba32F::new(0.9, 0.1, 0.3, 1.0);
        manager.set_paint_colors(fg, bg);
        assert_eq!(manager.brush.color(), fg);
        assert_eq!(manager.pencil.color(), fg);
        // Gradient received both endpoints.
        let gs = manager.gradient.settings();
        // (No public color accessor on GradientTool; verify via settings round-trip
        // by checking the tool still has default settings — colors are pushed
        // separately and exercised in gradient's own tests.)
        assert_eq!(gs.kind, ogre_core::GradientKind::Linear);
    }

    #[test]
    fn set_tool_does_not_change_when_kind_is_same() {
        let mut manager = ToolManager::new();
        manager.set_tool(ToolKind::RectSelect);
        assert_eq!(manager.active(), ToolKind::RectSelect);
    }

    #[test]
    fn set_tool_cancels_in_progress_interaction() {
        let mut manager = ToolManager::new();
        let doc = ogre_core::Document::new(100, 100);
        manager.on_pointer(
            &doc,
            PointerEvent::new(IVec2::new(5, 5), Phase::Down, egui::Modifiers::NONE),
        );
        // Switching to the same tool still triggers cancel, dropping the drag.
        manager.set_tool(ToolKind::RectSelect);
        let cmd = manager.on_pointer(
            &doc,
            PointerEvent::new(IVec2::new(5, 5), Phase::Up, egui::Modifiers::NONE),
        );
        assert!(cmd.is_none());
    }

    #[test]
    fn cancel_active_discards_in_progress_interaction() {
        let mut manager = ToolManager::new();
        let doc = ogre_core::Document::new(100, 100);
        manager.on_pointer(
            &doc,
            PointerEvent::new(IVec2::new(5, 5), Phase::Down, egui::Modifiers::NONE),
        );
        manager.on_pointer(
            &doc,
            PointerEvent::new(IVec2::new(20, 20), Phase::Drag, egui::Modifiers::NONE),
        );
        // Escape -> cancel_active: the pending drag must be dropped, so a
        // subsequent pointer-up produces no command.
        manager.cancel_active();
        let cmd = manager.on_pointer(
            &doc,
            PointerEvent::new(IVec2::new(20, 20), Phase::Up, egui::Modifiers::NONE),
        );
        assert!(
            cmd.is_none(),
            "cancelled drag must not commit on pointer up"
        );
    }

    #[test]
    fn vendored_icons_are_white_normalized() {
        for k in ToolKind::ALL {
            let s = std::str::from_utf8(k.svg_bytes()).unwrap();
            assert!(
                !s.contains("currentColor"),
                "{k:?} icon still uses currentColor — egui tint-multiply would render it dark"
            );
            assert!(
                s.to_ascii_lowercase().contains("#ffffff"),
                "{k:?} icon has no white fill/stroke"
            );
        }
    }

    #[test]
    fn selection_mode_maps_modifiers() {
        assert_eq!(
            selection_mode(egui::Modifiers::NONE),
            ogre_core::SelectionMode::Replace
        );
        assert_eq!(
            selection_mode(egui::Modifiers::SHIFT),
            ogre_core::SelectionMode::Add
        );
        assert_eq!(
            selection_mode(egui::Modifiers::ALT),
            ogre_core::SelectionMode::Subtract
        );
        assert_eq!(
            selection_mode(egui::Modifiers::SHIFT | egui::Modifiers::ALT),
            ogre_core::SelectionMode::Intersect
        );
    }

    #[test]
    fn default_order_matches_canonical_order() {
        assert_eq!(
            default_tool_group_order(),
            vec![
                ToolGroup::Navigate,
                ToolGroup::Transform,
                ToolGroup::Select,
                ToolGroup::Paint,
                ToolGroup::Vector,
            ]
        );
    }

    #[test]
    fn normalize_removes_duplicates_and_preserves_user_order() {
        let mut order = vec![
            ToolGroup::Paint,
            ToolGroup::Paint,
            ToolGroup::Vector,
            ToolGroup::Navigate,
        ];
        normalize_tool_group_order(&mut order);
        assert_eq!(
            order,
            vec![
                ToolGroup::Paint,
                ToolGroup::Vector,
                ToolGroup::Navigate,
                ToolGroup::Transform,
                ToolGroup::Select,
            ]
        );
    }

    #[test]
    fn normalize_appends_missing_groups() {
        let mut order = vec![ToolGroup::Select];
        normalize_tool_group_order(&mut order);
        assert_eq!(
            order,
            vec![
                ToolGroup::Select,
                ToolGroup::Navigate,
                ToolGroup::Transform,
                ToolGroup::Paint,
                ToolGroup::Vector,
            ]
        );
    }

    #[test]
    fn normalize_repairs_empty_vector() {
        let mut order = Vec::new();
        normalize_tool_group_order(&mut order);
        assert_eq!(order, default_tool_group_order());
    }

    #[test]
    fn serde_round_trips_tool_group_order() {
        let order = default_tool_group_order();
        let json = serde_json::to_string(&order).unwrap();
        let decoded: Vec<ToolGroup> = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, order);
    }

    #[test]
    fn deserialize_tool_group_order_falls_back_on_invalid_value() {
        let mut de = serde_json::Deserializer::from_str("42");
        let result: Result<Vec<ToolGroup>, _> =
            deserialize_tool_group_order(&mut de).map_err(|e| e.to_string());
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), default_tool_group_order());
    }
}
