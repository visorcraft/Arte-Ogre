// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC
//! Native image-open flow: rfd picker + recent-files bookkeeping.
use crate::state::{AppState, AppView, DocumentTab, FileDialog, SvgImportDialog};
use std::path::PathBuf;

/// Show the native file picker (filtered to image types); returns the chosen path.
pub fn pick_image_path() -> Option<std::path::PathBuf> {
    rfd::FileDialog::new()
        .add_filter(
            "Images and documents",
            &[
                "png", "jpg", "jpeg", "webp", "tiff", "tif", "exr", "bmp", "ora", "ogre", "svg",
                "svgz",
            ],
        )
        .pick_file()
}

fn is_svg(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| matches!(e.to_lowercase().as_str(), "svg" | "svgz"))
        .unwrap_or(false)
}

/// Open the app-level image-open modal used by File -> Open.
pub fn show_open_dialog(state: &mut AppState) -> bool {
    if state.is_busy() {
        state.file_dialog_feedback = "Busy; please wait for the current operation.".to_string();
        return false;
    }
    state.file_dialog = Some(FileDialog::Open {
        path: String::new(),
    });
    true
}

/// Load `path` into a document tab on the background I/O worker.
///
/// If the current tab is a pristine blank (no save path, no edits, no history —
/// e.g. the startup welcome tab), it is reused in place so opening the first
/// image doesn't leave a stray "Untitled" tab behind. Otherwise a new tab is
/// created. Mirrors `AppState::new_blank_document`'s reuse rule.
///
/// For SVG files, this opens the SVG import options dialog instead of loading
/// immediately, so the user can choose between rasterization, editable vectors,
/// or both.
pub fn open_image(state: &mut AppState, path: impl Into<PathBuf>) {
    let path = path.into();
    if is_svg(&path) {
        state.svg_import_dialog = Some(SvgImportDialog::new(path));
        return;
    }
    open_image_with_options(state, path, ogre_io::svg::SvgImportOptions::default());
}

/// Like [`open_image`], but with explicit SVG import options. Used when the SVG
/// import options dialog has already been confirmed.
pub fn open_image_with_options(
    state: &mut AppState,
    path: impl Into<PathBuf>,
    svg_options: ogre_io::svg::SvgImportOptions,
) {
    let path = path.into();
    if state.is_busy() {
        state.file_dialog_feedback = "Busy; please wait for the current operation.".to_string();
        return;
    }
    // On Bird's Eye View every open is an explicit "Add Image" — always add a
    // new section rather than reusing the active card. The reuse rule only
    // applies in Editor View, where it absorbs the pristine startup tab.
    //
    // Reuse only a truly empty scratch tab: no imported/loaded content
    // (`import_path`), no save path, no edits, and no history. A freshly-opened
    // image has `import_path` set, so opening another image adds a new tab
    // instead of clobbering it.
    let reuse = state.view != AppView::BirdsEye && {
        let tab = state.current_tab();
        tab.import_path.is_none()
            && tab.last_save_path.is_none()
            && !tab.unsaved
            && tab.history.undo_len() == 0
    };
    let id = if reuse {
        let id = state.current_tab().id;
        *state.current_tab_mut() = DocumentTab::new_blank(id, state.preferences.default_canvas);
        id
    } else {
        let id = state.new_tab_id();
        state
            .tabs
            .push(DocumentTab::new_blank(id, state.preferences.default_canvas));
        state.active_tab = state.tabs.len() - 1;
        id
    };
    state.pending_open_tab_id = Some(id);
    state.welcome = false;
    crate::shell::load_document_from_path_with_options(
        state,
        path.to_string_lossy().as_ref(),
        svg_options,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_ogre(tag: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "arte-ogre-open-flow-{tag}-{}.ogre",
            std::process::id()
        ));
        let mut doc = ogre_core::Document::new(32, 32);
        doc.add_raster_layer("Background");
        ogre_io::ogre::save(&doc, &path).unwrap();
        path
    }

    fn drain_io(state: &mut AppState) {
        // Let the background worker settle so it doesn't outlive the test.
        while state.io_worker.poll_result().is_none() {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    /// Opening an image from the welcome/startup blank tab must reuse that tab,
    /// not leave a stray "Untitled" tab behind.
    #[test]
    fn open_image_reuses_pristine_welcome_tab() {
        let path = tmp_ogre("reuse");
        let mut state = AppState::new_document(64, 64);
        assert!(state.welcome, "starts on the welcome screen");
        assert_eq!(state.tabs.len(), 1, "single startup tab");

        open_image(&mut state, path.clone());

        assert!(!state.welcome);
        assert_eq!(
            state.tabs.len(),
            1,
            "opening into the pristine welcome tab must reuse it, not add a tab"
        );
        drain_io(&mut state);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn show_open_dialog_uses_file_modal() {
        let mut state = AppState::new_document(64, 64);

        assert!(show_open_dialog(&mut state));

        assert!(matches!(
            state.file_dialog,
            Some(FileDialog::Open { ref path }) if path.is_empty()
        ));
    }

    #[test]
    fn show_open_dialog_is_blocked_while_busy() {
        let mut state = AppState::new_document(64, 64);
        state.io_busy = true;

        assert!(!show_open_dialog(&mut state));

        assert!(state.file_dialog.is_none());
        assert!(state.file_dialog_feedback.contains("Busy"));
    }

    /// Opening an image while the current tab already has work must create a new
    /// tab rather than clobbering it.
    #[test]
    fn open_image_adds_tab_when_current_is_edited() {
        let path = tmp_ogre("newtab");
        let mut state = AppState::new_document(64, 64);
        // Mark the startup tab as having unsaved work so it is no longer pristine.
        state.current_tab_mut().unsaved = true;
        assert_eq!(state.tabs.len(), 1);

        open_image(&mut state, path.clone());

        assert_eq!(
            state.tabs.len(),
            2,
            "an edited current tab must be preserved and a new tab opened"
        );
        drain_io(&mut state);
        let _ = std::fs::remove_file(path);
    }

    /// A freshly-opened image (imported content, no edits) must not be treated
    /// as a reusable blank: opening another image keeps it and adds a new tab.
    /// Regression for File -> Open clobbering the current image in Editor View.
    #[test]
    fn open_image_adds_tab_when_current_is_opened_image() {
        let path = tmp_ogre("newtab_import");
        let mut state = AppState::new_document(64, 64);
        // Mimic an already-open PNG: imported content, no native save path, no
        // edits, empty history. This matched the old pristine-reuse rule.
        state.welcome = false;
        let tab = state.current_tab_mut();
        tab.import_path = Some(std::path::PathBuf::from("/some/existing.png"));
        tab.last_save_path = None;
        tab.unsaved = false;
        assert_eq!(state.tabs.len(), 1);

        open_image(&mut state, path.clone());

        assert_eq!(
            state.tabs.len(),
            2,
            "opening a second image must add a tab, not replace the open image"
        );
        drain_io(&mut state);
        let _ = std::fs::remove_file(path);
    }

    fn tmp_svg(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "arte-ogre-open-flow-{name}-{}.svg",
            std::process::id()
        ));
        std::fs::write(
            &path,
            br##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10">
                <rect width="10" height="10" fill="#ff0000"/>
            </svg>"##,
        )
        .unwrap();
        path
    }

    /// Opening an SVG file via the default flow must show the import options
    /// dialog instead of loading immediately.
    #[test]
    fn open_image_defers_svg_to_import_dialog() {
        let path = tmp_svg("dialog");
        let mut state = AppState::new_document(64, 64);

        open_image(&mut state, path.clone());

        assert!(
            state.svg_import_dialog.is_some(),
            "SVG files must open the import options dialog"
        );
        assert_eq!(
            state.svg_import_dialog.as_ref().unwrap().path,
            path,
            "dialog must remember the SVG path"
        );
        assert!(!state.io_busy, "no background I/O should start yet");
        let _ = std::fs::remove_file(path);
    }

    /// Loading an SVG with explicit import options creates a document tab.
    #[test]
    fn open_image_with_options_loads_svg() {
        let path = tmp_svg("loaded");
        let mut state = AppState::new_document(64, 64);

        open_image_with_options(
            &mut state,
            path.clone(),
            ogre_io::svg::SvgImportOptions {
                mode: ogre_io::svg::SvgImportMode::Both,
                dpi: 96.0,
            },
        );

        assert!(state.svg_import_dialog.is_none());
        assert!(state.io_busy);
        let result = loop {
            if let Some(r) = state.io_worker.poll_result() {
                break r;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        };
        state.apply_io_result(result);
        assert_eq!(state.current_tab().doc.canvas.w, 10);
        assert_eq!(state.current_tab().doc.canvas.h, 10);
        let _ = std::fs::remove_file(path);
    }

    /// Opening an image while Bird's Eye View is active must not kick the user
    /// back to Editor View.
    #[test]
    fn open_image_from_birds_eye_preserves_view() {
        let path = tmp_ogre("be_open");
        let mut state = AppState::new_document(64, 64);
        state.welcome = false;
        state.view = crate::state::AppView::BirdsEye;

        open_image(&mut state, path.clone());

        assert_eq!(
            state.view,
            crate::state::AppView::BirdsEye,
            "opening must not leave Bird's Eye View"
        );
        drain_io(&mut state);
        let _ = std::fs::remove_file(path);
    }

    /// On Bird's Eye View, opening an image must always add a new section
    /// (tab), never reuse/clobber the active card — even when that card is an
    /// unedited freshly-opened import, which looks "pristine" to the editor's
    /// reuse heuristic (no save path, no edits, empty history).
    #[test]
    fn open_image_on_birds_eye_always_adds_tab() {
        let path = tmp_ogre("be_addtab");
        let mut state = AppState::new_document(64, 64);
        state.welcome = false;
        state.view = crate::state::AppView::BirdsEye;
        // The single startup tab is pristine; in Editor View this would be
        // reused. On Bird's Eye it must be preserved and a new tab added.
        assert_eq!(state.tabs.len(), 1);

        open_image(&mut state, path.clone());

        assert_eq!(
            state.tabs.len(),
            2,
            "Add Image on Bird's Eye must add a new section, not reuse the active card"
        );
        drain_io(&mut state);
        let _ = std::fs::remove_file(path);
    }

    /// Applying the open result must also keep Bird's Eye View active.
    #[test]
    fn apply_open_result_from_birds_eye_preserves_view() {
        let path = tmp_ogre("be_apply");
        let mut state = AppState::new_document(64, 64);
        state.welcome = false;
        state.view = crate::state::AppView::BirdsEye;

        open_image(&mut state, path.clone());
        let result = loop {
            if let Some(r) = state.io_worker.poll_result() {
                break r;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        };
        state.apply_io_result(result);

        assert_eq!(state.view, crate::state::AppView::BirdsEye);
        let _ = std::fs::remove_file(path);
    }

    /// Creating a new blank document always starts in Editor View.
    #[test]
    fn new_blank_document_sets_editor_view() {
        let mut state = AppState::new_document(64, 64);
        state.view = crate::state::AppView::BirdsEye;

        state.new_blank_document((32, 32));

        assert_eq!(state.view, crate::state::AppView::Editor);
    }
}
