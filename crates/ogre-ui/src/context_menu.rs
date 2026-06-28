// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Canvas context menu.
//!
//! The right-click menu on the canvas offers "Copy to New Layer",
//! "Cut to New Layer", and "Deselect". All enabled/disabled logic is
//! computed from the current [`AppState`] and all actions route through the
//! single command dispatch path.

use crate::dispatch;
use crate::state::AppState;

/// A single entry in the canvas context menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextMenuItem {
    /// Display label.
    pub label: &'static str,
    /// Whether the item can be activated right now.
    pub enabled: bool,
    /// Unique identifier used to dispatch the action.
    pub action: ContextAction,
}

/// Action triggered by a context menu item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextAction {
    /// Copy the current selection to a new raster layer.
    CopyToNewLayer,
    /// Cut the current selection to a new raster layer.
    CutToNewLayer,
    /// Invert the current selection within the canvas.
    SelectInverse,
    /// Clear the current selection.
    Deselect,
}

/// Build the right-click context menu items for the current state.
///
/// Copy/cut are enabled only when there is a non-empty selection and a raster
/// layer is active; cut is additionally disabled when that layer is locked.
/// Select Inverse and Deselect are enabled when a selection exists.
pub fn context_menu_items(state: &AppState) -> Vec<ContextMenuItem> {
    let has_selection = !state.doc().selection.is_empty();
    let active_raster = active_raster_layer(state);
    let can_copy = has_selection && active_raster.is_some();
    let can_cut = has_selection && active_raster.is_some_and(|l| !l.locked);
    vec![
        ContextMenuItem {
            label: "Copy to New Layer",
            enabled: can_copy,
            action: ContextAction::CopyToNewLayer,
        },
        ContextMenuItem {
            label: "Cut to New Layer",
            enabled: can_cut,
            action: ContextAction::CutToNewLayer,
        },
        ContextMenuItem {
            label: "Select Inverse",
            enabled: has_selection,
            action: ContextAction::SelectInverse,
        },
        ContextMenuItem {
            label: "Deselect",
            enabled: has_selection,
            action: ContextAction::Deselect,
        },
    ]
}

/// Build the left-click-inside-selection menu items: quick "Select Inverse" and
/// "Deselect" for when the user clicks (without dragging) inside a selection.
pub fn left_click_menu_items(state: &AppState) -> Vec<ContextMenuItem> {
    let has_selection = !state.doc().selection.is_empty();
    vec![
        ContextMenuItem {
            label: "Select Inverse",
            enabled: has_selection,
            action: ContextAction::SelectInverse,
        },
        ContextMenuItem {
            label: "Deselect",
            enabled: has_selection,
            action: ContextAction::Deselect,
        },
    ]
}

fn active_raster_layer(state: &AppState) -> Option<&ogre_core::Layer> {
    let active = state.doc().active?;
    state.doc().layer(active).ok().filter(|l| l.is_raster())
}

/// Apply a context-menu action to `state`.
pub fn apply_context_action(state: &mut AppState, action: ContextAction) {
    match action {
        ContextAction::Deselect => {
            let _ = dispatch::dispatch(
                state,
                Box::new(ogre_core::SetSelectionCmd::new(ogre_core::Selection::none())),
            );
        }
        ContextAction::SelectInverse => {
            let _ = dispatch::dispatch(state, Box::new(ogre_core::InvertSelectionCmd::new()));
        }
        ContextAction::CopyToNewLayer => {
            let Some(active) = active_raster_layer_id(state) else {
                return;
            };
            let _ = dispatch::dispatch(
                state,
                Box::new(ogre_core::CopyToNewLayerCmd::new(
                    active,
                    state.doc().selection.clone(),
                )),
            );
        }
        ContextAction::CutToNewLayer => {
            let Some(active) = active_raster_layer_id(state) else {
                return;
            };
            let _ = dispatch::dispatch(
                state,
                Box::new(ogre_core::CutToNewLayerCmd::new(
                    active,
                    state.doc().selection.clone(),
                )),
            );
        }
    }
}

fn active_raster_layer_id(state: &AppState) -> Option<ogre_core::LayerId> {
    let active = state.doc().active?;
    state
        .doc()
        .layer(active)
        .ok()
        .filter(|l| l.is_raster())
        .map(|_| active)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ogre_core::{Rect, Selection};

    #[test]
    fn context_menu_items_disabled_with_no_selection() {
        let state = AppState::new_document(100, 100);
        let items = context_menu_items(&state);
        assert!(!items[0].enabled); // Copy
        assert!(!items[1].enabled); // Cut
        assert!(!items[2].enabled); // Deselect
    }

    #[test]
    fn context_menu_items_enabled_with_selection_and_raster_layer() {
        let mut state = AppState::new_document(100, 100);
        let _active = state.doc().active.unwrap();
        dispatch::dispatch(
            &mut state,
            Box::new(ogre_core::SetSelectionCmd::new(Selection::rect(Rect::new(
                0, 0, 10, 10,
            )))),
        )
        .unwrap();

        let items = context_menu_items(&state);
        assert_eq!(items.len(), 4);
        assert!(items.iter().all(|i| i.enabled));
        assert_eq!(items[0].action, ContextAction::CopyToNewLayer);
        assert_eq!(items[1].action, ContextAction::CutToNewLayer);
        assert_eq!(items[2].action, ContextAction::SelectInverse);
        assert_eq!(items[3].action, ContextAction::Deselect);
    }

    #[test]
    fn select_inverse_action_inverts_the_selection() {
        let mut state = AppState::new_document(20, 20);
        dispatch::dispatch(
            &mut state,
            Box::new(ogre_core::SetSelectionCmd::new(Selection::rect(Rect::new(
                0, 0, 5, 5,
            )))),
        )
        .unwrap();
        // (2,2) selected, (10,10) not.
        assert!(
            state
                .doc()
                .selection
                .coverage_at(ogre_core::IVec2::new(2, 2))
                > 0.0
        );
        apply_context_action(&mut state, ContextAction::SelectInverse);
        // After inverting within the canvas, the relationship flips.
        assert_eq!(
            state
                .doc()
                .selection
                .coverage_at(ogre_core::IVec2::new(2, 2)),
            0.0
        );
        assert!(
            state
                .doc()
                .selection
                .coverage_at(ogre_core::IVec2::new(10, 10))
                > 0.0
        );
    }

    #[test]
    fn cut_disabled_when_active_layer_is_locked() {
        let mut state = AppState::new_document(100, 100);
        let active = state.doc().active.unwrap();
        state.doc_mut().layer_mut(active).unwrap().locked = true;
        dispatch::dispatch(
            &mut state,
            Box::new(ogre_core::SetSelectionCmd::new(Selection::rect(Rect::new(
                0, 0, 10, 10,
            )))),
        )
        .unwrap();

        let items = context_menu_items(&state);
        assert!(items[0].enabled); // Copy still works on a locked layer.
        assert!(!items[1].enabled); // Cut is disabled.
    }

    #[test]
    fn deselect_works_without_raster_active_layer() {
        let mut state = AppState::new_document(100, 100);
        let canvas = state.doc().canvas;
        dispatch::dispatch(
            &mut state,
            Box::new(ogre_core::SetSelectionCmd::new(Selection::select_all(
                canvas,
            ))),
        )
        .unwrap();
        // Simulate the active layer being a group (no raster active).
        state.doc_mut().active = None;

        apply_context_action(&mut state, ContextAction::Deselect);
        assert!(state.doc().selection.is_empty());
    }

    #[test]
    fn copy_to_new_layer_preserves_pixel_position() {
        let mut state = AppState::new_document(2000, 1500);
        let active = state.doc().active.unwrap();
        let target = ogre_core::IVec2::new(1000, 700);
        state
            .doc_mut()
            .layer_mut(active)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(target, ogre_core::Rgba32F::new(0.2, 0.4, 0.9, 1.0));

        dispatch::dispatch(
            &mut state,
            Box::new(ogre_core::SetSelectionCmd::new(Selection::rect(Rect::new(
                990, 690, 40, 40,
            )))),
        )
        .unwrap();

        apply_context_action(&mut state, ContextAction::CopyToNewLayer);

        let new_id = state.doc().active.unwrap();
        assert_ne!(new_id, active);
        assert_eq!(
            state
                .doc()
                .layer(new_id)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(target),
            ogre_core::Rgba32F::new(0.2, 0.4, 0.9, 1.0)
        );
        // Source is unchanged.
        assert_eq!(
            state
                .doc()
                .layer(active)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(target),
            ogre_core::Rgba32F::new(0.2, 0.4, 0.9, 1.0)
        );
    }

    #[test]
    fn cut_to_new_layer_clears_source() {
        let mut state = AppState::new_document(2000, 1500);
        let active = state.doc().active.unwrap();
        let target = ogre_core::IVec2::new(1000, 700);
        state
            .doc_mut()
            .layer_mut(active)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(target, ogre_core::Rgba32F::new(0.2, 0.4, 0.9, 1.0));

        dispatch::dispatch(
            &mut state,
            Box::new(ogre_core::SetSelectionCmd::new(Selection::rect(Rect::new(
                990, 690, 40, 40,
            )))),
        )
        .unwrap();

        apply_context_action(&mut state, ContextAction::CutToNewLayer);

        let new_id = state.doc().active.unwrap();
        assert_eq!(
            state
                .doc()
                .layer(new_id)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(target),
            ogre_core::Rgba32F::new(0.2, 0.4, 0.9, 1.0)
        );
        assert_eq!(
            state
                .doc()
                .layer(active)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(target),
            ogre_core::Rgba32F::TRANSPARENT
        );
    }
}
