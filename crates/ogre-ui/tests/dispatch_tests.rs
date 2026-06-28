// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! UI-level command dispatch integration tests.
//!
//! These tests exercise the public `ogre-ui` controller surface: dispatch,
//! undo/redo, and menu/keyboard shortcut handlers.

use ogre_core::{IVec2, MoveLayerByCmd, Selection};
use ogre_ui::dispatch;
use ogre_ui::shell::{self, Shortcut};
use ogre_ui::state::AppState;

#[test]
fn dispatch_applies_command_and_marks_dirty() {
    let mut state = AppState::new_document(256, 256);
    let id = state.doc().active.unwrap();

    dispatch::dispatch(
        &mut state,
        Box::new(MoveLayerByCmd::new(id, IVec2::new(3, 4))),
    )
    .unwrap();

    assert_eq!(state.doc().layer(id).unwrap().offset, IVec2::new(3, 4));
    assert!(state.dirty);
}

#[test]
fn undo_redo_restores_state_and_marks_dirty() {
    let mut state = AppState::new_document(256, 256);
    let id = state.doc().active.unwrap();

    dispatch::dispatch(
        &mut state,
        Box::new(MoveLayerByCmd::new(id, IVec2::new(10, 20))),
    )
    .unwrap();
    state.dirty = false;

    assert!(dispatch::undo(&mut state).is_some());
    assert_eq!(state.doc().layer(id).unwrap().offset, IVec2::ZERO);
    assert!(state.dirty);

    state.dirty = false;
    assert!(dispatch::redo(&mut state).is_some());
    assert_eq!(state.doc().layer(id).unwrap().offset, IVec2::new(10, 20));
    assert!(state.dirty);
}

#[test]
fn shortcut_handlers_route_through_dispatch() {
    let mut state = AppState::new_document(100, 100);
    let canvas = state.doc().canvas;

    assert!(shell::handle_shortcut(&mut state, Shortcut::SelectAll));
    assert_eq!(state.doc().selection, Selection::select_all(canvas));
    assert!(state.dirty);

    state.dirty = false;
    assert!(shell::handle_shortcut(&mut state, Shortcut::Deselect));
    assert!(state.doc().selection.is_empty());
    assert!(state.dirty);
}
