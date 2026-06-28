// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Clipboard image paste utilities.
//!
//! The paste flow is:
//! 1. [`try_paste`] reads the clipboard. If no image is present it returns
//!    immediately. If an image is found it checks whether a resize prompt is
//!    needed (see [`needs_resize_prompt`]).
//! 2. When no prompt is needed the image is pasted directly via
//!    [`paste_new_layer`].
//! 3. When a prompt is needed the image is stored in
//!    [`AppState::paste_prompt`] and the UI renders the modal (see
//!    [`render_paste_prompt`]).

use ogre_core::Rgba32F;

use crate::state::{AppState, PastePrompt};

/// Convert a linear-light `f32` channel in `[0, 1]` from an sRGB `u8` source.
///
/// arboard returns clipboard image bytes as sRGB RGBA8. The document stores
/// pixels in linear light, so each channel must be converted.
fn srgb_u8_to_linear(v: u8) -> f32 {
    let s = v as f32 / 255.0;
    if s <= 0.04045 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}

/// Read an image from the system clipboard.
///
/// Returns `(width, height, pixels)` where `pixels` is a row-major linear-RGBA
/// slice, or `None` if the clipboard contains no image or an error occurs.
pub fn read_clipboard_image() -> Option<(u32, u32, Vec<Rgba32F>)> {
    let mut cb = arboard::Clipboard::new().ok()?;
    let img = cb.get_image().ok()?;

    let w = img.width as u32;
    let h = img.height as u32;
    if w == 0 || h == 0 {
        return None;
    }

    let expected_len = (w as usize) * (h as usize) * 4;
    if img.bytes.len() < expected_len {
        return None;
    }

    let mut pixels = Vec::with_capacity((w * h) as usize);
    for chunk in img.bytes[..expected_len].chunks_exact(4) {
        pixels.push(Rgba32F::new(
            srgb_u8_to_linear(chunk[0]),
            srgb_u8_to_linear(chunk[1]),
            srgb_u8_to_linear(chunk[2]),
            chunk[3] as f32 / 255.0, // alpha is linear
        ));
    }
    Some((w, h, pixels))
}

/// Return `true` when a resize prompt should be shown before pasting.
///
/// A prompt is shown when:
/// - the welcome screen is still active (`welcome == true`), or
/// - the pasted image exceeds either canvas dimension.
pub fn needs_resize_prompt(
    welcome: bool,
    canvas_w: u32,
    canvas_h: u32,
    img_w: u32,
    img_h: u32,
) -> bool {
    welcome || img_w > canvas_w || img_h > canvas_h
}

/// Paste `pixels` as a new raster layer named `"Pasted image"`.
///
/// Dispatches a [`ogre_core::PasteImageLayerCmd`], clears the welcome flag, and
/// marks the document dirty.
pub fn paste_new_layer(state: &mut AppState, w: u32, h: u32, pixels: Vec<Rgba32F>) {
    let cmd = ogre_core::PasteImageLayerCmd::new("Pasted image", w, h, pixels);
    let _ = crate::dispatch::dispatch(state, Box::new(cmd));
    state.welcome = false;
}

/// Add a decoded image as a new layer, routing through the resize prompt when
/// the image is larger than the canvas (same flow as a clipboard paste). Used
/// by the drag-and-drop handler so a dropped oversize image is not silently
/// clipped.
pub fn add_image(state: &mut AppState, w: u32, h: u32, pixels: Vec<Rgba32F>) {
    if needs_resize_prompt(
        state.welcome,
        state.doc().canvas.w,
        state.doc().canvas.h,
        w,
        h,
    ) {
        state.paste_prompt = Some(PastePrompt {
            width: w,
            height: h,
            pixels,
        });
    } else {
        paste_new_layer(state, w, h, pixels);
    }
}

/// Copy the current selection from the active raster layer to the OS clipboard
/// as an sRGB RGBA8 image (alpha = selection coverage). Returns `true` on
/// success. Does not modify the document.
pub fn copy_selection_to_clipboard(state: &mut AppState) -> bool {
    // Build the clipboard image while only borrowing the document, then release
    // the borrow before touching the clipboard or status text.
    let image = {
        let doc = state.doc();
        if doc.selection.is_empty() {
            None
        } else if let Some(active) = doc.active {
            doc.layer(active)
                .ok()
                .and_then(|layer| layer.buffer().map(|b| (b, layer.offset)))
                .and_then(|(buffer, offset)| {
                    let sel = ogre_core::Selection::rect(doc.canvas).intersect(&doc.selection);
                    let bounds = sel.bounds()?;
                    let extracted = ogre_core::extract_selection(buffer, offset, &sel);
                    Some(selection_to_rgba8(&extracted, bounds))
                })
        } else {
            None
        }
    };

    match image {
        Some((w, h, bytes)) => match write_clipboard_image(w, h, bytes) {
            true => {
                state.io_status_feedback = format!("Copied {w}×{h} selection.");
                true
            }
            false => {
                state.io_status_feedback = "Copy to clipboard failed.".to_string();
                false
            }
        },
        None => {
            state.io_status_feedback = "No selection to copy.".to_string();
            false
        }
    }
}

/// Cut the current selection: copy it to the clipboard, then clear the selected
/// pixels from the active layer (making them transparent). Returns `true` on
/// success.
pub fn cut_selection_to_clipboard(state: &mut AppState) -> bool {
    if !copy_selection_to_clipboard(state) {
        return false;
    }
    let Some(active) = state.doc().active else {
        return false;
    };
    let cleared = {
        let Ok(layer) = state.doc().layer(active) else {
            return false;
        };
        if layer.locked {
            state.io_status_feedback = "Layer is locked.".to_string();
            return false;
        }
        let Some(buffer) = layer.buffer() else {
            return false;
        };
        ogre_core::erase_selection_from_buffer(buffer, layer.offset, &state.doc().selection)
    };
    crate::dispatch::dispatch(
        state,
        Box::new(ogre_core::SetLayerBufferCmd::new(active, cleared, "Cut")),
    )
    .is_ok()
}

/// Convert a document-space extracted buffer over `bounds` into a tightly-packed
/// sRGB RGBA8 image `(width, height, bytes)`.
fn selection_to_rgba8(
    extracted: &ogre_core::TiledBuffer,
    bounds: ogre_core::Rect,
) -> (u32, u32, Vec<u8>) {
    let w = bounds.w;
    let h = bounds.h;
    let mut bytes = Vec::with_capacity((w as usize) * (h as usize) * 4);
    let enc = |c: f32| (ogre_io::color::linear_to_srgb(c.clamp(0.0, 1.0)) * 255.0).round() as u8;
    for j in 0..h {
        for i in 0..w {
            let p = extracted.get_pixel(ogre_core::IVec2::new(
                bounds.x + i as i32,
                bounds.y + j as i32,
            ));
            let a = if p.a.is_nan() {
                0.0
            } else {
                p.a.clamp(0.0, 1.0)
            };
            bytes.push(enc(p.r));
            bytes.push(enc(p.g));
            bytes.push(enc(p.b));
            bytes.push((a * 255.0).round() as u8);
        }
    }
    (w, h, bytes)
}

/// Put an sRGB RGBA8 image on the system clipboard. Returns `false` on failure.
fn write_clipboard_image(w: u32, h: u32, bytes: Vec<u8>) -> bool {
    let img = arboard::ImageData {
        width: w as usize,
        height: h as usize,
        bytes: std::borrow::Cow::Owned(bytes),
    };
    arboard::Clipboard::new()
        .and_then(|mut cb| cb.set_image(img))
        .is_ok()
}

/// Attempt to read a clipboard image and start the paste flow.
///
/// If the clipboard contains an image and no prompt is needed the image is
/// pasted immediately.  Otherwise the image data is stored in
/// [`AppState::paste_prompt`] so the UI can render the resize-prompt modal.
pub fn try_paste(state: &mut AppState) {
    let Some((w, h, pixels)) = read_clipboard_image() else {
        state.file_dialog_feedback = "No image found in the clipboard.".to_string();
        return;
    };
    add_image(state, w, h, pixels);
}

/// Render the resize-prompt modal for a pending paste.
///
/// The modal offers the user three choices:
/// - **Resize & Paste** — enlarge the canvas to fit the image, then paste.
/// - **Paste only** — paste the image at its original size.
/// - **Cancel** — discard the pending paste.
///
/// Call this once per frame from the main UI loop when
/// `state.paste_prompt.is_some()`.
pub fn render_paste_prompt(ctx: &egui::Context, state: &mut AppState) {
    let Some((w, h)) = state.paste_prompt.as_ref().map(|p| (p.width, p.height)) else {
        return;
    };
    let mut action: Option<bool> = None; // Some(true)=resize+paste, Some(false)=paste only
    let mut cancel = false;
    let mut open = true;
    let mut escape = false;
    egui::Window::new("Paste image")
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .collapsible(false)
        .resizable(false)
        .open(&mut open)
        .show(ctx, |ui| {
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                escape = true;
            }
            ui.label(format!("Resize canvas to {w}×{h} to fit the pasted image?"));
            ui.horizontal(|ui| {
                if ui.button("Resize & Paste").clicked() {
                    action = Some(true);
                }
                if ui.button("Paste only").clicked() {
                    action = Some(false);
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
        });
    if let Some(resize) = action {
        if let Some(p) = state.paste_prompt.take() {
            if resize {
                let _ = crate::dispatch::dispatch(
                    state,
                    Box::new(ogre_core::ResizeCanvasCmd::new(
                        p.width,
                        p.height,
                        ogre_core::CanvasAnchor::TopLeft,
                    )),
                );
            }
            paste_new_layer(state, p.width, p.height, p.pixels);
        }
    } else if cancel || !open || escape {
        state.paste_prompt = None;
    }
}
