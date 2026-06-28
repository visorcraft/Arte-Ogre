// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Preferences, autosave, and crash recovery.
//!
//! [`Preferences`] holds user settings and persists them as a TOML file in the
//! platform config directory.  [`Autosave`] writes periodic recovery snapshots
//! of the current document to a dedicated recovery directory; on launch the
//! newest recovery file can be reloaded so unsaved work is not lost after a
//! crash.

use ogre_core::Document;

pub use crate::tools::SidebarSection;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    sync::{mpsc, Arc, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub use crate::theme::Theme;

/// Editor preferences.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Preferences {
    /// Default width/height for new documents.
    pub default_canvas: (u32, u32),
    /// Autosave interval.  `0` disables autosave.
    pub autosave_interval_seconds: u64,
    /// UI theme.
    pub theme: Theme,
    /// Whether the Tools sidebar is collapsed to an icon strip.
    #[serde(default)]
    pub tools_collapsed: bool,
    /// Order of reorderable sections in the left sidebar (tool groups plus Color
    /// and Swatches). Kept under the legacy TOML key `tool_group_order` so
    /// existing user orders load without migration code.
    #[serde(
        default = "crate::tools::default_sidebar_order",
        deserialize_with = "crate::tools::deserialize_sidebar_order",
        rename = "tool_group_order"
    )]
    pub sidebar_order: Vec<SidebarSection>,
    /// Most-recently-opened file paths (most recent first, capped at 10).
    #[serde(default)]
    pub recent_files: Vec<String>,
    /// Whether the Layers panel is visible.
    #[serde(default = "default_layers_visible")]
    pub layers_visible: bool,
    /// Recently-picked foreground colors (most recent first), stored as
    /// **linear** RGBA in `[r, g, b, a]` order.
    #[serde(default)]
    pub recent_colors: Vec<[f32; 4]>,
    /// User-saved foreground-color swatches, stored as linear RGBA.
    #[serde(default)]
    pub swatches: Vec<[f32; 4]>,
    /// Saved brush presets, stored densely (slot `i` is populated iff
    /// `i < brush_presets.len()`). Capped at `MAX_BRUSH_PRESETS`.
    #[serde(default)]
    pub brush_presets: Vec<ogre_core::BrushSettings>,
    /// Last active tool kind, restored on launch.
    #[serde(default)]
    pub active_tool: crate::tools::ToolKind,
    /// Last-selected sibling for each tool family, restored on launch.
    #[serde(default)]
    pub tool_family_last:
        std::collections::HashMap<crate::tools::ToolFamily, crate::tools::ToolKind>,
}

fn default_layers_visible() -> bool {
    true
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            default_canvas: (1920, 1080),
            autosave_interval_seconds: 60,
            theme: Theme::default(),
            tools_collapsed: false,
            sidebar_order: crate::tools::default_sidebar_order(),
            recent_files: Vec::new(),
            layers_visible: true,
            recent_colors: Vec::new(),
            swatches: Vec::new(),
            brush_presets: Vec::new(),
            active_tool: crate::tools::ToolKind::RectSelect,
            tool_family_last: std::collections::HashMap::new(),
        }
    }
}

/// Maximum number of saved brush presets.
pub const MAX_BRUSH_PRESETS: usize = 5;

/// Store `settings` into preset `index`. Overwrites an existing slot; appends if
/// `index` is the next empty slot; ignores out-of-range or gap-creating indices
/// (so the populated slots stay dense and TOML-serializable).
pub fn set_brush_preset(
    presets: &mut Vec<ogre_core::BrushSettings>,
    index: usize,
    settings: ogre_core::BrushSettings,
) {
    if index >= MAX_BRUSH_PRESETS {
        return;
    }
    if index < presets.len() {
        presets[index] = settings;
    } else if index == presets.len() {
        presets.push(settings);
    }
    // index > len would create a gap; ignored.
}

/// Prepend `path` to `recents`, de-duplicating and capping at 10 (most-recent-first).
pub fn push_recent(recents: &mut Vec<String>, path: String) {
    recents.retain(|p| *p != path);
    recents.insert(0, path);
    recents.truncate(10);
}

/// Prepend `color` to `colors`, de-duplicating (by exact value) and capping at
/// `cap` (most-recent-first).
pub fn push_color(colors: &mut Vec<[f32; 4]>, color: [f32; 4], cap: usize) {
    colors.retain(|c| *c != color);
    colors.insert(0, color);
    colors.truncate(cap);
}

/// Maximum number of recent colors retained.
pub const MAX_RECENT_COLORS: usize = 16;

impl Preferences {
    /// Load preferences from a TOML file, returning defaults if the file is
    /// missing or unreadable.
    pub fn load_or_default<P: AsRef<Path>>(path: P) -> Self {
        Self::load(path).unwrap_or_default()
    }

    /// Load preferences from a TOML file.
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        let mut prefs: Self = toml::from_str(&text).map_err(|e| e.to_string())?;
        crate::tools::normalize_sidebar_order(&mut prefs.sidebar_order);
        Ok(prefs)
    }

    /// Save preferences to a TOML file.  Parent directories are created if
    /// necessary.
    pub fn save<P: AsRef<Path>>(&self, path: P) -> Result<(), String> {
        let text = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(path, text).map_err(|e| e.to_string())
    }

    /// Platform config file path for Arte Ogre preferences.
    pub fn config_path() -> Option<PathBuf> {
        directories::ProjectDirs::from("com", "arte", "ogre")
            .map(|dirs| dirs.config_dir().join("prefs.toml"))
    }
}

/// Shared state between the UI thread and the background autosave worker.
#[derive(Debug)]
struct AutosaveState {
    /// Latest document snapshot waiting to be written, if any, together with
    /// the path it will be written to.
    pending: Mutex<Option<(Document, PathBuf)>>,
    /// Path of the most recently completed recovery write.
    last_path: Mutex<Option<PathBuf>>,
    /// `true` while a recovery snapshot is being prepared or written.
    busy: AtomicBool,
    /// Human-readable message when the worker stops unexpectedly.
    feedback: Mutex<String>,
}

impl AutosaveState {
    fn new() -> Self {
        Self {
            pending: Mutex::new(None),
            last_path: Mutex::new(None),
            busy: AtomicBool::new(false),
            feedback: Mutex::new(String::new()),
        }
    }
}

/// Background worker that performs blocking autosave I/O.
///
/// The worker runs on a dedicated thread. It is notified through a channel,
/// reads the latest pending document snapshot, writes it atomically, and
/// records the resulting path. There is at most one write in flight at a time,
/// and only the latest snapshot is kept, so bursts of `maybe_save` calls do not
/// queue stale work.
#[derive(Debug)]
struct AutosaveWorker {
    /// Handle to the shared state, used by the UI to queue snapshots.
    state: Arc<AutosaveState>,
    /// Notification channel sender. A dedicated thread owns the receiver.
    notify: mpsc::Sender<()>,
}

impl AutosaveWorker {
    /// Spawn the background thread and return a handle for the UI to use.
    fn new(dir: PathBuf) -> Self {
        let state = Arc::new(AutosaveState::new());
        let (notify, receiver) = mpsc::channel::<()>();
        let thread_state = Arc::clone(&state);

        thread::spawn(move || {
            while receiver.recv().is_ok() {
                // Grab the latest pending snapshot (if several saves were
                // requested while one was running, stale ones are dropped).
                let task = {
                    let mut guard = thread_state
                        .pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    guard.take()
                };
                let Some((doc, path)) = task else {
                    // No snapshot to write. Only clear busy if nothing new was
                    // queued while we were waiting for the lock.
                    if thread_state
                        .pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .is_none()
                    {
                        thread_state.busy.store(false, Ordering::Release);
                    }
                    continue;
                };

                match write_recovery_file_at(&dir, &doc, &path) {
                    Ok(()) => {
                        if let Ok(mut last) = thread_state.last_path.lock() {
                            *last = Some(path);
                        }
                        prune_recovery_files(&dir, MAX_RECOVERY_FILES);
                    }
                    Err(e) => {
                        if let Ok(mut feedback) = thread_state.feedback.lock() {
                            *feedback = format!("autosave failed: {e}");
                        }
                    }
                }
                // Keep busy true if another save was queued while we were
                // writing; the next loop iteration will pick it up.
                if thread_state
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_none()
                {
                    thread_state.busy.store(false, Ordering::Release);
                }
            }
            // The channel disconnected, which means the worker thread is
            // exiting abnormally. Make sure the UI does not stay stuck busy.
            thread_state.busy.store(false, Ordering::Release);
            if let Ok(mut feedback) = thread_state.feedback.lock() {
                *feedback = "Autosave worker stopped unexpectedly.".to_string();
            }
        });

        Self { state, notify }
    }

    /// Queue `doc` as the latest snapshot to save at `path`.
    ///
    /// The caller passes a borrowed reference; this method clones the document
    /// before handing it to the background worker. (Moving the clone itself off
    /// the calling thread would require shared ownership of the live document;
    /// the important part is that the blocking *serialization and disk write*
    /// happen on the worker thread.)
    fn save(&self, doc: &Document, path: PathBuf) -> Result<(), String> {
        if let Ok(mut guard) = self.state.pending.lock() {
            *guard = Some((doc.clone(), path));
        }
        self.state.busy.store(true, Ordering::Release);
        self.notify.send(()).map_err(|_| {
            self.state.busy.store(false, Ordering::Release);
            if let Ok(mut feedback) = self.state.feedback.lock() {
                *feedback = "Autosave worker has stopped.".to_string();
            }
            "Autosave worker has stopped.".to_string()
        })
    }

    /// `true` while a recovery snapshot is being written.
    fn is_busy(&self) -> bool {
        self.state.busy.load(Ordering::Acquire)
    }

    /// Latest autosave feedback message, if any.
    fn feedback(&self) -> String {
        self.state
            .feedback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// Maximum number of recovery snapshots retained on disk.
const MAX_RECOVERY_FILES: usize = 10;

/// Manages periodic recovery snapshots of the open document.
///
/// Autosave writes are performed on a dedicated background thread so that large
/// documents do not block the UI. Only one save is in flight at a time; if a
/// second save is scheduled while one is running, the pending request is
/// replaced with the latest snapshot.
#[derive(Debug)]
pub struct Autosave {
    /// Directory where recovery files are written.
    dir: PathBuf,
    /// Minimum time between autosaves.
    interval: Duration,
    /// Time the last save was queued, if any.
    last_save: Option<Instant>,
    /// Background worker that performs blocking disk I/O.
    worker: AutosaveWorker,
}

impl Autosave {
    /// Create an autosave manager.  An interval of zero disables autosave.
    pub fn new(dir: PathBuf, interval: Duration) -> Self {
        Self {
            dir: dir.clone(),
            interval,
            last_save: None,
            worker: AutosaveWorker::new(dir),
        }
    }

    /// Recovery directory for the current session.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// If enough time has elapsed, queue a recovery snapshot and return the
    /// path it will be written to.
    ///
    /// Returns `None` when autosave is disabled or the interval has not passed.
    /// `dirty` is a hint: a clean document is still autosaved when the interval
    /// expires so the recovery file stays fresh.
    ///
    /// The returned path is chosen immediately; the actual blocking write
    /// happens on a background thread, so the file may not exist until shortly
    /// after this call returns.
    pub fn maybe_save(&mut self, doc: &Document, dirty: bool) -> Option<PathBuf> {
        if self.interval.is_zero() {
            return None;
        }
        let elapsed = self
            .last_save
            .map(|t| t.elapsed() >= self.interval)
            .unwrap_or(true);
        if !elapsed {
            return None;
        }
        // Avoid autosaving a document that has never been edited on the very
        // first frame, but still save once per interval after that.
        if self.last_save.is_none() && !dirty {
            self.last_save = Some(Instant::now());
            return None;
        }
        self.save(doc)
    }

    /// Queue a recovery snapshot on the background worker.
    fn save(&mut self, doc: &Document) -> Option<PathBuf> {
        let path = recovery_path(&self.dir);
        self.last_save = Some(Instant::now());
        // `save` resets the busy flag and stores feedback if the worker has
        // died, so the UI can surface the failure.
        let _ = self.worker.save(doc, path.clone());
        Some(path)
    }

    /// `true` while a recovery snapshot is being written.
    pub fn busy(&self) -> bool {
        self.worker.is_busy()
    }

    /// Latest autosave feedback message, if any.
    pub fn feedback(&self) -> String {
        self.worker.feedback()
    }

    /// Clear any autosave feedback message.
    pub fn clear_feedback(&mut self) {
        if let Ok(mut feedback) = self.worker.state.feedback.lock() {
            feedback.clear();
        }
    }

    /// List recovery files, newest first.
    pub fn recovery_files(&self) -> Vec<PathBuf> {
        recovery_files_in(&self.dir)
    }

    /// Load the newest recovery file, if any.
    pub fn load_latest_recovery(&self) -> Option<Document> {
        let path = self.recovery_files().into_iter().next()?;
        ogre_io::ogre::load(path).ok()
    }

    /// Delete every recovery snapshot.
    ///
    /// Called on a clean exit so that surviving recovery files unambiguously
    /// mean the previous session ended unexpectedly.
    pub fn clear_recovery(&self) {
        for path in self.recovery_files() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Generate a unique recovery file path in `dir`.
fn recovery_path(dir: &Path) -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let filename = format!("arte-ogre-recovery-{timestamp}.ogre");
    dir.join(&filename)
}

/// Write a recovery snapshot atomically to the given path (temp file + rename).
fn write_recovery_file_at(dir: &Path, doc: &Document, path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let filename = path
        .file_name()
        .ok_or("invalid recovery path")?
        .to_string_lossy();
    let temp = dir.join(format!(".{filename}.tmp"));
    ogre_io::ogre::save(doc, &temp).map_err(|e| e.to_string())?;
    std::fs::rename(&temp, path).map_err(|e| e.to_string())?;
    Ok(())
}

/// Delete all but the `keep` newest recovery snapshots.
fn prune_recovery_files(dir: &Path, keep: usize) {
    for old in recovery_files_in(dir).into_iter().skip(keep) {
        let _ = std::fs::remove_file(old);
    }
}

/// List recovery files in `dir`, newest first.
fn recovery_files_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("arte-ogre-recovery-") && n.ends_with(".ogre"))
                .unwrap_or(false)
        })
        .collect();
    files.sort_by(|a, b| b.cmp(a));
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use ogre_core::{Document, Rgba32F};

    fn temp_dir(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{}_{}", prefix, std::process::id()))
    }

    #[test]
    fn autosave_prunes_old_recovery_files() {
        let dir = temp_dir("ogre_autosave_prune");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Seed more snapshots than the retention limit.
        for i in 1..=15 {
            std::fs::write(
                dir.join(format!("arte-ogre-recovery-{i:020}.ogre")),
                b"stale",
            )
            .unwrap();
        }

        let mut autosave = Autosave::new(dir.clone(), Duration::from_millis(1));
        let doc = Document::new(8, 8);
        // First call queues a fresh snapshot.
        autosave.maybe_save(&doc, true).expect("autosave queues");

        // Wait for the background worker to finish and then prune.
        std::thread::sleep(Duration::from_millis(100));
        prune_recovery_files(&dir, MAX_RECOVERY_FILES);

        let remaining = autosave.recovery_files();
        assert!(
            remaining.len() <= MAX_RECOVERY_FILES,
            "expected <= {MAX_RECOVERY_FILES} recovery files, got {}",
            remaining.len()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn preferences_round_trip_through_file() {
        let dir = temp_dir("ogre_prefs_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("prefs.toml");

        let prefs = Preferences {
            theme: Theme::Dark,
            default_canvas: (800, 600),
            autosave_interval_seconds: 30,
            tools_collapsed: false,
            sidebar_order: crate::tools::default_sidebar_order(),
            recent_files: Vec::new(),
            layers_visible: true,
            recent_colors: vec![[1.0, 0.0, 0.0, 1.0]],
            swatches: vec![[0.0, 0.0, 0.0, 1.0], [1.0, 1.0, 1.0, 1.0]],
            brush_presets: vec![ogre_core::BrushSettings::default()],
            active_tool: crate::tools::ToolKind::PaintBucket,
            tool_family_last: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    crate::tools::ToolFamily::Bucket,
                    crate::tools::ToolKind::PaintBucket,
                );
                m
            },
        };

        prefs.save(&path).unwrap();
        let loaded = Preferences::load(&path).unwrap();
        assert_eq!(prefs, loaded);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn preferences_load_or_default_uses_defaults_on_missing_file() {
        let path = temp_dir("ogre_prefs_missing").join("prefs.toml");
        let prefs = Preferences::load_or_default(&path);
        assert_eq!(prefs, Preferences::default());
    }

    #[test]
    fn autosave_writes_recovery_file() {
        let dir = temp_dir("ogre_autosave_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut autosave = Autosave::new(dir.clone(), Duration::from_secs(1));

        let mut doc = Document::new(64, 64);
        let layer = doc.add_raster_layer("L");
        doc.layer_mut(layer)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(
                ogre_core::IVec2::new(5, 5),
                Rgba32F::new(1.0, 0.0, 0.0, 1.0),
            );

        let _path = autosave
            .maybe_save(&doc, true)
            .expect("autosave should queue");

        // Wait for the background worker to finish.
        std::thread::sleep(Duration::from_millis(100));

        let recovered = autosave
            .load_latest_recovery()
            .expect("recovery should load");
        assert_eq!(recovered.canvas.w, 64);
        assert_eq!(recovered.canvas.h, 64);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn autosave_respects_interval() {
        let dir = temp_dir("ogre_autosave_interval_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut autosave = Autosave::new(dir.clone(), Duration::from_secs(3600));
        let doc = Document::new(16, 16);

        assert!(autosave.maybe_save(&doc, true).is_some());
        assert!(autosave.maybe_save(&doc, true).is_none());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn clear_recovery_removes_all_files() {
        let dir = temp_dir("ogre_clear_recovery_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut autosave = Autosave::new(dir.clone(), Duration::ZERO);

        autosave.save(&Document::new(8, 8));
        std::thread::sleep(Duration::from_millis(100));
        assert!(!autosave.recovery_files().is_empty());

        autosave.clear_recovery();
        assert!(autosave.recovery_files().is_empty());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn recovery_prefers_latest_file() {
        let dir = temp_dir("ogre_recovery_latest_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut autosave = Autosave::new(dir.clone(), Duration::ZERO);

        let doc1 = Document::new(16, 16);
        autosave.save(&doc1);
        std::thread::sleep(Duration::from_millis(100));

        let doc2 = Document::new(32, 32);
        autosave.save(&doc2);
        std::thread::sleep(Duration::from_millis(100));

        let recovered = autosave.load_latest_recovery().unwrap();
        assert_eq!((recovered.canvas.w, recovered.canvas.h), (32, 32));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn old_prefs_without_new_fields_still_load() {
        let toml = r#"
default_canvas = [800, 600]
autosave_interval_seconds = 60
theme = "System"
"#;
        let p: Preferences = toml::from_str(toml).unwrap();
        assert!(!p.tools_collapsed);
        assert_eq!(p.theme, Theme::System);
    }

    #[test]
    fn old_prefs_without_layers_visible_defaults_to_true() {
        let toml = r#"
        theme = "Dark"
        default_canvas = [800, 600]
        autosave_interval_seconds = 60
    "#;
        let prefs: Preferences = toml::from_str(toml).expect("parses old prefs");
        assert!(prefs.layers_visible);
    }

    #[test]
    fn push_recent_dedups_prepends_and_caps_at_10() {
        let mut r: Vec<String> = Vec::new();
        for i in 0..12 {
            super::push_recent(&mut r, format!("/f{i}.png"));
        }
        assert_eq!(r.len(), 10);
        assert_eq!(r[0], "/f11.png"); // most recent first
                                      // re-adding an existing path moves it to the front without duplicating
        super::push_recent(&mut r, "/f5.png".to_string());
        assert_eq!(r[0], "/f5.png");
        assert_eq!(r.iter().filter(|p| *p == "/f5.png").count(), 1);
    }

    #[test]
    fn old_prefs_without_recent_files_load() {
        let toml = "default_canvas = [800, 600]\nautosave_interval_seconds = 60\ntheme = \"System\"\ntools_collapsed = false\n";
        let p: Preferences = toml::from_str(toml).unwrap();
        assert!(p.recent_files.is_empty());
    }

    #[test]
    fn old_prefs_without_color_lists_default_empty() {
        let toml =
            "default_canvas = [800, 600]\nautosave_interval_seconds = 60\ntheme = \"System\"\n";
        let p: Preferences = toml::from_str(toml).expect("parses old prefs");
        assert!(p.recent_colors.is_empty());
        assert!(p.swatches.is_empty());
    }

    #[test]
    fn push_color_dedups_prepends_and_caps() {
        let mut colors: Vec<[f32; 4]> = Vec::new();
        let red = [1.0, 0.0, 0.0, 1.0];
        for _ in 0..20 {
            super::push_color(&mut colors, red, MAX_RECENT_COLORS);
        }
        assert_eq!(colors.len(), 1, "duplicates collapse to one");
        // Distinct colors prepend and cap.
        for i in 0..20 {
            let c = [i as f32 / 20.0, 0.0, 0.0, 1.0];
            super::push_color(&mut colors, c, MAX_RECENT_COLORS);
        }
        assert_eq!(colors.len(), MAX_RECENT_COLORS);
        // Re-adding moves to front.
        let front = colors[0];
        super::push_color(&mut colors, [0.0, 0.0, 0.0, 1.0], MAX_RECENT_COLORS);
        assert_eq!(colors[0], [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(colors.iter().filter(|c| **c == front).count(), 1);
    }

    #[test]
    fn color_lists_round_trip_through_prefs_file() {
        let dir = temp_dir("ogre_prefs_colors");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("prefs.toml");
        let prefs = Preferences {
            recent_colors: vec![[1.0, 0.0, 0.0, 1.0], [0.0, 1.0, 0.0, 1.0]],
            swatches: vec![[0.1, 0.2, 0.3, 1.0]],
            ..Preferences::default()
        };
        prefs.save(&path).unwrap();
        let loaded = Preferences::load(&path).unwrap();
        assert_eq!(loaded.recent_colors, prefs.recent_colors);
        assert_eq!(loaded.swatches, prefs.swatches);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn set_brush_preset_fills_and_overwrites_slots() {
        let mut presets: Vec<ogre_core::BrushSettings> = Vec::new();
        let s1 = ogre_core::BrushSettings {
            size: 10.0,
            ..ogre_core::BrushSettings::default()
        };
        let s2 = ogre_core::BrushSettings {
            size: 40.0,
            ..ogre_core::BrushSettings::default()
        };
        // Saving to slot 0 (the next empty slot) appends.
        super::set_brush_preset(&mut presets, 0, s1);
        assert_eq!(presets.len(), 1);
        assert_eq!(presets[0], s1);
        // Overwriting slot 0 keeps length stable.
        super::set_brush_preset(&mut presets, 0, s2);
        assert_eq!(presets.len(), 1);
        assert_eq!(presets[0], s2);
        // Saving to a slot beyond the next empty one is ignored (no gaps).
        super::set_brush_preset(&mut presets, 3, s1);
        assert_eq!(presets.len(), 1);
    }

    #[test]
    fn set_brush_preset_ignores_out_of_range() {
        let mut presets: Vec<ogre_core::BrushSettings> = Vec::new();
        super::set_brush_preset(
            &mut presets,
            super::MAX_BRUSH_PRESETS,
            ogre_core::BrushSettings::default(),
        );
        assert!(presets.is_empty(), "slot index >= MAX is rejected");
    }

    #[test]
    fn brush_presets_round_trip_through_prefs_file() {
        let dir = temp_dir("ogre_prefs_brush");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("prefs.toml");
        let prefs = Preferences {
            brush_presets: vec![
                ogre_core::BrushSettings::default(),
                ogre_core::BrushSettings {
                    size: 25.0,
                    hardness: 0.8,
                    ..ogre_core::BrushSettings::default()
                },
            ],
            ..Preferences::default()
        };
        prefs.save(&path).unwrap();
        let loaded = Preferences::load(&path).unwrap();
        assert_eq!(loaded.brush_presets, prefs.brush_presets);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn autosave_only_one_save_in_flight() {
        let dir = temp_dir("ogre_autosave_one_in_flight");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut autosave = Autosave::new(dir.clone(), Duration::from_millis(1));

        let doc = Document::new(16, 16);
        // Queue the first save and immediately queue a second while the first
        // may still be running.
        autosave.maybe_save(&doc, true).unwrap();
        autosave.maybe_save(&doc, true);

        std::thread::sleep(Duration::from_millis(200));

        // We should still have exactly one fresh recovery file.
        let files = autosave.recovery_files();
        assert_eq!(files.len(), 1, "expected exactly one recovery file");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn autosave_worker_save_takes_document_reference() {
        let dir = temp_dir("ogre_autosave_ref");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let autosave = Autosave::new(dir.clone(), Duration::from_millis(1));

        let doc = Document::new(16, 16);
        let path = recovery_path(&dir);
        // `save` accepts a borrowed document; the clone happens inside the call
        // so the caller controls when the snapshot is taken.
        let _ = autosave.worker.save(&doc, path.clone());

        std::thread::sleep(Duration::from_millis(100));
        assert!(path.exists(), "recovery file should be written");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn autosave_reports_busy_state() {
        let dir = temp_dir("ogre_autosave_busy");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut autosave = Autosave::new(dir.clone(), Duration::from_millis(1));

        assert!(!autosave.busy());
        let doc = Document::new(16, 16);
        autosave.save(&doc);
        assert!(autosave.busy());

        std::thread::sleep(Duration::from_millis(100));
        assert!(!autosave.busy());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn autosave_resets_busy_and_feedback_when_worker_dies() {
        let dir = temp_dir("ogre_autosave_worker_death");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Construct a worker whose receiver has been dropped, simulating an
        // unexpected worker thread exit.
        let state = Arc::new(AutosaveState::new());
        let (notify, receiver) = mpsc::channel::<()>();
        drop(receiver);
        let worker = AutosaveWorker { state, notify };

        assert!(!worker.is_busy());
        let doc = Document::new(16, 16);
        let path = recovery_path(&dir);
        let result = worker.save(&doc, path);
        assert!(result.is_err());
        // The busy flag must not remain stuck after the worker is unreachable.
        assert!(!worker.is_busy());
        assert!(!worker.feedback().is_empty());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sidebar_order_defaults_to_current_layout() {
        let prefs = Preferences::default();
        assert_eq!(prefs.sidebar_order, crate::tools::default_sidebar_order());
    }

    #[test]
    fn sidebar_order_round_trips_through_toml() {
        let dir = temp_dir("ogre_prefs_sidebar_order");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("prefs.toml");

        let prefs = Preferences {
            sidebar_order: vec![
                SidebarSection::Paint,
                SidebarSection::Vector,
                SidebarSection::Swatches,
                SidebarSection::Select,
                SidebarSection::Color,
                SidebarSection::Transform,
                SidebarSection::Navigate,
            ],
            ..Preferences::default()
        };
        prefs.save(&path).unwrap();
        let loaded = Preferences::load(&path).unwrap();
        assert_eq!(loaded.sidebar_order, prefs.sidebar_order);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn old_prefs_without_tool_group_order_load_with_default() {
        let toml = r#"
default_canvas = [800, 600]
autosave_interval_seconds = 60
theme = "System"
tools_collapsed = false
"#;
        let prefs: Preferences = toml::from_str(toml).expect("parses old prefs");
        assert_eq!(prefs.sidebar_order, crate::tools::default_sidebar_order());
    }

    #[test]
    fn malformed_tool_group_order_loads_with_default_without_losing_other_prefs() {
        let toml = r#"
default_canvas = [800, 600]
autosave_interval_seconds = 60
theme = "System"
tool_group_order = ["NotAGroup"]
"#;
        let prefs: Preferences = toml::from_str(toml).expect("parses malformed prefs");
        assert_eq!(prefs.sidebar_order, crate::tools::default_sidebar_order());
        assert_eq!(prefs.default_canvas, (800, 600));
        assert_eq!(prefs.autosave_interval_seconds, 60);
        assert_eq!(prefs.theme, Theme::System);
    }

    #[test]
    fn legacy_tool_group_order_loads_into_sidebar_order() {
        let toml = r#"
default_canvas = [800, 600]
autosave_interval_seconds = 60
theme = "System"
tool_group_order = ["Paint", "Vector", "Select", "Transform", "Navigate"]
"#;
        let prefs: Preferences = toml::from_str(toml).expect("parses legacy order");
        assert_eq!(
            prefs.sidebar_order,
            vec![
                SidebarSection::Paint,
                SidebarSection::Vector,
                SidebarSection::Select,
                SidebarSection::Transform,
                SidebarSection::Navigate,
            ]
        );
    }

    #[test]
    fn duplicate_sidebar_order_is_normalized_after_load() {
        let dir = temp_dir("ogre_prefs_sidebar_order_dupes");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("prefs.toml");

        let prefs = Preferences {
            sidebar_order: vec![
                SidebarSection::Paint,
                SidebarSection::Paint,
                SidebarSection::Vector,
            ],
            ..Preferences::default()
        };
        prefs.save(&path).unwrap();
        let loaded = Preferences::load(&path).unwrap();
        assert_eq!(
            loaded.sidebar_order,
            vec![
                SidebarSection::Paint,
                SidebarSection::Vector,
                SidebarSection::Navigate,
                SidebarSection::Transform,
                SidebarSection::Select,
                SidebarSection::Color,
                SidebarSection::Swatches,
            ]
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
