// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Additional undoable commands used by the Arte Ogre UI.
//!
//! These commands live alongside the commands in [`crate::history`] and follow
//! the same rules: they capture the information needed to undo their effect and
//! restore the prior state in [`Command::undo`].

use crate::coord::{IVec2, Rect};
use crate::document::Document;
use crate::error::{OgreError, Result};
use crate::history::Command;
use crate::layer::{BlendMode, LayerId};
use crate::ops;

use crate::pixel::clamp01_nan0 as sanitize_opacity;

/// A group of commands that are applied and undone as a single history entry.
///
/// This is used by scripts and macros that need to perform several document
/// edits while exposing only one undo/redo step to the user. If any command
/// in the batch fails, the commands that already succeeded are undone before
/// the error is returned, leaving the document in its original state.
pub struct BatchCmd {
    label: &'static str,
    commands: Vec<Box<dyn Command>>,
}

impl std::fmt::Debug for BatchCmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BatchCmd")
            .field("label", &self.label)
            .field("len", &self.commands.len())
            .finish()
    }
}

impl BatchCmd {
    /// Create an empty batch with the given undo/redo label.
    pub fn new(label: &'static str) -> Self {
        Self {
            label,
            commands: Vec::new(),
        }
    }

    /// Add a command to the end of the batch.
    pub fn push(&mut self, cmd: Box<dyn Command>) {
        self.commands.push(cmd);
    }
}

impl Command for BatchCmd {
    fn label(&self) -> &'static str {
        self.label
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        for (applied, cmd) in self.commands.iter_mut().enumerate() {
            if let Err(e) = cmd.apply(doc) {
                // Undo the commands that already succeeded so the document is
                // left in its original state.
                for prev in self.commands[..applied].iter_mut().rev() {
                    prev.undo(doc);
                }
                return Err(e);
            }
        }
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        for cmd in self.commands.iter_mut().rev() {
            cmd.undo(doc);
        }
    }

    fn cleanup(&self, doc: &mut Document) {
        for cmd in &self.commands {
            cmd.cleanup(doc);
        }
    }

    fn referenced_layers(&self) -> Vec<LayerId> {
        self.commands
            .iter()
            .flat_map(|cmd| cmd.referenced_layers())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect()
    }
}

/// Set whether a layer is visible.
#[derive(Debug)]
pub struct SetVisibleCmd {
    layer: LayerId,
    visible: bool,
    old: Option<bool>,
}

impl SetVisibleCmd {
    /// Create a visibility command.
    pub fn new(layer: LayerId, visible: bool) -> Self {
        Self {
            layer,
            visible,
            old: None,
        }
    }
}

impl Command for SetVisibleCmd {
    fn label(&self) -> &'static str {
        "Set visible"
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        let layer = doc.layer_mut(self.layer)?;
        self.old = Some(layer.visible);
        layer.visible = self.visible;
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let (Some(old), Ok(layer)) = (self.old, doc.layer_mut(self.layer)) {
            layer.visible = old;
        }
    }

    fn referenced_layers(&self) -> Vec<LayerId> {
        vec![self.layer]
    }
}

/// Set a layer's opacity.
#[derive(Debug)]
pub struct SetOpacityCmd {
    layer: LayerId,
    opacity: f32,
    old: Option<f32>,
}

impl SetOpacityCmd {
    /// Create an opacity command.
    pub fn new(layer: LayerId, opacity: f32) -> Self {
        Self {
            layer,
            opacity: sanitize_opacity(opacity),
            old: None,
        }
    }
}

impl Command for SetOpacityCmd {
    fn label(&self) -> &'static str {
        "Set opacity"
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        let layer = doc.layer_mut(self.layer)?;
        self.old = Some(layer.opacity);
        layer.opacity = self.opacity;
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let (Some(old), Ok(layer)) = (self.old, doc.layer_mut(self.layer)) {
            layer.opacity = old;
        }
    }

    fn coalesce_opacity(&mut self, doc: &mut Document, layer: LayerId, opacity: f32) -> bool {
        if self.layer != layer {
            return false;
        }
        self.opacity = sanitize_opacity(opacity);
        if let Ok(layer) = doc.layer_mut(self.layer) {
            layer.opacity = self.opacity;
        }
        true
    }

    fn referenced_layers(&self) -> Vec<LayerId> {
        vec![self.layer]
    }
}

/// Set a layer's blend mode.
#[derive(Debug)]
pub struct SetBlendCmd {
    layer: LayerId,
    blend: BlendMode,
    old: Option<BlendMode>,
}

impl SetBlendCmd {
    /// Create a blend-mode command.
    pub fn new(layer: LayerId, blend: BlendMode) -> Self {
        Self {
            layer,
            blend,
            old: None,
        }
    }
}

impl Command for SetBlendCmd {
    fn label(&self) -> &'static str {
        "Set blend"
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        let layer = doc.layer_mut(self.layer)?;
        self.old = Some(layer.blend);
        layer.blend = self.blend;
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let (Some(old), Ok(layer)) = (self.old, doc.layer_mut(self.layer)) {
            layer.blend = old;
        }
    }

    fn referenced_layers(&self) -> Vec<LayerId> {
        vec![self.layer]
    }
}

/// Rename a layer.
#[derive(Debug)]
pub struct RenameCmd {
    layer: LayerId,
    name: String,
    old: Option<String>,
}

impl RenameCmd {
    /// Create a rename command.
    pub fn new(layer: LayerId, name: impl Into<String>) -> Self {
        Self {
            layer,
            name: name.into(),
            old: None,
        }
    }
}

impl Command for RenameCmd {
    fn label(&self) -> &'static str {
        "Rename layer"
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        let layer = doc.layer_mut(self.layer)?;
        self.old = Some(layer.name.clone());
        layer.name = self.name.clone();
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let (Some(old), Ok(layer)) = (self.old.as_ref(), doc.layer_mut(self.layer)) {
            layer.name.clone_from(old);
        }
    }

    fn referenced_layers(&self) -> Vec<LayerId> {
        vec![self.layer]
    }
}

/// Add a new empty raster layer on top of the stack.
#[derive(Debug)]
pub struct AddRasterLayerCmd {
    name: String,
    new_id: Option<LayerId>,
    inserted_index: Option<usize>,
    prior_active: Option<LayerId>,
}

impl AddRasterLayerCmd {
    /// Create an add-raster-layer command.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            new_id: None,
            inserted_index: None,
            prior_active: None,
        }
    }
}

impl Command for AddRasterLayerCmd {
    fn label(&self) -> &'static str {
        "Add layer"
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        self.prior_active = doc.active;

        if let Some(id) = self.new_id {
            // Redo after undo: restore the soft-removed layer.
            if doc.removed.contains(&id) {
                let index = self.inserted_index.unwrap_or(doc.order.len());
                doc.restore_layer_soft(None, index, id, &[id]);
                doc.active = Some(id);
                return Ok(());
            }
        }

        // First application (or redo after the layer was hard-removed): create
        // a fresh layer.
        let index = doc.order.len();
        let id = doc.add_raster_layer(&self.name);
        self.new_id = Some(id);
        self.inserted_index = Some(index);
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let Some(id) = self.new_id {
            let _ = doc.remove_layer_soft(id);
        }
        doc.active = self.prior_active;
    }

    fn cleanup(&self, doc: &mut Document) {
        if let Some(id) = self.new_id {
            if doc.removed.contains(&id) {
                let _ = doc.remove_layer(id);
            }
        }
    }

    fn referenced_layers(&self) -> Vec<LayerId> {
        self.new_id.into_iter().collect()
    }
}

/// Add a non-destructive vector layer.
///
/// Mirrors [`AddRasterLayerCmd`]: the layer is soft-removed on undo and
/// restored on redo.
#[derive(Debug)]
pub struct AddVectorLayerCmd {
    name: String,
    data: crate::layer::VectorData,
    offset: Option<glam::IVec2>,
    new_id: Option<LayerId>,
    inserted_index: Option<usize>,
    prior_active: Option<LayerId>,
}

impl AddVectorLayerCmd {
    /// Create an add-vector-layer command.
    pub fn new(name: impl Into<String>, data: crate::layer::VectorData) -> Self {
        Self {
            name: name.into(),
            data,
            offset: None,
            new_id: None,
            inserted_index: None,
            prior_active: None,
        }
    }

    /// Set the offset for the new vector layer.
    pub fn with_offset(mut self, offset: glam::IVec2) -> Self {
        self.offset = Some(offset);
        self
    }
}

impl Command for AddVectorLayerCmd {
    fn label(&self) -> &'static str {
        "Add vector layer"
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        self.prior_active = doc.active;
        if let Some(id) = self.new_id {
            if doc.removed.contains(&id) {
                let index = self.inserted_index.unwrap_or(doc.order.len());
                doc.restore_layer_soft(None, index, id, &[id]);
                doc.active = Some(id);
                return Ok(());
            }
        }
        let index = doc.order.len();
        let id = doc.add_vector_layer(&self.name, self.data.clone());
        if let Some(offset) = self.offset {
            if let Ok(layer) = doc.layer_mut(id) {
                layer.offset = offset;
            }
        }
        self.new_id = Some(id);
        self.inserted_index = Some(index);
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let Some(id) = self.new_id {
            let _ = doc.remove_layer_soft(id);
        }
        doc.active = self.prior_active;
    }

    fn cleanup(&self, doc: &mut Document) {
        if let Some(id) = self.new_id {
            if doc.removed.contains(&id) {
                let _ = doc.remove_layer(id);
            }
        }
    }

    fn referenced_layers(&self) -> Vec<LayerId> {
        self.new_id.into_iter().collect()
    }
}

/// Replace a vector layer's data. Deep-clones for undo.
#[derive(Debug)]
pub struct EditVectorCmd {
    layer: LayerId,
    old_data: Option<crate::layer::VectorData>,
    new_data: crate::layer::VectorData,
}

impl EditVectorCmd {
    /// Create a command that replaces the vector data on `layer` with `new_data`.
    pub fn new(layer: LayerId, new_data: crate::layer::VectorData) -> Self {
        Self {
            layer,
            old_data: None,
            new_data,
        }
    }
}

impl Command for EditVectorCmd {
    fn label(&self) -> &'static str {
        "Edit vector"
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        let layer = doc.layer_mut(self.layer)?;
        if !layer.is_vector() {
            return Err(OgreError::InvalidOperation(
                "operation requires a vector layer",
            ));
        }
        self.old_data = layer.vector_data().cloned();
        if let Some(data) = layer.vector_data_mut() {
            *data = self.new_data.clone();
        }
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let Some(old) = &self.old_data {
            if let Ok(layer) = doc.layer_mut(self.layer) {
                if let Some(data) = layer.vector_data_mut() {
                    *data = old.clone();
                }
            }
        }
    }

    fn referenced_layers(&self) -> Vec<LayerId> {
        vec![self.layer]
    }
}

/// Add a raster layer containing pasted image pixels (top-left at the canvas origin).
///
/// On first application the layer is created and filled with the provided
/// pixels. Undo soft-removes the layer; redo restores it without re-writing
/// the pixel data (the buffer is preserved in the soft-removed layer).
#[derive(Debug)]
pub struct PasteImageLayerCmd {
    name: String,
    width: u32,
    height: u32,
    pixels: Vec<crate::pixel::Rgba32F>,
    new_id: Option<LayerId>,
    inserted_index: Option<usize>,
    prior_active: Option<LayerId>,
}

impl PasteImageLayerCmd {
    /// Create a paste-image-layer command. `pixels` is row-major RGBA,
    /// len = `width * height`.
    pub fn new(
        name: impl Into<String>,
        width: u32,
        height: u32,
        pixels: Vec<crate::pixel::Rgba32F>,
    ) -> Self {
        Self {
            name: name.into(),
            width,
            height,
            pixels,
            new_id: None,
            inserted_index: None,
            prior_active: None,
        }
    }
}

impl Command for PasteImageLayerCmd {
    fn label(&self) -> &'static str {
        "Paste image"
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        self.prior_active = doc.active;

        if let Some(id) = self.new_id {
            // Redo after undo: restore the soft-removed layer.
            if doc.removed.contains(&id) {
                let index = self.inserted_index.unwrap_or(doc.order.len());
                doc.restore_layer_soft(None, index, id, &[id]);
                doc.active = Some(id);
                return Ok(());
            }
        }

        // First application (or redo after hard-remove): create a fresh layer
        // and write the pasted pixels.
        let index = doc.order.len();
        let id = doc.add_raster_layer(&self.name);
        {
            let buffer = doc.layer_mut(id).unwrap().buffer_mut().unwrap();
            for y in 0..self.height {
                for x in 0..self.width {
                    let px = self.pixels[(y * self.width + x) as usize];
                    buffer.set_pixel(crate::coord::IVec2::new(x as i32, y as i32), px);
                }
            }
        }
        self.new_id = Some(id);
        self.inserted_index = Some(index);
        doc.active = Some(id);
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let Some(id) = self.new_id {
            let _ = doc.remove_layer_soft(id);
        }
        doc.active = self.prior_active;
    }

    fn cleanup(&self, doc: &mut Document) {
        if let Some(id) = self.new_id {
            if doc.removed.contains(&id) {
                let _ = doc.remove_layer(id);
            }
        }
    }

    fn referenced_layers(&self) -> Vec<LayerId> {
        self.new_id.into_iter().collect()
    }
}

/// Duplicate a raster layer, inserting the copy directly above the original.
#[derive(Debug)]
pub struct DuplicateLayerCmd {
    source: LayerId,
    new_id: Option<LayerId>,
    inserted_parent: Option<LayerId>,
    inserted_index: Option<usize>,
    prior_active: Option<LayerId>,
}

impl DuplicateLayerCmd {
    /// Create a duplicate-layer command.
    pub fn new(source: LayerId) -> Self {
        Self {
            source,
            new_id: None,
            inserted_parent: None,
            inserted_index: None,
            prior_active: None,
        }
    }
}

impl Command for DuplicateLayerCmd {
    fn label(&self) -> &'static str {
        "Duplicate layer"
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        self.prior_active = doc.active;

        if let Some(id) = self.new_id {
            // Redo after undo: restore the soft-removed duplicate.
            if doc.removed.contains(&id) {
                let parent = self.inserted_parent;
                let index = self.inserted_index.unwrap_or(doc.order.len());
                doc.restore_layer_soft(parent, index, id, &[id]);
                doc.active = Some(id);
                return Ok(());
            }
        }

        // First application (or redo after hard-remove): duplicate again.
        let (parent, index) = doc
            .sibling_index(self.source)
            .ok_or(OgreError::LayerNotFound(self.source))?;
        let id = ops::duplicate_layer(doc, self.source)?;
        self.new_id = Some(id);
        self.inserted_parent = parent;
        self.inserted_index = Some(index + 1);
        doc.active = Some(id);
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let Some(id) = self.new_id {
            let _ = doc.remove_layer_soft(id);
        }
        doc.active = self.prior_active;
    }

    fn cleanup(&self, doc: &mut Document) {
        if let Some(id) = self.new_id {
            if doc.removed.contains(&id) {
                let _ = doc.remove_layer(id);
            }
        }
    }

    fn referenced_layers(&self) -> Vec<LayerId> {
        self.new_id.into_iter().collect()
    }
}

/// Anchor point used when resizing the canvas.
///
/// The anchor determines which edges stay fixed; content is shifted by the
/// opposite edges.  For example, [`CanvasAnchor::TopLeft`] keeps the top-left
/// corner fixed and grows/shrinks toward the bottom-right.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CanvasAnchor {
    /// Keep the top-left corner fixed.
    TopLeft,
    /// Keep the top edge centered.
    TopCenter,
    /// Keep the top-right corner fixed.
    TopRight,
    /// Keep the left edge centered.
    CenterLeft,
    /// Keep the center fixed.
    Center,
    /// Keep the right edge centred.
    CenterRight,
    /// Keep the bottom-left corner fixed.
    BottomLeft,
    /// Keep the bottom edge centred.
    BottomCenter,
    /// Keep the bottom-right corner fixed.
    BottomRight,
}

impl CanvasAnchor {
    /// Fractional anchor position `(ax, ay)` where `0` is the left/top edge
    /// and `1` is the right/bottom edge.
    fn factors(self) -> (f32, f32) {
        match self {
            CanvasAnchor::TopLeft => (0.0, 0.0),
            CanvasAnchor::TopCenter => (0.5, 0.0),
            CanvasAnchor::TopRight => (1.0, 0.0),
            CanvasAnchor::CenterLeft => (0.0, 0.5),
            CanvasAnchor::Center => (0.5, 0.5),
            CanvasAnchor::CenterRight => (1.0, 0.5),
            CanvasAnchor::BottomLeft => (0.0, 1.0),
            CanvasAnchor::BottomCenter => (0.5, 1.0),
            CanvasAnchor::BottomRight => (1.0, 1.0),
        }
    }
}

/// Change the document canvas to `canvas` without destroying layer pixels.
///
/// Layers keep their existing offsets and buffers; only the exported viewport
/// changes.  Undo restores the previous canvas rectangle.
#[derive(Debug)]
pub struct CropCmd {
    canvas: Rect,
    old_canvas: Option<Rect>,
}

impl CropCmd {
    /// Create a crop command.
    pub fn new(canvas: Rect) -> Self {
        Self {
            canvas,
            old_canvas: None,
        }
    }
}

impl Command for CropCmd {
    fn label(&self) -> &'static str {
        "Crop canvas"
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        if self.canvas.is_empty() {
            return Err(OgreError::InvalidOperation("crop rectangle is empty"));
        }
        self.old_canvas = Some(doc.canvas);
        doc.canvas = self.canvas;
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let Some(old) = self.old_canvas.take() {
            doc.canvas = old;
        }
    }
}

/// Resize the document canvas, shifting layer content according to `anchor`.
///
/// Layer pixel data is preserved; only the canvas bounds and layer offsets are
/// changed.  Undo restores the previous canvas and the previous offsets of
/// every layer that existed when the command was applied.
#[derive(Debug)]
pub struct ResizeCanvasCmd {
    new_width: u32,
    new_height: u32,
    anchor: CanvasAnchor,
    old_canvas: Option<Rect>,
    old_offsets: Vec<(LayerId, IVec2)>,
}

impl ResizeCanvasCmd {
    /// Create a canvas-resize command.
    pub fn new(new_width: u32, new_height: u32, anchor: CanvasAnchor) -> Self {
        Self {
            new_width,
            new_height,
            anchor,
            old_canvas: None,
            old_offsets: Vec::new(),
        }
    }
}

impl Command for ResizeCanvasCmd {
    fn label(&self) -> &'static str {
        "Resize canvas"
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        if self.new_width == 0 || self.new_height == 0 {
            return Err(OgreError::InvalidOperation("canvas size must be non-zero"));
        }

        let old = doc.canvas;
        let (ax, ay) = self.anchor.factors();
        let dw = (self.new_width as i64) - (old.w as i64);
        let dh = (self.new_height as i64) - (old.h as i64);
        let dx = (ax * dw as f32).round() as i32;
        let dy = (ay * dh as f32).round() as i32;

        self.old_canvas = Some(old);
        self.old_offsets.clear();
        self.old_offsets.reserve(doc.layers.len());
        for (id, layer) in doc.layers.iter_mut() {
            self.old_offsets.push((id, layer.offset));
            let new_offset = IVec2::new(
                layer.offset.x.saturating_add(dx),
                layer.offset.y.saturating_add(dy),
            );
            layer.offset = new_offset;
        }

        doc.canvas = Rect::new(old.x, old.y, self.new_width, self.new_height);
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let Some(old) = self.old_canvas.take() {
            doc.canvas = old;
        }
        for (id, offset) in self.old_offsets.drain(..) {
            if let Ok(layer) = doc.layer_mut(id) {
                layer.offset = offset;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Document;

    #[test]
    fn set_visible_cmd_round_trips() {
        let mut doc = Document::new(100, 100);
        let id = doc.add_raster_layer("L");

        let mut cmd = SetVisibleCmd::new(id, false);
        cmd.apply(&mut doc).unwrap();
        assert!(!doc.layer(id).unwrap().visible);

        cmd.undo(&mut doc);
        assert!(doc.layer(id).unwrap().visible);
    }

    #[test]
    fn set_opacity_cmd_round_trips() {
        let mut doc = Document::new(100, 100);
        let id = doc.add_raster_layer("L");

        let mut cmd = SetOpacityCmd::new(id, 0.5);
        cmd.apply(&mut doc).unwrap();
        assert_eq!(doc.layer(id).unwrap().opacity, 0.5);

        cmd.undo(&mut doc);
        assert_eq!(doc.layer(id).unwrap().opacity, 1.0);
    }

    #[test]
    fn set_opacity_cmd_clamps_out_of_range() {
        let mut doc = Document::new(100, 100);
        let id = doc.add_raster_layer("L");

        let mut cmd = SetOpacityCmd::new(id, 1.5);
        cmd.apply(&mut doc).unwrap();
        assert_eq!(doc.layer(id).unwrap().opacity, 1.0);

        cmd.undo(&mut doc);
        assert_eq!(doc.layer(id).unwrap().opacity, 1.0);
    }

    #[test]
    fn set_opacity_cmd_sanitizes_nan() {
        let mut doc = Document::new(100, 100);
        let id = doc.add_raster_layer("L");

        let mut cmd = SetOpacityCmd::new(id, f32::NAN);
        cmd.apply(&mut doc).unwrap();
        assert_eq!(doc.layer(id).unwrap().opacity, 0.0);

        cmd.undo(&mut doc);
        assert_eq!(doc.layer(id).unwrap().opacity, 1.0);
    }

    #[test]
    fn set_blend_cmd_round_trips() {
        let mut doc = Document::new(100, 100);
        let id = doc.add_raster_layer("L");

        let mut cmd = SetBlendCmd::new(id, BlendMode::Multiply);
        cmd.apply(&mut doc).unwrap();
        assert_eq!(doc.layer(id).unwrap().blend, BlendMode::Multiply);

        cmd.undo(&mut doc);
        assert_eq!(doc.layer(id).unwrap().blend, BlendMode::Normal);
    }

    #[test]
    fn rename_cmd_round_trips() {
        let mut doc = Document::new(100, 100);
        let id = doc.add_raster_layer("L");

        let mut cmd = RenameCmd::new(id, "Renamed");
        cmd.apply(&mut doc).unwrap();
        assert_eq!(doc.layer(id).unwrap().name, "Renamed");

        cmd.undo(&mut doc);
        assert_eq!(doc.layer(id).unwrap().name, "L");
    }

    #[test]
    fn add_raster_layer_cmd_round_trips() {
        let mut doc = Document::new(100, 100);
        let bg = doc.add_raster_layer("Background");
        doc.active = Some(bg);

        let mut cmd = AddRasterLayerCmd::new("New");
        cmd.apply(&mut doc).unwrap();
        let new_id = doc.active.unwrap();
        assert_ne!(new_id, bg);
        assert_eq!(doc.layer(new_id).unwrap().name, "New");
        assert_eq!(doc.order, vec![bg, new_id]);

        cmd.undo(&mut doc);
        assert_eq!(doc.active, Some(bg));
        assert!(!doc.order.contains(&new_id));

        cmd.apply(&mut doc).unwrap();
        assert_eq!(doc.active, Some(new_id));
        assert!(doc.order.contains(&new_id));
    }

    #[test]
    fn duplicate_layer_cmd_round_trips() {
        let mut doc = Document::new(100, 100);
        let src = doc.add_raster_layer("Source");

        let mut cmd = DuplicateLayerCmd::new(src);
        cmd.apply(&mut doc).unwrap();
        let dup = doc.active.unwrap();
        assert_ne!(dup, src);
        assert_eq!(doc.layer(dup).unwrap().name, "Source copy");
        assert_eq!(doc.order, vec![src, dup]);

        cmd.undo(&mut doc);
        assert_eq!(doc.active, Some(src));
        assert!(!doc.order.contains(&dup));

        cmd.apply(&mut doc).unwrap();
        assert_eq!(doc.active, Some(dup));
        assert!(doc.order.contains(&dup));
    }

    #[test]
    fn crop_cmd_changes_canvas_and_preserves_layers() {
        let mut doc = Document::new(100, 100);
        let id = doc.add_raster_layer("L");
        doc.layer_mut(id).unwrap().offset = IVec2::new(10, 20);

        let mut cmd = CropCmd::new(Rect::new(25, 30, 50, 60));
        cmd.apply(&mut doc).unwrap();
        assert_eq!(doc.canvas, Rect::new(25, 30, 50, 60));
        assert_eq!(doc.layer(id).unwrap().offset, IVec2::new(10, 20));

        cmd.undo(&mut doc);
        assert_eq!(doc.canvas, Rect::new(0, 0, 100, 100));
        assert_eq!(doc.layer(id).unwrap().offset, IVec2::new(10, 20));
    }

    #[test]
    fn crop_cmd_rejects_empty_rectangle() {
        let mut doc = Document::new(100, 100);
        let mut cmd = CropCmd::new(Rect::new(10, 10, 0, 50));
        assert!(cmd.apply(&mut doc).is_err());
    }

    #[test]
    fn resize_canvas_cmd_shifts_layers_by_anchor() {
        let mut doc = Document::new(100, 100);
        let a = doc.add_raster_layer("A");
        let b = doc.add_raster_layer("B");
        doc.layer_mut(a).unwrap().offset = IVec2::new(0, 0);
        doc.layer_mut(b).unwrap().offset = IVec2::new(50, 50);

        let mut cmd = ResizeCanvasCmd::new(200, 150, CanvasAnchor::Center);
        cmd.apply(&mut doc).unwrap();
        assert_eq!(doc.canvas, Rect::new(0, 0, 200, 150));
        // Center anchor: content shifts by ((200-100)/2, (150-100)/2) = (50, 25).
        assert_eq!(doc.layer(a).unwrap().offset, IVec2::new(50, 25));
        assert_eq!(doc.layer(b).unwrap().offset, IVec2::new(100, 75));

        cmd.undo(&mut doc);
        assert_eq!(doc.canvas, Rect::new(0, 0, 100, 100));
        assert_eq!(doc.layer(a).unwrap().offset, IVec2::new(0, 0));
        assert_eq!(doc.layer(b).unwrap().offset, IVec2::new(50, 50));
    }

    #[test]
    fn resize_canvas_cmd_rejects_zero_dimension() {
        let mut doc = Document::new(100, 100);
        let mut cmd = ResizeCanvasCmd::new(0, 150, CanvasAnchor::TopLeft);
        assert!(cmd.apply(&mut doc).is_err());
    }

    #[test]
    fn resize_canvas_cmd_corner_anchor_keeps_corner_fixed() {
        let mut doc = Document::new(100, 100);
        let id = doc.add_raster_layer("L");
        doc.layer_mut(id).unwrap().offset = IVec2::new(5, 10);

        let mut cmd = ResizeCanvasCmd::new(120, 140, CanvasAnchor::TopLeft);
        cmd.apply(&mut doc).unwrap();
        // Top-left anchor: content does not shift.
        assert_eq!(doc.layer(id).unwrap().offset, IVec2::new(5, 10));
        assert_eq!(doc.canvas, Rect::new(0, 0, 120, 140));

        cmd.undo(&mut doc);
        assert_eq!(doc.layer(id).unwrap().offset, IVec2::new(5, 10));
        assert_eq!(doc.canvas, Rect::new(0, 0, 100, 100));
    }

    #[test]
    fn resize_canvas_cmd_shrinking_shifts_layers_correctly() {
        let mut doc = Document::new(100, 100);
        let id = doc.add_raster_layer("L");
        doc.layer_mut(id).unwrap().offset = IVec2::new(40, 40);

        // Shrink by 20 px on each axis with center anchor -> shift by (-10,-10).
        let mut cmd = ResizeCanvasCmd::new(80, 80, CanvasAnchor::Center);
        cmd.apply(&mut doc).unwrap();
        assert_eq!(doc.canvas, Rect::new(0, 0, 80, 80));
        assert_eq!(doc.layer(id).unwrap().offset, IVec2::new(30, 30));

        cmd.undo(&mut doc);
        assert_eq!(doc.canvas, Rect::new(0, 0, 100, 100));
        assert_eq!(doc.layer(id).unwrap().offset, IVec2::new(40, 40));
    }

    #[test]
    fn resize_canvas_cmd_preserves_origin_after_crop() {
        let mut doc = Document::new(100, 100);
        let id = doc.add_raster_layer("L");
        doc.layer_mut(id).unwrap().offset = IVec2::new(0, 0);

        // Crop to a non-zero origin.
        let mut crop = CropCmd::new(Rect::new(25, 30, 50, 60));
        crop.apply(&mut doc).unwrap();
        assert_eq!(doc.canvas, Rect::new(25, 30, 50, 60));

        // Resize back to 100x100 centered on the cropped canvas.
        let mut resize = ResizeCanvasCmd::new(100, 100, CanvasAnchor::Center);
        resize.apply(&mut doc).unwrap();
        // New canvas keeps the old origin (25,30).
        assert_eq!(doc.canvas, Rect::new(25, 30, 100, 100));
        // Width grew by 50, height by 40 -> center shift (25,20).
        assert_eq!(doc.layer(id).unwrap().offset, IVec2::new(25, 20));

        resize.undo(&mut doc);
        assert_eq!(doc.canvas, Rect::new(25, 30, 50, 60));
        assert_eq!(doc.layer(id).unwrap().offset, IVec2::new(0, 0));
    }

    #[test]
    fn resize_canvas_cmd_undo_does_not_touch_layers_added_after_apply() {
        let mut doc = Document::new(100, 100);
        let a = doc.add_raster_layer("A");
        doc.layer_mut(a).unwrap().offset = IVec2::new(10, 10);

        let mut cmd = ResizeCanvasCmd::new(120, 120, CanvasAnchor::TopLeft);
        cmd.apply(&mut doc).unwrap();

        let b = doc.add_raster_layer("B");
        doc.layer_mut(b).unwrap().offset = IVec2::new(5, 5);

        cmd.undo(&mut doc);
        assert_eq!(doc.layer(a).unwrap().offset, IVec2::new(10, 10));
        assert_eq!(doc.layer(b).unwrap().offset, IVec2::new(5, 5));
    }

    #[test]
    fn batch_cmd_applies_and_undoes_subcommands_as_one() {
        let mut doc = Document::new(10, 10);
        let id = doc.add_raster_layer("L");
        doc.layer_mut(id).unwrap().opacity = 0.5;
        doc.layer_mut(id).unwrap().visible = false;

        let mut batch = BatchCmd::new("Set opacity and visible");
        batch.push(Box::new(SetOpacityCmd::new(id, 0.9)));
        batch.push(Box::new(SetVisibleCmd::new(id, true)));

        batch.apply(&mut doc).unwrap();
        assert!((doc.layer(id).unwrap().opacity - 0.9).abs() < 1e-6);
        assert!(doc.layer(id).unwrap().visible);

        batch.undo(&mut doc);
        assert!((doc.layer(id).unwrap().opacity - 0.5).abs() < 1e-6);
        assert!(!doc.layer(id).unwrap().visible);
    }

    #[derive(Debug)]
    struct AlwaysFailCmd;

    impl Command for AlwaysFailCmd {
        fn label(&self) -> &'static str {
            "Fail"
        }
        fn apply(&mut self, _doc: &mut Document) -> Result<()> {
            Err(OgreError::InvalidOperation("always fails"))
        }
        fn undo(&mut self, _doc: &mut Document) {}
    }

    #[test]
    fn batch_cmd_rolls_back_applied_commands_on_failure() {
        let mut doc = Document::new(10, 10);
        let id = doc.add_raster_layer("L");
        doc.layer_mut(id).unwrap().opacity = 0.5;

        let mut batch = BatchCmd::new("Partial then fail");
        batch.push(Box::new(SetOpacityCmd::new(id, 0.9)));
        batch.push(Box::new(AlwaysFailCmd));

        assert!(batch.apply(&mut doc).is_err());
        assert!((doc.layer(id).unwrap().opacity - 0.5).abs() < 1e-6);
    }

    #[test]
    fn paste_image_layer_cmd_round_trips() {
        let mut doc = Document::new(4, 4);

        // Four distinct pixels for a 2×2 paste.
        let pixels = vec![
            crate::pixel::Rgba32F::new(1.0, 0.0, 0.0, 1.0),
            crate::pixel::Rgba32F::new(0.0, 1.0, 0.0, 1.0),
            crate::pixel::Rgba32F::new(0.0, 0.0, 1.0, 1.0),
            crate::pixel::Rgba32F::new(1.0, 1.0, 0.0, 1.0),
        ];
        let mut cmd = PasteImageLayerCmd::new("P", 2, 2, pixels);

        cmd.apply(&mut doc).unwrap();
        let new_id = cmd.new_id.unwrap();
        assert!(doc.order.contains(&new_id));
        assert_eq!(doc.layer(new_id).unwrap().name, "P");

        // Verify the pixel at (1, 0) was written correctly.
        let buf = doc.layer(new_id).unwrap().buffer().unwrap();
        let px = buf.get_pixel(crate::coord::IVec2::new(1, 0));
        assert!((px.g - 1.0).abs() < 1e-6);

        // Undo should soft-remove the layer.
        cmd.undo(&mut doc);
        assert!(!doc.order.contains(&new_id));

        // Redo (second apply) restores it from soft-removed state.
        cmd.apply(&mut doc).unwrap();
        assert!(doc.order.contains(&new_id));
    }

    #[test]
    fn batch_cmd_referenced_layers_is_union() {
        let mut doc = Document::new(10, 10);
        let a = doc.add_raster_layer("A");
        let b = doc.add_raster_layer("B");

        let mut batch = BatchCmd::new("Group");
        batch.push(Box::new(SetOpacityCmd::new(a, 0.5)));
        batch.push(Box::new(SetVisibleCmd::new(b, false)));

        let mut refs = batch.referenced_layers();
        refs.sort();
        assert_eq!(refs, vec![a, b]);
    }

    #[test]
    fn add_vector_layer_cmd_round_trips() {
        let mut doc = Document::new(100, 100);
        let bg = doc.add_raster_layer("Background");
        doc.active = Some(bg);

        let data = crate::layer::VectorData {
            paths: vec![crate::layer::VectorPath {
                vertices: vec![
                    IVec2::new(0, 0),
                    IVec2::new(10, 0),
                    IVec2::new(10, 10),
                    IVec2::new(0, 10),
                ],
                fill: crate::layer::VectorFill::Solid(crate::pixel::Rgba32F::new(
                    1.0, 0.0, 0.0, 1.0,
                )),
                stroke: crate::layer::VectorStroke {
                    color: crate::pixel::Rgba32F::TRANSPARENT,
                    width: 0.0,
                    dash: Vec::new(),
                    cap: crate::layer::StrokeCap::Butt,
                    join: crate::layer::StrokeJoin::Miter,
                },
                closed: true,
            }],
            rasterized: None,
            text: None,
            svg_source: None,
            version: 0,
        };

        let mut cmd = AddVectorLayerCmd::new("Vector", data.clone());
        cmd.apply(&mut doc).unwrap();
        let new_id = doc.active.unwrap();
        assert_ne!(new_id, bg);
        assert_eq!(doc.layer(new_id).unwrap().name, "Vector");
        assert!(matches!(
            doc.layer(new_id).unwrap().content,
            crate::layer::LayerContent::Vector(_)
        ));
        assert_eq!(doc.layer(new_id).unwrap().offset, IVec2::ZERO);

        cmd.undo(&mut doc);
        assert_eq!(doc.active, Some(bg));
        assert!(!doc.order.contains(&new_id));

        cmd.apply(&mut doc).unwrap();
        assert_eq!(doc.active, Some(new_id));
        assert!(doc.order.contains(&new_id));
    }

    #[test]
    fn add_vector_layer_cmd_with_offset_positions_layer() {
        let mut doc = Document::new(100, 100);
        let bg = doc.add_raster_layer("Background");
        doc.active = Some(bg);

        let data = crate::layer::VectorData {
            paths: vec![crate::layer::VectorPath {
                vertices: vec![IVec2::new(0, 0), IVec2::new(5, 0), IVec2::new(5, 5)],
                fill: crate::layer::VectorFill::Solid(crate::pixel::Rgba32F::new(
                    0.0, 0.0, 1.0, 1.0,
                )),
                stroke: crate::layer::VectorStroke {
                    color: crate::pixel::Rgba32F::TRANSPARENT,
                    width: 0.0,
                    dash: Vec::new(),
                    cap: crate::layer::StrokeCap::Butt,
                    join: crate::layer::StrokeJoin::Miter,
                },
                closed: true,
            }],
            rasterized: None,
            text: None,
            svg_source: None,
            version: 0,
        };

        let mut cmd = AddVectorLayerCmd::new("Vector", data).with_offset(IVec2::new(12, 34));
        cmd.apply(&mut doc).unwrap();
        let new_id = doc.active.unwrap();
        assert_eq!(doc.layer(new_id).unwrap().offset, IVec2::new(12, 34));
    }
}
