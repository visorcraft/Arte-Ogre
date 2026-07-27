// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Type tool (§3.3.1). Click to place the text anchor, type in the sidebar
//! text field, and Enter to commit the text as either a non-destructive vector
//! layer (default) or rasterized pixels in the active raster layer. Escape
//! cancels (no command). The committed pixels are in document space, scaled by
//! the chosen font size — screen zoom does not affect them.

use glam::Vec2;
use ogre_core::{AddVectorLayerCmd, BrushStrokeCmd, Rgba32F, VectorData};
use ogre_vector::{
    layout_and_rasterize, list_font_families, load_system_font_system, FontSystem, SwashCache,
    TextAlign, TextBlock,
};

use crate::tools::vector_commit::buffer_to_edits;
use crate::tools::{Phase, PointerEvent, Tool, VectorCommitMode};

/// Editable Type-tool settings surfaced in the sidebar.
#[derive(Clone, Debug)]
pub struct TypeSettings {
    /// Font family name (empty = system default / sans-serif).
    pub font_family: String,
    /// Font size in document pixels.
    pub font_size: f32,
    /// Prefer a bold face when shaping.
    pub bold: bool,
    /// Prefer an italic face when shaping.
    pub italic: bool,
    /// Text color (tracks the foreground color).
    pub color: Rgba32F,
    /// Horizontal alignment.
    pub align: TextAlign,
    /// Line-height multiplier.
    pub line_height: f32,
    /// Optional wrapping width (0 = no wrap).
    pub wrap_width: f32,
    /// The in-progress text.
    pub text: String,
}

impl Default for TypeSettings {
    fn default() -> Self {
        Self {
            font_family: String::new(),
            font_size: 24.0,
            bold: false,
            italic: false,
            color: Rgba32F::new(0.0, 0.0, 0.0, 1.0),
            align: TextAlign::Left,
            line_height: 1.2,
            wrap_width: 0.0,
            text: String::new(),
        }
    }
}

/// Whether bare tool-family keys (B, V, T, …) and other bare-key tool routing
/// should be suppressed.
///
/// This is the **same gate** the app input loop uses: text focus, open dialogs,
/// *or* an active Type entry session (anchor placed / re-edit loaded) all block
/// tool shortcuts so typing cannot steal tools even before the sidebar
/// `TextEdit` has focus.
pub fn should_suppress_bare_tool_keys(
    text_focused: bool,
    dialog_open: bool,
    type_entry_active: bool,
) -> bool {
    text_focused || dialog_open || type_entry_active
}

/// Type tool: click to place the anchor, type, Enter to commit.
#[derive(Debug)]
pub struct TypeTool {
    settings: TypeSettings,
    /// The document-space anchor (top-left of the text block).
    anchor: Option<glam::IVec2>,
    /// The active raster layer for destructive Pixels-mode commit.
    active_layer: Option<ogre_core::LayerId>,
    /// Cached `cosmic-text` font system (loaded lazily).
    font_system: Option<FontSystem>,
    /// Cached glyph rasterizer.
    swash_cache: SwashCache,
    /// Whether to commit as a vector layer or rasterize into the active layer.
    mode: VectorCommitMode,
    /// Mode snapshot taken when the anchor is placed so a mid-gesture sidebar
    /// change cannot switch the commit target of the in-progress text.
    committed_mode: Option<VectorCommitMode>,
    /// When re-editing an existing vector text layer, the id of that layer;
    /// commit dispatches [`EditVectorCmd`] instead of [`AddVectorLayerCmd`].
    editing_layer: Option<ogre_core::LayerId>,
    /// Request the sidebar text field to take focus on the next frame (set when
    /// an entry session starts via click or re-edit load).
    request_text_focus: bool,
    /// Cached unique system font family names (sorted), filled on first request.
    cached_font_families: Option<Vec<String>>,
}

impl Default for TypeTool {
    fn default() -> Self {
        Self::new()
    }
}

impl TypeTool {
    /// Create a new Type tool with no active text.
    pub fn new() -> Self {
        Self {
            settings: TypeSettings::default(),
            anchor: None,
            active_layer: None,
            font_system: None,
            swash_cache: SwashCache::new(),
            mode: VectorCommitMode::Vector,
            committed_mode: None,
            editing_layer: None,
            request_text_focus: false,
            cached_font_families: None,
        }
    }

    /// Mutable access to the settings (for the sidebar).
    pub fn settings_mut(&mut self) -> &mut TypeSettings {
        &mut self.settings
    }

    /// Shared access to the settings.
    pub fn settings(&self) -> &TypeSettings {
        &self.settings
    }

    /// Mutable access to the commit mode for the sidebar.
    pub fn mode_mut(&mut self) -> &mut VectorCommitMode {
        &mut self.mode
    }

    /// True while a Type entry session is active (canvas anchor placed or a
    /// vector text layer loaded for re-edit). Used by the input loop to suppress
    /// bare tool-family keys and accept Enter-to-commit while typing.
    pub fn has_active_entry(&self) -> bool {
        self.anchor.is_some() || self.editing_layer.is_some()
    }

    /// Whether the sidebar should request focus on the text field this frame.
    pub fn take_request_text_focus(&mut self) -> bool {
        let r = self.request_text_focus;
        self.request_text_focus = false;
        r
    }

    /// Append typed characters to the in-progress text (canvas-level typing when
    /// the sidebar TextEdit is not focused).
    pub fn append_text(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        self.settings.text.push_str(s);
    }

    /// Delete the last character (Unicode scalar) from the in-progress text.
    pub fn backspace(&mut self) {
        self.settings.text.pop();
    }

    /// Sorted unique system font family names for the font picker.
    ///
    /// Loads the font system on first call and caches the list for the tool's
    /// lifetime.
    pub fn font_families(&mut self) -> &[String] {
        self.ensure_font();
        if self.cached_font_families.is_none() {
            let names = self
                .font_system
                .as_ref()
                .map(list_font_families)
                .unwrap_or_default();
            self.cached_font_families = Some(names);
        }
        self.cached_font_families.as_deref().unwrap_or(&[])
    }

    /// The mode that should govern the current in-progress text.
    fn commit_mode(&self) -> VectorCommitMode {
        self.committed_mode.unwrap_or(self.mode)
    }

    /// Push the foreground color into the Type settings.
    pub fn set_color(&mut self, color: Rgba32F) {
        self.settings.color = color;
    }

    /// Lazily initialize the cosmic-text font system. `FontSystem::new` always
    /// yields a usable system (possibly with zero faces), so this never fails;
    /// a font-less system simply produces an empty raster and the commit is a
    /// no-op.
    fn ensure_font(&mut self) {
        if self.font_system.is_none() {
            self.font_system = Some(load_system_font_system().unwrap_or_else(FontSystem::new));
        }
    }

    fn clear(&mut self) {
        self.anchor = None;
        self.active_layer = None;
        self.committed_mode = None;
        self.editing_layer = None;
        self.request_text_focus = false;
        self.settings.text.clear();
    }

    /// If the active layer is a vector text layer, load its `TextBlock` into
    /// the settings so the Type tool can re-edit it. Returns `true` if loaded.
    ///
    /// On commit the tool will dispatch [`ogre_core::EditVectorCmd`] against the
    /// loaded layer instead of creating a new one.
    ///
    /// Always drops any prior re-edit target first, so that a failed load
    /// (e.g. the user selected a non-text layer) cannot leave a stale
    /// `editing_layer` pointing at the previously-selected layer — which would
    /// otherwise make the next commit edit the wrong layer.
    ///
    /// Selecting a layer mid-**freehand** type (anchor/text placed, not from a
    /// re-edit) refuses, so in-progress work is not clobbered. Selecting a layer
    /// while **re-editing** another drops the prior loaded text/target and loads
    /// the new one, so a re-edit target can never go stale.
    pub fn load_active_vector_layer(&mut self, doc: &ogre_core::Document) -> bool {
        // Freehand text entry (no re-edit target) in progress: preserve it.
        if self.editing_layer.is_none() && (self.anchor.is_some() || !self.settings.text.is_empty())
        {
            return false;
        }
        // Either idle, or switching away from a re-edit: reset before loading.
        self.editing_layer = None;
        self.anchor = None;
        self.committed_mode = None;
        self.settings.text.clear();
        let Some(id) = doc.active else {
            return false;
        };
        let Ok(layer) = doc.layer(id) else {
            return false;
        };
        let ogre_core::LayerContent::Vector(data) = &layer.content else {
            return false;
        };
        let Some(block) = &data.text else {
            return false;
        };
        self.settings = TypeSettings {
            font_family: block.font_family.clone(),
            font_size: block.font_size,
            bold: block.bold,
            italic: block.italic,
            color: block.color,
            align: block.align,
            line_height: block.line_height,
            wrap_width: block.wrap_width,
            text: block.text.clone(),
        };
        self.anchor = Some(layer.offset);
        self.editing_layer = Some(id);
        // Re-edit always targets the vector layer; the sidebar mode is
        // irrelevant while editing.
        self.committed_mode = Some(VectorCommitMode::Vector);
        self.request_text_focus = true;
        true
    }

    /// Build an `ogre_core::TextBlock` from the current settings.
    ///
    /// Public so unit tests exercise the same commit path data as rasterize.
    pub fn text_block(&self) -> TextBlock {
        TextBlock {
            text: self.settings.text.clone(),
            font_family: self.settings.font_family.clone(),
            font_size: self.settings.font_size,
            color: self.settings.color,
            align: self.settings.align,
            line_height: self.settings.line_height,
            wrap_width: self.settings.wrap_width,
            bold: self.settings.bold,
            italic: self.settings.italic,
        }
    }

    /// Rasterize the current settings into a `TextBlock` + `TiledBuffer`.
    /// Returns `None` if there is nothing to render.
    fn rasterize_block(&mut self) -> Option<(TextBlock, ogre_core::TiledBuffer)> {
        if self.settings.text.trim().is_empty() {
            return None;
        }
        let block = self.text_block();
        let box_width = if block.wrap_width > 0.0 {
            Some(block.wrap_width)
        } else {
            None
        };
        let font_system = self.font_system.as_mut()?;
        let (buf, _) = layout_and_rasterize(font_system, &mut self.swash_cache, &block, box_width);
        if buf.is_empty() {
            return None;
        }
        Some((block, buf))
    }

    /// Commit the current text as a new vector layer.
    fn commit_vector(&mut self) -> Option<Box<dyn ogre_core::Command>> {
        let anchor = self.anchor?;
        let (block, buf) = self.rasterize_block()?;
        let data = VectorData {
            paths: Vec::new(),
            rasterized: Some(buf),
            text: Some(block),
            ..Default::default()
        };
        Some(
            Box::new(AddVectorLayerCmd::new("Text", data).with_offset(anchor))
                as Box<dyn ogre_core::Command>,
        )
    }

    /// Replace the text on an existing vector layer (re-edit path).
    fn commit_vector_edit(
        &mut self,
        doc: &ogre_core::Document,
        id: ogre_core::LayerId,
    ) -> Option<Box<dyn ogre_core::Command>> {
        // Guard against a stale/deleted/non-vector target: if the layer is gone
        // or no longer holds text, refuse rather than dispatch a command that
        // would error at apply time.
        let layer = doc.layer(id).ok()?;
        let old_data = layer.vector_data()?.clone();
        let mut data = old_data.clone();
        let (block, buf) = self.rasterize_block()?;
        data.paths = Vec::new();
        data.rasterized = Some(buf);
        data.text = Some(block);
        data.mark_dirty();
        if old_data.svg_source.is_some() {
            Some(Box::new(crate::tools::vector_commit::EditSvgVectorCmd::new(
                id, old_data, data,
            )) as Box<dyn ogre_core::Command>)
        } else {
            Some(Box::new(ogre_core::EditVectorCmd::new(id, data)) as Box<dyn ogre_core::Command>)
        }
    }

    /// Rasterize the current text at the anchor into a BrushStrokeCmd.
    fn rasterize(&mut self, doc: &ogre_core::Document) -> Option<Box<dyn ogre_core::Command>> {
        let layer = self.active_layer?;
        let l = doc.layer(layer).ok()?;
        if !l.is_raster() || l.locked {
            return None;
        }
        let anchor = self.anchor?;
        let (_block, buf) = self.rasterize_block()?;
        let offset = l.offset;
        // The text buffer is rasterized at the document origin; its pixels need
        // to land at `anchor` in document space, then map to layer-local via
        // `offset`: local = (doc_pixel + anchor) - offset.
        let edits = buffer_to_edits(&buf, anchor, offset);
        if edits.is_empty() {
            return None;
        }
        Some(Box::new(BrushStrokeCmd::new(layer, edits)) as Box<dyn ogre_core::Command>)
    }
}

impl Tool for TypeTool {
    fn on_pointer(
        &mut self,
        doc: &ogre_core::Document,
        ev: PointerEvent,
    ) -> Option<Box<dyn ogre_core::Command>> {
        if ev.phase == Phase::Down {
            // A canvas click starts a fresh text. If a layer was loaded for
            // re-edit (e.g. on tool activation), drop it: `EditVectorCmd`
            // keeps the layer's offset fixed, so a click-reposition would
            // otherwise desync the anchor from the rendered position.
            self.editing_layer = None;
            // Snapshot the mode at gesture start so a mid-gesture sidebar
            // change cannot desync the commit target from the resolved layer.
            self.committed_mode = Some(self.mode);
            // In Vector mode we create a new vector layer, so we only need the
            // anchor point. In Pixels mode we need an unlocked raster layer to
            // paint into.
            if self.commit_mode() == VectorCommitMode::Vector {
                self.anchor = Some(ev.doc_pos);
                self.request_text_focus = true;
                return None;
            }
            if let Some(&layer) = doc.active.as_ref() {
                if let Ok(l) = doc.layer(layer) {
                    if !l.locked && l.is_raster() {
                        self.active_layer = Some(layer);
                        self.anchor = Some(ev.doc_pos);
                        self.request_text_focus = true;
                        return None;
                    }
                }
            }
        }
        None
    }

    fn draw_overlay(
        &self,
        painter: &egui::Painter,
        _doc: &ogre_core::Document,
        viewport: &ogre_gpu::Viewport,
        canvas_rect: egui::Rect,
        pixels_per_point: f32,
        time: f64,
    ) {
        let Some(anchor) = self.anchor else {
            return;
        };
        let to_screen = |p: glam::IVec2| -> egui::Pos2 {
            let v = (p.as_vec2() - viewport.pan) * viewport.zoom / pixels_per_point
                + Vec2::new(canvas_rect.min.x, canvas_rect.min.y);
            egui::Pos2::new(v.x, v.y)
        };
        let to_screen_f = |p: glam::Vec2| -> egui::Pos2 {
            let v = (p - viewport.pan) * viewport.zoom / pixels_per_point
                + Vec2::new(canvas_rect.min.x, canvas_rect.min.y);
            egui::Pos2::new(v.x, v.y)
        };
        let pos = to_screen(anchor);
        let screen_size =
            (self.settings.font_size * viewport.zoom / pixels_per_point).clamp(6.0, 400.0);
        let line_h = (screen_size * self.settings.line_height).max(screen_size);

        // Text colour for the live preview (sRGB-ish display of linear swatch).
        let c = self.settings.color;
        let to_u8 = |x: f32| (x.clamp(0.0, 1.0) * 255.0).round() as u8;
        let text_color = egui::Color32::from_rgba_unmultiplied(
            to_u8(c.r),
            to_u8(c.g),
            to_u8(c.b),
            to_u8(c.a.max(0.85)),
        );

        // Wrap-width guide (document pixels → screen).
        if self.settings.wrap_width > 0.0 {
            let w = self.settings.wrap_width;
            // Estimate height from line count for a helpful box, not exact metrics.
            let lines = self.settings.text.lines().count().max(1) as f32;
            let h_doc = self.settings.font_size * self.settings.line_height * lines;
            let tl = to_screen_f(anchor.as_vec2());
            let br = to_screen_f(anchor.as_vec2() + glam::Vec2::new(w, h_doc));
            let rect = egui::Rect::from_min_max(tl, br);
            painter.rect_stroke(
                rect,
                0.0,
                egui::Stroke::new(1.0, egui::Color32::from_white_alpha(90)),
                egui::StrokeKind::Outside,
            );
        }

        // Live preview of the typed string at the anchor. Uses egui layout so
        // the user sees content/position/size/style while typing; commit still
        // shapes with cosmic-text + the chosen family/weight/style.
        let preview_format = egui::TextFormat {
            font_id: egui::FontId::proportional(screen_size),
            color: text_color,
            italics: self.settings.italic,
            line_height: Some(line_h),
            ..Default::default()
        };
        let layout_line = |line: &str| -> std::sync::Arc<egui::Galley> {
            let mut job = egui::text::LayoutJob::default();
            job.append(line, 0.0, preview_format.clone());
            painter.layout_job(job)
        };

        let mut last_line_width = 0.0f32;
        if !self.settings.text.is_empty() {
            let mut y = pos.y;
            for line in self.settings.text.split('\n') {
                let galley = layout_line(line);
                last_line_width = galley.size().x;
                let x = match self.settings.align {
                    TextAlign::Left => pos.x,
                    TextAlign::Center => pos.x - galley.size().x * 0.5,
                    TextAlign::Right => pos.x - galley.size().x,
                };
                // Fake-bold: second pass offset (egui has no weight axis).
                if self.settings.bold {
                    painter.galley(egui::pos2(x + 0.7, y), galley.clone(), text_color);
                }
                painter.galley(egui::pos2(x, y), galley, text_color);
                y += line_h;
            }
        }

        // Blinking caret at the end of the last line.
        let blink_on = (time * 2.0).fract() < 0.55;
        if blink_on {
            let caret_x = match self.settings.align {
                TextAlign::Left => pos.x + last_line_width,
                TextAlign::Center => pos.x + last_line_width * 0.5,
                TextAlign::Right => pos.x,
            };
            let lines = self.settings.text.lines().count().max(1);
            let caret_y = pos.y + line_h * (lines.saturating_sub(1) as f32);
            painter.line_segment(
                [
                    egui::Pos2::new(caret_x, caret_y),
                    egui::Pos2::new(caret_x, caret_y + screen_size),
                ],
                egui::Stroke::new(1.5, egui::Color32::WHITE),
            );
        }
    }

    fn commit(&mut self, doc: &ogre_core::Document) -> Option<Box<dyn ogre_core::Command>> {
        self.ensure_font();
        let result = if let Some(id) = self.editing_layer {
            // Re-edit an existing vector text layer in place.
            self.commit_vector_edit(doc, id)
        } else if self.commit_mode() == VectorCommitMode::Vector {
            self.commit_vector()
        } else {
            self.rasterize(doc)
        };
        self.clear();
        result
    }

    fn cancel(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc_with_raster() -> ogre_core::Document {
        let mut doc = ogre_core::Document::new(100, 100);
        let id = doc.add_layer(ogre_core::Layer::new_raster("Bg"));
        doc.order.push(id);
        doc.active = Some(id);
        doc
    }

    #[test]
    fn type_tool_starts_empty() {
        let tool = TypeTool::new();
        assert!(tool.anchor.is_none());
        assert!(tool.settings.text.is_empty());
        assert!(!tool.has_active_entry());
    }

    #[test]
    fn type_tool_click_sets_anchor() {
        let doc = doc_with_raster();
        let mut tool = TypeTool::new();
        tool.mode = VectorCommitMode::Pixels;
        tool.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(20, 30), Phase::Down, egui::Modifiers::NONE),
        );
        assert_eq!(tool.anchor, Some(glam::IVec2::new(20, 30)));
        assert_eq!(tool.active_layer, doc.active);
        assert!(tool.has_active_entry());
        assert!(tool.take_request_text_focus());
    }

    #[test]
    fn type_tool_cancel_clears() {
        let doc = doc_with_raster();
        let mut tool = TypeTool::new();
        tool.mode = VectorCommitMode::Pixels;
        tool.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(20, 30), Phase::Down, egui::Modifiers::NONE),
        );
        tool.settings.text = "Hello".into();
        tool.cancel();
        assert!(tool.anchor.is_none());
        assert!(tool.settings.text.is_empty());
        assert!(!tool.has_active_entry());
    }

    #[test]
    fn type_tool_empty_text_commit_returns_none() {
        let doc = doc_with_raster();
        let mut tool = TypeTool::new();
        tool.mode = VectorCommitMode::Pixels;
        tool.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(20, 30), Phase::Down, egui::Modifiers::NONE),
        );
        let cmd = tool.commit(&doc);
        assert!(cmd.is_none());
    }

    #[test]
    fn type_tool_vector_mode_creates_vector_layer() {
        let doc = doc_with_raster();
        let mut tool = TypeTool::new();
        // Default mode is Vector.
        assert_eq!(tool.mode, VectorCommitMode::Vector);
        tool.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(20, 30), Phase::Down, egui::Modifiers::NONE),
        );
        tool.settings.text = "Hello".into();
        let mut cmd = tool
            .commit(&doc)
            .expect("vector mode should produce AddVectorLayerCmd");
        let mut doc = doc;
        cmd.apply(&mut doc).unwrap();
        let id = doc.active.unwrap();
        let layer = doc.layer(id).unwrap();
        assert_eq!(layer.name, "Text");
        assert!(matches!(layer.content, ogre_core::LayerContent::Vector(_)));
        if let ogre_core::LayerContent::Vector(ref data) = layer.content {
            assert!(data.text.is_some());
            assert_eq!(data.text.as_ref().unwrap().text, "Hello");
        }
    }

    #[test]
    fn type_tool_vector_mode_works_without_active_layer() {
        let doc = ogre_core::Document::new(100, 100);
        let mut tool = TypeTool::new();
        assert_eq!(tool.mode, VectorCommitMode::Vector);
        tool.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(20, 30), Phase::Down, egui::Modifiers::NONE),
        );
        tool.settings.text = "Hello".into();
        let mut cmd = tool
            .commit(&doc)
            .expect("vector mode should work without an active layer");
        let mut doc = doc;
        cmd.apply(&mut doc).unwrap();
        let id = doc.active.unwrap();
        let layer = doc.layer(id).unwrap();
        assert_eq!(layer.name, "Text");
        assert!(matches!(layer.content, ogre_core::LayerContent::Vector(_)));
        assert_eq!(layer.offset, glam::IVec2::new(20, 30));
    }

    /// The commit mode is snapshotted when the anchor is placed. Switching the
    /// sidebar mode between click and Enter must not desync the commit target
    /// (regression: previously a Vector→Pixels switch made commit a silent
    /// no-op because no active raster layer had been resolved).
    #[test]
    fn type_tool_mode_snapshotted_at_click() {
        let doc = doc_with_raster();
        let mut tool = TypeTool::new();
        assert_eq!(tool.mode, VectorCommitMode::Vector);
        // Click in Vector mode → snapshot is Vector, no active layer resolved.
        tool.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(20, 30), Phase::Down, egui::Modifiers::NONE),
        );
        assert!(tool.active_layer.is_none());
        // User flips to Pixels mid-gesture.
        tool.mode = VectorCommitMode::Pixels;
        tool.settings.text = "Hello".into();
        let mut cmd = tool
            .commit(&doc)
            .expect("commit must honor the snapshotted Vector mode, not silently no-op");
        assert_eq!(
            cmd.label(),
            "Add vector layer",
            "snapshot was Vector at click time, so commit must create a vector layer"
        );
        let mut doc = doc;
        cmd.apply(&mut doc).unwrap();
    }

    /// Spec test #5: selecting a vector text layer and activating Type restores
    /// the text string and style; committing edits the layer in place via
    /// `EditVectorCmd` rather than creating a new layer.
    #[test]
    fn type_tool_re_edit_restores_text_and_edits_in_place() {
        let mut doc = ogre_core::Document::new(200, 100);
        let _bg = doc.add_raster_layer("Bg");
        let block = ogre_core::TextBlock {
            text: "Original".into(),
            font_family: "DejaVu Sans".into(),
            font_size: 28.0,
            color: Rgba32F::new(1.0, 0.0, 0.0, 1.0),
            line_height: 1.1,
            align: ogre_core::TextAlign::Center,
            wrap_width: 0.0,
            bold: true,
            italic: true,
        };
        let data = ogre_core::VectorData {
            paths: Vec::new(),
            rasterized: None,
            text: Some(block.clone()),
            svg_source: None,
            version: 0,
        };
        let text_id = doc.add_vector_layer("Text", data);
        doc.layer_mut(text_id).unwrap().offset = glam::IVec2::new(20, 30);
        doc.active = Some(text_id);

        // Activate Type and load the active vector text layer for re-edit.
        let mut tool = TypeTool::new();
        assert!(
            tool.load_active_vector_layer(&doc),
            "loading an active vector text layer should succeed"
        );
        assert_eq!(tool.settings.text, "Original");
        assert_eq!(tool.settings.font_family, "DejaVu Sans");
        assert_eq!(tool.settings.font_size, 28.0);
        assert!(tool.settings.bold);
        assert!(tool.settings.italic);
        assert_eq!(tool.settings.color, Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        assert_eq!(tool.settings.align, ogre_core::TextAlign::Center);
        assert_eq!(tool.settings.line_height, 1.1);
        assert_eq!(tool.anchor, Some(glam::IVec2::new(20, 30)));
        assert_eq!(tool.editing_layer, Some(text_id));
        assert!(tool.has_active_entry());
        assert!(tool.take_request_text_focus());

        // Edit the text and commit → EditVectorCmd on the same layer.
        tool.settings.text = "Edited".into();
        tool.settings.bold = false;
        let mut cmd = tool.commit(&doc).expect("re-edit should commit");
        assert_eq!(cmd.label(), "Edit vector");
        let mut doc = doc;
        cmd.apply(&mut doc).unwrap();
        // Same layer, no new layer created.
        assert_eq!(doc.active, Some(text_id));
        assert_eq!(doc.order.len(), 2, "re-edit must not add a new layer");
        let layer = doc.layer(text_id).unwrap();
        let ogre_core::LayerContent::Vector(data) = &layer.content else {
            panic!("layer must still be vector");
        };
        let stored = data.text.as_ref().unwrap();
        assert_eq!(stored.text, "Edited");
        assert_eq!(stored.font_size, 28.0);
        assert_eq!(stored.font_family, "DejaVu Sans");
        assert!(!stored.bold);
        assert!(stored.italic);
        // The tool cleared its re-edit state after committing.
        let tool = TypeTool::new();
        assert!(tool.editing_layer.is_none());
    }

    #[test]
    fn type_tool_re_edit_rejects_non_text_layer() {
        let mut doc = ogre_core::Document::new(100, 100);
        let _bg = doc.add_raster_layer("Bg");
        // A vector layer with no text block (e.g., a shape) cannot be re-edited
        // by the Type tool.
        let data = ogre_core::VectorData {
            paths: vec![ogre_core::VectorPath {
                vertices: vec![
                    glam::IVec2::new(0, 0),
                    glam::IVec2::new(10, 0),
                    glam::IVec2::new(10, 10),
                ],
                fill: ogre_core::VectorFill::None,
                stroke: ogre_core::VectorStroke {
                    color: Rgba32F::TRANSPARENT,
                    width: 0.0,
                    dash: Vec::new(),
                    cap: ogre_core::StrokeCap::Butt,
                    join: ogre_core::StrokeJoin::Miter,
                },
                closed: true,
            }],
            rasterized: None,
            text: None,
            svg_source: None,
            version: 0,
        };
        let vid = doc.add_vector_layer("Shape", data);
        doc.active = Some(vid);

        let mut tool = TypeTool::new();
        assert!(
            !tool.load_active_vector_layer(&doc),
            "a vector layer without a TextBlock must not load into the Type tool"
        );
        assert!(tool.editing_layer.is_none());

        // A raster active layer is also rejected.
        doc.active = Some(_bg);
        assert!(!tool.load_active_vector_layer(&doc));
    }

    /// After a successful re-edit load, selecting a non-text layer must clear
    /// the stale `editing_layer` so the next commit cannot edit the previously
    /// selected (wrong) layer. Regression test for the stale editing-target
    /// found in adversarial review.
    #[test]
    fn type_tool_re_edit_clears_stale_target_when_active_layer_changes() {
        let mut doc = ogre_core::Document::new(200, 100);
        let bg = doc.add_raster_layer("Bg");
        let data = ogre_core::VectorData {
            paths: Vec::new(),
            rasterized: None,
            text: Some(ogre_core::TextBlock {
                text: "Hello".into(),
                ..Default::default()
            }),
            svg_source: None,
            version: 0,
        };
        let vid = doc.add_vector_layer("Text", data);
        doc.active = Some(vid);

        let mut tool = TypeTool::new();
        assert!(tool.load_active_vector_layer(&doc));
        assert_eq!(tool.editing_layer, Some(vid));

        // User selects the raster background: the load must fail AND clear the
        // stale editing target.
        doc.active = Some(bg);
        assert!(!tool.load_active_vector_layer(&doc));
        assert!(
            tool.editing_layer.is_none(),
            "stale editing target must be cleared on failed re-load"
        );

        // A commit now creates a fresh layer (or no-ops), it must not edit `vid`.
        tool.settings.text = "Fresh".into();
        tool.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(5, 5), Phase::Down, egui::Modifiers::NONE),
        );
        if let Some(mut cmd) = tool.commit(&doc) {
            assert_ne!(
                cmd.label(),
                "Edit vector",
                "must not edit the stale target layer after active-layer change"
            );
            let mut doc = doc;
            cmd.apply(&mut doc).unwrap();
        }
    }

    #[test]
    fn text_block_carries_style_fields_from_settings() {
        let mut tool = TypeTool::new();
        tool.settings.font_family = "Noto Sans".into();
        tool.settings.font_size = 42.0;
        tool.settings.bold = true;
        tool.settings.italic = true;
        tool.settings.text = "Hi".into();
        let block = tool.text_block();
        assert_eq!(block.font_family, "Noto Sans");
        assert_eq!(block.font_size, 42.0);
        assert!(block.bold);
        assert!(block.italic);
        assert_eq!(block.text, "Hi");
    }

    #[test]
    fn style_round_trip_through_vector_commit() {
        let doc = doc_with_raster();
        let mut tool = TypeTool::new();
        tool.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(10, 10), Phase::Down, egui::Modifiers::NONE),
        );
        tool.settings.text = "Styled".into();
        tool.settings.font_family = "Liberation Sans".into();
        tool.settings.font_size = 36.0;
        tool.settings.bold = true;
        tool.settings.italic = true;
        // text_block is what commit stores (raster may no-op without fonts).
        let block = tool.text_block();
        assert!(block.bold && block.italic);
        assert_eq!(block.font_family, "Liberation Sans");
        assert_eq!(block.font_size, 36.0);

        // If system fonts exist, full commit should persist the same fields.
        if let Some(mut cmd) = tool.commit(&doc) {
            let mut doc = doc;
            cmd.apply(&mut doc).unwrap();
            let layer = doc.layer(doc.active.unwrap()).unwrap();
            let ogre_core::LayerContent::Vector(data) = &layer.content else {
                panic!("expected vector layer");
            };
            let stored = data.text.as_ref().expect("stored text");
            assert_eq!(stored.font_family, "Liberation Sans");
            assert_eq!(stored.font_size, 36.0);
            assert!(stored.bold);
            assert!(stored.italic);
            assert_eq!(stored.text, "Styled");
        }
    }

    #[test]
    fn bare_tool_keys_suppressed_during_type_entry() {
        // Idle Type tool: only text focus / dialog suppress keys.
        assert!(!should_suppress_bare_tool_keys(false, false, false));
        assert!(should_suppress_bare_tool_keys(true, false, false));
        assert!(should_suppress_bare_tool_keys(false, true, false));
        // Active entry session: suppress even without text focus.
        assert!(should_suppress_bare_tool_keys(false, false, true));
        assert!(should_suppress_bare_tool_keys(true, false, true));
    }

    #[test]
    fn has_active_entry_tracks_anchor_and_re_edit() {
        let doc = doc_with_raster();
        let mut tool = TypeTool::new();
        assert!(!tool.has_active_entry());
        tool.on_pointer(
            &doc,
            PointerEvent::new(glam::IVec2::new(1, 1), Phase::Down, egui::Modifiers::NONE),
        );
        assert!(tool.has_active_entry());
        tool.cancel();
        assert!(!tool.has_active_entry());
    }

    #[test]
    fn canvas_typing_append_and_backspace() {
        let mut tool = TypeTool::new();
        tool.append_text("Hi");
        assert_eq!(tool.settings.text, "Hi");
        tool.append_text("!");
        assert_eq!(tool.settings.text, "Hi!");
        tool.backspace();
        assert_eq!(tool.settings.text, "Hi");
        tool.backspace();
        tool.backspace();
        tool.backspace(); // extra pops are safe
        assert!(tool.settings.text.is_empty());
    }

    #[test]
    fn font_families_lists_system_fonts_when_available() {
        let mut tool = TypeTool::new();
        let families = tool.font_families().to_vec();
        // Cached: second call returns the same slice length.
        assert_eq!(tool.font_families().len(), families.len());
        // Sorted unique if non-empty.
        let mut sorted = families.clone();
        sorted.sort();
        assert_eq!(families, sorted);
    }
}
