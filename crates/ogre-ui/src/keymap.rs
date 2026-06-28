// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Configurable keyboard shortcuts.
//!
//! A [`Keymap`] maps [`Chord`]s (modifier + key combinations) to [`Shortcut`]
//! actions.  Defaults are baked in; user overrides can be loaded from and saved
//! to a TOML configuration file.  The keymap detects when two actions are bound
//! to the same chord.

use crate::shell::Shortcut;
use ahash::AHashMap;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

/// A single key chord: zero or more modifiers plus a physical key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Chord {
    /// Modifier flags active when the key is pressed.
    pub modifiers: egui::Modifiers,
    /// The primary key.
    pub key: egui::Key,
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let m = self.modifiers;
        let parts: Vec<&str> = [
            (m.command || m.ctrl, "Ctrl"),
            (m.alt, "Alt"),
            (m.shift, "Shift"),
            (m.command && m.ctrl, "Cmd"),
        ]
        .into_iter()
        .filter_map(|(active, name)| if active { Some(name) } else { None })
        .collect();
        if parts.is_empty() {
            write!(f, "{:?}", self.key)
        } else {
            write!(f, "{}+{:?}", parts.join("+"), self.key)
        }
    }
}

/// Serializable representation of a [`Chord`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct ChordSerde {
    #[serde(default)]
    ctrl: bool,
    #[serde(default)]
    shift: bool,
    #[serde(default)]
    alt: bool,
    #[serde(default)]
    command: bool,
    key: String,
}

impl From<Chord> for ChordSerde {
    fn from(c: Chord) -> Self {
        Self {
            ctrl: c.modifiers.ctrl,
            shift: c.modifiers.shift,
            alt: c.modifiers.alt,
            command: c.modifiers.command,
            key: format!("{:?}", c.key),
        }
    }
}

impl TryFrom<ChordSerde> for Chord {
    type Error = String;

    fn try_from(value: ChordSerde) -> Result<Self, Self::Error> {
        let key = parse_key(&value.key)?;
        Ok(Self {
            modifiers: egui::Modifiers {
                ctrl: value.ctrl,
                shift: value.shift,
                alt: value.alt,
                command: value.command,
                ..Default::default()
            },
            key,
        })
    }
}

fn parse_key(name: &str) -> Result<egui::Key, String> {
    use egui::Key;
    // Letters.
    if name.len() == 1 {
        let ch = name.chars().next().unwrap();
        if ch.is_ascii_alphabetic() {
            return Ok(match ch.to_ascii_uppercase() {
                'A' => Key::A,
                'B' => Key::B,
                'C' => Key::C,
                'D' => Key::D,
                'E' => Key::E,
                'F' => Key::F,
                'G' => Key::G,
                'H' => Key::H,
                'I' => Key::I,
                'J' => Key::J,
                'K' => Key::K,
                'L' => Key::L,
                'M' => Key::M,
                'N' => Key::N,
                'O' => Key::O,
                'P' => Key::P,
                'Q' => Key::Q,
                'R' => Key::R,
                'S' => Key::S,
                'T' => Key::T,
                'U' => Key::U,
                'V' => Key::V,
                'W' => Key::W,
                'X' => Key::X,
                'Y' => Key::Y,
                'Z' => Key::Z,
                _ => return Err(format!("unknown key: {name}")),
            });
        }
    }
    // Named keys used by the default bindings.
    match name {
        "Escape" => Ok(Key::Escape),
        "Enter" => Ok(Key::Enter),
        "Space" => Ok(Key::Space),
        "Backspace" => Ok(Key::Backspace),
        "Delete" => Ok(Key::Delete),
        "Tab" => Ok(Key::Tab),
        "=" | "+" | "Equals" => Ok(Key::Equals),
        "-" | "−" | "Minus" => Ok(Key::Minus),
        "[" | "OpenBracket" => Ok(Key::OpenBracket),
        "]" | "CloseBracket" => Ok(Key::CloseBracket),
        "0" | "Num0" => Ok(Key::Num0),
        "F2" => Ok(Key::F2),
        _ => Err(format!("unknown key: {name}")),
    }
}

impl FromStr for Chord {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split('+').map(str::trim).collect();
        if parts.is_empty() {
            return Err("empty chord".to_string());
        }
        let mut ctrl = false;
        let mut shift = false;
        let mut alt = false;
        let mut command = false;
        let mut key_part = None;
        for part in &parts {
            match *part {
                "Ctrl" => ctrl = true,
                "Shift" => shift = true,
                "Alt" => alt = true,
                "Cmd" | "Command" | "Meta" => command = true,
                _ => {
                    if key_part.is_some() {
                        return Err(format!("multiple keys in chord: {s}"));
                    }
                    key_part = Some(*part);
                }
            }
        }
        let key = key_part.ok_or_else(|| format!("missing key in chord: {s}"))?;
        Ok(Self {
            modifiers: egui::Modifiers {
                ctrl,
                shift,
                alt,
                command,
                ..Default::default()
            },
            key: parse_key(key)?,
        })
    }
}

/// A conflict between two actions mapped to the same chord.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyConflict {
    /// The chord shared by both actions.
    pub chord: Chord,
    /// The action already bound to the chord.
    pub existing: Shortcut,
    /// The action trying to take the chord.
    pub incoming: Shortcut,
}

/// Serializable representation of a single binding override.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct BindingSerde {
    chord: ChordSerde,
    action: String,
}

/// Serializable root of a keymap configuration file.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct KeymapFile {
    #[serde(default)]
    binding: Vec<BindingSerde>,
}

/// A configurable mapping from chords to actions.
#[derive(Clone, Debug, Default)]
pub struct Keymap {
    defaults: AHashMap<Chord, Shortcut>,
    bindings: AHashMap<Chord, Shortcut>,
}

impl Keymap {
    /// Create a keymap with the application's default shortcuts.
    pub fn default_shortcuts() -> Self {
        use egui::{Key, Modifiers};
        // `Modifiers::COMMAND` is the platform-aware control key: Cmd on macOS,
        // Ctrl elsewhere.  This matches how egui reports `i.modifiers.command`.
        let command = Modifiers::COMMAND;
        let command_shift = Modifiers::COMMAND | Modifiers::SHIFT;
        let mut defaults = AHashMap::new();
        defaults.insert(
            Chord {
                modifiers: command,
                key: Key::Z,
            },
            Shortcut::Undo,
        );
        defaults.insert(
            Chord {
                modifiers: command_shift,
                key: Key::Z,
            },
            Shortcut::Redo,
        );
        defaults.insert(
            Chord {
                modifiers: command,
                key: Key::Y,
            },
            Shortcut::Redo,
        );
        defaults.insert(
            Chord {
                modifiers: command,
                key: Key::D,
            },
            Shortcut::Deselect,
        );
        defaults.insert(
            Chord {
                modifiers: command,
                key: Key::A,
            },
            Shortcut::SelectAll,
        );
        defaults.insert(
            Chord {
                modifiers: command_shift,
                key: Key::I,
            },
            Shortcut::InvertSelection,
        );
        defaults.insert(
            Chord {
                modifiers: command_shift,
                key: Key::N,
            },
            Shortcut::NewRasterLayer,
        );
        defaults.insert(
            Chord {
                modifiers: command,
                key: Key::J,
            },
            Shortcut::DuplicateLayer,
        );
        defaults.insert(
            Chord {
                modifiers: Modifiers::NONE,
                key: Key::Delete,
            },
            Shortcut::DeleteLayer,
        );
        defaults.insert(
            Chord {
                modifiers: Modifiers::NONE,
                key: Key::F2,
            },
            Shortcut::RenameLayer,
        );
        defaults.insert(
            Chord {
                modifiers: command,
                key: Key::Equals,
            },
            Shortcut::ZoomIn,
        );
        defaults.insert(
            Chord {
                modifiers: command,
                key: Key::Minus,
            },
            Shortcut::ZoomOut,
        );
        defaults.insert(
            Chord {
                modifiers: command,
                key: Key::Num0,
            },
            Shortcut::Zoom100,
        );
        // Standard File operations.
        defaults.insert(
            Chord {
                modifiers: command,
                key: Key::N,
            },
            Shortcut::NewDocument,
        );
        defaults.insert(
            Chord {
                modifiers: command,
                key: Key::O,
            },
            Shortcut::OpenDocument,
        );
        defaults.insert(
            Chord {
                modifiers: command,
                key: Key::S,
            },
            Shortcut::SaveDocument,
        );
        defaults.insert(
            Chord {
                modifiers: command_shift,
                key: Key::S,
            },
            Shortcut::SaveAsDocument,
        );
        defaults.insert(
            Chord {
                modifiers: command,
                key: Key::T,
            },
            Shortcut::FreeTransform,
        );
        defaults.insert(
            Chord {
                modifiers: command,
                key: Key::W,
            },
            Shortcut::CloseDocument,
        );
        defaults.insert(
            Chord {
                modifiers: command,
                key: Key::Q,
            },
            Shortcut::Quit,
        );
        defaults.insert(
            Chord {
                modifiers: command,
                key: Key::Tab,
            },
            Shortcut::NextTab,
        );
        defaults.insert(
            Chord {
                modifiers: command_shift,
                key: Key::Tab,
            },
            Shortcut::PrevTab,
        );
        // Fill shortcuts (Photoshop conventions).
        defaults.insert(
            Chord {
                modifiers: Modifiers::ALT,
                key: Key::Backspace,
            },
            Shortcut::FillForeground,
        );
        defaults.insert(
            Chord {
                modifiers: command,
                key: Key::Backspace,
            },
            Shortcut::FillBackground,
        );
        // Default/swap colors (bare keys — gated against text-field focus in
        // the dispatch loop, like Delete/F2).
        defaults.insert(
            Chord {
                modifiers: Modifiers::NONE,
                key: Key::D,
            },
            Shortcut::DefaultColors,
        );
        defaults.insert(
            Chord {
                modifiers: Modifiers::NONE,
                key: Key::X,
            },
            Shortcut::SwapColors,
        );
        // Layer arrange: Ctrl+] bring forward, Ctrl+[ send backward.
        defaults.insert(
            Chord {
                modifiers: command,
                key: Key::CloseBracket,
            },
            Shortcut::BringForward,
        );
        defaults.insert(
            Chord {
                modifiers: command,
                key: Key::OpenBracket,
            },
            Shortcut::SendBackward,
        );
        // Toggle Bird's Eye View.
        defaults.insert(
            Chord {
                modifiers: command_shift,
                key: Key::B,
            },
            Shortcut::ToggleBirdsEye,
        );
        Self {
            defaults,
            bindings: AHashMap::new(),
        }
    }

    /// Return the action bound to `chord`, considering user overrides first and
    /// then defaults.
    pub fn action(&self, chord: Chord) -> Option<Shortcut> {
        self.bindings
            .get(&chord)
            .copied()
            .or_else(|| self.defaults.get(&chord).copied())
    }

    /// Reverse lookup: return the effective chord bound to `action`, if any.
    /// Used to surface keyboard-shortcut hints next to menu items.
    pub fn chord_for(&self, action: Shortcut) -> Option<Chord> {
        self.effective()
            .into_iter()
            .find(|(_, a)| *a == action)
            .map(|(c, _)| c)
    }

    /// Resolve a pressed `key` plus the currently-active `modifiers` to a bound
    /// action.
    ///
    /// Uses [`egui::Modifiers::matches_exact`] rather than a raw `==`: egui
    /// reports `command == ctrl` on Windows/Linux, so comparing the raw fields
    /// against a `COMMAND` chord would never match a physical Ctrl press and all
    /// shortcuts would silently die off-macOS. `matches_exact` resolves the
    /// command/ctrl alias while still requiring Shift/Alt to match exactly, so
    /// `Ctrl+Z` (Undo) and `Ctrl+Shift+Z` (Redo) remain distinct.
    pub fn resolve(&self, key: egui::Key, modifiers: egui::Modifiers) -> Option<Shortcut> {
        self.effective()
            .into_iter()
            .find(|(chord, _)| chord.key == key && modifiers.matches_exact(chord.modifiers))
            .map(|(_, action)| action)
    }

    /// Bind `chord` to `action`.  Returns a conflict if the chord is already
    /// mapped to a different action in the user bindings or defaults.
    ///
    /// The binding is applied even when a conflict is reported, so callers can
    /// choose to ignore it or prompt the user.
    pub fn bind(&mut self, chord: Chord, action: Shortcut) -> Option<KeyConflict> {
        let existing = self
            .bindings
            .get(&chord)
            .or_else(|| self.defaults.get(&chord))
            .copied();
        self.bindings.insert(chord, action);
        existing
            .filter(|&existing| existing != action)
            .map(|existing| KeyConflict {
                chord,
                existing,
                incoming: action,
            })
    }

    /// Remove a user binding for `chord`.  The default binding, if any, remains
    /// effective.
    pub fn unbind(&mut self, chord: Chord) -> Option<Shortcut> {
        self.bindings.remove(&chord)
    }

    /// All current effective bindings (defaults overwritten by user bindings).
    pub fn effective(&self) -> AHashMap<Chord, Shortcut> {
        let mut out = self.defaults.clone();
        out.extend(self.bindings.iter().map(|(&k, &v)| (k, v)));
        out
    }

    /// Return every pair of bindings that map different actions to the same chord.
    ///
    /// Because each chord maps to at most one action in `effective`, this only
    /// reports conflicts introduced by overriding a default with a chord that is
    /// already the default for another action.
    pub fn conflicts(&self) -> Vec<KeyConflict> {
        let mut conflicts = Vec::new();
        for (&chord, &incoming) in &self.bindings {
            if let Some(&existing) = self.defaults.get(&chord) {
                if existing != incoming {
                    conflicts.push(KeyConflict {
                        chord,
                        existing,
                        incoming,
                    });
                }
            }
        }
        conflicts
    }

    /// Serialize the user overrides to a TOML string.
    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        let file = KeymapFile {
            binding: self
                .bindings
                .iter()
                .map(|(&chord, &action)| BindingSerde {
                    chord: chord.into(),
                    action: format!("{action:?}"),
                })
                .collect(),
        };
        toml::to_string_pretty(&file)
    }

    /// Load user overrides from a TOML string, replacing any existing overrides.
    ///
    /// Returns an error if the TOML is malformed or references unknown chords or
    /// actions.
    pub fn from_toml(&mut self, text: &str) -> Result<(), String> {
        let file: KeymapFile = toml::from_str(text).map_err(|e| e.to_string())?;
        let mut new_bindings = AHashMap::new();
        for binding in file.binding {
            let chord: Chord = binding.chord.try_into()?;
            let action = parse_action(&binding.action)?;
            new_bindings.insert(chord, action);
        }
        self.bindings = new_bindings;
        Ok(())
    }

    /// Load user overrides from a TOML file at `path`.
    pub fn load<P: AsRef<std::path::Path>>(&mut self, path: P) -> Result<(), String> {
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        self.from_toml(&text)
    }

    /// Save the current user overrides to a TOML file at `path`.
    pub fn save<P: AsRef<std::path::Path>>(&self, path: P) -> Result<(), String> {
        let text = self.to_toml().map_err(|e| e.to_string())?;
        std::fs::write(path, text).map_err(|e| e.to_string())
    }

    /// Platform config file path for keymap overrides.
    pub fn config_path() -> Option<PathBuf> {
        directories::ProjectDirs::from("com", "arte", "ogre")
            .map(|dirs| dirs.config_dir().join("keymap.toml"))
    }

    /// Render the keyboard-shortcuts editor inline into `ui`.
    ///
    /// `chord_buf`, `action_buf`, and `feedback_buf` hold the editor's transient
    /// input state and must be kept alive by the caller across frames.
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        chord_buf: &mut String,
        action_buf: &mut String,
        feedback_buf: &mut String,
    ) {
        ui.label("Enter a chord (e.g. Ctrl+Shift+D) and choose an action.");

        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(chord_buf)
                    .hint_text("Ctrl+Shift+D")
                    .desired_width(140.0),
            );
            egui::ComboBox::from_id_salt("keymap_action")
                .width(180.0)
                .selected_text(action_buf.as_str())
                .show_ui(ui, |ui| {
                    for &action in Self::all_actions() {
                        let label = format!("{action:?}");
                        ui.selectable_value(action_buf, label.clone(), label);
                    }
                });
            if ui.button("Bind").clicked() {
                feedback_buf.clear();
                match chord_buf.parse::<Chord>() {
                    Ok(chord) => match parse_action(action_buf) {
                        Ok(action) => {
                            if let Some(conflict) = self.bind(chord, action) {
                                feedback_buf.push_str(&format!(
                                    "Conflict: {} already bound to {:?}; overwritten to {:?}.",
                                    conflict.chord, conflict.existing, conflict.incoming
                                ));
                            } else {
                                feedback_buf.push_str("Binding saved.");
                            }
                            if let Some(path) = Self::config_path() {
                                if let Err(e) = self.save(&path) {
                                    feedback_buf.push_str(&format!(" Failed to save: {e}."));
                                }
                            }
                        }
                        Err(e) => feedback_buf.push_str(&format!("Unknown action: {e}")),
                    },
                    Err(e) => feedback_buf.push_str(&format!("Invalid chord: {e}")),
                }
            }
        });

        if !feedback_buf.is_empty() {
            ui.colored_label(egui::Color32::YELLOW, feedback_buf.as_str());
        }

        ui.separator();

        ui.horizontal(|ui| {
            ui.heading("Current bindings");
            if ui.button("Reset to defaults").clicked() {
                self.bindings.clear();
                feedback_buf.clear();
                if let Some(path) = Self::config_path() {
                    let _ = self.save(&path);
                }
            }
        });

        let mut bindings: Vec<(Shortcut, Chord)> = self
            .effective()
            .into_iter()
            .map(|(chord, action)| (action, chord))
            .collect();
        bindings.sort_by(|a, b| format!("{:?}", a.0).cmp(&format!("{:?}", b.0)));

        egui::ScrollArea::vertical().max_height(200.0).show_rows(
            ui,
            ui.text_style_height(&egui::TextStyle::Body),
            bindings.len(),
            |ui, range| {
                for (action, chord) in &bindings[range] {
                    ui.horizontal(|ui| {
                        ui.label(format!("{action:?}"));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if self.bindings.contains_key(chord) {
                                let label = format!("{chord}");
                                if ui
                                    .small_button("✖")
                                    .on_hover_text(format!("Remove {label}"))
                                    .clicked()
                                {
                                    self.unbind(*chord);
                                    if let Some(path) = Self::config_path() {
                                        let _ = self.save(&path);
                                    }
                                }
                            }
                            ui.label(format!("{chord}"));
                        });
                    });
                }
            },
        );

        let conflicts = self.conflicts();
        if !conflicts.is_empty() {
            ui.separator();
            ui.heading("Conflicts");
            for conflict in &conflicts {
                ui.colored_label(
                    egui::Color32::LIGHT_RED,
                    format!(
                        "{} is bound to both {:?} and {:?}",
                        conflict.chord, conflict.existing, conflict.incoming
                    ),
                );
            }
        }
    }

    /// All shortcut actions that can be bound.
    pub fn all_actions() -> &'static [Shortcut] {
        &[
            Shortcut::Undo,
            Shortcut::Redo,
            Shortcut::Deselect,
            Shortcut::SelectAll,
            Shortcut::InvertSelection,
            Shortcut::CommitActiveTool,
            Shortcut::NewRasterLayer,
            Shortcut::DuplicateLayer,
            Shortcut::DeleteLayer,
            Shortcut::RenameLayer,
            Shortcut::ZoomIn,
            Shortcut::ZoomOut,
            Shortcut::Zoom100,
            Shortcut::NewDocument,
            Shortcut::OpenDocument,
            Shortcut::SaveDocument,
            Shortcut::SaveAsDocument,
            Shortcut::FreeTransform,
            Shortcut::DefaultColors,
            Shortcut::SwapColors,
            Shortcut::FillForeground,
            Shortcut::FillBackground,
            Shortcut::CloseDocument,
            Shortcut::Quit,
            Shortcut::NextTab,
            Shortcut::PrevTab,
            Shortcut::BringForward,
            Shortcut::SendBackward,
            Shortcut::ToggleBirdsEye,
        ]
    }
}

fn parse_action(name: &str) -> Result<Shortcut, String> {
    match name {
        "Undo" => Ok(Shortcut::Undo),
        "Redo" => Ok(Shortcut::Redo),
        "Deselect" => Ok(Shortcut::Deselect),
        "SelectAll" => Ok(Shortcut::SelectAll),
        "InvertSelection" => Ok(Shortcut::InvertSelection),
        "CommitActiveTool" => Ok(Shortcut::CommitActiveTool),
        "NewRasterLayer" => Ok(Shortcut::NewRasterLayer),
        "DuplicateLayer" => Ok(Shortcut::DuplicateLayer),
        "DeleteLayer" => Ok(Shortcut::DeleteLayer),
        "RenameLayer" => Ok(Shortcut::RenameLayer),
        "ZoomIn" => Ok(Shortcut::ZoomIn),
        "ZoomOut" => Ok(Shortcut::ZoomOut),
        "Zoom100" => Ok(Shortcut::Zoom100),
        "NewDocument" => Ok(Shortcut::NewDocument),
        "OpenDocument" => Ok(Shortcut::OpenDocument),
        "SaveDocument" => Ok(Shortcut::SaveDocument),
        "SaveAsDocument" => Ok(Shortcut::SaveAsDocument),
        "FreeTransform" => Ok(Shortcut::FreeTransform),
        "DefaultColors" => Ok(Shortcut::DefaultColors),
        "SwapColors" => Ok(Shortcut::SwapColors),
        "FillForeground" => Ok(Shortcut::FillForeground),
        "FillBackground" => Ok(Shortcut::FillBackground),
        "CloseDocument" => Ok(Shortcut::CloseDocument),
        "Quit" => Ok(Shortcut::Quit),
        "NextTab" => Ok(Shortcut::NextTab),
        "PrevTab" => Ok(Shortcut::PrevTab),
        "BringForward" => Ok(Shortcut::BringForward),
        "SendBackward" => Ok(Shortcut::SendBackward),
        "ToggleBirdsEye" => Ok(Shortcut::ToggleBirdsEye),
        _ => Err(format!("unknown action: {name}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Key, Modifiers};

    fn chord(modifiers: Modifiers, key: Key) -> Chord {
        Chord { modifiers, key }
    }

    #[test]
    fn default_keymap_resolves_undo_and_redo() {
        let keymap = Keymap::default_shortcuts();
        assert_eq!(
            keymap.action(chord(Modifiers::COMMAND, Key::Z)),
            Some(Shortcut::Undo)
        );
        assert_eq!(
            keymap.action(chord(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z)),
            Some(Shortcut::Redo)
        );
        assert_eq!(
            keymap.action(chord(Modifiers::COMMAND, Key::Y)),
            Some(Shortcut::Redo)
        );
        assert_eq!(
            keymap.action(chord(Modifiers::NONE, Key::X)),
            Some(Shortcut::SwapColors)
        );
        // A truly unbound key still resolves to None.
        assert_eq!(keymap.action(chord(Modifiers::NONE, Key::F)), None);
    }

    #[test]
    fn default_keymap_resolves_layer_shortcuts() {
        let keymap = Keymap::default_shortcuts();
        assert_eq!(
            keymap.action(chord(Modifiers::COMMAND | Modifiers::SHIFT, Key::N)),
            Some(Shortcut::NewRasterLayer)
        );
        assert_eq!(
            keymap.action(chord(Modifiers::COMMAND, Key::J)),
            Some(Shortcut::DuplicateLayer)
        );
    }

    #[test]
    fn default_keymap_resolves_zoom_shortcuts() {
        let keymap = Keymap::default_shortcuts();
        assert_eq!(
            keymap.action(chord(Modifiers::COMMAND, Key::Equals)),
            Some(Shortcut::ZoomIn)
        );
        assert_eq!(
            keymap.action(chord(Modifiers::COMMAND, Key::Minus)),
            Some(Shortcut::ZoomOut)
        );
        assert_eq!(
            keymap.action(chord(Modifiers::COMMAND, Key::Num0)),
            Some(Shortcut::Zoom100)
        );
    }

    #[test]
    fn default_keymap_resolves_toggle_birds_eye() {
        let keymap = Keymap::default_shortcuts();
        assert_eq!(
            keymap.action(chord(Modifiers::COMMAND | Modifiers::SHIFT, Key::B)),
            Some(Shortcut::ToggleBirdsEye)
        );
    }

    #[test]
    fn toggle_birds_eye_round_trips_through_toml() {
        let mut keymap = Keymap::default_shortcuts();
        // Rebind to a fresh chord and confirm parse_action handles the action.
        let c = chord(Modifiers::COMMAND, Key::B);
        keymap.bind(c, Shortcut::ToggleBirdsEye);
        let text = keymap.to_toml().unwrap();
        let mut loaded = Keymap::default_shortcuts();
        loaded.from_toml(&text).unwrap();
        assert_eq!(loaded.action(c), Some(Shortcut::ToggleBirdsEye));
    }

    #[test]
    fn parse_key_accepts_equals_minus_zero() {
        assert_eq!(parse_key("=").unwrap(), Key::Equals);
        assert_eq!(parse_key("+").unwrap(), Key::Equals);
        assert_eq!(parse_key("-").unwrap(), Key::Minus);
        assert_eq!(parse_key("0").unwrap(), Key::Num0);
    }

    #[test]
    fn layer_shortcuts_round_trip_through_toml() {
        let mut keymap = Keymap::default_shortcuts();
        // Bind to a fresh chord (Ctrl+F is unbound by default) to exercise
        // parse_action + serialization without colliding with a default.
        let c = chord(Modifiers::COMMAND, Key::F);
        assert!(keymap.bind(c, Shortcut::NewRasterLayer).is_none());
        let text = keymap.to_toml().unwrap();
        let mut loaded = Keymap::default_shortcuts();
        loaded.from_toml(&text).unwrap();
        assert_eq!(loaded.action(c), Some(Shortcut::NewRasterLayer));
    }

    #[test]
    fn rebind_overrides_default() {
        let mut keymap = Keymap::default_shortcuts();
        let c = chord(Modifiers::COMMAND, Key::Z);
        assert_eq!(keymap.action(c), Some(Shortcut::Undo));
        let conflict = keymap.bind(c, Shortcut::Redo);
        assert!(conflict.is_some());
        assert_eq!(keymap.action(c), Some(Shortcut::Redo));
    }

    #[test]
    fn unbind_restores_default() {
        let mut keymap = Keymap::default_shortcuts();
        let c = chord(Modifiers::COMMAND, Key::Z);
        keymap.bind(c, Shortcut::Redo);
        assert_eq!(keymap.action(c), Some(Shortcut::Redo));
        keymap.unbind(c);
        assert_eq!(keymap.action(c), Some(Shortcut::Undo));
    }

    #[test]
    fn round_trip_through_toml() {
        let mut keymap = Keymap::default_shortcuts();
        let c = chord(Modifiers::COMMAND | Modifiers::SHIFT, Key::D);
        keymap.bind(c, Shortcut::Deselect);

        let text = keymap.to_toml().unwrap();
        let mut loaded = Keymap::default_shortcuts();
        loaded.from_toml(&text).unwrap();

        assert_eq!(loaded.action(c), Some(Shortcut::Deselect));
        assert_eq!(
            loaded.action(chord(Modifiers::COMMAND, Key::Z)),
            Some(Shortcut::Undo)
        );
    }

    #[test]
    fn conflict_detection_reports_default_override() {
        let mut keymap = Keymap::default_shortcuts();
        let c = chord(Modifiers::COMMAND, Key::Z);
        keymap.bind(c, Shortcut::Redo);
        let conflicts = keymap.conflicts();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].chord, c);
        assert_eq!(conflicts[0].existing, Shortcut::Undo);
        assert_eq!(conflicts[0].incoming, Shortcut::Redo);
    }

    #[test]
    fn from_toml_rejects_unknown_action() {
        let mut keymap = Keymap::default_shortcuts();
        let text = r#"
[[binding]]
chord = { ctrl = true, key = "Z" }
action = "Explode"
"#;
        assert!(keymap.from_toml(text).is_err());
    }

    #[test]
    fn save_and_load_round_trip_through_file() {
        let mut keymap = Keymap::default_shortcuts();
        let c = chord(Modifiers::COMMAND | Modifiers::SHIFT, Key::D);
        keymap.bind(c, Shortcut::Deselect);

        let dir = std::env::temp_dir().join(format!("ogre_keymap_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("keymap.toml");

        keymap.save(&path).unwrap();
        let mut loaded = Keymap::default_shortcuts();
        loaded.load(&path).unwrap();

        assert_eq!(loaded.action(c), Some(Shortcut::Deselect));
        assert_eq!(
            loaded.action(chord(Modifiers::COMMAND, Key::Z)),
            Some(Shortcut::Undo)
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn chord_from_str_parses_modifiers_and_key() {
        let c: Chord = "Ctrl+Shift+D".parse().unwrap();
        assert!(c.modifiers.ctrl);
        assert!(c.modifiers.shift);
        assert!(!c.modifiers.alt);
        assert!(!c.modifiers.command);
        assert_eq!(c.key, Key::D);

        let c: Chord = "Escape".parse().unwrap();
        assert_eq!(c.modifiers, Modifiers::NONE);
        assert_eq!(c.key, Key::Escape);

        assert!("Ctrl+".parse::<Chord>().is_err());
        assert!("UnknownKey".parse::<Chord>().is_err());
    }

    #[test]
    fn config_path_returns_toml_file_in_config_dir() {
        let path = Keymap::config_path().unwrap();
        assert!(path.to_str().unwrap().contains("keymap.toml"));
    }

    #[test]
    fn resolve_treats_physical_ctrl_as_command() {
        let keymap = Keymap::default_shortcuts();
        // On Windows/Linux egui reports a physical Ctrl press as BOTH ctrl and
        // command set. A raw `==` against a `COMMAND` chord would never match.
        let ctrl = Modifiers {
            ctrl: true,
            command: true,
            ..Default::default()
        };
        assert_eq!(keymap.resolve(Key::Z, ctrl), Some(Shortcut::Undo));
        assert_eq!(keymap.resolve(Key::A, ctrl), Some(Shortcut::SelectAll));
        assert_eq!(keymap.resolve(Key::D, ctrl), Some(Shortcut::Deselect));

        // Shift must still be distinguished: Ctrl+Shift+Z is Redo, not Undo.
        let ctrl_shift = Modifiers {
            ctrl: true,
            command: true,
            shift: true,
            ..Default::default()
        };
        assert_eq!(keymap.resolve(Key::Z, ctrl_shift), Some(Shortcut::Redo));

        // A bare key with no command modifier resolves nothing.
        assert_eq!(keymap.resolve(Key::Z, Modifiers::default()), None);
    }
}
