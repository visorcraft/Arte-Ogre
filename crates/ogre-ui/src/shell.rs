// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Dock layout, menu bar, and panel routing for the Arte Ogre shell.
//!
//! Defines the three core panels (Canvas, Layers, Tools) arranged in a default
//! [`egui_dock::DockState`], and wires the menu bar and keyboard shortcuts
//! through the command dispatch path so every edit remains undoable.

use egui::WidgetText;
use egui_dock::widgets::tab_viewer::OnCloseResponse;
use egui_dock::{AllowedSplits, DockArea, DockState, NodeIndex, TabViewer};
use std::path::PathBuf;
use std::time::Duration;

use crate::dispatch;
use crate::io_worker::IoKind;
use crate::panels;
use crate::state::{
    AppState, AppView, ColorRangeDialog, DocumentTab, FileDialog, FilterDialog, FilterKind,
    SelectionDialog,
};
use crate::tools::ToolFamily;
use crate::OgreApp;
use ogre_core::{PaintMode, Rgba32F};

/// A dockable panel in the Arte Ogre editor shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Panel {
    /// An open document, keyed by its stable [`DocumentTab`] id. Every open
    /// document is a tab in the central canvas leaf; selecting one makes it the
    /// active document and shows the GPU canvas view.
    Document(u64),
    /// Layer stack, visibility, opacity, and blend controls.
    Layers,
}

/// Builds the default dock layout:
///
/// - The first document's canvas tab in the center.
/// - `Layers` panel on the right (25% of the window width).
///
/// The Tools palette is no longer a dock tab; it lives in a fixed left
/// `egui::Panel::left` rendered in `lib.rs` before the dock area.
pub fn default_dock_state(show_layers: bool, doc_id: u64) -> DockState<Panel> {
    let mut dock_state = DockState::new(vec![Panel::Document(doc_id)]);
    if show_layers {
        let surface = dock_state.main_surface_mut();
        // Layers occupies the right 25% of the window; Canvas keeps the rest.
        let [_canvas_node, _layers_node] =
            surface.split_right(NodeIndex::root(), 0.75, vec![Panel::Layers]);
    }
    dock_state
}

/// Ensure the Layers panel is present or absent according to `visible`.
///
/// When `visible` is true and no Layers tab exists, a new Layers tab is split
/// to the right of the main surface root. When false, every Layers tab is
/// removed from the dock.
pub(crate) fn ensure_layers_tab(state: &mut AppState, visible: bool) {
    let has_layers = state
        .dock
        .iter_all_tabs()
        .any(|(_, p)| matches!(p, Panel::Layers));
    if visible && !has_layers {
        let surface = state.dock.main_surface_mut();
        let _ = surface.split_right(NodeIndex::root(), 0.75, vec![Panel::Layers]);
    }
    if !visible && has_layers {
        let paths: Vec<egui_dock::TabPath> = state
            .dock
            .iter_all_tabs()
            .filter(|(_, p)| matches!(p, Panel::Layers))
            .map(|(path, _)| path)
            .collect();
        for path in paths {
            if path.surface == egui_dock::SurfaceIndex::main() {
                state
                    .dock
                    .main_surface_mut()
                    .remove_tab((path.node, path.tab));
            }
        }
    }
}

/// Reset the dock layout to the factory default for the current document.
///
/// Call this from View → Reset Layout when the user (or a bug) has left the
/// dock in a weird state. Layers visibility is preserved from preferences.
pub(crate) fn reset_layout(state: &mut AppState) {
    let doc_id = state.tabs[state.active_tab].id;
    state.dock = default_dock_state(state.preferences.layers_visible, doc_id);
}

/// Reconcile the dock's `Document` tabs with `state.tabs` before the dock is
/// shown: drop tabs for closed documents, add tabs for new ones, and mark the
/// dock's active document tab to match `state.active_tab`.
///
/// New tabs go to the first leaf (the canvas leaf — Layers, when shown, is split
/// to its right), so document tabs stay grouped together.
fn sync_document_tabs(state: &mut AppState) {
    let live: Vec<u64> = state.tabs.iter().map(|t| t.id).collect();

    // Remove tabs whose document was closed. `remove_tab` shifts indices, so
    // find-and-remove one at a time until none remain. The `let` ends the
    // immutable borrow from `iter_all_tabs` before the mutating `remove_tab`.
    loop {
        let stale = state.dock.iter_all_tabs().find_map(|(path, p)| match p {
            Panel::Document(id) if !live.contains(id) => Some(path),
            _ => None,
        });
        match stale {
            Some(path) => {
                state.dock.remove_tab(path);
            }
            None => break,
        }
    }

    // Add a tab for any document that doesn't have one yet.
    for &id in &live {
        let exists = state
            .dock
            .iter_all_tabs()
            .any(|(_, p)| matches!(p, Panel::Document(d) if *d == id));
        if !exists {
            state.dock.push_to_first_leaf(Panel::Document(id));
        }
    }

    // Point the dock at the active document (programmatic switches; user clicks
    // are synced back in `PanelViewer::ui`).
    let active_id = state.tabs[state.active_tab].id;
    let active_path = state
        .dock
        .iter_all_tabs()
        .find_map(|(path, p)| matches!(p, Panel::Document(d) if *d == active_id).then_some(path));
    if let Some(path) = active_path {
        let _ = state.dock.set_active_tab(path);
    }
}

fn tab_label(tab: &DocumentTab) -> String {
    tab.last_save_path
        .as_ref()
        .or(tab.import_path.as_ref())
        .and_then(|p| p.file_stem())
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "Untitled".to_string())
}

/// A user-facing shortcut action handled by the shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shortcut {
    /// Undo the most recent command.
    Undo,
    /// Redo the most recently undone command.
    Redo,
    /// Clear the current selection.
    Deselect,
    /// Select the entire canvas.
    SelectAll,
    /// Invert the current selection within the canvas.
    InvertSelection,
    /// Commit the active tool's pending interaction.
    CommitActiveTool,
    /// Cancel the active tool's in-progress interaction (e.g. via Escape).
    CancelActiveTool,
    /// Add a new empty raster layer above the active one and activate it.
    NewRasterLayer,
    /// Duplicate the active layer.
    DuplicateLayer,
    /// Delete the active layer.
    DeleteLayer,
    /// Rename the active layer (opens a dialog).
    RenameLayer,
    /// Zoom in one step (around the canvas centre).
    ZoomIn,
    /// Zoom out one step.
    ZoomOut,
    /// Reset zoom to 100 %.
    Zoom100,
    /// Open the New Document dialog.
    NewDocument,
    /// Open the Open Image dialog.
    OpenDocument,
    /// Save the current document (Save As if never saved).
    SaveDocument,
    /// Open the Save As dialog.
    SaveAsDocument,
    /// Activate the Free Transform tool.
    FreeTransform,
    /// Reset foreground/background colors to black/white.
    DefaultColors,
    /// Swap foreground and background colors.
    SwapColors,
    /// Fill the selection (or whole canvas) with the foreground color.
    FillForeground,
    /// Fill the selection (or whole canvas) with the background color.
    FillBackground,
    /// Close the current document tab.
    CloseDocument,
    /// Quit the application.
    Quit,
    /// Switch to the next document tab.
    NextTab,
    /// Switch to the previous document tab.
    PrevTab,
    /// Move the active layer one slot toward the viewer (Bring Forward).
    BringForward,
    /// Move the active layer one slot away from the viewer (Send Backward).
    SendBackward,
    /// Toggle between Editor View and Bird's Eye View.
    ToggleBirdsEye,
}

impl Shortcut {
    /// Whether this action must be suppressed while a text field has focus or a
    /// modal dialog is open, because its triggering key is also a text-edit key
    /// or a bare letter that would fire spuriously while typing.
    pub fn requires_no_text_focus(self) -> bool {
        matches!(
            self,
            Shortcut::DeleteLayer
                | Shortcut::RenameLayer
                | Shortcut::DefaultColors
                | Shortcut::SwapColors
                | Shortcut::FillForeground
                | Shortcut::FillBackground
        )
    }
}

/// Format a menu-item label with a right-aligned, dimmed keyboard-shortcut
/// hint. If no chord is bound to `action`, the label is returned unchanged.
fn menu_label(
    ui: &egui::Ui,
    label: &str,
    action: Option<Shortcut>,
    keymap: &Keymap,
) -> egui::WidgetText {
    match action.and_then(|a| keymap.chord_for(a)) {
        Some(chord) => {
            let hint = format!("{chord}");
            // Build a two-segment layout job: the label in the body style, then
            // the chord in a smaller, dimmed style to mimic native menu hints.
            let mut job = egui::text::LayoutJob::default();
            job.append(
                label,
                0.0,
                egui::TextFormat {
                    font_id: egui::TextStyle::Body.resolve(ui.style()),
                    color: ui.visuals().text_color(),
                    ..Default::default()
                },
            );
            // Right-align the shortcut by expanding with tab-like spacing.
            job.append(
                &format!("   {hint}"),
                0.0,
                egui::TextFormat {
                    font_id: egui::TextStyle::Small.resolve(ui.style()),
                    color: ui.visuals().weak_text_color(),
                    ..Default::default()
                },
            );
            egui::WidgetText::LayoutJob(job.into())
        }
        None => label.into(),
    }
}

use crate::keymap::Keymap;

/// Handle a shortcut action on `state`.
///
/// Returns `true` if the shortcut was consumed. Undo/redo and selection changes
/// are routed through the command dispatch path so they remain undoable and
/// mark the renderer dirty.
pub fn handle_shortcut(state: &mut AppState, shortcut: Shortcut) -> bool {
    match shortcut {
        Shortcut::Undo => dispatch::undo(state).is_some(),
        Shortcut::Redo => dispatch::redo(state).is_some(),
        Shortcut::Deselect => dispatch::dispatch(
            state,
            Box::new(ogre_core::SetSelectionCmd::new(ogre_core::Selection::none())),
        )
        .is_ok(),
        Shortcut::SelectAll => {
            let canvas = state.current_tab().doc.canvas;
            dispatch::dispatch(
                state,
                Box::new(ogre_core::SetSelectionCmd::new(
                    ogre_core::Selection::select_all(canvas),
                )),
            )
            .is_ok()
        }
        Shortcut::InvertSelection => {
            dispatch::dispatch(state, Box::new(ogre_core::InvertSelectionCmd::new())).is_ok()
        }
        Shortcut::CommitActiveTool => commit_active_tool(state),
        Shortcut::CancelActiveTool => {
            state.tool_manager.cancel_active();
            true
        }
        Shortcut::NewRasterLayer => {
            crate::panels::layers::add_raster_layer(state, "Layer");
            true
        }
        Shortcut::DuplicateLayer => {
            if let Some(id) = state.current_tab().doc.active {
                crate::panels::layers::duplicate_layer(state, id);
                true
            } else {
                false
            }
        }
        Shortcut::DeleteLayer => {
            if let Some(id) = state.current_tab().doc.active {
                crate::panels::layers::delete_layer(state, id);
                true
            } else {
                false
            }
        }
        Shortcut::RenameLayer => {
            // Open the rename dialog for the active layer. The panel's inline
            // rename field is always-present and not focusable on demand, so a
            // small modal is the correct UX.
            if let Some(id) = state.current_tab().doc.active {
                let name = state
                    .current_tab()
                    .doc
                    .layer(id)
                    .map(|l| l.name.clone())
                    .unwrap_or_default();
                state.rename_dialog = Some((id, name));
                true
            } else {
                false
            }
        }
        Shortcut::ZoomIn | Shortcut::ZoomOut | Shortcut::Zoom100 => {
            let factor = match shortcut {
                Shortcut::ZoomIn => 1.25,
                Shortcut::ZoomOut => 1.0 / 1.25,
                _ => 1.0,
            };
            let before = state.current_tab().viewport.zoom;
            let target = if matches!(shortcut, Shortcut::Zoom100) {
                1.0
            } else {
                crate::state::clamp_zoom(before * factor)
            };
            if (target - before).abs() > f32::EPSILON {
                state.current_tab_mut().viewport.zoom = target;
                state.dirty = true;
            }
            true
        }
        Shortcut::NewDocument => {
            if state.is_busy() {
                return false;
            }
            let (w, h) = state.preferences.default_canvas;
            state.file_dialog = Some(FileDialog::New {
                width: w.to_string(),
                height: h.to_string(),
            });
            true
        }
        Shortcut::OpenDocument => crate::open_flow::show_open_dialog(state),
        Shortcut::SaveDocument => {
            if state.is_busy() || state.welcome {
                return false;
            }
            if let Some(path) = state.current_tab().last_save_path.clone() {
                request_save(state, path);
            } else if let Some(path) = state.current_tab().import_path.clone() {
                request_export_path(state, path);
            } else {
                state.file_dialog = Some(FileDialog::SaveAs {
                    path: String::new(),
                });
            }
            true
        }
        Shortcut::SaveAsDocument => {
            if state.is_busy() || state.welcome {
                return false;
            }
            state.file_dialog = Some(FileDialog::SaveAs {
                path: String::new(),
            });
            true
        }
        Shortcut::FreeTransform => {
            if state.welcome {
                return false;
            }
            state.set_tool_and_persist(crate::tools::ToolKind::FreeTransform);
            true
        }
        Shortcut::DefaultColors => {
            state.foreground = ogre_core::Rgba32F::new(0.0, 0.0, 0.0, 1.0);
            state.background = ogre_core::Rgba32F::new(1.0, 1.0, 1.0, 1.0);
            true
        }
        Shortcut::SwapColors => {
            std::mem::swap(&mut state.foreground, &mut state.background);
            true
        }
        Shortcut::FillForeground => fill_with(state, state.foreground),
        Shortcut::FillBackground => fill_with(state, state.background),
        Shortcut::CloseDocument => {
            if state.welcome {
                return false;
            }
            state.close_tab(state.active_tab);
            true
        }
        Shortcut::Quit => {
            // The actual ViewportCommand::Close is sent by the dispatch loop in
            // OgreApp::ui (which has the egui::Context); the close-request
            // interceptor there checks for unsaved changes and shows the
            // quit-confirm modal if needed.
            true
        }
        Shortcut::NextTab => {
            if state.tabs.len() < 2 {
                return false;
            }
            let n = state.tabs.len();
            state.active_tab = (state.active_tab + 1) % n;
            state.dirty = true;
            true
        }
        Shortcut::PrevTab => {
            if state.tabs.len() < 2 {
                return false;
            };
            let n = state.tabs.len();
            state.active_tab = (state.active_tab + n - 1) % n;
            state.dirty = true;
            true
        }
        Shortcut::BringForward => reorder_active_layer_by(state, 1),
        Shortcut::SendBackward => reorder_active_layer_by(state, -1),
        Shortcut::ToggleBirdsEye => {
            if state.welcome {
                return false;
            }
            state.view = match state.view {
                AppView::BirdsEye => AppView::Editor,
                AppView::Editor => AppView::BirdsEye,
                // Don't pull Settings/Licenses/Credits/Plugins into Bird's Eye.
                _ => return false,
            };
            true
        }
    }
}

fn reorder_active_layer_by(state: &mut AppState, delta: isize) -> bool {
    if state.welcome || state.is_busy() {
        return false;
    }
    let Some(id) = state.current_tab().doc.active else {
        return false;
    };
    let Some((_, current)) = state.current_tab().doc.sibling_index(id) else {
        return false;
    };
    let target = if delta.is_positive() {
        current.saturating_add(delta as usize)
    } else {
        current.saturating_sub(delta.unsigned_abs())
    };
    if target == current {
        return false;
    }
    dispatch::dispatch_or_report(state, Box::new(ogre_core::ReorderCmd::new(id, target)));
    true
}

/// Fill the active selection (or whole canvas) with `color`, if a raster layer
/// is active and the app is not busy. Shared by the Fill Foreground/Background
/// shortcuts.
fn fill_with(state: &mut AppState, color: ogre_core::Rgba32F) -> bool {
    if active_raster_layer_id(state).is_none() || state.is_busy() {
        return false;
    }
    let sel = state.current_tab().doc.selection.clone();
    dispatch::dispatch_or_report(
        state,
        Box::new(ogre_core::FillSelectionCmd::new(color, 1.0, sel)),
    );
    true
}

/// Commit the active tool's pending interaction.
///
/// Returns `true` if a command was produced and dispatched. This is used for
/// tools such as Free Transform that commit on Enter rather than on pointer up.
pub fn commit_active_tool(state: &mut AppState) -> bool {
    let cmd = state
        .tool_manager
        .commit_active(&state.tabs[state.active_tab].doc);
    if let Some(cmd) = cmd {
        dispatch::dispatch(state, cmd).is_ok()
    } else {
        false
    }
}

/// Activate a tool family via its bare-key shortcut (§2.1).
///
/// If the active tool is already in `family`, cycle to the next sibling
/// (forward on the plain key, backward on `Shift+key`). Otherwise activate the
/// last-used sibling for that family, or the family primary if none. The
/// activated sibling is recorded in `tool_family_last` so the next press from
/// off-family restores it.
pub fn activate_tool_family(state: &mut AppState, family: ToolFamily, forward: bool) {
    use crate::tools::ToolKind;
    let active = state.tool_manager.active();
    let siblings = family.siblings();
    let target: ToolKind = if active.family() == family {
        let idx = siblings.iter().position(|&k| k == active).unwrap_or(0);
        let n = siblings.len();
        let next = if forward {
            (idx + 1) % n
        } else {
            (idx + n - 1) % n
        };
        siblings[next]
    } else {
        // From off-family: plain activates the last-used sibling (or primary).
        // Shift jumps straight to the last sibling, so e.g. `Shift+W` reaches
        // Quick Selection directly (per spec §3.4.1) rather than the primary.
        if !forward && siblings.len() > 1 {
            *siblings.last().unwrap()
        } else {
            *state
                .tool_family_last
                .get(&family)
                .unwrap_or(&family.primary())
        }
    };
    state.set_tool_and_persist(target);
    // If the new tool is vector-capable and a compatible vector layer is
    // active, load it for re-edit (spec §4).
    let doc = state.doc().clone();
    state.tool_manager.load_active_vector_layer(&doc);
}

/// Scale the menu bar so it is ~50% taller with ~50% more space between menus.
///
/// `interact_size.y` drives the bar's minimum height (egui's `MenuBar` sizes the
/// bar from it, not from `button_padding`, which `menu_style` overrides), and
/// `item_spacing.x` is the horizontal gap between the menu buttons.
fn apply_menu_bar_spacing(spacing: &mut egui::style::Spacing) {
    spacing.interact_size.y *= 1.5;
    spacing.item_spacing.x *= 1.5;
}

/// Decide which top-level menu should be open during hover-switching.
///
/// `clicked[i]`/`hovered[i]`/`was_open[i]` describe the i-th top-level menu
/// button. Returns the index of the menu that should be shown this frame, or
/// `None` if every menu should be closed.
fn pick_active_menu(clicked: &[bool], hovered: &[bool], was_open: &[bool]) -> Option<usize> {
    debug_assert_eq!(clicked.len(), hovered.len());
    debug_assert_eq!(hovered.len(), was_open.len());

    let bar_active = was_open.iter().any(|&o| o);

    if let Some(i) = clicked.iter().position(|&c| c) {
        // Clicking an open menu closes it; clicking a closed menu opens it.
        return if was_open[i] { None } else { Some(i) };
    }

    if bar_active {
        if let Some(i) = hovered.iter().position(|&h| h) {
            return Some(i);
        }
    }

    was_open.iter().position(|&o| o)
}

/// Apply standard dropdown style: minimum width + roomier item padding.
fn menu_dropdown_setup(ui: &mut egui::Ui) {
    ui.set_min_width(200.0);
    ui.spacing_mut().button_padding = egui::vec2(8.0, 4.0);
}

/// Renders the top application menu bar and wires menu items to command dispatch.
pub fn menu_bar(ui: &mut egui::Ui, state: &mut AppState, _frame: &mut eframe::Frame) {
    apply_menu_bar_spacing(ui.spacing_mut());
    let has_canvas = !state.welcome;
    egui::MenuBar::new().ui(ui, |ui| {
        ui.spacing_mut().button_padding = egui::vec2(8.0, 4.0);

        let file_menu = move |ui: &mut egui::Ui, state: &mut AppState| {
            menu_dropdown_setup(ui);
            let busy = state.is_busy();
            if ui
                .add_enabled(
                    !busy,
                    egui::Button::new(menu_label(
                        ui,
                        "New",
                        Some(Shortcut::NewDocument),
                        &state.keymap,
                    )),
                )
                .clicked()
            {
                let (w, h) = state.preferences.default_canvas;
                state.file_dialog = Some(FileDialog::New {
                    width: w.to_string(),
                    height: h.to_string(),
                });
                ui.close_kind(egui::UiKind::Menu);
            }
            if ui
                .add_enabled(
                    !busy,
                    egui::Button::new(menu_label(
                        ui,
                        "Open",
                        Some(Shortcut::OpenDocument),
                        &state.keymap,
                    )),
                )
                .clicked()
            {
                crate::open_flow::show_open_dialog(state);
                ui.close_kind(egui::UiKind::Menu);
            }
            // Save/Export only apply to an open document.
            if has_canvas {
                if ui
                    .add_enabled(
                        !busy,
                        egui::Button::new(menu_label(
                            ui,
                            "Save",
                            Some(Shortcut::SaveDocument),
                            &state.keymap,
                        )),
                    )
                    .clicked()
                {
                    if let Some(path) = state.current_tab().last_save_path.clone() {
                        request_save(state, path);
                    } else if let Some(path) = state.current_tab().import_path.clone() {
                        request_export_path(state, path);
                    } else {
                        state.file_dialog = Some(FileDialog::SaveAs {
                            path: String::new(),
                        });
                    }
                    ui.close_kind(egui::UiKind::Menu);
                }
                if ui
                    .add_enabled(
                        !busy,
                        egui::Button::new(menu_label(
                            ui,
                            "Save As",
                            Some(Shortcut::SaveAsDocument),
                            &state.keymap,
                        )),
                    )
                    .clicked()
                {
                    state.file_dialog = Some(FileDialog::SaveAs {
                        path: String::new(),
                    });
                    ui.close_kind(egui::UiKind::Menu);
                }
                if ui.add_enabled(!busy, egui::Button::new("Export")).clicked() {
                    state.file_dialog = Some(FileDialog::Export {
                        path: String::new(),
                        format: "PNG".to_string(),
                        quality: "90".to_string(),
                        bit_depth: "8".to_string(),
                    });
                    ui.close_kind(egui::UiKind::Menu);
                }
                if ui
                    .add_enabled(!busy, egui::Button::new("Export Slices…"))
                    .clicked()
                {
                    state.file_dialog = Some(FileDialog::ExportSlices {
                        path: String::new(),
                        format: "PNG".to_string(),
                        quality: "90".to_string(),
                        bit_depth: "8".to_string(),
                    });
                    ui.close_kind(egui::UiKind::Menu);
                }
            }
            if has_canvas
                && ui
                    .add_enabled(
                        !busy,
                        egui::Button::new(menu_label(
                            ui,
                            "Close",
                            Some(Shortcut::CloseDocument),
                            &state.keymap,
                        )),
                    )
                    .clicked()
            {
                state.close_tab(state.active_tab);
                ui.close_kind(egui::UiKind::Menu);
            }
            if has_canvas
                && ui
                    .add_enabled(!busy, egui::Button::new("Duplicate"))
                    .clicked()
            {
                state.duplicate_current_tab();
                ui.close_kind(egui::UiKind::Menu);
            }
            ui.separator();
            if ui
                .add(egui::Button::new(menu_label(
                    ui,
                    "Quit",
                    Some(Shortcut::Quit),
                    &state.keymap,
                )))
                .clicked()
            {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                ui.close_kind(egui::UiKind::Menu);
            }
        };

        // These menus only make sense with a document open, so hide them on the
        // welcome screen rather than showing them disabled.
        let view_menu = move |ui: &mut egui::Ui, state: &mut AppState| {
            menu_dropdown_setup(ui);
            if ui
                .selectable_label(state.view == AppView::Editor, "Editor View")
                .clicked()
            {
                state.view = AppView::Editor;
                ui.close_kind(egui::UiKind::Menu);
            }
            if ui
                .selectable_label(state.view == AppView::BirdsEye, "Bird's Eye View")
                .clicked()
            {
                state.view = AppView::BirdsEye;
                ui.close_kind(egui::UiKind::Menu);
            }
            // Layout/grid controls only apply to the Editor canvas; hide them
            // while Bird's Eye View is active.
            if state.view == AppView::Editor {
                ui.separator();
                let mut visible = state.preferences.layers_visible;
                if ui.checkbox(&mut visible, "Layers").changed() {
                    state.preferences.layers_visible = visible;
                    ensure_layers_tab(state, visible);
                    if let Some(p) = crate::prefs::Preferences::config_path() {
                        let _ = state.preferences.save(&p);
                    }
                }
                ui.separator();
                if ui.button("Reset Layout").clicked() {
                    reset_layout(state);
                    ui.close_kind(egui::UiKind::Menu);
                }
                ui.checkbox(&mut state.show_grid, "Show Grid");
                ui.checkbox(&mut state.snap_to_grid, "Snap to Grid");
            }
        };
        let edit_menu = move |ui: &mut egui::Ui, state: &mut AppState| {
            menu_dropdown_setup(ui);
            let can_undo = state.current_tab().history.undo_len() > 0;
            let can_redo = state.current_tab().history.redo_len() > 0;

            if ui
                .add_enabled(
                    can_undo,
                    egui::Button::new(menu_label(ui, "Undo", Some(Shortcut::Undo), &state.keymap)),
                )
                .clicked()
            {
                handle_shortcut(state, Shortcut::Undo);
                ui.close_kind(egui::UiKind::Menu);
            }
            if ui
                .add_enabled(
                    can_redo,
                    egui::Button::new(menu_label(ui, "Redo", Some(Shortcut::Redo), &state.keymap)),
                )
                .clicked()
            {
                handle_shortcut(state, Shortcut::Redo);
                ui.close_kind(egui::UiKind::Menu);
            }
            ui.separator();
            let has_selection = !state.current_tab().doc.selection.is_empty();
            if ui
                .add_enabled(has_selection, egui::Button::new("Copy   Ctrl+C"))
                .clicked()
            {
                crate::paste::copy_selection_to_clipboard(state);
                ui.close_kind(egui::UiKind::Menu);
            }
            if ui
                .add_enabled(has_selection, egui::Button::new("Cut   Ctrl+X"))
                .clicked()
            {
                crate::paste::cut_selection_to_clipboard(state);
                ui.close_kind(egui::UiKind::Menu);
            }
            if ui.button("Paste   Ctrl+V").clicked() {
                crate::paste::try_paste(state);
                ui.close_kind(egui::UiKind::Menu);
            }
            ui.separator();
            // Fill the active selection (or the whole canvas) with the
            // foreground / background color. Dispatches an undoable
            // FillSelectionCmd against a snapshot of the live selection.
            let has_raster = active_raster_layer_id(state).is_some();
            let can_fill = has_raster && !state.is_busy();
            let raster_reason = if state.is_busy() {
                "Wait for the current operation to finish"
            } else {
                "Select a raster layer first"
            };
            if ui
                .add_enabled(
                    can_fill,
                    egui::Button::new(menu_label(
                        ui,
                        "Fill with Foreground",
                        Some(Shortcut::FillForeground),
                        &state.keymap,
                    )),
                )
                .on_hover_text("Fill the selection with the foreground color")
                .on_disabled_hover_text(raster_reason)
                .clicked()
            {
                let sel = state.current_tab().doc.selection.clone();
                let color = state.foreground;
                dispatch::dispatch_or_report(
                    state,
                    Box::new(ogre_core::FillSelectionCmd::new(color, 1.0, sel)),
                );
                ui.close_kind(egui::UiKind::Menu);
            }
            if ui
                .add_enabled(
                    can_fill,
                    egui::Button::new(menu_label(
                        ui,
                        "Fill with Background",
                        Some(Shortcut::FillBackground),
                        &state.keymap,
                    )),
                )
                .on_hover_text("Fill the selection with the background color")
                .on_disabled_hover_text(raster_reason)
                .clicked()
            {
                let sel = state.current_tab().doc.selection.clone();
                let color = state.background;
                dispatch::dispatch_or_report(
                    state,
                    Box::new(ogre_core::FillSelectionCmd::new(color, 1.0, sel)),
                );
                ui.close_kind(egui::UiKind::Menu);
            }
            // Stroke the selection outline (v1: Rect selections only) with
            // the foreground colour at a user-chosen width.
            if ui
                .add_enabled(
                    can_fill && has_selection,
                    egui::Button::new("Stroke Selection…"),
                )
                .on_hover_text("Outline the selection with the foreground colour")
                .clicked()
            {
                state.selection_dialog = Some(SelectionDialog::Stroke {
                    width: "2".to_string(),
                });
                ui.close_kind(egui::UiKind::Menu);
            }
        };
        let select_menu = move |ui: &mut egui::Ui, state: &mut AppState| {
            menu_dropdown_setup(ui);
            if ui
                .add(egui::Button::new(menu_label(
                    ui,
                    "All",
                    Some(Shortcut::SelectAll),
                    &state.keymap,
                )))
                .clicked()
            {
                handle_shortcut(state, Shortcut::SelectAll);
                ui.close_kind(egui::UiKind::Menu);
            }
            if ui
                .add(egui::Button::new(menu_label(
                    ui,
                    "None",
                    Some(Shortcut::Deselect),
                    &state.keymap,
                )))
                .clicked()
            {
                handle_shortcut(state, Shortcut::Deselect);
                ui.close_kind(egui::UiKind::Menu);
            }
            ui.separator();
            if ui
                .add(egui::Button::new(menu_label(
                    ui,
                    "Invert",
                    Some(Shortcut::InvertSelection),
                    &state.keymap,
                )))
                .clicked()
            {
                handle_shortcut(state, Shortcut::InvertSelection);
                ui.close_kind(egui::UiKind::Menu);
            }
            ui.separator();
            if ui.button("Color Range…").clicked() {
                open_color_range_dialog(state);
                ui.close_kind(egui::UiKind::Menu);
            }
            ui.separator();
            if ui.button("Feather").clicked() {
                state.selection_dialog = Some(SelectionDialog::Feather {
                    radius: "2".to_string(),
                });
                ui.close_kind(egui::UiKind::Menu);
            }
            if ui.button("Grow").clicked() {
                state.selection_dialog = Some(SelectionDialog::Grow {
                    amount: "1".to_string(),
                });
                ui.close_kind(egui::UiKind::Menu);
            }
            if ui.button("Shrink").clicked() {
                state.selection_dialog = Some(SelectionDialog::Shrink {
                    amount: "1".to_string(),
                });
                ui.close_kind(egui::UiKind::Menu);
            }
        };
        let image_menu = move |ui: &mut egui::Ui, state: &mut AppState| {
            let has_raster = active_raster_layer_id(state).is_some();
            menu_dropdown_setup(ui);
            // Destructive tonal/colour adjustments (the non-destructive
            // versions live under Layer → New Adjustment Layer).
            ui.menu_button("Adjustments", |ui| {
                menu_dropdown_setup(ui);
                tonal_menu_items(ui, state, has_raster, /* adjustment */ false);
            });
            ui.separator();
            if ui.button("Canvas Size").clicked() {
                open_canvas_resize_dialog(state);
                ui.close_kind(egui::UiKind::Menu);
            }
            // Crop to the active selection's bounding box (intersected with
            // the canvas). Disabled when there is no selection or the
            // intersection is empty/outside the canvas.
            let crop_target = crop_to_selection_target(&state.current_tab().doc);
            if ui
                .add_enabled(
                    crop_target.is_some() && !state.is_busy(),
                    egui::Button::new("Crop to Selection"),
                )
                .on_hover_text("Crop the canvas to the selection's bounding box")
                .on_disabled_hover_text("Make a selection first")
                .clicked()
            {
                if let Some(rect) = crop_target {
                    dispatch::dispatch_or_report(state, Box::new(ogre_core::CropCmd::new(rect)));
                }
                ui.close_kind(egui::UiKind::Menu);
            }
            // Trim transparent borders by cropping to the bounding box of
            // all non-transparent pixels in the visible composite.
            let can_trim = !state.is_busy();
            if ui
                .add_enabled(can_trim, egui::Button::new("Trim"))
                .on_hover_text("Crop away transparent borders of the visible composite")
                .clicked()
            {
                if let Some(rect) = trim_target(&state.current_tab().doc) {
                    dispatch::dispatch_or_report(state, Box::new(ogre_core::CropCmd::new(rect)));
                }
                ui.close_kind(egui::UiKind::Menu);
            }
            ui.separator();
            let can_remove_bg = has_raster && !state.is_busy() && state.remove_bg_dialog.is_none();
            if ui
                .add_enabled(can_remove_bg, egui::Button::new("Remove Background…"))
                .on_hover_text(
                    "Turn a solid or checkerboard \"fake transparency\" matte into true \
                         transparency on the active layer",
                )
                .clicked()
            {
                state.remove_bg_dialog = Some(ogre_core::MatteOptions::default());
                ui.close_kind(egui::UiKind::Menu);
            }
        };
        // Non-destructive adjustment layers live under Layer → New
        // Adjustment Layer (Photoshop convention); the destructive variants
        // stay under Filters.
        let layer_menu = move |ui: &mut egui::Ui, state: &mut AppState| {
            let has_raster = active_raster_layer_id(state).is_some();
            menu_dropdown_setup(ui);
            let can_add = !state.is_busy();
            if ui
                .add_enabled(
                    can_add,
                    egui::Button::new(menu_label(
                        ui,
                        "New Raster Layer",
                        Some(Shortcut::NewRasterLayer),
                        &state.keymap,
                    )),
                )
                .on_hover_text("Add an empty raster layer")
                .clicked()
            {
                handle_shortcut(state, Shortcut::NewRasterLayer);
                ui.close_kind(egui::UiKind::Menu);
            }
            let active_id = state.current_tab().doc.active;
            if ui
                .add_enabled(
                    can_add && active_id.is_some(),
                    egui::Button::new(menu_label(
                        ui,
                        "Duplicate Layer",
                        Some(Shortcut::DuplicateLayer),
                        &state.keymap,
                    )),
                )
                .on_hover_text("Duplicate the active layer")
                .clicked()
            {
                handle_shortcut(state, Shortcut::DuplicateLayer);
                ui.close_kind(egui::UiKind::Menu);
            }
            if ui
                .add_enabled(
                    can_add && active_id.is_some(),
                    egui::Button::new(menu_label(
                        ui,
                        "Delete Layer",
                        Some(Shortcut::DeleteLayer),
                        &state.keymap,
                    )),
                )
                .on_hover_text("Delete the active layer")
                .clicked()
            {
                handle_shortcut(state, Shortcut::DeleteLayer);
                ui.close_kind(egui::UiKind::Menu);
            }
            if ui
                .add_enabled(
                    can_add && active_id.is_some(),
                    egui::Button::new(menu_label(
                        ui,
                        "Rename Layer…",
                        Some(Shortcut::RenameLayer),
                        &state.keymap,
                    )),
                )
                .on_hover_text("Rename the active layer")
                .clicked()
            {
                handle_shortcut(state, Shortcut::RenameLayer);
                ui.close_kind(egui::UiKind::Menu);
            }
            if ui
                .add_enabled(
                    has_raster && !state.is_busy(),
                    egui::Button::new("Merge Down"),
                )
                .on_hover_text("Merge the active layer into the one below")
                .on_disabled_hover_text(if has_raster {
                    "No raster layer below to merge into"
                } else {
                    "Select a raster layer first"
                })
                .clicked()
            {
                if let Some(id) = active_raster_layer_id(state) {
                    let _ = dispatch::dispatch(state, Box::new(ogre_core::MergeDownCmd::new(id)));
                }
                ui.close_kind(egui::UiKind::Menu);
            }
            ui.menu_button("Arrange", |ui| {
                menu_dropdown_setup(ui);
                let can_arrange = active_id.is_some() && !state.is_busy();
                if ui
                    .add_enabled(
                        can_arrange,
                        egui::Button::new(menu_label(
                            ui,
                            "Bring Forward",
                            Some(Shortcut::BringForward),
                            &state.keymap,
                        )),
                    )
                    .on_hover_text("Move the active layer one step toward the top")
                    .clicked()
                {
                    handle_shortcut(state, Shortcut::BringForward);
                    ui.close_kind(egui::UiKind::Menu);
                }
                if ui
                    .add_enabled(
                        can_arrange,
                        egui::Button::new(menu_label(
                            ui,
                            "Send Backward",
                            Some(Shortcut::SendBackward),
                            &state.keymap,
                        )),
                    )
                    .on_hover_text("Move the active layer one step toward the bottom")
                    .clicked()
                {
                    handle_shortcut(state, Shortcut::SendBackward);
                    ui.close_kind(egui::UiKind::Menu);
                }
            });
            ui.separator();
            let can_flip = has_raster && !state.is_busy();
            // Lock the active layer — the toggle is available for any layer
            // type, not just raster.
            if let Some(id) = active_id {
                let locked = state
                    .current_tab()
                    .doc
                    .layer(id)
                    .map(|l| l.locked)
                    .unwrap_or(false);
                if ui
                    .add_enabled(
                        !state.is_busy(),
                        egui::Button::new(if locked { "Unlock Layer" } else { "Lock Layer" }),
                    )
                    .on_hover_text(if locked {
                        "Allow edits to the active layer"
                    } else {
                        "Prevent edits to the active layer"
                    })
                    .clicked()
                {
                    dispatch::dispatch_or_report(
                        state,
                        Box::new(ogre_core::SetLayerLockedCmd::new(id, !locked)),
                    );
                    ui.close_kind(egui::UiKind::Menu);
                }
            }
            if ui
                .add_enabled(can_flip, egui::Button::new("Flip Horizontal"))
                .on_hover_text("Mirror the active layer left↔right")
                .on_disabled_hover_text(if has_raster {
                    "Wait for the current operation to finish"
                } else {
                    "Select a raster layer first"
                })
                .clicked()
            {
                let id = active_raster_layer_id(state).unwrap();
                dispatch::dispatch_or_report(
                    state,
                    Box::new(ogre_core::FlipLayerCmd::new(
                        id,
                        ogre_core::FlipAxis::Horizontal,
                    )),
                );
                ui.close_kind(egui::UiKind::Menu);
            }
            if ui
                .add_enabled(can_flip, egui::Button::new("Flip Vertical"))
                .on_hover_text("Mirror the active layer top↔bottom")
                .clicked()
            {
                let id = active_raster_layer_id(state).unwrap();
                dispatch::dispatch_or_report(
                    state,
                    Box::new(ogre_core::FlipLayerCmd::new(
                        id,
                        ogre_core::FlipAxis::Vertical,
                    )),
                );
                ui.close_kind(egui::UiKind::Menu);
            }
            ui.separator();
            ui.menu_button("New Adjustment Layer", |ui| {
                menu_dropdown_setup(ui);
                tonal_menu_items(ui, state, has_raster, /* adjustment */ true);
            });
            ui.menu_button("Layer Mask", |ui| {
                menu_dropdown_setup(ui);
                layer_mask_menu_items(ui, state);
            });
        };
        let filters_menu = move |ui: &mut egui::Ui, state: &mut AppState| {
            let has_raster = active_raster_layer_id(state).is_some();
            menu_dropdown_setup(ui);
            spatial_filter_menu_items(ui, state, has_raster);
        };

        let plugins_menu = move |ui: &mut egui::Ui, state: &mut AppState| {
            menu_dropdown_setup(ui);
            if ui.button("Plugin Manager").clicked() {
                state.view = AppView::Plugins;
                ui.close_kind(egui::UiKind::Menu);
            }
        };
        let help_menu = move |ui: &mut egui::Ui, state: &mut AppState| {
            menu_dropdown_setup(ui);
            if ui.button("Command palette").clicked() {
                state.command_palette.open = true;
                state.command_palette.just_opened = true;
                ui.close_kind(egui::UiKind::Menu);
            }
            if ui.button("Settings").clicked() {
                state.view = AppView::Settings;
                ui.close_kind(egui::UiKind::Menu);
            }
            if ui.button("Licenses").clicked() {
                state.view = AppView::Licenses;
                ui.close_kind(egui::UiKind::Menu);
            }
            if ui.button("Credits").clicked() {
                state.view = AppView::Credits;
                ui.close_kind(egui::UiKind::Menu);
            }
            if ui.button("About").clicked() {
                state.show_about = true;
                ui.close_kind(egui::UiKind::Menu);
            }
            if ui.button("Check for Updates").clicked() {
                crate::help::start_update_check(state, ui.ctx());
                ui.close_kind(egui::UiKind::Menu);
            }
        };

        let ctx = ui.ctx().clone();
        let mut labels: Vec<&'static str> = Vec::new();
        let mut responses: Vec<egui::Response> = Vec::new();
        let mut popup_ids: Vec<egui::Id> = Vec::new();
        let mut clicked: Vec<bool> = Vec::new();
        let mut hovered: Vec<bool> = Vec::new();
        let mut was_open: Vec<bool> = Vec::new();
        let mut add_menu = |label: &'static str, response: egui::Response| {
            let popup_id = egui::Popup::default_response_id(&response);
            labels.push(label);
            clicked.push(response.clicked());
            hovered.push(response.hovered());
            was_open.push(egui::Popup::is_id_open(&ctx, popup_id));
            responses.push(response);
            popup_ids.push(popup_id);
        };

        add_menu("File", ui.add(egui::Button::new("File")));
        if has_canvas {
            add_menu("View", ui.add(egui::Button::new("View")));
            add_menu("Edit", ui.add(egui::Button::new("Edit")));
            add_menu("Select", ui.add(egui::Button::new("Select")));
            add_menu("Image", ui.add(egui::Button::new("Image")));
            add_menu("Layer", ui.add(egui::Button::new("Layer")));
            add_menu("Filters", ui.add(egui::Button::new("Filters")));
        }
        add_menu("Plugins", ui.add(egui::Button::new("Plugins")));
        add_menu("Help", ui.add(egui::Button::new("Help")));

        let active_idx = pick_active_menu(&clicked, &hovered, &was_open);

        if let Some(i) = active_idx {
            match labels[i] {
                "File" => {
                    egui::Popup::menu(&responses[i])
                        .open_memory(Some(egui::containers::SetOpenCommand::Bool(true)))
                        .show(|ui| {
                            file_menu(ui, state);
                        });
                }
                "View" => {
                    egui::Popup::menu(&responses[i])
                        .open_memory(Some(egui::containers::SetOpenCommand::Bool(true)))
                        .show(|ui| {
                            view_menu(ui, state);
                        });
                }
                "Edit" => {
                    egui::Popup::menu(&responses[i])
                        .open_memory(Some(egui::containers::SetOpenCommand::Bool(true)))
                        .show(|ui| {
                            edit_menu(ui, state);
                        });
                }
                "Select" => {
                    egui::Popup::menu(&responses[i])
                        .open_memory(Some(egui::containers::SetOpenCommand::Bool(true)))
                        .show(|ui| {
                            select_menu(ui, state);
                        });
                }
                "Image" => {
                    egui::Popup::menu(&responses[i])
                        .open_memory(Some(egui::containers::SetOpenCommand::Bool(true)))
                        .show(|ui| {
                            image_menu(ui, state);
                        });
                }
                "Layer" => {
                    egui::Popup::menu(&responses[i])
                        .open_memory(Some(egui::containers::SetOpenCommand::Bool(true)))
                        .show(|ui| {
                            layer_menu(ui, state);
                        });
                }
                "Filters" => {
                    egui::Popup::menu(&responses[i])
                        .open_memory(Some(egui::containers::SetOpenCommand::Bool(true)))
                        .show(|ui| {
                            filters_menu(ui, state);
                        });
                }
                "Plugins" => {
                    egui::Popup::menu(&responses[i])
                        .open_memory(Some(egui::containers::SetOpenCommand::Bool(true)))
                        .show(|ui| {
                            plugins_menu(ui, state);
                        });
                }
                "Help" => {
                    egui::Popup::menu(&responses[i])
                        .open_memory(Some(egui::containers::SetOpenCommand::Bool(true)))
                        .show(|ui| {
                            help_menu(ui, state);
                        });
                }
                _ => {}
            }
        }

        for (j, popup_id) in popup_ids.iter().enumerate() {
            if Some(j) != active_idx {
                egui::Popup::close_id(&ctx, *popup_id);
            }
        }
    });

    render_selection_dialogs(ui, state);
    render_canvas_size_dialog(ui, state);
    render_rename_dialog(ui, state);
    render_file_dialogs(ui, state);
    render_close_prompt(ui, state);
}

fn render_file_dialogs(ui: &mut egui::Ui, state: &mut AppState) {
    let Some(mut dialog) = state.file_dialog.take() else {
        return;
    };

    let title = match &dialog {
        FileDialog::New { .. } => "New Document",
        FileDialog::Open { .. } => "Open Image",
        FileDialog::SaveAs { .. } => "Save As",
        FileDialog::Export { .. } => "Export",
        FileDialog::ExportSlices { .. } => "Export Slices",
    };

    let mut close = false;
    let palette = crate::theme::resolve(state.preferences.theme, ui.ctx());

    egui::Window::new(title)
        .collapsible(false)
        .resizable(false)
        .title_bar(false)
        .auto_sized()
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ui.ctx(), |ui| {
            // The Open dialog spans 85% of the window; other file dialogs use a
            // compact fixed width so the modal is no taller/wider than its
            // content. Set the width before the heading so the whole modal
            // (heading included) takes the chosen width.
            match dialog {
                FileDialog::Open { .. } => {
                    ui.set_width(ui.ctx().content_rect().width() * 0.85);
                }
                FileDialog::SaveAs { .. }
                | FileDialog::Export { .. }
                | FileDialog::ExportSlices { .. } => {
                    ui.set_width(FILE_DIALOG_MODAL_WIDTH);
                }
                FileDialog::New { .. } => {
                    ui.set_width(260.0);
                }
            }
            modal_heading(ui, title);
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                close = true;
            }
            match &mut dialog {
                FileDialog::New { width, height } => {
                    ui.horizontal(|ui| {
                        ui.label("Width:");
                        ui.text_edit_singleline(width);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Height:");
                        ui.text_edit_singleline(height);
                    });
                }
                FileDialog::Open { path } => {
                    // "Image path:" on its own line, then a full-width field
                    // with a height-matched Browse button beside it.
                    ui.label("Image path:");
                    ui.add_space(crate::theme::SPACE_XS);
                    ui.horizontal(|ui| {
                        let row_h = ui.spacing().interact_size.y.max(30.0);
                        let browse_w = 92.0;
                        let input_w = (ui.available_width()
                            - browse_w
                            - ui.spacing().item_spacing.x)
                            .max(80.0);
                        ui.add_sized(
                            [input_w, row_h],
                            egui::TextEdit::singleline(path)
                                .hint_text("/path/to/image.png")
                                .vertical_align(egui::Align::Center),
                        );
                        if ui
                            .add_sized([browse_w, row_h], egui::Button::new("Browse…"))
                            .clicked()
                        {
                            if let Some(p) = crate::open_flow::pick_image_path() {
                                // On Bird's Eye View, Browse opens the picked
                                // file immediately (the modal self-closes when
                                // the load completes). In Editor View it just
                                // fills the field so the user can review before
                                // clicking Open.
                                if state.view == AppView::BirdsEye {
                                    crate::open_flow::open_image(state, &p);
                                } else {
                                    *path = p.to_string_lossy().to_string();
                                }
                            }
                        }
                    });

                    let recents = state.preferences.recent_files.clone();
                    if !recents.is_empty() {
                        ui.add_space(crate::theme::SPACE_M);
                        ui.label(
                            egui::RichText::new("RECENTLY OPENED")
                                .size(crate::theme::TEXT_CAPTION)
                                .color(ui.visuals().weak_text_color()),
                        );
                        ui.add_space(crate::theme::SPACE_XS);
                        // Bounded, scrollable list: it never grows past ~half the
                        // window, so the action row below always stays visible.
                        let max_h = (ui.ctx().content_rect().height() * 0.5).max(120.0);
                        egui::Frame::new()
                            .stroke(egui::Stroke::new(1.0, palette.separator))
                            .corner_radius(crate::theme::RADIUS_INPUT)
                            .show(ui, |ui| {
                                egui::ScrollArea::vertical()
                                    .max_height(max_h)
                                    .auto_shrink([false, true])
                                    .show(ui, |ui| {
                                        let full_w = ui.available_width();
                                        for (i, recent) in recents.iter().enumerate() {
                                            if i > 0 {
                                                ui.separator();
                                            }
                                            let file_name = std::path::Path::new(recent)
                                                .file_name()
                                                .map(|s| s.to_string_lossy().into_owned())
                                                .unwrap_or_else(|| recent.clone());
                                            let resp = ui
                                                .scope_builder(
                                                    egui::UiBuilder::new()
                                                        .sense(egui::Sense::click()),
                                                    |ui| {
                                                        ui.set_width(full_w);
                                                        let hovered = ui.response().hovered();
                                                        egui::Frame::new()
                                                            .inner_margin(egui::Margin::symmetric(
                                                                10, 8,
                                                            ))
                                                            .fill(if hovered {
                                                                palette.tertiary
                                                            } else {
                                                                egui::Color32::TRANSPARENT
                                                            })
                                                            .show(ui, |ui| {
                                                                ui.set_width(
                                                                    ui.available_width(),
                                                                );
                                                                ui.add(
                                                                    egui::Label::new(
                                                                        egui::RichText::new(
                                                                            &file_name,
                                                                        )
                                                                        .strong(),
                                                                    )
                                                                    .truncate()
                                                                    .selectable(false),
                                                                );
                                                                ui.add(
                                                                    egui::Label::new(
                                                                        egui::RichText::new(recent)
                                                                            .small()
                                                                            .color(
                                                                                ui.visuals()
                                                                                    .weak_text_color(
                                                                                    ),
                                                                            ),
                                                                    )
                                                                    .truncate()
                                                                    .selectable(false),
                                                                );
                                                            });
                                                    },
                                                )
                                                .response;
                                            if resp.hovered() {
                                                ui.ctx().set_cursor_icon(
                                                    egui::CursorIcon::PointingHand,
                                                );
                                            }
                                            // One-click open: load the recent
                                            // file directly (the dialog closes
                                            // itself when the load completes).
                                            if resp.clicked() {
                                                crate::open_flow::open_image(
                                                    state,
                                                    recent.clone(),
                                                );
                                            }
                                        }
                                    });
                            });
                    }

                    let open_enabled = !state.io_busy && !path.trim().is_empty();
                    modal_actions(ui, |ui| {
                        if ui
                            .add_enabled(open_enabled, accent_button("Open", palette.accent))
                            .clicked()
                        {
                            let path = path.clone();
                            crate::open_flow::open_image(state, &path);
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                }
                FileDialog::SaveAs { path } => {
                    file_dialog_path_row(ui, "Path:", path, "/path/to/file.ogre", |path| {
                        if let Some(p) = rfd::FileDialog::new().save_file() {
                            *path = p.to_string_lossy().to_string();
                        }
                    });
                }
                FileDialog::Export {
                    path,
                    format,
                    quality,
                    bit_depth,
                } => {
                    file_dialog_path_row(ui, "Path:", path, "/path/to/image.png", |path| {
                        if let Some(p) = rfd::FileDialog::new().save_file() {
                            *path = p.to_string_lossy().to_string();
                        }
                    });
                    file_dialog_combo_row(ui, "Format:", "export_format", format, &[
                        "PNG", "JPEG", "TIFF", "WebP", "EXR", "ORA", "SVG",
                    ]);
                    if format == "SVG" {
                        ui.label("SVG exports a flat image embedded in the SVG.");
                    }
                    if format == "JPEG" {
                        file_dialog_text_row(ui, "Quality:", quality);
                    }
                    file_dialog_combo_row(ui, "Bit depth:", "export_bit_depth", bit_depth, &[
                        "8", "16",
                    ]);
                }
                FileDialog::ExportSlices {
                    path,
                    format,
                    quality,
                    bit_depth,
                } => {
                    file_dialog_path_row(ui, "Path:", path, "/path/to/slices", |path| {
                        if let Some(p) = rfd::FileDialog::new().pick_folder() {
                            *path = p.to_string_lossy().to_string();
                        }
                    });
                    file_dialog_combo_row(ui, "Format:", "slice_export_format", format, &[
                        "PNG", "JPEG", "TIFF", "WebP", "EXR",
                    ]);
                    if format == "JPEG" {
                        file_dialog_text_row(ui, "Quality:", quality);
                    }
                    file_dialog_combo_row(
                        ui,
                        "Bit depth:",
                        "slice_export_bit_depth",
                        bit_depth,
                        &["8", "16"],
                    );
                }
            }

            if state.io_busy {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(&state.file_dialog_feedback);
                });
            } else if !state.file_dialog_feedback.is_empty() {
                ui.colored_label(ui.visuals().error_fg_color, &state.file_dialog_feedback);
            }

            if !matches!(dialog, FileDialog::Open { .. }) {
                modal_actions(ui, |ui| {
                    if ui
                        .add_enabled(!state.io_busy, accent_button("OK", palette.accent))
                        .clicked()
                    {
                        match &dialog {
                            FileDialog::New { width, height } => {
                                if state.is_busy() {
                                    state.file_dialog_feedback =
                                        "Busy; please wait for the current operation.".to_string();
                                } else if let (Ok(w), Ok(h)) =
                                    (width.parse::<u32>(), height.parse::<u32>())
                                {
                                    if w > 0 && h > 0 {
                                        state.new_blank_document((w, h));
                                        close = true;
                                    } else {
                                        state.file_dialog_feedback = "Invalid size".to_string();
                                    }
                                } else {
                                    state.file_dialog_feedback = "Invalid size".to_string();
                                }
                            }
                            FileDialog::SaveAs { path } => {
                                commit_save_as(state, path);
                            }
                            FileDialog::Export {
                                path,
                                format,
                                quality,
                                bit_depth,
                            } => {
                                let id = state.io_worker.next_id();
                                state.io_expected.insert(IoKind::Export, id);
                                let path_buf = PathBuf::from(path);
                                if format == "ORA" {
                                    state
                                        .io_worker
                                        .request(crate::io_worker::IoRequest::Export {
                                            id,
                                            doc: state.current_tab().doc.clone(),
                                            path: path_buf,
                                            kind: crate::io_worker::ExportKind::Ora,
                                        });
                                } else if format == "SVG" {
                                    state
                                        .io_worker
                                        .request(crate::io_worker::IoRequest::Export {
                                            id,
                                            doc: state.current_tab().doc.clone(),
                                            path: path_buf,
                                            kind: crate::io_worker::ExportKind::Svg,
                                        });
                                } else {
                                    let raster_format = match format.as_str() {
                                        "PNG" => ogre_io::RasterFormat::Png,
                                        "JPEG" => ogre_io::RasterFormat::Jpeg,
                                        "TIFF" => ogre_io::RasterFormat::Tiff,
                                        "WebP" => ogre_io::RasterFormat::WebP,
                                        "EXR" => ogre_io::RasterFormat::Exr,
                                        _ => {
                                            state.file_dialog_feedback =
                                                "Invalid format".to_string();
                                            return;
                                        }
                                    };
                                    let options = ogre_io::raster::ExportOptions {
                                        format: raster_format,
                                        quality: quality.parse().ok(),
                                        bit_depth: if bit_depth == "16" {
                                            ogre_io::RasterBitDepth::Sixteen
                                        } else {
                                            ogre_io::RasterBitDepth::Eight
                                        },
                                    };
                                    state
                                        .io_worker
                                        .request(crate::io_worker::IoRequest::Export {
                                            id,
                                            doc: state.current_tab().doc.clone(),
                                            path: path_buf,
                                            kind: crate::io_worker::ExportKind::Raster {
                                                canvas: state.current_tab().doc.canvas,
                                                options,
                                            },
                                        });
                                }
                                state.io_busy = true;
                                state.file_dialog_feedback = "Exporting…".to_string();
                            }
                            FileDialog::ExportSlices {
                                path,
                                format,
                                quality,
                                bit_depth,
                            } => {
                                let path_buf = PathBuf::from(path);
                                let id = state.io_worker.next_id();
                                state.io_expected.insert(IoKind::Export, id);
                                let raster_format = match format.as_str() {
                                    "PNG" => ogre_io::RasterFormat::Png,
                                    "JPEG" => ogre_io::RasterFormat::Jpeg,
                                    "TIFF" => ogre_io::RasterFormat::Tiff,
                                    "WebP" => ogre_io::RasterFormat::WebP,
                                    "EXR" => ogre_io::RasterFormat::Exr,
                                    _ => {
                                        state.file_dialog_feedback = "Invalid format".to_string();
                                        return;
                                    }
                                };
                                let options = ogre_io::raster::ExportOptions {
                                    format: raster_format,
                                    quality: quality.parse().ok(),
                                    bit_depth: if bit_depth == "16" {
                                        ogre_io::RasterBitDepth::Sixteen
                                    } else {
                                        ogre_io::RasterBitDepth::Eight
                                    },
                                };
                                state
                                    .io_worker
                                    .request(crate::io_worker::IoRequest::Export {
                                        id,
                                        doc: state.current_tab().doc.clone(),
                                        path: path_buf,
                                        kind: crate::io_worker::ExportKind::Slices { options },
                                    });
                                state.io_busy = true;
                                state.file_dialog_feedback = "Exporting slices…".to_string();
                            }
                            FileDialog::Open { .. } => {}
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            }
        });

    if close {
        state.file_dialog = None;
        state.file_dialog_feedback.clear();
        // Dismissing a Save As cancels any close that was waiting on it.
        state.close_after_save = false;
        state.pending_close_tab_id = None;
    } else {
        state.file_dialog = Some(dialog);
    }
}

const FILE_DIALOG_MODAL_WIDTH: f32 = 440.0;
const FILE_DIALOG_LABEL_WIDTH: f32 = 84.0;
const FILE_DIALOG_BROWSE_WIDTH: f32 = 92.0;
const FILE_DIALOG_SELECT_WIDTH: f32 = 110.0;

fn file_dialog_labeled_row<R>(
    ui: &mut egui::Ui,
    label: &str,
    add: impl FnOnce(&mut egui::Ui, f32) -> R,
) -> R {
    let row_h = ui.spacing().interact_size.y.max(30.0);
    ui.horizontal(|ui| {
        file_dialog_label_cell(ui, label, row_h);
        add(ui, row_h)
    })
    .inner
}

fn file_dialog_label_cell(ui: &mut egui::Ui, label: &str, row_h: f32) -> egui::Rect {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(FILE_DIALOG_LABEL_WIDTH, row_h),
        egui::Sense::hover(),
    );
    if ui.is_rect_visible(rect) {
        ui.painter().text(
            rect.left_center(),
            egui::Align2::LEFT_CENTER,
            label,
            egui::TextStyle::Body.resolve(ui.style()),
            ui.visuals().text_color(),
        );
    }
    response.rect
}

fn file_dialog_path_row(
    ui: &mut egui::Ui,
    label: &str,
    path: &mut String,
    hint: &str,
    browse: impl FnOnce(&mut String),
) -> egui::Rect {
    file_dialog_labeled_row(ui, label, |ui, row_h| {
        let input_w =
            (ui.available_width() - FILE_DIALOG_BROWSE_WIDTH - ui.spacing().item_spacing.x)
                .max(80.0);
        let input = ui.add_sized(
            [input_w, row_h],
            egui::TextEdit::singleline(path)
                .hint_text(hint)
                .vertical_align(egui::Align::Center),
        );
        if ui
            .add_sized(
                [FILE_DIALOG_BROWSE_WIDTH, row_h],
                egui::Button::new("Browse…"),
            )
            .clicked()
        {
            browse(path);
        }
        input.rect
    })
}

fn file_dialog_text_row(ui: &mut egui::Ui, label: &str, value: &mut String) -> egui::Rect {
    file_dialog_text_row_with_hint(ui, label, value, "")
}

fn file_dialog_text_row_with_hint(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut String,
    hint: &str,
) -> egui::Rect {
    file_dialog_labeled_row(ui, label, |ui, row_h| {
        ui.add_sized(
            [ui.available_width(), row_h],
            egui::TextEdit::singleline(value)
                .hint_text(hint)
                .vertical_align(egui::Align::Center),
        )
        .rect
    })
}

fn file_dialog_combo_row(
    ui: &mut egui::Ui,
    label: &str,
    id_salt: impl egui::AsIdSalt,
    value: &mut String,
    options: &[&str],
) -> egui::Rect {
    file_dialog_labeled_row(ui, label, |ui, _row_h| {
        let response = egui::ComboBox::from_id_salt(id_salt)
            .width(FILE_DIALOG_SELECT_WIDTH)
            .selected_text(value.as_str())
            .show_ui(ui, |ui| {
                for option in options {
                    ui.selectable_value(value, (*option).to_string(), *option);
                }
            });
        response.response.rect
    })
}

/// Request a background save of the current document to `path`.
pub(crate) fn request_save(state: &mut AppState, path: std::path::PathBuf) {
    let id = state.io_worker.next_id();
    state.pending_save_tab_id = Some(state.current_tab().id);
    state.io_expected.insert(IoKind::Save, id);
    state.io_worker.request(crate::io_worker::IoRequest::Save {
        id,
        doc: state.current_tab().doc.clone(),
        path,
    });
    state.io_busy = true;
    state.io_status_feedback = "Saving…".to_string();
}

/// Infer the export kind for `path` from its extension.
///
/// Unrecognised or missing extensions fall back to PNG so the user always gets
/// a usable image file rather than a silent failure.
fn infer_export_kind(
    path: &std::path::Path,
    canvas: ogre_core::Rect,
) -> crate::io_worker::ExportKind {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_lowercase());
    if ext.as_deref() == Some("ora") {
        crate::io_worker::ExportKind::Ora
    } else if ext.as_deref() == Some("svg") {
        crate::io_worker::ExportKind::Svg
    } else {
        let format = match ext.as_deref() {
            Some("jpg") | Some("jpeg") => ogre_io::RasterFormat::Jpeg,
            Some("tiff") | Some("tif") => ogre_io::RasterFormat::Tiff,
            Some("webp") => ogre_io::RasterFormat::WebP,
            Some("exr") => ogre_io::RasterFormat::Exr,
            _ => ogre_io::RasterFormat::Png,
        };
        let options = ogre_io::raster::ExportOptions {
            format,
            quality: (format == ogre_io::RasterFormat::Jpeg).then_some(90),
            bit_depth: ogre_io::RasterBitDepth::Eight,
        };
        crate::io_worker::ExportKind::Raster { canvas, options }
    }
}

/// Export the current tab back to `path`, inferring the export kind from the
/// file extension. Used by `File → Save` for imported non-`.ogre` files.
pub(crate) fn request_export_path(state: &mut AppState, path: std::path::PathBuf) {
    let id = state.io_worker.next_id();
    state.io_expected.insert(IoKind::Export, id);
    let kind = infer_export_kind(&path, state.current_tab().doc.canvas);
    state
        .io_worker
        .request(crate::io_worker::IoRequest::Export {
            id,
            doc: state.current_tab().doc.clone(),
            path,
            kind,
        });
    state.io_busy = true;
    state.io_status_feedback = "Saving…".to_string();
}

/// Commit the Save As dialog, writing `.ogre` for native paths and exporting
/// for any other extension so the saved file matches the extension the user
/// actually chose.
fn commit_save_as(state: &mut AppState, path: &str) {
    let path_buf = PathBuf::from(path);
    let ext = path_buf
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_lowercase());
    if ext.as_deref() == Some("ogre") || ext.is_none() {
        let id = state.io_worker.next_id();
        state.pending_save_tab_id = Some(state.current_tab().id);
        state.io_expected.insert(IoKind::Save, id);
        state.io_worker.request(crate::io_worker::IoRequest::Save {
            id,
            doc: state.current_tab().doc.clone(),
            path: path_buf,
        });
    } else {
        let id = state.io_worker.next_id();
        state.io_expected.insert(IoKind::Export, id);
        let kind = infer_export_kind(&path_buf, state.current_tab().doc.canvas);
        state
            .io_worker
            .request(crate::io_worker::IoRequest::Export {
                id,
                doc: state.current_tab().doc.clone(),
                path: path_buf,
                kind,
            });
    }
    state.io_busy = true;
    state.file_dialog_feedback = "Saving…".to_string();
}

/// Render the "save before closing?" confirmation modal. Save writes the
/// document (prompting for a path if needed) and closes once the save lands;
/// Discard closes immediately; Cancel keeps editing.
/// The screen point a modal should center on: the canvas center when the canvas
/// is fully visible in the window, otherwise `None` so the caller centers on the
/// whole window instead.
fn modal_center(ctx: &egui::Context, state: &AppState) -> Option<egui::Pos2> {
    let rect = state.canvas_screen_rect?;
    let screen = ctx.content_rect();
    (screen.contains(rect.min) && screen.contains(rect.max)).then(|| rect.center())
}

/// Apply [`modal_center`] to a window builder (canvas-centered, else window-centered).
fn center_modal<'a>(
    window: egui::Window<'a>,
    ctx: &egui::Context,
    state: &AppState,
) -> egui::Window<'a> {
    match modal_center(ctx, state) {
        Some(c) => window.pivot(egui::Align2::CENTER_CENTER).fixed_pos(c),
        None => window.anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO),
    }
}

/// An accent-filled primary button with legible on-accent text. Used for the
/// confirming action (Open/OK/Apply/…) in every modal so they look identical.
pub(crate) fn accent_button(text: &str, accent: egui::Color32) -> egui::Button<'static> {
    egui::Button::new(
        egui::RichText::new(text.to_owned()).color(crate::theme::visuals::on_accent(accent)),
    )
    .fill(accent)
}

/// Left-aligned modal title, styled identically across every dialog. Render it
/// as the first item inside a `.title_bar(false)` window so all headings match.
pub(crate) fn modal_heading(ui: &mut egui::Ui, title: &str) {
    ui.label(
        egui::RichText::new(title)
            .size(crate::theme::TEXT_HEADING)
            .strong(),
    );
    ui.add_space(crate::theme::SPACE_M);
}

/// Right-aligned action-button row pinned at the bottom of a modal. Add the
/// primary (accent) button first so it lands on the far right and Cancel to its
/// left. Buttons stay visible because callers keep any scrollable content in a
/// bounded `ScrollArea` above this row.
pub(crate) fn modal_actions(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(crate::theme::SPACE_M);
    // A right-to-left layout anchors to the far edge, so inside an auto-sizing
    // window it would claim the full remaining height and the modal would never
    // shrink. Allocate an explicit single-row rect so the height stays bounded.
    let row_h = ui.spacing().interact_size.y;
    ui.allocate_ui_with_layout(
        egui::vec2(ui.available_width(), row_h),
        egui::Layout::right_to_left(egui::Align::Center),
        add,
    );
}

fn render_close_prompt(ui: &mut egui::Ui, state: &mut AppState) {
    if !state.close_prompt {
        return;
    }
    let palette = crate::theme::resolve(state.preferences.theme, ui.ctx());
    egui::Window::new("Close document")
        .collapsible(false)
        .resizable(false)
        .title_bar(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ui.ctx(), |ui| {
            modal_heading(ui, "Close document");
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                state.close_prompt = false;
            }
            ui.label("This document has unsaved changes. Save before closing?");
            modal_actions(ui, |ui| {
                if ui.add(accent_button("Save", palette.accent)).clicked() {
                    state.close_prompt = false;
                    state.close_after_save = true;
                    state.pending_close_tab_id = Some(state.tabs[state.active_tab].id);
                    match state.current_tab().last_save_path.clone() {
                        Some(path) => request_save(state, path),
                        None => {
                            state.file_dialog = Some(FileDialog::SaveAs {
                                path: String::new(),
                            })
                        }
                    }
                }
                if ui.button("Discard").clicked() {
                    state.close_prompt = false;
                    state.close_after_save = false;
                    state.close_document();
                }
                if ui.button("Cancel").clicked() {
                    state.close_prompt = false;
                }
            });
        });
}

/// Maximum time a background-removal worker is allowed to run before the UI
/// treats it as failed and drops the result.
const BG_REMOVAL_TIMEOUT: Duration = Duration::from_secs(30);
/// Longer ceiling for AI refinement, whose first run downloads a ~170 MB model.
const BG_REMOVAL_AI_TIMEOUT: Duration = Duration::from_secs(600);

/// Start background removal on the active raster layer in a worker thread.
///
/// `remove_matte` can take seconds on a large image, so it runs off the UI
/// thread; a snapshot of the layer's buffer (Arc-cheap) is handed to the worker
/// and the result is applied via [`SetLayerBufferCmd`](ogre_core::SetLayerBufferCmd)
/// when it arrives. Edits are blocked and a spinner shows while it runs. With
/// `ai`, the color-key result is then refined by the ML matte (model fetched on
/// first use); if that fails, the plain color-key result is used.
fn start_background_removal(state: &mut AppState, options: ogre_core::MatteOptions, ai: bool) {
    if state.is_busy() {
        return;
    }
    let Some(id) = active_raster_layer_id(state) else {
        return;
    };
    let layer = match state.current_tab().doc.layer(id) {
        Ok(l) => l,
        Err(_) => return,
    };
    if layer.locked {
        state.io_status_feedback = "Layer is locked.".to_string();
        return;
    }
    let buffer = layer.buffer().expect("raster layer has a buffer").clone();

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = ogre_core::remove_matte_with(&buffer, options);
        let result = refine_if_ai(result, &buffer, ai);
        let _ = tx.send(result);
    });
    state.bg_removal_rx = Some(rx);
    state.bg_removal_layer = Some(id);
    state.bg_removal_started = Some(std::time::Instant::now());
    state.bg_removal_ai = ai;
    state.io_status_feedback = if ai {
        "Refining with AI…".to_string()
    } else {
        "Removing background…".to_string()
    };
}

/// Whether the AI matte model is already downloaded (so the spinner can skip the
/// one-time-download note). Always `false` without the `ml` feature, where AI
/// refinement is unavailable anyway.
#[cfg(feature = "ml")]
fn ai_model_cached() -> bool {
    ogre_io::matte_ml::model_is_cached()
}

#[cfg(not(feature = "ml"))]
fn ai_model_cached() -> bool {
    false
}

/// Refine a color-key result with the ML matte when `ai` is set and the `ml`
/// feature is built in; otherwise return it unchanged. ML failures (no network,
/// bad model) fall back to the plain color-key result rather than aborting.
#[cfg(feature = "ml")]
fn refine_if_ai(
    result: Option<ogre_core::TiledBuffer>,
    original: &ogre_core::TiledBuffer,
    ai: bool,
) -> Option<ogre_core::TiledBuffer> {
    let colorkey = result?;
    if !ai {
        return Some(colorkey);
    }
    match ogre_io::matte_ml::ensure_model()
        .and_then(|m| ogre_io::matte_ml::refine_with_ml(&colorkey, original, &m))
    {
        Ok(refined) => Some(refined),
        Err(e) => {
            eprintln!("AI matte refinement failed, using color-key result: {e}");
            Some(colorkey)
        }
    }
}

#[cfg(not(feature = "ml"))]
fn refine_if_ai(
    result: Option<ogre_core::TiledBuffer>,
    _original: &ogre_core::TiledBuffer,
    _ai: bool,
) -> Option<ogre_core::TiledBuffer> {
    result
}

/// Poll the background-removal worker and apply its result. Called once per
/// frame; a no-op when no job is running.
pub(crate) fn poll_background_removal(state: &mut AppState) {
    let Some(rx) = state.bg_removal_rx.take() else {
        return;
    };
    let timeout = if state.bg_removal_ai {
        BG_REMOVAL_AI_TIMEOUT
    } else {
        BG_REMOVAL_TIMEOUT
    };
    if state
        .bg_removal_started
        .map(|t| t.elapsed() > timeout)
        .unwrap_or(false)
    {
        state.bg_removal_layer = None;
        state.bg_removal_started = None;
        state.bg_removal_ai = false;
        state.io_status_feedback = "Background removal timed out.".to_string();
        return;
    }
    match rx.try_recv() {
        Ok(result) => {
            let layer = state.bg_removal_layer.take();
            state.bg_removal_started = None;
            state.bg_removal_ai = false;
            state.io_status_feedback.clear();
            match (result, layer) {
                (Some(buffer), Some(layer)) => {
                    dispatch::dispatch_or_report(
                        state,
                        Box::new(ogre_core::SetLayerBufferCmd::new(
                            layer,
                            buffer,
                            "Remove background",
                        )),
                    );
                }
                _ => {
                    state.io_status_feedback = "No removable background detected.".to_string();
                }
            }
        }
        Err(std::sync::mpsc::TryRecvError::Empty) => {
            // Still working; keep the receiver.
            state.bg_removal_rx = Some(rx);
        }
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            state.bg_removal_layer = None;
            state.bg_removal_started = None;
            state.bg_removal_ai = false;
            state.io_status_feedback = "Background removal failed.".to_string();
        }
    }
}

/// Render the Remove Background options dialog. Confirming starts the worker.
pub(crate) fn render_remove_bg_dialog(ui: &mut egui::Ui, state: &mut AppState) {
    let Some(mut options) = state.remove_bg_dialog else {
        return;
    };
    #[cfg(feature = "ml")]
    let mut ai = state.remove_bg_ai;
    #[cfg(not(feature = "ml"))]
    let ai = state.remove_bg_ai;
    let busy = state.is_busy();
    let palette = crate::theme::resolve(state.preferences.theme, ui.ctx());
    let mut start = false;
    let mut open = true;
    center_modal(
        egui::Window::new("Remove Background")
            .collapsible(false)
            .resizable(false)
            .title_bar(false),
        ui.ctx(),
        state,
    )
    .show(ui.ctx(), |ui| {
        modal_heading(ui, "Remove Background");
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            open = false;
        }
        ui.add(
            egui::Slider::new(&mut options.tolerance, 0.0..=1.0)
                .text("Tolerance")
                .step_by(0.01),
        )
        .on_hover_text("How close to the background color counts as fully removed");
        ui.add(
            egui::Slider::new(&mut options.edge_softness, 0.0..=0.8)
                .text("Edge cleanup")
                .step_by(0.01),
        )
        .on_hover_text("Removes the leftover fringe by softening anti-aliased edges");
        ui.checkbox(&mut options.remove_enclosed, "Remove enclosed areas")
            .on_hover_text(
                "Also clear background trapped inside the subject (e.g. gaps between hairs). \
                     May remove matching colors inside the subject.",
            );
        #[cfg(feature = "ml")]
        ui.checkbox(&mut ai, "Refine edges with AI").on_hover_text(
            "Use a segmentation model to clean background trapped in fine detail (hair) \
                 that color matching can't reach. First use downloads a ~170 MB model.",
        );
        modal_actions(ui, |ui| {
            if ui
                .add_enabled(!busy, accent_button("Remove", palette.accent))
                .clicked()
            {
                start = true;
            }
            if ui.button("Cancel").clicked() {
                open = false;
            }
        });
    });

    state.remove_bg_ai = ai;
    if start {
        state.remove_bg_dialog = None;
        start_background_removal(state, options, ai);
    } else if !open {
        state.remove_bg_dialog = None;
    } else {
        // Persist edited slider/checkbox values for next frame.
        state.remove_bg_dialog = Some(options);
    }
}

/// Render the modal "Removing background" spinner while the worker runs, and
/// keep the UI repainting so the result is polled promptly.
pub(crate) fn render_bg_removal_spinner(ui: &mut egui::Ui, state: &AppState) {
    if state.bg_removal_rx.is_none() {
        return;
    }
    center_modal(
        egui::Window::new("Removing background")
            .collapsible(false)
            .resizable(false),
        ui.ctx(),
        state,
    )
    .show(ui.ctx(), |ui| {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new());
            ui.add_space(crate::theme::SPACE_S);
            // Only mention the one-time download when the model isn't cached yet.
            let label = if state.bg_removal_ai {
                if ai_model_cached() {
                    "Refining with AI…"
                } else {
                    "Refining with AI… (first run downloads a model)"
                }
            } else {
                "Removing background…"
            };
            ui.label(label);
        });
    });
    // The worker runs off-thread; request repaints so the result is applied
    // without waiting for the next input event.
    ui.ctx().request_repaint();
}

/// Render the "quit with unsaved changes" confirmation modal.
///
/// Shown when a window-close request is intercepted (OS close button or
/// File → Quit) while the open document has unsaved edits. "Exit without
/// Saving" confirms the quit and closes the window; "Cancel" (the themed,
/// safe default) dismisses and keeps editing.
pub(crate) fn render_quit_prompt(ui: &mut egui::Ui, state: &mut AppState) {
    if !state.quit_confirm {
        return;
    }
    let palette = crate::theme::resolve(state.preferences.theme, ui.ctx());
    center_modal(
        egui::Window::new("Unsaved changes")
            .collapsible(false)
            .resizable(false)
            .title_bar(false),
        ui.ctx(),
        state,
    )
    .show(ui.ctx(), |ui| {
        modal_heading(ui, "Unsaved changes");
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            state.quit_confirm = false;
        }
        let unsaved_count = state.tabs.iter().filter(|t| t.unsaved).count();
        let message = if unsaved_count == 1 {
            "You have 1 unsaved document. Exit without saving?".to_string()
        } else {
            format!("You have {unsaved_count} unsaved documents. Exit without saving?")
        };
        ui.label(message);
        modal_actions(ui, |ui| {
            // Accent (safe-default) Cancel is added first so it lands on the right.
            if ui.add(accent_button("Cancel", palette.accent)).clicked() {
                state.quit_confirm = false;
            }
            if ui.button("Exit without Saving").clicked() {
                state.quit_confirm = false;
                state.confirmed_quit = true;
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    });
}

pub(crate) fn load_document_from_path_with_options(
    state: &mut AppState,
    path: &str,
    svg_options: crate::io_worker::SvgImportOptions,
) {
    if state.is_busy() {
        state.io_status_feedback = "Busy; please wait for the current operation.".to_string();
        return;
    }
    let id = state.io_worker.next_id();
    state.io_expected.insert(IoKind::Open, id);
    state.io_worker.request(crate::io_worker::IoRequest::Open {
        id,
        path: path.into(),
        svg_options,
    });
    state.io_busy = true;
    if state.file_dialog.is_some() {
        state.file_dialog_feedback = "Opening…".to_string();
    } else {
        state.io_status_feedback = "Opening…".to_string();
    }
}

/// Handle files dragged onto the window: with no document open, open the first
/// as a new document; with one open, add the first as a new layer.
///
/// Only the first dropped file is handled per drop (the resize prompt tracks one
/// pending image at a time), with a note when more were dropped.
pub(crate) fn handle_dropped_paths(state: &mut AppState, paths: &[std::path::PathBuf]) {
    if paths.is_empty() {
        return;
    }
    // `dropped_files` is delivered for a single frame, so report rather than
    // silently discard when a background operation blocks the drop.
    if state.is_busy() {
        state.io_status_feedback = "Busy; please wait for the current operation.".to_string();
        return;
    }
    let first = &paths[0];
    if state.welcome {
        crate::open_flow::open_image(state, first.clone());
    } else {
        let id = state.io_worker.next_id();
        state
            .io_expected
            .insert(crate::io_worker::IoKind::AddAsLayer, id);
        state
            .io_worker
            .request(crate::io_worker::IoRequest::AddAsLayer {
                id,
                path: first.clone(),
            });
        state.io_busy = true;
        if paths.len() > 1 {
            state.io_status_feedback = format!(
                "Adding the first of {} dropped files as a layer…",
                paths.len()
            );
        } else {
            state.io_status_feedback = format!("Adding {} as layer…", first.display());
        }
    }
}

fn render_selection_dialogs(ui: &mut egui::Ui, state: &mut AppState) {
    let mut close = false;
    let mut invalid = false;
    let mut command: Option<Box<dyn ogre_core::Command>> = None;
    let palette = crate::theme::resolve(state.preferences.theme, ui.ctx());
    // Captured up-front so the Stroke arm can read them without borrowing
    // `state` while `state.selection_dialog` is mutably borrowed below.
    let stroke_sel = state.current_tab().doc.selection.clone();
    let stroke_color = state.foreground;

    if let Some(dialog) = state.selection_dialog.as_mut() {
        let (title, label) = match dialog {
            SelectionDialog::Feather { .. } => ("Feather Selection", "Radius:"),
            SelectionDialog::Grow { .. } => ("Grow Selection", "Pixels:"),
            SelectionDialog::Shrink { .. } => ("Shrink Selection", "Pixels:"),
            SelectionDialog::Stroke { .. } => ("Stroke Selection", "Width:"),
        };

        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .title_bar(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ui.ctx(), |ui| {
                modal_heading(ui, title);
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    close = true;
                }
                ui.horizontal(|ui| {
                    ui.label(label);
                    let value = match dialog {
                        SelectionDialog::Feather { radius } => radius,
                        SelectionDialog::Grow { amount } => amount,
                        SelectionDialog::Shrink { amount } => amount,
                        SelectionDialog::Stroke { width } => width,
                    };
                    ui.text_edit_singleline(value);
                });
                if invalid {
                    ui.colored_label(ui.visuals().error_fg_color, "Invalid value");
                }
                modal_actions(ui, |ui| {
                    if ui.add(accent_button("OK", palette.accent)).clicked() {
                        match dialog {
                            SelectionDialog::Feather { radius } => {
                                if let Ok(r) = radius.parse::<f32>() {
                                    if r > 0.0 {
                                        command =
                                            Some(Box::new(ogre_core::FeatherSelectionCmd::new(r)));
                                        close = true;
                                    } else {
                                        invalid = true;
                                    }
                                } else {
                                    invalid = true;
                                }
                            }
                            SelectionDialog::Grow { amount } => {
                                if let Ok(n) = amount.parse::<u32>() {
                                    if n > 0 {
                                        command =
                                            Some(Box::new(ogre_core::GrowSelectionCmd::new(n)));
                                        close = true;
                                    } else {
                                        invalid = true;
                                    }
                                } else {
                                    invalid = true;
                                }
                            }
                            SelectionDialog::Shrink { amount } => {
                                if let Ok(n) = amount.parse::<u32>() {
                                    if n > 0 {
                                        command =
                                            Some(Box::new(ogre_core::ShrinkSelectionCmd::new(n)));
                                        close = true;
                                    } else {
                                        invalid = true;
                                    }
                                } else {
                                    invalid = true;
                                }
                            }
                            SelectionDialog::Stroke { width } => {
                                if let Ok(w) = width.parse::<f32>() {
                                    if w > 0.0 {
                                        command =
                                            Some(Box::new(ogre_core::StrokeSelectionCmd::new(
                                                stroke_color,
                                                w,
                                                1.0,
                                                stroke_sel.clone(),
                                            )));
                                        close = true;
                                    } else {
                                        invalid = true;
                                    }
                                } else {
                                    invalid = true;
                                }
                            }
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
    }

    if let Some(cmd) = command {
        dispatch::dispatch_or_report(state, cmd);
    }
    if close {
        state.selection_dialog = None;
    }
}

/// Open the Canvas-Size dialog pre-filled with the document's current size.
pub(crate) fn open_canvas_resize_dialog(state: &mut AppState) {
    state.canvas_size_dialog = Some(crate::state::CanvasSizeDialog {
        width: state.current_tab().doc.canvas.w.max(1),
        height: state.current_tab().doc.canvas.h.max(1),
        anchor: ogre_core::CanvasAnchor::Center,
    });
}

/// Open the Color Range dialog (`Select → Color Range…`).
pub(crate) fn open_color_range_dialog(state: &mut AppState) {
    state.color_range_dialog = Some(ColorRangeDialog::default());
}

/// Render the Color Range modal (§3.4.3). Picks a seed color + fuzziness and,
/// on OK, builds a selection from every pixel within that perceptual distance
/// (`select_by_color` over the merged composite or the active layer).
pub(crate) fn render_color_range_dialog(ctx: &egui::Context, state: &mut AppState) {
    let palette = crate::theme::resolve(state.preferences.theme, ctx);
    let mut close = false;
    let mut confirm = false;

    if let Some(ColorRangeDialog {
        seed,
        fuzziness,
        invert,
        sample_all_layers,
    }) = state.color_range_dialog.as_mut()
    {
        egui::Window::new("Color Range")
            .collapsible(false)
            .resizable(false)
            .title_bar(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                modal_heading(ui, "Color Range");
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    close = true;
                }
                ui.horizontal(|ui| {
                    ui.label("Color:");
                    let mut rgba = [seed.r, seed.g, seed.b, seed.a];
                    ui.color_edit_button_rgba_unmultiplied(&mut rgba);
                    *seed = ogre_core::Rgba32F::new(rgba[0], rgba[1], rgba[2], rgba[3]);
                });
                ui.add(
                    egui::Slider::new(fuzziness, 0.0..=1.0)
                        .text("Fuzziness")
                        .step_by(0.01),
                )
                .on_hover_text("Higher selects a wider range of similar colors");
                ui.checkbox(sample_all_layers, "Sample all layers");
                ui.checkbox(invert, "Invert");
                modal_actions(ui, |ui| {
                    if ui.add(accent_button("OK", palette.accent)).clicked() {
                        confirm = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true; // no command ⇒ no history entry
                    }
                });
            });
    }

    if close {
        state.color_range_dialog = None;
    }
    if confirm {
        // The dialog borrow above has ended; read the confirmed values into
        // locals, then compute + dispatch without aliasing `state`.
        let Some(ColorRangeDialog {
            seed,
            fuzziness,
            invert,
            sample_all_layers,
        }) = state.color_range_dialog.as_ref()
        else {
            return;
        };
        let (seed, fuzziness, invert, sample_all_layers) =
            (*seed, *fuzziness, *invert, *sample_all_layers);
        let canvas = state.current_tab().doc.canvas;
        let sel = if sample_all_layers {
            match ogre_core::composite_region(&state.current_tab().doc, canvas) {
                Ok(merged) => {
                    ogre_core::select_by_color(&merged, glam::IVec2::ZERO, seed, fuzziness, canvas)
                }
                Err(_) => ogre_core::Selection::none(),
            }
        } else {
            let active = state.current_tab().doc.active;
            active
                .and_then(|l| state.current_tab().doc.layer(l).ok())
                .filter(|l| l.is_raster())
                .map(|l| {
                    // Scan the whole canvas in layer-local space so transparent
                    // pixels are considered (get_pixel returns TRANSPARENT for
                    // unoccupied local pixels).
                    let local_bounds = ogre_core::Rect::new(
                        canvas.x - l.offset.x,
                        canvas.y - l.offset.y,
                        canvas.w,
                        canvas.h,
                    );
                    ogre_core::select_by_color(
                        l.buffer().unwrap(),
                        l.offset,
                        seed,
                        fuzziness,
                        local_bounds,
                    )
                })
                .unwrap_or_else(ogre_core::Selection::none)
        };
        let sel = if invert { sel.invert(canvas) } else { sel };
        state.color_range_dialog = None;
        dispatch::dispatch_or_report(state, Box::new(ogre_core::SetSelectionCmd::new(sel)));
    }
}

/// SVG import options modal (shown before opening an SVG/SVGZ file).
pub(crate) fn render_svg_import_dialog(ctx: &egui::Context, state: &mut AppState) {
    let palette = crate::theme::resolve(state.preferences.theme, ctx);
    let mut close = false;
    let mut confirm = false;

    if let Some(dialog) = state.svg_import_dialog.as_mut() {
        egui::Window::new("Import SVG")
            .collapsible(false)
            .resizable(false)
            .title_bar(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                modal_heading(ui, "Import SVG");
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    close = true;
                }
                ui.label("Import mode:");
                ui.radio_value(
                    &mut dialog.mode,
                    ogre_io::svg::SvgImportMode::Rasterize,
                    "Rasterize — flatten to a pixel layer",
                );
                ui.radio_value(
                    &mut dialog.mode,
                    ogre_io::svg::SvgImportMode::Vector,
                    "Vector — editable paths only",
                );
                ui.radio_value(
                    &mut dialog.mode,
                    ogre_io::svg::SvgImportMode::Both,
                    "Both — vector layer with pre-rasterized fallback",
                );
                ui.horizontal(|ui| {
                    ui.label("DPI:");
                    ui.text_edit_singleline(&mut dialog.dpi);
                });
                if !dialog.feedback.is_empty() {
                    ui.colored_label(ui.visuals().error_fg_color, &dialog.feedback);
                }
                modal_actions(ui, |ui| {
                    if ui.add(accent_button("Import", palette.accent)).clicked() {
                        confirm = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
    }

    if close {
        state.svg_import_dialog = None;
        return;
    }

    if confirm {
        let Some(dialog) = state.svg_import_dialog.as_mut() else {
            return;
        };
        let dpi = match dialog.dpi.parse::<f32>() {
            Ok(v) if v.is_finite() && v > 0.0 && v <= 2400.0 => v,
            _ => {
                dialog.feedback = "DPI must be a positive number no greater than 2400".to_string();
                return;
            }
        };
        let options = ogre_io::svg::SvgImportOptions {
            mode: dialog.mode,
            dpi,
        };
        let path = dialog.path.clone();
        state.svg_import_dialog = None;
        crate::open_flow::open_image_with_options(state, path, options);
    }
}

/// Layer-rename modal (opened by F2 / Layer → Rename).
fn render_rename_dialog(ui: &mut egui::Ui, state: &mut AppState) {
    let mut close = false;
    let mut commit = false;
    if let Some((_id, name)) = state.rename_dialog.as_mut() {
        let palette = crate::theme::resolve(state.preferences.theme, ui.ctx());
        egui::Window::new("Rename Layer")
            .collapsible(false)
            .resizable(false)
            .title_bar(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ui.ctx(), |ui| {
                modal_heading(ui, "Rename Layer");
                // Escape cancels; Enter confirms (single-line edit doesn't
                // consume Enter, so this is safe).
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    close = true;
                }
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    commit = true;
                    close = true;
                }
                ui.horizontal(|ui| {
                    ui.label("Name:");
                    ui.text_edit_singleline(name);
                });
                modal_actions(ui, |ui| {
                    if ui.add(accent_button("OK", palette.accent)).clicked() {
                        commit = true;
                        close = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
    }
    if commit {
        if let Some((id, name)) = state.rename_dialog.take() {
            dispatch::dispatch_or_report(state, Box::new(ogre_core::RenameCmd::new(id, name)));
        }
    } else if close {
        state.rename_dialog = None;
    }
}

fn render_canvas_size_dialog(ui: &mut egui::Ui, state: &mut AppState) {
    let mut close = false;
    let mut command: Option<Box<dyn ogre_core::Command>> = None;
    let palette = crate::theme::resolve(state.preferences.theme, ui.ctx());

    if let Some(crate::state::CanvasSizeDialog {
        width,
        height,
        anchor,
    }) = state.canvas_size_dialog.as_mut()
    {
        egui::Window::new("Canvas Size")
            .collapsible(false)
            .resizable(false)
            .title_bar(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ui.ctx(), |ui| {
                // All three input controls share one width.
                ui.set_width(190.0);
                const FIELD_W: f32 = 110.0;
                // Numeric spinners are made exactly as tall as the Anchor combo so
                // the three rows line up.
                let field_h = ui.spacing().interact_size.y;

                modal_heading(ui, "Canvas Size");
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    close = true;
                }
                // Spinners only accept digits and respond to scroll + Up/Down
                // arrow keys; the range guarantees a valid (≥1) size, so there is
                // no "invalid size" state to report.
                egui::Grid::new("canvas_size_grid")
                    .num_columns(2)
                    .spacing([8.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Width:");
                        ui.add_sized(
                            [FIELD_W, field_h],
                            egui::DragValue::new(width).speed(1.0).range(1..=8192),
                        );
                        ui.end_row();
                        ui.label("Height:");
                        ui.add_sized(
                            [FIELD_W, field_h],
                            egui::DragValue::new(height).speed(1.0).range(1..=8192),
                        );
                        ui.end_row();
                        ui.label("Anchor:");
                        egui::ComboBox::from_id_salt("canvas_anchor")
                            .width(FIELD_W)
                            .selected_text(anchor_name(*anchor))
                            .show_ui(ui, |ui| {
                                for a in all_anchors() {
                                    ui.selectable_value(anchor, a, anchor_name(a));
                                }
                            });
                        ui.end_row();
                    });
                modal_actions(ui, |ui| {
                    if ui.add(accent_button("Apply", palette.accent)).clicked() {
                        command = Some(Box::new(ogre_core::ResizeCanvasCmd::new(
                            (*width).max(1),
                            (*height).max(1),
                            *anchor,
                        )));
                        close = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
    }

    if let Some(cmd) = command {
        dispatch::dispatch_or_report(state, cmd);
    }
    if close {
        state.canvas_size_dialog = None;
    }
}

fn anchor_name(anchor: ogre_core::CanvasAnchor) -> &'static str {
    match anchor {
        ogre_core::CanvasAnchor::TopLeft => "Top-left",
        ogre_core::CanvasAnchor::TopCenter => "Top-center",
        ogre_core::CanvasAnchor::TopRight => "Top-right",
        ogre_core::CanvasAnchor::CenterLeft => "Center-left",
        ogre_core::CanvasAnchor::Center => "Center",
        ogre_core::CanvasAnchor::CenterRight => "Center-right",
        ogre_core::CanvasAnchor::BottomLeft => "Bottom-left",
        ogre_core::CanvasAnchor::BottomCenter => "Bottom-center",
        ogre_core::CanvasAnchor::BottomRight => "Bottom-right",
    }
}

fn all_anchors() -> [ogre_core::CanvasAnchor; 9] {
    [
        ogre_core::CanvasAnchor::TopLeft,
        ogre_core::CanvasAnchor::TopCenter,
        ogre_core::CanvasAnchor::TopRight,
        ogre_core::CanvasAnchor::CenterLeft,
        ogre_core::CanvasAnchor::Center,
        ogre_core::CanvasAnchor::CenterRight,
        ogre_core::CanvasAnchor::BottomLeft,
        ogre_core::CanvasAnchor::BottomCenter,
        ogre_core::CanvasAnchor::BottomRight,
    ]
}

/// Show the dock area inside the provided `Ui`.
///
/// The dock state is temporarily moved out of `app` so that `egui_dock` can
/// borrow it mutably while panel renderers receive a mutable borrow of the rest
/// of [`OgreApp`] (including the GPU renderer). The dock state is restored after
/// the draw call.
pub fn show_dock_area(ui: &mut egui::Ui, app: &mut OgreApp) {
    sync_document_tabs(&mut app.state);
    let mut dock = std::mem::replace(&mut app.state.dock, DockState::new(vec![Panel::Layers]));
    let mut viewer = PanelViewer { app };

    // egui_dock derives the tab-bar background from `extreme_bg_color`, which is
    // our `bg` stop (black for Black Knight / OLED Black). Use the main panel
    // background (`secondary` / `panel_fill`) so the tab bar blends with the
    // rest of the chrome.
    let mut dock_style = egui_dock::Style::from_egui(ui.style().as_ref());
    dock_style.tab_bar.bg_fill = ui.visuals().panel_fill;

    DockArea::new(&mut dock)
        .style(dock_style)
        // Keep the layout locked: document canvas on the left, Layers panel on
        // the right. Without this, a dragged tab can accidentally merge the
        // Layers panel into the document tab bar.
        .draggable_tabs(false)
        // Also forbid creating new splits via drag handles, so the layout can
        // only change through the explicit View → Reset Layout action.
        .allowed_splits(AllowedSplits::None)
        .show_inside(ui, &mut viewer);
    app.state.dock = dock;
}

/// Information displayed in the global status bar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusInfo {
    /// Zoom percentage text, e.g. "100%".
    pub zoom: String,
    /// Document-space cursor coordinate text, e.g. "12, 34".
    pub cursor: String,
    /// Canvas size text, e.g. "1920×1080".
    pub canvas: String,
    /// Name of the active layer, or "—".
    pub layer: String,
    /// Active or in-progress selection dimensions, e.g. "100×80", or empty.
    pub selection: String,
}

/// Minimum logical inner window size requested by "fit window to canvas".
const FIT_WINDOW_MIN_INNER_W: f32 = 600.0;
const FIT_WINDOW_MIN_INNER_H: f32 = 600.0;

/// Build the strings shown in the status bar for `state`.
pub fn status_info(state: &AppState) -> StatusInfo {
    let zoom = format!("{:.0}%", state.current_tab().viewport.zoom * 100.0);
    let cursor = state
        .cursor_doc_pos
        .map(|p| format!("{}, {}", p.x, p.y))
        .unwrap_or_else(|| "—".to_string());
    let canvas = format!(
        "{}×{}",
        state.current_tab().doc.canvas.w,
        state.current_tab().doc.canvas.h
    );
    let layer = state
        .current_tab()
        .doc
        .active
        .and_then(|id| {
            state
                .current_tab()
                .doc
                .layer(id)
                .ok()
                .map(|l| l.name.clone())
        })
        .unwrap_or_else(|| "—".to_string());
    // Live selection dimensions: prefer the in-progress marquee (so the user
    // sees W×H while dragging), falling back to the committed selection bounds.
    let selection = state
        .tool_manager
        .pending_selection_rect()
        .or_else(|| state.current_tab().doc.selection.bounds())
        .map(|r| format!("{}×{}", r.w, r.h))
        .unwrap_or_default();
    StatusInfo {
        zoom,
        cursor,
        canvas,
        layer,
        selection,
    }
}

/// Resize the OS window so the canvas viewport fits the whole document at the
/// current zoom, while keeping a usable minimum window size.
///
/// The non-canvas chrome (menu bar, tool sidebar, status bar, any docked panels)
/// is the difference between the full egui screen and the recorded canvas rect;
/// the new inner size is that chrome plus the image's on-screen size at the
/// current zoom, clamped to a 600×600 inner window minimum. The pan is reset so
/// the document's top-left sits at the canvas origin.
///
/// On Wayland the compositor may ignore programmatic resizes; on X11 and most
/// compositors it is honored on the next frame.
/// Begin fitting the window so the canvas fills the viewport, subject to the
/// minimum window size.
///
/// A single resize only partly fits, because the Layers dock panel is sized as
/// a *fraction* of the window: growing the window also grows the panel, so the
/// canvas lands short (a geometric series that took ~5 manual clicks to
/// converge). Instead we run [`step_fit_window_to_canvas`] for a few frames
/// until the canvas matches the image.
fn request_fit_window_to_canvas(state: &mut AppState) {
    state.fit_to_canvas_pending = 24;
}

/// Compute the requested inner window size for a canvas-fit step.
fn fit_window_inner_size(
    screen: egui::Vec2,
    canvas_rect: egui::Rect,
    image_logical: egui::Vec2,
) -> egui::Vec2 {
    let chrome = screen - canvas_rect.size();
    let exact = chrome + image_logical;
    egui::vec2(
        exact.x.max(FIT_WINDOW_MIN_INNER_W),
        exact.y.max(FIT_WINDOW_MIN_INNER_H),
    )
}

fn vec2_close(a: egui::Vec2, b: egui::Vec2) -> bool {
    (a.x - b.x).abs() <= 1.0 && (a.y - b.y).abs() <= 1.0
}

/// One iteration of the window-fit loop. Call after the canvas has rendered so
/// `canvas_screen_rect` reflects this frame's layout.
pub(crate) fn step_fit_window_to_canvas(ctx: &egui::Context, state: &mut AppState) {
    if state.fit_to_canvas_pending == 0 {
        return;
    }
    let Some(canvas_rect) = state.canvas_screen_rect else {
        state.fit_to_canvas_pending = 0;
        return;
    };
    let canvas = state.current_tab().doc.canvas;
    if canvas.w == 0 || canvas.h == 0 {
        state.fit_to_canvas_pending = 0;
        return;
    }
    let ppp = ctx.pixels_per_point();
    let zoom = state.viewport().zoom;
    // Image size on screen in logical points: doc px → physical px (×zoom) →
    // logical points (÷ppp).
    let image_logical = egui::vec2(canvas.w as f32 * zoom / ppp, canvas.h as f32 * zoom / ppp);
    let screen = ctx.content_rect().size();
    let new_inner = fit_window_inner_size(screen, canvas_rect, image_logical);

    // Converged: either the canvas matches the image, or the 600×600 minimum is
    // the limiting size and the window is already at the requested dimensions.
    if vec2_close(canvas_rect.size(), image_logical) || vec2_close(screen, new_inner) {
        state.fit_to_canvas_pending = 0;
        return;
    }

    // Align the document's top-left with the canvas origin so it fills exactly.
    state.viewport_mut().pan = glam::Vec2::ZERO;
    state.dirty = true;
    state.fit_to_canvas_pending -= 1;
    ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(new_inner));
    ctx.request_repaint();
}

/// Render the global status bar at the bottom of the window.
pub fn status_bar(ui: &mut egui::Ui, state: &mut AppState) {
    let info = status_info(state);
    ui.horizontal(|ui| {
        if ui
            .button(concat!("v", env!("CARGO_PKG_VERSION")))
            .on_hover_text("Check for Updates")
            .clicked()
        {
            crate::help::start_update_check(state, ui.ctx());
        }
        // Document-specific status only applies when one is open.
        if !state.welcome {
            ui.separator();
            let zoom_btn = ui.button(format!("Zoom: {}", info.zoom));
            if zoom_btn.clicked() {
                state.show_zoom_popup = !state.show_zoom_popup;
            }
            let mut open = state.show_zoom_popup;
            egui::Popup::from_response(&zoom_btn)
                .align(egui::RectAlign::TOP_START)
                .open_bool(&mut open)
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                .show(|ui| {
                    ui.set_width(90.0);
                    let before = state.current_tab().viewport.zoom;
                    let mut zoom = before;
                    ui.label(format!("{:.0}%", zoom * 100.0));
                    ui.add_sized(
                        [60.0, 180.0],
                        egui::Slider::new(&mut zoom, 0.1..=8.0)
                            .vertical()
                            .logarithmic(true),
                    );
                    ui.horizontal(|ui| {
                        if ui.button("−").clicked() {
                            zoom = crate::state::clamp_zoom(zoom / 1.25);
                        }
                        if ui.button("Reset").clicked() {
                            zoom = 1.0;
                        }
                        if ui.button("+").clicked() {
                            zoom = crate::state::clamp_zoom(zoom * 1.25);
                        }
                    });
                    zoom = crate::state::clamp_zoom(zoom);
                    if (zoom - before).abs() > f32::EPSILON {
                        state.current_tab_mut().viewport.zoom = zoom;
                        state.dirty = true;
                    }
                });
            state.show_zoom_popup = open;
            ui.separator();
            ui.label(format!("Cursor: {}", info.cursor));
            // Per-tool modifier-behavior hint, dimmed so it sits below the
            // primary status info.
            ui.label(
                egui::RichText::new(state.tool_manager.active().hint())
                    .small()
                    .color(ui.visuals().weak_text_color()),
            );
            // Surface the most recent command-dispatch failure (cleared on the
            // next successful dispatch). Most actions succeed silently; this
            // only appears when a menu action could not run.
            if let Some(msg) = state.error_feedback.as_ref() {
                ui.separator();
                ui.colored_label(ui.visuals().error_fg_color, msg);
            }
            ui.separator();
            let canvas_btn = ui
                .button(format!("Canvas: {}", info.canvas))
                .on_hover_text("Click to resize the canvas · right-click to fit the window to it");
            if canvas_btn.clicked() {
                open_canvas_resize_dialog(state);
            }
            if canvas_btn.secondary_clicked() {
                request_fit_window_to_canvas(state);
            }
            if !info.selection.is_empty() {
                ui.separator();
                ui.label(format!("Sel: {}", info.selection));
            }
        }
        if !state.io_status_feedback.is_empty() {
            ui.separator();
            ui.label(&state.io_status_feedback);
        }
        if !state.welcome {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(format!("Layer: {}", info.layer));
            });
        }
    });
}

/// [`TabViewer`] implementation that routes each [`Panel`] to its render fn.
struct PanelViewer<'a> {
    app: &'a mut OgreApp,
}

impl TabViewer for PanelViewer<'_> {
    type Tab = Panel;

    fn title(&mut self, tab: &mut Self::Tab) -> WidgetText {
        match tab {
            Panel::Document(id) => self
                .app
                .state
                .tabs
                .iter()
                .find(|t| t.id == *id)
                .map(tab_label)
                .unwrap_or_else(|| "Untitled".to_string())
                .into(),
            Panel::Layers => "Layers".into(),
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Self::Tab) {
        match tab {
            Panel::Document(id) => {
                // egui_dock only renders the active tab's `ui`, so being called
                // here means this document's tab was selected — sync the active
                // document to match a user click on the tab bar.
                if let Some(idx) = self.app.state.tabs.iter().position(|t| t.id == *id) {
                    if idx != self.app.state.active_tab {
                        self.app.state.tool_manager.cancel_active();
                        self.app.state.active_tab = idx;
                        self.app.state.renderer_needs_clear = true;
                        self.app.state.dirty = true;
                    }
                    panels::canvas::render_canvas(ui, self.app);
                }
            }
            Panel::Layers => panels::layers::render_layers(ui, &mut self.app.state),
        }
    }

    fn on_close(&mut self, tab: &mut Self::Tab) -> OnCloseResponse {
        match tab {
            // Close the document (honouring any unsaved-changes prompt). The dock
            // tab is removed by `sync_document_tabs` once the document is gone, so
            // tell egui_dock to leave it for now.
            Panel::Document(id) => {
                self.app.state.close_tab_by_id(*id);
                OnCloseResponse::Ignore
            }
            Panel::Layers => {
                self.app.state.preferences.layers_visible = false;
                OnCloseResponse::Close
            }
        }
    }
}

pub(crate) fn render_brush_settings(ui: &mut egui::Ui, tool: &mut crate::tools::PaintTool) {
    ui.label("Brush settings");

    let settings = tool.settings_mut();
    ui.horizontal(|ui| {
        ui.label("Size");
        ui.add(egui::Slider::new(&mut settings.size, 1.0..=200.0));
    });
    ui.horizontal(|ui| {
        ui.label("Hardness");
        ui.add(egui::Slider::new(&mut settings.hardness, 0.0..=1.0));
    });
    ui.horizontal(|ui| {
        ui.label("Opacity");
        ui.add(egui::Slider::new(&mut settings.opacity, 0.0..=1.0));
    });
    ui.horizontal(|ui| {
        ui.label("Flow");
        ui.add(egui::Slider::new(&mut settings.flow, 0.0..=1.0));
    });
    ui.horizontal(|ui| {
        ui.label("Spacing");
        ui.add(egui::Slider::new(&mut settings.spacing, 0.05..=1.0));
    });

    if tool.mode() != PaintMode::Eraser {
        ui.horizontal(|ui| {
            ui.label("Colour");
            let mut rgb = [tool.color().r, tool.color().g, tool.color().b];
            if ui.color_edit_button_rgb(&mut rgb).changed() {
                *tool.color_mut() = Rgba32F::new(rgb[0], rgb[1], rgb[2], tool.color().a);
            }
        });
    }
}

/// Return the active raster layer, if any.
pub(crate) fn active_raster_layer_id(state: &AppState) -> Option<ogre_core::LayerId> {
    state.current_tab().doc.active.filter(|id| {
        state
            .current_tab()
            .doc
            .layer(*id)
            .map(|l| l.is_raster())
            .unwrap_or(false)
    })
}

/// The rectangle to crop to for the active selection: the selection's bounding
/// box intersected with the canvas. Returns `None` when there is no selection
/// or the intersection is empty.
fn crop_to_selection_target(doc: &ogre_core::Document) -> Option<ogre_core::Rect> {
    let bounds = doc.selection.bounds()?;
    let inter = doc.canvas.intersect(bounds)?;
    if inter.is_empty() {
        None
    } else {
        Some(inter)
    }
}

/// The rectangle to trim to: the bounding box of all non-transparent pixels in
/// the *visible composited* document (so masks, groups, adjustment layers, and
/// vector layers all count correctly). Returns `None` when the composite is
/// fully transparent (nothing to trim to).
fn trim_target(doc: &ogre_core::Document) -> Option<ogre_core::Rect> {
    let canvas = doc.canvas;
    let pixels = ogre_core::composite_document(doc, canvas).ok()?;
    let w = canvas.w as usize;
    let mut min_x = i64::MAX;
    let mut min_y = i64::MAX;
    let mut max_x = i64::MIN;
    let mut max_y = i64::MIN;
    for (i, p) in pixels.iter().enumerate() {
        if p.a > 0.0 {
            let x = (i % w) as i64 + canvas.x as i64;
            let y = (i / w) as i64 + canvas.y as i64;
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
    }
    if max_x < min_x {
        return None; // no content
    }
    // from_corners gives [min, max] inclusive → Rect with w/h = span.
    let r = ogre_core::Rect::from_corners(
        ogre_core::IVec2::new(min_x as i32, min_y as i32),
        ogre_core::IVec2::new(max_x as i32, max_y as i32),
    );
    // from_corners is inclusive on both ends; Rect is half-open (w = right-left).
    // from_corners already computes w = max-min+1, so this is correct.
    Some(r)
}

/// Shared filter/adjustment menu entries.
///
/// Parameter-less entries commit immediately; parameterized entries open a
/// dialog.  `adjustment` selects between destructive application and adding an
/// adjustment layer.
/// The six tonal/colour operations shared by **Image → Adjustments**
/// (destructive, `adjustment = false`) and **Layer → New Adjustment Layer**
/// (non-destructive, `adjustment = true`).
fn tonal_menu_items(ui: &mut egui::Ui, state: &mut AppState, has_raster: bool, adjustment: bool) {
    // Adjustment layers transform the backdrop and do not need an active raster
    // layer; destructive application always requires one.
    let busy = state.is_busy();
    let enabled = !busy && (if adjustment { true } else { has_raster });

    let mut action = None;
    if ui
        .add_enabled(enabled, egui::Button::new("Invert"))
        .clicked()
    {
        action = Some(FilterAction {
            filter: Box::new(ogre_gpu::InvertFilter::new()),
            adjustment_kind: ogre_core::AdjustmentKind::Invert,
            label: "Invert",
        });
        ui.close_kind(egui::UiKind::Menu);
    }
    if ui
        .add_enabled(enabled, egui::Button::new("Desaturate"))
        .clicked()
    {
        action = Some(FilterAction {
            filter: Box::new(ogre_gpu::DesaturateFilter::new()),
            adjustment_kind: ogre_core::AdjustmentKind::Desaturate,
            label: "Desaturate",
        });
        ui.close_kind(egui::UiKind::Menu);
    }
    if ui
        .add_enabled(enabled, egui::Button::new("Brightness / Contrast"))
        .clicked()
    {
        state.filter_dialog = Some(FilterDialog::with_defaults(FilterKind::BrightnessContrast));
        state.filter_dialog_adjustment = adjustment;
        ui.close_kind(egui::UiKind::Menu);
    }
    if ui
        .add_enabled(enabled, egui::Button::new("Levels"))
        .clicked()
    {
        state.filter_dialog = Some(FilterDialog::with_defaults(FilterKind::Levels));
        state.filter_dialog_adjustment = adjustment;
        ui.close_kind(egui::UiKind::Menu);
    }
    if ui
        .add_enabled(enabled, egui::Button::new("Hue / Saturation"))
        .clicked()
    {
        state.filter_dialog = Some(FilterDialog::with_defaults(FilterKind::HueSat));
        state.filter_dialog_adjustment = adjustment;
        ui.close_kind(egui::UiKind::Menu);
    }
    if ui
        .add_enabled(enabled, egui::Button::new("Curves"))
        .clicked()
    {
        state.filter_dialog = Some(FilterDialog::with_defaults(FilterKind::Curves));
        state.filter_dialog_adjustment = adjustment;
        ui.close_kind(egui::UiKind::Menu);
    }
    if ui
        .add_enabled(enabled, egui::Button::new("Posterize"))
        .clicked()
    {
        state.filter_dialog = Some(FilterDialog::with_defaults(FilterKind::Posterize));
        state.filter_dialog_adjustment = adjustment;
        ui.close_kind(egui::UiKind::Menu);
    }
    if ui
        .add_enabled(enabled, egui::Button::new("Threshold"))
        .clicked()
    {
        state.filter_dialog = Some(FilterDialog::with_defaults(FilterKind::Threshold));
        state.filter_dialog_adjustment = adjustment;
        ui.close_kind(egui::UiKind::Menu);
    }
    if ui
        .add_enabled(enabled, egui::Button::new("Gradient Map"))
        .clicked()
    {
        state.filter_dialog = Some(FilterDialog::with_defaults(FilterKind::GradientMap));
        state.filter_dialog_adjustment = adjustment;
        ui.close_kind(egui::UiKind::Menu);
    }
    if let Some(a) = action {
        apply_filter_action(state, a, adjustment);
    }
}

/// Spatial, destructive-only filters under the top-level **Filters** menu. The
/// compositor's adjustment-layer model does not yet support blur/sharpen on the
/// backdrop, so these have no non-destructive variant.
fn spatial_filter_menu_items(ui: &mut egui::Ui, state: &mut AppState, has_raster: bool) {
    if ui
        .add_enabled(has_raster, egui::Button::new("Gaussian Blur"))
        .clicked()
    {
        state.filter_dialog = Some(FilterDialog::with_defaults(FilterKind::GaussianBlur));
        state.filter_dialog_adjustment = false;
        ui.close_kind(egui::UiKind::Menu);
    }
    if ui
        .add_enabled(has_raster, egui::Button::new("Sharpen"))
        .clicked()
    {
        state.filter_dialog = Some(FilterDialog::with_defaults(FilterKind::Sharpen));
        state.filter_dialog_adjustment = false;
        ui.close_kind(egui::UiKind::Menu);
    }
    if ui
        .add_enabled(has_raster, egui::Button::new("Emboss"))
        .clicked()
    {
        state.filter_dialog = Some(FilterDialog::with_defaults(FilterKind::Emboss));
        state.filter_dialog_adjustment = false;
        ui.close_kind(egui::UiKind::Menu);
    }
    if ui
        .add_enabled(has_raster, egui::Button::new("Edge Detect"))
        .clicked()
    {
        state.filter_dialog = Some(FilterDialog::with_defaults(FilterKind::EdgeDetect));
        state.filter_dialog_adjustment = false;
        ui.close_kind(egui::UiKind::Menu);
    }
}

struct FilterAction {
    filter: Box<dyn ogre_gpu::Filter>,
    adjustment_kind: ogre_core::AdjustmentKind,
    label: &'static str,
}

/// **Layer → Layer Mask** submenu: the full mask lifecycle (add / apply / delete).
///
/// Mask *editing* (painting on the mask) is deferred; this exposes the
/// undoable create/apply/delete path that the data model already supported but
/// had no command or UI for.
fn layer_mask_menu_items(ui: &mut egui::Ui, state: &mut AppState) {
    let busy = state.is_busy();
    let layer_id = active_raster_layer_id(state);
    let selection = state.current_tab().doc.selection.clone();
    let has_mask = layer_id
        .and_then(|id| state.current_tab().doc.layer(id).ok())
        .is_some_and(|l| l.mask().is_some());
    let has_selection = !selection.is_empty();
    let can_add = !busy && layer_id.is_some() && !has_mask;
    let can_use_selection = can_add && has_selection;
    let can_modify = !busy && has_mask;

    let mut cmd: Option<Box<dyn ogre_core::Command>> = None;
    if ui
        .add_enabled(can_add, egui::Button::new("Reveal All"))
        .clicked()
    {
        cmd = Some(Box::new(ogre_core::AddLayerMaskCmd::new(
            layer_id.unwrap(),
            ogre_core::MaskInit::RevealAll,
        )));
        ui.close_kind(egui::UiKind::Menu);
    }
    if ui
        .add_enabled(can_add, egui::Button::new("Hide All"))
        .clicked()
    {
        cmd = Some(Box::new(ogre_core::AddLayerMaskCmd::new(
            layer_id.unwrap(),
            ogre_core::MaskInit::HideAll,
        )));
        ui.close_kind(egui::UiKind::Menu);
    }
    if ui
        .add_enabled(can_use_selection, egui::Button::new("Reveal Selection"))
        .on_disabled_hover_text("Make a selection first")
        .clicked()
    {
        cmd = Some(Box::new(ogre_core::AddLayerMaskCmd::new(
            layer_id.unwrap(),
            ogre_core::MaskInit::RevealSelection(selection),
        )));
        ui.close_kind(egui::UiKind::Menu);
    }
    ui.separator();
    if ui
        .add_enabled(can_modify, egui::Button::new("Apply Mask"))
        .on_disabled_hover_text("The active layer has no mask")
        .clicked()
    {
        cmd = Some(Box::new(ogre_core::ApplyLayerMaskCmd::new(
            layer_id.unwrap(),
        )));
        ui.close_kind(egui::UiKind::Menu);
    }
    if ui
        .add_enabled(can_modify, egui::Button::new("Delete Mask"))
        .on_disabled_hover_text("The active layer has no mask")
        .clicked()
    {
        cmd = Some(Box::new(ogre_core::DeleteLayerMaskCmd::new(
            layer_id.unwrap(),
        )));
        ui.close_kind(egui::UiKind::Menu);
    }

    if let Some(c) = cmd {
        dispatch::dispatch_or_report(state, c);
    }
}

fn apply_filter_action(state: &mut AppState, action: FilterAction, adjustment: bool) {
    let FilterAction {
        filter,
        adjustment_kind,
        label,
    } = action;
    if adjustment {
        dispatch::dispatch_or_report(
            state,
            Box::new(ogre_core::AddAdjustmentLayerCmd::new(
                label,
                adjustment_kind,
            )),
        );
    } else if let Some(id) = active_raster_layer_id(state) {
        dispatch::dispatch_or_report(state, Box::new(ogre_gpu::ApplyFilterCmd::new(id, filter)));
    }
}

/// Largest sigma accepted by the blur/sharpen preview and commit paths.
/// Larger values produce impractical 2D convolution radii and would hang the
/// live preview on every keystroke.
const MAX_FILTER_SIGMA: f32 = 20.0;

/// Delay between the user changing a filter parameter and the live preview
/// recomputing. This prevents every keystroke or slider micro-drag from
/// issuing a synchronous GPU compute pass.
const FILTER_PREVIEW_DEBOUNCE: Duration = Duration::from_millis(150);

/// Recompute the filter preview if the debounce window has elapsed.
///
/// Called once per frame from [`OgreApp::ui`](crate::OgreApp). The preview is
/// only computed after the user has stopped editing parameters for the
/// debounce duration (currently 150 ms).
pub fn update_filter_preview_debounced(ui: &mut egui::Ui, app: &mut crate::OgreApp) {
    let Some(dialog) = app.state.filter_dialog.as_ref() else {
        app.filter_preview_changed_at = None;
        return;
    };

    let Some(changed_at) = app.filter_preview_changed_at else {
        return;
    };

    if changed_at.elapsed() < FILTER_PREVIEW_DEBOUNCE {
        // Still inside the debounce window; request another frame so we can
        // compute as soon as the delay elapses.
        ui.ctx()
            .request_repaint_after(FILTER_PREVIEW_DEBOUNCE - changed_at.elapsed());
        return;
    }

    app.filter_preview_changed_at = None;

    let Some((filter, _)) = parse_filter_dialog(dialog) else {
        app.filter_preview_texture = None;
        return;
    };

    update_filter_preview(ui, app, &*filter);
}

/// Render the open filter/adjustment dialog with live preview.
pub fn render_filter_dialog(ui: &mut egui::Ui, app: &mut crate::OgreApp) {
    if app.state.filter_dialog.is_none() {
        app.filter_preview_texture = None;
        app.state.filter_dialog_invalid = false;
        app.filter_preview_changed_at = None;
        return;
    }

    // Seed the debounce timer when the dialog first appears so the initial
    // default parameters are previewed after the debounce delay.
    if app.filter_preview_changed_at.is_none() {
        app.filter_preview_changed_at = Some(std::time::Instant::now());
    }

    let mut close = false;
    let mut command: Option<Box<dyn ogre_core::Command>> = None;
    let mut changed = false;
    let mut parsed: Option<(Box<dyn ogre_gpu::Filter>, ogre_core::AdjustmentKind)> = None;
    let adjustment = app.state.filter_dialog_adjustment;
    let palette = crate::theme::resolve(app.state.preferences.theme, ui.ctx());

    let title = app.state.filter_dialog.as_ref().unwrap().title();
    egui::Window::new(title)
        .collapsible(false)
        .resizable(false)
        .title_bar(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ui.ctx(), |ui| {
            modal_heading(ui, title);
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                close = true;
            }
            parsed =
                render_filter_params(ui, app.state.filter_dialog.as_mut().unwrap(), &mut changed);

            if app.state.filter_dialog_invalid {
                ui.colored_label(ui.visuals().error_fg_color, "Invalid value");
            }

            modal_actions(ui, |ui| {
                let busy = app.state.is_busy();
                if ui
                    .add_enabled(!busy, accent_button("OK", palette.accent))
                    .clicked()
                {
                    if let Some((filter, adjustment_kind)) = parsed.take() {
                        command =
                            build_filter_command(&app.state, filter, adjustment_kind, adjustment);
                        app.state.filter_dialog_invalid = command.is_none();
                        close = command.is_some();
                    } else {
                        app.state.filter_dialog_invalid = true;
                    }
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });

    if let Some(cmd) = command {
        let _ = dispatch::dispatch(&mut app.state, cmd);
    }
    if close {
        app.state.filter_dialog = None;
        app.state.filter_dialog_invalid = false;
        app.filter_preview_texture = None;
        app.filter_preview_changed_at = None;
        return;
    }

    // Clear the invalid flag as soon as the user edits a value again and
    // mark the preview dirty so it recomputes after the debounce delay.
    if changed {
        app.state.filter_dialog_invalid = false;
        app.filter_preview_changed_at = Some(std::time::Instant::now());
    }

    // The live preview is computed from OgreApp::ui after FILTER_PREVIEW_DEBOUNCE
    // has elapsed; do not issue a synchronous GPU compute pass here.

    // Show the preview texture if we have one.
    if let Some(handle) = app.filter_preview_texture.as_ref() {
        egui::Window::new("Preview")
            .collapsible(false)
            .resizable(false)
            .show(ui.ctx(), |ui| {
                ui.image(handle);
            });
    }
}

/// Parse the current values of `dialog` into a filter and matching adjustment
/// kind without rendering any widgets.
///
/// This is used by the debounced live-preview path so the preview can be
/// recomputed outside the immediate dialog render closure.
fn parse_filter_dialog(
    dialog: &FilterDialog,
) -> Option<(Box<dyn ogre_gpu::Filter>, ogre_core::AdjustmentKind)> {
    match dialog {
        FilterDialog::BrightnessContrast {
            brightness,
            contrast,
        } => {
            let b = brightness.parse::<f32>().ok()?;
            let c = contrast.parse::<f32>().ok()?;
            Some((
                Box::new(ogre_gpu::BrightnessContrastFilter::new(b, c)),
                ogre_core::AdjustmentKind::BrightnessContrast {
                    brightness: b,
                    contrast: c,
                },
            ))
        }
        FilterDialog::Levels {
            input_black,
            input_white,
            output_black,
            output_white,
            gamma,
        } => {
            let ib = input_black.parse::<f32>().ok()?;
            let iw = input_white.parse::<f32>().ok()?;
            let ob = output_black.parse::<f32>().ok()?;
            let ow = output_white.parse::<f32>().ok()?;
            let g = gamma.parse::<f32>().ok()?;
            Some((
                Box::new(ogre_gpu::LevelsFilter::new(ib, iw, ob, ow, g)),
                ogre_core::AdjustmentKind::Levels {
                    input_black: ib,
                    input_white: iw,
                    output_black: ob,
                    output_white: ow,
                    gamma: g,
                },
            ))
        }
        FilterDialog::HueSat {
            hue,
            saturation,
            lightness,
        } => {
            let h = hue.parse::<f32>().ok()?;
            let s = saturation.parse::<f32>().ok()?;
            let l = lightness.parse::<f32>().ok()?;
            Some((
                Box::new(ogre_gpu::HueSaturationFilter::new(h, s, l)),
                ogre_core::AdjustmentKind::HueSat {
                    hue: h,
                    saturation: s,
                    lightness: l,
                },
            ))
        }
        FilterDialog::GaussianBlur { radius } => {
            let r = radius.parse::<f32>().ok()?;
            if !(0.0..=MAX_FILTER_SIGMA).contains(&r) {
                return None;
            }
            Some((
                Box::new(ogre_gpu::GaussianBlurFilter::new(r)),
                ogre_core::AdjustmentKind::Invert,
            ))
        }
        FilterDialog::Sharpen { amount, radius } => {
            let a = amount.parse::<f32>().ok()?;
            let r = radius.parse::<f32>().ok()?;
            if a < 0.0 || !(0.0..=MAX_FILTER_SIGMA).contains(&r) {
                return None;
            }
            Some((
                Box::new(ogre_gpu::SharpenFilter::new(r, a)),
                ogre_core::AdjustmentKind::Invert,
            ))
        }
        FilterDialog::Emboss { amount } => {
            let a = amount.parse::<f32>().ok()?;
            if !(0.0..=1.0).contains(&a) {
                return None;
            }
            Some((
                Box::new(ogre_gpu::EmbossFilter::new(a)),
                ogre_core::AdjustmentKind::Invert,
            ))
        }
        FilterDialog::EdgeDetect { amount } => {
            let a = amount.parse::<f32>().ok()?;
            if !(0.0..=1.0).contains(&a) {
                return None;
            }
            Some((
                Box::new(ogre_gpu::EdgeDetectFilter::new(a)),
                ogre_core::AdjustmentKind::Invert,
            ))
        }
        FilterDialog::Curves { points } => {
            let mut parsed = [(0.0f32, 0.0f32); 4];
            for (i, (input, output)) in points.iter().enumerate() {
                let inp = input.parse::<f32>().ok()?;
                let out = output.parse::<f32>().ok()?;
                parsed[i] = (inp, out);
            }
            Some((
                Box::new(ogre_gpu::CurvesFilter::new(parsed)),
                ogre_core::AdjustmentKind::curves(parsed),
            ))
        }
        FilterDialog::Posterize { levels } => {
            let n = levels.parse::<u32>().ok()?;
            if n < 2 {
                return None;
            }
            Some((
                Box::new(ogre_gpu::PosterizeFilter::new(n)),
                ogre_core::AdjustmentKind::Posterize { levels: n },
            ))
        }
        FilterDialog::Threshold { level } => {
            let l = level.parse::<f32>().ok()?;
            if !(0.0..=1.0).contains(&l) {
                return None;
            }
            Some((
                Box::new(ogre_gpu::ThresholdFilter::new(l)),
                ogre_core::AdjustmentKind::Threshold { level: l },
            ))
        }
        FilterDialog::GradientMap { fg, bg } => {
            let fg_c = ogre_core::Rgba32F::new(fg[0], fg[1], fg[2], fg[3]);
            let bg_c = ogre_core::Rgba32F::new(bg[0], bg[1], bg[2], bg[3]);
            Some((
                Box::new(ogre_gpu::GradientMapFilter::new(fg_c, bg_c)),
                ogre_core::AdjustmentKind::GradientMap { fg: fg_c, bg: bg_c },
            ))
        }
    }
}

/// Render parameter widgets for `dialog`.  Returns a filter + its matching
/// adjustment kind if the current values parse, and records whether any widget
/// changed in `changed`.
fn render_filter_params(
    ui: &mut egui::Ui,
    dialog: &mut FilterDialog,
    changed: &mut bool,
) -> Option<(Box<dyn ogre_gpu::Filter>, ogre_core::AdjustmentKind)> {
    match dialog {
        FilterDialog::BrightnessContrast {
            brightness,
            contrast,
        } => {
            ui.horizontal(|ui| {
                ui.label("Brightness:");
                *changed |= ui.text_edit_singleline(brightness).changed();
            });
            ui.horizontal(|ui| {
                ui.label("Contrast:");
                *changed |= ui.text_edit_singleline(contrast).changed();
            });
        }
        FilterDialog::Levels {
            input_black,
            input_white,
            output_black,
            output_white,
            gamma,
        } => {
            let mut field = |label: &str, value: &mut String| {
                ui.horizontal(|ui| {
                    ui.label(label);
                    *changed |= ui.text_edit_singleline(value).changed();
                });
            };
            field("Input Black:", input_black);
            field("Input White:", input_white);
            field("Output Black:", output_black);
            field("Output White:", output_white);
            field("Gamma:", gamma);
        }
        FilterDialog::HueSat {
            hue,
            saturation,
            lightness,
        } => {
            ui.horizontal(|ui| {
                ui.label("Hue:");
                *changed |= ui.text_edit_singleline(hue).changed();
            });
            ui.horizontal(|ui| {
                ui.label("Saturation:");
                *changed |= ui.text_edit_singleline(saturation).changed();
            });
            ui.horizontal(|ui| {
                ui.label("Lightness:");
                *changed |= ui.text_edit_singleline(lightness).changed();
            });
        }
        FilterDialog::GaussianBlur { radius } => {
            ui.horizontal(|ui| {
                ui.label("Sigma:");
                *changed |= ui.text_edit_singleline(radius).changed();
            });
        }
        FilterDialog::Sharpen { amount, radius } => {
            ui.horizontal(|ui| {
                ui.label("Amount:");
                *changed |= ui.text_edit_singleline(amount).changed();
            });
            ui.horizontal(|ui| {
                ui.label("Sigma:");
                *changed |= ui.text_edit_singleline(radius).changed();
            });
        }
        FilterDialog::Emboss { amount } => {
            ui.horizontal(|ui| {
                ui.label("Amount:");
                *changed |= ui.text_edit_singleline(amount).changed();
            });
        }
        FilterDialog::EdgeDetect { amount } => {
            ui.horizontal(|ui| {
                ui.label("Amount:");
                *changed |= ui.text_edit_singleline(amount).changed();
            });
        }
        FilterDialog::Curves { points } => {
            for (i, (input, output)) in points.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(format!("Point {} input:", i + 1));
                    *changed |= ui.text_edit_singleline(input).changed();
                    ui.label("output:");
                    *changed |= ui.text_edit_singleline(output).changed();
                });
            }
        }
        FilterDialog::Posterize { levels } => {
            ui.horizontal(|ui| {
                ui.label("Levels:");
                *changed |= ui.text_edit_singleline(levels).changed();
            });
        }
        FilterDialog::Threshold { level } => {
            ui.horizontal(|ui| {
                ui.label("Level:");
                *changed |= ui.text_edit_singleline(level).changed();
            });
        }
        FilterDialog::GradientMap { fg, bg } => {
            ui.horizontal(|ui| {
                ui.label("Shadows:");
                *changed |= ui.color_edit_button_rgba_unmultiplied(fg).changed();
            });
            ui.horizontal(|ui| {
                ui.label("Highlights:");
                *changed |= ui.color_edit_button_rgba_unmultiplied(bg).changed();
            });
        }
    }
    parse_filter_dialog(dialog)
}

fn build_filter_command(
    state: &AppState,
    filter: Box<dyn ogre_gpu::Filter>,
    adjustment_kind: ogre_core::AdjustmentKind,
    adjustment: bool,
) -> Option<Box<dyn ogre_core::Command>> {
    if adjustment {
        return Some(Box::new(ogre_core::AddAdjustmentLayerCmd::new(
            filter.label(),
            adjustment_kind,
        )));
    }
    active_raster_layer_id(state).map(|id| {
        Box::new(ogre_gpu::ApplyFilterCmd::new(id, filter)) as Box<dyn ogre_core::Command>
    })
}

fn update_filter_preview(
    ui: &mut egui::Ui,
    app: &mut crate::OgreApp,
    filter: &dyn ogre_gpu::Filter,
) {
    let Some(renderer) = app.renderer.as_ref() else {
        return;
    };
    let Some(layer_id) = active_raster_layer_id(&app.state) else {
        return;
    };
    let Ok(layer) = app.state.current_tab().doc.layer(layer_id) else {
        return;
    };
    let Some(buffer) = layer.buffer() else {
        return;
    };

    if app.filter_preview.is_none() {
        app.filter_preview = Some(ogre_gpu::FilterPreview::with_context(
            renderer.context().clone(),
        ));
    }
    let preview = app.filter_preview.as_ref().unwrap();
    let (pixels, width, height) = preview.compute(filter, buffer, 256);
    if width == 0 || height == 0 {
        app.filter_preview_texture = None;
        return;
    }

    let image = pixels_to_color_image(&pixels, width, height);
    let handle = ui
        .ctx()
        .load_texture("filter_preview", image, egui::TextureOptions::default());
    app.filter_preview_texture = Some(handle);
}

fn pixels_to_color_image(
    pixels: &[ogre_core::Rgba32F],
    width: u32,
    height: u32,
) -> egui::ColorImage {
    let mut rgba: Vec<u8> = Vec::with_capacity(pixels.len() * 4);
    for p in pixels {
        rgba.push(linear_to_srgb_u8(p.r));
        rgba.push(linear_to_srgb_u8(p.g));
        rgba.push(linear_to_srgb_u8(p.b));
        rgba.push((p.a.clamp(0.0, 1.0) * 255.0).round() as u8);
    }
    egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], &rgba)
}

fn linear_to_srgb_u8(v: f32) -> u8 {
    let v = v.clamp(0.0, 1.0);
    let s = if v <= 0.0031308 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    };
    (s * 255.0).round() as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collect every panel currently stored in the dock state.
    fn collect_panels(dock: &DockState<Panel>) -> Vec<Panel> {
        dock.iter_all_tabs().map(|(_path, tab)| *tab).collect()
    }

    #[test]
    fn default_dock_state_contains_canvas_and_layers() {
        let dock = default_dock_state(true, 0);
        let panels = collect_panels(&dock);
        assert!(panels.contains(&Panel::Document(0)));
        assert!(panels.contains(&Panel::Layers));
    }

    #[test]
    fn default_dock_state_can_hide_layers() {
        let dock = default_dock_state(false, 0);
        let panels = collect_panels(&dock);
        assert!(panels.contains(&Panel::Document(0)));
        assert!(!panels.contains(&Panel::Layers));
    }

    /// A modal with little content must size to its content, not stretch to the
    /// window. Regression guard: the earlier `with_layout(right_to_left)` action
    /// row claimed the full remaining height, ballooning the About modal.
    #[test]
    fn modal_actions_keeps_auto_sized_window_compact() {
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(2000.0, 2000.0),
            )),
            ..Default::default()
        };
        let mut height = 0.0_f32;
        // Auto-size settles over a couple of frames.
        for _ in 0..4 {
            ctx.begin_pass(raw.clone());
            let resp = egui::Window::new("modal-size-test")
                .title_bar(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(&ctx, |ui| {
                    modal_heading(ui, "About");
                    ui.label("A short line of body text.");
                    modal_actions(ui, |ui| {
                        let _ = ui.button("Close");
                    });
                });
            if let Some(resp) = resp {
                height = resp.response.rect.height();
            }
            let _ = ctx.end_pass();
        }
        // Heading + one line + a button row is well under 160px; the buggy
        // layout stretched it toward the full 2000px screen height.
        assert!(
            height > 0.0 && height < 160.0,
            "modal stretched: {height}px"
        );
    }

    #[test]
    fn file_dialog_rows_keep_auto_sized_window_compact() {
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(2000.0, 2000.0),
            )),
            ..Default::default()
        };
        let mut height = 0.0_f32;
        for _ in 0..4 {
            ctx.begin_pass(raw.clone());
            let mut path = String::new();
            let mut format = "PNG".to_string();
            let mut bit_depth = "8".to_string();
            let resp = egui::Window::new("file-dialog-size-test")
                .title_bar(false)
                .resizable(false)
                .auto_sized()
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(&ctx, |ui| {
                    ui.set_width(FILE_DIALOG_MODAL_WIDTH);
                    modal_heading(ui, "Export");
                    file_dialog_path_row(ui, "Path:", &mut path, "/path/to/image.png", |_| {});
                    file_dialog_combo_row(
                        ui,
                        "Format:",
                        "test_export_format",
                        &mut format,
                        &["PNG", "JPEG", "TIFF"],
                    );
                    file_dialog_combo_row(
                        ui,
                        "Bit depth:",
                        "test_export_bit_depth",
                        &mut bit_depth,
                        &["8", "16"],
                    );
                    modal_actions(ui, |ui| {
                        let _ = ui.button("OK");
                        let _ = ui.button("Cancel");
                    });
                });
            if let Some(resp) = resp {
                height = resp.response.rect.height();
            }
            let _ = ctx.end_pass();
        }
        assert!(
            height > 0.0 && height < 220.0,
            "file dialog stretched: {height}px"
        );
    }

    #[test]
    fn file_dialog_rows_align_fields_after_fixed_label_column() {
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(2000.0, 2000.0),
            )),
            ..Default::default()
        };
        let mut left_edges = Vec::new();
        for _ in 0..4 {
            ctx.begin_pass(raw.clone());
            let mut path = String::new();
            let mut format = "PNG".to_string();
            let mut bit_depth = "8".to_string();
            let resp = egui::Window::new("file-dialog-align-test")
                .title_bar(false)
                .resizable(false)
                .auto_sized()
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(&ctx, |ui| {
                    ui.set_width(FILE_DIALOG_MODAL_WIDTH);
                    modal_heading(ui, "Export Slices");
                    let path_rect =
                        file_dialog_path_row(ui, "Path:", &mut path, "/path/to/slices", |_| {});
                    let format_rect = file_dialog_combo_row(
                        ui,
                        "Format:",
                        "test_slice_export_format",
                        &mut format,
                        &["PNG", "JPEG", "TIFF"],
                    );
                    let bit_depth_rect = file_dialog_combo_row(
                        ui,
                        "Bit depth:",
                        "test_slice_export_bit_depth",
                        &mut bit_depth,
                        &["8", "16"],
                    );
                    (path_rect.min.x, format_rect.min.x, bit_depth_rect.min.x)
                });
            if let Some(resp) = resp {
                if let Some((path_left, format_left, bit_depth_left)) = resp.inner {
                    left_edges = vec![path_left, format_left, bit_depth_left];
                }
            }
            let _ = ctx.end_pass();
        }

        assert_eq!(left_edges.len(), 3);
        let baseline = left_edges[0];
        for edge in left_edges {
            assert!(
                (edge - baseline).abs() < 0.5,
                "file dialog field left edges differ: {baseline}px vs {edge}px"
            );
        }
    }

    #[test]
    fn file_dialog_label_cells_share_left_edge() {
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(2000.0, 2000.0),
            )),
            ..Default::default()
        };
        let mut left_edges = Vec::new();
        for _ in 0..4 {
            ctx.begin_pass(raw.clone());
            let resp = egui::Window::new("file-dialog-label-align-test")
                .title_bar(false)
                .resizable(false)
                .auto_sized()
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(&ctx, |ui| {
                    ui.set_width(FILE_DIALOG_MODAL_WIDTH);
                    modal_heading(ui, "Export");
                    let row_h = ui.spacing().interact_size.y.max(30.0);
                    let path_left = ui
                        .horizontal(|ui| {
                            let label = file_dialog_label_cell(ui, "Path:", row_h);
                            ui.allocate_space(egui::vec2(80.0, row_h));
                            label.min.x
                        })
                        .inner;
                    let format_left = ui
                        .horizontal(|ui| {
                            let label = file_dialog_label_cell(ui, "Format:", row_h);
                            ui.allocate_space(egui::vec2(80.0, row_h));
                            label.min.x
                        })
                        .inner;
                    let bit_depth_left = ui
                        .horizontal(|ui| {
                            let label = file_dialog_label_cell(ui, "Bit depth:", row_h);
                            ui.allocate_space(egui::vec2(80.0, row_h));
                            label.min.x
                        })
                        .inner;
                    (path_left, format_left, bit_depth_left)
                });
            if let Some(resp) = resp {
                if let Some((path_left, format_left, bit_depth_left)) = resp.inner {
                    left_edges = vec![path_left, format_left, bit_depth_left];
                }
            }
            let _ = ctx.end_pass();
        }

        assert_eq!(left_edges.len(), 3);
        let baseline = left_edges[0];
        for edge in left_edges {
            assert!(
                (edge - baseline).abs() < 0.5,
                "file dialog label left edges differ: {baseline}px vs {edge}px"
            );
        }
    }

    /// Collect leaf panels in left-to-right order.
    fn leaf_panels_left_to_right(dock: &DockState<Panel>) -> Vec<Panel> {
        let tree = dock.main_surface();
        let mut out = Vec::new();

        fn walk(tree: &egui_dock::Tree<Panel>, node: NodeIndex, out: &mut Vec<Panel>) {
            match &tree[node] {
                egui_dock::Node::Leaf(leaf) => {
                    out.extend(leaf.tabs.iter().copied());
                }
                egui_dock::Node::Horizontal(_) | egui_dock::Node::Vertical(_) => {
                    walk(tree, node.left(), out);
                    walk(tree, node.right(), out);
                }
                egui_dock::Node::Empty => {}
            }
        }

        walk(tree, NodeIndex::root(), &mut out);
        out
    }

    #[test]
    fn default_dock_state_is_canvas_then_layers_left_to_right() {
        assert_eq!(
            leaf_panels_left_to_right(&default_dock_state(true, 0)),
            vec![Panel::Document(0), Panel::Layers]
        );
    }

    #[test]
    fn layers_toggle_hides_and_shows_layers_panel() {
        let mut state = AppState::new_document(1920, 1080);
        state.new_blank_document((100, 100));
        state.preferences.layers_visible = false;
        ensure_layers_tab(&mut state, false);
        assert!(!state
            .dock
            .iter_all_tabs()
            .any(|(_, p)| matches!(p, Panel::Layers)));
        state.preferences.layers_visible = true;
        ensure_layers_tab(&mut state, true);
        assert!(state
            .dock
            .iter_all_tabs()
            .any(|(_, p)| matches!(p, Panel::Layers)));
    }

    #[test]
    fn sync_document_tabs_tracks_open_and_closed_documents() {
        let mut state = AppState::new_document(64, 64);
        // Open a second document. (Pushed directly because `new_blank_document`
        // would reuse the blank first tab; real opens push like this.)
        let id = state.next_tab_id;
        state.next_tab_id += 1;
        state.tabs.push(DocumentTab::new_blank(id, (64, 64)));
        state.active_tab = 1;
        let ids: Vec<u64> = state.tabs.iter().map(|t| t.id).collect();
        assert_eq!(ids.len(), 2);

        sync_document_tabs(&mut state);
        let doc_tabs: Vec<u64> = state
            .dock
            .iter_all_tabs()
            .filter_map(|(_, p)| match p {
                Panel::Document(id) => Some(*id),
                _ => None,
            })
            .collect();
        assert!(ids.iter().all(|id| doc_tabs.contains(id)));
        assert_eq!(doc_tabs.len(), 2, "one dock tab per document");

        // Close one document and re-sync: its dock tab is dropped.
        state.close_tab(0);
        sync_document_tabs(&mut state);
        let remaining: Vec<u64> = state
            .dock
            .iter_all_tabs()
            .filter_map(|(_, p)| match p {
                Panel::Document(id) => Some(*id),
                _ => None,
            })
            .collect();
        assert_eq!(remaining, vec![state.tabs[0].id]);
    }

    #[test]
    fn handle_shortcut_undo_redo() {
        let mut state = AppState::new_document(100, 100);
        let id = state.current_tab().doc.active.unwrap();
        dispatch::dispatch(
            &mut state,
            Box::new(ogre_core::MoveLayerByCmd::new(
                id,
                ogre_core::IVec2::new(4, 5),
            )),
        )
        .unwrap();

        assert!(handle_shortcut(&mut state, Shortcut::Undo));
        assert_eq!(
            state.current_tab().doc.layer(id).unwrap().offset,
            ogre_core::IVec2::ZERO
        );
        assert!(handle_shortcut(&mut state, Shortcut::Redo));
        assert_eq!(
            state.current_tab().doc.layer(id).unwrap().offset,
            ogre_core::IVec2::new(4, 5)
        );
    }

    #[test]
    fn handle_shortcut_deselect_and_select_all() {
        let mut state = AppState::new_document(100, 100);
        let canvas = state.current_tab().doc.canvas;

        assert!(handle_shortcut(&mut state, Shortcut::SelectAll));
        assert_eq!(
            state.current_tab().doc.selection,
            ogre_core::Selection::select_all(canvas)
        );

        assert!(handle_shortcut(&mut state, Shortcut::Deselect));
        assert!(state.current_tab().doc.selection.is_empty());
    }

    #[test]
    fn handle_shortcut_new_and_duplicate_layer() {
        let mut state = AppState::new_document(64, 64);
        let initial = state.current_tab().doc.order.len();
        // New Raster Layer adds one layer and activates it.
        assert!(handle_shortcut(&mut state, Shortcut::NewRasterLayer));
        assert_eq!(state.current_tab().doc.order.len(), initial + 1);
        let new_id = state.current_tab().doc.active;
        assert!(new_id.is_some(), "new layer becomes active");

        // Duplicate Layer duplicates the active one.
        assert!(handle_shortcut(&mut state, Shortcut::DuplicateLayer));
        assert_eq!(state.current_tab().doc.order.len(), initial + 2);
    }

    #[test]
    fn handle_shortcut_duplicate_without_active_layer_is_noop() {
        let mut state = AppState::new_document(64, 64);
        state.current_tab_mut().doc.active = None;
        let initial = state.current_tab().doc.order.len();
        assert!(!handle_shortcut(&mut state, Shortcut::DuplicateLayer));
        assert_eq!(state.current_tab().doc.order.len(), initial);
    }

    #[test]
    fn crop_to_selection_target_intersects_canvas() {
        let mut state = AppState::new_document(100, 100);
        // Selection partly outside the canvas: bounds (90, 90, 40, 40) →
        // intersected with canvas (0,0,100,100) → (90, 90, 10, 10).
        state.current_tab_mut().doc.selection =
            ogre_core::Selection::rect(ogre_core::Rect::new(90, 90, 40, 40));
        let target = crop_to_selection_target(&state.current_tab().doc).unwrap();
        assert_eq!(target, ogre_core::Rect::new(90, 90, 10, 10));
    }

    #[test]
    fn crop_to_selection_target_none_without_selection() {
        let state = AppState::new_document(100, 100);
        assert!(crop_to_selection_target(&state.current_tab().doc).is_none());
    }

    #[test]
    fn crop_to_selection_target_none_when_selection_outside_canvas() {
        let mut state = AppState::new_document(50, 50);
        // Selection entirely outside the canvas.
        state.current_tab_mut().doc.selection =
            ogre_core::Selection::rect(ogre_core::Rect::new(200, 200, 10, 10));
        assert!(crop_to_selection_target(&state.current_tab().doc).is_none());
    }

    #[test]
    fn trim_target_finds_content_bbox() {
        let mut state = AppState::new_document(100, 100);
        // Paint a small opaque block at (30, 40)..(34, 44) on the sole layer.
        let id = state.current_tab().doc.active.unwrap();
        let buf = state
            .current_tab_mut()
            .doc
            .layer_mut(id)
            .unwrap()
            .buffer_mut()
            .unwrap();
        for y in 40..44 {
            for x in 30..34 {
                buf.set_pixel(
                    ogre_core::IVec2::new(x, y),
                    ogre_core::Rgba32F::new(1.0, 0.0, 0.0, 1.0),
                );
            }
        }
        let target = trim_target(&state.current_tab().doc).unwrap();
        assert_eq!(target, ogre_core::Rect::new(30, 40, 4, 4));
    }

    #[test]
    fn trim_target_none_when_transparent() {
        let state = AppState::new_document(32, 32);
        assert!(trim_target(&state.current_tab().doc).is_none());
    }

    #[test]
    fn handle_shortcut_zoom_in_out_and_reset() {
        let mut state = AppState::new_document(100, 100);
        assert_eq!(state.current_tab().viewport.zoom, 1.0);

        assert!(handle_shortcut(&mut state, Shortcut::ZoomIn));
        assert!(state.current_tab().viewport.zoom > 1.0);

        assert!(handle_shortcut(&mut state, Shortcut::ZoomOut));
        // One in + one out returns to ~1.0.
        assert!((state.current_tab().viewport.zoom - 1.0).abs() < 1e-4);

        handle_shortcut(&mut state, Shortcut::ZoomIn);
        assert!(handle_shortcut(&mut state, Shortcut::Zoom100));
        assert_eq!(state.current_tab().viewport.zoom, 1.0);
    }

    #[test]
    fn status_info_default_state() {
        let state = AppState::new_document(100, 200);
        let info = status_info(&state);
        assert_eq!(info.zoom, "100%");
        assert_eq!(info.cursor, "—");
        assert_eq!(info.canvas, "100×200");
        assert_eq!(info.layer, "Background");
        assert!(info.selection.is_empty());
    }

    #[test]
    fn fit_window_inner_size_clamps_tiny_canvas_to_minimum() {
        let screen = egui::vec2(900.0, 700.0);
        let canvas_rect =
            egui::Rect::from_min_size(egui::pos2(100.0, 50.0), egui::vec2(600.0, 500.0));
        let image_logical = egui::vec2(16.0, 16.0);

        let size = fit_window_inner_size(screen, canvas_rect, image_logical);

        assert_eq!(size, egui::vec2(600.0, 600.0));
    }

    #[test]
    fn fit_window_inner_size_keeps_large_canvas_exact() {
        let screen = egui::vec2(900.0, 700.0);
        let canvas_rect =
            egui::Rect::from_min_size(egui::pos2(100.0, 50.0), egui::vec2(600.0, 500.0));
        let image_logical = egui::vec2(800.0, 500.0);

        let size = fit_window_inner_size(screen, canvas_rect, image_logical);

        assert_eq!(size, egui::vec2(1100.0, 700.0));
    }

    #[test]
    fn status_info_zoom_and_cursor() {
        let mut state = AppState::new_document(640, 480);
        state.current_tab_mut().viewport.zoom = 2.5;
        state.cursor_doc_pos = Some(ogre_core::IVec2::new(-3, 42));
        let info = status_info(&state);
        assert_eq!(info.zoom, "250%");
        assert_eq!(info.cursor, "-3, 42");
        assert_eq!(info.canvas, "640×480");
    }

    #[test]
    fn status_info_no_active_layer() {
        let mut state = AppState::new_document(32, 32);
        state.current_tab_mut().doc.active = None;
        let info = status_info(&state);
        assert_eq!(info.layer, "—");
    }

    #[test]
    fn menu_bar_spacing_scales_height_and_gaps_1_5x() {
        let mut s = egui::style::Spacing::default();
        let base_y = s.interact_size.y;
        let base_x = s.item_spacing.x;
        super::apply_menu_bar_spacing(&mut s);
        assert!(
            (s.interact_size.y - base_y * 1.5).abs() < 1e-3,
            "height not 1.5x"
        );
        assert!(
            (s.item_spacing.x - base_x * 1.5).abs() < 1e-3,
            "gap not 1.5x"
        );
    }

    #[test]
    fn pick_active_menu_opens_closed_menu_on_click() {
        let clicked = vec![false, true, false];
        let hovered = vec![false, true, false];
        let was_open = vec![false, false, false];
        assert_eq!(
            super::pick_active_menu(&clicked, &hovered, &was_open),
            Some(1)
        );
    }

    #[test]
    fn pick_active_menu_closes_open_menu_on_reclick() {
        let clicked = vec![false, true, false];
        let hovered = vec![false, true, false];
        let was_open = vec![false, true, false];
        assert_eq!(super::pick_active_menu(&clicked, &hovered, &was_open), None);
    }

    #[test]
    fn pick_active_menu_switches_to_hovered_menu_while_any_open() {
        // Help (index 2) is open; pointer hovers File (index 0).
        let clicked = vec![false, false, false];
        let hovered = vec![true, false, false];
        let was_open = vec![false, false, true];
        assert_eq!(
            super::pick_active_menu(&clicked, &hovered, &was_open),
            Some(0)
        );
    }

    #[test]
    fn pick_active_menu_keeps_open_menu_when_hovering_dropdown() {
        let clicked = vec![false, false, false];
        let hovered = vec![false, false, false];
        let was_open = vec![false, false, true];
        assert_eq!(
            super::pick_active_menu(&clicked, &hovered, &was_open),
            Some(2)
        );
    }

    #[test]
    fn pick_active_menu_ignores_hover_when_bar_is_inactive() {
        let clicked = vec![false, false, false];
        let hovered = vec![true, false, false];
        let was_open = vec![false, false, false];
        assert_eq!(super::pick_active_menu(&clicked, &hovered, &was_open), None);
    }

    #[test]
    fn open_canvas_resize_dialog_uses_current_size() {
        let mut st = crate::state::AppState::new_document(640, 480);
        super::open_canvas_resize_dialog(&mut st);
        match st.canvas_size_dialog {
            Some(crate::state::CanvasSizeDialog {
                width,
                height,
                anchor,
            }) => {
                assert_eq!(width, 640);
                assert_eq!(height, 480);
                assert_eq!(anchor, ogre_core::CanvasAnchor::Center);
            }
            _ => panic!("expected a Resize dialog"),
        }
    }

    #[test]
    fn load_document_from_path_uses_finite_undo_limit() {
        let tmp_path = std::env::temp_dir().join(format!(
            "arte-ogre-load-undo-test-{}.ogre",
            std::process::id()
        ));
        let mut doc = ogre_core::Document::new(64, 64);
        doc.add_raster_layer("Layer 1");
        ogre_io::ogre::save(&doc, &tmp_path).unwrap();

        let mut state = AppState::new_document(32, 32);
        super::load_document_from_path_with_options(
            &mut state,
            tmp_path.to_str().unwrap(),
            crate::io_worker::SvgImportOptions::default(),
        );
        let result = loop {
            if let Some(r) = state.io_worker.poll_result() {
                break r;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        state.apply_io_result(result);

        assert_eq!(
            state.current_tab().history.limit(),
            crate::state::DEFAULT_UNDO_LIMIT
        );
        assert_ne!(
            state.current_tab().history.limit(),
            0,
            "0 means unlimited undo"
        );

        let _ = std::fs::remove_file(&tmp_path);
    }

    #[test]
    fn handle_dropped_paths_adds_as_layer_via_worker() {
        let mut state = AppState::new_document(64, 64);
        state.welcome = false;
        let path = std::path::PathBuf::from("/tmp/arte-ogre-drop-test.png");
        super::handle_dropped_paths(&mut state, std::slice::from_ref(&path));
        assert!(state.io_busy);
        assert!(
            state
                .io_expected
                .contains_key(&crate::io_worker::IoKind::AddAsLayer),
            "expected an AddAsLayer request to be queued"
        );
    }

    #[test]
    fn background_removal_times_out() {
        let mut state = AppState::new_document(64, 64);
        let (_tx, rx) = std::sync::mpsc::channel();
        state.bg_removal_rx = Some(rx);
        state.bg_removal_layer = Some(state.current_tab().doc.order[0]);
        state.bg_removal_started = Some(
            std::time::Instant::now()
                - super::BG_REMOVAL_TIMEOUT
                - std::time::Duration::from_secs(1),
        );
        super::poll_background_removal(&mut state);
        assert!(state.bg_removal_rx.is_none());
        assert!(state.bg_removal_layer.is_none());
        assert!(state.bg_removal_started.is_none());
        assert!(state.io_status_feedback.contains("timed out"));
    }

    #[test]
    fn save_as_ogre_path_queues_save() {
        let mut state = AppState::new_document(16, 16);
        super::commit_save_as(&mut state, "/tmp/test.ogre");
        assert!(
            state
                .io_expected
                .contains_key(&crate::io_worker::IoKind::Save),
            "Save As to .ogre must queue a native save"
        );
        assert!(
            !state
                .io_expected
                .contains_key(&crate::io_worker::IoKind::Export),
            "Save As to .ogre must not queue an export"
        );
        assert!(state.io_busy);
    }

    #[test]
    fn save_as_png_path_queues_export() {
        let mut state = AppState::new_document(16, 16);
        super::commit_save_as(&mut state, "/tmp/test.png");
        assert!(
            state
                .io_expected
                .contains_key(&crate::io_worker::IoKind::Export),
            "Save As to .png must queue an export"
        );
        assert!(
            !state
                .io_expected
                .contains_key(&crate::io_worker::IoKind::Save),
            "Save As to .png must not queue a native save"
        );
        assert!(state.io_busy);
    }

    #[test]
    fn save_as_no_extension_queues_save() {
        let mut state = AppState::new_document(16, 16);
        super::commit_save_as(&mut state, "/tmp/test");
        assert!(
            state
                .io_expected
                .contains_key(&crate::io_worker::IoKind::Save),
            "Save As without extension must queue a native save"
        );
        assert!(
            !state
                .io_expected
                .contains_key(&crate::io_worker::IoKind::Export),
            "Save As without extension must not queue an export"
        );
    }

    #[test]
    fn save_as_png_writes_valid_png_file() {
        let mut state = AppState::new_document(16, 16);
        let id = state.current_tab().doc.active.unwrap();
        // Paint an opaque pixel so the export is non-empty.
        state
            .current_tab_mut()
            .doc
            .layer_mut(id)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(
                ogre_core::IVec2::new(4, 4),
                ogre_core::Rgba32F::new(1.0, 0.0, 0.0, 1.0),
            );

        let path = std::env::temp_dir().join(format!(
            "arte-ogre-save-as-png-test-{}.png",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        super::commit_save_as(&mut state, path.to_str().unwrap());

        // Wait for the worker to finish.
        let kind = *state.io_expected.keys().next().unwrap();
        let result = loop {
            if let Some(r) = state.io_worker.poll_result() {
                break r;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        state.apply_io_result(result);

        assert_eq!(kind, crate::io_worker::IoKind::Export);
        let decoded = ogre_io::import_image(&path).expect("Save As .png must write a valid PNG");
        assert_eq!(decoded.canvas.w, 16);
        assert_eq!(decoded.canvas.h, 16);
        let _ = std::fs::remove_file(&path);
    }
}
