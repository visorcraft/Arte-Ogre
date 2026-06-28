// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! The single command dispatch path for the Arte Ogre UI.
//!
//! All undoable edits to the document must go through [`dispatch`]. Direct
//! mutation of [`AppState::doc`](crate::state::AppState::doc) is reserved for
//! internal command implementations.

use ogre_core::Command;

use crate::state::AppState;

/// Apply `cmd` to the document and mark the renderer dirty.
///
/// The command is pushed onto `state.history()`. On success `state.dirty` is set
/// to `true`.
///
/// While a background plugin is running this returns
/// [`OgreError::Busy`](ogre_core::OgreError::Busy) so that edits cannot
/// interleave with an asynchronous plugin result.
pub fn dispatch(state: &mut AppState, cmd: Box<dyn Command>) -> ogre_core::Result<()> {
    if state.plugin_busy {
        return Err(ogre_core::OgreError::Busy("a plugin is currently running"));
    }
    if state.io_busy {
        return Err(ogre_core::OgreError::Busy("I/O is in progress"));
    }
    if state.bg_removal_rx.is_some() {
        return Err(ogre_core::OgreError::Busy("removing background"));
    }
    let tab = state.current_tab_mut();
    tab.history.do_command(&mut tab.doc, cmd)?;
    state.dirty = true;
    state.mark_current_unsaved();
    let tab_id = state.current_tab().id;
    state.mark_birdseye_tab_dirty(tab_id);
    Ok(())
}

/// Apply `cmd` to a specific tab identified by its stable
/// [`DocumentTab::id`](crate::state::DocumentTab::id), rather than the active
/// tab.
///
/// This backs cross-document operations such as Bird's Eye View layer
/// move/copy, where the affected document is not necessarily the active one.
/// On success the target tab is marked unsaved, `state.dirty` is set, and the
/// target tab's Bird's Eye thumbnails are invalidated.
///
/// Returns [`OgreError::Busy`](ogre_core::OgreError::Busy) while a background
/// job is running, or [`OgreError::LayerNotFound`](ogre_core::OgreError::LayerNotFound)
/// if no tab has the given id.
pub fn dispatch_to_tab_id(
    state: &mut AppState,
    tab_id: u64,
    cmd: Box<dyn Command>,
) -> ogre_core::Result<()> {
    if state.is_busy() {
        return Err(ogre_core::OgreError::Busy("a background job is running"));
    }
    let Some(tab) = state.tabs.iter_mut().find(|t| t.id == tab_id) else {
        return Err(ogre_core::OgreError::InvalidOperation("tab not found"));
    };
    tab.history.do_command(&mut tab.doc, cmd)?;
    tab.unsaved = true;
    state.dirty = true;
    state.mark_birdseye_tab_dirty(tab_id);
    Ok(())
}

/// Dispatch `cmd`, and on failure surface a short human-readable message in
/// `state.error_feedback` so the UI can show *why* a menu action did nothing
/// (instead of silently swallowing the error). Clears any prior feedback on
/// success.
pub fn dispatch_or_report(state: &mut AppState, cmd: Box<dyn Command>) {
    match dispatch(state, cmd) {
        Ok(()) => state.error_feedback = None,
        Err(e) => state.error_feedback = Some(e.to_string()),
    }
}

/// Undo the most recent command.
///
/// Returns the label of the undone command and sets `state.dirty` to `true` if
/// a command was undone. Returns `None` if a background operation is in flight
/// and mutating the document would race with its result.
pub fn undo(state: &mut AppState) -> Option<&'static str> {
    if state.is_busy() {
        return None;
    }
    let tab = state.current_tab_mut();
    let label = tab.history.undo(&mut tab.doc);
    if label.is_some() {
        state.dirty = true;
        state.mark_current_unsaved();
        let tab_id = state.current_tab().id;
        state.mark_birdseye_tab_dirty(tab_id);
    }
    label
}

/// Redo the most recently undone command.
///
/// Returns the label of the redone command and sets `state.dirty` to `true` if
/// a command was redone. Returns `None` if a background operation is in flight.
pub fn redo(state: &mut AppState) -> Option<&'static str> {
    if state.is_busy() {
        return None;
    }
    let tab = state.current_tab_mut();
    let label = tab.history.redo(&mut tab.doc);
    if label.is_some() {
        state.dirty = true;
        state.mark_current_unsaved();
        let tab_id = state.current_tab().id;
        state.mark_birdseye_tab_dirty(tab_id);
    }
    label
}

#[cfg(test)]
mod tests {
    use super::*;
    use ogre_core::{IVec2, MoveLayerByCmd};

    #[test]
    fn dispatch_applies_command_sets_dirty_and_pushes_history() {
        let mut state = AppState::new_document(100, 100);
        let id = state.doc().active.unwrap();

        dispatch(
            &mut state,
            Box::new(MoveLayerByCmd::new(id, IVec2::new(7, 9))),
        )
        .unwrap();

        assert_eq!(state.doc().layer(id).unwrap().offset, IVec2::new(7, 9));
        assert_eq!(state.history().undo_len(), 1);
        assert!(state.dirty);
    }

    #[test]
    fn undo_redo_cycle_restores_state_and_sets_dirty() {
        let mut state = AppState::new_document(100, 100);
        let id = state.doc().active.unwrap();

        dispatch(
            &mut state,
            Box::new(MoveLayerByCmd::new(id, IVec2::new(5, 5))),
        )
        .unwrap();
        state.dirty = false; // simulate a render

        let label = undo(&mut state);
        assert!(label.is_some());
        assert_eq!(state.doc().layer(id).unwrap().offset, IVec2::ZERO);
        assert!(state.dirty);

        state.dirty = false;
        let label = redo(&mut state);
        assert!(label.is_some());
        assert_eq!(state.doc().layer(id).unwrap().offset, IVec2::new(5, 5));
        assert!(state.dirty);
    }

    #[test]
    fn dispatch_rejects_commands_while_plugin_busy() {
        let mut state = AppState::new_document(100, 100);
        let id = state.doc().active.unwrap();
        state.plugin_busy = true;

        let result = dispatch(
            &mut state,
            Box::new(MoveLayerByCmd::new(id, IVec2::new(3, 3))),
        );
        assert!(matches!(result, Err(ogre_core::OgreError::Busy(_))));
        assert_eq!(state.doc().layer(id).unwrap().offset, IVec2::ZERO);
    }

    #[test]
    fn dispatch_rejects_commands_while_io_busy() {
        let mut state = AppState::new_document(100, 100);
        let id = state.doc().active.unwrap();
        state.io_busy = true;

        let result = dispatch(
            &mut state,
            Box::new(MoveLayerByCmd::new(id, IVec2::new(3, 3))),
        );
        assert!(matches!(result, Err(ogre_core::OgreError::Busy(_))));
        assert_eq!(state.doc().layer(id).unwrap().offset, IVec2::ZERO);
    }

    #[test]
    fn undo_redo_are_no_ops_while_background_operation_is_busy() {
        let mut state = AppState::new_document(100, 100);
        let id = state.doc().active.unwrap();
        dispatch(
            &mut state,
            Box::new(MoveLayerByCmd::new(id, IVec2::new(5, 5))),
        )
        .unwrap();

        state.plugin_busy = true;
        assert!(undo(&mut state).is_none());
        state.plugin_busy = false;
        state.io_busy = true;
        assert!(redo(&mut state).is_none());

        state.io_busy = false;
        assert!(undo(&mut state).is_some());
    }

    #[test]
    fn dispatch_and_undo_blocked_during_background_removal() {
        let mut state = AppState::new_document(100, 100);
        let id = state.doc().active.unwrap();
        dispatch(
            &mut state,
            Box::new(MoveLayerByCmd::new(id, IVec2::new(5, 5))),
        )
        .unwrap();

        // A pending background-removal job blocks edits, undo, and redo so the
        // document cannot change under the worker.
        let (_tx, rx) = std::sync::mpsc::channel::<Option<ogre_core::TiledBuffer>>();
        state.bg_removal_rx = Some(rx);
        assert!(state.is_busy());
        assert!(matches!(
            dispatch(
                &mut state,
                Box::new(MoveLayerByCmd::new(id, IVec2::new(1, 1)))
            ),
            Err(ogre_core::OgreError::Busy(_))
        ));
        assert!(undo(&mut state).is_none());
        assert!(redo(&mut state).is_none());
        // The layer never moved past the first edit.
        assert_eq!(state.doc().layer(id).unwrap().offset, IVec2::new(5, 5));
    }

    #[test]
    fn dispatch_marks_document_unsaved() {
        let mut state = AppState::new_document(100, 100);
        let id = state.doc().active.unwrap();
        state.current_tab_mut().unsaved = false;
        dispatch(
            &mut state,
            Box::new(MoveLayerByCmd::new(id, IVec2::new(2, 2))),
        )
        .unwrap();
        assert!(
            state.current_tab().unsaved,
            "an edit must mark the document unsaved"
        );
    }

    #[test]
    fn dispatch_undo_redo_invalidate_birdseye_cache() {
        let mut state = AppState::new_document(64, 64);
        let id = state.doc().active.unwrap();
        let tab_id = state.current_tab().id;

        // Seed a cached thumbnail for the active tab so we can observe it being
        // dropped by an edit.
        let ctx = egui::Context::default();
        let tex = ctx.load_texture(
            "test",
            egui::ColorImage::new([1, 1], vec![egui::Color32::WHITE]),
            egui::TextureOptions::default(),
        );
        state.birds_eye.thumbnails.insert((tab_id, id), tex);

        dispatch(
            &mut state,
            Box::new(MoveLayerByCmd::new(id, IVec2::new(1, 1))),
        )
        .unwrap();
        assert!(
            !state.birds_eye.thumbnails.contains_key(&(tab_id, id)),
            "dispatch must invalidate the active tab's thumbnails"
        );

        // Re-seed and confirm undo and redo also invalidate.
        let tex = ctx.load_texture(
            "test2",
            egui::ColorImage::new([1, 1], vec![egui::Color32::WHITE]),
            egui::TextureOptions::default(),
        );
        state.birds_eye.thumbnails.insert((tab_id, id), tex);
        assert!(undo(&mut state).is_some());
        assert!(!state.birds_eye.thumbnails.contains_key(&(tab_id, id)));

        let tex = ctx.load_texture(
            "test3",
            egui::ColorImage::new([1, 1], vec![egui::Color32::WHITE]),
            egui::TextureOptions::default(),
        );
        state.birds_eye.thumbnails.insert((tab_id, id), tex);
        assert!(redo(&mut state).is_some());
        assert!(!state.birds_eye.thumbnails.contains_key(&(tab_id, id)));
    }

    #[test]
    fn dispatch_or_report_surfaces_failure_and_clears_on_success() {
        let mut state = AppState::new_document(100, 100);
        // MergeDownCmd on the sole layer fails (no layer below).
        let id = state.doc().active.unwrap();
        dispatch_or_report(&mut state, Box::new(ogre_core::MergeDownCmd::new(id)));
        assert!(
            state.error_feedback.is_some(),
            "failure must populate error_feedback"
        );

        // A successful dispatch clears it.
        dispatch_or_report(
            &mut state,
            Box::new(MoveLayerByCmd::new(id, IVec2::new(1, 1))),
        );
        assert!(state.error_feedback.is_none(), "success clears feedback");
    }

    /// Open a second tab so the first becomes non-active. Returns
    /// `(first_tab_id, first_layer_id)`.
    fn two_tab_state() -> (AppState, u64, ogre_core::LayerId) {
        let mut state = AppState::new_document(64, 64);
        let first_id = state.current_tab().id;
        let first_layer = state.doc().active.unwrap();
        // Give the first tab history so `new_blank_document` opens a second tab
        // instead of reusing the (otherwise pristine) first one.
        dispatch(
            &mut state,
            Box::new(MoveLayerByCmd::new(first_layer, IVec2::new(1, 1))),
        )
        .unwrap();
        state.new_blank_document((64, 64));
        assert_ne!(
            state.current_tab().id,
            first_id,
            "second tab must be active"
        );
        (state, first_id, first_layer)
    }

    #[test]
    fn dispatch_to_tab_id_changes_only_target_tab() {
        let (mut state, first_id, first_layer) = two_tab_state();
        let second_layer = state.doc().active.unwrap();
        let second_offset = state.doc().layer(second_layer).unwrap().offset;

        dispatch_to_tab_id(
            &mut state,
            first_id,
            Box::new(MoveLayerByCmd::new(first_layer, IVec2::new(5, 0))),
        )
        .unwrap();

        let first_tab = state.tabs.iter().find(|t| t.id == first_id).unwrap();
        assert_eq!(
            first_tab.doc.layer(first_layer).unwrap().offset,
            IVec2::new(6, 1)
        );
        // The active (second) tab is untouched.
        assert_eq!(
            state.doc().layer(second_layer).unwrap().offset,
            second_offset
        );
    }

    #[test]
    fn dispatch_to_tab_id_marks_target_unsaved() {
        let (mut state, first_id, first_layer) = two_tab_state();
        state
            .tabs
            .iter_mut()
            .find(|t| t.id == first_id)
            .unwrap()
            .unsaved = false;

        dispatch_to_tab_id(
            &mut state,
            first_id,
            Box::new(MoveLayerByCmd::new(first_layer, IVec2::new(2, 2))),
        )
        .unwrap();

        assert!(
            state
                .tabs
                .iter()
                .find(|t| t.id == first_id)
                .unwrap()
                .unsaved,
            "the target tab must become unsaved"
        );
    }

    #[test]
    fn dispatch_to_tab_id_rejects_while_busy() {
        let (mut state, first_id, first_layer) = two_tab_state();
        state.plugin_busy = true;
        let result = dispatch_to_tab_id(
            &mut state,
            first_id,
            Box::new(MoveLayerByCmd::new(first_layer, IVec2::new(3, 3))),
        );
        assert!(matches!(result, Err(ogre_core::OgreError::Busy(_))));
    }

    #[test]
    fn dispatch_to_tab_id_rejects_unknown_tab() {
        let mut state = AppState::new_document(64, 64);
        let layer = state.doc().active.unwrap();
        let result = dispatch_to_tab_id(
            &mut state,
            9_999,
            Box::new(MoveLayerByCmd::new(layer, IVec2::new(1, 1))),
        );
        assert!(matches!(
            result,
            Err(ogre_core::OgreError::InvalidOperation(_))
        ));
    }
}
