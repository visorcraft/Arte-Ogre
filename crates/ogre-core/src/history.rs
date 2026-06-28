// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Undo/redo history and the `Command` trait.
//!
//! All document mutations go through a [`Command`] that is pushed onto a
//! [`History`] stack. Commands capture the inverse of their effects using
//! cheap [`Arc`]-shared tile snapshots, so undo snapshots are proportional to
//! the number of touched tiles rather than the size of the layer.

use std::fmt;
use std::sync::Arc;

use ahash::AHashSet;

use crate::brush::{rasterize_stroke, BrushSettings, InputSample, PaintMode};
use crate::buffer::TiledBuffer;
use crate::coord::{pixel_to_tile, IVec2, Rect, TileCoord};
use crate::document::Document;
use crate::error::{OgreError, Result};
use crate::layer::{AdjustmentKind, Layer, LayerContent, LayerId};
use crate::ops::{
    apply_mask_to_buffer, copy_selection_to_new_layer, cut_selection_to_new_layer, fill_gradient,
    fill_region, fill_selection, magic_wand, remove_matte, stroke_selection, GradientKind,
    MaskInit, WrapMode,
};
use crate::pixel::Rgba32F;
use crate::selection::{Selection, SelectionMode};
use crate::tile::Tile;

use glam::Vec2;

/// A single reversible document edit.
///
/// Commands are the only supported mutation path for a [`Document`]. Each
/// command is responsible for applying its effect, capturing enough
/// information to undo it, and restoring the prior state when [`Command::undo`]
/// is called.
///
/// Commands must be [`Send`] so history can be owned by a background thread.
pub trait Command: Send {
    /// Human-readable label shown in the undo/redo menu.
    fn label(&self) -> &'static str;

    /// Apply the command's effect to `doc`.
    ///
    /// On success the command should store any information it needs to be
    /// undone later.
    fn apply(&mut self, doc: &mut Document) -> Result<()>;

    /// Undo the command's effect, restoring `doc` to the state before
    /// [`Command::apply`] was called.
    fn undo(&mut self, doc: &mut Document);

    /// Clean up any transient state before the command is dropped from history.
    ///
    /// This is called when a command is evicted from the undo stack due to the
    /// depth limit, or when the history is cleared. The default implementation
    /// does nothing.
    fn cleanup(&self, _doc: &mut Document) {}

    /// Try to absorb a new opacity change for `layer` into this command.
    ///
    /// Used by the UI while dragging an opacity slider so that a single drag
    /// produces exactly one history entry. The default implementation returns
    /// `false`; commands that represent an opacity change should update `doc`
    /// in place and return `true`.
    fn coalesce_opacity(&mut self, _doc: &mut Document, _layer: LayerId, _opacity: f32) -> bool {
        false
    }

    /// Coalesce a live selection update into this command instead of pushing a
    /// new history entry.
    ///
    /// Used by the UI while building a Polygon Lasso so each placed vertex
    /// updates the live selection but the whole polygon is one undo step. The
    /// default returns `false`; a replace-mode selection command updates `doc`
    /// in place and returns `true`.
    fn coalesce_selection(&mut self, _doc: &mut Document, _selection: &Selection) -> bool {
        false
    }

    /// Return the layer ids this command may need to access during `undo` or
    /// `redo`.
    ///
    /// This is used by [`History`] to avoid hard-removing a layer while a later
    /// command on the stack still references it. The default implementation
    /// returns an empty list.
    fn referenced_layers(&self) -> Vec<LayerId> {
        Vec::new()
    }
}

/// An undo/redo stack for [`Command`]s.
///
/// `History` owns the commands that have been applied to a document. It
/// supports the classic push/undo/redo workflow and enforces an optional
/// depth limit by evicting the oldest command from the undo stack.
pub struct History {
    undo: Vec<Box<dyn Command>>,
    redo: Vec<Box<dyn Command>>,
    limit: usize,
}

impl fmt::Debug for History {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("History")
            .field("undo_len", &self.undo_len())
            .field("redo_len", &self.redo_len())
            .field("limit", &self.limit)
            .finish()
    }
}

impl History {
    /// Create an empty history with the given undo depth limit.
    ///
    /// A limit of `0` means unlimited undo.
    pub fn new(limit: usize) -> Self {
        Self {
            undo: Vec::new(),
            redo: Vec::new(),
            limit,
        }
    }

    /// Apply `cmd` to `doc` and push it onto the undo stack.
    ///
    /// If the command succeeds, the redo stack is cleared. If the undo stack
    /// grows beyond `limit`, the oldest command is dropped.
    pub fn do_command(&mut self, doc: &mut Document, mut cmd: Box<dyn Command>) -> Result<()> {
        cmd.apply(doc)?;
        self.push_applied(doc, cmd);
        Ok(())
    }

    /// Push a command that has already been applied onto the undo stack.
    ///
    /// This is used by batch operations and scripts that apply a group of
    /// subcommands incrementally but want to expose them as a single undo/redo
    /// entry. The redo stack is cleared and the depth limit is enforced exactly
    /// like [`History::do_command`].
    pub fn push_applied(&mut self, doc: &mut Document, cmd: Box<dyn Command>) {
        self.undo.push(cmd);

        // Collect layer ids still referenced by commands that remain on the
        // undo stack. Redo commands are about to be discarded, so they do not
        // protect a layer from cleanup.
        let remaining_refs: AHashSet<LayerId> = self
            .undo
            .iter()
            .flat_map(|c| c.referenced_layers())
            .collect();
        let evicted_redo: Vec<Box<dyn Command>> = self.redo.drain(..).collect();
        for evicted in evicted_redo {
            cleanup_if_unreferenced(evicted, &remaining_refs, doc);
        }

        if self.limit > 0 && self.undo.len() > self.limit {
            let evicted = self.undo.remove(0);
            let remaining_refs: AHashSet<LayerId> = self
                .undo
                .iter()
                .flat_map(|c| c.referenced_layers())
                .collect();
            cleanup_if_unreferenced(evicted, &remaining_refs, doc);
        }
    }

    /// Borrow the top of the undo stack mutably.
    ///
    /// This is used by coalescing UI interactions (such as dragging an opacity
    /// slider) to update the most recent command in place instead of pushing a
    /// new one each frame.
    pub fn undo_top_mut(&mut self) -> Option<&mut Box<dyn Command>> {
        self.undo.last_mut()
    }

    /// Undo the most recently applied command.
    ///
    /// Returns the label of the undone command, or `None` if the undo stack
    /// was empty.
    pub fn undo(&mut self, doc: &mut Document) -> Option<&'static str> {
        let mut cmd = self.undo.pop()?;
        let label = cmd.label();
        cmd.undo(doc);
        self.redo.push(cmd);
        Some(label)
    }

    /// Redo the most recently undone command.
    ///
    /// Returns the label of the redone command, or `None` if the redo stack
    /// was empty or if re-applying the command failed.
    pub fn redo(&mut self, doc: &mut Document) -> Option<&'static str> {
        let mut cmd = self.redo.pop()?;
        let label = cmd.label();
        if cmd.apply(doc).is_err() {
            self.redo.push(cmd);
            return None;
        }
        self.undo.push(cmd);
        Some(label)
    }

    /// Drop every command from both stacks.
    ///
    /// `doc` is the document the history applies to; it is passed to
    /// [`Command::cleanup`] so commands can reclaim transient state (such as
    /// soft-deleted layers) before they are dropped.
    pub fn clear(&mut self, doc: &mut Document) {
        for cmd in self.undo.drain(..) {
            cmd.cleanup(doc);
        }
        for cmd in self.redo.drain(..) {
            cmd.cleanup(doc);
        }
    }

    /// Number of commands currently on the undo stack.
    pub fn undo_len(&self) -> usize {
        self.undo.len()
    }

    /// Number of commands currently on the redo stack.
    pub fn redo_len(&self) -> usize {
        self.redo.len()
    }

    /// The configured undo depth limit.
    ///
    /// `0` means unlimited undo.
    pub fn limit(&self) -> usize {
        self.limit
    }
}

fn cleanup_if_unreferenced(
    cmd: Box<dyn Command>,
    remaining_refs: &AHashSet<LayerId>,
    doc: &mut Document,
) {
    let refs = cmd.referenced_layers();
    if refs.iter().all(|id| !remaining_refs.contains(id)) {
        cmd.cleanup(doc);
    }
}

/// Capture an [`Arc<Tile>`] snapshot of every tile coordinate in `coords`.
fn snapshot_tiles(
    buffer: &TiledBuffer,
    coords: &[TileCoord],
) -> Vec<(TileCoord, Option<Arc<Tile>>)> {
    coords.iter().map(|&c| (c, buffer.get_tile(c))).collect()
}

/// Restore a buffer from a tile snapshot.
fn restore_tiles(buffer: &mut TiledBuffer, snapshots: &[(TileCoord, Option<Arc<Tile>>)]) {
    for (c, snap) in snapshots {
        buffer.restore_tile(*c, snap.clone());
    }
}

/// Paint individual pixels onto a raster layer.
///
/// The command snapshots every tile touched by the paint operation before any
/// pixel is written. Undo restores the exact [`Arc`]s that were shared before
/// the edit, so a one-pixel paint costs one tile snapshot.
#[derive(Debug)]
pub struct PaintCmd {
    layer: LayerId,
    points: Vec<(IVec2, Rgba32F)>,
    snapshots: Vec<(TileCoord, Option<Arc<Tile>>)>,
    prior_active: Option<LayerId>,
}

impl PaintCmd {
    /// Create a new paint command.
    ///
    /// `points` are in **layer-local** coordinates, matching the coordinate
    /// space used by [`TiledBuffer::set_pixel`].
    pub fn new(layer: LayerId, points: Vec<(IVec2, Rgba32F)>) -> Self {
        Self {
            layer,
            points,
            snapshots: Vec::new(),
            prior_active: None,
        }
    }
}

impl Command for PaintCmd {
    fn label(&self) -> &'static str {
        "Paint"
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        let layer = doc.layer(self.layer)?;
        if !layer.is_raster() {
            return Err(OgreError::NotRaster);
        }
        if layer.locked {
            return Err(OgreError::LayerLocked(self.layer));
        }

        let mut tiles = AHashSet::new();
        for (p, _) in &self.points {
            tiles.insert(pixel_to_tile(*p).0);
        }
        let tiles: Vec<TileCoord> = tiles.into_iter().collect();

        {
            let buffer = doc.layer(self.layer).unwrap().buffer().unwrap();
            self.snapshots = snapshot_tiles(buffer, &tiles);
        }

        {
            let buffer = doc.layer_mut(self.layer).unwrap().buffer_mut().unwrap();
            for (p, px) in &self.points {
                buffer.set_pixel(*p, *px);
            }
        }

        self.prior_active = doc.active;
        doc.active = Some(self.layer);
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        let buffer = match doc.layer_mut(self.layer) {
            Ok(layer) => layer.buffer_mut().unwrap(),
            Err(_) => return,
        };
        restore_tiles(buffer, &self.snapshots);
        doc.active = self.prior_active;
    }

    fn referenced_layers(&self) -> Vec<LayerId> {
        vec![self.layer]
    }
}

/// Paint a brush stroke onto a raster layer.
///
/// The command snapshots every tile touched by the stroke's bounding box,
/// converts the input samples from document to layer-local coordinates, and
/// rasterizes the stroke into the target layer.  Undo restores the exact
/// [`Arc`]s that existed before the edit.
#[derive(Debug)]
pub struct PaintStrokeCmd {
    layer: LayerId,
    samples: Vec<InputSample>,
    settings: BrushSettings,
    color: Rgba32F,
    mode: PaintMode,
    snapshots: Vec<(TileCoord, Option<Arc<Tile>>)>,
    prior_active: Option<LayerId>,
}

impl PaintStrokeCmd {
    /// Create a new stroke paint command.
    ///
    /// `samples` are in **document** coordinates.  They are converted to the
    /// target layer's local coordinate space when the command is applied.
    pub fn new(
        layer: LayerId,
        samples: Vec<InputSample>,
        settings: BrushSettings,
        color: Rgba32F,
        mode: PaintMode,
    ) -> Self {
        Self {
            layer,
            samples,
            settings,
            color,
            mode,
            snapshots: Vec::new(),
            prior_active: None,
        }
    }
}

impl Command for PaintStrokeCmd {
    fn label(&self) -> &'static str {
        match self.mode {
            PaintMode::Brush => "Paint stroke",
            PaintMode::Eraser => "Erase stroke",
        }
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        let layer = doc.layer(self.layer)?;
        if !layer.is_raster() {
            return Err(OgreError::NotRaster);
        }
        if layer.locked {
            return Err(OgreError::LayerLocked(self.layer));
        }
        let offset = layer.offset;

        // Convert samples to layer-local coordinates.
        let local_samples: Vec<InputSample> = self
            .samples
            .iter()
            .map(|s| InputSample {
                pos: s.pos - glam::Vec2::new(offset.x as f32, offset.y as f32),
                pressure: s.pressure,
            })
            .collect();

        if local_samples.is_empty() {
            self.prior_active = doc.active;
            doc.active = Some(self.layer);
            return Ok(());
        }

        // Compute the bounding box of all stamps, expanded by each sample's
        // pressure-mapped radius.
        let mut min = None;
        let mut max = None;
        for s in &local_samples {
            let radius = self.settings.width_at_pressure(s.pressure) / 2.0;
            let p_min = IVec2::new(
                (s.pos.x - radius).floor() as i32,
                (s.pos.y - radius).floor() as i32,
            );
            let p_max = IVec2::new(
                (s.pos.x + radius).ceil() as i32,
                (s.pos.y + radius).ceil() as i32,
            );
            min = Some(min.map(|m: IVec2| m.min(p_min)).unwrap_or(p_min));
            max = Some(max.map(|m: IVec2| m.max(p_max)).unwrap_or(p_max));
        }

        let tiles = if let (Some(min), Some(max)) = (min, max) {
            let rect = Rect::from_corners(min, max);
            rect.tiles_covered().collect()
        } else {
            Vec::new()
        };

        if !tiles.is_empty() {
            {
                let buffer = doc.layer(self.layer).unwrap().buffer().unwrap();
                self.snapshots = snapshot_tiles(buffer, &tiles);
            }
            {
                let buffer = doc.layer_mut(self.layer).unwrap().buffer_mut().unwrap();
                rasterize_stroke(
                    buffer,
                    &local_samples,
                    &self.settings,
                    self.color,
                    self.mode,
                );
            }
        }

        self.prior_active = doc.active;
        doc.active = Some(self.layer);
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        let buffer = match doc.layer_mut(self.layer) {
            Ok(layer) => layer.buffer_mut().unwrap(),
            Err(_) => return,
        };
        restore_tiles(buffer, &self.snapshots);
        doc.active = self.prior_active;
    }

    fn referenced_layers(&self) -> Vec<LayerId> {
        vec![self.layer]
    }
}

/// Apply a set of pre-computed per-pixel writes to a raster layer.
///
/// This is the commit path for the brush-engine "finger" tools (Blur/Sharpen/
/// Smudge/Dodge/Burn/Sponge/Color Replacement/Clone Stamp/Healing — see
/// `docs/TOOLS_SPEC.md` §3.2). Unlike [`PaintStrokeCmd`] (which re-rasterizes
/// from stroke samples + a `PaintMode`), this command takes the **final** pixel
/// values for each touched layer-local pixel; the tool — not the command —
/// resolves overlapping stamps against its frozen backdrop and produces the
/// fully-composited result. The command therefore writes pixels directly
/// (overwrite) and snapshots only the tiles it touches for undo.
pub struct BrushStrokeCmd {
    layer: LayerId,
    edits: Vec<(IVec2, Rgba32F)>,
    snapshots: Vec<(TileCoord, Option<Arc<Tile>>)>,
    prior_active: Option<LayerId>,
}

impl std::fmt::Debug for BrushStrokeCmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrushStrokeCmd")
            .field("layer", &self.layer)
            .field("edits", &self.edits.len())
            .finish()
    }
}

impl BrushStrokeCmd {
    /// Create a brush-stroke command. `edits` are `(layer-local pixel, final
    /// value)` pairs; the order within a single pixel is irrelevant because the
    /// tool already resolved overlaps.
    pub fn new(layer: LayerId, edits: Vec<(IVec2, Rgba32F)>) -> Self {
        Self {
            layer,
            edits,
            snapshots: Vec::new(),
            prior_active: None,
        }
    }
}

impl Command for BrushStrokeCmd {
    fn label(&self) -> &'static str {
        "Brush stroke"
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        let layer = doc.layer(self.layer)?;
        if !layer.is_raster() {
            return Err(OgreError::NotRaster);
        }
        if layer.locked {
            return Err(OgreError::LayerLocked(self.layer));
        }

        self.prior_active = doc.active;

        if self.edits.is_empty() {
            doc.active = Some(self.layer);
            return Ok(());
        }

        // Snapshot the tiles the edits touch (sparse undo, mirroring
        // PaintStrokeCmd). Empty tiles are recorded as `None` so undo can
        // restore the "didn't exist" state too.
        let (min, max) = self.edits.iter().fold(
            (
                IVec2::new(i32::MAX, i32::MAX),
                IVec2::new(i32::MIN, i32::MIN),
            ),
            |(mn, mx), (p, _)| (mn.min(*p), mx.max(*p)),
        );
        let rect = Rect::from_corners(min, max);
        let tiles: Vec<TileCoord> = rect.tiles_covered().collect();
        {
            let buffer = doc.layer(self.layer).unwrap().buffer().unwrap();
            self.snapshots = snapshot_tiles(buffer, &tiles);
        }
        {
            let buffer = doc.layer_mut(self.layer).unwrap().buffer_mut().unwrap();
            for (p, v) in &self.edits {
                buffer.set_pixel(*p, *v);
            }
        }

        doc.active = Some(self.layer);
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let Ok(layer) = doc.layer_mut(self.layer) {
            if let Some(buffer) = layer.buffer_mut() {
                restore_tiles(buffer, &self.snapshots);
            }
        }
        doc.active = self.prior_active;
    }

    fn referenced_layers(&self) -> Vec<LayerId> {
        vec![self.layer]
    }
}

/// Copy the selected pixels from one layer to a new raster layer.
///
/// Undo removes the inserted layer. The command stores the id assigned to the
/// new layer so it can be removed precisely on undo.
#[derive(Debug)]
pub struct CopyToNewLayerCmd {
    source: LayerId,
    selection: Selection,
    new_id: Option<LayerId>,
    prior_active: Option<LayerId>,
}

impl CopyToNewLayerCmd {
    /// Create a copy-to-new-layer command.
    pub fn new(source: LayerId, selection: Selection) -> Self {
        Self {
            source,
            selection,
            new_id: None,
            prior_active: None,
        }
    }
}

impl Command for CopyToNewLayerCmd {
    fn label(&self) -> &'static str {
        "Copy to new layer"
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        self.prior_active = doc.active;
        let new_id = copy_selection_to_new_layer(doc, self.source, &self.selection)?;
        self.new_id = Some(new_id);
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let Some(id) = self.new_id.take() {
            let _ = doc.remove_layer(id);
        }
        doc.active = self.prior_active;
    }

    fn referenced_layers(&self) -> Vec<LayerId> {
        let mut ids = vec![self.source];
        if let Some(id) = self.new_id {
            ids.push(id);
        }
        ids
    }
}

/// Cut the selected pixels from one layer to a new raster layer.
///
/// The command snapshots the source tiles touched by the selection before the
/// cut. Undo uses a soft delete so the new layer keeps its id; this keeps any
/// later commands that reference the new layer (e.g. a move) valid across
/// undo/redo. The layer is hard-removed only when the command is evicted from
/// history.
#[derive(Debug)]
pub struct CutToNewLayerCmd {
    source: LayerId,
    selection: Selection,
    snapshots: Vec<(TileCoord, Option<Arc<Tile>>)>,
    new_id: Option<LayerId>,
    prior_active: Option<LayerId>,
    parent: Option<LayerId>,
    index: usize,
    removed_ids: Vec<LayerId>,
}

impl CutToNewLayerCmd {
    /// Create a cut-to-new-layer command.
    pub fn new(source: LayerId, selection: Selection) -> Self {
        Self {
            source,
            selection,
            snapshots: Vec::new(),
            new_id: None,
            prior_active: None,
            parent: None,
            index: 0,
            removed_ids: Vec::new(),
        }
    }
}

impl Command for CutToNewLayerCmd {
    fn label(&self) -> &'static str {
        "Cut to new layer"
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        if let Some(new_id) = self.new_id {
            // Redo after an undo: restore the soft-removed layer and re-erase the
            // source selection.
            doc.restore_layer_soft(self.parent, self.index, new_id, &self.removed_ids);
            crate::ops::extract::erase_selection(doc, self.source, &self.selection)?;
            doc.active = Some(new_id);
            return Ok(());
        }

        self.prior_active = doc.active;

        {
            let layer = doc.layer(self.source)?;
            if !layer.is_raster() {
                return Err(OgreError::NotRaster);
            }
            if layer.locked {
                return Err(OgreError::LayerLocked(self.source));
            }
            let tiles: Vec<TileCoord> = self.selection.selected_tiles(layer.offset).collect();
            let buffer = layer.buffer().unwrap();
            self.snapshots = snapshot_tiles(buffer, &tiles);
        }

        let new_id = cut_selection_to_new_layer(doc, self.source, &self.selection)?;
        if let Some((parent, idx)) = doc.sibling_index(new_id) {
            self.parent = parent;
            self.index = idx;
        }
        self.new_id = Some(new_id);
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let Ok(layer) = doc.layer_mut(self.source) {
            let buffer = layer.buffer_mut().unwrap();
            restore_tiles(buffer, &self.snapshots);
        }

        if let Some(id) = self.new_id {
            if self.parent.is_none() {
                if let Some((parent, idx)) = doc.sibling_index(id) {
                    self.parent = parent;
                    self.index = idx;
                }
            }
            self.removed_ids = doc.remove_layer_soft(id).unwrap_or_default();
        }

        doc.active = self.prior_active;
    }

    fn cleanup(&self, doc: &mut Document) {
        for &id in self.removed_ids.iter().rev() {
            if doc.removed.contains(&id) {
                let _ = doc.remove_layer(id);
            }
        }
    }

    fn referenced_layers(&self) -> Vec<LayerId> {
        let mut ids = vec![self.source];
        if let Some(id) = self.new_id {
            ids.push(id);
        }
        ids
    }
}

/// Delete a layer (and its descendants if it is a group).
///
/// The command uses a soft delete: the layer subtree is detached from the
/// tree and marked as removed, but it stays in the layer arena so undo can
/// restore it with the exact same [`LayerId`]. This keeps every other command
/// in the history stack that references this id valid after an undo.
#[derive(Debug)]
pub struct DeleteLayerCmd {
    id: LayerId,
    parent: Option<LayerId>,
    index: usize,
    removed_ids: Vec<LayerId>,
    prior_active: Option<LayerId>,
}

impl DeleteLayerCmd {
    /// Create a delete-layer command.
    pub fn new(id: LayerId) -> Self {
        Self {
            id,
            parent: None,
            index: 0,
            removed_ids: Vec::new(),
            prior_active: None,
        }
    }
}

impl Command for DeleteLayerCmd {
    fn label(&self) -> &'static str {
        "Delete layer"
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        self.prior_active = doc.active;
        let (parent, index) = doc
            .sibling_index(self.id)
            .ok_or(OgreError::LayerNotFound(self.id))?;
        self.parent = parent;
        self.index = index;
        self.removed_ids = doc.remove_layer_soft(self.id)?;
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if self.removed_ids.is_empty() {
            return;
        }
        doc.restore_layer_soft(self.parent, self.index, self.id, &self.removed_ids);
        doc.active = self.prior_active;
    }

    fn cleanup(&self, doc: &mut Document) {
        for &id in self.removed_ids.iter().rev() {
            if doc.removed.contains(&id) {
                let _ = doc.remove_layer(id);
            }
        }
    }

    fn referenced_layers(&self) -> Vec<LayerId> {
        self.removed_ids.clone()
    }
}

/// A layer subtree snapshotted from a source document for insertion into
/// another document.
///
/// Group children are stored recursively in [`LayerSubtreeSnapshot::children`]
/// rather than as source-document [`LayerId`]s, which are meaningless in the
/// destination. The stored group [`Layer`] therefore has an empty child list;
/// [`InsertLayerSubtreeCmd::apply`] rebuilds it with freshly allocated
/// destination ids.
#[derive(Clone, Debug)]
struct LayerSubtreeSnapshot {
    layer: Layer,
    children: Vec<LayerSubtreeSnapshot>,
}

/// Recursively snapshot the subtree rooted at `id` in `doc`.
fn snapshot_subtree(doc: &Document, id: LayerId) -> Result<LayerSubtreeSnapshot> {
    let mut layer = doc.layer(id)?.clone();
    let mut children = Vec::new();
    if let LayerContent::Group { children: kids } = &layer.content {
        for child in kids.clone() {
            children.push(snapshot_subtree(doc, child)?);
        }
    }
    // The stored ids are from the source document; clear them so apply can
    // rebuild the list from freshly allocated destination ids.
    if let LayerContent::Group { children: kids } = &mut layer.content {
        kids.clear();
    }
    Ok(LayerSubtreeSnapshot { layer, children })
}

/// Insert a snapshotted layer subtree into a destination document.
///
/// `Command::apply` only receives the destination document, so the source
/// subtree is captured up front by [`InsertLayerSubtreeCmd::new_from_source`].
/// This enables cross-document copy/move in Bird's Eye View: the same command
/// performs the destination-side insert for both, and a separate
/// [`DeleteLayerCmd`] handles the source-side removal of a move.
///
/// Undo soft-removes the inserted root so redo can restore it with stable ids;
/// if the command is evicted and its layers hard-removed, a later redo
/// re-inserts a fresh copy from the stored snapshot.
#[derive(Debug)]
pub struct InsertLayerSubtreeCmd {
    snapshot: LayerSubtreeSnapshot,
    dest_index: usize,
    inserted_root: Option<LayerId>,
    inserted_ids: Vec<LayerId>,
    parent: Option<LayerId>,
    index: usize,
    removed_ids: Vec<LayerId>,
    prior_active: Option<LayerId>,
}

impl InsertLayerSubtreeCmd {
    /// Snapshot `layer_id`'s subtree from `source` for later insertion at
    /// `dest_index`, a bottom-to-top index into the destination root order.
    pub fn new_from_source(
        source: &Document,
        layer_id: LayerId,
        dest_index: usize,
    ) -> Result<Self> {
        let snapshot = snapshot_subtree(source, layer_id)?;
        Ok(Self {
            snapshot,
            dest_index,
            inserted_root: None,
            inserted_ids: Vec::new(),
            parent: None,
            index: 0,
            removed_ids: Vec::new(),
            prior_active: None,
        })
    }

    /// Recursively allocate fresh destination ids for `node` and its children,
    /// rebuilding each group's child list. Returns the new root id.
    fn insert_subtree(
        doc: &mut Document,
        node: &LayerSubtreeSnapshot,
        ids: &mut Vec<LayerId>,
    ) -> LayerId {
        let id = doc.add_layer(node.layer.clone());
        ids.push(id);
        let mut child_ids = Vec::with_capacity(node.children.len());
        for child in &node.children {
            child_ids.push(Self::insert_subtree(doc, child, ids));
        }
        if let Ok(layer) = doc.layer_mut(id) {
            if let LayerContent::Group { children } = &mut layer.content {
                *children = child_ids;
            }
        }
        id
    }
}

impl Command for InsertLayerSubtreeCmd {
    fn label(&self) -> &'static str {
        "Insert layer"
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        self.prior_active = doc.active;
        // Redo path: restore the soft-removed subtree with its stable ids while
        // it still lives in the arena.
        if let Some(root) = self.inserted_root {
            if doc.all_layers().contains_key(root) {
                doc.restore_layer_soft(self.parent, self.index, root, &self.removed_ids);
                doc.active = Some(root);
                return Ok(());
            }
        }
        // First apply, or redo after the prior copy was hard-removed: rebuild
        // from the stored snapshot with newly allocated ids.
        let mut ids = Vec::new();
        let root = Self::insert_subtree(doc, &self.snapshot, &mut ids);
        let index = self.dest_index.min(doc.order.len());
        doc.insert_into_siblings(None, index, root)?;
        self.inserted_root = Some(root);
        self.inserted_ids = ids;
        self.parent = None;
        self.index = index;
        doc.active = Some(root);
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let Some(root) = self.inserted_root {
            if let Some((parent, idx)) = doc.sibling_index(root) {
                self.parent = parent;
                self.index = idx;
            }
            self.removed_ids = doc.remove_layer_soft(root).unwrap_or_default();
        }
        doc.active = self.prior_active;
    }

    fn cleanup(&self, doc: &mut Document) {
        for &id in self.removed_ids.iter().rev() {
            if doc.removed.contains(&id) {
                let _ = doc.remove_layer(id);
            }
        }
    }

    fn referenced_layers(&self) -> Vec<LayerId> {
        self.inserted_ids.clone()
    }
}

/// Move a layer to a new index within its current sibling list.
#[derive(Debug)]
pub struct ReorderCmd {
    id: LayerId,
    new_index: usize,
    old_index: Option<usize>,
    applied_index: Option<usize>,
}

impl ReorderCmd {
    /// Create a reorder command.
    pub fn new(id: LayerId, new_index: usize) -> Self {
        Self {
            id,
            new_index,
            old_index: None,
            applied_index: None,
        }
    }
}

impl Command for ReorderCmd {
    fn label(&self) -> &'static str {
        "Reorder layer"
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        let (_, current) = doc
            .sibling_index(self.id)
            .ok_or(OgreError::LayerNotFound(self.id))?;
        self.old_index = Some(current);
        doc.reorder(self.id, self.new_index)?;
        let (_, applied) = doc
            .sibling_index(self.id)
            .ok_or(OgreError::LayerNotFound(self.id))?;
        self.applied_index = Some(applied);
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let Some(idx) = self.old_index {
            let _ = doc.reorder(self.id, idx);
        }
    }

    fn referenced_layers(&self) -> Vec<LayerId> {
        vec![self.id]
    }
}

/// Toggle a layer's `locked` flag (undoable).
///
/// Stores the prior value so undo restores it exactly. Locking is enforced
/// by every destructive command (`FlipLayerCmd`, `BrushStrokeCmd`, etc.)
/// via `OgreError::LayerLocked`.
#[derive(Debug)]
pub struct SetLayerLockedCmd {
    id: LayerId,
    locked: bool,
    prior: Option<bool>,
}

impl SetLayerLockedCmd {
    /// Create a lock-toggle command. `locked` is the desired new state.
    pub fn new(id: LayerId, locked: bool) -> Self {
        Self {
            id,
            locked,
            prior: None,
        }
    }
}

impl Command for SetLayerLockedCmd {
    fn label(&self) -> &'static str {
        if self.locked {
            "Lock layer"
        } else {
            "Unlock layer"
        }
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        let layer = doc.layer_mut(self.id)?;
        self.prior = Some(layer.locked);
        layer.locked = self.locked;
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let Some(prior) = self.prior {
            if let Ok(layer) = doc.layer_mut(self.id) {
                layer.locked = prior;
            }
        }
    }

    fn referenced_layers(&self) -> Vec<LayerId> {
        vec![self.id]
    }
}

/// Move a layer by adding `delta` to its offset.
///
/// This is the undoable version of [`crate::ops::move_layer_by`]. The command
/// stores the layer's prior offset and active-layer state so undo can restore
/// the exact original state. If the stored layer does not exist, the command
/// returns an error rather than falling back to the active layer.
#[derive(Debug)]
pub struct MoveLayerByCmd {
    id: Option<LayerId>,
    delta: IVec2,
    prior_offset: Option<IVec2>,
    prior_active: Option<LayerId>,
}

impl MoveLayerByCmd {
    /// Create a move-layer-by-delta command.
    pub fn new(id: LayerId, delta: IVec2) -> Self {
        Self {
            id: Some(id),
            delta,
            prior_offset: None,
            prior_active: None,
        }
    }
}

impl Command for MoveLayerByCmd {
    fn label(&self) -> &'static str {
        "Move layer"
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        let id = self
            .id
            .ok_or(OgreError::InvalidOperation("no layer to move"))?;
        if doc.layer(id).is_err() {
            return Err(OgreError::LayerNotFound(id));
        }

        let old_offset = doc.layer(id).unwrap().offset;
        let new_offset = match (
            old_offset.x.checked_add(self.delta.x),
            old_offset.y.checked_add(self.delta.y),
        ) {
            (Some(x), Some(y)) => IVec2::new(x, y),
            _ => return Err(OgreError::InvalidOperation("layer offset out of range")),
        };
        self.prior_offset = Some(old_offset);
        self.prior_active = doc.active;

        let layer = doc.layer_mut(id)?;
        if layer.locked {
            return Err(OgreError::LayerLocked(id));
        }
        layer.offset = new_offset;
        doc.active = Some(id);
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let (Some(id), Some(offset)) = (self.id, self.prior_offset) {
            if let Ok(layer) = doc.layer_mut(id) {
                layer.offset = offset;
            }
        }
        doc.active = self.prior_active;
    }

    fn referenced_layers(&self) -> Vec<LayerId> {
        self.id.into_iter().collect()
    }
}

/// Kind of transform applied by [`TransformLayerCmd`].
#[derive(Debug, Clone, PartialEq)]
pub enum TransformKind {
    /// Affine transform: source document space -> destination document space.
    Affine(glam::Affine2),
    /// Projective (perspective/distort) transform defined by the four
    /// destination corners in document space.
    Perspective([IVec2; 4]),
    /// Bezier-grid warp defined by displaced control points in document space.
    Warp(crate::resample::WarpGrid),
}

/// Apply an arbitrary affine/projective/warp transform to a raster layer.
///
/// The command stores the layer's previous buffer and offset so undo can
/// restore the original pixels exactly. It resamples the layer into a new
/// local buffer whose integer offset is the floor of the transformed
/// document-space bounds, preserving sub-pixel placement as an offset change.
#[derive(Debug)]
pub struct TransformLayerCmd {
    layer: LayerId,
    kind: TransformKind,
    old_buffer: Option<TiledBuffer>,
    old_offset: Option<IVec2>,
    prior_active: Option<LayerId>,
}

impl TransformLayerCmd {
    /// Create an affine transform-layer command.
    ///
    /// `affine` maps **source** document coordinates to **destination**
    /// document coordinates, with pixel indices denoting the upper-left
    /// corners of unit squares.
    pub fn new(layer: LayerId, affine: glam::Affine2) -> Self {
        Self {
            layer,
            kind: TransformKind::Affine(affine),
            old_buffer: None,
            old_offset: None,
            prior_active: None,
        }
    }

    /// Create a projective transform-layer command.
    ///
    /// `dst_quad` are the four destination corners in **document** space
    /// (TL, TR, BR, BL winding). The source is the layer's current occupied
    /// bounds.
    pub fn new_perspective(layer: LayerId, dst_quad: [IVec2; 4]) -> Self {
        Self {
            layer,
            kind: TransformKind::Perspective(dst_quad),
            old_buffer: None,
            old_offset: None,
            prior_active: None,
        }
    }

    /// Create a bezier-grid warp command.
    ///
    /// `grid` control points are in **document** space.
    pub fn new_warp(layer: LayerId, grid: crate::resample::WarpGrid) -> Self {
        Self {
            layer,
            kind: TransformKind::Warp(grid),
            old_buffer: None,
            old_offset: None,
            prior_active: None,
        }
    }
}

impl Command for TransformLayerCmd {
    fn label(&self) -> &'static str {
        "Transform layer"
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        let layer = doc.layer(self.layer)?;
        if !layer.is_raster() {
            return Err(OgreError::NotRaster);
        }
        if layer.locked {
            return Err(OgreError::LayerLocked(self.layer));
        }

        let old_offset = layer.offset;
        let old_buffer = layer.buffer().unwrap().clone();

        let (new_offset, new_buffer) = match &self.kind {
            TransformKind::Affine(affine) => {
                if !affine.is_finite() || affine.matrix2.determinant().abs() <= 1e-6 {
                    return Err(OgreError::InvalidOperation("degenerate transform"));
                }

                let new_offset = if let Some(bounds) = old_buffer.exact_bounds() {
                    let min_local = IVec2::new(bounds.x, bounds.y);
                    let max_local = IVec2::new(
                        bounds.x + bounds.w as i32 - 1,
                        bounds.y + bounds.h as i32 - 1,
                    );
                    let min_doc = min_local + old_offset;
                    let max_doc = max_local + old_offset;

                    let corners = [
                        glam::Vec2::new(min_doc.x as f32, min_doc.y as f32),
                        glam::Vec2::new(max_doc.x as f32 + 1.0, min_doc.y as f32),
                        glam::Vec2::new(min_doc.x as f32, max_doc.y as f32 + 1.0),
                        glam::Vec2::new(max_doc.x as f32 + 1.0, max_doc.y as f32 + 1.0),
                    ];

                    let mut min = glam::Vec2::splat(f32::INFINITY);
                    for c in corners {
                        min = min.min(affine.transform_point2(c));
                    }
                    let ox = i32::try_from(min.x.floor() as i64).map_err(|_| {
                        OgreError::InvalidOperation("transform offset out of range")
                    })?;
                    let oy = i32::try_from(min.y.floor() as i64).map_err(|_| {
                        OgreError::InvalidOperation("transform offset out of range")
                    })?;
                    IVec2::new(ox, oy)
                } else {
                    old_offset
                };

                let local_affine = glam::Affine2::from_translation(-new_offset.as_vec2())
                    * *affine
                    * glam::Affine2::from_translation(old_offset.as_vec2());

                let buffer = crate::resample::resample(
                    &old_buffer,
                    local_affine,
                    crate::resample::Filter::Bicubic,
                );
                (new_offset, buffer)
            }
            TransformKind::Perspective(dst_quad_doc) => {
                let bounds = match old_buffer.exact_bounds() {
                    Some(b) => b,
                    None => {
                        // Empty layer: nothing to warp.
                        return Ok(());
                    }
                };
                let src_bounds = Rect::new(
                    bounds.x + old_offset.x,
                    bounds.y + old_offset.y,
                    bounds.w,
                    bounds.h,
                );
                let min_x = dst_quad_doc.iter().map(|p| p.x).min().unwrap_or(0);
                let min_y = dst_quad_doc.iter().map(|p| p.y).min().unwrap_or(0);
                let new_offset = IVec2::new(min_x, min_y);
                let local_quad = dst_quad_doc.map(|p| p - new_offset);
                let buffer = crate::resample::projective_warp(
                    &old_buffer,
                    src_bounds,
                    local_quad,
                    crate::resample::Filter::Bicubic,
                );
                (new_offset, buffer)
            }
            TransformKind::Warp(grid_doc) => {
                if old_buffer.exact_bounds().is_none() {
                    return Ok(());
                }
                let min_x = grid_doc.points.iter().map(|p| p.x).min().unwrap_or(0);
                let min_y = grid_doc.points.iter().map(|p| p.y).min().unwrap_or(0);
                let new_offset = IVec2::new(min_x, min_y);
                let mut local_grid = grid_doc.clone();
                for p in &mut local_grid.points {
                    *p -= new_offset;
                }
                let buffer = crate::resample::bezier_warp(
                    &old_buffer,
                    &local_grid,
                    crate::resample::Filter::Bicubic,
                );
                (new_offset, buffer)
            }
        };

        let layer = doc.layer_mut(self.layer)?;
        self.old_buffer = Some(std::mem::replace(layer.buffer_mut().unwrap(), new_buffer));
        self.old_offset = Some(old_offset);
        layer.offset = new_offset;

        self.prior_active = doc.active;
        doc.active = Some(self.layer);
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let (Some(buffer), Some(offset)) = (self.old_buffer.take(), self.old_offset.take()) {
            if let Ok(layer) = doc.layer_mut(self.layer) {
                if let Some(buf) = layer.buffer_mut() {
                    *buf = buffer;
                    layer.offset = offset;
                }
            }
        }
        doc.active = self.prior_active;
    }

    fn referenced_layers(&self) -> Vec<LayerId> {
        vec![self.layer]
    }
}

/// Change the document selection.
///
/// This command captures the previous selection so it can be restored on undo.
/// It is used by the UI for shortcuts such as "Select All" and "Deselect".
#[derive(Debug, Clone)]
pub struct SetSelectionCmd {
    new_selection: Selection,
    mode: SelectionMode,
    old_selection: Option<Selection>,
}

impl SetSelectionCmd {
    /// Create a selection command that combines `selection` with the current
    /// document selection using `mode`.
    pub fn with_mode(selection: Selection, mode: SelectionMode) -> Self {
        Self {
            new_selection: selection,
            mode,
            old_selection: None,
        }
    }

    /// Create a selection command that will replace the document selection with
    /// `selection`.
    pub fn new(selection: Selection) -> Self {
        Self::with_mode(selection, SelectionMode::Replace)
    }
}

impl Command for SetSelectionCmd {
    fn label(&self) -> &'static str {
        match self.mode {
            SelectionMode::Replace => "Set selection",
            SelectionMode::Add => "Add to selection",
            SelectionMode::Subtract => "Subtract from selection",
            SelectionMode::Intersect => "Intersect selection",
        }
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        self.old_selection = Some(doc.selection.clone());
        doc.selection = doc.selection.combine(&self.new_selection, self.mode);
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let Some(old) = self.old_selection.take() {
            doc.selection = old;
        }
    }

    fn coalesce_selection(&mut self, doc: &mut Document, selection: &Selection) -> bool {
        // Only replace-mode commands coalesce; the original `old_selection`
        // (captured on first apply) is kept so undo returns to before the
        // polygon, regardless of how many vertices were placed.
        if self.mode != SelectionMode::Replace {
            return false;
        }
        self.new_selection = selection.clone();
        doc.selection = selection.clone();
        true
    }
}

/// Select a contiguous or global region by color.
///
/// The command resolves the active raster layer at apply time and uses the
/// magic-wand algorithm to build a coverage mask. Undo restores the previous
/// selection.
#[derive(Debug, Clone)]
pub struct MagicWandCmd {
    seed_doc: IVec2,
    tolerance: f32,
    contiguous: bool,
    layer: Option<LayerId>,
    mode: SelectionMode,
    old_selection: Option<Selection>,
}

impl MagicWandCmd {
    /// Create a magic-wand command that combines the result with the current
    /// selection using `mode`.
    pub fn with_mode(
        seed_doc: IVec2,
        tolerance: f32,
        contiguous: bool,
        mode: SelectionMode,
    ) -> Self {
        Self {
            seed_doc,
            tolerance,
            contiguous,
            layer: None,
            mode,
            old_selection: None,
        }
    }
}

impl Command for MagicWandCmd {
    fn label(&self) -> &'static str {
        if self.contiguous {
            "Magic wand"
        } else {
            "Select by color"
        }
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        let layer_id = doc.active.ok_or(OgreError::NoActiveLayer)?;
        let layer = doc.layer(layer_id)?;
        if !layer.is_raster() {
            return Err(OgreError::NotRaster);
        }
        let offset = layer.offset;
        let buffer = layer.buffer().unwrap();

        let new_selection = magic_wand(
            buffer,
            offset,
            self.seed_doc,
            self.tolerance,
            self.contiguous,
        );
        self.layer = Some(layer_id);
        self.old_selection = Some(doc.selection.clone());
        doc.selection = doc.selection.combine(&new_selection, self.mode);
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let Some(old) = self.old_selection.take() {
            doc.selection = old;
        }
    }

    fn referenced_layers(&self) -> Vec<LayerId> {
        self.layer.into_iter().collect()
    }
}

/// Invert the current selection within the document canvas.
///
/// Undo restores the previous selection.
#[derive(Debug, Clone, Default)]
pub struct InvertSelectionCmd {
    old_selection: Option<Selection>,
}

impl InvertSelectionCmd {
    /// Create an invert-selection command.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Command for InvertSelectionCmd {
    fn label(&self) -> &'static str {
        "Invert selection"
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        self.old_selection = Some(doc.selection.clone());
        doc.selection = doc.selection.invert(doc.canvas);
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let Some(old) = self.old_selection.take() {
            doc.selection = old;
        }
    }
}

/// Feather (blur) the current selection edge.
#[derive(Debug, Clone)]
pub struct FeatherSelectionCmd {
    radius: f32,
    old_selection: Option<Selection>,
}

impl FeatherSelectionCmd {
    /// Create a feather-selection command with the given Gaussian radius.
    pub fn new(radius: f32) -> Self {
        Self {
            radius,
            old_selection: None,
        }
    }
}

impl Command for FeatherSelectionCmd {
    fn label(&self) -> &'static str {
        "Feather selection"
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        self.old_selection = Some(doc.selection.clone());
        let max_dim = doc.canvas.w.max(doc.canvas.h).max(1);
        let radius = self.radius.clamp(0.0, max_dim as f32);
        doc.selection = doc.selection.feather(radius);
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let Some(old) = self.old_selection.take() {
            doc.selection = old;
        }
    }
}

/// Grow (dilate) the current selection by a number of pixels.
#[derive(Debug, Clone)]
pub struct GrowSelectionCmd {
    amount: u32,
    old_selection: Option<Selection>,
}

impl GrowSelectionCmd {
    /// Create a grow-selection command.
    pub fn new(amount: u32) -> Self {
        Self {
            amount,
            old_selection: None,
        }
    }
}

impl Command for GrowSelectionCmd {
    fn label(&self) -> &'static str {
        "Grow selection"
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        self.old_selection = Some(doc.selection.clone());
        let max_dim = doc.canvas.w.max(doc.canvas.h).max(1);
        let amount = self.amount.min(max_dim);
        doc.selection = doc.selection.grow(amount);
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let Some(old) = self.old_selection.take() {
            doc.selection = old;
        }
    }
}

/// Shrink (erode) the current selection by a number of pixels.
#[derive(Debug, Clone)]
pub struct ShrinkSelectionCmd {
    amount: u32,
    old_selection: Option<Selection>,
}

impl ShrinkSelectionCmd {
    /// Create a shrink-selection command.
    pub fn new(amount: u32) -> Self {
        Self {
            amount,
            old_selection: None,
        }
    }
}

impl Command for ShrinkSelectionCmd {
    fn label(&self) -> &'static str {
        "Shrink selection"
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        self.old_selection = Some(doc.selection.clone());
        let max_dim = doc.canvas.w.max(doc.canvas.h).max(1);
        let amount = self.amount.min(max_dim);
        doc.selection = doc.selection.shrink(amount);
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let Some(old) = self.old_selection.take() {
            doc.selection = old;
        }
    }
}

/// Add a new adjustment layer on top of the root stack.
#[derive(Debug)]
pub struct AddAdjustmentLayerCmd {
    name: String,
    kind: AdjustmentKind,
    new_id: Option<LayerId>,
    inserted_index: Option<usize>,
    prior_active: Option<LayerId>,
}

impl AddAdjustmentLayerCmd {
    /// Create an add-adjustment-layer command.
    pub fn new(name: impl Into<String>, kind: AdjustmentKind) -> Self {
        Self {
            name: name.into(),
            kind,
            new_id: None,
            inserted_index: None,
            prior_active: None,
        }
    }
}

impl Command for AddAdjustmentLayerCmd {
    fn label(&self) -> &'static str {
        "Add adjustment layer"
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
        // a fresh adjustment layer.
        let index = doc.order.len();
        let id = doc.add_adjustment_layer(&self.name, self.kind);
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

/// Change the adjustment kind of an adjustment layer.
#[derive(Debug)]
pub struct SetAdjustmentCmd {
    layer: LayerId,
    kind: AdjustmentKind,
    old: Option<AdjustmentKind>,
}

impl SetAdjustmentCmd {
    /// Create a set-adjustment command.
    pub fn new(layer: LayerId, kind: AdjustmentKind) -> Self {
        Self {
            layer,
            kind,
            old: None,
        }
    }
}

impl Command for SetAdjustmentCmd {
    fn label(&self) -> &'static str {
        "Set adjustment"
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        let layer = doc.layer_mut(self.layer)?;
        match &mut layer.content {
            LayerContent::Adjustment(current) => {
                self.old = Some(*current);
                *current = self.kind;
                Ok(())
            }
            _ => Err(OgreError::InvalidOperation(
                "layer is not an adjustment layer",
            )),
        }
    }

    fn undo(&mut self, doc: &mut Document) {
        if let (Some(old), Ok(layer)) = (self.old, doc.layer_mut(self.layer)) {
            if let LayerContent::Adjustment(current) = &mut layer.content {
                *current = old;
            }
        }
    }

    fn referenced_layers(&self) -> Vec<LayerId> {
        vec![self.layer]
    }
}

/// Remove a "fake transparency" matte from the active raster layer.
///
/// Detects a solid or checkerboard background from the layer border and lifts
/// the connected background region to true transparency (see
/// [`crate::ops::remove_matte`]). The command stores the layer's previous
/// buffer so undo restores the original pixels exactly. `apply` fails with
/// [`OgreError::InvalidOperation`] when no matte is detected, so nothing is
/// pushed onto history.
#[derive(Debug)]
pub struct RemoveBackgroundCmd {
    tolerance: f32,
    layer: Option<LayerId>,
    old_buffer: Option<TiledBuffer>,
    prior_active: Option<LayerId>,
}

impl RemoveBackgroundCmd {
    /// Create a remove-background command with the given color tolerance.
    pub fn new(tolerance: f32) -> Self {
        Self {
            tolerance,
            layer: None,
            old_buffer: None,
            prior_active: None,
        }
    }
}

impl Command for RemoveBackgroundCmd {
    fn label(&self) -> &'static str {
        "Remove background"
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        let id = doc.active.ok_or(OgreError::NoActiveLayer)?;
        let layer = doc.layer(id)?;
        if !layer.is_raster() {
            return Err(OgreError::NotRaster);
        }
        if layer.locked {
            return Err(OgreError::LayerLocked(id));
        }

        let new_buffer = remove_matte(layer.buffer().unwrap(), self.tolerance).ok_or(
            OgreError::InvalidOperation("no removable background detected"),
        )?;

        let layer = doc.layer_mut(id)?;
        self.old_buffer = Some(std::mem::replace(layer.buffer_mut().unwrap(), new_buffer));
        self.layer = Some(id);
        self.prior_active = doc.active;
        doc.active = Some(id);
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let (Some(id), Some(buffer)) = (self.layer, self.old_buffer.take()) {
            if let Ok(layer) = doc.layer_mut(id) {
                if let Some(buf) = layer.buffer_mut() {
                    *buf = buffer;
                }
            }
        }
        doc.active = self.prior_active;
    }

    fn referenced_layers(&self) -> Vec<LayerId> {
        self.layer.into_iter().collect()
    }
}

/// Flood-fill (paint bucket) the active raster layer with a color.
///
/// Fills the connected region of similar-colored pixels around `seed_doc` with
/// `color`, anti-aliased and clipped to the current selection (see
/// [`crate::ops::fill_region`]). Like [`PaintCmd`] it snapshots only the touched
/// tiles so undo is proportional to the filled area.
#[derive(Debug)]
pub struct PaintBucketCmd {
    seed_doc: IVec2,
    color: Rgba32F,
    tolerance: f32,
    layer: Option<LayerId>,
    snapshots: Vec<(TileCoord, Option<Arc<Tile>>)>,
    prior_active: Option<LayerId>,
}

impl PaintBucketCmd {
    /// Create a paint-bucket command seeded at document point `seed_doc`.
    pub fn new(seed_doc: IVec2, color: Rgba32F, tolerance: f32) -> Self {
        Self {
            seed_doc,
            color,
            tolerance,
            layer: None,
            snapshots: Vec::new(),
            prior_active: None,
        }
    }
}

impl Command for PaintBucketCmd {
    fn label(&self) -> &'static str {
        "Paint bucket"
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        let id = doc.active.ok_or(OgreError::NoActiveLayer)?;
        {
            let layer = doc.layer(id)?;
            if !layer.is_raster() {
                return Err(OgreError::NotRaster);
            }
            if layer.locked {
                return Err(OgreError::LayerLocked(id));
            }
        }

        let offset = doc.layer(id).unwrap().offset;
        let writes = fill_region(
            doc.layer(id).unwrap().buffer().unwrap(),
            offset,
            self.seed_doc,
            self.color,
            self.tolerance,
            &doc.selection,
            doc.canvas,
        );

        let mut tiles = AHashSet::new();
        for (p, _) in &writes {
            tiles.insert(pixel_to_tile(*p).0);
        }
        let tiles: Vec<TileCoord> = tiles.into_iter().collect();

        {
            let buffer = doc.layer(id).unwrap().buffer().unwrap();
            self.snapshots = snapshot_tiles(buffer, &tiles);
        }
        {
            let buffer = doc.layer_mut(id).unwrap().buffer_mut().unwrap();
            for (p, px) in &writes {
                buffer.set_pixel(*p, *px);
            }
        }

        self.layer = Some(id);
        self.prior_active = doc.active;
        doc.active = Some(id);
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let Some(id) = self.layer {
            if let Ok(layer) = doc.layer_mut(id) {
                let buffer = layer.buffer_mut().unwrap();
                restore_tiles(buffer, &self.snapshots);
            }
        }
        doc.active = self.prior_active;
    }

    fn referenced_layers(&self) -> Vec<LayerId> {
        self.layer.into_iter().collect()
    }
}

/// Fill the active selection (or the whole canvas when none) with a solid
/// color, composited over each pixel with the selection's coverage modulating
/// the blend at edges. Snapshots only the touched tiles for undo. Errors if the
/// active layer is not raster or is locked, or if the selection is empty and the
/// canvas is degenerate.
#[derive(Debug)]
pub struct FillSelectionCmd {
    layer: Option<LayerId>,
    color: Rgba32F,
    opacity: f32,
    selection: Selection,
    snapshots: Vec<(TileCoord, Option<Arc<Tile>>)>,
    prior_active: Option<LayerId>,
}

impl FillSelectionCmd {
    /// Create a fill-selection command. `color` is in linear straight-alpha
    /// space; `opacity` is in `0.0..=1.0`. The selection is snapshotted at
    /// construction so undo is consistent even if the live selection changes.
    pub fn new(color: Rgba32F, opacity: f32, selection: Selection) -> Self {
        Self {
            layer: None,
            color,
            opacity,
            selection,
            snapshots: Vec::new(),
            prior_active: None,
        }
    }
}

impl Command for FillSelectionCmd {
    fn label(&self) -> &'static str {
        "Fill"
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        let id = doc.active.ok_or(OgreError::NoActiveLayer)?;
        {
            let layer = doc.layer(id)?;
            if !layer.is_raster() {
                return Err(OgreError::NotRaster);
            }
            if layer.locked {
                return Err(OgreError::LayerLocked(id));
            }
        }
        let offset = doc.layer(id).unwrap().offset;
        let canvas = doc.canvas;

        let writes = fill_selection(
            doc.layer(id).unwrap().buffer().unwrap(),
            offset,
            self.color,
            self.opacity,
            &self.selection,
            canvas,
        );

        let mut tiles: AHashSet<TileCoord> = AHashSet::new();
        for (p, _) in &writes {
            tiles.insert(pixel_to_tile(*p).0);
        }
        let tiles: Vec<TileCoord> = tiles.into_iter().collect();

        {
            let buffer = doc.layer(id).unwrap().buffer().unwrap();
            self.snapshots = snapshot_tiles(buffer, &tiles);
        }
        {
            let buffer = doc.layer_mut(id).unwrap().buffer_mut().unwrap();
            for (p, px) in &writes {
                buffer.set_pixel(*p, *px);
            }
        }

        self.layer = Some(id);
        self.prior_active = doc.active;
        doc.active = Some(id);
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let Some(id) = self.layer {
            if let Ok(layer) = doc.layer_mut(id) {
                let buffer = layer.buffer_mut().unwrap();
                restore_tiles(buffer, &self.snapshots);
            }
        }
        doc.active = self.prior_active;
    }

    fn referenced_layers(&self) -> Vec<LayerId> {
        self.layer.into_iter().collect()
    }
}

/// Stroke the outline of the active selection with the foreground colour (v1:
/// `Rect` selections only). Composites the stroke over each touched pixel,
/// snapshots those tiles for undo. Errors if the active layer is not raster or
/// is locked. Non-rect selections are a no-op (their boundary extraction is
/// deferred — see `stroke_selection`).
#[derive(Debug)]
pub struct StrokeSelectionCmd {
    layer: Option<LayerId>,
    color: Rgba32F,
    width: f32,
    opacity: f32,
    selection: Selection,
    snapshots: Vec<(TileCoord, Option<Arc<Tile>>)>,
    prior_active: Option<LayerId>,
}

impl StrokeSelectionCmd {
    /// Create a stroke-selection command. `width` is the stroke diameter in
    /// pixels; `opacity` is in `0.0..=1.0`.
    pub fn new(color: Rgba32F, width: f32, opacity: f32, selection: Selection) -> Self {
        Self {
            layer: None,
            color,
            width,
            opacity,
            selection,
            snapshots: Vec::new(),
            prior_active: None,
        }
    }
}

impl Command for StrokeSelectionCmd {
    fn label(&self) -> &'static str {
        "Stroke"
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        let id = doc.active.ok_or(OgreError::NoActiveLayer)?;
        {
            let layer = doc.layer(id)?;
            if !layer.is_raster() {
                return Err(OgreError::NotRaster);
            }
            if layer.locked {
                return Err(OgreError::LayerLocked(id));
            }
        }
        let offset = doc.layer(id).unwrap().offset;
        let canvas = doc.canvas;

        let writes = stroke_selection(
            doc.layer(id).unwrap().buffer().unwrap(),
            offset,
            self.color,
            self.width,
            self.opacity,
            &self.selection,
            canvas,
        );

        let mut tiles: AHashSet<TileCoord> = AHashSet::new();
        for (p, _) in &writes {
            tiles.insert(pixel_to_tile(*p).0);
        }
        let tiles: Vec<TileCoord> = tiles.into_iter().collect();

        if !tiles.is_empty() {
            let buffer = doc.layer(id).unwrap().buffer().unwrap();
            self.snapshots = snapshot_tiles(buffer, &tiles);
            let buffer = doc.layer_mut(id).unwrap().buffer_mut().unwrap();
            for (p, px) in &writes {
                buffer.set_pixel(*p, *px);
            }
        }

        self.layer = Some(id);
        self.prior_active = doc.active;
        doc.active = Some(id);
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let Some(id) = self.layer {
            if let Ok(layer) = doc.layer_mut(id) {
                let buffer = layer.buffer_mut().unwrap();
                restore_tiles(buffer, &self.snapshots);
            }
        }
        doc.active = self.prior_active;
    }

    fn referenced_layers(&self) -> Vec<LayerId> {
        self.layer.into_iter().collect()
    }
}

/// Merge the active layer down into the raster layer below it.
///
/// Unlike the raw [`merge_down`](crate::ops::merge_down) op (which hard-removes
/// the upper layer, making it non-undoable), this snapshots the lower layer and
/// soft-removes the upper so undo restores both. Errors if there is no layer
/// below, either layer is not raster/locked, or the lower layer's blend is not
/// Normal.
#[derive(Debug)]
pub struct MergeDownCmd {
    upper: LayerId,
    lower: Option<LayerId>,
    old_lower: Option<crate::layer::Layer>,
    upper_parent: Option<LayerId>,
    upper_index: usize,
    removed_ids: Vec<LayerId>,
    prior_active: Option<LayerId>,
}

impl MergeDownCmd {
    /// Create a merge-down command for `upper` (merged into the layer below).
    pub fn new(upper: LayerId) -> Self {
        Self {
            upper,
            lower: None,
            old_lower: None,
            upper_parent: None,
            upper_index: 0,
            removed_ids: Vec::new(),
            prior_active: None,
        }
    }
}

impl Command for MergeDownCmd {
    fn label(&self) -> &'static str {
        "Merge Down"
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        let upper_layer = doc.layer(self.upper)?;
        if !upper_layer.is_raster() {
            return Err(OgreError::NotRaster);
        }
        if upper_layer.locked {
            return Err(OgreError::LayerLocked(self.upper));
        }
        let (parent, upper_index) = doc
            .sibling_index(self.upper)
            .ok_or(OgreError::LayerNotFound(self.upper))?;
        if upper_index == 0 {
            return Err(OgreError::InvalidOperation("no layer below the target"));
        }
        let lower = doc
            .siblings(self.upper)
            .and_then(|list| list.get(upper_index - 1).copied())
            .ok_or(OgreError::InvalidOperation("no layer below the target"))?;

        let merged_buffer = {
            let lower_layer = doc.layer(lower)?;
            let upper_layer = doc.layer(self.upper)?;
            crate::ops::layer_ops::merge_raster_layers(lower_layer, upper_layer)?
        };

        // Snapshot the pre-merge lower layer for undo (full clone; tile data is
        // Arc-shared so this is cheap).
        let old_lower = doc.layer(lower).unwrap().clone();
        let lower_name = old_lower.name.clone();
        let mut merged = crate::layer::Layer::new_raster(lower_name);
        merged.id = lower;
        merged.offset = IVec2::ZERO;
        merged.content = crate::layer::LayerContent::Raster {
            buffer: merged_buffer,
            mask: None,
        };
        *doc.layer_mut(lower).unwrap() = merged;

        // Soft-remove the upper so undo can restore it with its id intact.
        let removed_ids = doc.remove_layer_soft(self.upper)?;
        if doc.active == Some(self.upper) {
            doc.active = Some(lower);
        }

        self.lower = Some(lower);
        self.old_lower = Some(old_lower);
        self.upper_parent = parent;
        self.upper_index = upper_index;
        self.removed_ids = removed_ids;
        self.prior_active = doc.active;
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let (Some(lower), Some(old_lower)) = (self.lower, self.old_lower.clone()) {
            if let Ok(slot) = doc.layer_mut(lower) {
                *slot = old_lower;
            }
            doc.restore_layer_soft(
                self.upper_parent,
                self.upper_index,
                self.upper,
                &self.removed_ids,
            );
        }
        doc.active = self.prior_active;
    }

    fn referenced_layers(&self) -> Vec<LayerId> {
        let mut ids = vec![self.upper];
        if let Some(lower) = self.lower {
            ids.push(lower);
        }
        ids
    }
}

/// Flip (mirror) a raster layer exactly — buffer and mask — across its content
/// centre. Unlike [`TransformLayerCmd`] (bicubic resample, which can mis-center
/// a discrete mirror), this writes each pixel to its exact mirrored coordinate.
/// The layer's offset is unchanged (the doc-space bounding box is preserved).
/// Errors if the layer is not raster or is locked.
#[derive(Debug)]
pub struct FlipLayerCmd {
    layer: LayerId,
    axis: crate::ops::FlipAxis,
    old_buffer: Option<TiledBuffer>,
    old_mask: Option<TiledBuffer>,
    prior_active: Option<LayerId>,
}

impl FlipLayerCmd {
    /// Create a flip command for `layer` across `axis`.
    pub fn new(layer: LayerId, axis: crate::ops::FlipAxis) -> Self {
        Self {
            layer,
            axis,
            old_buffer: None,
            old_mask: None,
            prior_active: None,
        }
    }
}

impl Command for FlipLayerCmd {
    fn label(&self) -> &'static str {
        match self.axis {
            crate::ops::FlipAxis::Horizontal => "Flip layer horizontal",
            crate::ops::FlipAxis::Vertical => "Flip layer vertical",
        }
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        {
            let layer = doc.layer(self.layer)?;
            if !layer.is_raster() {
                return Err(OgreError::NotRaster);
            }
            if layer.locked {
                return Err(OgreError::LayerLocked(self.layer));
            }
        }
        // Mirror axis = the buffer's content bounds, shared with the mask so the
        // two stay aligned. An empty buffer is a no-op (old state preserved).
        let axis_bounds = match doc
            .layer(self.layer)?
            .buffer()
            .and_then(|b| b.exact_bounds())
        {
            Some(b) => b,
            None => {
                self.prior_active = doc.active;
                doc.active = Some(self.layer);
                return Ok(());
            }
        };
        let layer = doc.layer_mut(self.layer)?;
        self.old_buffer = Some(layer.buffer().expect("checked raster above").clone());
        self.old_mask = layer.mask().cloned();
        let axis = self.axis;
        let new_buffer = crate::ops::flip_buffer(
            layer.buffer().expect("checked raster above"),
            axis,
            axis_bounds,
        );
        if let Some(buffer) = layer.buffer_mut() {
            *buffer = new_buffer;
        }
        if let Some(mask) = layer.mask_mut() {
            *mask = crate::ops::flip_buffer(mask, axis, axis_bounds);
        }
        self.prior_active = doc.active;
        doc.active = Some(self.layer);
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let Ok(layer) = doc.layer_mut(self.layer) {
            if let (Some(old_buf), old_mask) = (self.old_buffer.clone(), self.old_mask.clone()) {
                if let Some(buffer) = layer.buffer_mut() {
                    *buffer = old_buf;
                }
                layer.set_mask(old_mask);
            }
        }
        doc.active = self.prior_active;
    }

    fn referenced_layers(&self) -> Vec<LayerId> {
        vec![self.layer]
    }
}

/// raster layer (§3.2.1). The selection at construction time is snapshotted so
/// undo is consistent even if the live selection later changes. Like
/// [`PaintBucketCmd`] it snapshots only the touched tiles for undo.
#[derive(Debug)]
pub struct GradientFillCmd {
    layer: LayerId,
    p0_doc: Vec2,
    p1_doc: Vec2,
    kind: GradientKind,
    fg: Rgba32F,
    bg: Rgba32F,
    reverse: bool,
    opacity: f32,
    wrap: WrapMode,
    selection: Selection,
    snapshots: Vec<(TileCoord, Option<Arc<Tile>>)>,
    prior_active: Option<LayerId>,
}

impl GradientFillCmd {
    /// Create a gradient-fill command. `p0_doc`/`p1_doc` are document-space
    /// endpoints; `selection` is the snapshot to clip against.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        layer: LayerId,
        p0_doc: Vec2,
        p1_doc: Vec2,
        kind: GradientKind,
        fg: Rgba32F,
        bg: Rgba32F,
        reverse: bool,
        opacity: f32,
        wrap: WrapMode,
        selection: Selection,
    ) -> Self {
        Self {
            layer,
            p0_doc,
            p1_doc,
            kind,
            fg,
            bg,
            reverse,
            opacity,
            wrap,
            selection,
            snapshots: Vec::new(),
            prior_active: None,
        }
    }
}

impl Command for GradientFillCmd {
    fn label(&self) -> &'static str {
        "Gradient fill"
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        let layer = doc.layer(self.layer)?;
        if !layer.is_raster() {
            return Err(OgreError::NotRaster);
        }
        if layer.locked {
            return Err(OgreError::LayerLocked(self.layer));
        }
        let offset = layer.offset;
        let canvas = doc.canvas;

        let writes = fill_gradient(
            layer.buffer().unwrap(),
            offset,
            self.p0_doc,
            self.p1_doc,
            self.kind,
            self.fg,
            self.bg,
            self.reverse,
            self.opacity,
            &self.selection,
            canvas,
            self.wrap,
        );

        let mut tiles: AHashSet<TileCoord> = AHashSet::new();
        for (p, _) in &writes {
            tiles.insert(pixel_to_tile(*p).0);
        }
        let tiles: Vec<TileCoord> = tiles.into_iter().collect();

        {
            let buffer = doc.layer(self.layer).unwrap().buffer().unwrap();
            self.snapshots = snapshot_tiles(buffer, &tiles);
        }
        {
            let buffer = doc.layer_mut(self.layer).unwrap().buffer_mut().unwrap();
            for (p, px) in &writes {
                buffer.set_pixel(*p, *px);
            }
        }

        self.prior_active = doc.active;
        doc.active = Some(self.layer);
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let Ok(layer) = doc.layer_mut(self.layer) {
            if let Some(buffer) = layer.buffer_mut() {
                restore_tiles(buffer, &self.snapshots);
            }
        }
        doc.active = self.prior_active;
    }

    fn referenced_layers(&self) -> Vec<LayerId> {
        vec![self.layer]
    }
}

/// Replace a raster layer's entire pixel buffer with a precomputed one.
///
/// This is the apply-step for edits whose expensive computation runs off the UI
/// thread (e.g. background removal): the worker produces the new buffer, and
/// this command swaps it in undoably. `label` lets the caller name the history
/// entry. The previous buffer is stored for undo; both buffers share tile
/// `Arc`s, so storing and cloning them is cheap.
#[derive(Debug)]
pub struct SetLayerBufferCmd {
    layer: LayerId,
    new_buffer: TiledBuffer,
    label: &'static str,
    old_buffer: Option<TiledBuffer>,
    prior_active: Option<LayerId>,
}

impl SetLayerBufferCmd {
    /// Create a command that replaces `layer`'s buffer with `new_buffer`,
    /// recording the history entry under `label`.
    pub fn new(layer: LayerId, new_buffer: TiledBuffer, label: &'static str) -> Self {
        Self {
            layer,
            new_buffer,
            label,
            old_buffer: None,
            prior_active: None,
        }
    }
}

impl Command for SetLayerBufferCmd {
    fn label(&self) -> &'static str {
        self.label
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        {
            let layer = doc.layer(self.layer)?;
            if !layer.is_raster() {
                return Err(OgreError::NotRaster);
            }
            if layer.locked {
                return Err(OgreError::LayerLocked(self.layer));
            }
        }
        let layer = doc.layer_mut(self.layer)?;
        let buffer = layer.buffer_mut().unwrap();
        // Clone is Arc-cheap per tile; cloning on each apply keeps the command
        // re-runnable on redo.
        self.old_buffer = Some(std::mem::replace(buffer, self.new_buffer.clone()));
        self.prior_active = doc.active;
        doc.active = Some(self.layer);
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let Some(old) = self.old_buffer.take() {
            if let Ok(layer) = doc.layer_mut(self.layer) {
                if let Some(buffer) = layer.buffer_mut() {
                    *buffer = old;
                }
            }
        }
        doc.active = self.prior_active;
    }

    fn referenced_layers(&self) -> Vec<LayerId> {
        vec![self.layer]
    }
}

/// Add a layer mask to a raster layer.
///
/// The mask is built from [`MaskInit`] (reveal-all / hide-all / reveal-selection)
/// and stored on the layer. Undo removes it; redo restores the same mask. Errors
/// if the layer is not raster, is locked, or already has a mask (use
/// [`DeleteLayerMaskCmd`] or [`SetLayerMaskCmd`] to replace an existing mask).
#[derive(Debug)]
pub struct AddLayerMaskCmd {
    layer: LayerId,
    init: MaskInit,
    /// Built on first apply; reused on redo so the mask is stable across
    /// undo/redo even if the layer buffer later changes.
    built: Option<TiledBuffer>,
    prior_active: Option<LayerId>,
}

impl AddLayerMaskCmd {
    /// Create a command that adds a mask initialised from `init` to `layer`.
    pub fn new(layer: LayerId, init: MaskInit) -> Self {
        Self {
            layer,
            init,
            built: None,
            prior_active: None,
        }
    }
}

impl Command for AddLayerMaskCmd {
    fn label(&self) -> &'static str {
        "Add layer mask"
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        let (offset, has_mask) = {
            let layer = doc.layer(self.layer)?;
            if !layer.is_raster() {
                return Err(OgreError::NotRaster);
            }
            if layer.locked {
                return Err(OgreError::LayerLocked(self.layer));
            }
            (layer.offset, layer.mask().is_some())
        };
        if has_mask {
            return Err(OgreError::InvalidOperation("layer already has a mask"));
        }
        let mask = if let Some(m) = &self.built {
            m.clone()
        } else {
            let buffer = doc
                .layer(self.layer)?
                .buffer()
                .expect("checked raster above");
            let m = self.init.build(buffer, offset);
            self.built = Some(m.clone());
            m
        };
        doc.layer_mut(self.layer)?.set_mask(Some(mask));
        self.prior_active = doc.active;
        doc.active = Some(self.layer);
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let Ok(layer) = doc.layer_mut(self.layer) {
            layer.set_mask(None);
        }
        doc.active = self.prior_active;
    }

    fn referenced_layers(&self) -> Vec<LayerId> {
        vec![self.layer]
    }
}

/// Delete (detach) a layer mask, discarding its effect.
///
/// Stores the removed mask so undo restores it exactly. Errors if the layer is
/// not raster, is locked, or has no mask.
#[derive(Debug)]
pub struct DeleteLayerMaskCmd {
    layer: LayerId,
    removed_mask: Option<TiledBuffer>,
    prior_active: Option<LayerId>,
}

impl DeleteLayerMaskCmd {
    /// Create a command that removes the mask from `layer`.
    pub fn new(layer: LayerId) -> Self {
        Self {
            layer,
            removed_mask: None,
            prior_active: None,
        }
    }
}

impl Command for DeleteLayerMaskCmd {
    fn label(&self) -> &'static str {
        "Delete layer mask"
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        {
            let layer = doc.layer(self.layer)?;
            if !layer.is_raster() {
                return Err(OgreError::NotRaster);
            }
            if layer.locked {
                return Err(OgreError::LayerLocked(self.layer));
            }
            if layer.mask().is_none() {
                return Err(OgreError::InvalidOperation("layer has no mask to delete"));
            }
        }
        // Arc-cheap clone for undo; the live mask is then dropped.
        let removed = doc.layer_mut(self.layer)?.mask().cloned();
        doc.layer_mut(self.layer)?.set_mask(None);
        self.removed_mask = removed;
        self.prior_active = doc.active;
        doc.active = Some(self.layer);
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let Some(mask) = self.removed_mask.clone() {
            if let Ok(layer) = doc.layer_mut(self.layer) {
                layer.set_mask(Some(mask));
            }
        }
        doc.active = self.prior_active;
    }

    fn referenced_layers(&self) -> Vec<LayerId> {
        vec![self.layer]
    }
}

/// Apply (bake) the layer mask into the raster buffer, then discard the mask.
///
/// Each pixel's alpha is multiplied by the mask coverage at its layer-local
/// position; the mask is then removed. Errors if the layer is not raster, is
/// locked, or has no mask.
#[derive(Debug)]
pub struct ApplyLayerMaskCmd {
    layer: LayerId,
    old_buffer: Option<TiledBuffer>,
    old_mask: Option<TiledBuffer>,
    prior_active: Option<LayerId>,
}

impl ApplyLayerMaskCmd {
    /// Create a command that bakes `layer`'s mask into its buffer.
    pub fn new(layer: LayerId) -> Self {
        Self {
            layer,
            old_buffer: None,
            old_mask: None,
            prior_active: None,
        }
    }
}

impl Command for ApplyLayerMaskCmd {
    fn label(&self) -> &'static str {
        "Apply layer mask"
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        {
            let layer = doc.layer(self.layer)?;
            if !layer.is_raster() {
                return Err(OgreError::NotRaster);
            }
            if layer.locked {
                return Err(OgreError::LayerLocked(self.layer));
            }
            if layer.mask().is_none() {
                return Err(OgreError::InvalidOperation("layer has no mask to apply"));
            }
        }
        let layer = doc.layer_mut(self.layer)?;
        // Snapshot the pre-bake state for undo. Tiles are Arc-shared (COW), so
        // cloning is cheap and the snapshot's tiles stay intact when the bake
        // copy-on-writes them below.
        self.old_buffer = Some(layer.buffer().expect("checked raster above").clone());
        self.old_mask = layer.mask().cloned();
        let mask = self.old_mask.as_ref().expect("checked mask above").clone();
        let live_buffer = layer.buffer_mut().expect("checked raster above");
        apply_mask_to_buffer(live_buffer, &mask);
        layer.set_mask(None);
        self.prior_active = doc.active;
        doc.active = Some(self.layer);
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let Ok(layer) = doc.layer_mut(self.layer) {
            if let (Some(old_buf), Some(old_mask)) =
                (self.old_buffer.clone(), self.old_mask.clone())
            {
                if let Some(buffer) = layer.buffer_mut() {
                    *buffer = old_buf;
                }
                layer.set_mask(Some(old_mask));
            }
        }
        doc.active = self.prior_active;
    }

    fn referenced_layers(&self) -> Vec<LayerId> {
        vec![self.layer]
    }
}

/// Swap a layer's entire mask buffer.
///
/// This is the low-level edit primitive for mask painting: a caller (e.g. a
/// future brush-on-mask path) builds the new mask buffer off-thread and
/// dispatches this command to install it undoably. Replaces whatever mask
/// exists (including `None`). Errors if the layer is not raster or is locked.
#[derive(Debug)]
pub struct SetLayerMaskCmd {
    layer: LayerId,
    new_mask: Option<TiledBuffer>,
    old_mask: Option<TiledBuffer>,
    prior_active: Option<LayerId>,
}

impl SetLayerMaskCmd {
    /// Create a command that replaces `layer`'s mask with `new_mask`.
    pub fn new(layer: LayerId, new_mask: Option<TiledBuffer>) -> Self {
        Self {
            layer,
            new_mask,
            old_mask: None,
            prior_active: None,
        }
    }
}

impl Command for SetLayerMaskCmd {
    fn label(&self) -> &'static str {
        "Set layer mask"
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        {
            let layer = doc.layer(self.layer)?;
            if !layer.is_raster() {
                return Err(OgreError::NotRaster);
            }
            if layer.locked {
                return Err(OgreError::LayerLocked(self.layer));
            }
        }
        let layer = doc.layer_mut(self.layer)?;
        // Capture the existing mask (Arc-cheap) for undo, then install the new.
        self.old_mask = layer.mask().cloned();
        layer.set_mask(self.new_mask.clone());
        self.prior_active = doc.active;
        doc.active = Some(self.layer);
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let Ok(layer) = doc.layer_mut(self.layer) {
            layer.set_mask(self.old_mask.clone());
        }
        doc.active = self.prior_active;
    }

    fn referenced_layers(&self) -> Vec<LayerId> {
        vec![self.layer]
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::commands::AddRasterLayerCmd;
    use crate::coord::Rect;
    use crate::selection::Selection;
    use crate::Layer;

    // ------------------------------------------------------------------
    // BrushStrokeCmd
    // ------------------------------------------------------------------

    #[test]
    fn brush_stroke_cmd_writes_edits_and_round_trips_byte_identical() {
        let mut doc = Document::new(16, 16);
        let layer = doc.add_raster_layer("L");
        // Seed two pixels so undo has something to restore.
        {
            let buf = doc.layer_mut(layer).unwrap().buffer_mut().unwrap();
            buf.set_pixel(IVec2::new(0, 0), Rgba32F::new(0.0, 0.0, 0.0, 1.0));
            buf.set_pixel(IVec2::new(1, 0), Rgba32F::new(0.0, 0.0, 0.0, 1.0));
        }
        let before = doc.layer(layer).unwrap().buffer().unwrap().clone();

        let edits = vec![
            (IVec2::new(0, 0), Rgba32F::new(1.0, 0.0, 0.0, 1.0)),
            (IVec2::new(1, 0), Rgba32F::new(0.0, 1.0, 0.0, 1.0)),
        ];
        let mut cmd = BrushStrokeCmd::new(layer, edits);
        cmd.apply(&mut doc).unwrap();
        assert_eq!(
            doc.layer(layer)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(0, 0)),
            Rgba32F::new(1.0, 0.0, 0.0, 1.0)
        );
        assert_eq!(
            doc.layer(layer)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(1, 0)),
            Rgba32F::new(0.0, 1.0, 0.0, 1.0)
        );
        // Outside the edits: unchanged.
        assert_eq!(
            doc.layer(layer)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(5, 5)),
            Rgba32F::TRANSPARENT,
        );

        // Undo restores the touched tiles byte-for-byte.
        cmd.undo(&mut doc);
        assert_eq!(doc.layer(layer).unwrap().buffer().unwrap(), &before);

        // Redo is byte-identical to the first apply.
        cmd.apply(&mut doc).unwrap();
        assert_eq!(
            doc.layer(layer)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(0, 0)),
            Rgba32F::new(1.0, 0.0, 0.0, 1.0)
        );
    }

    #[test]
    fn brush_stroke_cmd_empty_edits_and_non_raster_rejection() {
        let mut doc = Document::new(8, 8);
        let layer = doc.add_raster_layer("L");
        // Empty edits: no-op but sets active and snapshots nothing.
        let mut cmd = BrushStrokeCmd::new(layer, Vec::new());
        cmd.apply(&mut doc).unwrap();
        assert_eq!(doc.active, Some(layer));
        cmd.undo(&mut doc); // no-op undo is safe.

        // Non-raster (adjustment) layer → NotRaster error, no pixels touched.
        let adj = doc.add_adjustment_layer("A", crate::layer::AdjustmentKind::Invert);
        let mut bad = BrushStrokeCmd::new(adj, vec![(IVec2::new(0, 0), Rgba32F::TRANSPARENT)]);
        assert!(bad.apply(&mut doc).is_err());
    }

    #[test]
    fn brush_stroke_cmd_referenced_layers_is_target() {
        let layer = LayerId::default();
        let cmd = BrushStrokeCmd::new(layer, Vec::new());
        assert_eq!(cmd.referenced_layers(), vec![layer]);
        assert_eq!(cmd.label(), "Brush stroke");
    }

    // ------------------------------------------------------------------
    // Task 1.5.1 — Command trait
    // ------------------------------------------------------------------

    struct SetOpacityCmd {
        layer: LayerId,
        old_opacity: Option<f32>,
        new_opacity: f32,
    }

    impl SetOpacityCmd {
        fn new(layer: LayerId, opacity: f32) -> Self {
            Self {
                layer,
                old_opacity: None,
                new_opacity: opacity,
            }
        }
    }

    impl Command for SetOpacityCmd {
        fn label(&self) -> &'static str {
            "Set opacity"
        }

        fn apply(&mut self, doc: &mut Document) -> Result<()> {
            let layer = doc.layer_mut(self.layer)?;
            self.old_opacity = Some(layer.opacity);
            layer.opacity = self.new_opacity;
            Ok(())
        }

        fn undo(&mut self, doc: &mut Document) {
            if let Some(opacity) = self.old_opacity {
                if let Ok(layer) = doc.layer_mut(self.layer) {
                    layer.opacity = opacity;
                }
            }
        }
    }

    #[test]
    fn set_opacity_cmd_applies_and_undoes() {
        let mut doc = Document::new(100, 100);
        let id = doc.add_raster_layer("L");
        assert_eq!(doc.layer(id).unwrap().opacity, 1.0);

        let mut cmd = SetOpacityCmd::new(id, 0.5);
        cmd.apply(&mut doc).unwrap();
        assert_eq!(doc.layer(id).unwrap().opacity, 0.5);

        cmd.undo(&mut doc);
        assert_eq!(doc.layer(id).unwrap().opacity, 1.0);
    }

    // ------------------------------------------------------------------
    // Task 1.5.2 — History stack
    // ------------------------------------------------------------------

    #[test]
    fn history_push_undo_redo_restores_state() {
        let mut doc = Document::new(100, 100);
        let id = doc.add_raster_layer("L");

        let mut history = History::new(0);
        history
            .do_command(&mut doc, Box::new(SetOpacityCmd::new(id, 0.25)))
            .unwrap();
        history
            .do_command(&mut doc, Box::new(SetOpacityCmd::new(id, 0.75)))
            .unwrap();

        assert_eq!(doc.layer(id).unwrap().opacity, 0.75);
        history.undo(&mut doc);
        assert_eq!(doc.layer(id).unwrap().opacity, 0.25);
        history.undo(&mut doc);
        assert_eq!(doc.layer(id).unwrap().opacity, 1.0);
        history.redo(&mut doc);
        assert_eq!(doc.layer(id).unwrap().opacity, 0.25);
        history.redo(&mut doc);
        assert_eq!(doc.layer(id).unwrap().opacity, 0.75);
    }

    #[test]
    fn history_push_after_undo_clears_redo_stack() {
        let mut doc = Document::new(100, 100);
        let id = doc.add_raster_layer("L");

        let mut history = History::new(0);
        history
            .do_command(&mut doc, Box::new(SetOpacityCmd::new(id, 0.25)))
            .unwrap();
        history
            .do_command(&mut doc, Box::new(SetOpacityCmd::new(id, 0.75)))
            .unwrap();
        history.undo(&mut doc);

        history
            .do_command(&mut doc, Box::new(SetOpacityCmd::new(id, 0.5)))
            .unwrap();

        assert_eq!(history.redo_len(), 0);
        assert_eq!(doc.layer(id).unwrap().opacity, 0.5);
        history.redo(&mut doc);
        assert_eq!(doc.layer(id).unwrap().opacity, 0.5);
    }

    #[test]
    fn history_depth_limit_evicts_oldest() {
        let mut doc = Document::new(100, 100);
        let id = doc.add_raster_layer("L");

        let mut history = History::new(2);
        history
            .do_command(&mut doc, Box::new(SetOpacityCmd::new(id, 0.1)))
            .unwrap();
        history
            .do_command(&mut doc, Box::new(SetOpacityCmd::new(id, 0.2)))
            .unwrap();
        history
            .do_command(&mut doc, Box::new(SetOpacityCmd::new(id, 0.3)))
            .unwrap();

        assert_eq!(history.undo_len(), 2);
        history.undo(&mut doc);
        history.undo(&mut doc);
        // The oldest command (0.1) was evicted, so only two undos are possible
        // and we end at 0.1 (the untouched state).
        assert_eq!(doc.layer(id).unwrap().opacity, 0.1);
    }

    #[test]
    fn history_clear_drops_both_stacks() {
        let mut doc = Document::new(100, 100);
        let id = doc.add_raster_layer("L");

        let mut history = History::new(0);
        history
            .do_command(&mut doc, Box::new(SetOpacityCmd::new(id, 0.5)))
            .unwrap();
        history.undo(&mut doc);
        history.clear(&mut doc);

        assert_eq!(history.undo_len(), 0);
        assert_eq!(history.redo_len(), 0);
    }

    // ------------------------------------------------------------------
    // Task 1.5.4 — Snapshot efficiency
    // ------------------------------------------------------------------

    #[test]
    fn paint_cmd_snapshots_exactly_one_tile_and_no_deep_copy() {
        let mut doc = Document::new(100, 100);
        let layer = doc.add_raster_layer("Paint");

        // Materialise exactly one tile.
        doc.layer_mut(layer)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(0, 0), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        let pre_edit_tile = doc
            .layer(layer)
            .unwrap()
            .buffer()
            .unwrap()
            .get_tile(TileCoord { x: 0, y: 0 })
            .unwrap();

        let mut cmd = PaintCmd::new(
            layer,
            vec![(IVec2::new(1, 1), Rgba32F::new(0.0, 1.0, 0.0, 1.0))],
        );

        // Snapshotting happens during apply, not construction.
        cmd.apply(&mut doc).unwrap();

        // The command should have captured exactly one tile snapshot.
        assert_eq!(cmd.snapshots.len(), 1);
        assert_eq!(cmd.snapshots[0].0, TileCoord { x: 0, y: 0 });
        assert!(Arc::ptr_eq(
            cmd.snapshots[0].1.as_ref().unwrap(),
            &pre_edit_tile
        ));
    }

    // ------------------------------------------------------------------
    // Task 1.5.3 — Command wrappers (smoke tests)
    // ------------------------------------------------------------------

    #[test]
    fn paint_cmd_round_trips() {
        let mut doc = Document::new(100, 100);
        let layer = doc.add_raster_layer("Paint");

        let mut history = History::new(0);
        history
            .do_command(
                &mut doc,
                Box::new(PaintCmd::new(
                    layer,
                    vec![(IVec2::new(5, 5), Rgba32F::new(1.0, 0.0, 0.0, 1.0))],
                )),
            )
            .unwrap();

        assert_eq!(
            doc.layer(layer)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(5, 5)),
            Rgba32F::new(1.0, 0.0, 0.0, 1.0)
        );

        history.undo(&mut doc);
        assert_eq!(
            doc.layer(layer)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(5, 5)),
            Rgba32F::TRANSPARENT
        );
    }

    #[test]
    fn paint_stroke_cmd_round_trips() {
        use glam::Vec2;

        let mut doc = Document::new(2000, 1500);
        let layer = doc.add_raster_layer("Stroke");

        // Seed a tile that the stroke will not touch so we can verify that
        // untouched tiles keep their original Arc after undo.
        doc.layer_mut(layer)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(0, 0), Rgba32F::new(0.0, 1.0, 0.0, 1.0));

        let settings = BrushSettings {
            size: 20.0,
            hardness: 1.0,
            opacity: 1.0,
            pressure_size: false,
            pressure_opacity: false,
            ..Default::default()
        };

        let samples = vec![
            InputSample::new(Vec2::new(1000.0, 700.0)),
            InputSample::new(Vec2::new(1010.0, 700.0)),
        ];
        let red = Rgba32F::new(1.0, 0.0, 0.0, 1.0);

        let mut history = History::new(0);
        history
            .do_command(
                &mut doc,
                Box::new(PaintStrokeCmd::new(
                    layer,
                    samples,
                    settings,
                    red,
                    PaintMode::Brush,
                )),
            )
            .unwrap();

        assert!(
            doc.layer(layer)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(1000, 700))
                .a
                > 0.0
        );

        let before_undo = doc.layer(layer).unwrap().buffer().unwrap().clone();
        history.undo(&mut doc);
        assert_eq!(
            doc.layer(layer)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(1000, 700)),
            Rgba32F::TRANSPARENT
        );

        // Tiles that were never touched must still share the original Arc.
        let tile = TileCoord { x: 0, y: 0 };
        assert!(Arc::ptr_eq(
            &before_undo.get_tile(tile).unwrap(),
            &doc.layer(layer)
                .unwrap()
                .buffer()
                .unwrap()
                .get_tile(tile)
                .unwrap()
        ));

        // The touched tile was restored to its pre-stroke state.
        let touched = TileCoord { x: 3, y: 2 };
        assert_eq!(
            doc.layer(layer)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(1000, 700)),
            Rgba32F::TRANSPARENT
        );
        assert!(doc
            .layer(layer)
            .unwrap()
            .buffer()
            .unwrap()
            .get_tile(touched)
            .is_none());
    }

    #[test]
    fn copy_to_new_layer_cmd_round_trips() {
        let mut doc = Document::new(2000, 1500);
        let src = doc.add_raster_layer("src");
        let target = IVec2::new(1000, 700);
        doc.layer_mut(src)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(target, Rgba32F::new(0.2, 0.4, 0.9, 1.0));

        let mut history = History::new(0);
        let sel = Selection::rect(Rect::new(990, 690, 40, 40));
        history
            .do_command(&mut doc, Box::new(CopyToNewLayerCmd::new(src, sel)))
            .unwrap();

        let new_id = doc.active.unwrap();
        assert_eq!(
            doc.layer(new_id)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(target),
            Rgba32F::new(0.2, 0.4, 0.9, 1.0)
        );

        history.undo(&mut doc);
        assert!(matches!(
            doc.layer(new_id),
            Err(OgreError::LayerNotFound(_))
        ));
        assert_eq!(doc.active, Some(src));
    }

    #[test]
    fn cut_to_new_layer_cmd_round_trips() {
        let mut doc = Document::new(2000, 1500);
        let src = doc.add_raster_layer("src");
        let target = IVec2::new(1000, 700);
        doc.layer_mut(src)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(target, Rgba32F::new(0.2, 0.4, 0.9, 1.0));

        let mut history = History::new(0);
        let sel = Selection::rect(Rect::new(990, 690, 40, 40));
        history
            .do_command(&mut doc, Box::new(CutToNewLayerCmd::new(src, sel)))
            .unwrap();

        let new_id = doc.active.unwrap();
        assert_eq!(
            doc.layer(new_id)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(target),
            Rgba32F::new(0.2, 0.4, 0.9, 1.0)
        );
        assert_eq!(
            doc.layer(src).unwrap().buffer().unwrap().get_pixel(target),
            Rgba32F::TRANSPARENT
        );

        history.undo(&mut doc);
        // The cut command now soft-deletes the new layer so later commands that
        // reference it (e.g. MoveLayerByCmd) remain valid across undo/redo.
        assert!(doc.layer(new_id).is_ok());
        assert!(doc.removed.contains(&new_id));
        assert!(!doc.order.contains(&new_id));
        assert_eq!(
            doc.layer(src).unwrap().buffer().unwrap().get_pixel(target),
            Rgba32F::new(0.2, 0.4, 0.9, 1.0)
        );
    }

    #[test]
    fn delete_layer_cmd_round_trips() {
        let mut doc = Document::new(100, 100);
        let a = doc.add_raster_layer("A");
        let b = doc.add_raster_layer("B");

        let mut history = History::new(0);
        history
            .do_command(&mut doc, Box::new(DeleteLayerCmd::new(a)))
            .unwrap();

        // Soft delete keeps the layer in the arena but marks it removed and
        // takes it out of the render tree.
        assert!(doc.layer(a).is_ok());
        assert!(doc.removed.contains(&a));
        assert_eq!(doc.order, vec![b]);

        history.undo(&mut doc);
        assert!(!doc.removed.contains(&a));
        assert_eq!(doc.layer(a).unwrap().name, "A");
        assert_eq!(doc.order, vec![a, b]);
    }

    #[test]
    fn delete_after_paint_undoes_with_stable_id() {
        let mut doc = Document::new(100, 100);
        let a = doc.add_raster_layer("A");

        let mut history = History::new(0);
        history
            .do_command(
                &mut doc,
                Box::new(PaintCmd::new(
                    a,
                    vec![(IVec2::new(5, 5), Rgba32F::new(1.0, 0.0, 0.0, 1.0))],
                )),
            )
            .unwrap();
        history
            .do_command(&mut doc, Box::new(DeleteLayerCmd::new(a)))
            .unwrap();

        // Undo delete first, then paint; the paint command must still find
        // layer `a` because the soft delete preserved its id.
        history.undo(&mut doc);
        history.undo(&mut doc);
        assert_eq!(
            doc.layer(a)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(5, 5)),
            Rgba32F::TRANSPARENT
        );
    }

    #[test]
    fn reorder_cmd_round_trips() {
        let mut doc = Document::new(100, 100);
        let a = doc.add_raster_layer("A");
        let b = doc.add_raster_layer("B");
        let c = doc.add_raster_layer("C");

        let mut history = History::new(0);
        history
            .do_command(&mut doc, Box::new(ReorderCmd::new(a, 2)))
            .unwrap();

        assert_eq!(doc.order, vec![b, c, a]);

        history.undo(&mut doc);
        assert_eq!(doc.order, vec![a, b, c]);
    }

    // ------------------------------------------------------------------
    // Redo-after-undo for layer commands
    // ------------------------------------------------------------------

    #[test]
    fn delete_layer_cmd_redo_after_undo() {
        let mut doc = Document::new(100, 100);
        let a = doc.add_raster_layer("A");
        let b = doc.add_raster_layer("B");

        let mut history = History::new(0);
        history
            .do_command(&mut doc, Box::new(DeleteLayerCmd::new(a)))
            .unwrap();

        history.undo(&mut doc);
        assert!(!doc.removed.contains(&a));
        assert_eq!(doc.order, vec![a, b]);

        history.redo(&mut doc);
        assert!(doc.removed.contains(&a));
        assert_eq!(doc.order, vec![b]);
    }

    #[test]
    fn copy_to_new_layer_cmd_redo_after_undo() {
        let mut doc = Document::new(2000, 1500);
        let src = doc.add_raster_layer("src");
        let target = IVec2::new(1000, 700);
        doc.layer_mut(src)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(target, Rgba32F::new(0.2, 0.4, 0.9, 1.0));

        let mut history = History::new(0);
        let sel = Selection::rect(Rect::new(990, 690, 40, 40));
        history
            .do_command(&mut doc, Box::new(CopyToNewLayerCmd::new(src, sel)))
            .unwrap();

        let first_id = doc.active.unwrap();
        history.undo(&mut doc);
        assert!(matches!(
            doc.layer(first_id),
            Err(OgreError::LayerNotFound(_))
        ));

        history.redo(&mut doc);
        let second_id = doc.active.unwrap();
        assert_eq!(
            doc.layer(second_id)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(target),
            Rgba32F::new(0.2, 0.4, 0.9, 1.0)
        );
    }

    #[test]
    fn cut_to_new_layer_cmd_redo_after_undo() {
        let mut doc = Document::new(2000, 1500);
        let src = doc.add_raster_layer("src");
        let target = IVec2::new(1000, 700);
        doc.layer_mut(src)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(target, Rgba32F::new(0.2, 0.4, 0.9, 1.0));

        let mut history = History::new(0);
        let sel = Selection::rect(Rect::new(990, 690, 40, 40));
        history
            .do_command(&mut doc, Box::new(CutToNewLayerCmd::new(src, sel)))
            .unwrap();

        let first_id = doc.active.unwrap();
        history.undo(&mut doc);
        assert!(doc.layer(first_id).is_ok());
        assert!(doc.removed.contains(&first_id));
        assert_eq!(
            doc.layer(src).unwrap().buffer().unwrap().get_pixel(target),
            Rgba32F::new(0.2, 0.4, 0.9, 1.0)
        );

        history.redo(&mut doc);
        let second_id = doc.active.unwrap();
        assert_eq!(second_id, first_id, "redo must restore the same layer id");
        assert_eq!(
            doc.layer(second_id)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(target),
            Rgba32F::new(0.2, 0.4, 0.9, 1.0)
        );
        assert_eq!(
            doc.layer(src).unwrap().buffer().unwrap().get_pixel(target),
            Rgba32F::TRANSPARENT
        );
    }

    #[test]
    fn reorder_cmd_redo_after_undo() {
        let mut doc = Document::new(100, 100);
        let a = doc.add_raster_layer("A");
        let b = doc.add_raster_layer("B");
        let c = doc.add_raster_layer("C");

        let mut history = History::new(0);
        history
            .do_command(&mut doc, Box::new(ReorderCmd::new(a, 2)))
            .unwrap();

        history.undo(&mut doc);
        assert_eq!(doc.order, vec![a, b, c]);

        history.redo(&mut doc);
        assert_eq!(doc.order, vec![b, c, a]);
    }

    #[test]
    fn evicted_add_does_not_hard_remove_layer_referenced_by_later_delete() {
        let mut doc = Document::new(100, 100);
        let _a = doc.add_raster_layer("A");
        let b = doc.add_raster_layer("B");

        let mut history = History::new(2);
        history
            .do_command(&mut doc, Box::new(AddRasterLayerCmd::new("A")))
            .unwrap();
        let added = doc.active.unwrap();
        history
            .do_command(&mut doc, Box::new(DeleteLayerCmd::new(added)))
            .unwrap();

        // Pushing a third command evicts the add. The delete still references
        // the soft-removed layer, so cleanup must be skipped.
        history
            .do_command(&mut doc, Box::new(SetOpacityCmd::new(b, 0.5)))
            .unwrap();

        history.undo(&mut doc); // undo set opacity
        history.undo(&mut doc); // undo delete -> layer restored
        assert!(doc.order.contains(&added));
        assert!(!doc.removed.contains(&added));
    }

    #[test]
    fn history_depth_evicts_delete_and_reclaims_soft_deleted_layers() {
        let mut doc = Document::new(100, 100);
        let a = doc.add_raster_layer("A");
        let b = doc.add_raster_layer("B");

        let mut history = History::new(2);
        // Fill the stack with cheap commands.
        history
            .do_command(&mut doc, Box::new(SetOpacityCmd::new(a, 0.5)))
            .unwrap();
        history
            .do_command(&mut doc, Box::new(SetOpacityCmd::new(b, 0.5)))
            .unwrap();
        // Pushing the delete evicts the oldest opacity command.
        history
            .do_command(&mut doc, Box::new(DeleteLayerCmd::new(a)))
            .unwrap();

        // Two more commands push the soft delete off the undo stack; its
        // cleanup hook hard-removes the reclaimed layer ids.
        history
            .do_command(&mut doc, Box::new(SetOpacityCmd::new(b, 0.25)))
            .unwrap();
        history
            .do_command(&mut doc, Box::new(SetOpacityCmd::new(b, 0.75)))
            .unwrap();

        assert!(matches!(doc.layer(a), Err(OgreError::LayerNotFound(_))));
        assert!(doc.order.contains(&b));
    }

    #[test]
    fn paint_then_delete_then_undo_both_orders() {
        let mut doc = Document::new(100, 100);
        let a = doc.add_raster_layer("A");

        let mut history = History::new(0);
        history
            .do_command(
                &mut doc,
                Box::new(PaintCmd::new(
                    a,
                    vec![(IVec2::new(5, 5), Rgba32F::new(1.0, 0.0, 0.0, 1.0))],
                )),
            )
            .unwrap();
        history
            .do_command(&mut doc, Box::new(DeleteLayerCmd::new(a)))
            .unwrap();

        // Undo delete first.
        history.undo(&mut doc);
        history.undo(&mut doc);
        assert_eq!(
            doc.layer(a)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(5, 5)),
            Rgba32F::TRANSPARENT
        );

        // Redo paint then delete.
        history.redo(&mut doc);
        history.redo(&mut doc);
        assert!(doc.removed.contains(&a));

        // Undo paint first (while layer is deleted) then delete.
        history.undo(&mut doc);
        history.undo(&mut doc);
        assert_eq!(
            doc.layer(a)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(5, 5)),
            Rgba32F::TRANSPARENT
        );
    }

    #[test]
    fn move_layer_by_cmd_rejects_overflowing_offset() {
        let mut doc = Document::new(100, 100);
        let id = doc.add_raster_layer("L");
        doc.layer_mut(id).unwrap().offset = IVec2::new(i32::MAX, 0);

        let mut cmd = MoveLayerByCmd::new(id, IVec2::new(1, 0));
        assert!(matches!(
            cmd.apply(&mut doc),
            Err(OgreError::InvalidOperation("layer offset out of range"))
        ));
        assert_eq!(doc.layer(id).unwrap().offset, IVec2::new(i32::MAX, 0));
    }

    #[test]
    fn set_selection_cmd_round_trips() {
        let mut doc = Document::new(100, 100);
        let mut history = History::new(0);
        let r = Rect::new(10, 10, 50, 50);

        history
            .do_command(&mut doc, Box::new(SetSelectionCmd::new(Selection::rect(r))))
            .unwrap();
        assert_eq!(doc.selection, Selection::rect(r));

        history.undo(&mut doc);
        assert!(doc.selection.is_empty());

        history.redo(&mut doc);
        assert_eq!(doc.selection, Selection::rect(r));
    }

    #[test]
    fn set_selection_cmd_add_mode_combines_and_restores() {
        let mut doc = Document::new(100, 100);
        let mut history = History::new(0);
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(5, 5, 10, 10);

        history
            .do_command(&mut doc, Box::new(SetSelectionCmd::new(Selection::rect(a))))
            .unwrap();
        history
            .do_command(
                &mut doc,
                Box::new(SetSelectionCmd::with_mode(
                    Selection::rect(b),
                    SelectionMode::Add,
                )),
            )
            .unwrap();
        assert!(doc.selection.coverage_at(IVec2::new(2, 2)) > 0.0);
        assert!(doc.selection.coverage_at(IVec2::new(12, 12)) > 0.0);

        history.undo(&mut doc);
        assert_eq!(doc.selection, Selection::rect(a));

        history.redo(&mut doc);
        assert!(doc.selection.coverage_at(IVec2::new(12, 12)) > 0.0);
    }

    #[test]
    fn set_selection_cmd_subtract_mode_removes_region() {
        let mut doc = Document::new(100, 100);
        let mut history = History::new(0);
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(5, 0, 10, 10);

        history
            .do_command(&mut doc, Box::new(SetSelectionCmd::new(Selection::rect(a))))
            .unwrap();
        history
            .do_command(
                &mut doc,
                Box::new(SetSelectionCmd::with_mode(
                    Selection::rect(b),
                    SelectionMode::Subtract,
                )),
            )
            .unwrap();
        assert!(doc.selection.coverage_at(IVec2::new(2, 2)) > 0.0);
        assert_eq!(doc.selection.coverage_at(IVec2::new(7, 2)), 0.0);

        history.undo(&mut doc);
        assert_eq!(doc.selection, Selection::rect(a));

        history.redo(&mut doc);
        assert_eq!(doc.selection.coverage_at(IVec2::new(7, 2)), 0.0);
    }

    #[test]
    fn set_selection_cmd_intersect_mode_keeps_overlap() {
        let mut doc = Document::new(100, 100);
        let mut history = History::new(0);
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(5, 5, 10, 10);

        history
            .do_command(&mut doc, Box::new(SetSelectionCmd::new(Selection::rect(a))))
            .unwrap();
        history
            .do_command(
                &mut doc,
                Box::new(SetSelectionCmd::with_mode(
                    Selection::rect(b),
                    SelectionMode::Intersect,
                )),
            )
            .unwrap();
        assert_eq!(doc.selection.coverage_at(IVec2::new(7, 7)), 1.0);
        assert_eq!(doc.selection.coverage_at(IVec2::new(2, 2)), 0.0);

        history.undo(&mut doc);
        assert_eq!(doc.selection, Selection::rect(a));

        history.redo(&mut doc);
        assert_eq!(doc.selection.coverage_at(IVec2::new(7, 7)), 1.0);
    }

    #[test]
    fn magic_wand_cmd_selects_and_restores() {
        use crate::pixel::Rgba32F;

        let mut doc = Document::new(100, 100);
        let id = doc.add_raster_layer("L");
        doc.layer_mut(id)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(5, 5), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        doc.layer_mut(id)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(6, 5), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        let mut history = History::new(0);
        history
            .do_command(
                &mut doc,
                Box::new(MagicWandCmd::with_mode(
                    IVec2::new(5, 5),
                    0.1,
                    true,
                    SelectionMode::Replace,
                )),
            )
            .unwrap();
        assert!(doc.selection.coverage_at(IVec2::new(5, 5)) > 0.0);
        assert!(doc.selection.coverage_at(IVec2::new(6, 5)) > 0.0);

        history.undo(&mut doc);
        assert!(doc.selection.is_empty());

        history.redo(&mut doc);
        assert!(doc.selection.coverage_at(IVec2::new(5, 5)) > 0.0);
    }

    #[test]
    fn invert_selection_cmd_round_trips() {
        let mut doc = Document::new(100, 100);
        doc.selection = Selection::rect(Rect::new(10, 10, 20, 20));
        let original = doc.selection.clone();

        let mut history = History::new(0);
        history
            .do_command(&mut doc, Box::new(InvertSelectionCmd::new()))
            .unwrap();

        assert_eq!(doc.selection.coverage_at(IVec2::new(0, 0)), 1.0);
        assert_eq!(doc.selection.coverage_at(IVec2::new(15, 15)), 0.0);

        history.undo(&mut doc);
        assert_eq!(doc.selection, original);

        history.redo(&mut doc);
        assert_eq!(doc.selection.coverage_at(IVec2::new(0, 0)), 1.0);
    }

    #[test]
    fn feather_selection_cmd_round_trips() {
        let mut doc = Document::new(100, 100);
        doc.selection = Selection::rect(Rect::new(20, 20, 20, 20));
        let original = doc.selection.clone();

        let mut history = History::new(0);
        history
            .do_command(&mut doc, Box::new(FeatherSelectionCmd::new(2.0)))
            .unwrap();

        assert!(doc.selection.bounds().unwrap().x < 20);
        assert!((doc.selection.coverage_at(IVec2::new(30, 30)) - 1.0).abs() < 1e-5);

        history.undo(&mut doc);
        assert_eq!(doc.selection, original);
    }

    #[test]
    fn grow_selection_cmd_round_trips() {
        let mut doc = Document::new(100, 100);
        doc.selection = Selection::rect(Rect::new(20, 20, 10, 10));
        let original = doc.selection.clone();

        let mut history = History::new(0);
        history
            .do_command(&mut doc, Box::new(GrowSelectionCmd::new(3)))
            .unwrap();

        assert_eq!(doc.selection.coverage_at(IVec2::new(17, 25)), 1.0);

        history.undo(&mut doc);
        assert_eq!(doc.selection, original);
    }

    #[test]
    fn shrink_selection_cmd_round_trips() {
        let mut doc = Document::new(100, 100);
        doc.selection = Selection::rect(Rect::new(20, 20, 10, 10));
        let original = doc.selection.clone();

        let mut history = History::new(0);
        history
            .do_command(&mut doc, Box::new(ShrinkSelectionCmd::new(2)))
            .unwrap();

        assert_eq!(doc.selection.coverage_at(IVec2::new(22, 22)), 1.0);
        assert_eq!(doc.selection.coverage_at(IVec2::new(20, 22)), 0.0);

        history.undo(&mut doc);
        assert_eq!(doc.selection, original);
    }

    // ------------------------------------------------------------------
    // TransformLayerCmd
    // ------------------------------------------------------------------

    #[test]
    fn transform_layer_cmd_resamples_and_undo_restores() {
        use crate::pixel::Rgba32F;
        use glam::{Affine2, Vec2};

        let mut doc = Document::new(100, 100);
        let id = doc.add_raster_layer("L");
        for y in 0..5 {
            for x in 0..5 {
                doc.layer_mut(id)
                    .unwrap()
                    .buffer_mut()
                    .unwrap()
                    .set_pixel(IVec2::new(x, y), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
            }
        }

        let original_buffer = doc.layer(id).unwrap().buffer().unwrap().clone();
        let original_offset = doc.layer(id).unwrap().offset;

        // 5x5 block at (0,0)-(4,4); centre is (2,2). Scale 2x about centre.
        let centre = Vec2::new(2.0, 2.0);
        let affine = Affine2::from_translation(centre)
            * Affine2::from_scale(Vec2::splat(2.0))
            * Affine2::from_translation(-centre);

        let mut history = History::new(0);
        history
            .do_command(&mut doc, Box::new(TransformLayerCmd::new(id, affine)))
            .unwrap();

        let layer = doc.layer(id).unwrap();
        // Source corner (0,0) maps to (-2,-2); new offset is floor of that.
        assert_eq!(layer.offset, IVec2::new(-2, -2));
        // Source (2,2) is the centre of the 5x5 block; scaling about that centre
        // maps it to dest doc (2,2), which with offset (-2,-2) is local (4,4).
        assert_eq!(
            layer.buffer().unwrap().get_pixel(IVec2::new(4, 4)),
            Rgba32F::new(1.0, 0.0, 0.0, 1.0)
        );
        // Bounds grew substantially (the 5x5 interior scaled to 10x10).
        let bounds = layer.buffer().unwrap().exact_bounds().unwrap();
        assert!(bounds.w >= 10 && bounds.h >= 10);

        history.undo(&mut doc);
        let layer = doc.layer(id).unwrap();
        assert_eq!(layer.buffer().unwrap(), &original_buffer);
        assert_eq!(layer.offset, original_offset);
    }

    #[test]
    fn transform_layer_cmd_rejects_locked_layer() {
        use glam::{Affine2, Vec2};

        let mut doc = Document::new(100, 100);
        let id = doc.add_raster_layer("L");
        doc.layer_mut(id).unwrap().locked = true;

        let mut cmd = TransformLayerCmd::new(id, Affine2::from_translation(Vec2::new(5.0, 5.0)));
        assert!(matches!(
            cmd.apply(&mut doc),
            Err(OgreError::LayerLocked(_))
        ));
    }

    #[test]
    fn transform_layer_cmd_undo_redo_round_trips() {
        use crate::pixel::Rgba32F;
        use glam::{Affine2, Vec2};

        let mut doc = Document::new(100, 100);
        let id = doc.add_raster_layer("L");
        for y in 0..5 {
            for x in 0..5 {
                doc.layer_mut(id)
                    .unwrap()
                    .buffer_mut()
                    .unwrap()
                    .set_pixel(IVec2::new(x, y), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
            }
        }

        let affine = Affine2::from_translation(Vec2::new(3.5, 4.0));
        let mut history = History::new(0);
        history
            .do_command(&mut doc, Box::new(TransformLayerCmd::new(id, affine)))
            .unwrap();

        let transformed_buffer = doc.layer(id).unwrap().buffer().unwrap().clone();
        let transformed_offset = doc.layer(id).unwrap().offset;

        history.undo(&mut doc);
        assert_eq!(doc.layer(id).unwrap().offset, IVec2::ZERO);

        history.redo(&mut doc);
        let layer = doc.layer(id).unwrap();
        assert_eq!(layer.buffer().unwrap(), &transformed_buffer);
        assert_eq!(layer.offset, transformed_offset);
    }

    #[test]
    fn transform_layer_cmd_rejects_non_raster_layer() {
        use glam::{Affine2, Vec2};

        let mut doc = Document::new(100, 100);
        let bg = doc.add_raster_layer("bg");
        let group = crate::Layer::new_group("G");
        let id = doc.insert_layer_above(group, bg).unwrap();

        let mut cmd = TransformLayerCmd::new(id, Affine2::from_translation(Vec2::new(5.0, 5.0)));
        assert!(matches!(cmd.apply(&mut doc), Err(OgreError::NotRaster)));
    }

    #[test]
    fn transform_layer_cmd_identity_is_no_op() {
        use crate::pixel::Rgba32F;
        use glam::Affine2;

        let mut doc = Document::new(100, 100);
        let id = doc.add_raster_layer("L");
        for y in 0..5 {
            for x in 0..5 {
                doc.layer_mut(id)
                    .unwrap()
                    .buffer_mut()
                    .unwrap()
                    .set_pixel(IVec2::new(x, y), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
            }
        }
        doc.active = None;

        let original_buffer = doc.layer(id).unwrap().buffer().unwrap().clone();
        let original_offset = doc.layer(id).unwrap().offset;

        let mut history = History::new(0);
        history
            .do_command(
                &mut doc,
                Box::new(TransformLayerCmd::new(id, Affine2::IDENTITY)),
            )
            .unwrap();

        let layer = doc.layer(id).unwrap();
        assert_eq!(layer.buffer().unwrap(), &original_buffer);
        assert_eq!(layer.offset, original_offset);
        assert_eq!(doc.active, Some(id));
    }

    #[test]
    fn transform_layer_cmd_perspective_keystone_maps_corners() {
        use crate::pixel::Rgba32F;

        let mut doc = Document::new(100, 100);
        let id = doc.add_raster_layer("L");
        for y in 0..5 {
            for x in 0..5 {
                doc.layer_mut(id)
                    .unwrap()
                    .buffer_mut()
                    .unwrap()
                    .set_pixel(IVec2::new(x, y), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
            }
        }

        let original_buffer = doc.layer(id).unwrap().buffer().unwrap().clone();
        let original_offset = doc.layer(id).unwrap().offset;

        // Source occupied bounds are (0,0)-(4,4) in local, so document bounds
        // corners are (0,0), (5,0), (5,5), (0,5). Apply a simple horizontal
        // keystone: top edge stays width 5, bottom edge shrinks to width 3.
        let dst_quad = [
            IVec2::new(0, 0), // TL
            IVec2::new(5, 0), // TR
            IVec2::new(4, 5), // BR
            IVec2::new(1, 5), // BL
        ];

        let mut history = History::new(0);
        history
            .do_command(
                &mut doc,
                Box::new(TransformLayerCmd::new_perspective(id, dst_quad)),
            )
            .unwrap();

        let layer = doc.layer(id).unwrap();
        // Offset should be the min quad corner (0,0).
        assert_eq!(layer.offset, IVec2::new(0, 0));
        // The interior should still contain red pixels after the warp.
        let bounds = layer.buffer().unwrap().exact_bounds().unwrap();
        assert!(bounds.w >= 3 && bounds.h >= 5);

        history.undo(&mut doc);
        let layer = doc.layer(id).unwrap();
        assert_eq!(layer.buffer().unwrap(), &original_buffer);
        assert_eq!(layer.offset, original_offset);
    }

    #[test]
    fn transform_layer_cmd_warp_identity_preserves_buffer() {
        use crate::pixel::Rgba32F;

        let mut doc = Document::new(100, 100);
        let id = doc.add_raster_layer("L");
        for y in 0..5 {
            for x in 0..5 {
                doc.layer_mut(id)
                    .unwrap()
                    .buffer_mut()
                    .unwrap()
                    .set_pixel(IVec2::new(x, y), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
            }
        }

        let original_buffer = doc.layer(id).unwrap().buffer().unwrap().clone();
        let original_offset = doc.layer(id).unwrap().offset;

        // Identity 4x4 grid over the document-space occupied bounds (0,0)-(5,5).
        let grid = crate::resample::WarpGrid::identity(Rect::new(0, 0, 5, 5), 4, 4);

        let mut history = History::new(0);
        history
            .do_command(&mut doc, Box::new(TransformLayerCmd::new_warp(id, grid)))
            .unwrap();

        let layer = doc.layer(id).unwrap();
        assert_eq!(layer.offset, original_offset);
        // Bicubic sampling of an identity warp should be nearly identical.
        let bounds = layer.buffer().unwrap().exact_bounds().unwrap();
        assert_eq!(bounds, Rect::new(0, 0, 5, 5));

        history.undo(&mut doc);
        let layer = doc.layer(id).unwrap();
        assert_eq!(layer.buffer().unwrap(), &original_buffer);
        assert_eq!(layer.offset, original_offset);
    }

    // ------------------------------------------------------------------
    // Remove background (fake-transparency matte) command.
    // ------------------------------------------------------------------

    fn fill_white(doc: &mut Document, id: LayerId, w: i32, h: i32) {
        let buffer = doc.layer_mut(id).unwrap().buffer_mut().unwrap();
        for y in 0..h {
            for x in 0..w {
                buffer.set_pixel(IVec2::new(x, y), Rgba32F::new(1.0, 1.0, 1.0, 1.0));
            }
        }
    }

    #[test]
    fn remove_background_cmd_round_trips() {
        let mut doc = Document::new(40, 40);
        let id = doc.add_raster_layer("L");
        fill_white(&mut doc, id, 40, 40);
        // Opaque subject block.
        for y in 15..25 {
            for x in 15..25 {
                doc.layer_mut(id)
                    .unwrap()
                    .buffer_mut()
                    .unwrap()
                    .set_pixel(IVec2::new(x, y), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
            }
        }
        let original = doc.layer(id).unwrap().buffer().unwrap().clone();

        let mut history = History::new(0);
        history
            .do_command(
                &mut doc,
                Box::new(RemoveBackgroundCmd::new(
                    crate::ops::DEFAULT_MATTE_TOLERANCE,
                )),
            )
            .unwrap();

        assert_eq!(
            doc.layer(id)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(2, 2)),
            Rgba32F::TRANSPARENT
        );
        assert_eq!(
            doc.layer(id)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(20, 20)),
            Rgba32F::new(1.0, 0.0, 0.0, 1.0)
        );

        history.undo(&mut doc);
        assert_eq!(doc.layer(id).unwrap().buffer().unwrap(), &original);
    }

    #[test]
    fn remove_background_cmd_errors_when_no_matte() {
        let mut doc = Document::new(16, 16);
        let id = doc.add_raster_layer("L");
        // A gradient: no uniform or checker matte.
        for y in 0..16 {
            for x in 0..16 {
                let v = x as f32 / 15.0;
                doc.layer_mut(id).unwrap().buffer_mut().unwrap().set_pixel(
                    IVec2::new(x, y),
                    Rgba32F::new(v, 1.0 - v, (v * 0.5).fract(), 1.0),
                );
            }
        }
        let mut cmd = RemoveBackgroundCmd::new(crate::ops::DEFAULT_MATTE_TOLERANCE);
        assert!(matches!(
            cmd.apply(&mut doc),
            Err(OgreError::InvalidOperation(_))
        ));
    }

    #[test]
    fn remove_background_cmd_requires_active_raster() {
        let mut doc = Document::new(16, 16);
        let id = doc.add_raster_layer("L");
        fill_white(&mut doc, id, 16, 16);
        doc.active = None;
        let mut cmd = RemoveBackgroundCmd::new(crate::ops::DEFAULT_MATTE_TOLERANCE);
        assert!(matches!(cmd.apply(&mut doc), Err(OgreError::NoActiveLayer)));
    }

    #[test]
    fn remove_background_cmd_errors_on_locked_layer() {
        let mut doc = Document::new(16, 16);
        let id = doc.add_raster_layer("L");
        fill_white(&mut doc, id, 16, 16);
        doc.layer_mut(id).unwrap().locked = true;
        let mut cmd = RemoveBackgroundCmd::new(crate::ops::DEFAULT_MATTE_TOLERANCE);
        assert!(matches!(
            cmd.apply(&mut doc),
            Err(OgreError::LayerLocked(locked)) if locked == id
        ));
    }

    // ------------------------------------------------------------------
    // Paint bucket command.
    // ------------------------------------------------------------------

    #[test]
    fn fill_selection_cmd_round_trips() {
        let mut doc = Document::new(16, 16);
        let id = doc.add_raster_layer("L");
        fill_white(&mut doc, id, 16, 16);
        let original = doc.layer(id).unwrap().buffer().unwrap().clone();
        let red = Rgba32F::new(1.0, 0.0, 0.0, 1.0);
        let sel = Selection::rect(Rect::new(2, 2, 4, 4));

        let mut history = History::new(0);
        history
            .do_command(
                &mut doc,
                Box::new(FillSelectionCmd::new(red, 1.0, sel.clone())),
            )
            .unwrap();
        // Inside selection: red; outside: unchanged.
        assert_eq!(
            doc.layer(id)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(3, 3)),
            red
        );
        assert_eq!(
            doc.layer(id)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(0, 0)),
            Rgba32F::new(1.0, 1.0, 1.0, 1.0)
        );

        history.undo(&mut doc);
        assert_eq!(doc.layer(id).unwrap().buffer().unwrap(), &original);
        history.redo(&mut doc);
        assert_eq!(
            doc.layer(id)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(3, 3)),
            red
        );
    }

    #[test]
    fn fill_selection_cmd_requires_active_raster() {
        let mut doc = Document::new(8, 8);
        doc.active = None;
        let mut cmd = FillSelectionCmd::new(
            Rgba32F::new(1.0, 0.0, 0.0, 1.0),
            1.0,
            Selection::rect(Rect::new(0, 0, 4, 4)),
        );
        assert!(matches!(cmd.apply(&mut doc), Err(OgreError::NoActiveLayer)));
    }

    #[test]
    fn fill_selection_cmd_with_no_selection_fills_canvas() {
        let mut doc = Document::new(8, 8);
        let id = doc.add_raster_layer("L");
        fill_white(&mut doc, id, 8, 8);
        let red = Rgba32F::new(1.0, 0.0, 0.0, 1.0);
        let mut history = History::new(0);
        history
            .do_command(
                &mut doc,
                Box::new(FillSelectionCmd::new(red, 1.0, Selection::none())),
            )
            .unwrap();
        assert_eq!(
            doc.layer(id)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(5, 5)),
            red
        );
    }

    #[test]
    fn merge_down_cmd_merges_and_undo_restores_both_layers() {
        let mut doc = Document::new(8, 8);
        let lower = doc.add_raster_layer("lower");
        // Paint red on the lower, green on the upper (above it).
        doc.layer_mut(lower)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(2, 2), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        let upper = doc.add_raster_layer("upper");
        doc.layer_mut(upper)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(2, 2), Rgba32F::new(0.0, 1.0, 0.0, 1.0));
        let lower_before = doc.layer(lower).unwrap().clone();
        let upper_before = doc.layer(upper).unwrap().clone();
        let order_before = doc.order.clone();

        let mut history = History::new(0);
        history
            .do_command(&mut doc, Box::new(MergeDownCmd::new(upper)))
            .unwrap();
        // Upper is soft-removed (kept for undo): it must not appear in the
        // render order, though its slotmap entry is preserved.
        let order: AHashSet<LayerId> = doc
            .render_order()
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert!(
            !order.contains(&upper),
            "upper removed from render path after merge"
        );
        let merged_pixel = doc
            .layer(lower)
            .unwrap()
            .buffer()
            .unwrap()
            .get_pixel(IVec2::new(2, 2));
        assert_eq!(merged_pixel, Rgba32F::new(0.0, 1.0, 0.0, 1.0));

        // Undo restores both layers exactly (content + tree order).
        history.undo(&mut doc);
        assert_eq!(doc.layer(lower).unwrap(), &lower_before);
        assert_eq!(doc.layer(upper).unwrap(), &upper_before);
        assert_eq!(doc.order, order_before);
    }

    #[test]
    fn merge_down_cmd_errors_when_no_layer_below() {
        let mut doc = Document::new(8, 8);
        let only = doc.add_raster_layer("only");
        let mut cmd = MergeDownCmd::new(only);
        assert!(matches!(
            cmd.apply(&mut doc),
            Err(OgreError::InvalidOperation(_))
        ));
    }

    #[test]
    fn flip_layer_cmd_mirrors_exactly_and_preserves_offset() {
        let mut doc = Document::new(16, 16);
        let id = doc.add_raster_layer("L");
        // Place three distinguishable pixels in local x = 1, 2, 3 at y = 0.
        let layer = doc.layer_mut(id).unwrap();
        let buf = layer.buffer_mut().unwrap();
        buf.set_pixel(IVec2::new(1, 0), Rgba32F::new(0.1, 0.0, 0.0, 1.0));
        buf.set_pixel(IVec2::new(2, 0), Rgba32F::new(0.2, 0.0, 0.0, 1.0));
        buf.set_pixel(IVec2::new(3, 0), Rgba32F::new(0.3, 0.0, 0.0, 1.0));
        layer.offset = IVec2::new(10, 20);
        let original = doc.layer(id).unwrap().buffer().unwrap().clone();
        let prior_offset = doc.layer(id).unwrap().offset;

        let mut history = History::new(0);
        history
            .do_command(
                &mut doc,
                Box::new(FlipLayerCmd::new(id, crate::FlipAxis::Horizontal)),
            )
            .unwrap();
        // Content bounds = local x 1..=3 → mirror: x=1↔3, x=2 stays.
        let buf = doc.layer(id).unwrap().buffer().unwrap();
        assert_eq!(
            buf.get_pixel(IVec2::new(1, 0)),
            Rgba32F::new(0.3, 0.0, 0.0, 1.0)
        );
        assert_eq!(
            buf.get_pixel(IVec2::new(3, 0)),
            Rgba32F::new(0.1, 0.0, 0.0, 1.0)
        );
        assert_eq!(
            buf.get_pixel(IVec2::new(2, 0)),
            Rgba32F::new(0.2, 0.0, 0.0, 1.0)
        );
        // Offset unchanged (doc-space bounding box preserved).
        assert_eq!(doc.layer(id).unwrap().offset, prior_offset);

        // Undo restores the buffer byte-for-byte.
        history.undo(&mut doc);
        assert_eq!(doc.layer(id).unwrap().buffer().unwrap(), &original);
        // Redo is identical to the first apply.
        history.redo(&mut doc);
        assert_eq!(
            doc.layer(id)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(1, 0)),
            Rgba32F::new(0.3, 0.0, 0.0, 1.0)
        );
    }

    #[test]
    fn flip_layer_cmd_rejects_non_raster_and_locked() {
        let mut doc = Document::new(8, 8);
        let anchor = doc.add_raster_layer("anchor");
        let group = doc
            .insert_layer_above(Layer::new_group("G"), anchor)
            .unwrap();
        let mut cmd = FlipLayerCmd::new(group, crate::FlipAxis::Vertical);
        assert!(matches!(cmd.apply(&mut doc), Err(OgreError::NotRaster)));

        doc.layer_mut(anchor).unwrap().locked = true;
        let mut cmd = FlipLayerCmd::new(anchor, crate::FlipAxis::Vertical);
        assert!(matches!(
            cmd.apply(&mut doc),
            Err(OgreError::LayerLocked(id)) if id == anchor
        ));
    }

    #[test]
    fn flip_layer_cmd_no_op_on_empty_buffer() {
        let mut doc = Document::new(8, 8);
        let id = doc.add_raster_layer("L");
        // Empty buffer: flip is a no-op (no error, no pixels written).
        let mut history = History::new(0);
        history
            .do_command(
                &mut doc,
                Box::new(FlipLayerCmd::new(id, crate::FlipAxis::Horizontal)),
            )
            .unwrap();
        assert!(doc.layer(id).unwrap().buffer().unwrap().is_empty());
    }

    #[test]
    fn stroke_selection_cmd_round_trips_rect() {
        let mut doc = Document::new(16, 16);
        let id = doc.add_raster_layer("L");
        fill_white(&mut doc, id, 16, 16);
        let original = doc.layer(id).unwrap().buffer().unwrap().clone();
        let red = Rgba32F::new(1.0, 0.0, 0.0, 1.0);
        let sel = Selection::rect(Rect::new(2, 2, 6, 6));

        let mut history = History::new(0);
        history
            .do_command(
                &mut doc,
                Box::new(StrokeSelectionCmd::new(red, 2.0, 1.0, sel)),
            )
            .unwrap();
        // Border stroked red; interior unchanged.
        assert_eq!(
            doc.layer(id)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(2, 2)),
            red
        );
        assert_eq!(
            doc.layer(id)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(4, 4)),
            Rgba32F::new(1.0, 1.0, 1.0, 1.0)
        );

        history.undo(&mut doc);
        assert_eq!(doc.layer(id).unwrap().buffer().unwrap(), &original);
    }

    #[test]
    fn paint_bucket_cmd_round_trips() {
        let mut doc = Document::new(16, 16);
        let id = doc.add_raster_layer("L");
        fill_white(&mut doc, id, 16, 16);
        let original = doc.layer(id).unwrap().buffer().unwrap().clone();

        let red = Rgba32F::new(1.0, 0.0, 0.0, 1.0);
        let mut history = History::new(0);
        history
            .do_command(
                &mut doc,
                Box::new(PaintBucketCmd::new(
                    IVec2::new(8, 8),
                    red,
                    crate::ops::DEFAULT_FILL_TOLERANCE,
                )),
            )
            .unwrap();

        assert_eq!(
            doc.layer(id)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(8, 8)),
            red
        );
        assert_eq!(
            doc.layer(id)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(0, 0)),
            red
        );

        history.undo(&mut doc);
        assert_eq!(doc.layer(id).unwrap().buffer().unwrap(), &original);
    }

    #[test]
    fn set_layer_buffer_cmd_round_trips() {
        let mut doc = Document::new(16, 16);
        let id = doc.add_raster_layer("L");
        fill_white(&mut doc, id, 16, 16);
        let original = doc.layer(id).unwrap().buffer().unwrap().clone();

        // A replacement buffer with a single red pixel.
        let mut replacement = TiledBuffer::new();
        replacement.set_pixel(IVec2::new(1, 1), Rgba32F::new(1.0, 0.0, 0.0, 1.0));

        let mut history = History::new(0);
        history
            .do_command(
                &mut doc,
                Box::new(SetLayerBufferCmd::new(
                    id,
                    replacement.clone(),
                    "Remove background",
                )),
            )
            .unwrap();
        assert_eq!(doc.layer(id).unwrap().buffer().unwrap(), &replacement);

        history.undo(&mut doc);
        assert_eq!(doc.layer(id).unwrap().buffer().unwrap(), &original);

        history.redo(&mut doc);
        assert_eq!(doc.layer(id).unwrap().buffer().unwrap(), &replacement);
    }

    #[test]
    fn paint_bucket_cmd_requires_active_raster() {
        let mut doc = Document::new(16, 16);
        let id = doc.add_raster_layer("L");
        fill_white(&mut doc, id, 16, 16);
        doc.active = None;
        let mut cmd = PaintBucketCmd::new(IVec2::new(8, 8), Rgba32F::new(1.0, 0.0, 0.0, 1.0), 0.1);
        assert!(matches!(cmd.apply(&mut doc), Err(OgreError::NoActiveLayer)));
    }

    #[test]
    fn paint_bucket_cmd_errors_on_locked_layer() {
        let mut doc = Document::new(16, 16);
        let id = doc.add_raster_layer("L");
        fill_white(&mut doc, id, 16, 16);
        doc.layer_mut(id).unwrap().locked = true;
        let mut cmd = PaintBucketCmd::new(IVec2::new(8, 8), Rgba32F::new(1.0, 0.0, 0.0, 1.0), 0.1);
        assert!(matches!(
            cmd.apply(&mut doc),
            Err(OgreError::LayerLocked(locked)) if locked == id
        ));
    }

    // ------------------------------------------------------------------
    // Layer-mask commands.
    // ------------------------------------------------------------------

    fn sample(doc: &Document, p: IVec2) -> Rgba32F {
        crate::compositor::sample_document_pixel(doc, p).unwrap_or(Rgba32F::TRANSPARENT)
    }

    #[test]
    fn add_layer_mask_reveal_selection_round_trips() {
        let mut doc = Document::new(16, 16);
        let id = doc.add_raster_layer("L");
        fill_white(&mut doc, id, 16, 16);
        // Mask reveals only the top-left 4×4 region (doc space).
        let sel = Selection::rect(Rect::new(0, 0, 4, 4));
        let mut history = History::new(0);
        history
            .do_command(
                &mut doc,
                Box::new(AddLayerMaskCmd::new(
                    id,
                    crate::MaskInit::RevealSelection(sel),
                )),
            )
            .unwrap();

        // Mask present; content inside the selection is visible, outside hidden.
        assert!(doc.layer(id).unwrap().mask().is_some());
        let opaque = Rgba32F::new(1.0, 1.0, 1.0, 1.0);
        assert_eq!(sample(&doc, IVec2::new(2, 2)), opaque);
        assert_eq!(sample(&doc, IVec2::new(10, 10)), Rgba32F::TRANSPARENT);

        let after_add = doc.layer(id).unwrap().mask().unwrap().clone();
        history.undo(&mut doc);
        assert!(doc.layer(id).unwrap().mask().is_none(), "undo removes mask");
        // Without a mask, all content is visible again.
        assert_eq!(sample(&doc, IVec2::new(10, 10)), opaque);

        history.redo(&mut doc);
        // Redo restores the *same* mask byte-for-byte (stable across redo).
        assert_eq!(doc.layer(id).unwrap().mask().unwrap(), &after_add);
    }

    #[test]
    fn add_layer_mask_hide_all_hides_content() {
        let mut doc = Document::new(8, 8);
        let id = doc.add_raster_layer("L");
        fill_white(&mut doc, id, 8, 8);
        let mut history = History::new(0);
        history
            .do_command(
                &mut doc,
                Box::new(AddLayerMaskCmd::new(id, crate::MaskInit::HideAll)),
            )
            .unwrap();
        // Empty mask → coverage 0 everywhere → fully hidden.
        assert!(doc.layer(id).unwrap().mask().unwrap().is_empty());
        assert_eq!(sample(&doc, IVec2::new(4, 4)), Rgba32F::TRANSPARENT);
    }

    #[test]
    fn add_layer_mask_reveal_all_covers_content_tiles() {
        let mut doc = Document::new(8, 8);
        let id = doc.add_raster_layer("L");
        fill_white(&mut doc, id, 8, 8);
        let mut history = History::new(0);
        history
            .do_command(
                &mut doc,
                Box::new(AddLayerMaskCmd::new(id, crate::MaskInit::RevealAll)),
            )
            .unwrap();
        // RevealAll has no effect: content still fully visible.
        let opaque = Rgba32F::new(1.0, 1.0, 1.0, 1.0);
        assert_eq!(sample(&doc, IVec2::new(4, 4)), opaque);
    }

    #[test]
    fn add_layer_mask_rejects_non_raster_and_locked() {
        let mut doc = Document::new(8, 8);
        let raster = doc.add_raster_layer("anchor");
        let group = doc
            .insert_layer_above(Layer::new_group("G"), raster)
            .unwrap();
        let mut cmd = AddLayerMaskCmd::new(group, crate::MaskInit::HideAll);
        assert!(matches!(cmd.apply(&mut doc), Err(OgreError::NotRaster)));

        fill_white(&mut doc, raster, 8, 8);
        doc.layer_mut(raster).unwrap().locked = true;
        let mut cmd = AddLayerMaskCmd::new(raster, crate::MaskInit::HideAll);
        assert!(matches!(
            cmd.apply(&mut doc),
            Err(OgreError::LayerLocked(id)) if id == raster
        ));
    }

    #[test]
    fn add_layer_mask_errors_on_existing_mask() {
        let mut doc = Document::new(8, 8);
        let id = doc.add_raster_layer("L");
        fill_white(&mut doc, id, 8, 8);
        let mut history = History::new(0);
        history
            .do_command(
                &mut doc,
                Box::new(AddLayerMaskCmd::new(id, crate::MaskInit::HideAll)),
            )
            .unwrap();
        // Second add is rejected (must delete or set first).
        let mut second = AddLayerMaskCmd::new(id, crate::MaskInit::RevealAll);
        assert!(matches!(
            second.apply(&mut doc),
            Err(OgreError::InvalidOperation(_))
        ));
    }

    #[test]
    fn delete_layer_mask_round_trips() {
        let mut doc = Document::new(8, 8);
        let id = doc.add_raster_layer("L");
        fill_white(&mut doc, id, 8, 8);
        let mut history = History::new(0);
        history
            .do_command(
                &mut doc,
                Box::new(AddLayerMaskCmd::new(id, crate::MaskInit::HideAll)),
            )
            .unwrap();
        let mask_before = doc.layer(id).unwrap().mask().unwrap().clone();

        history
            .do_command(&mut doc, Box::new(DeleteLayerMaskCmd::new(id)))
            .unwrap();
        assert!(doc.layer(id).unwrap().mask().is_none());

        history.undo(&mut doc);
        // Restored exactly.
        assert_eq!(doc.layer(id).unwrap().mask().unwrap(), &mask_before);
    }

    #[test]
    fn delete_layer_mask_errors_without_mask() {
        let mut doc = Document::new(8, 8);
        let id = doc.add_raster_layer("L");
        let mut cmd = DeleteLayerMaskCmd::new(id);
        assert!(matches!(
            cmd.apply(&mut doc),
            Err(OgreError::InvalidOperation(_))
        ));
    }

    #[test]
    fn apply_layer_mask_bakes_alpha_and_removes_mask() {
        let mut doc = Document::new(16, 16);
        let id = doc.add_raster_layer("L");
        fill_white(&mut doc, id, 16, 16);
        let original_buffer = doc.layer(id).unwrap().buffer().unwrap().clone();
        // Reveal only (0..4, 0..4); apply should bake alpha=0 elsewhere.
        let sel = Selection::rect(Rect::new(0, 0, 4, 4));
        let mut history = History::new(0);
        history
            .do_command(
                &mut doc,
                Box::new(AddLayerMaskCmd::new(
                    id,
                    crate::MaskInit::RevealSelection(sel),
                )),
            )
            .unwrap();
        let mask_before = doc.layer(id).unwrap().mask().unwrap().clone();

        history
            .do_command(&mut doc, Box::new(ApplyLayerMaskCmd::new(id)))
            .unwrap();
        // Mask is gone; its effect is baked into the buffer.
        assert!(doc.layer(id).unwrap().mask().is_none());
        let baked_buffer = doc.layer(id).unwrap().buffer().unwrap().clone();
        // Inside selection: opaque; outside: transparent.
        assert_eq!(baked_buffer.get_pixel(IVec2::new(2, 2)).a, 1.0);
        assert_eq!(baked_buffer.get_pixel(IVec2::new(10, 10)).a, 0.0);

        // Undo restores the pre-apply buffer AND the mask exactly.
        history.undo(&mut doc);
        assert_eq!(doc.layer(id).unwrap().buffer().unwrap(), &original_buffer);
        assert_eq!(doc.layer(id).unwrap().mask().unwrap(), &mask_before);
    }

    #[test]
    fn apply_layer_mask_errors_without_mask() {
        let mut doc = Document::new(8, 8);
        let id = doc.add_raster_layer("L");
        let mut cmd = ApplyLayerMaskCmd::new(id);
        assert!(matches!(
            cmd.apply(&mut doc),
            Err(OgreError::InvalidOperation(_))
        ));
    }

    #[test]
    fn set_layer_mask_round_trips() {
        let mut doc = Document::new(8, 8);
        let id = doc.add_raster_layer("L");
        fill_white(&mut doc, id, 8, 8);

        // Install a HideAll mask via the primitive.
        let mut history = History::new(0);
        history
            .do_command(
                &mut doc,
                Box::new(SetLayerMaskCmd::new(id, Some(TiledBuffer::new()))),
            )
            .unwrap();
        assert!(doc.layer(id).unwrap().mask().is_some());

        // Replace with None (detach) — old is captured for undo.
        history
            .do_command(&mut doc, Box::new(SetLayerMaskCmd::new(id, None)))
            .unwrap();
        assert!(doc.layer(id).unwrap().mask().is_none());

        // Two undos restore the Some mask.
        history.undo(&mut doc);
        assert!(doc.layer(id).unwrap().mask().is_some());
        history.undo(&mut doc);
        assert!(doc.layer(id).unwrap().mask().is_none(), "no mask initially");
    }

    #[test]
    fn mask_add_delete_history_loop_is_stable() {
        // Mask add/delete must leave the layer buffer byte-identical, and a full
        // undo-all / redo-all loop must return to the same state. This guards the
        // killer-feature-style byte-identity invariant for the mask lifecycle.
        let mut doc = Document::new(16, 16);
        let id = doc.add_raster_layer("L");
        fill_white(&mut doc, id, 16, 16);
        let sel = Selection::rect(Rect::new(2, 2, 4, 4));
        let buffer_before = doc.layer(id).unwrap().buffer().unwrap().clone();

        let mut history = History::new(0);
        history
            .do_command(
                &mut doc,
                Box::new(AddLayerMaskCmd::new(
                    id,
                    crate::MaskInit::RevealSelection(sel),
                )),
            )
            .unwrap();
        // While a mask is attached, the *buffer* is untouched (mask is separate).
        assert_eq!(doc.layer(id).unwrap().buffer().unwrap(), &buffer_before);
        history
            .do_command(&mut doc, Box::new(DeleteLayerMaskCmd::new(id)))
            .unwrap();
        assert!(doc.layer(id).unwrap().mask().is_none());
        assert_eq!(doc.layer(id).unwrap().buffer().unwrap(), &buffer_before);

        // Full undo back to the pristine state, then full redo.
        while history.undo(&mut doc).is_some() {}
        assert!(doc.layer(id).unwrap().mask().is_none());
        assert_eq!(doc.layer(id).unwrap().buffer().unwrap(), &buffer_before);
        while history.redo(&mut doc).is_some() {}
        assert!(doc.layer(id).unwrap().mask().is_none());
        assert_eq!(doc.layer(id).unwrap().buffer().unwrap(), &buffer_before);
    }
}
