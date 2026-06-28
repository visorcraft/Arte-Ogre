// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Document model: canvas, layer tree, and render order.

use ahash::AHashSet;
use slotmap::SlotMap;

use crate::coord::Rect;
use crate::error::{OgreError, Result};
use crate::layer::{AdjustmentKind, Layer, LayerContent, LayerId};
use crate::selection::Selection;

/// The color space in which document pixels are interpreted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum ColorSpace {
    /// Linear sRGB, the working space for correct compositing.
    #[default]
    LinearSrgb,
    /// Gamma-encoded sRGB, typically used for final export/display.
    Srgb,
}

/// A layered image document.
///
/// The document owns the layer arena (`SlotMap`), the root z-order, and the
/// current selection. Layers are never mutated directly by outside code;
/// instead they are accessed through the methods on this type.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Document {
    /// The fixed canvas rectangle. Pixels outside this rect are not part of the
    /// exported image but may still exist in layer buffers.
    pub canvas: Rect,
    /// Layer arena.
    pub(crate) layers: SlotMap<LayerId, Layer>,
    /// Root z-order, bottom-to-top.
    pub order: Vec<LayerId>,
    /// Currently active layer, if any.
    pub active: Option<LayerId>,
    /// Current selection.
    pub selection: Selection,
    /// Color space used for compositing/export.
    pub color_space: ColorSpace,
    /// Optional embedded ICC profile bytes.  When present this overrides the
    /// generic `color_space` hint for accurate import/export color conversion.
    pub icc_profile: Option<Vec<u8>>,
    /// Layers that have been removed from the tree by a soft delete but are
    /// still kept in the arena so undo can restore them with a stable id.
    pub(crate) removed: AHashSet<LayerId>,
    /// Named slice rectangles for web export. Stored as
    /// metadata — they do not affect the composited image.
    #[serde(default)]
    pub slices: Vec<SliceRect>,
}

/// A named rectangular slice region.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SliceRect {
    /// Human-readable name (used as the export filename stem).
    pub name: String,
    /// The rectangle in document coordinates.
    pub rect: Rect,
}

impl PartialEq for Document {
    fn eq(&self, other: &Self) -> bool {
        self.canvas == other.canvas
            && self.order == other.order
            && self.active == other.active
            && self.selection == other.selection
            && self.color_space == other.color_space
            && self.icc_profile == other.icc_profile
            && self.removed == other.removed
            && self.slices == other.slices
            && self.layers.len() == other.layers.len()
            && self
                .layers
                .iter()
                .all(|(k, v)| other.layers.get(k) == Some(v))
    }
}

impl Document {
    /// Create a new document with the given canvas size.
    pub fn new(w: u32, h: u32) -> Self {
        Self::with_canvas(Rect::new(0, 0, w, h))
    }

    /// Create a new document with the given canvas rectangle.
    pub fn with_canvas(canvas: Rect) -> Self {
        Self {
            canvas,
            layers: SlotMap::with_key(),
            order: Vec::new(),
            active: None,
            selection: Selection::none(),
            color_space: ColorSpace::LinearSrgb,
            icc_profile: None,
            removed: AHashSet::new(),
            slices: Vec::new(),
        }
    }

    /// Insert an arbitrary layer into the document's arena.
    ///
    /// The layer's [`Layer::id`] is overwritten with the id assigned by the
    /// document. The caller is responsible for adding the returned id to the
    /// layer tree (`order` or a group's `children`).
    pub fn add_layer(&mut self, layer: Layer) -> LayerId {
        let id = self.layers.insert(layer);
        self.layers[id].id = id;
        id
    }

    /// Replace the set of soft-deleted layer ids.
    pub fn set_removed(&mut self, removed: AHashSet<LayerId>) {
        self.removed = removed;
    }

    /// Borrow the full layer arena.
    #[doc(hidden)]
    pub fn all_layers(&self) -> &SlotMap<LayerId, Layer> {
        &self.layers
    }

    /// Borrow the set of soft-deleted layer ids.
    #[doc(hidden)]
    pub fn removed_layers(&self) -> &AHashSet<LayerId> {
        &self.removed
    }

    /// Borrow a layer by id.
    pub fn layer(&self, id: LayerId) -> Result<&Layer> {
        self.layers.get(id).ok_or(OgreError::LayerNotFound(id))
    }

    /// Mutably borrow a layer by id.
    pub fn layer_mut(&mut self, id: LayerId) -> Result<&mut Layer> {
        self.layers.get_mut(id).ok_or(OgreError::LayerNotFound(id))
    }

    /// Add a new empty raster layer on top of the root stack.
    ///
    /// The new layer becomes the active layer.
    pub fn add_raster_layer(&mut self, name: &str) -> LayerId {
        let layer = Layer::new_raster(name);
        let id = self.layers.insert(layer);
        self.layers[id].id = id;
        self.order.push(id);
        self.active = Some(id);
        id
    }

    /// Add a new adjustment layer on top of the root stack.
    ///
    /// The new layer becomes the active layer.
    pub fn add_adjustment_layer(&mut self, name: &str, kind: AdjustmentKind) -> LayerId {
        let layer = Layer::new_adjustment(name, kind);
        let id = self.layers.insert(layer);
        self.layers[id].id = id;
        self.order.push(id);
        self.active = Some(id);
        id
    }

    /// Add a new vector layer on top of the root stack.
    ///
    /// The new layer becomes the active layer.
    pub fn add_vector_layer(&mut self, name: &str, data: crate::layer::VectorData) -> LayerId {
        let layer = Layer::new_vector(name, data);
        let id = self.layers.insert(layer);
        self.layers[id].id = id;
        self.order.push(id);
        self.active = Some(id);
        id
    }

    /// Insert an existing layer directly above `anchor` in its sibling list.
    ///
    /// The layer's [`Layer::id`] is overwritten with the id assigned by the
    /// document's arena. Returns the new id on success.
    pub fn insert_layer_above(&mut self, mut layer: Layer, anchor: LayerId) -> Result<LayerId> {
        if !self.layers.contains_key(anchor) {
            return Err(OgreError::LayerNotFound(anchor));
        }
        // If the caller handed us a group with pre-populated children, make
        // the tree consistent before the group enters the arena: adopt
        // children that already live in the document and drop references to
        // ids that do not exist yet.
        self.detach_group_children(&mut layer);

        let id = self.layers.insert(layer);
        self.layers[id].id = id;

        let (parent, index) = self
            .sibling_index(anchor)
            .ok_or(OgreError::LayerNotFound(anchor))?;
        self.insert_into_siblings(parent, index + 1, id)?;
        Ok(id)
    }

    /// Remove a layer (and all of its descendants, if it is a group) from the
    /// document.
    ///
    /// Returns the removed layer. If the active layer is removed, it is
    /// cleared.
    ///
    /// This is a crate-private hard remove; external code must route deletion
    /// through [`DeleteLayerCmd`](crate::history::DeleteLayerCmd) so the undo
    /// system can preserve stable layer ids.
    pub(crate) fn remove_layer(&mut self, id: LayerId) -> Result<Layer> {
        if !self.layers.contains_key(id) {
            return Err(OgreError::LayerNotFound(id));
        }

        // Collect the subtree rooted at `id` so nothing is leaked from the
        // SlotMap when a group is removed. Cycle detection defends against
        // corrupt trees that would otherwise loop forever.
        let mut to_remove = Vec::new();
        let mut visited = AHashSet::new();
        self.collect_descendants(id, &mut to_remove, &mut visited)?;
        to_remove.push(id);

        // Remove `id` from its sibling list. Descendants live only inside
        // group children, so they do not need to be removed from other lists.
        if let Some((parent, idx)) = self.sibling_index(id) {
            let _ = self.remove_from_siblings(parent, idx);
        }

        if self.active.is_some_and(|a| to_remove.contains(&a)) {
            self.active = None;
        }

        // Remove in reverse order so children are removed before parents; this
        // avoids invalidating `LayerContent::Group` data while we still hold
        // references to it.
        let mut removed = None;
        for rid in to_remove.into_iter().rev() {
            self.removed.remove(&rid);
            let layer = self.layers.remove(rid).expect("collected layer exists");
            if rid == id {
                removed = Some(layer);
            }
        }

        Ok(removed.expect("removed layer was collected"))
    }

    /// Soft-remove a layer subtree from the tree while keeping it in the arena.
    ///
    /// This is the implementation used by the undoable `DeleteLayerCmd`. The
    /// returned vector contains every layer id that became removed, in the
    /// order collected (descendants first, then `id`).
    pub(crate) fn remove_layer_soft(&mut self, id: LayerId) -> Result<Vec<LayerId>> {
        if !self.layers.contains_key(id) {
            return Err(OgreError::LayerNotFound(id));
        }

        let mut to_remove = Vec::new();
        let mut visited = AHashSet::new();
        self.collect_descendants(id, &mut to_remove, &mut visited)?;
        to_remove.push(id);

        if let Some((parent, idx)) = self.sibling_index(id) {
            let _ = self.remove_from_siblings(parent, idx);
        }

        if self.active.is_some_and(|a| to_remove.contains(&a)) {
            self.active = None;
        }

        self.removed.extend(to_remove.iter().copied());
        Ok(to_remove)
    }

    /// Restore a soft-removed layer subtree to the tree.
    pub(crate) fn restore_layer_soft(
        &mut self,
        parent: Option<LayerId>,
        index: usize,
        id: LayerId,
        ids: &[LayerId],
    ) {
        let _ = self.insert_into_siblings(parent, index, id);
        for &rid in ids {
            self.removed.remove(&rid);
        }
    }

    /// Return the flattened bottom-to-top render order with nesting depth.
    ///
    /// Groups appear before their children; children are emitted with an
    /// incremented depth. This matches the CPU reference compositor's
    /// depth-first traversal.
    ///
    /// Returns an error if the layer tree contains a cycle.
    pub fn render_order(&self) -> Result<Vec<(LayerId, u8)>> {
        let mut out = Vec::new();
        let mut visited = AHashSet::new();
        for &id in &self.order {
            self.render_order_visit(id, 0, &mut out, &mut visited)?;
        }
        Ok(out)
    }

    fn render_order_visit(
        &self,
        id: LayerId,
        depth: u8,
        out: &mut Vec<(LayerId, u8)>,
        visited: &mut AHashSet<LayerId>,
    ) -> Result<()> {
        if self.removed.contains(&id) {
            return Ok(());
        }
        if !visited.insert(id) {
            return Err(OgreError::InvalidOperation("cycle in layer tree"));
        }
        let Some(layer) = self.layers.get(id) else {
            return Ok(());
        };
        out.push((id, depth));
        match &layer.content {
            LayerContent::Group { children } => {
                let child_depth = depth.saturating_add(1);
                for &child in children {
                    self.render_order_visit(child, child_depth, out, visited)?;
                }
            }
            LayerContent::Raster { .. } | LayerContent::Adjustment(_) | LayerContent::Vector(_) => {
            }
        }
        Ok(())
    }

    /// Return the parent of `id`, or `None` if it is a root layer.
    ///
    /// Soft-removed parents are ignored, so a layer inside a deleted subtree
    /// appears as an orphan.
    pub fn parent_of(&self, id: LayerId) -> Option<LayerId> {
        for layer in self.layers.values() {
            if self.removed.contains(&layer.id) {
                continue;
            }
            if let LayerContent::Group { children } = &layer.content {
                if children.contains(&id) {
                    return Some(layer.id);
                }
            }
        }
        None
    }

    /// Return the sibling list that contains `id`.
    ///
    /// Returns `None` if the layer does not exist in the tree or has been
    /// soft-removed.
    pub fn siblings(&self, id: LayerId) -> Option<&[LayerId]> {
        if !self.layers.contains_key(id) || self.removed.contains(&id) {
            return None;
        }
        if let Some(parent) = self.parent_of(id) {
            let parent = self.layers.get(parent)?;
            if let LayerContent::Group { children } = &parent.content {
                return Some(children);
            }
        }
        Some(&self.order)
    }

    /// Move `id` to `new_index` within its current sibling list.
    ///
    /// `new_index` is clamped to the valid final-index range for that list,
    /// so passing a very large value moves the layer to the bottom of its
    /// sibling list.
    pub fn reorder(&mut self, id: LayerId, new_index: usize) -> Result<()> {
        let (parent, current) = self.sibling_index(id).ok_or(OgreError::LayerNotFound(id))?;
        let list_len = self
            .sibling_list_len(parent)
            .ok_or(OgreError::LayerNotFound(parent.unwrap_or(id)))?;
        let target = new_index.min(list_len.saturating_sub(1));
        if current == target {
            return Ok(());
        }
        self.remove_from_siblings(parent, current)?;
        self.insert_into_siblings(parent, target, id)?;
        Ok(())
    }

    /// Move `id` into `group` at `index` within the group's children.
    ///
    /// `index` is clamped to the valid range. Moving a group into itself or
    /// into one of its descendants is rejected with an error.
    pub fn move_into_group(&mut self, id: LayerId, group: LayerId, index: usize) -> Result<()> {
        if id == group {
            return Err(OgreError::InvalidOperation(
                "cannot move a layer into itself",
            ));
        }
        if !self.layers.contains_key(id) {
            return Err(OgreError::LayerNotFound(id));
        }
        let group_layer = self
            .layers
            .get(group)
            .ok_or(OgreError::LayerNotFound(group))?;
        if self.removed.contains(&group) {
            return Err(OgreError::InvalidOperation("target layer is removed"));
        }
        if !matches!(group_layer.content, LayerContent::Group { .. }) {
            return Err(OgreError::InvalidOperation("target layer is not a group"));
        }

        // Prevent cycles: the group must not be `id` or a descendant of `id`.
        let mut descendants = Vec::new();
        let mut visited = AHashSet::new();
        self.collect_descendants(id, &mut descendants, &mut visited)?;
        if descendants.contains(&group) {
            return Err(OgreError::InvalidOperation(
                "cannot move a layer into its own descendant",
            ));
        }

        let (parent, current) = self.sibling_index(id).ok_or(OgreError::LayerNotFound(id))?;
        self.remove_from_siblings(parent, current)?;

        let group_children_len = match &self
            .layers
            .get(group)
            .ok_or(OgreError::LayerNotFound(group))?
            .content
        {
            LayerContent::Group { children } => children.len(),
            _ => return Err(OgreError::InvalidOperation("target layer is not a group")),
        };
        let target = index.min(group_children_len);
        match &mut self
            .layers
            .get_mut(group)
            .ok_or(OgreError::LayerNotFound(group))?
            .content
        {
            LayerContent::Group { children } => children.insert(target, id),
            _ => return Err(OgreError::InvalidOperation("target layer is not a group")),
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------

    /// Return `(parent, index)` for `id` within its sibling list, or `None`
    /// if the layer is not present (or soft-removed). A root-level layer
    /// returns `(None, index_into_order)`.
    pub fn sibling_index(&self, id: LayerId) -> Option<(Option<LayerId>, usize)> {
        if !self.layers.contains_key(id) || self.removed.contains(&id) {
            return None;
        }
        if let Some(parent) = self.parent_of(id) {
            if self.removed.contains(&parent) {
                return None;
            }
            let parent = self.layers.get(parent)?;
            if let LayerContent::Group { children } = &parent.content {
                let idx = children.iter().position(|&c| c == id)?;
                return Some((Some(parent.id), idx));
            }
        }
        let idx = self.order.iter().position(|&c| c == id)?;
        Some((None, idx))
    }

    fn sibling_list_len(&self, parent: Option<LayerId>) -> Option<usize> {
        match parent {
            Some(pid) => match &self.layers.get(pid)?.content {
                LayerContent::Group { children } => Some(children.len()),
                _ => Some(0),
            },
            None => Some(self.order.len()),
        }
    }

    pub(crate) fn insert_into_siblings(
        &mut self,
        parent: Option<LayerId>,
        index: usize,
        id: LayerId,
    ) -> Result<()> {
        match parent {
            Some(pid) => {
                let children = match &mut self
                    .layers
                    .get_mut(pid)
                    .ok_or(OgreError::LayerNotFound(pid))?
                    .content
                {
                    LayerContent::Group { children } => children,
                    _ => return Err(OgreError::InvalidOperation("parent must be a group")),
                };
                let idx = index.min(children.len());
                children.insert(idx, id);
            }
            None => {
                let idx = index.min(self.order.len());
                self.order.insert(idx, id);
            }
        }
        Ok(())
    }

    fn remove_from_siblings(&mut self, parent: Option<LayerId>, index: usize) -> Result<()> {
        match parent {
            Some(pid) => {
                let children = match &mut self
                    .layers
                    .get_mut(pid)
                    .ok_or(OgreError::LayerNotFound(pid))?
                    .content
                {
                    LayerContent::Group { children } => children,
                    _ => return Err(OgreError::InvalidOperation("parent must be a group")),
                };
                if index >= children.len() {
                    return Err(OgreError::InvalidOperation("sibling index out of range"));
                }
                children.remove(index);
            }
            None => {
                if index >= self.order.len() {
                    return Err(OgreError::InvalidOperation("sibling index out of range"));
                }
                self.order.remove(index);
            }
        }
        Ok(())
    }

    /// Detach children referenced by a not-yet-inserted group layer.
    ///
    /// Existing layers in the document are removed from their current sibling
    /// list so they can be adopted by the group. Ids that do not yet exist in
    /// the document, self-references, and duplicates are dropped. This keeps
    /// the single-parent invariant intact when a group is inserted with a
    /// pre-built children list.
    fn detach_group_children(&mut self, layer: &mut Layer) {
        let children = match &mut layer.content {
            LayerContent::Group { children } => std::mem::take(children),
            _ => return,
        };

        let mut adopted = AHashSet::new();
        let mut cleaned = Vec::with_capacity(children.len());
        for child in children {
            if !adopted.insert(child) {
                continue;
            }
            if self.removed.contains(&child) {
                continue;
            }
            if self.layers.contains_key(child) {
                if let Some((parent, idx)) = self.sibling_index(child) {
                    let _ = self.remove_from_siblings(parent, idx);
                }
                cleaned.push(child);
            }
        }

        if let LayerContent::Group { children } = &mut layer.content {
            *children = cleaned;
        }
    }

    fn collect_descendants(
        &self,
        id: LayerId,
        out: &mut Vec<LayerId>,
        visited: &mut AHashSet<LayerId>,
    ) -> Result<()> {
        if !visited.insert(id) {
            return Err(OgreError::InvalidOperation("cycle in layer tree"));
        }
        match &self
            .layers
            .get(id)
            .ok_or(OgreError::LayerNotFound(id))?
            .content
        {
            LayerContent::Group { children } => {
                for &child in children {
                    out.push(child);
                    self.collect_descendants(child, out, visited)?;
                }
            }
            LayerContent::Raster { .. } | LayerContent::Adjustment(_) | LayerContent::Vector(_) => {
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::layer::Layer;

    use super::*;

    #[test]
    fn new_document_has_empty_canvas_and_no_layers() {
        let doc = Document::new(1920, 1080);
        assert_eq!(doc.canvas, Rect::new(0, 0, 1920, 1080));
        assert!(doc.order.is_empty());
        assert_eq!(doc.active, None);
        assert!(doc.selection.is_empty());
        assert_eq!(doc.color_space, ColorSpace::LinearSrgb);
    }

    #[test]
    fn add_raster_layer_returns_id_and_updates_state() {
        let mut doc = Document::new(100, 100);
        let id = doc.add_raster_layer("Background");
        assert_eq!(doc.layer(id).unwrap().name, "Background");
        assert!(doc.layer(id).unwrap().buffer().unwrap().is_empty());
        assert_eq!(doc.order, vec![id]);
        assert_eq!(doc.active, Some(id));
    }

    #[test]
    fn insert_layer_above_places_after_anchor() {
        let mut doc = Document::new(100, 100);
        let a = doc.add_raster_layer("A");
        let b = Layer::new_raster("B");
        let b_id = doc.insert_layer_above(b, a).unwrap();
        assert_eq!(doc.order, vec![a, b_id]);
    }

    #[test]
    fn insert_layer_above_fails_for_missing_anchor() {
        let mut doc = Document::new(100, 100);
        let missing = LayerId::default();
        let layer = Layer::new_raster("Orphan");
        assert!(matches!(
            doc.insert_layer_above(layer, missing),
            Err(OgreError::LayerNotFound(_))
        ));
    }

    #[test]
    fn remove_layer_removes_from_order_and_clears_active() {
        let mut doc = Document::new(100, 100);
        let a = doc.add_raster_layer("A");
        let removed = doc.remove_layer(a).unwrap();
        assert_eq!(removed.name, "A");
        assert!(doc.order.is_empty());
        assert_eq!(doc.active, None);
        assert!(matches!(doc.layer(a), Err(OgreError::LayerNotFound(_))));
    }

    #[test]
    fn removing_group_clears_active_descendant() {
        let mut doc = Document::new(100, 100);
        let bg = doc.add_raster_layer("bg");
        let group = Layer::new_group("group");
        let group_id = doc.insert_layer_above(group, bg).unwrap();
        let child = doc.add_raster_layer("child");
        doc.move_into_group(child, group_id, 0).unwrap();

        // `add_raster_layer` made `child` the active layer.
        assert_eq!(doc.active, Some(child));
        doc.remove_layer(group_id).unwrap();
        assert_eq!(doc.active, None);
    }

    #[test]
    fn remove_group_also_removes_children() {
        let mut doc = Document::new(100, 100);
        let bg = doc.add_raster_layer("bg");
        let group = Layer::new_group("group");
        let group_id = doc.insert_layer_above(group, bg).unwrap();
        let child = doc.add_raster_layer("child");
        doc.move_into_group(child, group_id, 0).unwrap();

        let removed = doc.remove_layer(group_id).unwrap();
        assert_eq!(
            removed.content,
            LayerContent::Group {
                children: vec![child]
            }
        );
        assert!(matches!(
            doc.layer(group_id),
            Err(OgreError::LayerNotFound(_))
        ));
        assert!(matches!(doc.layer(child), Err(OgreError::LayerNotFound(_))));
        // The unrelated background layer remains.
        assert_eq!(doc.order, vec![bg]);
    }

    #[test]
    fn render_order_emits_group_before_children() {
        let mut doc = Document::new(100, 100);
        let bg = doc.add_raster_layer("bg");
        let group = Layer::new_group("group");
        let group_id = doc.insert_layer_above(group, bg).unwrap();
        let c1 = doc.add_raster_layer("c1");
        let c2 = doc.add_raster_layer("c2");
        doc.move_into_group(c1, group_id, 0).unwrap();
        doc.move_into_group(c2, group_id, 1).unwrap();

        let order = doc.render_order().unwrap();
        assert_eq!(order, vec![(bg, 0), (group_id, 0), (c1, 1), (c2, 1),]);
    }

    #[test]
    fn parent_of_and_siblings_track_hierarchy() {
        let mut doc = Document::new(100, 100);
        let bg = doc.add_raster_layer("bg");
        let group = Layer::new_group("group");
        let group_id = doc.insert_layer_above(group, bg).unwrap();
        let child = doc.add_raster_layer("child");
        doc.move_into_group(child, group_id, 0).unwrap();

        assert_eq!(doc.parent_of(bg), None);
        assert_eq!(doc.parent_of(group_id), None);
        assert_eq!(doc.parent_of(child), Some(group_id));
        assert_eq!(doc.siblings(child).unwrap(), &[child]);
    }

    #[test]
    fn reorder_moves_within_root_order() {
        let mut doc = Document::new(100, 100);
        let a = doc.add_raster_layer("A");
        let b = doc.add_raster_layer("B");
        let c = doc.add_raster_layer("C");
        // Move `a` to final index 2 (the end).
        doc.reorder(a, 2).unwrap();
        assert_eq!(doc.order, vec![b, c, a]);
    }

    #[test]
    fn reorder_clamps_out_of_bounds() {
        let mut doc = Document::new(100, 100);
        let a = doc.add_raster_layer("A");
        let b = doc.add_raster_layer("B");
        doc.reorder(a, 100).unwrap();
        assert_eq!(doc.order, vec![b, a]);
    }

    #[test]
    fn move_into_group_rejects_non_group_target() {
        let mut doc = Document::new(100, 100);
        let a = doc.add_raster_layer("A");
        let b = doc.add_raster_layer("B");
        assert!(matches!(
            doc.move_into_group(a, b, 0),
            Err(OgreError::InvalidOperation(_))
        ));
    }

    #[test]
    fn move_into_group_rejects_cycles() {
        let mut doc = Document::new(100, 100);

        // Build a parent group with a child group inside it.
        let anchor = doc.add_raster_layer("anchor");
        let parent = Layer::new_group("parent");
        let parent_id = doc.insert_layer_above(parent, anchor).unwrap();
        doc.remove_layer(anchor).unwrap();

        let child = Layer::new_group("child");
        let child_id = doc.insert_layer_above(child, parent_id).unwrap();
        doc.move_into_group(child_id, parent_id, 0).unwrap();

        // Moving the parent into its own descendant would create a cycle.
        assert!(matches!(
            doc.move_into_group(parent_id, child_id, 0),
            Err(OgreError::InvalidOperation(_))
        ));
    }

    #[test]
    fn remove_layer_inside_group_clears_it_from_parent() {
        let mut doc = Document::new(100, 100);
        let bg = doc.add_raster_layer("bg");
        let group = Layer::new_group("group");
        let group_id = doc.insert_layer_above(group, bg).unwrap();
        let child = doc.add_raster_layer("child");
        doc.move_into_group(child, group_id, 0).unwrap();

        let removed = doc.remove_layer(child).unwrap();
        assert_eq!(removed.name, "child");
        assert!(matches!(doc.layer(child), Err(OgreError::LayerNotFound(_))));

        let group = doc.layer(group_id).unwrap();
        if let LayerContent::Group { children } = &group.content {
            assert!(children.is_empty());
        } else {
            panic!("expected a group");
        }
        assert_eq!(doc.order, vec![bg, group_id]);
    }

    #[test]
    fn reorder_moves_within_group_children() {
        let mut doc = Document::new(100, 100);
        let bg = doc.add_raster_layer("bg");
        let group = Layer::new_group("group");
        let group_id = doc.insert_layer_above(group, bg).unwrap();
        let a = doc.add_raster_layer("a");
        let b = doc.add_raster_layer("b");
        doc.move_into_group(a, group_id, 0).unwrap();
        doc.move_into_group(b, group_id, 1).unwrap();

        doc.reorder(a, 1).unwrap();

        let children = match &doc.layer(group_id).unwrap().content {
            LayerContent::Group { children } => children.clone(),
            _ => panic!("expected a group"),
        };
        assert_eq!(children, vec![b, a]);
    }

    #[test]
    fn active_preserved_when_unrelated_layer_removed() {
        let mut doc = Document::new(100, 100);
        let a = doc.add_raster_layer("a");
        let b = doc.add_raster_layer("b");
        let c = doc.add_raster_layer("c");
        doc.active = Some(b);

        doc.remove_layer(a).unwrap();
        assert_eq!(doc.active, Some(b));

        doc.remove_layer(b).unwrap();
        assert_eq!(doc.active, None);
        assert!(doc.layer(c).is_ok());
    }

    #[test]
    fn insert_group_with_new_children_drops_invalid_ids() {
        let mut doc = Document::new(100, 100);
        let bg = doc.add_raster_layer("bg");
        let mut group = Layer::new_group("group");
        let dummy = LayerId::default();
        group.content = LayerContent::Group {
            children: vec![dummy, dummy, LayerId::default()],
        };

        let group_id = doc.insert_layer_above(group, bg).unwrap();
        let children = match &doc.layer(group_id).unwrap().content {
            LayerContent::Group { children } => children.clone(),
            _ => panic!("expected a group"),
        };
        assert!(children.is_empty());
        assert_eq!(doc.order, vec![bg, group_id]);
        assert_tree_integrity(&doc);
    }

    #[test]
    fn insert_group_with_existing_children_reparents_them() {
        let mut doc = Document::new(100, 100);
        let bg = doc.add_raster_layer("bg");
        let a = doc.add_raster_layer("a");
        let b = doc.add_raster_layer("b");
        let mut group = Layer::new_group("group");
        group.content = LayerContent::Group {
            children: vec![a, b],
        };

        let group_id = doc.insert_layer_above(group, bg).unwrap();
        assert_eq!(doc.order, vec![bg, group_id]);

        let children = match &doc.layer(group_id).unwrap().content {
            LayerContent::Group { children } => children.clone(),
            _ => panic!("expected a group"),
        };
        assert_eq!(children, vec![a, b]);
        assert_eq!(doc.parent_of(a), Some(group_id));
        assert_eq!(doc.parent_of(b), Some(group_id));
        assert_tree_integrity(&doc);
    }

    #[test]
    fn render_order_on_nested_group_is_depth_first() {
        let mut doc = Document::new(100, 100);
        let bg = doc.add_raster_layer("bg");
        let g1 = Layer::new_group("g1");
        let g1_id = doc.insert_layer_above(g1, bg).unwrap();
        let c1 = doc.add_raster_layer("c1");
        doc.move_into_group(c1, g1_id, 0).unwrap();

        let g2 = Layer::new_group("g2");
        let g2_id = doc.insert_layer_above(g2, c1).unwrap();
        let c2 = doc.add_raster_layer("c2");
        doc.move_into_group(c2, g2_id, 0).unwrap();

        let order = doc.render_order().unwrap();
        assert_eq!(
            order,
            vec![(bg, 0), (g1_id, 0), (c1, 1), (g2_id, 1), (c2, 2)]
        );
    }

    #[test]
    fn render_order_detects_manual_cycle() {
        let mut doc = Document::new(100, 100);
        let bg = doc.add_raster_layer("bg");
        let parent = Layer::new_group("parent");
        let parent_id = doc.insert_layer_above(parent, bg).unwrap();
        let child = Layer::new_group("child");
        let child_id = doc.insert_layer_above(child, parent_id).unwrap();
        doc.move_into_group(child_id, parent_id, 0).unwrap();

        // Intentionally corrupt the tree to create p -> c -> p.
        doc.layer_mut(child_id).unwrap().content = LayerContent::Group {
            children: vec![parent_id],
        };

        assert!(matches!(
            doc.render_order(),
            Err(OgreError::InvalidOperation("cycle in layer tree"))
        ));
    }

    #[test]
    fn tree_integrity_round_trip() {
        let mut doc = Document::new(100, 100);
        let bg = doc.add_raster_layer("bg");
        let a = doc.add_raster_layer("a");
        let b = doc.add_raster_layer("b");
        assert_tree_integrity(&doc);

        let group = Layer::new_group("group");
        let group_id = doc.insert_layer_above(group, bg).unwrap();
        doc.move_into_group(a, group_id, 0).unwrap();
        doc.reorder(a, 0).unwrap();
        doc.active = Some(b);
        assert_tree_integrity(&doc);

        doc.remove_layer_soft(a).unwrap();
        assert_tree_integrity(&doc);

        doc.remove_layer_soft(group_id).unwrap();
        assert_tree_integrity(&doc);

        // Hard remove the remaining layers. `a` is an orphaned soft-deleted
        // layer after its parent group was soft-removed, so it must be
        // reclaimed explicitly.
        doc.remove_layer(a).unwrap();
        doc.remove_layer(group_id).unwrap();
        doc.remove_layer(bg).unwrap();
        doc.remove_layer(b).unwrap();
        assert_tree_integrity(&doc);
        assert!(
            doc.layers.is_empty(),
            "remaining layers: {:?}",
            doc.layers.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn insert_group_with_soft_deleted_child_drops_it_safely() {
        let mut doc = Document::new(100, 100);
        let bg = doc.add_raster_layer("bg");
        let a = doc.add_raster_layer("a");
        doc.remove_layer_soft(a).unwrap();

        let mut group = Layer::new_group("group");
        group.content = LayerContent::Group { children: vec![a] };

        let group_id = doc.insert_layer_above(group, bg).unwrap();
        let children = match &doc.layer(group_id).unwrap().content {
            LayerContent::Group { children } => children.clone(),
            _ => panic!("expected a group"),
        };
        assert!(children.is_empty());
        assert_eq!(doc.order, vec![bg, group_id]);
        assert_tree_integrity(&doc);
    }

    fn assert_tree_integrity(doc: &Document) {
        let mut seen = AHashSet::new();

        fn visit(doc: &Document, id: LayerId, seen: &mut AHashSet<LayerId>) {
            assert!(
                seen.insert(id),
                "layer {:?} appears more than once in the tree",
                id
            );
            if let Ok(layer) = doc.layer(id) {
                if let LayerContent::Group { children } = &layer.content {
                    for &child in children {
                        visit(doc, child, seen);
                    }
                }
            }
        }

        for &id in &doc.order {
            visit(doc, id, &mut seen);
        }

        for id in doc.layers.keys() {
            if doc.removed.contains(&id) {
                continue;
            }
            assert!(
                seen.contains(&id),
                "layer {:?} is in the arena but not in the layer tree",
                id
            );
        }
        assert_eq!(
            seen.len() + doc.removed.len(),
            doc.layers.len(),
            "layer tree ids do not match arena ids"
        );

        if let Some(active) = doc.active {
            assert!(
                doc.layer(active).is_ok(),
                "active layer {:?} does not resolve",
                active
            );
        }
    }
}
