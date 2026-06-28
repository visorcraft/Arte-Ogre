// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Layer stack panel.
//!
//! This panel renders the document's layers in top-to-bottom display order and
//! routes every edit through [`dispatch`]. The active layer selection is treated
//! as view-state and is allowed to change outside the history stack.

use crate::dispatch;
use crate::panels::tools_sidebar::{paint_six_dot_grip, SIX_DOT_GRIP_SIZE};
use crate::state::AppState;
use ogre_core::{
    AddRasterLayerCmd, BlendMode, DeleteLayerCmd, DuplicateLayerCmd, LayerId, RenameCmd,
    ReorderCmd, SetBlendCmd, SetOpacityCmd,
};

/// A single row in the layers panel, in top-to-bottom display order.
#[derive(Debug, Clone, PartialEq)]
pub struct LayerRow {
    /// The layer identifier.
    pub id: LayerId,
    /// Layer name shown in the panel.
    pub name: String,
    /// Whether the layer is currently visible.
    pub visible: bool,
    /// Current opacity in the range `[0.0, 1.0]`.
    pub opacity: f32,
    /// Current blend mode.
    pub blend: BlendMode,
    /// Whether this row is the active layer.
    pub active: bool,
    /// Whether the layer is locked (edits blocked).
    pub locked: bool,
}

/// Build the layer rows for display.
///
/// Layers are returned top-to-bottom, which is the reverse of the document's
/// bottom-to-top [`order`](ogre_core::Document::order).
pub fn layer_rows(doc: &ogre_core::Document) -> Vec<LayerRow> {
    let active = doc.active;
    doc.order
        .iter()
        .rev()
        .filter_map(|&id| {
            let layer = doc.layer(id).ok()?;
            Some(LayerRow {
                id,
                name: layer.name.clone(),
                visible: layer.visible,
                opacity: layer.opacity,
                blend: layer.blend,
                active: active == Some(id),
                locked: layer.locked,
            })
        })
        .collect()
}

/// Toggle a layer's visibility.
///
/// Visibility is a *view* property, not a document edit, so this deliberately
/// bypasses the undo history (toggling the eye should never clutter undo/redo).
/// It mutates the flag directly, marks the renderer dirty for a recomposite, and
/// flags the document unsaved (visibility is persisted in the file). The
/// `SetVisibleCmd` command still exists for the internal batched operations
/// (merge, group) where a visibility change is part of an undoable action.
//
// ponytail: the documented exception to the single-mutation-path invariant — a
// view toggle has no business in undo.
pub fn set_layer_visible(state: &mut AppState, layer: LayerId, visible: bool) {
    if state.plugin_busy || state.io_busy {
        return;
    }
    if let Ok(l) = state.current_tab_mut().doc.layer_mut(layer) {
        if l.visible == visible {
            return;
        }
        l.visible = visible;
        state.dirty = true;
        state.mark_current_unsaved();
    }
}

/// Update a layer's opacity, coalescing with the most recent history entry.
///
/// Coalescing only happens while an opacity-edit session is active for the
/// same layer (set by [`start_opacity_edit`]). This ensures that typed edits
/// and separate drags each produce their own undo step.
pub fn set_layer_opacity(state: &mut AppState, layer: LayerId, opacity: f32) {
    if state.plugin_busy || state.io_busy {
        return;
    }
    if state.opacity_drag_layer == Some(layer) {
        let tab = state.current_tab_mut();
        if let Some(top) = tab.history.undo_top_mut() {
            if top.coalesce_opacity(&mut tab.doc, layer, opacity) {
                state.dirty = true;
                let tab_id = state.current_tab().id;
                state.mark_birdseye_tab_dirty(tab_id);
                return;
            }
        }
    }
    let _ = dispatch::dispatch(state, Box::new(SetOpacityCmd::new(layer, opacity)));
}

/// Start a fresh opacity-edit session for `layer`.
///
/// Pushes a new [`SetOpacityCmd`] onto the history stack. Subsequent changes
/// while the same drag is in progress should be routed through
/// [`set_layer_opacity`] so they coalesce into this command.
pub fn start_opacity_edit(state: &mut AppState, layer: LayerId, opacity: f32) {
    if state.plugin_busy || state.io_busy {
        return;
    }
    state.opacity_drag_layer = Some(layer);
    let _ = dispatch::dispatch(state, Box::new(SetOpacityCmd::new(layer, opacity)));
}

/// Dispatch a blend-mode change for `layer`.
pub fn set_layer_blend(state: &mut AppState, layer: LayerId, blend: BlendMode) {
    if state.plugin_busy || state.io_busy {
        return;
    }
    let _ = dispatch::dispatch(state, Box::new(SetBlendCmd::new(layer, blend)));
}

/// Dispatch a layer rename.
pub fn rename_layer(state: &mut AppState, layer: LayerId, name: impl Into<String>) {
    if state.plugin_busy || state.io_busy {
        return;
    }
    let _ = dispatch::dispatch(state, Box::new(RenameCmd::new(layer, name)));
}

/// Add a new raster layer on top and make it active.
pub fn add_raster_layer(state: &mut AppState, name: impl Into<String>) {
    if state.plugin_busy || state.io_busy {
        return;
    }
    let _ = dispatch::dispatch(state, Box::new(AddRasterLayerCmd::new(name)));
}

/// Delete a layer.
pub fn delete_layer(state: &mut AppState, layer: LayerId) {
    if state.plugin_busy || state.io_busy {
        return;
    }
    let _ = dispatch::dispatch(state, Box::new(DeleteLayerCmd::new(layer)));
}

/// Duplicate a raster layer.
pub fn duplicate_layer(state: &mut AppState, layer: LayerId) {
    if state.plugin_busy || state.io_busy {
        return;
    }
    let _ = dispatch::dispatch(state, Box::new(DuplicateLayerCmd::new(layer)));
}

/// Reorder a layer to `new_index` in bottom-to-top z-order.
pub fn reorder_layer(state: &mut AppState, layer: LayerId, new_index: usize) {
    if state.plugin_busy || state.io_busy {
        return;
    }
    let _ = dispatch::dispatch(state, Box::new(ReorderCmd::new(layer, new_index)));
}

/// All blend modes available in the UI dropdown.
const BLEND_MODES: [BlendMode; 13] = [
    BlendMode::Normal,
    BlendMode::Multiply,
    BlendMode::Screen,
    BlendMode::Overlay,
    BlendMode::Darken,
    BlendMode::Lighten,
    BlendMode::ColorDodge,
    BlendMode::ColorBurn,
    BlendMode::HardLight,
    BlendMode::SoftLight,
    BlendMode::Difference,
    BlendMode::Exclusion,
    BlendMode::Add,
];

/// Convert a display-order drop index to a bottom-to-top z-order index.
///
/// `rows_len` is the number of visible layers. `original_display` is the
/// current top-to-bottom index of the dragged layer; `target_display` is the
/// insertion point in the same display order. Returns the index in
/// `doc.order` (bottom-to-top) that corresponds to the drop location.
pub fn display_drop_to_z_index(
    rows_len: usize,
    original_display: usize,
    target_display: usize,
) -> usize {
    if rows_len == 0 {
        return 0;
    }
    let final_display = if target_display <= original_display {
        target_display
    } else {
        target_display.saturating_sub(1)
    }
    .min(rows_len.saturating_sub(1));
    rows_len.saturating_sub(1).saturating_sub(final_display)
}

/// Render the layers panel.
pub fn render_layers(ui: &mut egui::Ui, state: &mut AppState) {
    let rows = layer_rows(&state.tabs[state.active_tab].doc);
    let active = state.doc().active;
    let busy = state.plugin_busy || state.io_busy;

    // Toolbar.
    ui.horizontal(|ui| {
        let add_icon = crate::icons::add_image(16.0, ui.visuals().text_color());
        if ui
            .add_enabled(!busy, egui::Button::image(add_icon))
            .on_hover_text("New raster layer")
            .clicked()
        {
            add_raster_layer(state, "Layer");
        }

        let can_duplicate =
            active.is_some_and(|id| state.doc().layer(id).is_ok_and(|l| l.is_raster()));
        let dup_icon = crate::icons::duplicate_image(16.0, ui.visuals().text_color());
        if ui
            .add_enabled(!busy && can_duplicate, egui::Button::image(dup_icon))
            .on_hover_text("Duplicate layer")
            .clicked()
        {
            if let Some(id) = active {
                duplicate_layer(state, id);
            }
        }

        let can_delete = active.is_some();
        let trash_icon = crate::icons::trash_image(16.0, ui.visuals().text_color());
        if ui
            .add_enabled(!busy && can_delete, egui::Button::image(trash_icon))
            .on_hover_text("Delete layer")
            .clicked()
        {
            if let Some(id) = active {
                delete_layer(state, id);
            }
        }
    });

    ui.separator();

    let mut row_rects: Vec<egui::Rect> = Vec::new();
    let mut drop_target: Option<usize> = None;
    let mut released_layer: Option<LayerId> = None;

    egui::ScrollArea::vertical().show(ui, |ui| {
        // Every control is a plain widget added to a top-aligned row (no
        // `add_sized` cells, which positioned differently from the combo's
        // button frame). The rename field is given the same vertical padding as
        // the buttons so it shares their height instead of sitting short.
        for (display_index, row) in rows.iter().enumerate() {
            // Top-aligned so every control (including the combo's button frame,
            // which positions itself independently of cross-axis centering)
            // pins to the same top edge.
            let row_response = ui.horizontal_top(|ui| {
                // Drag handle — six-dot grip, matching the tool-section reorder
                // icon. Frameless so it doesn't look like a button.
                let handle_h = 16.0 + 2.0 * ui.spacing().button_padding.y;
                let (handle_rect, handle) =
                    ui.allocate_exact_size(egui::vec2(16.0, handle_h), egui::Sense::drag());
                let handle = handle.on_hover_text("Drag to reorder");
                if !busy {
                    if handle.drag_started() {
                        state.dragging_layer = Some(row.id);
                    }
                    if handle.drag_stopped() {
                        released_layer = Some(row.id);
                    }
                }
                if ui.is_rect_visible(handle_rect) {
                    let tint = if busy {
                        ui.visuals().weak_text_color()
                    } else {
                        ui.visuals().text_color()
                    };
                    // Center the grip inside the handle cell.
                    let grip_rect = egui::Rect::from_min_size(
                        egui::pos2(
                            handle_rect.center().x - SIX_DOT_GRIP_SIZE.x / 2.0,
                            handle_rect.center().y - SIX_DOT_GRIP_SIZE.y / 2.0,
                        ),
                        SIX_DOT_GRIP_SIZE,
                    );
                    paint_six_dot_grip(ui.painter(), grip_rect, tint);
                }

                // Visibility toggle — an eye icon: open when visible, crossed-out
                // when hidden. Rendered as a real button, like Duplicate layer.
                let eye = crate::icons::eye_image(row.visible, 16.0, ui.visuals().text_color());
                if ui
                    .add_enabled(!busy, egui::Button::image(eye))
                    .on_hover_text(if row.visible {
                        "Hide layer"
                    } else {
                        "Show layer"
                    })
                    .clicked()
                {
                    set_layer_visible(state, row.id, !row.visible);
                }

                // Lock toggle — a padlock icon: closed when locked, open when
                // unlocked. Rendered as a real button, like Duplicate layer.
                let lock = crate::icons::lock_image(row.locked, 16.0, ui.visuals().text_color());
                if ui
                    .add_enabled(!busy, egui::Button::image(lock))
                    .on_hover_text(if row.locked {
                        "Unlock layer"
                    } else {
                        "Lock layer"
                    })
                    .clicked()
                {
                    let _ = crate::dispatch::dispatch(
                        state,
                        Box::new(ogre_core::SetLayerLockedCmd::new(row.id, !row.locked)),
                    );
                }

                // Name / active selection.
                let name_label = if row.active {
                    egui::RichText::new(&row.name).strong()
                } else {
                    egui::RichText::new(&row.name)
                };
                let name_response = ui.selectable_label(row.active, name_label);
                if name_response.clicked() {
                    // Active layer is view-state; it does not go through history.
                    state.doc_mut().active = Some(row.id);
                    // If a vector-capable tool is active, load the newly
                    // selected layer for re-edit (spec §4).
                    let doc = state.doc().clone();
                    state.tool_manager.load_active_vector_layer(&doc);
                }

                // Opacity drag.
                let mut opacity = row.opacity;
                let opacity_response = ui.add_enabled(
                    !busy,
                    egui::DragValue::new(&mut opacity)
                        .speed(0.01)
                        .range(0.0..=1.0),
                );
                handle_opacity_response(state, row.id, opacity, opacity_response);

                // Blend mode dropdown. Rendered directly on the row (not wrapped
                // in `add_enabled_ui`) so its button frame shares the row's
                // baseline like the other controls — the wrapping child UI
                // offset it downward. Blend changes are already ignored while
                // busy by `set_layer_blend`.
                let mut current_blend = row.blend;
                egui::ComboBox::from_id_salt(format!("blend_{:?}", row.id))
                    .selected_text(row.blend.name())
                    .width(90.0)
                    .show_ui(ui, |ui| {
                        for mode in BLEND_MODES {
                            if ui
                                .selectable_value(&mut current_blend, mode, mode.name())
                                .clicked()
                                && mode != row.blend
                            {
                                set_layer_blend(state, row.id, mode);
                            }
                        }
                    });

                // Rename field — pinned to the row's natural control height with
                // vertically-centered text so it lines up with the rest. The edit
                // buffer lives in egui's frame-local memory so typing does not snap
                // back each frame.
                let rename_id = ui.make_persistent_id(format!("rename_{:?}", row.id));
                let mut name = ui.memory_mut(|mem| {
                    mem.data
                        .get_temp_mut_or(rename_id, row.name.clone())
                        .clone()
                });
                // Match the buttons' padding (incl. the left inset of the text)
                // so "name" lines up with the "1.00" / blend values.
                let bpad = ui.spacing().button_padding;
                let rename_response = ui.add_enabled(
                    !busy,
                    egui::TextEdit::singleline(&mut name)
                        .margin(egui::Margin::symmetric(bpad.x as i8, bpad.y as i8))
                        .vertical_align(egui::Align::Center),
                );
                ui.memory_mut(|mem| {
                    *mem.data.get_temp_mut_or_default::<String>(rename_id) = name.clone()
                });
                if rename_response.lost_focus() && name != row.name {
                    rename_layer(state, row.id, name);
                    ui.memory_mut(|mem| mem.data.remove::<String>(rename_id));
                }
            });

            let rect = row_response.response.rect;
            row_rects.push(rect);

            // Drop target indicator while dragging.
            if state.dragging_layer.is_some() {
                if let Some(pos) = ui.input(|i| i.pointer.hover_pos()) {
                    if pos.y < rect.center().y && drop_target.is_none() {
                        drop_target = Some(display_index);
                    }
                }
            }
        }
    });

    // Default drop target: below the last row.
    if state.dragging_layer.is_some() && drop_target.is_none() && !row_rects.is_empty() {
        drop_target = Some(rows.len());
    }

    // Draw the drop indicator between rows (or after the last row).
    if state.dragging_layer.is_some() {
        if let Some(target) = drop_target {
            let y = if target < row_rects.len() {
                row_rects[target].top()
            } else if let Some(last) = row_rects.last() {
                last.bottom()
            } else {
                0.0
            };
            if let Some(first) = row_rects.first() {
                let min_x = first.left();
                let max_x = first.right();
                ui.painter().hline(
                    min_x..=max_x,
                    y,
                    egui::Stroke::new(2.0, egui::Color32::YELLOW),
                );
            }
        }
    }

    // Finalize drag-to-reorder only when the dragged handle is released.
    if released_layer.is_some() {
        if let (Some(dragged), Some(target)) = (state.dragging_layer, drop_target) {
            if released_layer == Some(dragged) {
                if let Some(original_display) = rows.iter().position(|r| r.id == dragged) {
                    let z_target = display_drop_to_z_index(rows.len(), original_display, target);
                    let current_z = state
                        .doc()
                        .order
                        .iter()
                        .position(|&id| id == dragged)
                        .unwrap_or(0);
                    if z_target != current_z {
                        reorder_layer(state, dragged, z_target);
                    }
                }
            }
        }
        state.dragging_layer = None;
    }
}

/// Route an opacity drag-value response through the coalescing helper.
///
/// A new history entry is started when the user begins a drag; subsequent
/// changes while the drag continues coalesce into the same entry. Separate
/// drags therefore produce separate undo steps.
fn handle_opacity_response(
    state: &mut AppState,
    layer: LayerId,
    opacity: f32,
    response: egui::Response,
) {
    if state.plugin_busy || state.io_busy {
        return;
    }
    if response.drag_started() {
        start_opacity_edit(state, layer, opacity);
    } else if response.changed() {
        set_layer_opacity(state, layer, opacity);
    }

    if response.drag_stopped() || response.lost_focus() {
        state.opacity_drag_layer = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_rows_returns_top_to_bottom() {
        let mut doc = ogre_core::Document::new(100, 100);
        let a = doc.add_raster_layer("A");
        let b = doc.add_raster_layer("B");
        let c = doc.add_raster_layer("C");
        doc.active = Some(b);

        let rows = layer_rows(&doc);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].id, c);
        assert_eq!(rows[0].name, "C");
        assert!(!rows[0].active);
        assert_eq!(rows[1].id, b);
        assert!(rows[1].active);
        assert_eq!(rows[2].id, a);
    }

    #[test]
    fn set_visible_toggles_without_recording_undo() {
        let mut state = AppState::new_document(100, 100);
        let id = state.doc().order[0];
        state.dirty = false;

        set_layer_visible(&mut state, id, false);

        assert!(!state.doc().layer(id).unwrap().visible);
        assert!(state.dirty, "renderer must recomposite");
        // Visibility is a view toggle, not an undoable document edit.
        assert_eq!(state.history().undo_len(), 0);
    }

    #[test]
    fn set_blend_dispatches_command_and_sets_dirty() {
        let mut state = AppState::new_document(100, 100);
        let id = state.doc().order[0];
        state.dirty = false;

        set_layer_blend(&mut state, id, BlendMode::Multiply);

        assert_eq!(state.doc().layer(id).unwrap().blend, BlendMode::Multiply);
        assert!(state.dirty);
        assert_eq!(state.history().undo_len(), 1);
    }

    #[test]
    fn rename_dispatches_command_and_sets_dirty() {
        let mut state = AppState::new_document(100, 100);
        let id = state.doc().order[0];
        state.dirty = false;

        rename_layer(&mut state, id, "Renamed");

        assert_eq!(state.doc().layer(id).unwrap().name, "Renamed");
        assert!(state.dirty);
        assert_eq!(state.history().undo_len(), 1);
    }

    #[test]
    fn opacity_drag_produces_one_history_entry() {
        let mut state = AppState::new_document(100, 100);
        let id = state.doc().order[0];
        state.dirty = false;

        start_opacity_edit(&mut state, id, 0.5);
        assert!(state.dirty);
        assert_eq!(state.doc().layer(id).unwrap().opacity, 0.5);
        assert_eq!(state.history().undo_len(), 1);

        state.dirty = false;
        set_layer_opacity(&mut state, id, 0.6);
        assert!(state.dirty);
        assert_eq!(state.doc().layer(id).unwrap().opacity, 0.6);
        assert_eq!(state.history().undo_len(), 1);

        state.dirty = false;
        set_layer_opacity(&mut state, id, 0.7);
        assert!(state.dirty);
        assert_eq!(state.doc().layer(id).unwrap().opacity, 0.7);
        assert_eq!(state.history().undo_len(), 1);

        dispatch::undo(&mut state);
        assert_eq!(state.doc().layer(id).unwrap().opacity, 1.0);
    }

    #[test]
    fn opacity_typing_without_drag_creates_separate_history_entry() {
        let mut state = AppState::new_document(100, 100);
        let id = state.doc().order[0];

        // Simulate a completed drag.
        start_opacity_edit(&mut state, id, 0.5);
        state.opacity_drag_layer = None;

        // A subsequent typed edit must not coalesce into the drag's command.
        set_layer_opacity(&mut state, id, 0.7);
        assert_eq!(state.history().undo_len(), 2);
        assert_eq!(state.doc().layer(id).unwrap().opacity, 0.7);
    }

    #[test]
    fn add_raster_layer_makes_new_layer_active() {
        let mut state = AppState::new_document(100, 100);
        let bg = state.doc().order[0];
        state.dirty = false;

        add_raster_layer(&mut state, "New");

        assert_eq!(state.doc().order.len(), 2);
        let new_id = state.doc().order[1];
        assert_eq!(state.doc().layer(new_id).unwrap().name, "New");
        assert_eq!(state.doc().active, Some(new_id));
        assert!(state.dirty);
        assert_eq!(state.history().undo_len(), 1);

        dispatch::undo(&mut state);
        assert_eq!(state.doc().active, Some(bg));
        assert_eq!(state.doc().order.len(), 1);
    }

    #[test]
    fn delete_layer_dispatches_command() {
        let mut state = AppState::new_document(100, 100);
        let id = state.doc().order[0];
        state.dirty = false;

        delete_layer(&mut state, id);

        assert!(!state.doc().order.contains(&id));
        assert!(state.doc().active.is_none());
        assert!(state.dirty);
        assert_eq!(state.history().undo_len(), 1);

        dispatch::undo(&mut state);
        assert_eq!(state.doc().order, vec![id]);
        assert_eq!(state.doc().active, Some(id));
    }

    #[test]
    fn duplicate_layer_dispatches_command() {
        let mut state = AppState::new_document(100, 100);
        let src = state.doc().order[0];
        state.dirty = false;

        duplicate_layer(&mut state, src);

        assert_eq!(state.doc().order.len(), 2);
        let dup = state.doc().order[1];
        assert_eq!(state.doc().layer(dup).unwrap().name, "Background copy");
        assert_eq!(state.doc().active, Some(dup));
        assert!(state.dirty);
        assert_eq!(state.history().undo_len(), 1);

        dispatch::undo(&mut state);
        assert_eq!(state.doc().active, Some(src));
        assert_eq!(state.doc().order.len(), 1);
    }

    #[test]
    fn separate_opacity_edits_produce_separate_history_entries() {
        let mut state = AppState::new_document(100, 100);
        let id = state.doc().order[0];

        // First edit: simulate a completed drag by starting fresh and updating.
        start_opacity_edit(&mut state, id, 0.5);
        assert_eq!(state.history().undo_len(), 1);

        // A separate edit (e.g., the user releases and starts a new drag) must
        // create a new history entry, not coalesce into the previous one.
        start_opacity_edit(&mut state, id, 0.7);
        assert_eq!(state.history().undo_len(), 2);

        dispatch::undo(&mut state);
        assert_eq!(state.doc().layer(id).unwrap().opacity, 0.5);
    }

    #[test]
    fn reorder_layer_dispatches_command_and_restores_on_undo() {
        let mut state = AppState::new_document(100, 100);
        let bg = state.doc().order[0];
        let a = state.doc_mut().add_raster_layer("A");
        let b = state.doc_mut().add_raster_layer("B");
        let c = state.doc_mut().add_raster_layer("C");
        state.dirty = false;

        // Move a to z-index 2 (between bg and b).
        reorder_layer(&mut state, a, 2);

        assert_eq!(state.doc().order, vec![bg, b, a, c]);
        assert!(state.dirty);
        assert_eq!(state.history().undo_len(), 1);

        dispatch::undo(&mut state);
        assert_eq!(state.doc().order, vec![bg, a, b, c]);
    }

    #[test]
    fn display_drop_to_z_index_maps_top_to_bottom_to_bottom_to_top() {
        // Four layers displayed top-to-bottom as [C, B, A, bg].
        // Dropping at display index 0 (above C) => z-index 3 (top).
        assert_eq!(display_drop_to_z_index(4, 0, 0), 3);
        // Dropping at display index 3 (below bg) => z-index 0 (bottom).
        assert_eq!(display_drop_to_z_index(4, 0, 4), 0);
        // Dropping at display index 2 (below B) => z-index 2.
        assert_eq!(display_drop_to_z_index(4, 0, 2), 2);
    }

    #[test]
    fn display_drop_to_z_index_accounts_for_dragged_row() {
        // Drag top row C (display 0) to display 3 (below A). After removing C,
        // the final display position is 2, which maps to z-index 1.
        assert_eq!(display_drop_to_z_index(4, 0, 3), 1);
        // Drag bottom row bg (display 3) to display 0 (above C). target <=
        // original, so final display stays 0 => z-index 3.
        assert_eq!(display_drop_to_z_index(4, 3, 0), 3);
        // Dropping at the same position is a no-op (z-index unchanged).
        assert_eq!(display_drop_to_z_index(4, 1, 1), 2);
    }

    #[test]
    fn display_drop_to_z_index_handles_single_layer() {
        assert_eq!(display_drop_to_z_index(1, 0, 0), 0);
        assert_eq!(display_drop_to_z_index(1, 0, 5), 0);
    }

    #[test]
    fn display_drop_to_z_index_handles_middle_row_and_oversized_target() {
        // Four rows [C, B, A, bg]; drag B (display 1) to display 0 (above C).
        // target <= original, so final_display = 0 => z-index 3.
        assert_eq!(display_drop_to_z_index(4, 1, 0), 3);
        // Drag B to far past the end => clamped to bottom.
        assert_eq!(display_drop_to_z_index(4, 1, 100), 0);
        // Drag A (display 2) to display 1 (between C and B).
        // target <= original, final_display = 1 => z-index 2.
        assert_eq!(display_drop_to_z_index(4, 2, 1), 2);
    }

    #[test]
    fn display_drop_to_z_index_empty_is_zero() {
        assert_eq!(display_drop_to_z_index(0, 0, 0), 0);
    }
}
