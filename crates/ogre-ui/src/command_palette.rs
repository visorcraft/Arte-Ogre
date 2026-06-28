// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC
//! Ctrl+Shift+P command palette: a fuzzy overlay over the app's menu actions.
use crate::state::{AppState, AppView, FileDialog};

/// An action the palette (and menus) can dispatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandAction {
    /// Create a new blank canvas.
    NewCanvas,
    /// Open an existing file.
    Open,
    /// Save the current file.
    Save,
    /// Save the current file under a new name.
    SaveAs,
    /// Export the current file.
    Export,
    /// Undo the most recent edit.
    Undo,
    /// Redo the most recently undone edit.
    Redo,
    /// Open the Settings view.
    OpenSettings,
    /// Open the Licenses view.
    OpenLicenses,
    /// Open the Credits view.
    OpenCredits,
    /// Switch to the Editor view.
    OpenEditorView,
    /// Switch to the Bird's Eye View workspace.
    OpenBirdsEyeView,
    /// Show the About dialog.
    About,
    /// Trigger a background update check.
    CheckUpdates,
    /// Toggle the Tools sidebar collapsed/expanded.
    ToggleToolsSidebar,
}

/// A palette entry: a label + the action it runs.
#[derive(Clone, Copy)]
pub struct Command {
    /// Human-readable label shown in the palette.
    pub label: &'static str,
    /// The action dispatched when this entry is selected.
    pub action: CommandAction,
}

/// All commands shown in the palette.
pub fn registry() -> Vec<Command> {
    use CommandAction::*;
    vec![
        Command {
            label: "New canvas…",
            action: NewCanvas,
        },
        Command {
            label: "Open…",
            action: Open,
        },
        Command {
            label: "Save",
            action: Save,
        },
        Command {
            label: "Save As…",
            action: SaveAs,
        },
        Command {
            label: "Export…",
            action: Export,
        },
        Command {
            label: "Undo",
            action: Undo,
        },
        Command {
            label: "Redo",
            action: Redo,
        },
        Command {
            label: "Settings…",
            action: OpenSettings,
        },
        Command {
            label: "Licenses…",
            action: OpenLicenses,
        },
        Command {
            label: "Credits…",
            action: OpenCredits,
        },
        Command {
            label: "View: Editor View",
            action: OpenEditorView,
        },
        Command {
            label: "View: Bird's Eye View",
            action: OpenBirdsEyeView,
        },
        Command {
            label: "About Arte Ogre…",
            action: About,
        },
        Command {
            label: "Check for Updates…",
            action: CheckUpdates,
        },
        Command {
            label: "Toggle Tools sidebar",
            action: ToggleToolsSidebar,
        },
    ]
}

/// Case-insensitive subsequence fuzzy filter, preserving registry order.
pub fn fuzzy_filter(cmds: &[Command], query: &str) -> Vec<Command> {
    if query.is_empty() {
        return cmds.to_vec();
    }
    let q: Vec<char> = query.to_lowercase().chars().collect();
    cmds.iter()
        .filter(|c| is_subsequence(&q, &c.label.to_lowercase()))
        .copied()
        .collect()
}

fn is_subsequence(needle: &[char], haystack: &str) -> bool {
    let mut it = haystack.chars();
    needle.iter().all(|&n| it.any(|h| h == n))
}

/// Transient palette UI state.
#[derive(Default, Debug)]
pub struct CommandPalette {
    /// Whether the palette overlay is currently visible.
    pub open: bool,
    /// Set to `true` for one frame after the palette is opened so that the
    /// click-outside-close guard does not fire on the same click that opened it.
    pub just_opened: bool,
    /// Current filter query typed by the user.
    pub query: String,
}

/// Execute a command against the app state. `ctx` is needed for the update check.
pub fn execute_command(state: &mut AppState, ctx: &egui::Context, action: CommandAction) {
    use CommandAction::*;
    match action {
        NewCanvas => {
            let (w, h) = state.preferences.default_canvas;
            state.file_dialog = Some(FileDialog::New {
                width: w.to_string(),
                height: h.to_string(),
            });
        }
        Open => {
            crate::open_flow::show_open_dialog(state);
        }
        SaveAs => {
            state.file_dialog = Some(FileDialog::SaveAs {
                path: String::new(),
            })
        }
        Save => {
            if state.is_busy() {
                state.io_status_feedback =
                    "Busy; please wait for the current operation.".to_string();
                return;
            }
            if let Some(ref path) = state.current_tab().last_save_path {
                crate::shell::request_save(state, path.clone());
            } else {
                state.file_dialog = Some(FileDialog::SaveAs {
                    path: String::new(),
                });
            }
        }
        Export => {
            state.file_dialog = Some(FileDialog::Export {
                path: String::new(),
                format: "PNG".into(),
                quality: "90".into(),
                bit_depth: "8".into(),
            })
        }
        Undo => {
            crate::dispatch::undo(state);
        }
        Redo => {
            crate::dispatch::redo(state);
        }
        OpenSettings => state.view = AppView::Settings,
        OpenLicenses => state.view = AppView::Licenses,
        OpenCredits => state.view = AppView::Credits,
        OpenEditorView => state.view = AppView::Editor,
        OpenBirdsEyeView => {
            // Bird's Eye View needs an open document; ignore it on the welcome
            // screen so the palette can't strand the user on an empty workspace.
            if !state.welcome {
                state.view = AppView::BirdsEye;
            }
        }
        About => state.show_about = true,
        CheckUpdates => crate::help::start_update_check(state, ctx),
        ToggleToolsSidebar => {
            state.preferences.tools_collapsed = !state.preferences.tools_collapsed;
        }
    }
}

/// Draw the palette overlay (centered) when open; runs the selected command on Enter.
pub fn show_overlay(ctx: &egui::Context, state: &mut AppState) {
    if !state.command_palette.open {
        return;
    }
    let mut run: Option<CommandAction> = None;
    let mut close = false;
    // Window::open(&mut bool) holds a mutable borrow for the whole .show() call,
    // so we cannot also mutate `close` (or any outer bool) inside the closure.
    // Instead we use a separate `close` flag and apply it after .show() returns.
    let win = egui::Window::new("Command palette")
        .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 80.0))
        .collapsible(false)
        .resizable(false)
        .title_bar(false)
        .fixed_size(egui::vec2(420.0, 0.0))
        .show(ctx, |ui| {
            let resp = ui.add(
                egui::TextEdit::singleline(&mut state.command_palette.query)
                    .hint_text("Type a command…")
                    .desired_width(f32::INFINITY),
            );
            resp.request_focus();
            let hits = fuzzy_filter(&registry(), &state.command_palette.query);
            if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                if let Some(first) = hits.first() {
                    run = Some(first.action);
                }
            }
            egui::ScrollArea::vertical()
                .max_height(280.0)
                .show(ui, |ui| {
                    for c in &hits {
                        if ui.selectable_label(false, c.label).clicked() {
                            run = Some(c.action);
                        }
                    }
                });
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                close = true;
            }
        });
    let skip_outside = std::mem::take(&mut state.command_palette.just_opened);
    if !skip_outside {
        if let Some(ir) = win {
            if ir.response.clicked_elsewhere() {
                close = true;
            }
        }
    }
    if let Some(action) = run {
        state.command_palette.open = false;
        execute_command(state, ctx, action);
    } else if close {
        state.command_palette.open = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_matches_subsequence_and_ranks() {
        let cmds = registry();
        let hits = fuzzy_filter(&cmds, "opn"); // subsequence of "Open…"
        assert!(hits.iter().any(|c| c.label.starts_with("Open")));
    }

    #[test]
    fn registry_is_non_empty_and_covers_core_actions() {
        let labels: Vec<&str> = registry().iter().map(|c| c.label).collect();
        assert!(labels.contains(&"New canvas…"));
        assert!(labels.contains(&"Open…"));
        assert!(labels.contains(&"Settings…"));
    }

    #[test]
    fn empty_query_returns_all() {
        assert_eq!(fuzzy_filter(&registry(), "").len(), registry().len());
    }

    #[test]
    fn registry_covers_both_view_entries() {
        let labels: Vec<&str> = registry().iter().map(|c| c.label).collect();
        assert!(labels.contains(&"View: Editor View"));
        assert!(labels.contains(&"View: Bird's Eye View"));
    }

    #[test]
    fn birds_eye_view_command_is_noop_on_welcome() {
        let mut state = AppState::new_document(64, 64);
        assert!(state.welcome, "fresh state starts on the welcome screen");
        execute_command(
            &mut state,
            &egui::Context::default(),
            CommandAction::OpenBirdsEyeView,
        );
        assert_eq!(
            state.view,
            AppView::Editor,
            "Bird's Eye is a no-op while on the welcome screen"
        );

        state.welcome = false;
        execute_command(
            &mut state,
            &egui::Context::default(),
            CommandAction::OpenBirdsEyeView,
        );
        assert_eq!(state.view, AppView::BirdsEye);

        execute_command(
            &mut state,
            &egui::Context::default(),
            CommandAction::OpenEditorView,
        );
        assert_eq!(state.view, AppView::Editor);
    }

    #[test]
    fn save_command_routes_through_io_worker() {
        let mut state = AppState::new_document(64, 64);
        state.current_tab_mut().last_save_path =
            Some(std::path::PathBuf::from("/tmp/arte-ogre-palette-save.ogre"));
        execute_command(&mut state, &egui::Context::default(), CommandAction::Save);
        assert!(state.io_busy);
        assert!(state
            .io_expected
            .contains_key(&crate::io_worker::IoKind::Save));
    }
}
