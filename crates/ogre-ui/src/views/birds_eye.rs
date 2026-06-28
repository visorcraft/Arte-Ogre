// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC
//! Bird's Eye View: a full-page workspace for arranging layers across open
//! documents. Each open document is a card; each root layer is a draggable
//! thumbnail ordered left-to-right from the bottom to the top of the stack.
pub mod drag;
pub mod thumbnail;

use crate::state::{AppState, AppView, BirdsEyeDrag, BirdsEyeDropTarget};
use crate::theme::Palette;
use ogre_core::{DeleteLayerCmd, InsertLayerSubtreeCmd, LayerContent, LayerId};

/// Vertical slack (points) added above/below a layer row so a drop still
/// registers when the pointer drifts slightly off the thumbnails.
const ROW_DROP_SLACK_Y: f32 = 24.0;
/// Corner radius for document cards. Kept at 8 for a flat, modern look.
const CARD_RADIUS: u8 = 8;

/// Minimum and maximum global thumbnail zoom.
const ZOOM_MIN: f32 = 0.4;
const ZOOM_MAX: f32 = 2.0;
/// Multiplicative step applied by the zoom in/out controls.
const ZOOM_STEP: f32 = 1.15;
/// Base thumbnail tile width in points at zoom `1.0`.
const TILE_BASE_W: f32 = 104.0;
/// Visual lift applied to a thumbnail while hovered, matching the page-card
/// hover affordance in the reference.
const THUMB_HOVER_LIFT_Y: f32 = 5.0;
/// Size of the global "Add Image" drop-zone style action at the end of the
/// document list.
const ADD_IMAGE_SECTION_W: f32 = 560.0;
const ADD_IMAGE_SECTION_H: f32 = 132.0;

/// A snapshot of one root layer row, captured before drawing so card rendering
/// never borrows the document while it also needs `&mut AppState`.
struct RowSnapshot {
    id: LayerId,
    name: String,
    kind: &'static str,
    active: bool,
    /// 1-based z-index where the bottom of the stack is `1`.
    z_index: usize,
}

/// A snapshot of one document card.
struct CardSnapshot {
    tab_id: u64,
    title: String,
    active: bool,
    rows: Vec<RowSnapshot>,
}

/// Human-readable layer kind for tooltips and badges.
fn layer_kind(content: &LayerContent) -> &'static str {
    match content {
        LayerContent::Raster { .. } => "Raster",
        LayerContent::Group { .. } => "Group",
        LayerContent::Adjustment(_) => "Adjustment",
        LayerContent::Vector(_) => "Vector",
    }
}

/// Display title for a tab: the saved/imported file stem, or "Untitled".
fn tab_title(tab: &crate::state::DocumentTab) -> String {
    tab.last_save_path
        .as_ref()
        .or(tab.import_path.as_ref())
        .and_then(|p| p.file_stem())
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "Untitled".to_string())
}

/// Build a per-card snapshot for the tab at `index`.
fn snapshot_card(state: &AppState, index: usize) -> CardSnapshot {
    let tab = &state.tabs[index];
    let doc = &tab.doc;
    let active_layer = doc.active;
    // `doc.order` is bottom-to-top, which is exactly the left-to-right display
    // order for Bird's Eye rows.
    let rows = doc
        .order
        .iter()
        .enumerate()
        .filter_map(|(i, &id)| {
            let layer = doc.layer(id).ok()?;
            Some(RowSnapshot {
                id,
                name: layer.name.clone(),
                kind: layer_kind(&layer.content),
                active: active_layer == Some(id),
                z_index: i + 1,
            })
        })
        .collect();
    CardSnapshot {
        tab_id: tab.id,
        title: tab_title(tab),
        active: index == state.active_tab,
        rows,
    }
}

/// Render the Bird's Eye View central surface.
pub fn render(ui: &mut egui::Ui, state: &mut AppState) {
    let palette = crate::theme::resolve(state.preferences.theme, ui.ctx());
    // Recomputed every frame while rows are drawn; cleared first so a stale
    // target never lingers once the pointer leaves every row.
    state.birds_eye.hover = None;
    egui::Frame::new().fill(palette.bg).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.set_height(ui.available_height());
        toolbar(ui, state);
        ui.separator();
        document_area(ui, state, &palette);
        // A cross-card drag shows whether it will move or copy at the
        // current pointer position and modifier state.
        draw_drag_badge(ui, state, &palette);
        // Drops are finalized from one place after every row is drawn, so a
        // release outside the source widget (common for cross-card drags) is
        // still observed.
        finalize_drop(ui, state);
    });
}

/// Draw a small "Copy"/"Move" badge near the pointer while a layer is being
/// dragged onto a *different* document. Same-document reorders show no badge
/// because they are neither a move nor a copy.
fn draw_drag_badge(ui: &egui::Ui, state: &AppState, palette: &Palette) {
    let Some(drag) = &state.birds_eye.drag else {
        return;
    };
    let Some(hover) = &state.birds_eye.hover else {
        return;
    };
    if hover.tab_id == drag.source_tab_id {
        return;
    }
    let Some(pointer) = ui.input(|i| i.pointer.hover_pos()) else {
        return;
    };
    let copy = ui.input(|i| i.modifiers.command);
    let label = if copy { "Copy" } else { "Move" };

    let painter = ui.ctx().layer_painter(egui::LayerId::new(
        egui::Order::Tooltip,
        egui::Id::new("birdseye_drag_badge"),
    ));
    let galley = painter.layout_no_wrap(
        label.to_string(),
        egui::FontId::proportional(12.0),
        egui::Color32::WHITE,
    );
    let pad = egui::vec2(8.0, 4.0);
    let min = pointer + egui::vec2(14.0, 14.0);
    let rect = egui::Rect::from_min_size(min, galley.size() + pad * 2.0);
    let radius = egui::CornerRadius::same(crate::theme::RADIUS_BUTTON);
    painter.rect_filled(rect, radius, egui::Color32::from_black_alpha(210));
    if copy {
        // An accent ring distinguishes copy from move across themes.
        painter.rect_stroke(
            rect,
            radius,
            egui::Stroke::new(1.5, palette.accent),
            egui::StrokeKind::Inside,
        );
    }
    painter.galley(rect.min + pad, galley, egui::Color32::WHITE);
}

/// Resolve an in-progress drag once the primary button is released.
///
/// Same-tab drops reorder the layer; cross-tab drops move (or copy, with the
/// Ctrl/Cmd modifier) the layer subtree into the target document. No-op drops
/// and drops without a valid hover target simply clear the drag.
fn finalize_drop(ui: &egui::Ui, state: &mut AppState) {
    if state.birds_eye.drag.is_none() {
        return;
    }
    // Only the primary button drives the drag; a stray right/middle release
    // should not commit a half-finished reorder.
    if !ui.input(|i| i.pointer.primary_released()) {
        return;
    }
    // Read the copy modifier at drop time, not drag start.
    let copy = ui.input(|i| i.modifiers.command);
    let Some(drag) = state.birds_eye.drag.take() else {
        return;
    };
    let Some(hover) = state.birds_eye.hover.take() else {
        return;
    };
    if hover.tab_id == drag.source_tab_id {
        same_document_reorder(state, &drag, hover.gap_index);
    } else {
        cross_document_drop(state, &drag, hover.tab_id, hover.gap_index, copy);
    }
}

/// Reorder a layer within its own document, converting the display gap to a
/// final z-index. No-op drops do not push history.
fn same_document_reorder(state: &mut AppState, drag: &BirdsEyeDrag, gap_index: usize) {
    let Some(tab_index) = state.tabs.iter().position(|t| t.id == drag.source_tab_id) else {
        return;
    };
    let len = state.tabs[tab_index].doc.order.len();
    let final_index = drag::same_document_gap_to_final_index(len, drag.original_index, gap_index);
    if final_index == drag.original_index {
        return;
    }
    state.active_tab = tab_index;
    crate::panels::layers::reorder_layer(state, drag.layer_id, final_index);
}

/// Move or copy a layer subtree from the source document into `target_tab_id` at
/// the raw insertion `gap_index`. A move deletes the source layer after a
/// successful insert; a copy leaves the source untouched.
fn cross_document_drop(
    state: &mut AppState,
    drag: &BirdsEyeDrag,
    target_tab_id: u64,
    gap_index: usize,
    copy: bool,
) {
    let Some(src_index) = state.tabs.iter().position(|t| t.id == drag.source_tab_id) else {
        return;
    };
    let cmd = match InsertLayerSubtreeCmd::new_from_source(
        &state.tabs[src_index].doc,
        drag.layer_id,
        gap_index,
    ) {
        Ok(cmd) => cmd,
        Err(e) => {
            state.error_feedback = Some(e.to_string());
            return;
        }
    };
    if let Err(e) = crate::dispatch::dispatch_to_tab_id(state, target_tab_id, Box::new(cmd)) {
        state.error_feedback = Some(e.to_string());
        return;
    }
    if !copy {
        // The insert succeeded; remove the layer from the source document. The
        // source may legitimately become empty.
        if let Err(e) = crate::dispatch::dispatch_to_tab_id(
            state,
            drag.source_tab_id,
            Box::new(DeleteLayerCmd::new(drag.layer_id)),
        ) {
            state.error_feedback = Some(e.to_string());
        }
    }
    if let Some(target_index) = state.tabs.iter().position(|t| t.id == target_tab_id) {
        state.active_tab = target_index;
    }
}

/// The top Bird's Eye toolbar: counts, zoom controls, Open, and a return-to
/// editor control.
fn toolbar(ui: &mut egui::Ui, state: &mut AppState) {
    let doc_count = state.tabs.len();
    let layer_count: usize = state.tabs.iter().map(|t| t.doc.order.len()).sum();
    let busy = state.is_busy();
    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(
            crate::theme::SPACE_L as i8,
            crate::theme::SPACE_M as i8,
        ))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "{doc_count} document{} · {layer_count} layer{}",
                        if doc_count == 1 { "" } else { "s" },
                        if layer_count == 1 { "" } else { "s" },
                    ))
                    .strong(),
                );

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Editor View").clicked() {
                        state.view = AppView::Editor;
                    }
                    if ui
                        .add_enabled(!busy, egui::Button::new("Open"))
                        .on_hover_text("Open an image")
                        .clicked()
                    {
                        crate::open_flow::show_open_dialog(state);
                    }
                    ui.separator();
                    // Compact zoom group: − [zoom%] +. Full-height buttons so
                    // they match the neighboring "Open" button.
                    if ui.button("+").on_hover_text("Zoom in").clicked() {
                        set_zoom(state, state.birds_eye.zoom * ZOOM_STEP);
                    }
                    ui.label(format!(
                        "{:>3}%",
                        (state.birds_eye.zoom * 100.0).round() as i32
                    ));
                    if ui.button("−").on_hover_text("Zoom out").clicked() {
                        set_zoom(state, state.birds_eye.zoom / ZOOM_STEP);
                    }
                });
            });
        });
}

/// Clamp and store the global thumbnail zoom.
fn set_zoom(state: &mut AppState, zoom: f32) {
    state.birds_eye.zoom = zoom.clamp(ZOOM_MIN, ZOOM_MAX);
}

/// The scrollable area of document cards.
fn document_area(ui: &mut egui::Ui, state: &mut AppState, palette: &Palette) {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(crate::theme::SPACE_M);
            let count = state.tabs.len();
            for index in 0..count {
                document_card(ui, state, index, palette);
            }
            add_image_section(ui, state, palette);
            ui.add_space(crate::theme::SPACE_XL);
        });
}

/// Draw the global "Add Image" action below the document cards.
fn add_image_section(ui: &mut egui::Ui, state: &mut AppState, palette: &Palette) {
    ui.add_space(crate::theme::SPACE_M);
    ui.horizontal(|ui| {
        let available = ui.available_width();
        let width = available.min(ADD_IMAGE_SECTION_W);
        ui.add_space(((available - width) * 0.5).max(0.0));
        let busy = state.is_busy();
        let sense = if busy {
            egui::Sense::hover()
        } else {
            egui::Sense::click()
        };
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(width, ADD_IMAGE_SECTION_H), sense);
        let hovered = response.hovered() && !busy;
        let fill = if hovered {
            palette.tertiary
        } else {
            egui::Color32::TRANSPARENT
        };
        let stroke_color = if hovered {
            palette.accent
        } else {
            palette.separator_strong
        };

        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(CARD_RADIUS), fill);
        draw_dashed_rect(
            ui.painter(),
            rect.shrink(0.5),
            egui::Stroke::new(1.0, stroke_color),
        );

        let center = rect.center();
        let circle_radius = 17.0;
        ui.painter().circle_filled(
            center + egui::vec2(0.0, -18.0),
            circle_radius,
            palette.accent,
        );
        ui.painter().text(
            center + egui::vec2(0.0, -18.0),
            egui::Align2::CENTER_CENTER,
            "+",
            egui::FontId::proportional(24.0),
            crate::theme::visuals::on_accent(palette.accent),
        );
        ui.painter().text(
            center + egui::vec2(0.0, 28.0),
            egui::Align2::CENTER_CENTER,
            "Add Image",
            egui::FontId::proportional(13.0),
            if busy {
                ui.visuals().weak_text_color()
            } else {
                palette.text
            },
        );

        if hovered {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if response.clicked() {
            crate::open_flow::show_open_dialog(state);
        }
    });
}

/// Draw a simple dashed rectangle border.
fn draw_dashed_rect(painter: &egui::Painter, rect: egui::Rect, stroke: egui::Stroke) {
    let dash = 6.0;
    let gap = 5.0;
    draw_dashed_line(
        painter,
        rect.left_top(),
        rect.right_top(),
        dash,
        gap,
        stroke,
    );
    draw_dashed_line(
        painter,
        rect.right_top(),
        rect.right_bottom(),
        dash,
        gap,
        stroke,
    );
    draw_dashed_line(
        painter,
        rect.right_bottom(),
        rect.left_bottom(),
        dash,
        gap,
        stroke,
    );
    draw_dashed_line(
        painter,
        rect.left_bottom(),
        rect.left_top(),
        dash,
        gap,
        stroke,
    );
}

/// Draw one dashed line segment between two axis-aligned points.
fn draw_dashed_line(
    painter: &egui::Painter,
    start: egui::Pos2,
    end: egui::Pos2,
    dash: f32,
    gap: f32,
    stroke: egui::Stroke,
) {
    let delta = end - start;
    let len = delta.length();
    if len <= f32::EPSILON {
        return;
    }
    let dir = delta / len;
    let mut traveled = 0.0;
    while traveled < len {
        let dash_end = (traveled + dash).min(len);
        painter.line_segment([start + dir * traveled, start + dir * dash_end], stroke);
        traveled += dash + gap;
    }
}

/// Render a single document card.
fn document_card(ui: &mut egui::Ui, state: &mut AppState, index: usize, palette: &Palette) {
    let card = snapshot_card(state, index);
    let stroke = if card.active {
        egui::Stroke::new(1.5, palette.accent)
    } else {
        egui::Stroke::new(1.0, palette.separator_strong)
    };
    let inner = egui::Frame::new()
        .fill(palette.secondary)
        .stroke(stroke)
        .corner_radius(egui::CornerRadius::same(CARD_RADIUS))
        .inner_margin(egui::Margin::same(crate::theme::SPACE_M as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            // The header is the document-select target. Using the header (rather
            // than the whole card) avoids an overlapping full-rect click area
            // that would otherwise swallow per-thumbnail clicks underneath it.
            let title_clicked = card_title(ui, &card, index).clicked();
            ui.add_space(crate::theme::SPACE_S);
            layer_row(ui, state, &card, palette);
            title_clicked
        });
    ui.add_space(crate::theme::SPACE_M);

    if inner.inner || card_primary_clicked(ui, inner.response.rect) {
        activate_document_card(state, index);
    }
}

/// Whether the current frame contains a primary click inside `rect`.
///
/// This deliberately reads raw pointer input instead of registering a full-card
/// interaction rectangle, so thumbnail clicks, double-clicks, and drags keep
/// their own widget responses.
fn card_primary_clicked(ui: &egui::Ui, rect: egui::Rect) -> bool {
    ui.input(|i| {
        i.pointer.primary_clicked()
            && i.pointer
                .interact_pos()
                .is_some_and(|pos| rect.contains(pos))
    })
}

/// Activate a document from Bird's Eye View, matching editor tab selection.
fn activate_document_card(state: &mut AppState, index: usize) {
    if index >= state.tabs.len() || index == state.active_tab {
        return;
    }
    state.tool_manager.cancel_active();
    state.active_tab = index;
    state.renderer_needs_clear = true;
    state.dirty = true;
}

/// The card header: ordinal, title, and root-layer count. Returns a click
/// response so the caller can select the document.
fn card_title(ui: &mut egui::Ui, card: &CardSnapshot, index: usize) -> egui::Response {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(format!("{:02}", index + 1)).weak());
        ui.add(egui::Label::new(egui::RichText::new(&card.title).strong()).truncate());
        let n = card.rows.len();
        ui.label(egui::RichText::new(format!("{n} layer{}", if n == 1 { "" } else { "s" })).weak());
    })
    .response
    .interact(egui::Sense::click())
}

/// Render the horizontal row of layer thumbnails for a card, left-to-right in
/// bottom-to-top z-order.
fn layer_row(ui: &mut egui::Ui, state: &mut AppState, card: &CardSnapshot, palette: &Palette) {
    let tile_w = (TILE_BASE_W * state.birds_eye.zoom).round();
    egui::ScrollArea::horizontal()
        .id_salt(("birdseye_row", card.tab_id))
        .auto_shrink([false, true])
        // Thumbnails own horizontal drags; drag-to-scroll would steal them.
        .scroll_source(egui::scroll_area::ScrollSource {
            drag: egui::scroll_area::DragScroll::Never,
            ..egui::scroll_area::ScrollSource::ALL
        })
        .show(ui, |ui| {
            let inner = ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = crate::theme::SPACE_S;
                // Span the full card width so the entire row is a drop target,
                // including the empty space to the right of the thumbnails and
                // the whole of an empty card.
                ui.set_min_width(ui.available_width());
                if card.rows.is_empty() {
                    ui.add_space(crate::theme::SPACE_S);
                    ui.label(egui::RichText::new("No layers").weak());
                    return Vec::new();
                }
                let mut rects = Vec::with_capacity(card.rows.len());
                for index in 0..card.rows.len() {
                    let rect =
                        layer_thumb(ui, state, card.tab_id, &card.rows[index], tile_w, palette);
                    rects.push(rect);
                }
                rects
            });
            if state.birds_eye.drag.is_some() {
                update_drop_target(
                    ui,
                    state,
                    card.tab_id,
                    &inner.inner,
                    inner.response.rect,
                    palette,
                );
            }
        });
}

/// While a drag is active, record the hover gap for this card and draw a
/// vertical insertion indicator when the pointer is over (or just above/below)
/// the row.
fn update_drop_target(
    ui: &egui::Ui,
    state: &mut AppState,
    tab_id: u64,
    rects: &[egui::Rect],
    row_rect: egui::Rect,
    palette: &Palette,
) {
    let Some(pointer) = ui.input(|i| i.pointer.hover_pos()) else {
        return;
    };
    let hit_rect = row_rect.expand2(egui::vec2(0.0, ROW_DROP_SLACK_Y));
    if !hit_rect.contains(pointer) {
        return;
    }
    let gap = drag::gap_index_from_pointer_x(rects, pointer.x);
    state.birds_eye.hover = Some(BirdsEyeDropTarget {
        tab_id,
        gap_index: gap,
    });
    let x = insertion_indicator_x(rects, gap, row_rect);
    ui.painter().vline(
        x,
        row_rect.y_range(),
        egui::Stroke::new(2.0, palette.accent),
    );
}

/// Screen-space x for the insertion indicator at `gap` within a left-to-right
/// row. Falls in the spacing between the neighboring thumbnails.
fn insertion_indicator_x(rects: &[egui::Rect], gap: usize, row_rect: egui::Rect) -> f32 {
    let half = crate::theme::SPACE_S * 0.5;
    if rects.is_empty() {
        return row_rect.left();
    }
    if gap == 0 {
        return rects[0].left() - half;
    }
    if gap >= rects.len() {
        return rects[rects.len() - 1].right() + half;
    }
    (rects[gap - 1].right() + rects[gap].left()) * 0.5
}

/// Truncate a string to at most `max_chars`, appending an ellipsis when cut.
fn truncate_to(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let keep = max_chars.saturating_sub(1).max(1);
    let head: String = s.chars().take(keep).collect();
    format!("{head}…")
}

/// Ensure a cached thumbnail texture exists for `(tab_id, layer_id)`, generating
/// it on a cache miss, and return its texture id.
fn ensure_thumbnail(
    ctx: &egui::Context,
    state: &mut AppState,
    tab_id: u64,
    tab_index: usize,
    layer_id: LayerId,
) -> egui::TextureId {
    let key = (tab_id, layer_id);
    if let Some(tex) = state.birds_eye.thumbnails.get(&key) {
        return tex.id();
    }
    let image = thumbnail::layer_thumbnail(&state.tabs[tab_index].doc, layer_id);
    let tex = ctx.load_texture(
        format!("birdseye_thumb_{tab_id}_{layer_id:?}"),
        image,
        egui::TextureOptions::LINEAR,
    );
    let id = tex.id();
    state.birds_eye.thumbnails.insert(key, tex);
    state.birds_eye.dirty_thumbnails.remove(&key);
    id
}

/// A single layer thumbnail tile with z-index and name badges. Returns the
/// tile's screen rect so the row can compute drop gaps.
fn layer_thumb(
    ui: &mut egui::Ui,
    state: &mut AppState,
    tab_id: u64,
    row: &RowSnapshot,
    tile_w: f32,
    palette: &Palette,
) -> egui::Rect {
    let Some(tab_index) = state.tabs.iter().position(|t| t.id == tab_id) else {
        return egui::Rect::NOTHING;
    };
    let tex_id = ensure_thumbnail(ui.ctx(), state, tab_id, tab_index, row.id);

    let size = egui::vec2(tile_w, tile_w);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
    let radius = egui::CornerRadius::same(crate::theme::RADIUS_BUTTON);
    let hovered_or_dragged = response.hovered() || response.dragged();
    let paint_rect = if hovered_or_dragged {
        rect.translate(egui::vec2(0.0, -THUMB_HOVER_LIFT_Y))
    } else {
        rect
    };
    let painter = ui.painter_at(rect.expand2(egui::vec2(0.0, THUMB_HOVER_LIFT_Y)));

    // The thumbnail texture already bakes in the checkerboard background.
    painter.image(
        tex_id,
        paint_rect,
        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        egui::Color32::WHITE,
    );

    // Active-layer border, drawn inside so it is never clipped.
    let border = if row.active {
        egui::Stroke::new(2.0, palette.accent)
    } else {
        egui::Stroke::new(1.0, palette.separator_strong)
    };
    painter.rect_stroke(paint_rect, radius, border, egui::StrokeKind::Inside);

    // Top-left z-index badge (bottom of the stack is 1).
    let z_text = format!("{}", row.z_index);
    let z_pos = paint_rect.left_top() + egui::vec2(4.0, 4.0);
    let z_galley = painter.layout_no_wrap(
        z_text,
        egui::FontId::proportional(11.0),
        egui::Color32::WHITE,
    );
    let z_bg = egui::Rect::from_min_size(z_pos, z_galley.size() + egui::vec2(8.0, 4.0));
    painter.rect_filled(
        z_bg,
        egui::CornerRadius::same(4),
        egui::Color32::from_black_alpha(170),
    );
    painter.galley(
        z_bg.min + egui::vec2(4.0, 2.0),
        z_galley,
        egui::Color32::WHITE,
    );

    // Bottom name badge spanning the tile width.
    let badge_h = 18.0_f32.min(tile_w * 0.5);
    let name_bg = egui::Rect::from_min_max(
        egui::pos2(paint_rect.left(), paint_rect.bottom() - badge_h),
        paint_rect.right_bottom(),
    );
    painter.rect_filled(name_bg, radius, egui::Color32::from_black_alpha(160));
    let max_chars = ((tile_w - 10.0) / 6.0).max(3.0) as usize;
    let name_galley = painter.layout_no_wrap(
        truncate_to(&row.name, max_chars),
        egui::FontId::proportional(11.0),
        egui::Color32::WHITE,
    );
    painter.galley(
        egui::pos2(
            name_bg.left() + 5.0,
            name_bg.center().y - name_galley.size().y / 2.0,
        ),
        name_galley,
        egui::Color32::WHITE,
    );

    // A drag that's clearly horizontal is highlighted by lifting the tile.
    if response.dragged() {
        ui.painter().rect_stroke(
            paint_rect,
            radius,
            egui::Stroke::new(2.0, palette.accent),
            egui::StrokeKind::Inside,
        );
    }

    if response.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
    } else if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
    }

    let response = response.on_hover_text(format!("{}\n{}", row.name, row.kind));
    if response.drag_started() {
        state.birds_eye.drag = Some(BirdsEyeDrag {
            source_tab_id: tab_id,
            layer_id: row.id,
            // `z_index` is 1-based from the bottom; the drag stores the
            // bottom-to-top display index.
            original_index: row.z_index - 1,
        });
    }
    if response.double_clicked() {
        // Jump back to the editor focused on this layer.
        activate_document_card(state, tab_index);
        state.doc_mut().active = Some(row.id);
        // Refresh vector re-edit state for vector-capable tools, mirroring the
        // Layers panel's layer-select behavior.
        let doc = state.doc().clone();
        state.tool_manager.load_active_vector_layer(&doc);
        state.view = AppView::Editor;
    } else if response.clicked() {
        activate_document_card(state, tab_index);
        state.tabs[tab_index].doc.active = Some(row.id);
    }

    rect
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::DocumentTab;

    fn two_tab_state() -> (AppState, u64, u64, LayerId) {
        let mut state = AppState::new_document(64, 64);
        state.welcome = false;

        let source_tab_id = state.tabs[0].id;
        let source_layer = state.tabs[0].doc.active.unwrap();
        state.tabs[0].doc.layer_mut(source_layer).unwrap().name = "source".to_string();

        let target_tab_id = state.new_tab_id();
        state
            .tabs
            .push(DocumentTab::new_blank(target_tab_id, (64, 64)));
        state.active_tab = 1;

        (state, source_tab_id, target_tab_id, source_layer)
    }

    #[test]
    fn activating_document_card_selects_tab_used_by_close() {
        let (mut state, source_tab_id, target_tab_id, _source_layer) = two_tab_state();
        state.dirty = false;
        state.renderer_needs_clear = false;
        state.active_tab = 1;

        activate_document_card(&mut state, 0);

        assert_eq!(state.active_tab, 0);
        assert!(state.dirty);
        assert!(state.renderer_needs_clear);

        state.close_tab(state.active_tab);

        assert_eq!(state.tabs.len(), 1);
        assert_eq!(state.tabs[0].id, target_tab_id);
        assert_ne!(state.tabs[0].id, source_tab_id);
    }

    #[test]
    fn cross_document_copy_keeps_source_and_inserts_target() {
        let (mut state, source_tab_id, target_tab_id, source_layer) = two_tab_state();
        let drag = BirdsEyeDrag {
            source_tab_id,
            layer_id: source_layer,
            original_index: 0,
        };

        cross_document_drop(&mut state, &drag, target_tab_id, 1, true);

        let source_idx = state
            .tabs
            .iter()
            .position(|t| t.id == source_tab_id)
            .unwrap();
        let target_idx = state
            .tabs
            .iter()
            .position(|t| t.id == target_tab_id)
            .unwrap();
        assert_eq!(state.tabs[source_idx].doc.order, vec![source_layer]);
        assert_eq!(state.tabs[source_idx].history.undo_len(), 0);
        assert_eq!(state.tabs[target_idx].doc.order.len(), 2);
        let inserted = state.tabs[target_idx].doc.active.unwrap();
        assert_eq!(
            state.tabs[target_idx].doc.layer(inserted).unwrap().name,
            "source"
        );
        assert_eq!(state.tabs[target_idx].history.undo_len(), 1);
        assert_eq!(state.active_tab, target_idx);
    }

    #[test]
    fn cross_document_move_has_independent_undo_on_each_tab() {
        let (mut state, source_tab_id, target_tab_id, source_layer) = two_tab_state();
        let drag = BirdsEyeDrag {
            source_tab_id,
            layer_id: source_layer,
            original_index: 0,
        };

        cross_document_drop(&mut state, &drag, target_tab_id, 1, false);

        let source_idx = state
            .tabs
            .iter()
            .position(|t| t.id == source_tab_id)
            .unwrap();
        let target_idx = state
            .tabs
            .iter()
            .position(|t| t.id == target_tab_id)
            .unwrap();
        assert!(state.tabs[source_idx].doc.order.is_empty());
        assert_eq!(state.tabs[source_idx].history.undo_len(), 1);
        assert_eq!(state.tabs[target_idx].doc.order.len(), 2);
        assert_eq!(state.tabs[target_idx].history.undo_len(), 1);

        state.active_tab = target_idx;
        assert_eq!(crate::dispatch::undo(&mut state), Some("Insert layer"));
        assert_eq!(state.tabs[target_idx].doc.order.len(), 1);

        state.active_tab = source_idx;
        assert_eq!(crate::dispatch::undo(&mut state), Some("Delete layer"));
        assert_eq!(state.tabs[source_idx].doc.order, vec![source_layer]);
    }
}
