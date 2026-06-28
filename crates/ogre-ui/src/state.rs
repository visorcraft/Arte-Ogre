// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Application state for the Arte Ogre editor shell.
//!
//! [`AppState`] owns the document, the undo history, the current viewport, the
//! active tool, and the dock layout. Keeping these together makes the shell a
//! thin controller that delegates all undoable edits to `dispatch`.

use egui_dock::DockState;
use glam::Vec2;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use crate::io_worker::{IoKind, IoResult, IoWorker};
use crate::keymap::Keymap;
use crate::plugin_worker::{PluginResult, PluginWorker};
use crate::prefs::{Autosave, Preferences};
use crate::shell::{default_dock_state, Panel};
use crate::tools::{SidebarSection, ToolFamily, ToolManager};
use ogre_plugins::PluginManager;

/// Which top-level surface fills the central area.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AppView {
    /// The editor (Tools sidebar + dock).
    #[default]
    Editor,
    /// Full-page Settings.
    Settings,
    /// Full-page Licenses.
    Licenses,
    /// Full-page Credits.
    Credits,
    /// Full-page Plugin Manager.
    Plugins,
    /// Full-page workspace for arranging layers across open documents.
    BirdsEye,
}

/// Transient UI state for [`AppView::BirdsEye`].
///
/// Holds the in-progress drag, the current drop target, a thumbnail texture
/// cache keyed by `(tab_id, layer_id)`, the set of thumbnails that need
/// regeneration, the global thumbnail zoom, and an optional feedback string for
/// rejected drops. None of this is persisted.
pub struct BirdsEyeState {
    /// The layer currently being dragged, if any.
    pub drag: Option<BirdsEyeDrag>,
    /// The current drop target under the pointer, if any.
    pub hover: Option<BirdsEyeDropTarget>,
    /// Cached thumbnail textures keyed by `(tab_id, layer_id)`.
    pub thumbnails: ahash::AHashMap<(u64, ogre_core::LayerId), egui::TextureHandle>,
    /// Thumbnails that need regeneration on the next render.
    pub dirty_thumbnails: ahash::AHashSet<(u64, ogre_core::LayerId)>,
    /// Global zoom factor applied to all thumbnails.
    pub zoom: f32,
    /// Short message shown when a drop is rejected.
    pub feedback: Option<String>,
}

impl std::fmt::Debug for BirdsEyeState {
    // `egui::TextureHandle` is not `Debug`, so report the cache size instead of
    // the handles themselves.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BirdsEyeState")
            .field("drag", &self.drag)
            .field("hover", &self.hover)
            .field("thumbnails", &self.thumbnails.len())
            .field("dirty_thumbnails", &self.dirty_thumbnails)
            .field("zoom", &self.zoom)
            .field("feedback", &self.feedback)
            .finish()
    }
}

impl Default for BirdsEyeState {
    /// The default Bird's Eye state starts with a neutral `1.0` thumbnail zoom.
    fn default() -> Self {
        Self {
            drag: None,
            hover: None,
            thumbnails: ahash::AHashMap::new(),
            dirty_thumbnails: ahash::AHashSet::new(),
            zoom: 1.0,
            feedback: None,
        }
    }
}

/// A layer being dragged in Bird's Eye View.
#[derive(Debug, Clone)]
pub struct BirdsEyeDrag {
    /// Stable id of the source document tab.
    pub source_tab_id: u64,
    /// The dragged layer's id.
    pub layer_id: ogre_core::LayerId,
    /// The layer's original index in bottom-to-top display order.
    pub original_index: usize,
}

/// A drop target in Bird's Eye View.
#[derive(Debug, Clone)]
pub struct BirdsEyeDropTarget {
    /// Stable id of the target document tab.
    pub tab_id: u64,
    /// Insertion gap in bottom-to-top display order.
    ///
    /// For cross-document inserts this is `0..=len`. For same-document reorders
    /// it is converted to a final `0..len-1` index.
    pub gap_index: usize,
}

/// Default undo history depth limit.
///
/// A finite limit prevents long sessions from retaining an unbounded number of
/// tile snapshots and full layer copies in memory. `ogre_core::History` uses `0`
/// to mean unlimited; the UI therefore supplies a concrete default.
pub const DEFAULT_UNDO_LIMIT: usize = 100;

/// A modal dialog asking the user for a selection-refinement parameter.
#[derive(Debug, Clone)]
pub enum SelectionDialog {
    /// Feather radius entry.
    Feather {
        /// Gaussian radius (sigma) as typed by the user.
        radius: String,
    },
    /// Grow amount entry, in pixels.
    Grow {
        /// Number of pixels to dilate by, as typed by the user.
        amount: String,
    },
    /// Shrink amount entry, in pixels.
    Shrink {
        /// Number of pixels to erode by, as typed by the user.
        amount: String,
    },
    /// Stroke width entry (v1: Rect selections only).
    Stroke {
        /// Stroke diameter in pixels, as typed by the user.
        width: String,
    },
}

/// The Color Range modal dialog (§3.4.3). Not a `ToolKind` — a menu command
/// (`Select → Color Range…`) that builds a selection from every pixel within a
/// perceptual distance of the chosen seed color.
#[derive(Debug, Clone)]
pub struct ColorRangeDialog {
    /// The seed color (picked from the dialog's color picker).
    pub seed: ogre_core::Rgba32F,
    /// Fuzziness in `[0, 1]`; mapped to the perceptual tolerance.
    pub fuzziness: f32,
    /// Select the inverse of the matched region.
    pub invert: bool,
    /// Sample the merged composite (on) vs the active layer only (off).
    pub sample_all_layers: bool,
}

impl Default for ColorRangeDialog {
    fn default() -> Self {
        Self {
            seed: ogre_core::Rgba32F::new(1.0, 0.0, 0.0, 1.0),
            fuzziness: 0.2,
            invert: false,
            sample_all_layers: true,
        }
    }
}

/// A modal dialog for applying a parameterized filter or adjustment.
#[derive(Debug, Clone)]
pub enum FilterDialog {
    /// Brightness and contrast parameters.
    BrightnessContrast {
        /// Brightness offset as typed by the user.
        brightness: String,
        /// Contrast multiplier as typed by the user.
        contrast: String,
    },
    /// Levels parameters.
    Levels {
        /// Input black point.
        input_black: String,
        /// Input white point.
        input_white: String,
        /// Output black point.
        output_black: String,
        /// Output white point.
        output_white: String,
        /// Gamma midtone correction.
        gamma: String,
    },
    /// Hue/saturation/lightness parameters.
    HueSat {
        /// Hue shift in degrees.
        hue: String,
        /// Saturation multiplier.
        saturation: String,
        /// Lightness offset.
        lightness: String,
    },
    /// Gaussian blur radius.
    GaussianBlur {
        /// Blur radius (sigma) in pixels.
        radius: String,
    },
    /// Sharpen/unsharp mask parameters.
    Sharpen {
        /// Sharpening amount.
        amount: String,
        /// Radius for the unsharp mask.
        radius: String,
    },
    /// Emboss blend strength.
    Emboss {
        /// Blend amount in `0.0..=1.0`.
        amount: String,
    },
    /// Edge-detection blend strength.
    EdgeDetect {
        /// Blend amount in `0.0..=1.0`.
        amount: String,
    },
    /// Curves / tone-response parameters.
    ///
    /// Four `(input, output)` pairs, edited as text.
    Curves {
        /// Input/output pairs for the four control points.
        points: [(String, String); 4],
    },
    /// Posterize level count.
    Posterize {
        /// Number of discrete levels per channel (≥ 2), as typed.
        levels: String,
    },
    /// Threshold cutoff.
    Threshold {
        /// Luminance cutoff in `0.0..=1.0`, as typed.
        level: String,
    },
    /// Gradient map foreground/background colours.
    GradientMap {
        /// Shadow colour (linear RGBA) as `[r, g, b, a]`.
        fg: [f32; 4],
        /// Highlight colour (linear RGBA).
        bg: [f32; 4],
    },
}

impl FilterDialog {
    /// Localized window heading for this dialog variant.
    pub fn title(&self) -> &'static str {
        match self {
            FilterDialog::BrightnessContrast { .. } => "Brightness / Contrast",
            FilterDialog::Levels { .. } => "Levels",
            FilterDialog::HueSat { .. } => "Hue / Saturation",
            FilterDialog::GaussianBlur { .. } => "Gaussian Blur",
            FilterDialog::Sharpen { .. } => "Sharpen",
            FilterDialog::Emboss { .. } => "Emboss",
            FilterDialog::EdgeDetect { .. } => "Edge Detect",
            FilterDialog::Curves { .. } => "Curves",
            FilterDialog::Posterize { .. } => "Posterize",
            FilterDialog::Threshold { .. } => "Threshold",
            FilterDialog::GradientMap { .. } => "Gradient Map",
        }
    }

    /// Open a dialog with sensible default values.
    pub fn with_defaults(kind: FilterKind) -> Self {
        match kind {
            FilterKind::BrightnessContrast => Self::BrightnessContrast {
                brightness: "0".to_string(),
                contrast: "1".to_string(),
            },
            FilterKind::Levels => Self::Levels {
                input_black: "0".to_string(),
                input_white: "1".to_string(),
                output_black: "0".to_string(),
                output_white: "1".to_string(),
                gamma: "1".to_string(),
            },
            FilterKind::HueSat => Self::HueSat {
                hue: "0".to_string(),
                saturation: "1".to_string(),
                lightness: "0".to_string(),
            },
            FilterKind::GaussianBlur => Self::GaussianBlur {
                radius: "1".to_string(),
            },
            FilterKind::Sharpen => Self::Sharpen {
                amount: "1".to_string(),
                radius: "1".to_string(),
            },
            FilterKind::Emboss => Self::Emboss {
                amount: "1".to_string(),
            },
            FilterKind::EdgeDetect => Self::EdgeDetect {
                amount: "1".to_string(),
            },
            FilterKind::Curves => Self::Curves {
                points: [
                    ("0".to_string(), "0".to_string()),
                    ("0.33".to_string(), "0.33".to_string()),
                    ("0.66".to_string(), "0.66".to_string()),
                    ("1".to_string(), "1".to_string()),
                ],
            },
            FilterKind::Posterize => Self::Posterize {
                levels: "4".to_string(),
            },
            FilterKind::Threshold => Self::Threshold {
                level: "0.5".to_string(),
            },
            FilterKind::GradientMap => Self::GradientMap {
                fg: [0.0, 0.0, 0.0, 1.0],
                bg: [1.0, 1.0, 1.0, 1.0],
            },
        }
    }
}

/// Identifies a parameterized filter/adjustment dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterKind {
    /// Brightness/contrast.
    BrightnessContrast,
    /// Levels.
    Levels,
    /// Hue/saturation/lightness.
    HueSat,
    /// Gaussian blur.
    GaussianBlur,
    /// Sharpen/unsharp mask.
    Sharpen,
    /// Emboss (directional relief).
    Emboss,
    /// Edge detection (Sobel magnitude).
    EdgeDetect,
    /// Curves / tone-response.
    Curves,
    /// Posterize.
    Posterize,
    /// Threshold.
    Threshold,
    /// Gradient map.
    GradientMap,
}

/// SVG import options shown before loading an SVG/SVGZ file.
#[derive(Debug, Clone)]
pub struct SvgImportDialog {
    /// Path to the SVG file being imported.
    pub path: PathBuf,
    /// Import mode selected by the user.
    pub mode: ogre_io::svg::SvgImportMode,
    /// Target DPI, edited as text.
    pub dpi: String,
    /// Transient validation message shown inside the dialog.
    pub feedback: String,
}

impl SvgImportDialog {
    /// Create a new dialog for `path` with sensible defaults.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            mode: ogre_io::svg::SvgImportMode::Both,
            dpi: "96".to_string(),
            feedback: String::new(),
        }
    }
}

/// A modal file-related dialog shown from the `File` menu.
#[derive(Debug, Clone)]
pub enum FileDialog {
    /// Create a new blank document.
    New {
        /// Width in pixels, as typed by the user.
        width: String,
        /// Height in pixels, as typed by the user.
        height: String,
    },
    /// Open an existing document or image.
    Open {
        /// Path to the file to open.
        path: String,
    },
    /// Save the current document under a new path.
    SaveAs {
        /// Path to write the `.ogre` file to.
        path: String,
    },
    /// Export the current document as a raster image or OpenRaster archive.
    Export {
        /// Path to write the exported file to.
        path: String,
        /// One of "PNG", "JPEG", "TIFF", "WebP", "EXR", "ORA".
        format: String,
        /// JPEG quality, 0–100.
        quality: String,
        /// Bit depth, "8" or "16".
        bit_depth: String,
    },
    /// Export each document slice as a separate raster image.
    ExportSlices {
        /// Directory to write the slice images into.
        path: String,
        /// One of "PNG", "JPEG", "TIFF", "WebP", "EXR".
        format: String,
        /// JPEG quality, 0–100.
        quality: String,
        /// Bit depth, "8" or "16".
        bit_depth: String,
    },
}

/// Pending paste operation waiting for the user to confirm or resize.
///
/// Populated when the pasted image is larger than the canvas or the document
/// is still showing the welcome screen.  The UI renders a resize-prompt modal
/// while this is `Some`.
#[derive(Debug, Clone)]
pub struct PastePrompt {
    /// Width of the image to paste, in pixels.
    pub width: u32,
    /// Height of the image to paste, in pixels.
    pub height: u32,
    /// Row-major RGBA pixels of the image to paste.
    pub pixels: Vec<ogre_core::Rgba32F>,
}

/// A modal dialog for resizing the document canvas.
///
/// Width/height are held as numbers so the dialog's numeric spinners cannot hold
/// non-numeric input.
#[derive(Debug, Clone)]
pub struct CanvasSizeDialog {
    /// New width in pixels (always ≥ 1).
    pub width: u32,
    /// New height in pixels (always ≥ 1).
    pub height: u32,
    /// Selected anchor point.
    pub anchor: ogre_core::CanvasAnchor,
}

/// State of the "Check for Updates" background request.
#[derive(Default)]
pub enum UpdateCheck {
    /// Not started / modal closed.
    #[default]
    Idle,
    /// Request in flight; receiver delivers the result once.
    Checking(std::sync::mpsc::Receiver<Result<String, String>>),
    /// Finished: `Ok(latest_tag)` or `Err(message)`.
    Done(Result<String, String>),
}

impl std::fmt::Debug for UpdateCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            UpdateCheck::Idle => "UpdateCheck::Idle",
            UpdateCheck::Checking(_) => "UpdateCheck::Checking",
            UpdateCheck::Done(_) => "UpdateCheck::Done",
        })
    }
}

/// A single open document tab.
#[derive(Debug)]
pub struct DocumentTab {
    /// Stable tab identifier.
    pub id: u64,
    /// The layered image document.
    pub doc: ogre_core::Document,
    /// Undo/redo stack for the document.
    pub history: ogre_core::History,
    /// Current pan/zoom viewport for the canvas.
    pub viewport: ogre_gpu::Viewport,
    /// Path the tab was last saved to, used for native `Save`.
    pub last_save_path: Option<PathBuf>,
    /// Path the tab was originally opened from (e.g. a PNG import). Used to
    /// re-export on `File → Save` when there is no native `.ogre` save path.
    pub import_path: Option<PathBuf>,
    /// Whether the tab has edits since the last save/open.
    pub unsaved: bool,
}

impl DocumentTab {
    /// Create a blank document tab with a background layer.
    pub fn new_blank(id: u64, size: (u32, u32)) -> Self {
        let mut doc = ogre_core::Document::new(size.0, size.1);
        doc.add_raster_layer("Background");
        Self {
            id,
            doc,
            history: ogre_core::History::new(DEFAULT_UNDO_LIMIT),
            viewport: ogre_gpu::Viewport::new(Vec2::ZERO, 1.0),
            last_save_path: None,
            import_path: None,
            unsaved: false,
        }
    }
}

/// The full mutable state of the editor.
#[derive(Debug)]
pub struct AppState {
    /// Open document tabs. Always contains at least one entry.
    pub tabs: Vec<DocumentTab>,
    /// Index into `tabs` of the currently active document.
    pub active_tab: usize,
    /// Next tab ID to allocate.
    pub next_tab_id: u64,
    /// Tab ID waiting for an open result, if any.
    pub pending_open_tab_id: Option<u64>,
    /// Tab ID waiting for a save result, if any.
    pub pending_save_tab_id: Option<u64>,
    /// Tab ID waiting for a close confirmation, if any.
    pub pending_close_tab_id: Option<u64>,
    /// Tool manager: owns the active tool and dispatches pointer events.
    pub tool_manager: ToolManager,
    /// Last-active sibling per tool family, used by bare-key cycling (§2.1).
    /// Empty until a family is first activated; cycling falls back to the
    /// family primary when an entry is absent.
    pub tool_family_last: ahash::AHashMap<ToolFamily, crate::tools::ToolKind>,
    /// Foreground color used by the brush, pencil, and paint bucket, and set by
    /// the eyedropper. Linear, straight alpha.
    pub foreground: ogre_core::Rgba32F,
    /// Background color (paired with the foreground via swap). Linear, straight
    /// alpha.
    pub background: ogre_core::Rgba32F,
    /// Screen-space rectangle of the canvas drawing area, updated each frame the
    /// canvas renders. Used to center modal dialogs over the canvas.
    pub canvas_screen_rect: Option<egui::Rect>,
    /// Frames remaining in a "fit window to canvas" request. The Layers dock
    /// panel is sized as a fraction of the window, so one resize only partly
    /// fits the canvas; re-fit for a few frames until it converges.
    pub fit_to_canvas_pending: u32,
    /// `true` when the GPU canvas needs to be recomposited.
    pub dirty: bool,
    /// `true` when the renderer should drop all cached GPU textures because the
    /// document has been replaced.
    pub renderer_needs_clear: bool,
    /// Docking layout of the editor panels.
    pub dock: DockState<Panel>,
    /// Layer currently being dragged for reorder, if any.
    pub dragging_layer: Option<ogre_core::LayerId>,
    /// Sidebar section currently being dragged for reorder, if any.
    pub dragging_sidebar_section: Option<SidebarSection>,
    /// Stable header-center y coordinates captured at drag start, used to
    /// compute the drop target without flicker as the list relayouts live.
    pub dragging_sidebar_section_anchors: Option<Vec<f32>>,
    /// Layer whose opacity slider is currently being dragged, if any.
    ///
    /// Used to coalesce a single drag into one history entry while preventing
    /// typed edits from merging into a previous drag's command.
    pub opacity_drag_layer: Option<ogre_core::LayerId>,
    /// Active selection-refinement parameter dialog, if any.
    pub selection_dialog: Option<SelectionDialog>,
    /// Active Color Range dialog, if open (`Select → Color Range…`).
    pub color_range_dialog: Option<ColorRangeDialog>,
    /// Active canvas-size dialog, if any.
    pub canvas_size_dialog: Option<CanvasSizeDialog>,
    /// Active layer-rename dialog: `(layer id, editable name buffer)`.
    pub rename_dialog: Option<(ogre_core::LayerId, String)>,
    /// Pending Remove-Background options dialog, if open. Holds the editable
    /// options; confirming starts the background-removal job.
    pub remove_bg_dialog: Option<ogre_core::MatteOptions>,
    /// Whether the Remove-Background dialog's "Refine with AI" box is ticked
    /// (only meaningful in an `ml`-feature build). Persists across opens.
    pub remove_bg_ai: bool,
    /// Active file operation dialog, if any.
    pub file_dialog: Option<FileDialog>,
    /// Feedback message shown in the active file dialog.
    pub file_dialog_feedback: String,
    /// Transient error message from a failed command dispatch, shown briefly in
    /// the status bar. Cleared after a short timeout or the next success.
    pub error_feedback: Option<String>,
    /// Active SVG import options dialog, if any.
    pub svg_import_dialog: Option<SvgImportDialog>,
    /// Active filter/adjustment parameter dialog, if any.
    pub filter_dialog: Option<FilterDialog>,
    /// `true` if the open filter dialog should create an adjustment layer on OK.
    pub filter_dialog_adjustment: bool,
    /// `true` if the current filter dialog values are invalid (persisted so the
    /// error label stays visible across frames).
    pub filter_dialog_invalid: bool,
    /// Configurable keyboard-shortcut mapping.
    pub keymap: Keymap,
    /// User preferences loaded from the platform config directory.
    pub preferences: Preferences,
    /// Periodic recovery autosave manager.
    pub autosave: Autosave,
    /// Current document-space cursor position while hovering the canvas.
    ///
    /// View state only; not part of the undo history.
    pub cursor_doc_pos: Option<ogre_core::IVec2>,
    /// Whether the zoom magnifier popup is currently open.
    pub show_zoom_popup: bool,
    /// Screen position of the left-click "Select Inverse / Deselect" menu, shown
    /// when the user clicks (without dragging) inside an existing selection.
    pub left_click_menu: Option<egui::Pos2>,
    /// True only on the frame the left-click menu opens, so the opening click is
    /// not mistaken for a click-outside that would close it immediately.
    pub left_click_menu_fresh: bool,
    /// Whether a Polygon Lasso path is mid-build, so each new vertex coalesces
    /// into the same selection history entry instead of pushing a new one.
    pub polygon_building: bool,
    /// Cached document-space selection outline edges (pixel-corner segments),
    /// retraced only when [`Self::selection_outline_key`] no longer matches the
    /// current selection — recomputing every frame would lag on big masks.
    pub selection_outline: Vec<[ogre_core::IVec2; 2]>,
    /// Outline cache key the cached `selection_outline` was built for; starts at
    /// a sentinel so the first frame always rebuilds.
    pub selection_outline_key: u64,
    /// Whether to draw the canvas grid overlay.
    pub show_grid: bool,
    /// Whether pointer input should snap to the canvas grid.
    pub snap_to_grid: bool,
    /// Transient (non-persisted) Tab-toggle: hide the tools sidebar and layers
    /// dock for an unobstructed canvas view. Not part of `Preferences` — it
    /// resets on restart and never touches serde.
    pub ui_chrome_hidden: bool,
    /// Grid line spacing in document pixels.
    pub grid_spacing: u32,
    /// Transient chord input in the keymap editor.
    pub keymap_editor_chord: String,
    /// Transient action input in the keymap editor.
    pub keymap_editor_action: String,
    /// Feedback message shown in the keymap editor.
    pub keymap_editor_feedback: String,
    /// Discovered plugins and their enable state.
    pub plugin_manager: PluginManager,
    /// Feedback message shown in the plugin manager.
    pub plugin_manager_feedback: String,
    /// Whether the welcome screen is shown in the central area.
    ///
    /// Set `true` when a new document is created; cleared when the user opens a
    /// file or dismisses the screen.
    pub welcome: bool,
    /// Whether the "Save changes before closing?" prompt is showing.
    pub close_prompt: bool,
    /// When `true`, the next successful save closes the document (returns to the
    /// welcome screen). Set by the close prompt's "Save" choice; cleared on a
    /// failed save or a cancelled Save As dialog.
    pub close_after_save: bool,

    /// Whether the "quit with unsaved changes" confirmation modal is showing.
    pub quit_confirm: bool,
    /// Set once the user confirms quitting so the window-close request is allowed
    /// through instead of being intercepted again.
    pub confirmed_quit: bool,
    /// Pending background-removal job. Receives the recomputed layer buffer, or
    /// `None` when no matte was detected, from a worker thread. `Some` here means
    /// the modal "Removing background" spinner is showing and edits are blocked.
    pub bg_removal_rx: Option<std::sync::mpsc::Receiver<Option<ogre_core::TiledBuffer>>>,
    /// The layer the pending background-removal result applies to.
    pub bg_removal_layer: Option<ogre_core::LayerId>,
    /// When the current background-removal job started, used to enforce a
    /// timeout so a hung worker cannot block edits forever.
    pub bg_removal_started: Option<std::time::Instant>,
    /// Whether the running background-removal job is doing AI refinement (which
    /// may download a model on first use), so the spinner can say so.
    pub bg_removal_ai: bool,
    /// Tracks whether a `V` key *press* reached egui since the last `V`
    /// release. egui-winit swallows the press of a Ctrl+V (paste) chord, so a
    /// `V` release with no matching press means the press was swallowed — i.e.
    /// the chord was Ctrl+V. Used to detect paste independent of the (racy)
    /// modifier state reported on the release event.
    pub paste_v_press_seen: bool,
    /// Which top-level surface is currently shown in the central area.
    pub view: AppView,
    /// Whether the About Arte Ogre dialog is open.
    pub show_about: bool,
    /// Selected tab on the Licenses page (0 = GPL-3.0, 1 = Third-party).
    pub licenses_tab: u8,
    /// State of the background "Check for Updates" request.
    pub update_check: UpdateCheck,
    /// Background worker for save/export/open I/O.
    pub io_worker: IoWorker,
    /// `true` while a background I/O operation is in flight or queued.
    pub io_busy: bool,
    /// Expected request IDs per I/O kind. Results with an ID that does not
    /// match the entry for their kind are stale and are ignored.
    pub io_expected: HashMap<IoKind, u64>,
    /// Status feedback for menu-triggered I/O operations that have no dialog.
    pub io_status_feedback: String,
    /// Background worker for Lua/WASM plugin execution.
    pub plugin_worker: PluginWorker,
    /// `true` while a background plugin operation is in flight.
    pub plugin_busy: bool,
    /// ID of the most recently enqueued plugin request. Results with a different
    /// ID are stale and are ignored.
    pub plugin_expected_id: Option<u64>,
    /// Transient state for the Ctrl+Shift+P command palette overlay.
    pub command_palette: crate::command_palette::CommandPalette,
    /// Pending clipboard paste awaiting user confirmation or resize.
    ///
    /// Set by the paste flow when the clipboard image is larger than the
    /// canvas or the welcome screen is still showing.  `None` means no paste
    /// is in progress.
    pub paste_prompt: Option<PastePrompt>,
    /// A document recovered from a previous session's crash snapshot, awaiting
    /// the user's Recover/Discard choice. `None` outside the startup recovery
    /// flow. Populated only by the real app entry point ([`crate::OgreApp::new`]),
    /// never by [`AppState::new_document`], so tests don't pick up stray files.
    pub recovery_prompt: Option<ogre_core::Document>,
    /// Transient UI state for Bird's Eye View (drag, thumbnails, zoom).
    pub birds_eye: BirdsEyeState,
}

/// Clamp a zoom factor to the interactive range used by the magnifier (10%–800%).
pub(crate) fn clamp_zoom(z: f32) -> f32 {
    z.clamp(0.1, 8.0)
}

/// Default directory where user plugins are discovered.
pub fn default_plugins_dir() -> PathBuf {
    directories::ProjectDirs::from("com", "arte", "ogre")
        .map(|dirs| dirs.data_local_dir().join("plugins"))
        .unwrap_or_else(|| PathBuf::from(".arte-ogre/plugins"))
}

impl AppState {
    /// Create a new app state with a blank document of the given size.
    ///
    /// The document contains one transparent raster layer named "Background",
    /// the active tool is rectangle select, the viewport is centered at the
    /// origin with a zoom of `1.0`, and `dirty` is `true` so the canvas is
    /// rendered on the first frame.
    pub fn new_document(w: u32, h: u32) -> Self {
        let preferences = Preferences::config_path()
            .map(Preferences::load_or_default)
            .unwrap_or_default();
        let recovery_dir = directories::ProjectDirs::from("com", "arte", "ogre")
            .map(|dirs| dirs.data_local_dir().join("recovery"))
            .unwrap_or_else(|| PathBuf::from(".arte-ogre/recovery"));
        let plugins_dir = default_plugins_dir();
        let _ = std::fs::create_dir_all(&plugins_dir);
        let autosave = Autosave::new(
            recovery_dir,
            Duration::from_secs(preferences.autosave_interval_seconds),
        );
        let active_tool = preferences.active_tool;
        let tool_family_last: ahash::AHashMap<_, _> = preferences
            .tool_family_last
            .iter()
            .map(|(&family, &kind)| (family, kind))
            .collect();

        let mut state = Self {
            tabs: vec![DocumentTab::new_blank(0, (w, h))],
            active_tab: 0,
            next_tab_id: 1,
            pending_open_tab_id: None,
            pending_save_tab_id: None,
            pending_close_tab_id: None,
            tool_manager: ToolManager::with_active(active_tool),
            tool_family_last,
            foreground: ogre_core::Rgba32F::new(0.0, 0.0, 0.0, 1.0),
            background: ogre_core::Rgba32F::new(1.0, 1.0, 1.0, 1.0),
            canvas_screen_rect: None,
            fit_to_canvas_pending: 0,
            dirty: true,
            renderer_needs_clear: false,
            dock: default_dock_state(preferences.layers_visible, 0),
            dragging_layer: None,
            dragging_sidebar_section: None,
            dragging_sidebar_section_anchors: None,
            opacity_drag_layer: None,
            selection_dialog: None,
            color_range_dialog: None,
            canvas_size_dialog: None,
            rename_dialog: None,
            remove_bg_dialog: None,
            remove_bg_ai: false,
            svg_import_dialog: None,
            file_dialog: None,
            file_dialog_feedback: String::new(),
            error_feedback: None,
            filter_dialog: None,
            filter_dialog_adjustment: false,
            filter_dialog_invalid: false,
            keymap: {
                let mut keymap = Keymap::default_shortcuts();
                if let Some(path) = Keymap::config_path() {
                    let _ = keymap.load(&path);
                }
                keymap
            },
            preferences,
            autosave,
            cursor_doc_pos: None,
            show_zoom_popup: false,
            left_click_menu: None,
            left_click_menu_fresh: false,
            polygon_building: false,
            selection_outline: Vec::new(),
            selection_outline_key: u64::MAX,
            show_grid: false,
            snap_to_grid: false,
            ui_chrome_hidden: false,
            grid_spacing: 64,
            keymap_editor_chord: String::new(),
            keymap_editor_action: String::new(),
            keymap_editor_feedback: String::new(),
            plugin_manager: PluginManager::new(&plugins_dir),
            plugin_manager_feedback: String::new(),
            welcome: true,
            close_prompt: false,
            close_after_save: false,
            quit_confirm: false,
            confirmed_quit: false,
            bg_removal_rx: None,
            bg_removal_layer: None,
            bg_removal_started: None,
            bg_removal_ai: false,
            paste_v_press_seen: false,
            view: AppView::default(),
            show_about: false,
            licenses_tab: 0,
            update_check: UpdateCheck::Idle,
            io_worker: IoWorker::new(),
            io_busy: false,
            io_expected: HashMap::new(),
            io_status_feedback: String::new(),
            plugin_worker: PluginWorker::new(),
            plugin_busy: false,
            plugin_expected_id: None,
            command_palette: crate::command_palette::CommandPalette::default(),
            paste_prompt: None,
            recovery_prompt: None,
            birds_eye: BirdsEyeState::default(),
        };
        let doc = state.doc().clone();
        state.tool_manager.load_active_vector_layer(&doc);
        state
    }

    /// Switch to `kind`, update the last-used sibling for its family, and persist
    /// the choice to user preferences so it survives the next launch.
    pub fn set_tool_and_persist(&mut self, kind: crate::tools::ToolKind) {
        let family = kind.family();
        self.tool_manager.set_tool(kind);
        self.tool_family_last.insert(family, kind);
        self.preferences.active_tool = kind;
        self.preferences.tool_family_last.insert(family, kind);
        if let Some(path) = Preferences::config_path() {
            let _ = self.preferences.save(&path);
        }
    }

    /// Borrow the active document tab.
    pub fn current_tab(&self) -> &DocumentTab {
        &self.tabs[self.active_tab]
    }

    /// Borrow the active document tab mutably.
    pub fn current_tab_mut(&mut self) -> &mut DocumentTab {
        &mut self.tabs[self.active_tab]
    }

    /// Borrow the active document.
    pub fn doc(&self) -> &ogre_core::Document {
        &self.current_tab().doc
    }

    /// Mark a single Bird's Eye thumbnail dirty so it is regenerated.
    pub fn mark_birdseye_thumbnail_dirty(&mut self, tab_id: u64, layer_id: ogre_core::LayerId) {
        self.birds_eye.thumbnails.remove(&(tab_id, layer_id));
        self.birds_eye.dirty_thumbnails.insert((tab_id, layer_id));
    }

    /// Drop every cached Bird's Eye thumbnail for `tab_id` and clear its dirty
    /// markers. A tab-wide clear is simpler and safer than tracking the exact
    /// layers affected by a command.
    pub fn mark_birdseye_tab_dirty(&mut self, tab_id: u64) {
        self.birds_eye.thumbnails.retain(|(t, _), _| *t != tab_id);
        self.birds_eye
            .dirty_thumbnails
            .retain(|(t, _)| *t != tab_id);
    }

    /// Remove all Bird's Eye cache state for a closed/removed tab id.
    pub fn drop_birdseye_tab_cache(&mut self, tab_id: u64) {
        self.mark_birdseye_tab_dirty(tab_id);
    }

    /// Clear any in-progress Bird's Eye drag and its hover target.
    pub fn clear_birdseye_drag(&mut self) {
        self.birds_eye.drag = None;
        self.birds_eye.hover = None;
    }

    /// Borrow the active document mutably.
    pub fn doc_mut(&mut self) -> &mut ogre_core::Document {
        &mut self.current_tab_mut().doc
    }

    /// Borrow the active document's history.
    pub fn history(&self) -> &ogre_core::History {
        &self.current_tab().history
    }

    /// Borrow the active document's history mutably.
    pub fn history_mut(&mut self) -> &mut ogre_core::History {
        &mut self.current_tab_mut().history
    }

    /// Borrow the active document's viewport.
    pub fn viewport(&self) -> &ogre_gpu::Viewport {
        &self.current_tab().viewport
    }

    /// Borrow the active document's viewport mutably.
    pub fn viewport_mut(&mut self) -> &mut ogre_gpu::Viewport {
        &mut self.current_tab_mut().viewport
    }

    /// Whether any tab has unsaved changes.
    pub fn any_unsaved(&self) -> bool {
        self.tabs.iter().any(|t| t.unsaved)
    }

    /// Mark the active tab as having unsaved changes.
    pub fn mark_current_unsaved(&mut self) {
        self.current_tab_mut().unsaved = true;
    }

    /// Allocate a fresh stable tab ID.
    pub(crate) fn new_tab_id(&mut self) -> u64 {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        id
    }

    /// Close the tab with the given stable ID.
    pub(crate) fn close_tab_by_id(&mut self, id: u64) {
        if let Some(idx) = self.tabs.iter().position(|t| t.id == id) {
            self.close_tab(idx);
        }
    }

    /// Close the tab at the given index, prompting if it has unsaved changes.
    pub fn close_tab(&mut self, idx: usize) {
        if idx >= self.tabs.len() {
            return;
        }
        if self.tabs[idx].unsaved {
            self.active_tab = idx;
            self.pending_close_tab_id = Some(self.tabs[idx].id);
            self.close_prompt = true;
            return;
        }
        self.remove_tab(idx);
    }

    fn remove_tab(&mut self, idx: usize) {
        if self.tabs.len() > 1 {
            let removed_id = self.tabs[idx].id;
            self.tabs.remove(idx);
            self.drop_birdseye_tab_cache(removed_id);
            if self.active_tab > idx {
                self.active_tab -= 1;
            } else if self.active_tab == idx && self.active_tab >= self.tabs.len() {
                self.active_tab = self.tabs.len() - 1;
            }
            self.renderer_needs_clear = true;
            self.dirty = true;
        } else {
            let id = self.tabs[0].id;
            self.drop_birdseye_tab_cache(id);
            self.tabs[0] = DocumentTab::new_blank(id, self.preferences.default_canvas);
            self.welcome = true;
            self.tool_manager = ToolManager::default();
            self.dirty = true;
            self.renderer_needs_clear = true;
            self.file_dialog = None;
            self.file_dialog_feedback.clear();
        }
    }

    /// Reset to a blank document of the given size while preserving user
    /// settings (keymap, preferences, autosave, plugin manager, grid options).
    pub fn new_blank_document(&mut self, size: (u32, u32)) {
        let reuse = {
            let tab = self.current_tab();
            tab.last_save_path.is_none() && !tab.unsaved && tab.history.undo_len() == 0
        };

        if reuse {
            let id = self.current_tab().id;
            self.drop_birdseye_tab_cache(id);
            *self.current_tab_mut() = DocumentTab::new_blank(id, size);
        } else {
            let id = self.new_tab_id();
            self.tabs.push(DocumentTab::new_blank(id, size));
            self.active_tab = self.tabs.len() - 1;
        }

        self.welcome = false;
        self.view = AppView::Editor;
        self.tool_manager = ToolManager::default();
        self.dirty = true;
        self.renderer_needs_clear = true;
        self.file_dialog = None;
        self.file_dialog_feedback.clear();
        self.close_prompt = false;
        self.close_after_save = false;
        self.quit_confirm = false;
        self.paste_prompt = None;
        self.bg_removal_rx = None;
        self.bg_removal_layer = None;
        self.bg_removal_started = None;
        self.bg_removal_ai = false;
        self.update_check = UpdateCheck::Idle;
    }

    /// Duplicate the current document into a new tab: clones the `Document`,
    /// starts a fresh `History`, and clears the save/import paths so the copy
    /// is untitled and cannot overwrite the source. The original tab is
    /// unaffected.
    pub fn duplicate_current_tab(&mut self) {
        let id = self.new_tab_id();
        let doc = self.current_tab().doc.clone();
        let viewport = self.current_tab().viewport;
        let tab = DocumentTab {
            id,
            doc,
            history: ogre_core::History::new(DEFAULT_UNDO_LIMIT),
            viewport,
            last_save_path: None,
            import_path: None,
            unsaved: true,
        };
        self.tabs.push(tab);
        self.active_tab = self.tabs.len() - 1;
        self.welcome = false;
        self.view = AppView::Editor;
        self.dirty = true;
        self.renderer_needs_clear = true;
    }

    /// Close the current document and return to the welcome screen, discarding
    /// the in-memory document. User settings (keymap, preferences, plugins) are
    /// preserved. Callers prompt to save first when [`dirty`](Self::dirty).
    pub fn close_document(&mut self) {
        self.close_prompt = false;
        self.close_after_save = false;
        if self.tabs.len() > 1 {
            let removed_id = self.tabs[self.active_tab].id;
            self.tabs.remove(self.active_tab);
            self.drop_birdseye_tab_cache(removed_id);
            if self.active_tab >= self.tabs.len() {
                self.active_tab = self.tabs.len() - 1;
            }
        } else {
            let id = self.tabs[0].id;
            self.drop_birdseye_tab_cache(id);
            self.tabs[0] = DocumentTab::new_blank(id, self.preferences.default_canvas);
            self.welcome = true;
        }
        self.tool_manager = ToolManager::default();
        self.view = AppView::Editor;
        self.dirty = false;
        self.renderer_needs_clear = true;
        self.file_dialog = None;
        self.file_dialog_feedback.clear();
    }

    /// Whether any background operation is in flight (plugin, I/O, or
    /// background removal). The single source of truth for gating edits, undo,
    /// redo, and busy-sensitive menu items.
    pub fn is_busy(&self) -> bool {
        self.plugin_busy || self.io_busy || self.bg_removal_rx.is_some()
    }

    /// Discard any in-flight background-removal job so its result is dropped.
    ///
    /// Called whenever the document is replaced: the worker thread can't be
    /// stopped, but dropping the receiver means its result is never applied
    /// (which would otherwise overwrite the *new* document, since layer ids are
    /// allocated per-`Document` and collide across fresh documents).
    fn cancel_background_removal(&mut self) {
        self.bg_removal_rx = None;
        self.bg_removal_layer = None;
        self.bg_removal_started = None;
        self.bg_removal_ai = false;
    }

    /// Reset busy state and surface user-facing feedback if a worker thread has
    /// died. Called once per frame after polling worker results.
    pub fn check_worker_health(&mut self) {
        if !self.io_worker.is_alive() && self.io_busy {
            self.io_busy = false;
            self.io_expected.clear();
            self.io_status_feedback =
                "I/O worker stopped unexpectedly; save/open actions are unavailable.".to_string();
            self.file_dialog_feedback.clear();
        }
        if !self.plugin_worker.is_alive() && self.plugin_busy {
            self.plugin_busy = false;
            self.plugin_expected_id = None;
            self.plugin_manager_feedback =
                "Plugin worker stopped unexpectedly; plugin actions are unavailable.".to_string();
        }
    }

    /// Adopt a document recovered from a crash snapshot as the active document,
    /// with a fresh history. Recovery files are left on disk until a clean exit
    /// so a second crash can still recover; `dirty` is `true` because the
    /// recovered work has no saved file backing it.
    pub fn adopt_recovered(&mut self, doc: ogre_core::Document) {
        self.cancel_background_removal();
        let idx = self.active_tab;
        self.tabs[idx].doc = doc;
        self.tabs[idx].history = ogre_core::History::new(DEFAULT_UNDO_LIMIT);
        self.tool_manager = ToolManager::default();
        self.tabs[idx].viewport = ogre_gpu::Viewport::new(Vec2::ZERO, 1.0);
        self.dirty = true;
        // Recovered work has no saved file backing it.
        self.tabs[idx].unsaved = true;
        self.welcome = false;
        self.view = AppView::Editor;
        self.tabs[idx].last_save_path = None;
        self.renderer_needs_clear = true;
        let tab_id = self.tabs[idx].id;
        self.drop_birdseye_tab_cache(tab_id);
    }

    /// Apply a completed plugin result to the application state.
    ///
    /// WASM filter results are pushed directly onto `history` so the prior
    /// undo history is preserved. Lua scripts return a replacement document and
    /// history because they may allocate new layers; their effect stays
    /// undoable as one group, but earlier history is dropped.
    ///
    /// Results whose ID does not match the most recently enqueued request are
    /// considered stale and are ignored.
    pub fn apply_plugin_result(&mut self, result: PluginResult) {
        let result_id = match &result {
            PluginResult::OkCommand { id, .. }
            | PluginResult::OkDocument { id, .. }
            | PluginResult::Err { id, .. } => *id,
        };
        if self.plugin_expected_id != Some(result_id) {
            return;
        }
        self.plugin_busy = false;
        self.plugin_expected_id = None;

        match result {
            PluginResult::OkCommand { label, command, .. } => {
                self.plugin_manager_feedback.clear();
                if let Some(cmd) = command {
                    // Plugin results must apply even when user-triggered I/O is
                    // in flight, so we push directly onto history rather than
                    // going through the busy-gated dispatch path.
                    let tab_id = self.tabs[self.active_tab].id;
                    let tab = &mut self.tabs[self.active_tab];
                    let applied = match tab.history.do_command(&mut tab.doc, cmd) {
                        Err(e) => {
                            self.plugin_manager_feedback = format!("{label} failed: {e}");
                            false
                        }
                        Ok(()) => {
                            self.dirty = true;
                            tab.unsaved = true;
                            true
                        }
                    };
                    if applied {
                        self.mark_birdseye_tab_dirty(tab_id);
                    }
                }
            }
            PluginResult::OkDocument {
                mut doc,
                mut history,
                ..
            } => {
                // A document replacement invalidates any pending I/O result and
                // any in-flight background-removal job.
                self.io_busy = false;
                self.io_expected.clear();
                self.io_status_feedback.clear();
                self.cancel_background_removal();

                let idx = self.active_tab;
                std::mem::swap(&mut self.tabs[idx].doc, &mut doc);
                std::mem::swap(&mut self.tabs[idx].history, &mut history);
                self.tabs[idx].unsaved = true;
                self.tool_manager = ToolManager::default();
                self.dirty = true;
                self.plugin_manager_feedback.clear();
                self.renderer_needs_clear = true;
                let tab_id = self.tabs[idx].id;
                self.drop_birdseye_tab_cache(tab_id);
            }
            PluginResult::Err { message, .. } => {
                self.plugin_manager_feedback = message;
            }
        }
    }

    /// Apply a completed I/O result to the application state.
    ///
    /// On success this updates `doc`, `history`, `last_save_path`, recent files,
    /// and feedback. On error it stores the message in `file_dialog_feedback`
    /// and removes the path from recents for failed opens.
    ///
    /// Results whose ID does not match the expected ID for their kind are
    /// considered stale and are ignored.
    pub fn apply_io_result(&mut self, result: IoResult) {
        let (result_id, kind) = match &result {
            IoResult::SaveOk { id, .. } => (*id, IoKind::Save),
            IoResult::ExportOk { id, .. } => (*id, IoKind::Export),
            IoResult::OpenOk { id, .. } => (*id, IoKind::Open),
            IoResult::AddAsLayerOk { id, .. } => (*id, IoKind::AddAsLayer),
            IoResult::Err { id, kind, .. } => (*id, *kind),
        };
        if self.io_expected.get(&kind) != Some(&result_id) {
            return;
        }
        self.io_expected.remove(&kind);
        self.io_busy = !self.io_expected.is_empty();

        match result {
            IoResult::SaveOk { path, .. } => {
                if let Some(target_id) = self.pending_save_tab_id.take() {
                    if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == target_id) {
                        tab.last_save_path = Some(path.clone());
                        tab.unsaved = false;
                    }
                } else {
                    let tab = self.current_tab_mut();
                    tab.last_save_path = Some(path.clone());
                    tab.unsaved = false;
                }
                self.file_dialog = None;
                self.file_dialog_feedback.clear();
                self.io_status_feedback.clear();

                if self.close_after_save {
                    if let Some(target_id) = self.pending_close_tab_id.take() {
                        self.close_tab_by_id(target_id);
                    } else {
                        self.close_document();
                    }
                    self.close_after_save = false;
                }
            }
            IoResult::ExportOk { path, .. } => {
                self.file_dialog = None;
                self.file_dialog_feedback.clear();
                self.io_status_feedback = format!("Exported {}", path.display());
                // Treat an export as a save for the current tab so the unsaved
                // indicator is cleared and the tab reflects the exported path.
                let tab = self.current_tab_mut();
                tab.unsaved = false;
                tab.import_path = Some(path.clone());
            }
            IoResult::OpenOk { doc, path, .. } => {
                if let Some(target_id) = self.pending_open_tab_id.take() {
                    if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == target_id) {
                        tab.doc = doc;
                        tab.history = ogre_core::History::new(DEFAULT_UNDO_LIMIT);
                        tab.viewport = ogre_gpu::Viewport::new(Vec2::ZERO, 1.0);
                        tab.unsaved = false;
                        tab.import_path = Some(path.clone());
                        if path.extension().and_then(|e| e.to_str()) == Some("ogre") {
                            tab.last_save_path = Some(path.clone());
                        } else {
                            tab.last_save_path = None;
                        }
                        self.active_tab = self
                            .tabs
                            .iter()
                            .position(|t| t.id == target_id)
                            .unwrap_or(self.active_tab);
                    }
                } else {
                    let tab = self.current_tab_mut();
                    tab.doc = doc;
                    tab.history = ogre_core::History::new(DEFAULT_UNDO_LIMIT);
                    tab.viewport = ogre_gpu::Viewport::new(Vec2::ZERO, 1.0);
                    tab.unsaved = false;
                    tab.import_path = Some(path.clone());
                    if path.extension().and_then(|e| e.to_str()) == Some("ogre") {
                        tab.last_save_path = Some(path.clone());
                    } else {
                        tab.last_save_path = None;
                    }
                }
                let opened_tab_id = self.tabs[self.active_tab].id;
                self.drop_birdseye_tab_cache(opened_tab_id);
                self.welcome = false;
                self.tool_manager = ToolManager::default();
                self.dirty = true;
                self.renderer_needs_clear = true;
                self.file_dialog = None;
                self.file_dialog_feedback.clear();
                self.close_after_save = false;
                self.io_status_feedback = format!("Opened {}", path.display());
                let path_str = path.to_string_lossy().to_string();
                crate::prefs::push_recent(&mut self.preferences.recent_files, path_str);
                if let Some(p) = crate::prefs::Preferences::config_path() {
                    let _ = self.preferences.save(&p);
                }
            }
            IoResult::AddAsLayerOk { doc, path, .. } => {
                // Extract the imported image as pixels and route through the
                // same resize/paste flow used for clipboard drops.
                let Some(layer_id) = doc.active else {
                    self.io_status_feedback =
                        format!("Could not add {}: no importable layer.", path.display());
                    return;
                };
                let Ok(layer) = doc.layer(layer_id) else {
                    self.io_status_feedback = format!(
                        "Could not add {}: importable layer missing.",
                        path.display()
                    );
                    return;
                };
                let Some(buffer) = layer.buffer() else {
                    self.io_status_feedback =
                        format!("Could not add {}: not a raster layer.", path.display());
                    return;
                };
                let (w, h) = (doc.canvas.w, doc.canvas.h);
                let pixels = buffer.read_rect(ogre_core::Rect::new(0, 0, w, h));
                crate::paste::add_image(self, w, h, pixels);
                let tab_id = self.current_tab().id;
                self.mark_birdseye_tab_dirty(tab_id);
                self.io_status_feedback = format!("Added {} as layer.", path.display());
                self.welcome = false;
            }
            IoResult::Err {
                kind: IoKind::Open,
                path,
                message,
                ..
            } => {
                if self.file_dialog.is_some() {
                    self.file_dialog_feedback = message.clone();
                    self.io_status_feedback.clear();
                } else {
                    self.io_status_feedback = message.clone();
                    self.file_dialog_feedback.clear();
                }
                let path_str = path.to_string_lossy().to_string();
                self.preferences.recent_files.retain(|p| p != &path_str);
                if let Some(p) = crate::prefs::Preferences::config_path() {
                    let _ = self.preferences.save(&p);
                }
                if let Some(target_id) = self.pending_open_tab_id.take() {
                    if let Some(pos) = self.tabs.iter().position(|t| t.id == target_id) {
                        let previous = self.active_tab;
                        self.tabs.remove(pos);
                        if self.tabs.is_empty() {
                            let id = self.new_tab_id();
                            self.tabs
                                .push(DocumentTab::new_blank(id, self.preferences.default_canvas));
                            self.active_tab = 0;
                            self.welcome = true;
                        } else {
                            self.active_tab = previous.min(self.tabs.len() - 1);
                            if self.active_tab >= pos {
                                self.active_tab = self.active_tab.saturating_sub(1);
                            }
                        }
                    }
                }
            }
            IoResult::Err {
                kind: IoKind::Export,
                message,
                ..
            } => {
                self.file_dialog_feedback.clear();
                self.io_status_feedback = message;
            }
            IoResult::Err {
                kind: IoKind::AddAsLayer,
                path,
                message,
                ..
            } => {
                self.io_status_feedback =
                    format!("Could not add {} as layer: {message}", path.display());
            }
            IoResult::Err { message, .. } => {
                // A failed save aborts any pending close so work isn't lost.
                self.close_after_save = false;
                if self.file_dialog.is_some() {
                    self.file_dialog_feedback = message.clone();
                    self.io_status_feedback.clear();
                } else {
                    self.io_status_feedback = message;
                    self.file_dialog_feedback.clear();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_view_defaults_to_editor() {
        let st = AppState::new_document(64, 64);
        assert_eq!(st.view, super::AppView::Editor);
        assert!(!st.show_about);
    }

    #[test]
    fn close_document_returns_to_welcome_and_clears_dirty() {
        let mut st = AppState::new_document(64, 64);
        st.new_blank_document((64, 64)); // leave the welcome screen
        st.dirty = true;
        st.close_document();
        assert!(st.welcome);
        assert!(!st.dirty);
        assert!(st.tabs[0].last_save_path.is_none());
    }

    #[test]
    fn close_document_clears_close_prompt_and_close_after_save() {
        let mut st = AppState::new_document(64, 64);
        st.new_blank_document((64, 64));
        st.close_prompt = true;
        st.close_after_save = true;
        st.close_document();
        assert!(!st.close_prompt);
        assert!(!st.close_after_save);
    }

    #[test]
    fn successful_save_with_close_after_save_closes_document() {
        let mut st = AppState::new_document(64, 64);
        st.new_blank_document((64, 64));
        st.close_after_save = true;
        let id = st.io_worker.next_id();
        st.io_expected.insert(IoKind::Save, id);
        st.apply_io_result(IoResult::SaveOk {
            id,
            path: std::path::PathBuf::from("/tmp/qa-verify.ogre"),
        });
        assert!(st.welcome, "save should have closed the document");
        assert!(!st.close_after_save);
    }

    #[test]
    fn clamp_zoom_keeps_within_range_and_resets() {
        assert_eq!(super::clamp_zoom(0.0), 0.1);
        assert_eq!(super::clamp_zoom(100.0), 8.0);
        assert!((super::clamp_zoom(2.5) - 2.5).abs() < 1e-6);
    }

    #[test]
    fn show_zoom_popup_defaults_false() {
        let st = AppState::new_document(64, 64);
        assert!(!st.show_zoom_popup);
    }

    #[test]
    fn adopt_recovered_replaces_document_and_leaves_welcome() {
        let mut st = AppState::new_document(64, 64);
        st.welcome = true;
        let mut doc = ogre_core::Document::new(128, 256);
        doc.add_raster_layer("recovered");
        st.adopt_recovered(doc);
        assert_eq!(
            (st.tabs[0].doc.canvas.w, st.tabs[0].doc.canvas.h),
            (128, 256)
        );
        assert!(!st.welcome);
        assert!(st.dirty);
        assert_eq!(st.view, AppView::Editor);
        assert_eq!(st.tabs[0].history.undo_len(), 0);
    }

    #[test]
    fn new_document_initialises_state() {
        let state = AppState::new_document(1920, 1080);
        assert_eq!(state.tabs.len(), 1);
        assert_eq!(state.active_tab, 0);
        assert_eq!(state.tabs[0].doc.order.len(), 1);
        assert_eq!(
            state.tabs[0]
                .doc
                .layer(state.tabs[0].doc.order[0])
                .unwrap()
                .name,
            "Background"
        );
        assert_eq!(state.tabs[0].history.undo_len(), 0);
        assert_eq!(state.tabs[0].history.redo_len(), 0);
        assert_eq!(state.tool_manager.active(), state.preferences.active_tool);
        assert_eq!(
            state.tabs[0].viewport,
            ogre_gpu::Viewport::new(Vec2::ZERO, 1.0)
        );
        assert!(state.dirty);
    }

    #[test]
    fn new_document_uses_finite_undo_limit() {
        let state = AppState::new_document(1920, 1080);
        assert_eq!(state.tabs[0].history.limit(), DEFAULT_UNDO_LIMIT);
        assert_ne!(state.tabs[0].history.limit(), 0, "0 means unlimited undo");
    }

    #[test]
    fn new_blank_document_uses_finite_undo_limit() {
        let mut state = AppState::new_document(1920, 1080);
        state.new_blank_document((800, 600));
        assert_eq!(state.tabs[0].history.limit(), DEFAULT_UNDO_LIMIT);
        assert_ne!(state.tabs[0].history.limit(), 0, "0 means unlimited undo");
    }

    #[test]
    fn new_blank_document_resets_canvas_and_preserves_settings() {
        let mut state = AppState::new_document(1920, 1080);
        let original_preferences = state.preferences.clone();
        let original_grid_spacing = state.grid_spacing;

        // Mutate some state that should be reset.
        state.tabs[0].doc.add_raster_layer("Extra");
        state.tabs[0].viewport.zoom = 2.0;
        state.tabs[0].last_save_path = Some(PathBuf::from("/tmp/test.ogre"));
        state.file_dialog_feedback = "error".to_string();

        state.new_blank_document((800, 600));

        let tab = state.current_tab();
        assert_eq!(tab.doc.canvas.w, 800);
        assert_eq!(tab.doc.canvas.h, 600);
        assert_eq!(tab.doc.order.len(), 1);
        assert_eq!(tab.doc.layer(tab.doc.order[0]).unwrap().name, "Background");
        assert_eq!(tab.viewport.zoom, 1.0);
        assert!(state.dirty);
        assert!(tab.last_save_path.is_none());
        assert!(state.file_dialog_feedback.is_empty());

        // Preserved settings.
        assert_eq!(state.preferences, original_preferences);
        assert_eq!(state.grid_spacing, original_grid_spacing);
    }

    #[test]
    fn welcome_defaults_true_and_clears_on_new_blank() {
        let mut st = AppState::new_document(64, 64);
        assert!(st.welcome);
        st.new_blank_document((100, 100));
        assert!(!st.welcome);
    }

    #[test]
    fn new_blank_document_resets_view_to_editor() {
        let mut st = AppState::new_document(64, 64);
        st.view = AppView::Settings;
        st.new_blank_document((100, 100));
        assert_eq!(
            st.view,
            AppView::Editor,
            "view must return to Editor on new_blank_document"
        );
    }

    #[test]
    fn io_worker_death_clears_busy_and_expected() {
        let mut st = AppState::new_document(64, 64);
        st.io_busy = true;
        st.io_expected.insert(IoKind::Save, 7);
        st.io_worker.kill();
        loop {
            if !st.io_worker.is_alive() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        st.check_worker_health();
        assert!(!st.io_busy);
        assert!(st.io_expected.is_empty());
        assert!(!st.io_status_feedback.is_empty());
    }

    #[test]
    fn plugin_worker_death_clears_busy_and_expected() {
        let mut st = AppState::new_document(64, 64);
        st.plugin_busy = true;
        st.plugin_expected_id = Some(3);
        st.plugin_worker.kill();
        loop {
            if !st.plugin_worker.is_alive() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        st.check_worker_health();
        assert!(!st.plugin_busy);
        assert!(st.plugin_expected_id.is_none());
        assert!(!st.plugin_manager_feedback.is_empty());
    }

    #[test]
    fn add_as_layer_ok_adds_imported_image_as_layer() {
        let mut st = AppState::new_document(64, 64);
        st.welcome = false;
        let mut imported = ogre_core::Document::new(8, 8);
        imported.add_raster_layer("imported");
        let id = st.io_worker.next_id();
        st.io_expected.insert(IoKind::AddAsLayer, id);
        st.apply_io_result(IoResult::AddAsLayerOk {
            id,
            doc: imported,
            path: std::path::PathBuf::from("/tmp/test.png"),
        });
        assert_eq!(st.tabs[0].doc.order.len(), 2);
        assert!(!st.welcome);
    }

    #[test]
    fn new_blank_document_requests_renderer_clear() {
        let mut st = AppState::new_document(64, 64);
        st.renderer_needs_clear = false;
        st.new_blank_document((32, 32));
        assert!(st.renderer_needs_clear);
    }

    #[test]
    fn close_document_requests_renderer_clear() {
        let mut st = AppState::new_document(64, 64);
        st.renderer_needs_clear = false;
        st.close_document();
        assert!(st.renderer_needs_clear);
    }

    #[test]
    fn adopt_recovered_requests_renderer_clear() {
        let mut st = AppState::new_document(64, 64);
        st.renderer_needs_clear = false;
        st.adopt_recovered(ogre_core::Document::new(32, 32));
        assert!(st.renderer_needs_clear);
    }

    #[test]
    fn apply_io_open_ok_requests_renderer_clear() {
        let mut st = AppState::new_document(64, 64);
        st.renderer_needs_clear = false;
        let id = st.io_worker.next_id();
        st.io_expected.insert(IoKind::Open, id);
        st.apply_io_result(IoResult::OpenOk {
            id,
            doc: ogre_core::Document::new(32, 32),
            path: std::path::PathBuf::from("/tmp/test.ogre"),
        });
        assert!(st.renderer_needs_clear);
    }

    #[test]
    fn apply_plugin_ok_document_requests_renderer_clear() {
        let mut st = AppState::new_document(64, 64);
        st.renderer_needs_clear = false;
        let id = 7;
        st.plugin_expected_id = Some(id);
        st.apply_plugin_result(PluginResult::OkDocument {
            id,
            label: "test".to_string(),
            doc: ogre_core::Document::new(32, 32),
            history: ogre_core::History::new(DEFAULT_UNDO_LIMIT),
        });
        assert!(st.renderer_needs_clear);
    }
}
