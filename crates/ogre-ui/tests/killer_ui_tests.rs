// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! End-to-end controller test for the core UI workflow.
//!
//! Exercises the product loop minus pixels-on-screen: create a document, paint,
//! rectangle-select, cut to a new layer, move the new layer, then undo/redo and
//! assert the document returns to the pre-undo snapshot.

use ogre_core::{IVec2, PaintCmd, Rgba32F};
use ogre_ui::context_menu::{apply_context_action, ContextAction};
use ogre_ui::dispatch;
use ogre_ui::panels::layers::add_raster_layer;
use ogre_ui::state::AppState;
use ogre_ui::tools::{Phase, PointerEvent, Tool};

#[test]
fn cut_to_new_layer_undo_redo_round_trip() {
    let mut state = AppState::new_document(256, 256);

    // Paint a 10x10 red rectangle on a new layer.
    add_raster_layer(&mut state, "Paint");
    let paint_layer = state.doc().active.unwrap();
    let red = Rgba32F::new(1.0, 0.0, 0.0, 1.0);
    let mut points = Vec::new();
    for y in 50..60 {
        for x in 50..60 {
            points.push((IVec2::new(x, y), red));
        }
    }
    dispatch::dispatch(&mut state, Box::new(PaintCmd::new(paint_layer, points))).unwrap();

    // Rectangle-select the painted area.
    let mut tool = ogre_ui::tools::RectSelectTool::new();
    assert!(send(
        &mut tool,
        &state.tabs[state.active_tab].doc,
        IVec2::new(50, 50),
        Phase::Down
    )
    .is_none());
    assert!(send(
        &mut tool,
        &state.tabs[state.active_tab].doc,
        IVec2::new(60, 60),
        Phase::Drag
    )
    .is_none());
    let cmd = send(
        &mut tool,
        &state.tabs[state.active_tab].doc,
        IVec2::new(60, 60),
        Phase::Up,
    )
    .unwrap();
    dispatch::dispatch(&mut state, cmd).unwrap();

    // Cut the selection to a new layer.
    apply_context_action(&mut state, ContextAction::CutToNewLayer);
    let cut_layer = state.doc().active.unwrap();
    assert_ne!(cut_layer, paint_layer);

    // Move the new layer so undo has to restore position as well as content.
    dispatch::dispatch(
        &mut state,
        Box::new(ogre_core::MoveLayerByCmd::new(
            cut_layer,
            IVec2::new(20, 30),
        )),
    )
    .unwrap();

    // Snapshot the document after the full workflow.
    let snapshot = state.doc().clone();

    // Undo cut + selection + paint? Actually we want 3 undos to roll back move,
    // cut, and selection (in that order). Redo them and compare.
    dispatch::undo(&mut state);
    dispatch::undo(&mut state);
    dispatch::undo(&mut state);

    dispatch::redo(&mut state);
    dispatch::redo(&mut state);
    dispatch::redo(&mut state);

    assert_docs_equivalent(&state.tabs[state.active_tab].doc, &snapshot);
}

/// Compare documents ignoring the concrete [`LayerId`] versions that change
/// when a layer is removed and re-created during undo/redo.
fn assert_docs_equivalent(a: &ogre_core::Document, b: &ogre_core::Document) {
    assert_eq!(a.canvas, b.canvas);
    assert_eq!(a.selection, b.selection);
    assert_eq!(a.color_space, b.color_space);
    assert_eq!(a.order.len(), b.order.len());

    for (idx, (&aid, &bid)) in a.order.iter().zip(b.order.iter()).enumerate() {
        let al = a.layer(aid).unwrap();
        let bl = b.layer(bid).unwrap();
        assert_eq!(al.name, bl.name, "layer {} name differs", idx);
        assert_eq!(al.offset, bl.offset, "layer {} offset differs", idx);
        assert_eq!(al.opacity, bl.opacity, "layer {} opacity differs", idx);
        assert_eq!(al.blend, bl.blend, "layer {} blend differs", idx);
        assert_eq!(al.visible, bl.visible, "layer {} visible differs", idx);
        assert_eq!(al.locked, bl.locked, "layer {} locked differs", idx);
        assert_eq!(al.buffer(), bl.buffer(), "layer {} buffer differs", idx);
        assert_eq!(al.mask(), bl.mask(), "layer {} mask differs", idx);
    }

    let active_a = a
        .active
        .and_then(|id| a.order.iter().position(|&x| x == id));
    let active_b = b
        .active
        .and_then(|id| b.order.iter().position(|&x| x == id));
    assert_eq!(active_a, active_b, "active layer index differs");
}

fn send(
    tool: &mut ogre_ui::tools::RectSelectTool,
    doc: &ogre_core::Document,
    pos: IVec2,
    phase: Phase,
) -> Option<Box<dyn ogre_core::Command>> {
    tool.on_pointer(doc, PointerEvent::new(pos, phase, egui::Modifiers::NONE))
}
