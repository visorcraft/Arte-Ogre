// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! GPU canvas panel: composited document view, pan/zoom, and screen↔doc mapping.
//!
//! The canvas panel is responsible for drawing the document through
//! [`ogre_gpu::CanvasRenderer`] and translating pointer input into document-space
//! coordinates for tools. All viewport mutations mark the renderer dirty so the
//! texture is recomposited on the next frame.

use std::sync::Arc;

use glam::{IVec2, UVec2, Vec2};

use crate::dispatch;
use crate::grid::{grid_lines_in_range, snap_to_grid};
use crate::tools::{Phase, PointerEvent, ToolKind};
use crate::OgreApp;

/// Zoom scaling factor: one mouse-wheel "tick" (≈50 egui points) changes zoom by
/// this ratio via `2^(scroll_delta / ZOOM_DIVISOR)`.
const ZOOM_DIVISOR: f32 = 200.0;

/// Render the GPU canvas into the provided `ui`, handling pan/zoom input.
///
/// # Panics
///
/// Panics if `app.renderer` or `app.egui_renderer` have not been initialized.
/// They are constructed lazily in [`OgreApp`]'s `ui` method before
/// the dock area is shown.
pub fn render_canvas(ui: &mut egui::Ui, app: &mut OgreApp) {
    let canvas_rect = ui.available_rect_before_wrap();
    let ppp = ui.ctx().pixels_per_point();

    // Record the canvas area so modal dialogs can center over it.
    app.state.canvas_screen_rect = Some(canvas_rect);

    let view_size_px = UVec2::new(
        (canvas_rect.width() * ppp).ceil().max(1.0) as u32,
        (canvas_rect.height() * ppp).ceil().max(1.0) as u32,
    );

    // Re-render when the canvas is resized even if no command changed the doc.
    if app.last_canvas_size != Some(view_size_px) {
        app.last_canvas_size = Some(view_size_px);
        app.state.dirty = true;
    }

    // Propagate viewport changes from the status-bar zoom magnifier (or any
    // other non-interaction path) to the GPU renderer.
    if app.state.viewport() != &app.last_viewport {
        renderer_set_viewport(app);
        app.last_viewport = *app.state.viewport();
    }

    let renderer = app
        .renderer
        .as_mut()
        .expect("CanvasRenderer must be initialized in OgreApp::ui before rendering");
    let egui_renderer = app
        .egui_renderer
        .as_ref()
        .expect("egui_wgpu::Renderer must be initialized in OgreApp::ui");

    if app.state.dirty {
        renderer.render(app.state.doc(), view_size_px);
        app.state.dirty = false;

        let mut guard = egui_renderer.write();
        let texture_id = renderer.register_with_egui(&mut guard, renderer.device());
        app.canvas_texture = Some(texture_id);
    }

    // Draw the composited texture filling the canvas rectangle.
    let texture_id = app
        .canvas_texture
        .expect("canvas texture registered on first render");
    ui.painter().image(
        texture_id,
        canvas_rect,
        egui::Rect::from_min_max(egui::Pos2::ZERO, egui::Pos2::new(1.0, 1.0)),
        egui::Color32::WHITE,
    );

    // Allocate a single response for the canvas and use it for input, context
    // menu, and overlay drawing.
    let response = ui.allocate_rect(canvas_rect, egui::Sense::click_and_drag());

    // Input handling: scroll zooms toward cursor; middle-drag or space-drag pans.
    handle_canvas_input(ui, app, &response, canvas_rect, ppp);

    // Grid overlay.
    if app.state.show_grid {
        draw_grid_overlay(
            ui.painter(),
            app.state.viewport(),
            canvas_rect,
            ppp,
            app.state.grid_spacing,
        );
    }

    // Committed selection outline (marching ants along the real boundary),
    // drawn centrally so it is visible and correct for every selection tool —
    // not just a bounding box, and not lost when a lasso commits. The boundary
    // is cached and only retraced when the selection changes (recomputing every
    // frame would lag on a large magic-wand mask).
    let time = ui.ctx().input(|i| i.time);
    let key = app.state.doc().selection.outline_cache_key();
    if key != app.state.selection_outline_key {
        app.state.selection_outline = app.state.doc().selection.outline_edges();
        app.state.selection_outline_key = key;
    }
    draw_selection_outline(
        ui.painter(),
        &app.state.selection_outline,
        app.state.viewport(),
        canvas_rect,
        ppp,
        time,
    );

    // Tool overlay: each tool's in-progress preview (drag rectangle, lasso path,
    // wand crosshair) on top of the committed outline.
    app.state.tool_manager.draw_overlay(
        ui.painter(),
        app.state.doc(),
        app.state.viewport(),
        canvas_rect,
        ppp,
        time,
    );

    // Busy overlay: a centered spinner over the canvas when a plugin or I/O
    // operation is in flight. Background removal has its own dedicated modal
    // (render_bg_removal_spinner), so it is excluded here to avoid a second
    // "Working…" overlay on top of the "Removing background…" modal.
    if app.state.plugin_busy || app.state.io_busy {
        egui::Area::new(egui::Id::new("canvas_busy_overlay"))
            .order(egui::Order::Foreground)
            .fixed_pos(canvas_rect.center())
            .show(ui.ctx(), |ui| {
                egui::Frame::canvas(ui.style())
                    .fill(ui.visuals().window_fill)
                    .corner_radius(8.0)
                    .inner_margin(egui::Margin::same(16))
                    .show(ui, |ui| {
                        ui.vertical_centered(|ui| {
                            ui.add(egui::Spinner::new().size(32.0));
                            ui.label("Working…");
                        });
                    });
            });
    }

    // Paint-bucket cursor: a tipping-bucket glyph whose pour point marks exactly
    // where the fill will seed.
    draw_paint_bucket_cursor(ui, app, &response);

    // Right-click context menu.
    response.context_menu(|ui| {
        for item in crate::context_menu::context_menu_items(&app.state) {
            if ui
                .add_enabled(item.enabled, egui::Button::new(item.label))
                .clicked()
            {
                crate::context_menu::apply_context_action(&mut app.state, item.action);
            }
        }
    });

    // Left-click-inside-selection menu (Select Inverse / Deselect).
    draw_left_click_menu(ui, app);

    // Track the doc-space cursor for the global status bar.
    update_cursor_state(ui, app, canvas_rect, ppp);
}

/// Handle pointer input for pan and zoom over the canvas.
fn handle_canvas_input(
    ui: &mut egui::Ui,
    app: &mut OgreApp,
    response: &egui::Response,
    canvas_rect: egui::Rect,
    ppp: f32,
) {
    // Scroll-to-zoom, centered on the cursor.
    let scroll_delta = ui.input(|i| i.smooth_scroll_delta.y);
    if scroll_delta != 0.0 {
        if let Some(cursor_abs) = response.hover_pos() {
            let cursor_rel = Vec2::new(
                cursor_abs.x - canvas_rect.min.x,
                cursor_abs.y - canvas_rect.min.y,
            );
            apply_zoom(app.state.viewport_mut(), cursor_rel, scroll_delta, ppp);
            renderer_set_viewport(app);
        }
    }

    // Pan via: middle-drag, space+primary-drag, or the Hand tool's primary drag.
    // The Hand tool dedicates the primary button to panning and never edits the
    // document, so a 4K image can be navigated at high zoom on a small screen.
    let hand_active = app.state.tool_manager.active() == ToolKind::Hand;
    if hand_active && response.hovered() {
        let icon = if response.dragged_by(egui::PointerButton::Primary) {
            egui::CursorIcon::Grabbing
        } else {
            egui::CursorIcon::Grab
        };
        ui.ctx().set_cursor_icon(icon);
    }
    let space_held = ui.input(|i| i.key_down(egui::Key::Space));
    // Temporary Hand: while Space is held the cursor becomes a Grab icon and
    // tool pointer events are suppressed so dragging pans instead of painting.
    // No ToolKind is swapped, so in-progress tool state (brush strokes, polygon
    // lasso points, etc.) is preserved when Space is released.
    if space_held && response.hovered() {
        let icon = if response.dragged_by(egui::PointerButton::Primary) {
            egui::CursorIcon::Grabbing
        } else {
            egui::CursorIcon::Grab
        };
        ui.ctx().set_cursor_icon(icon);
    }
    let middle_drag = response.dragged_by(egui::PointerButton::Middle);
    let space_pan_drag = space_held && response.dragged_by(egui::PointerButton::Primary);
    let hand_pan_drag = hand_active && response.dragged_by(egui::PointerButton::Primary);
    if middle_drag || space_pan_drag || hand_pan_drag {
        let delta_logical = ui.input(|i| i.pointer.delta());
        let delta_physical = Vec2::new(delta_logical.x, delta_logical.y) * ppp;
        apply_pan(app.state.viewport_mut(), delta_physical);
        renderer_set_viewport(app);
    } else if !hand_active && !space_held {
        // Forward primary pointer events to the active tool. The Hand tool has
        // no tool action, so a non-dragging click is ignored rather than routed.
        // While Space is held (temporary Hand), suppress forwarding so the
        // active tool does not fire.
        forward_pointer_events(ui, app, response, canvas_rect, ppp);
    }
}

/// Convert egui pointer state into document-space [`PointerEvent`]s, forward
/// them to the active tool, and dispatch any command it returns.
fn forward_pointer_events(
    ui: &egui::Ui,
    app: &mut OgreApp,
    response: &egui::Response,
    canvas_rect: egui::Rect,
    ppp: f32,
) {
    // Block tool edits while a background operation is in flight.
    if app.state.is_busy() {
        return;
    }

    // Per-tool OS cursor. Self-managed tools (Hand, Zoom, PaintBucket) return
    // None and set their own cursor later in the input/render path; for
    // everything else this is the fallback (their own blocks below override it
    // only for pan/zoom interactions).
    if response.hovered() {
        if let Some(icon) = app.state.tool_manager.active().cursor_icon() {
            ui.ctx().set_cursor_icon(icon);
        }
    }

    let Some(cursor_abs) = response
        .hover_pos()
        .or_else(|| response.interact_pointer_pos())
    else {
        return;
    };
    let cursor_rel = Vec2::new(
        cursor_abs.x - canvas_rect.min.x,
        cursor_abs.y - canvas_rect.min.y,
    );
    let raw_doc_pos = screen_to_doc(cursor_rel, app.state.viewport(), ppp);
    let mut doc_pos = raw_doc_pos;
    if app.state.snap_to_grid && app.state.grid_spacing > 0 {
        doc_pos = snap_to_grid(doc_pos.as_vec2(), app.state.grid_spacing);
    }

    // Keep the paint tools' color in sync with the shared foreground color.
    let foreground = app.state.foreground;
    let background = app.state.background;
    app.state
        .tool_manager
        .set_paint_colors(foreground, background);

    // Eyedropper samples the visible composite into the foreground color. It
    // edits no pixels, so it bypasses the tool/command path entirely. Sampling
    // uses the raw cursor position, not the grid-snapped one.
    if app.state.tool_manager.active() == ToolKind::Eyedropper {
        let sampling = response.clicked()
            || response.drag_started_by(egui::PointerButton::Primary)
            || response.dragged_by(egui::PointerButton::Primary);
        if sampling {
            let radius = app.state.tool_manager.eyedropper_sample().radius();
            if let Some(color) = sample_composite(app.state.doc(), raw_doc_pos, radius) {
                app.state.foreground = color;
            }
        }
        return;
    }

    // Zoom tool: click to zoom in toward the cursor, Alt+click to zoom out.
    // Like Eyedropper/Hand it edits the Viewport, never the document, so it
    // bypasses the command path. Marquee-zoom (drag-to-fit) is deferred.
    if app.state.tool_manager.active() == ToolKind::Zoom {
        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ZoomIn);
        }
        if response.clicked() {
            if let Some(cursor_abs) = response.interact_pointer_pos() {
                let cursor_rel = Vec2::new(
                    cursor_abs.x - canvas_rect.min.x,
                    cursor_abs.y - canvas_rect.min.y,
                );
                let alt = ui.input(|i| i.modifiers.alt);
                // ±ZOOM_DIVISOR is exactly a 2× / 0.5× step (factor = 2^(δ/200)).
                let delta = if alt { -ZOOM_DIVISOR } else { ZOOM_DIVISOR };
                apply_zoom(app.state.viewport_mut(), cursor_rel, delta, ppp);
                // Clamp to [1/16, 64] so a long session can't run off the rails.
                const MIN_ZOOM: f32 = 1.0 / 16.0;
                const MAX_ZOOM: f32 = 64.0;
                let vp = app.state.viewport_mut();
                vp.zoom = vp.zoom.clamp(MIN_ZOOM, MAX_ZOOM);
                renderer_set_viewport(app);
            }
        }
        return;
    }

    // A pure left-click (no drag) inside an existing selection opens the
    // "Select Inverse / Deselect" menu instead of starting a tool action — but
    // only for tools whose click does not itself build a selection (the marquee
    // and freehand-lasso tools). Magic Wand and Polygon Lasso keep using clicks
    // for their own action, and a click *outside* the selection falls through to
    // the tool (which deselects / starts fresh). Checked before the mutable
    // dispatch closure below borrows `app.state`.
    if response.clicked() {
        let menu_tool = matches!(
            app.state.tool_manager.active(),
            ToolKind::RectSelect | ToolKind::EllipseSelect | ToolKind::FreehandLasso
        );
        let sel = &app.state.doc().selection;
        if menu_tool && !sel.is_empty() && sel.coverage_at(doc_pos) > 0.0 {
            app.state.left_click_menu = Some(cursor_abs);
            app.state.left_click_menu_fresh = true;
            return;
        }
    }

    let modifiers = ui.input(|i| i.modifiers);

    // Polygon Lasso: each click places a vertex and the shape becomes a *live*
    // selection (marching ants) immediately — no separate "close" step. A
    // double-click (or Enter, handled by the shell) finalizes; a click near the
    // first vertex also ends the path.
    if app.state.tool_manager.active() == ToolKind::PolygonLasso {
        handle_polygon_lasso(app, response, doc_pos, modifiers);
        return;
    }

    let pressure = latest_pointer_pressure(ui);

    let doc = app.state.doc().clone();
    let mut dispatch_phase = |phase| {
        if let Some(cmd) = app.state.tool_manager.on_pointer(
            &doc,
            PointerEvent {
                doc_pos,
                phase,
                modifiers,
                pressure,
            },
        ) {
            let _ = dispatch::dispatch(&mut app.state, cmd);
        }
    };

    if response.drag_started_by(egui::PointerButton::Primary) {
        dispatch_phase(Phase::Down);
    }
    if response.dragged_by(egui::PointerButton::Primary) {
        dispatch_phase(Phase::Drag);
    }
    if response.drag_stopped_by(egui::PointerButton::Primary) {
        dispatch_phase(Phase::Up);
    }
    if response.clicked() {
        // A pure click (press + release without movement) dispatches a
        // zero-area Up so the active tool can clear its selection.
        dispatch_phase(Phase::Down);
        dispatch_phase(Phase::Up);
    }
}

/// Drive the Polygon Lasso: place a vertex on each click and keep the formed
/// shape committed as a single, coalesced live selection; finalize on
/// double-click.
fn handle_polygon_lasso(
    app: &mut OgreApp,
    response: &egui::Response,
    doc_pos: IVec2,
    modifiers: egui::Modifiers,
) {
    let doc = app.state.doc().clone();
    // Finalize the path on a double-click (the live selection stays committed).
    if response.double_clicked() {
        app.state.tool_manager.cancel_active();
        app.state.polygon_building = false;
        return;
    }
    if !response.clicked() {
        return;
    }
    // Place a vertex (or end the path if it clicked back near the first vertex).
    app.state
        .tool_manager
        .on_pointer(&doc, PointerEvent::new(doc_pos, Phase::Down, modifiers));
    sync_polygon_live_selection(app);
}

/// Sync the document selection to the Polygon Lasso's current vertices, coalesced
/// into one undo entry for the whole polygon.
fn sync_polygon_live_selection(app: &mut OgreApp) {
    let Some(sel) = app.state.tool_manager.polygon_live_selection() else {
        // Fewer than 3 vertices, or the path was just ended — stop coalescing.
        app.state.polygon_building = false;
        return;
    };
    if app.state.polygon_building {
        let coalesced = {
            let tab = app.state.current_tab_mut();
            match tab.history.undo_top_mut() {
                Some(top) => top.coalesce_selection(&mut tab.doc, &sel),
                None => false,
            }
        };
        if coalesced {
            app.state.dirty = true;
            return;
        }
    }
    let _ = dispatch::dispatch(
        &mut app.state,
        Box::new(ogre_core::SetSelectionCmd::new(sel)),
    );
    app.state.polygon_building = true;
}

/// Render the left-click-inside-selection menu, if open.
///
/// Opened by a non-dragging left click inside the selection (see
/// [`forward_pointer_events`]). The `fresh` guard skips close handling on the
/// opening frame so the click that opened it is not treated as a click-outside.
fn draw_left_click_menu(ui: &egui::Ui, app: &mut OgreApp) {
    let Some(pos) = app.state.left_click_menu else {
        return;
    };
    let items = crate::context_menu::left_click_menu_items(&app.state);
    let mut open = true;
    let mut chosen = None;
    egui::Popup::new(
        egui::Id::new("left_click_selection_menu"),
        ui.ctx().clone(),
        pos,
        ui.layer_id(),
    )
    .open_bool(&mut open)
    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
    .show(|ui| {
        ui.set_min_width(140.0);
        for item in items {
            if ui
                .add_enabled(item.enabled, egui::Button::new(item.label))
                .clicked()
            {
                chosen = Some(item.action);
            }
        }
    });

    if let Some(action) = chosen {
        crate::context_menu::apply_context_action(&mut app.state, action);
        app.state.left_click_menu = None;
        app.state.left_click_menu_fresh = false;
    } else if app.state.left_click_menu_fresh {
        // Ignore the opening click's own "outside" detection this one frame.
        app.state.left_click_menu_fresh = false;
    } else if !open {
        app.state.left_click_menu = None;
    }
}

/// Screen-pixel length of one marching-ants dash (a black dash + white dash is
/// two of these).
const ANTS_DASH: f32 = 4.0;
/// Marching-ants scroll speed in screen pixels per second.
const ANTS_SPEED: f32 = 16.0;

/// Draw the committed selection as a solid, always-visible marching-ants outline
/// from cached document-space `edges` (pixel-corner segments).
///
/// Drawn in two passes: a **solid black base** over the whole outline (so it
/// shows on light/busy artwork), then **marching white dashes** on top (so it
/// shows on dark artwork). This is the classic marching-ants look and is visible
/// on any background — unlike colouring each edge a single black-or-white, which
/// left runs of same-colour unit edges (an ellipse/lasso boundary) invisible
/// over a matching background.
fn draw_selection_outline(
    painter: &egui::Painter,
    edges: &[[IVec2; 2]],
    viewport: &ogre_gpu::Viewport,
    canvas_rect: egui::Rect,
    ppp: f32,
    time: f64,
) {
    if edges.is_empty() {
        return;
    }
    let origin = Vec2::new(canvas_rect.min.x, canvas_rect.min.y);
    let to_screen = |p: IVec2| {
        let s = doc_to_screen(p, viewport, ppp) + origin;
        egui::Pos2::new(s.x, s.y)
    };
    // Pass 1: solid black base over every edge.
    let black = egui::Stroke::new(1.0, egui::Color32::BLACK);
    for [a, b] in edges {
        painter.line_segment([to_screen(*a), to_screen(*b)], black);
    }
    // Pass 2: marching white dashes. The phase moves them along the boundary.
    let phase = (time as f32 * ANTS_SPEED).rem_euclid(2.0 * ANTS_DASH);
    for [a, b] in edges {
        draw_white_dashes(painter, to_screen(*a), to_screen(*b), phase);
    }
}

/// Draw the "on" half of the marching-dash pattern over one screen-space edge in
/// white. The dash phase comes from the dash's absolute screen position so the
/// pattern is continuous across edges and marches with `phase`.
fn draw_white_dashes(painter: &egui::Painter, a: egui::Pos2, b: egui::Pos2, phase: f32) {
    let delta = b - a;
    let len = delta.length();
    if len <= 0.0 {
        return;
    }
    let dir = delta / len;
    let white = egui::Stroke::new(1.0, egui::Color32::WHITE);
    let mut t = 0.0;
    while t < len {
        let start = t;
        let end = (t + ANTS_DASH).min(len);
        let mid = a + dir * ((start + end) * 0.5);
        // Even period → draw the white dash; odd → leave the black base showing.
        if (((mid.x + mid.y - phase) / ANTS_DASH).floor() as i64).rem_euclid(2) == 0 {
            painter.line_segment([a + dir * start, a + dir * end], white);
        }
        t += ANTS_DASH;
    }
}

/// Tipped paint-bucket cursor SVG (white fill, dark outline) shown over the
/// canvas when the bucket tool is active.
const PAINT_BUCKET_CURSOR_SVG: &[u8] = include_bytes!("../../assets/cursors/paint-bucket.svg");

/// Draw the paint-bucket cursor at the pointer when the bucket tool is active
/// and the pointer is over the canvas, hiding the OS cursor. The glyph's
/// pour-point (lower-left, the SVG hotspot at ~`(3, 21)/24`) is aligned to the
/// exact pixel the fill will seed.
fn draw_paint_bucket_cursor(ui: &egui::Ui, app: &OgreApp, response: &egui::Response) {
    if app.state.tool_manager.active() != ToolKind::PaintBucket {
        return;
    }
    let Some(pos) = response.hover_pos() else {
        return;
    };
    ui.ctx().set_cursor_icon(egui::CursorIcon::None);

    const SIZE: f32 = 24.0;
    // SVG hotspot in viewBox units, normalised to [0,1].
    let hot = egui::vec2(3.0 / 24.0, 21.0 / 24.0);
    let top_left = pos - egui::vec2(SIZE * hot.x, SIZE * hot.y);
    let rect = egui::Rect::from_min_size(top_left, egui::vec2(SIZE, SIZE));
    egui::Image::from_bytes(
        "bytes://ogre/cursors/paint-bucket.svg",
        PAINT_BUCKET_CURSOR_SVG,
    )
    .paint_at(ui, rect);
}

/// Sample the visible composited color at document pixel `doc_pos`, averaged
/// over a `(2*radius+1)` square neighborhood (clipped to the canvas).
///
/// Returns `None` when the centre is outside the canvas. Uses the fast
/// single-pixel CPU sampler so dragging the eyedropper does not re-composite
/// the whole document for every pixel.
fn sample_composite(
    doc: &ogre_core::Document,
    doc_pos: IVec2,
    radius: i32,
) -> Option<ogre_core::Rgba32F> {
    if radius <= 0 {
        return ogre_core::compositor::sample_document_pixel(doc, doc_pos);
    }
    // Average all in-canvas pixels in the neighborhood; skip transparent
    // (out-of-canvas) samples so edges don't pull toward black.
    let mut acc = ogre_core::Rgba32F::TRANSPARENT;
    let mut count = 0u32;
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            if let Some(c) = ogre_core::compositor::sample_document_pixel(
                doc,
                IVec2::new(doc_pos.x + dx, doc_pos.y + dy),
            ) {
                acc = ogre_core::Rgba32F::new(acc.r + c.r, acc.g + c.g, acc.b + c.b, acc.a + c.a);
                count += 1;
            }
        }
    }
    if count == 0 {
        return None;
    }
    let n = count as f32;
    Some(ogre_core::Rgba32F::new(
        acc.r / n,
        acc.g / n,
        acc.b / n,
        acc.a / n,
    ))
}

/// Extract the most recent touch pressure (if any) from egui's input events.
///
/// Returns `(1.0, Vec2::ZERO)` for mouse input. Tilt is not currently exposed
/// by egui, so it is always reported as zero.
fn latest_pointer_pressure(ui: &egui::Ui) -> f32 {
    let mut pressure = 1.0f32;
    ui.input(|i| {
        for ev in &i.events {
            if let egui::Event::Touch { force: Some(f), .. } = ev {
                pressure = f.clamp(0.0, 1.0);
            }
        }
    });
    pressure
}

/// Sync the GPU renderer's viewport with `app.state.viewport()` and mark dirty.
fn renderer_set_viewport(app: &mut OgreApp) {
    if let Some(renderer) = &mut app.renderer {
        renderer.set_viewport(*app.state.viewport());
    }
    app.state.dirty = true;
}

/// Document-pixel coordinate under the pointer, or `None` when the pointer is
/// not over the canvas image.
///
/// Returns `Some` only when `hover` is inside `canvas_rect` (the canvas widget)
/// AND the mapped document position lies within `[0, canvas_w) × [0, canvas_h)`.
pub fn cursor_doc_pos(
    hover: Option<egui::Pos2>,
    canvas_rect: egui::Rect,
    viewport: &ogre_gpu::Viewport,
    ppp: f32,
    canvas_w: u32,
    canvas_h: u32,
) -> Option<IVec2> {
    let pos = hover?;
    if !canvas_rect.contains(pos) {
        return None;
    }
    let rel = Vec2::new(pos.x - canvas_rect.min.x, pos.y - canvas_rect.min.y);
    let d = screen_to_doc(rel, viewport, ppp);
    if (0..canvas_w as i32).contains(&d.x) && (0..canvas_h as i32).contains(&d.y) {
        Some(d)
    } else {
        None
    }
}

/// Update the document-space cursor position while hovering the canvas.
///
/// The value is displayed by the global status bar rather than drawn inline.
fn update_cursor_state(ui: &mut egui::Ui, app: &mut OgreApp, canvas_rect: egui::Rect, ppp: f32) {
    let hover = ui.input(|i| i.pointer.hover_pos());
    app.state.cursor_doc_pos = cursor_doc_pos(
        hover,
        canvas_rect,
        app.state.viewport(),
        ppp,
        app.state.doc().canvas.w,
        app.state.doc().canvas.h,
    );
}

/// Map a screen position (logical pixels, relative to the canvas top-left) to a
/// document pixel coordinate.
///
/// The conversion accounts for `pixels_per_point` so that egui logical pixels are
/// first turned into physical screen pixels before the viewport zoom/pan is
/// applied. The returned coordinate is floored to the document pixel it covers.
pub fn screen_to_doc(
    screen_pos: Vec2,
    viewport: &ogre_gpu::Viewport,
    pixels_per_point: f32,
) -> IVec2 {
    let physical = screen_pos * pixels_per_point;
    let doc_f = viewport.pan + physical / viewport.zoom;
    IVec2::new(doc_f.x.floor() as i32, doc_f.y.floor() as i32)
}

/// Map a document pixel coordinate to a screen position (logical pixels, relative
/// to the canvas top-left).
///
/// This is the inverse of [`screen_to_doc`]: the result is in egui logical pixels
/// and must be added to the canvas rectangle's top-left corner for absolute
/// screen drawing.
pub fn doc_to_screen(doc_pos: IVec2, viewport: &ogre_gpu::Viewport, pixels_per_point: f32) -> Vec2 {
    let physical = (doc_pos.as_vec2() - viewport.pan) * viewport.zoom;
    physical / pixels_per_point
}

/// Map a continuous document-space position to a screen position (logical
/// pixels, relative to the canvas top-left).
///
/// This is the floating-point variant of [`doc_to_screen`] and preserves
/// sub-pixel placement for overlay drawing.
pub fn doc_to_screen_f(
    doc_pos: Vec2,
    viewport: &ogre_gpu::Viewport,
    pixels_per_point: f32,
) -> Vec2 {
    let physical = (doc_pos - viewport.pan) * viewport.zoom;
    physical / pixels_per_point
}

/// Draw a subtle grid overlay on the canvas.
fn draw_grid_overlay(
    painter: &egui::Painter,
    viewport: &ogre_gpu::Viewport,
    canvas_rect: egui::Rect,
    pixels_per_point: f32,
    spacing: u32,
) {
    // A zero spacing means "no grid"; `grid_lines_in_range` yields nothing.
    if spacing == 0 {
        return;
    }
    let top_left = screen_to_doc(Vec2::new(0.0, 0.0), viewport, pixels_per_point);
    let bottom_right = screen_to_doc(
        Vec2::new(canvas_rect.width(), canvas_rect.height()),
        viewport,
        pixels_per_point,
    );

    let base = painter
        .ctx()
        .global_style()
        .visuals
        .widgets
        .noninteractive
        .fg_stroke
        .color;
    let grid_color = egui::Color32::from_rgba_premultiplied(base.r(), base.g(), base.b(), 30);
    let stroke = egui::Stroke::new(1.0, grid_color);

    for x in grid_lines_in_range(top_left.x as f32, bottom_right.x as f32, spacing) {
        let screen_x = doc_to_screen_f(Vec2::new(x as f32, 0.0), viewport, pixels_per_point).x
            + canvas_rect.min.x;
        painter.line_segment(
            [
                egui::Pos2::new(screen_x, canvas_rect.min.y),
                egui::Pos2::new(screen_x, canvas_rect.max.y),
            ],
            stroke,
        );
    }
    for y in grid_lines_in_range(top_left.y as f32, bottom_right.y as f32, spacing) {
        let screen_y = doc_to_screen_f(Vec2::new(0.0, y as f32), viewport, pixels_per_point).y
            + canvas_rect.min.y;
        painter.line_segment(
            [
                egui::Pos2::new(canvas_rect.min.x, screen_y),
                egui::Pos2::new(canvas_rect.max.x, screen_y),
            ],
            stroke,
        );
    }
}

/// Zoom toward the cursor position (logical pixels, relative to canvas top-left).
///
/// The document point currently under `cursor_screen` stays fixed after the zoom.
pub fn apply_zoom(
    viewport: &mut ogre_gpu::Viewport,
    cursor_screen: Vec2,
    scroll_delta: f32,
    pixels_per_point: f32,
) {
    let cursor_physical = cursor_screen * pixels_per_point;
    let cursor_doc = viewport.pan + cursor_physical / viewport.zoom;

    let factor = 2.0_f32.powf(scroll_delta / ZOOM_DIVISOR);
    let new_zoom = viewport.zoom * factor;
    let new_pan = cursor_doc - cursor_physical / new_zoom;

    *viewport = ogre_gpu::Viewport::new(new_pan, new_zoom);
}

/// Pan the viewport by a screen-space drag delta (physical pixels).
///
/// `drag_delta` is the distance the pointer moved on screen in physical pixels.
/// The viewport's `pan` is shifted by `drag_delta / zoom` in document pixels.
pub fn apply_pan(viewport: &mut ogre_gpu::Viewport, drag_delta: Vec2) {
    viewport.pan += drag_delta / viewport.zoom;
}

/// Lazily construct the shared GPU renderer from eframe's wgpu render state.
///
/// This should be called once per app instance, before the canvas panel is shown.
/// # Panics
/// Panics if `frame.wgpu_render_state()` is `None`.
pub fn ensure_renderer(app: &mut OgreApp, frame: &mut eframe::Frame) {
    if app.renderer.is_some() {
        return;
    }

    let render_state = frame
        .wgpu_render_state()
        .expect("eframe wgpu render state is required; run ogre with Renderer::Wgpu");

    let device = render_state.device.clone();
    let queue = render_state.queue.clone();
    let info = render_state.adapter.get_info();

    app.renderer = Some(ogre_gpu::CanvasRenderer::new(device, queue, info));
    app.egui_renderer = Some(Arc::clone(&render_state.renderer));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_doc_pos_none_when_not_hovering() {
        let vp = ogre_gpu::Viewport::new(Vec2::ZERO, 1.0);
        let rect = egui::Rect::from_min_size(egui::pos2(100.0, 100.0), egui::vec2(500.0, 400.0));
        // No pointer.
        assert_eq!(cursor_doc_pos(None, rect, &vp, 1.0, 1920, 1080), None);
        // Pointer outside the canvas rect (over a panel).
        assert_eq!(
            cursor_doc_pos(Some(egui::pos2(10.0, 10.0)), rect, &vp, 1.0, 1920, 1080),
            None
        );
    }

    #[test]
    fn cursor_doc_pos_some_when_over_image() {
        // Identity viewport (pan 0, zoom 1), ppp 1 → doc = pos - rect.min.
        let vp = ogre_gpu::Viewport::new(Vec2::ZERO, 1.0);
        let rect = egui::Rect::from_min_size(egui::pos2(100.0, 100.0), egui::vec2(500.0, 400.0));
        let got = cursor_doc_pos(Some(egui::pos2(110.0, 120.0)), rect, &vp, 1.0, 1920, 1080);
        assert_eq!(got, Some(ogre_core::IVec2::new(10, 20)));
    }

    #[test]
    fn cursor_doc_pos_none_when_outside_image_bounds() {
        // Inside the canvas rect, but the doc maps outside a tiny 5x5 canvas.
        let vp = ogre_gpu::Viewport::new(Vec2::ZERO, 1.0);
        let rect = egui::Rect::from_min_size(egui::pos2(100.0, 100.0), egui::vec2(500.0, 400.0));
        assert_eq!(
            cursor_doc_pos(Some(egui::pos2(110.0, 120.0)), rect, &vp, 1.0, 5, 5),
            None
        );
    }
}
