// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Immediate-mode UI shell for Arte Ogre.
//!
//! `ogre-ui` provides the `eframe::App` implementation and the dockable panel
//! layout used by the `ogre` binary. It is intentionally thin: all document
//! edits are routed through `ogre-core` commands pushed onto a `History` stack.
//!
//! # Crate layout
//!
//! - `shell.rs` — dock layout, menu bar, and panel routing.
//! - `state.rs` — application state (`Document`, `History`, viewport).
//! - `dispatch.rs` — the single undoable command dispatch path.
//! - `panels/canvas.rs` — pannable/zoomable GPU canvas.
//! - `panels/layers.rs` — layer list and per-layer controls.
//! - `tools/mod.rs` / `rect_select.rs` — tool system.
//! - `context_menu.rs` — right-click copy/cut to new layer.
//! - `keymap.rs` — configurable keyboard shortcuts.
//! - `prefs.rs` — preferences, autosave, and crash recovery.

#![warn(missing_docs)]

pub mod command_palette;
pub mod context_menu;
pub mod dispatch;
pub mod grid;
pub mod help;
pub mod icons;
pub mod io_worker;
pub mod keymap;
pub mod open_flow;
pub mod panels;
pub mod paste;
pub mod plugin_worker;
pub mod prefs;
pub mod shell;
pub mod state;
pub mod theme;
pub mod tools;
pub mod views;
pub mod welcome;

pub use shell::{default_dock_state, Panel, Shortcut};

use std::sync::Arc;
use std::time::Instant;

use crate::state::AppState;

/// The Arte Ogre `eframe` application.
///
/// `OgreApp` owns [`AppState`] and delegates all document edits through the
/// command dispatch path. The dock layout lives inside [`AppState`] so the
/// shell remains a thin view layer.
///
/// GPU resources are kept outside [`AppState`] because the
/// [`ogre_gpu::CanvasRenderer`] is not guaranteed to be `Send`.
pub struct OgreApp {
    /// Full editor state: document, history, viewport, tool, and dock layout.
    pub state: AppState,
    /// Lazily constructed GPU canvas renderer, shared with eframe's wgpu device.
    pub renderer: Option<ogre_gpu::CanvasRenderer>,
    /// Handle to eframe's egui-wgpu renderer, used to register the canvas texture.
    pub egui_renderer: Option<Arc<egui::epaint::mutex::RwLock<egui_wgpu::Renderer>>>,
    /// Stable egui texture id for the canvas output.
    pub canvas_texture: Option<egui::TextureId>,
    /// Last canvas size that was rendered, used to detect view resize.
    pub last_canvas_size: Option<glam::UVec2>,
    /// Last viewport applied to the GPU renderer; used to detect zoom/pan changes
    /// from the status-bar magnifier and propagate them without waiting for a canvas
    /// interaction event.
    pub last_viewport: ogre_gpu::Viewport,
    /// Lazily constructed filter preview renderer.
    pub filter_preview: Option<ogre_gpu::FilterPreview>,
    /// Cached texture handle for the live filter preview, if a dialog is open.
    pub filter_preview_texture: Option<egui::TextureHandle>,
    /// Time of the most recent filter parameter change. Used to debounce the
    /// live preview so rapid edits (slider drags, keystrokes) do not queue a
    /// GPU compute pass for every frame.
    pub filter_preview_changed_at: Option<Instant>,
}

impl std::fmt::Debug for OgreApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OgreApp")
            .field("state", &self.state)
            .field("renderer_present", &self.renderer.is_some())
            .field("egui_renderer_present", &self.egui_renderer.is_some())
            .field("canvas_texture", &self.canvas_texture)
            .field("last_canvas_size", &self.last_canvas_size)
            .field("last_viewport", &self.last_viewport)
            .field("filter_preview_present", &self.filter_preview.is_some())
            .field(
                "filter_preview_texture_present",
                &self.filter_preview_texture.is_some(),
            )
            .field("filter_preview_changed_at", &self.filter_preview_changed_at)
            .finish()
    }
}

impl OgreApp {
    /// Create a new app with a default 1920×1080 document.
    pub fn new() -> Self {
        let mut state = AppState::new_document(1920, 1080);
        // Offer to restore work from a session that ended unexpectedly. Recovery
        // files only survive a crash — a clean exit clears them in `on_exit` —
        // so their presence here means the previous session did not exit cleanly.
        state.recovery_prompt = state.autosave.load_latest_recovery();
        let last_viewport = *state.viewport();
        Self {
            state,
            renderer: None,
            egui_renderer: None,
            canvas_texture: None,
            last_canvas_size: None,
            last_viewport,
            filter_preview: None,
            filter_preview_texture: None,
            filter_preview_changed_at: None,
        }
    }
}

impl Default for OgreApp {
    fn default() -> Self {
        Self::new()
    }
}

impl eframe::App for OgreApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        egui_extras::install_image_loaders(ui.ctx());

        // Drain completed background I/O results and apply them to state.
        if let Some(result) = self.state.io_worker.poll_result() {
            self.state.apply_io_result(result);
        }

        // Drain completed background plugin results and apply them to state.
        if let Some(result) = self.state.plugin_worker.poll_result() {
            self.state.apply_plugin_result(result);
        }

        // Reset busy state and surface feedback if a worker thread died.
        self.state.check_worker_health();

        // The document may have been replaced by I/O/plugin results or recovery.
        // Drop the renderer's cached textures so GPU memory from the previous
        // document is not retained.
        if self.state.renderer_needs_clear {
            if let Some(renderer) = &mut self.renderer {
                renderer.clear_caches();
                self.state.dirty = true;
            }
            self.state.renderer_needs_clear = false;
        }

        // Apply a finished background-removal job, if any.
        shell::poll_background_removal(&mut self.state);

        // Files dragged onto the window: open, or add as layers.
        let dropped: Vec<std::path::PathBuf> = ui.ctx().input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if !dropped.is_empty() {
            shell::handle_dropped_paths(&mut self.state, &dropped);
        }

        // Intercept a window-close request (OS button or File → Quit) when the
        // open document has unsaved changes: cancel the close and ask first.
        let close_requested = ui.ctx().input(|i| i.viewport().close_requested());
        if close_requested
            && !self.state.confirmed_quit
            && !self.state.welcome
            && self.state.any_unsaved()
        {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.state.quit_confirm = true;
        }

        let palette = crate::theme::resolve(self.state.preferences.theme, ui.ctx());
        crate::theme::visuals::apply_style(ui.ctx(), &palette);

        panels::canvas::ensure_renderer(self, frame);

        egui::Panel::top("ogre_menu_bar").show(ui, |ui| {
            shell::menu_bar(ui, &mut self.state, frame);
        });

        // The status bar (zoom, cursor, canvas size, version) only applies while
        // editing — hide it on the welcome screen for a cleaner landing window,
        // and in Bird's Eye View which is a full-page non-editor surface.
        if !self.state.welcome && self.state.view != crate::state::AppView::BirdsEye {
            egui::Panel::bottom("ogre_status_bar").show(ui, |ui| {
                shell::status_bar(ui, &mut self.state);
            });
        }

        crate::help::about_modal(ui.ctx(), &mut self.state);
        crate::help::updates_modal(ui.ctx(), &mut self.state);
        crate::welcome::render_recovery_prompt(ui.ctx(), &mut self.state);
        shell::render_quit_prompt(ui, &mut self.state);
        shell::render_remove_bg_dialog(ui, &mut self.state);
        shell::render_bg_removal_spinner(ui, &self.state);
        shell::render_color_range_dialog(ui.ctx(), &mut self.state);
        shell::render_svg_import_dialog(ui.ctx(), &mut self.state);

        // Computed outside the input closure (which borrows the input state).
        let text_focused = ui.ctx().memory(|m| m.focused().is_some());
        let dialog_open = self.state.file_dialog.is_some()
            || self.state.svg_import_dialog.is_some()
            || self.state.filter_dialog.is_some()
            || self.state.selection_dialog.is_some()
            || self.state.canvas_size_dialog.is_some()
            || self.state.remove_bg_dialog.is_some()
            || self.state.recovery_prompt.is_some()
            || self.state.command_palette.open;
        let shortcut = ui.ctx().input(|i| {
            for (chord, action) in &self.state.keymap.effective() {
                // `matches_exact` resolves the platform command/ctrl alias while
                // keeping Shift/Alt exact; a raw `==` here silently breaks every
                // shortcut on Windows/Linux (where egui sets command == ctrl).
                if i.key_pressed(chord.key) && i.modifiers.matches_exact(chord.modifiers) {
                    return Some(*action);
                }
            }
            // Escape cancels any in-progress tool interaction.
            if i.key_pressed(egui::Key::Escape) {
                return Some(shell::Shortcut::CancelActiveTool);
            }
            // Enter commits the active tool — but only when not typing in a
            // dialog/text field and with no modifiers held, so it cannot fire a
            // stray transform commit behind a modal or shadow a chord binding.
            if i.key_pressed(egui::Key::Enter)
                && !dialog_open
                && !text_focused
                && i.modifiers.is_none()
            {
                return Some(shell::Shortcut::CommitActiveTool);
            }
            None
        });
        if let Some(action) = shortcut {
            if action == shell::Shortcut::CancelActiveTool
                && self.state.view == crate::state::AppView::BirdsEye
            {
                // Bare Escape leaves Bird's Eye View before it can cancel an
                // active editor-tool interaction.
                self.state.view = crate::state::AppView::Editor;
            } else if action == shell::Shortcut::CommitActiveTool {
                shell::commit_active_tool(&mut self.state);
            } else if action == shell::Shortcut::Quit {
                // Close-request interceptor (below) checks for unsaved changes.
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            } else if action.requires_no_text_focus() {
                // Actions whose triggering key (bare letter, Delete, F2,
                // Backspace) is also a text-edit key must NOT fire while a text
                // field or modal has focus. The keymap loop above runs before
                // the text/dialog gating, so enforce it here.
                if !text_focused && !dialog_open {
                    shell::handle_shortcut(&mut self.state, action);
                }
            } else {
                shell::handle_shortcut(&mut self.state, action);
            }
        }

        // Bare-key tool activation + family cycling (§2.1). Non-configurable;
        // tool keys are a fixed Photoshop-style convention. Suppressed while
        // typing or while a modal dialog is open, and on OS key auto-repeat.
        if !text_focused && !dialog_open {
            let tool_key = ui.ctx().input(|i| {
                for ev in i.events.iter() {
                    if let egui::Event::Key {
                        key,
                        pressed: true,
                        repeat: false,
                        modifiers,
                        ..
                    } = ev
                    {
                        if !modifiers.ctrl && !modifiers.command && !modifiers.alt {
                            if let Some(family) = crate::tools::ToolFamily::for_key(*key) {
                                return Some((family, !modifiers.shift));
                            }
                        }
                    }
                }
                None
            });
            if let Some((family, forward)) = tool_key {
                shell::activate_tool_family(&mut self.state, family, forward);
            }
        }

        // Brush-size adjust: `[` shrinks, `]` grows the active paint tool's
        // diameter. Bare-key (no modifiers), suppressed while typing or while a
        // modal is open, and on OS key auto-repeat — matching the tool-key path.
        if !text_focused && !dialog_open {
            let size_dir = ui.ctx().input(|i| {
                for ev in i.events.iter() {
                    if let egui::Event::Key {
                        key,
                        pressed: true,
                        repeat: false,
                        modifiers,
                        ..
                    } = ev
                    {
                        // Size is strictly no-modifier: Shift is reserved for
                        // hardness (below), so it must NOT also resize.
                        if modifiers.is_none() {
                            match key {
                                egui::Key::OpenBracket => return Some(-1.0f32),
                                egui::Key::CloseBracket => return Some(1.0),
                                _ => {}
                            }
                        }
                    }
                }
                None
            });
            if let Some(dir) = size_dir {
                if let Some(tool) = self.state.tool_manager.active_paint_settings_mut() {
                    let s = tool.settings_mut();
                    // 10 % step, with a 1 px floor so small brushes still move.
                    let step = (s.size * 0.1).max(1.0);
                    s.size = (s.size + dir * step).clamp(1.0, 5000.0);
                }
            }

            // Brush hardness: Shift+[ / Shift+] (Shift only — no Ctrl/Cmd/Alt).
            let hardness_dir = ui.ctx().input(|i| {
                for ev in i.events.iter() {
                    if let egui::Event::Key {
                        key,
                        pressed: true,
                        repeat: false,
                        modifiers,
                        ..
                    } = ev
                    {
                        if modifiers.shift
                            && !modifiers.ctrl
                            && !modifiers.command
                            && !modifiers.alt
                        {
                            match key {
                                egui::Key::OpenBracket => return Some(-1.0f32),
                                egui::Key::CloseBracket => return Some(1.0),
                                _ => {}
                            }
                        }
                    }
                }
                None
            });
            if let Some(dir) = hardness_dir {
                if let Some(tool) = self.state.tool_manager.active_paint_settings_mut() {
                    let h = tool.settings_mut();
                    // 0.1 step, clamped to [0, 1].
                    h.hardness = (h.hardness + dir * 0.1).clamp(0.0, 1.0);
                }
            }

            // Brush opacity: digits `1`..`9` = 10 %..90 %, `0` = 100 %. Same
            // bare-key gates as brush size (no modifiers, no text/dialog focus,
            // no key auto-repeat). The paint engine snapshots settings at
            // stroke start, so changing the live value mid-stroke is safe.
            let opacity = ui.ctx().input(|i| {
                for ev in i.events.iter() {
                    if let egui::Event::Key {
                        key,
                        pressed: true,
                        repeat: false,
                        modifiers,
                        ..
                    } = ev
                    {
                        if !modifiers.ctrl && !modifiers.command && !modifiers.alt {
                            let digit = match key {
                                egui::Key::Num0 => Some(0),
                                egui::Key::Num1 => Some(1),
                                egui::Key::Num2 => Some(2),
                                egui::Key::Num3 => Some(3),
                                egui::Key::Num4 => Some(4),
                                egui::Key::Num5 => Some(5),
                                egui::Key::Num6 => Some(6),
                                egui::Key::Num7 => Some(7),
                                egui::Key::Num8 => Some(8),
                                egui::Key::Num9 => Some(9),
                                _ => None,
                            };
                            if digit.is_some() {
                                return digit;
                            }
                        }
                    }
                }
                None
            });
            if let Some(d) = opacity {
                if let Some(tool) = self.state.tool_manager.active_paint_settings_mut() {
                    tool.settings_mut().opacity = if d == 0 { 1.0 } else { d as f32 / 10.0 };
                }
            }
        }

        // Tab toggles UI chrome (tools sidebar + layers dock) for a clean
        // canvas view. Suppressed while typing or in a modal, and on
        // key-repeat. The key is consumed so egui's focus traversal does not
        // also receive it. The hidden state is a transient flag on AppState —
        // it is not persisted and never touches `Preferences`/serde.
        if !text_focused && !dialog_open && self.state.view == crate::state::AppView::Editor {
            let tab_pressed = ui.ctx().input(|i| {
                i.events.iter().any(|ev| {
                    matches!(
                            ev,
                            egui::Event::Key {
                                key: egui::Key::Tab,
                                pressed: true,
                                repeat: false,
                                modifiers,
                                ..
                            } if !modifiers.ctrl && !modifiers.command && !modifiers.alt
                    )
                })
            });
            if tab_pressed {
                ui.ctx()
                    .input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Tab));
                self.state.ui_chrome_hidden = !self.state.ui_chrome_hidden;
                // Sync the layers dock tab: hidden when chrome is off, else
                // restored from the persisted preference.
                let show_layers =
                    !self.state.ui_chrome_hidden && self.state.preferences.layers_visible;
                shell::ensure_layers_tab(&mut self.state, show_layers);
            }
        }

        if (self.state.command_palette.open || !text_focused)
            && ui
                .ctx()
                .input(|i| i.modifiers.command && i.modifiers.shift && i.key_pressed(egui::Key::P))
        {
            self.state.command_palette.open = !self.state.command_palette.open;
            if self.state.command_palette.open {
                self.state.command_palette.just_opened = true;
            }
            self.state.command_palette.query.clear();
        }

        // Ctrl+V / Cmd+V: paste a clipboard image as a new layer.
        // egui-winit treats Ctrl+V as a paste *command*: it swallows the V key
        // *press* (emitting at most a text `egui::Event::Paste`), so an
        // image-only clipboard produces no press event at all. It does still
        // deliver the V key *release* with the modifiers intact, so trigger on
        // that — it fires once for image-only and text clipboards alike.
        // Detect Ctrl+V robustly. egui-winit treats Ctrl+V as a paste command
        // and *swallows the V key press*, emitting only the V key *release* —
        // whose modifiers are unreliable (if Ctrl and V release in the same
        // frame, the release reports Ctrl already up). So instead of trusting
        // the release modifiers, use the swallow itself as the signal: a V
        // release with no preceding V press means the press was swallowed —
        // i.e. it was Ctrl+V. A plain `V` press is delivered normally and is
        // therefore ignored here.
        let (v_pressed, v_released) = ui.ctx().input(|i| {
            let mut pressed = false;
            let mut released = false;
            for event in &i.events {
                if let egui::Event::Key {
                    key: egui::Key::V,
                    pressed: is_press,
                    ..
                } = event
                {
                    if *is_press {
                        pressed = true;
                    } else {
                        released = true;
                    }
                }
            }
            (pressed, released)
        });
        if v_pressed {
            self.state.paste_v_press_seen = true;
        }
        let paste_command = if v_released {
            let press_was_swallowed = !self.state.paste_v_press_seen;
            self.state.paste_v_press_seen = false;
            press_was_swallowed
        } else {
            false
        };
        // Block paste only when a TEXT FIELD is focused (so Ctrl+V there edits
        // the text), NOT when any focusable widget — e.g. the canvas after a
        // click — merely holds focus.
        let paste_triggered = paste_command
            && !ui.ctx().text_edit_focused()
            && !dialog_open
            && self.state.paste_prompt.is_none();
        if paste_triggered {
            crate::paste::try_paste(&mut self.state);
        }

        // Ctrl+C / Ctrl+X: copy / cut the current selection to the OS clipboard.
        // egui-winit turns Ctrl+C/Ctrl+X into Copy/Cut *events*, so detect those
        // rather than the raw key chord. Ignore them while a text field is focused
        // (so they edit text there) or a modal dialog is open.
        let (copy_event, cut_event) = ui.ctx().input(|i| {
            let mut copy = false;
            let mut cut = false;
            for event in &i.events {
                match event {
                    egui::Event::Copy => copy = true,
                    egui::Event::Cut => cut = true,
                    _ => {}
                }
            }
            (copy, cut)
        });
        let clipboard_allowed =
            !ui.ctx().text_edit_focused() && !dialog_open && !self.state.welcome;
        if clipboard_allowed && cut_event {
            crate::paste::cut_selection_to_clipboard(&mut self.state);
        } else if clipboard_allowed && copy_event {
            crate::paste::copy_selection_to_clipboard(&mut self.state);
        }

        // Render the paste resize-prompt modal when a paste is pending.
        crate::paste::render_paste_prompt(ui.ctx(), &mut self.state);

        if self.state.view == crate::state::AppView::Editor {
            if self.state.welcome {
                egui::CentralPanel::default().show(ui, |ui| {
                    crate::welcome::render(ui, &mut self.state);
                });
            } else {
                if !self.state.ui_chrome_hidden {
                    egui::Panel::left("ogre_tools")
                        .resizable(false)
                        .exact_size(if self.state.preferences.tools_collapsed {
                            52.0
                        } else {
                            230.0
                        })
                        .show(ui, |ui| {
                            crate::panels::tools_sidebar::render(ui, &mut self.state);
                        });
                }
                egui::CentralPanel::default().show(ui, |ui| {
                    shell::show_dock_area(ui, self);
                });
            }
            // Run after the dock so `canvas_screen_rect` is current this frame.
            shell::step_fit_window_to_canvas(ui.ctx(), &mut self.state);
        } else {
            egui::CentralPanel::default().show(ui, |ui| {
                crate::views::render(ui, &mut self.state);
            });
        }

        shell::render_filter_dialog(ui, self);
        shell::update_filter_preview_debounced(ui, self);

        let doc = &self.state.tabs[self.state.active_tab].doc;
        self.state.autosave.maybe_save(doc, self.state.dirty);

        crate::command_palette::show_overlay(ui.ctx(), &mut self.state);
    }

    fn on_exit(&mut self) {
        // ponytail: best-effort cleanup so only a crash leaves recovery files
        // behind. A save queued on the final frame could race the background
        // worker and survive (a harmless one-time recover prompt next launch);
        // upgrade to a clean-shutdown marker if that ever bites.
        self.state.autosave.clear_recovery();
    }
}
