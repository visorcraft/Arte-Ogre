// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! About + Check-for-Updates modals and the background version check.

use crate::state::{AppState, UpdateCheck};
use std::time::Duration;

const REPO_URL: &str = "https://github.com/visorcraft/Arte-Ogre";
const RELEASES_URL: &str = "https://github.com/visorcraft/Arte-Ogre/releases";

/// Returns `true` if `tag` (e.g. `"v0.2.0"`) represents a newer semver than
/// `current` (e.g. `"0.1.1"`).
///
/// Strips a leading `v` before comparing three dot-separated numeric components.
/// Returns `false` for any tag that cannot be parsed as `MAJOR.MINOR.PATCH`.
pub fn is_newer(tag: &str, current: &str) -> bool {
    fn parse(s: &str) -> Option<(u64, u64, u64)> {
        let s = s.trim().trim_start_matches('v');
        let mut it = s.split('.');
        let v = (
            it.next()?.parse().ok()?,
            it.next()?.parse().ok()?,
            it.next()?.parse().ok()?,
        );
        if it.next().is_some() {
            return None;
        } // reject trailing segments
        Some(v)
    }
    match (parse(tag), parse(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// Kick off the background GitHub release check (if not already running/done).
///
/// Spawns a thread that fetches the latest release tag from the GitHub API and
/// sends the result through an [`mpsc`](std::sync::mpsc) channel stored in
/// [`UpdateCheck::Checking`].
pub fn start_update_check(state: &mut AppState, ctx: &egui::Context) {
    if matches!(state.update_check, crate::state::UpdateCheck::Checking(_)) {
        return;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let result = (|| {
            let mut resp =
                ureq::get("https://api.github.com/repos/visorcraft/Arte-Ogre/releases/latest")
                    .header("User-Agent", "arte-ogre")
                    .config()
                    .timeout_global(Some(Duration::from_secs(10)))
                    .build()
                    .call()
                    .map_err(|e| format!("network error: {e}"))?;
            let body = resp
                .body_mut()
                .read_to_string()
                .map_err(|e| format!("read error: {e}"))?;
            let v: serde_json::Value =
                serde_json::from_str(&body).map_err(|e| format!("parse error: {e}"))?;
            v.get("tag_name")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string())
                .ok_or_else(|| "no releases yet".to_string())
        })();
        let _ = tx.send(result);
        ctx.request_repaint();
    });
    state.update_check = UpdateCheck::Checking(rx);
}

/// Draw the About modal when `state.show_about` is `true`.
pub fn about_modal(ctx: &egui::Context, state: &mut AppState) {
    if !state.show_about {
        return;
    }
    let palette = crate::theme::resolve(state.preferences.theme, ctx);
    egui::Window::new("About Arte Ogre")
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .collapsible(false)
        .resizable(false)
        .title_bar(false)
        .show(ctx, |ui| {
            crate::shell::modal_heading(ui, "About Arte Ogre");
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                state.show_about = false;
            }
            ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
            ui.label("A Rust-native, GPU-accelerated layered image editor.");
            ui.hyperlink_to("View on GitHub", REPO_URL);
            crate::shell::modal_actions(ui, |ui| {
                if ui
                    .add(crate::shell::accent_button("Close", palette.accent))
                    .clicked()
                {
                    state.show_about = false;
                }
            });
        });
}

/// Draw the Check-for-Updates modal while a check is active or complete.
///
/// Polls the background receiver each frame and transitions
/// [`UpdateCheck::Checking`] → [`UpdateCheck::Done`] when the result arrives.
/// Closing the window resets the state to [`UpdateCheck::Idle`].
pub fn updates_modal(ctx: &egui::Context, state: &mut AppState) {
    // Poll the receiver.
    if let UpdateCheck::Checking(rx) = &state.update_check {
        if let Ok(res) = rx.try_recv() {
            state.update_check = UpdateCheck::Done(res);
        }
    }
    if matches!(state.update_check, UpdateCheck::Idle) {
        return;
    }
    let palette = crate::theme::resolve(state.preferences.theme, ctx);
    let mut close = false;
    egui::Window::new("Check for Updates")
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .collapsible(false)
        .resizable(false)
        .title_bar(false)
        .show(ctx, |ui| {
            crate::shell::modal_heading(ui, "Check for Updates");
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                close = true;
            }
            let current = env!("CARGO_PKG_VERSION");
            match &state.update_check {
                UpdateCheck::Checking(_) => {
                    ui.label("Checking…");
                    ctx.request_repaint();
                }
                UpdateCheck::Done(Ok(tag)) if is_newer(tag, current) => {
                    ui.label(format!("{tag} is available (you have v{current})."));
                    ui.hyperlink_to("Download", RELEASES_URL);
                }
                UpdateCheck::Done(Ok(_)) => {
                    ui.label(format!("Up to date (v{current})."));
                }
                UpdateCheck::Done(Err(e)) => {
                    ui.label(format!("Could not check: {e}"));
                }
                UpdateCheck::Idle => {}
            }
            crate::shell::modal_actions(ui, |ui| {
                if ui
                    .add(crate::shell::accent_button("Close", palette.accent))
                    .clicked()
                {
                    close = true;
                }
            });
        });
    if close {
        state.update_check = UpdateCheck::Idle;
    }
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn is_newer_compares_semverish() {
        assert!(is_newer("v0.2.0", "0.1.1"));
        assert!(is_newer("0.1.2", "0.1.1"));
        assert!(!is_newer("v0.1.1", "0.1.1"));
        assert!(!is_newer("0.1.0", "0.1.1"));
        assert!(!is_newer("garbage", "0.1.1")); // unparseable → not newer
        assert!(!is_newer("v1.0.0.1", "0.1.1")); // trailing segment → not newer
    }
}
