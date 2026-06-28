// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Collapsible, Grexa-styled Tools palette.

use egui::{CornerRadius, RichText};

use crate::prefs::{push_color, MAX_RECENT_COLORS};
use crate::state::AppState;
use crate::theme;
use crate::tools::{
    normalize_sidebar_order, SidebarSection, ToolFamily, ToolKind, VectorCommitMode,
};

/// Size of the drawn six-dot reorder grip (2 columns × 3 rows).
pub(crate) const SIX_DOT_GRIP_SIZE: egui::Vec2 = egui::vec2(9.0, 12.0);

/// Paint a six-dot grip inside `rect`, centered and tinted with `color`.
pub(crate) fn paint_six_dot_grip(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    const DOT_R: f32 = 0.9;
    const COL_GAP: f32 = 2.5;
    const ROW_GAP: f32 = 2.5;
    let top_left = rect.center() - SIX_DOT_GRIP_SIZE * 0.5;
    for row in 0..3 {
        for col in 0..2 {
            let cx = top_left.x + DOT_R + col as f32 * (DOT_R * 2.0 + COL_GAP);
            let cy = top_left.y + DOT_R + row as f32 * (DOT_R * 2.0 + ROW_GAP);
            painter.circle_filled(egui::pos2(cx, cy), DOT_R, color);
        }
    }
}

/// Flip the collapsed flag (extracted for testing).
pub fn toggle_collapsed(collapsed: &mut bool) {
    *collapsed = !*collapsed;
}

/// Render the Tools panel.
///
/// Shows a header with a hamburger toggle, grouped tool rows with Breeze icons,
/// and brush settings when a paint tool is active.
pub fn render(ui: &mut egui::Ui, state: &mut AppState) {
    let palette = theme::resolve(state.preferences.theme, ui.ctx());
    let collapsed = state.preferences.tools_collapsed;

    // Breathing room above the hamburger so it isn't flush against the menu bar.
    ui.add_space(theme::SPACE_S);

    // Header: hamburger toggle only.
    ui.horizontal(|ui| {
        if ui
            .button("\u{2630}")
            .on_hover_text("Collapse/expand")
            .clicked()
        {
            toggle_collapsed(&mut state.preferences.tools_collapsed);
        }
    });
    ui.separator();

    egui::ScrollArea::vertical()
        .id_salt("tools_sidebar_scroll")
        .auto_shrink([false, true])
        .show(ui, |ui| {
            // Safety cleanup: finalize any stale drag if the primary button is up or
            // the sidebar is collapsed (so no header rows were rendered this frame).
            if state.dragging_sidebar_section.is_some()
                && (collapsed || !ui.input(|i| i.pointer.primary_down()))
            {
                state.dragging_sidebar_section = None;
                state.dragging_sidebar_section_anchors = None;
                if let Some(path) = crate::prefs::Preferences::config_path() {
                    let _ = state.preferences.save(&path);
                }
            }

            // Ensure the persisted order is always valid, even if a bug ever produces
            // duplicates or missing sections at runtime.
            normalize_sidebar_order(&mut state.preferences.sidebar_order);

            let order = state.preferences.sidebar_order.clone();
            let mut header_rects: Vec<egui::Rect> = Vec::with_capacity(order.len());
            let mut released_section: Option<SidebarSection> = None;
            let mut started_section: Option<SidebarSection> = None;

            for section in order.iter().copied() {
                if !collapsed {
                    let header = section_header(ui, state, section);
                    header_rects.push(header.rect);
                    if header.drag_started() {
                        started_section = Some(section);
                    }
                    if header.drag_stopped() {
                        released_section = Some(section);
                    }
                }

                match section {
                    SidebarSection::Color => {
                        if !collapsed {
                            render_color(ui, state);
                            ui.add_space(theme::SPACE_S);
                        }
                    }
                    SidebarSection::Swatches => {
                        if !collapsed {
                            render_swatches(ui, state);
                            ui.add_space(theme::SPACE_S);
                        }
                    }
                    _ => {
                        // One row per tool family that has a sibling in this group
                        // (mode-switched siblings share a slot with a right-click
                        // flyout). Iterate ToolKind in palette order and emit a row the
                        // first time each family appears, so each family renders
                        // exactly once per group.
                        if let Some(group) = section.as_tool_group() {
                            let mut seen: ahash::AHashSet<ToolFamily> = ahash::AHashSet::new();
                            for kind in ToolKind::ALL.into_iter().filter(|k| k.group() == group) {
                                if seen.insert(kind.family()) {
                                    family_row(ui, state, kind.family());
                                }
                            }
                            if !collapsed {
                                ui.add_space(theme::SPACE_S);
                            }
                        }
                    }
                }
            }

            // Capture stable anchor coordinates when a drag starts.
            if let Some(source) = started_section {
                state.dragging_sidebar_section = Some(source);
                let anchors: Vec<f32> = header_rects
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| order.get(*i).copied() != Some(source))
                    .map(|(_, r)| r.center().y)
                    .collect();
                state.dragging_sidebar_section_anchors = Some(anchors);
            }

            // Live reorder while dragging, using the stable anchors to avoid flicker.
            if let Some(source) = state.dragging_sidebar_section {
                if let Some(ref anchors) = state.dragging_sidebar_section_anchors {
                    if let Some(pointer) = ui.input(|i| i.pointer.hover_pos()) {
                        if let Some(new_order) =
                            reorder_at_pointer(&order, source, anchors, pointer.y)
                        {
                            state.preferences.sidebar_order = new_order;
                        }
                    }
                }
            }

            // Stop dragging and persist on release.
            if released_section.is_some() {
                state.dragging_sidebar_section = None;
                state.dragging_sidebar_section_anchors = None;
                if let Some(path) = crate::prefs::Preferences::config_path() {
                    let _ = state.preferences.save(&path);
                }
            }

            // Brush settings (expanded only), unchanged behavior.
            if !collapsed {
                ui.separator();
                if let Some(tool) = state.tool_manager.active_paint_settings_mut() {
                    if tool.is_ui_editable() {
                        crate::shell::render_brush_settings(ui, tool);
                        // Brush preset row: 5 slots. Left-click loads, right-click saves
                        // the current settings into that slot.
                        let presets = state.preferences.brush_presets.clone();
                        if let Some(action) = render_brush_presets(ui, &presets) {
                            if action.save {
                                let current = *tool.settings();
                                crate::prefs::set_brush_preset(
                                    &mut state.preferences.brush_presets,
                                    action.index,
                                    current,
                                );
                            } else if let Some(s) = presets.get(action.index) {
                                *tool.settings_mut() = *s;
                            }
                        }
                    }
                }
                // Paint-bucket fuzzy-match tolerance.
                if let Some(tolerance) = state.tool_manager.paint_bucket_tolerance_mut() {
                    ui.label(
                        RichText::new("FILL")
                            .size(theme::TEXT_CAPTION)
                            .color(palette.separator_strong),
                    );
                    ui.add(
                        egui::Slider::new(tolerance, 0.0..=1.0)
                            .text("Tolerance")
                            .step_by(0.01),
                    )
                    .on_hover_text("Higher fills more similar colors before stopping at an edge");
                }
                // Magic-wand tolerance and contiguity.
                if let Some((tolerance, contiguous)) = state.tool_manager.magic_wand_settings_mut()
                {
                    ui.label(
                        RichText::new("MAGIC WAND")
                            .size(theme::TEXT_CAPTION)
                            .color(palette.separator_strong),
                    );
                    ui.add(
                        egui::Slider::new(tolerance, 0.0..=1.0)
                            .text("Tolerance")
                            .step_by(0.01),
                    )
                    .on_hover_text("Higher selects a wider range of similar colors");
                    ui.checkbox(contiguous, "Contiguous").on_hover_text(
                        "Select only the connected region (off = all matching colors)",
                    );
                }
                // Quick Selection: brush radius, tolerance, sample-all-layers.
                if let Some((radius, tolerance, sample_all)) =
                    state.tool_manager.quick_select_settings_mut()
                {
                    ui.label(
                        RichText::new("QUICK SELECT")
                            .size(theme::TEXT_CAPTION)
                            .color(palette.separator_strong),
                    );
                    ui.add(egui::Slider::new(radius, 1..=200).text("Brush radius"));
                    ui.add(
                        egui::Slider::new(tolerance, 0.0..=1.0)
                            .text("Tolerance")
                            .step_by(0.01),
                    )
                    .on_hover_text("Higher grows the selection to a wider range of colors");
                    ui.checkbox(sample_all, "Sample all layers").on_hover_text(
                        "Grow against the merged composite (off = active layer only)",
                    );
                }
                // Gradient kind / reverse / opacity.
                if let Some(gs) = state.tool_manager.gradient_settings_mut() {
                    ui.label(
                        RichText::new("GRADIENT")
                            .size(theme::TEXT_CAPTION)
                            .color(palette.separator_strong),
                    );
                    egui::ComboBox::from_id_salt("gradient_kind")
                        .selected_text(kind_label(gs.kind))
                        .show_ui(ui, |ui| {
                            use ogre_core::GradientKind;
                            ui.selectable_value(&mut gs.kind, GradientKind::Linear, "Linear");
                            ui.selectable_value(&mut gs.kind, GradientKind::Radial, "Radial");
                            ui.selectable_value(&mut gs.kind, GradientKind::Conical, "Conical");
                            ui.selectable_value(&mut gs.kind, GradientKind::Diamond, "Diamond");
                        });
                    ui.checkbox(&mut gs.reverse, "Reverse")
                        .on_hover_text("Swap the foreground/background endpoint mapping");
                    ui.add(
                        egui::Slider::new(&mut gs.opacity, 0.0..=1.0)
                            .text("Opacity")
                            .step_by(0.01),
                    )
                    .on_hover_text("Overall opacity applied to the gradient pixels");
                    egui::ComboBox::from_id_salt("gradient_wrap")
                        .selected_text(wrap_label(gs.wrap))
                        .show_ui(ui, |ui| {
                            use ogre_core::WrapMode;
                            ui.selectable_value(&mut gs.wrap, WrapMode::Clamp, "Clamp");
                            ui.selectable_value(&mut gs.wrap, WrapMode::Repeat, "Repeat");
                            ui.selectable_value(&mut gs.wrap, WrapMode::Reflect, "Reflect");
                        });
                }
                // Free Transform mode selector.
                if let Some(mode) = state.tool_manager.free_transform_mode_mut() {
                    ui.label(
                        RichText::new("TRANSFORM")
                            .size(theme::TEXT_CAPTION)
                            .color(palette.separator_strong),
                    );
                    egui::ComboBox::from_id_salt("free_transform_mode")
                        .selected_text(transform_mode_label(*mode))
                        .show_ui(ui, |ui| {
                            use crate::tools::transform::FreeTransformMode;
                            ui.selectable_value(
                                mode,
                                FreeTransformMode::ScaleRotate,
                                "Scale / Rotate",
                            );
                            ui.selectable_value(mode, FreeTransformMode::Skew, "Skew");
                            ui.selectable_value(mode, FreeTransformMode::Distort, "Distort");
                            ui.selectable_value(
                                mode,
                                FreeTransformMode::Perspective,
                                "Perspective",
                            );
                            ui.selectable_value(mode, FreeTransformMode::Warp, "Warp");
                        });
                    // Numeric affine fields appear only in ScaleRotate mode once a
                    // transform preview is active (start a drag, then fine-tune here).
                    if *mode == crate::tools::transform::FreeTransformMode::ScaleRotate {
                        if let Some(tool) = state.tool_manager.free_transform_tool_mut() {
                            if let Some(params) = tool.numeric_params() {
                                let mut x = params.translate_x;
                                let mut y = params.translate_y;
                                let mut scale_pct = params.scale * 100.0;
                                let mut rot = params.rotation_deg;
                                let mut next: Option<crate::tools::transform::TransformParams> =
                                    None;
                                ui.horizontal(|ui| {
                                    ui.label("X:");
                                    if ui
                                        .add(
                                            egui::DragValue::new(&mut x)
                                                .speed(1.0)
                                                .fixed_decimals(0),
                                        )
                                        .changed()
                                    {
                                        next = Some(params);
                                    }
                                });
                                ui.horizontal(|ui| {
                                    ui.label("Y:");
                                    if ui
                                        .add(
                                            egui::DragValue::new(&mut y)
                                                .speed(1.0)
                                                .fixed_decimals(0),
                                        )
                                        .changed()
                                    {
                                        next = Some(params);
                                    }
                                });
                                ui.horizontal(|ui| {
                                    ui.label("Scale:");
                                    if ui
                                        .add(
                                            egui::DragValue::new(&mut scale_pct)
                                                .speed(1.0)
                                                .range(0.1..=1000.0)
                                                .suffix("%"),
                                        )
                                        .changed()
                                    {
                                        next = Some(params);
                                    }
                                });
                                ui.horizontal(|ui| {
                                    ui.label("Rotate:");
                                    if ui
                                        .add(
                                            egui::DragValue::new(&mut rot)
                                                .speed(1.0)
                                                .range(-360.0..=360.0)
                                                .suffix("\u{00B0}"),
                                        )
                                        .changed()
                                    {
                                        next = Some(params);
                                    }
                                });
                                if next.is_some() {
                                    tool.set_numeric_params(
                                        crate::tools::transform::TransformParams {
                                            translate_x: x,
                                            translate_y: y,
                                            scale: scale_pct / 100.0,
                                            rotation_deg: rot,
                                        },
                                    );
                                }
                            }
                        }
                    }
                }

                // Eyedropper sample size.
                if let Some(sample) = state.tool_manager.eyedropper_sample_mut() {
                    ui.label(
                        RichText::new("EYEDROPPER")
                            .size(theme::TEXT_CAPTION)
                            .color(palette.separator_strong),
                    );
                    use crate::tools::fill::EyedropperSample;
                    egui::ComboBox::from_id_salt("eyedropper_sample")
                        .selected_text(match *sample {
                            EyedropperSample::Point => "Point sample",
                            EyedropperSample::Average3x3 => "3×3 average",
                            EyedropperSample::Average5x5 => "5×5 average",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(sample, EyedropperSample::Point, "Point sample");
                            ui.selectable_value(
                                sample,
                                EyedropperSample::Average3x3,
                                "3×3 average",
                            );
                            ui.selectable_value(
                                sample,
                                EyedropperSample::Average5x5,
                                "5×5 average",
                            );
                        });
                }

                // Stateless finger tools: Blur/Sharpen/Dodge/Burn/Sponge/Color-Repl.
                let active_kind = state.tool_manager.active();
                if let Some(tool) = state.tool_manager.active_finger_tool_mut() {
                    ui.label(
                        RichText::new(finger_label(active_kind))
                            .size(theme::TEXT_CAPTION)
                            .color(palette.separator_strong),
                    );
                    {
                        let s = tool.settings_mut();
                        ui.add(egui::Slider::new(&mut s.size, 1.0..=400.0).text("Size"));
                        ui.add(egui::Slider::new(&mut s.opacity, 0.0..=1.0).text("Opacity"));
                    }
                    match tool.op_mut() {
                        crate::tools::FingerOp::Sharpen { strength } => {
                            ui.add(egui::Slider::new(strength, 0.0..=1.0).text("Strength"));
                        }
                        crate::tools::FingerOp::Dodge { range, exposure }
                        | crate::tools::FingerOp::Burn { range, exposure } => {
                            ui.add(egui::Slider::new(exposure, 0.0..=0.999).text("Exposure"));
                            egui::ComboBox::from_id_salt("finger_range")
                                .selected_text(range_label(*range))
                                .show_ui(ui, |ui| {
                                    use ogre_core::Range;
                                    ui.selectable_value(range, Range::Shadows, "Shadows");
                                    ui.selectable_value(range, Range::Midtones, "Midtones");
                                    ui.selectable_value(range, Range::Highlights, "Highlights");
                                });
                        }
                        crate::tools::FingerOp::Sponge { exposure, saturate } => {
                            ui.add(egui::Slider::new(exposure, 0.0..=1.0).text("Exposure"));
                            ui.checkbox(saturate, "Saturate (off = desaturate)");
                        }
                        crate::tools::FingerOp::Blur | crate::tools::FingerOp::Recolor { .. } => {}
                    }
                }
                // Smudge (separate, stateful tool).
                if let Some((settings, strength, finger_painting)) =
                    state.tool_manager.smudge_controls_mut()
                {
                    ui.label(
                        RichText::new("SMUDGE")
                            .size(theme::TEXT_CAPTION)
                            .color(palette.separator_strong),
                    );
                    ui.add(egui::Slider::new(&mut settings.size, 1.0..=400.0).text("Size"));
                    ui.add(egui::Slider::new(strength, 0.0..=1.0).text("Strength"));
                    ui.checkbox(finger_painting, "Finger painting");
                }

                // Shape controls (fill / stroke / width / commit mode).
                if let Some((fill, stroke, width, mode)) =
                    state.tool_manager.active_shape_controls_mut()
                {
                    ui.label(
                        RichText::new("SHAPE")
                            .size(theme::TEXT_CAPTION)
                            .color(palette.separator_strong),
                    );
                    ui.horizontal(|ui| {
                        ui.label("Fill:");
                        if let Some(c) = color_swatch(ui, *fill, "Shape fill color") {
                            *fill = c;
                        }
                        ui.label("Stroke:");
                        if let Some(c) = color_swatch(ui, *stroke, "Shape stroke color") {
                            *stroke = c;
                        }
                    });
                    ui.add(egui::Slider::new(width, 0..=20).text("Stroke width"));
                    render_commit_mode(ui, mode);
                }

                // Pen controls (fill / stroke / width / fill closed / commit mode).
                if let Some((fill, stroke, width, fill_closed, mode)) =
                    state.tool_manager.pen_controls_mut()
                {
                    ui.label(
                        RichText::new("PEN")
                            .size(theme::TEXT_CAPTION)
                            .color(palette.separator_strong),
                    );
                    ui.horizontal(|ui| {
                        ui.label("Fill:");
                        if let Some(c) = color_swatch(ui, *fill, "Pen fill color") {
                            *fill = c;
                        }
                        ui.label("Stroke:");
                        if let Some(c) = color_swatch(ui, *stroke, "Pen stroke color") {
                            *stroke = c;
                        }
                    });
                    ui.add(egui::Slider::new(width, 0..=20).text("Stroke width"));
                    ui.checkbox(fill_closed, "Fill closed paths");
                    render_commit_mode(ui, mode);
                }

                // Type commit mode.
                if let Some(mode) = state.tool_manager.type_mode_mut() {
                    ui.label(
                        RichText::new("TYPE")
                            .size(theme::TEXT_CAPTION)
                            .color(palette.separator_strong),
                    );
                    render_commit_mode(ui, mode);
                }
            }
        });
}

/// Render a Vector / Pixels radio group for vector-capable tools.
fn render_commit_mode(ui: &mut egui::Ui, mode: &mut VectorCommitMode) {
    ui.horizontal(|ui| {
        ui.radio_value(mode, VectorCommitMode::Vector, "Vector");
        ui.radio_value(mode, VectorCommitMode::Pixels, "Pixels");
    })
    .response
    .on_hover_text("Vector: editable layer · Pixels: rasterize into active layer");
}

/// Sidebar caption for the active finger tool.
fn finger_label(kind: crate::tools::ToolKind) -> &'static str {
    use crate::tools::ToolKind;
    match kind {
        ToolKind::Blur => "BLUR",
        ToolKind::Sharpen => "SHARPEN",
        ToolKind::Dodge => "DODGE",
        ToolKind::Burn => "BURN",
        ToolKind::Sponge => "SPONGE",
        ToolKind::ColorReplacement => "COLOR REPLACEMENT",
        _ => "FINGER",
    }
}

/// Display label for a tonal range.
fn range_label(r: ogre_core::Range) -> &'static str {
    use ogre_core::Range;
    match r {
        Range::Shadows => "Shadows",
        Range::Midtones => "Midtones",
        Range::Highlights => "Highlights",
    }
}

/// Display label for a gradient kind (sidebar combo).
fn kind_label(k: ogre_core::GradientKind) -> &'static str {
    use ogre_core::GradientKind;
    match k {
        GradientKind::Linear => "Linear",
        GradientKind::Radial => "Radial",
        GradientKind::Conical => "Conical",
        GradientKind::Diamond => "Diamond",
    }
}

/// Display label for a gradient wrap mode.
fn wrap_label(w: ogre_core::WrapMode) -> &'static str {
    use ogre_core::WrapMode;
    match w {
        WrapMode::Clamp => "Clamp",
        WrapMode::Repeat => "Repeat",
        WrapMode::Reflect => "Reflect",
    }
}

/// Display label for a Free Transform mode.
fn transform_mode_label(m: crate::tools::transform::FreeTransformMode) -> &'static str {
    use crate::tools::transform::FreeTransformMode;
    match m {
        FreeTransformMode::ScaleRotate => "Scale / Rotate",
        FreeTransformMode::Skew => "Skew",
        FreeTransformMode::Distort => "Distort",
        FreeTransformMode::Perspective => "Perspective",
        FreeTransformMode::Warp => "Warp",
    }
}

/// Foreground/background color swatches, swap/reset buttons, and recent colors.
///
/// egui's `*_rgba_unmultiplied` color button works in **linear** straight-alpha
/// space — the same space `Rgba32F` uses — so the round-trip is a plain copy.
fn render_color(ui: &mut egui::Ui, state: &mut AppState) {
    ui.horizontal(|ui| {
        // Make the color swatches (which size to `interact_size`) as tall as the
        // swap/reset buttons (text + button padding) so the row lines up.
        let row_h =
            ui.text_style_height(&egui::TextStyle::Button) + ui.spacing().button_padding.y * 2.0;
        ui.spacing_mut().interact_size.y = row_h;

        if let Some(c) = color_swatch(ui, state.foreground, "Foreground color") {
            state.foreground = c;
            push_color(
                &mut state.preferences.recent_colors,
                [c.r, c.g, c.b, c.a],
                MAX_RECENT_COLORS,
            );
        }
        if let Some(c) = color_swatch(ui, state.background, "Background color") {
            state.background = c;
        }
        // Swap icon (two arrows).
        let swap = crate::icons::swap_image(16.0, ui.visuals().text_color());
        if ui
            .add(egui::Button::image(swap))
            .on_hover_text("Swap colors")
            .clicked()
        {
            std::mem::swap(&mut state.foreground, &mut state.background);
        }
        if ui
            .button("\u{21BB}")
            .on_hover_text("Reset to black/white")
            .clicked()
        {
            state.foreground = ogre_core::Rgba32F::new(0.0, 0.0, 0.0, 1.0);
            state.background = ogre_core::Rgba32F::new(1.0, 1.0, 1.0, 1.0);
        }
    });

    // Recent colors: click to load into the foreground.
    if !state.preferences.recent_colors.is_empty() {
        let recents = state.preferences.recent_colors.clone();
        if let Some(click) = color_grid(ui, "RECENT", &recents) {
            if let Some(c) = recents.get(click.index) {
                state.foreground = ogre_core::Rgba32F::new(c[0], c[1], c[2], c[3]);
            }
        }
    }
}

/// Saved color swatches with an Add button.
fn render_swatches(ui: &mut egui::Ui, state: &mut AppState) {
    // Saved swatches: click to load into foreground, right-click to remove.
    let swatches = state.preferences.swatches.clone();
    // The section header already says "SWATCHES"; don't repeat it here.
    if let Some(click) = color_grid(ui, "", &swatches) {
        match click {
            // Left-click loads; right-click removes.
            GridClick {
                index,
                right_click: false,
            } => {
                if let Some(c) = swatches.get(index) {
                    state.foreground = ogre_core::Rgba32F::new(c[0], c[1], c[2], c[3]);
                }
            }
            GridClick {
                index,
                right_click: true,
            } => {
                if index < state.preferences.swatches.len() {
                    state.preferences.swatches.remove(index);
                }
            }
        }
    }
    // Use the SVG plus icon.
    let add = crate::icons::add_image(16.0, ui.visuals().text_color());
    if ui
        .add(egui::Button::image_and_text(add, "Add swatch"))
        .on_hover_text("Save the current foreground color as a swatch")
        .clicked()
    {
        let c = state.foreground;
        push_color(&mut state.preferences.swatches, [c.r, c.g, c.b, c.a], 64);
    }
}

/// A click on a brush-preset slot.
struct PresetClick {
    index: usize,
    save: bool,
}

/// Render a row of `MAX_BRUSH_PRESETS` numbered preset buttons. Populated slots
/// are filled; empty slots are outlined. Left-click loads, right-click saves.
fn render_brush_presets(
    ui: &mut egui::Ui,
    presets: &[ogre_core::BrushSettings],
) -> Option<PresetClick> {
    let mut clicked: Option<PresetClick> = None;
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("PRESETS")
                .size(theme::TEXT_CAPTION)
                .color(ui.visuals().widgets.inactive.bg_stroke.color),
        );
        for i in 0..crate::prefs::MAX_BRUSH_PRESETS {
            let populated = i < presets.len();
            let label = format!("{}", i + 1);
            let btn = egui::Button::new(label).min_size(egui::Vec2::splat(18.0));
            let btn = if populated {
                btn.fill(ui.visuals().widgets.active.bg_fill)
                    .stroke(ui.visuals().widgets.active.bg_stroke)
            } else {
                btn
            };
            let resp = ui.add(btn).on_hover_text(if populated {
                "Click to load; right-click to overwrite"
            } else {
                "Right-click to save current brush here"
            });
            if resp.secondary_clicked() {
                clicked = Some(PresetClick {
                    index: i,
                    save: true,
                });
            } else if resp.clicked() && populated {
                clicked = Some(PresetClick {
                    index: i,
                    save: false,
                });
            }
        }
    });
    clicked
}

/// A click on a color-grid cell.
struct GridClick {
    index: usize,
    right_click: bool,
}

/// Render `caption` followed by a wrapping row of color squares. Returns the
/// clicked cell, if any. Clones-free: the caller owns `colors` for the duration
/// and interprets the returned index after this returns.
fn color_grid(ui: &mut egui::Ui, caption: &str, colors: &[[f32; 4]]) -> Option<GridClick> {
    let mut clicked: Option<GridClick> = None;
    ui.horizontal_wrapped(|ui| {
        if !caption.is_empty() {
            ui.label(
                RichText::new(caption)
                    .size(theme::TEXT_CAPTION)
                    .color(ui.visuals().widgets.inactive.bg_stroke.color),
            );
        }
        let size = 18.0_f32;
        for (i, c) in colors.iter().enumerate() {
            let color = egui::Color32::from_rgba_unmultiplied(
                (c[0].clamp(0.0, 1.0) * 255.0) as u8,
                (c[1].clamp(0.0, 1.0) * 255.0) as u8,
                (c[2].clamp(0.0, 1.0) * 255.0) as u8,
                (c[3].clamp(0.0, 1.0) * 255.0) as u8,
            );
            let (rect, resp) =
                ui.allocate_exact_size(egui::Vec2::splat(size), egui::Sense::click());
            let _ = rect;
            ui.painter()
                .rect_filled(resp.rect, CornerRadius::same(theme::RADIUS_BUTTON), color);
            ui.painter().rect_stroke(
                resp.rect,
                CornerRadius::same(theme::RADIUS_BUTTON),
                ui.visuals().widgets.inactive.bg_stroke,
                egui::StrokeKind::Inside,
            );
            if resp.secondary_clicked() {
                clicked = Some(GridClick {
                    index: i,
                    right_click: true,
                });
            } else if resp.clicked() {
                clicked = Some(GridClick {
                    index: i,
                    right_click: false,
                });
            }
        }
    });
    clicked
}

/// A color-edit swatch with the same border the sidebar buttons have, so a
/// swatch whose color matches the panel background (e.g. black foreground on the
/// OLED Black theme) stays visible. Returns the new color when edited.
fn color_swatch(
    ui: &mut egui::Ui,
    color: ogre_core::Rgba32F,
    hover: &str,
) -> Option<ogre_core::Rgba32F> {
    let mut rgba = [color.r, color.g, color.b, color.a];
    let resp = ui
        .color_edit_button_rgba_unmultiplied(&mut rgba)
        .on_hover_text(hover);
    // Match the button outline (`widgets.inactive.bg_stroke`, the separator
    // color) drawn just outside the swatch so the fill is unobscured.
    ui.painter().rect_stroke(
        resp.rect,
        egui::CornerRadius::same(theme::RADIUS_BUTTON),
        ui.visuals().widgets.inactive.bg_stroke,
        egui::StrokeKind::Outside,
    );
    resp.changed()
        .then(|| ogre_core::Rgba32F::new(rgba[0], rgba[1], rgba[2], rgba[3]))
}

/// Render a section header with a left-aligned label and a right-aligned
/// six-dot grip. The whole row is a drag source.
fn section_header(ui: &mut egui::Ui, state: &AppState, section: SidebarSection) -> egui::Response {
    ui.push_id(section, |ui| {
        let palette = theme::resolve(state.preferences.theme, ui.ctx());
        let (rect, mut response) =
            ui.allocate_at_least(egui::vec2(ui.available_width(), 20.0), egui::Sense::drag());

        if response.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        } else if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }

        response = response.on_hover_text("Drag to reorder");

        let text_color = palette.separator_strong;

        // Section label, vertically centered.
        ui.painter().text(
            egui::pos2(rect.min.x + 4.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            section.label(),
            egui::FontId::proportional(theme::TEXT_CAPTION),
            text_color,
        );

        // Six-dot grip (2 columns × 3 rows), drawn so it cannot render as tofu.
        // Kept small (≈13 px tall) so it fits inside the 20 px header without
        // dominating the section label.
        let grip_rect = egui::Rect::from_min_size(
            egui::pos2(
                rect.max.x - 4.0 - SIX_DOT_GRIP_SIZE.x,
                rect.center().y - SIX_DOT_GRIP_SIZE.y / 2.0,
            ),
            SIX_DOT_GRIP_SIZE,
        );
        paint_six_dot_grip(ui.painter(), grip_rect, text_color);

        response
    })
    .inner
}

/// Compute a new order that moves `source` to the position indicated by the
/// pointer's y coordinate relative to the stable anchor centers.
/// `anchors` must be the source-removed header centers captured at drag start.
fn reorder_at_pointer<T: Copy + PartialEq>(
    order: &[T],
    source: T,
    anchors: &[f32],
    pointer_y: f32,
) -> Option<Vec<T>> {
    let source_idx = order.iter().position(|g| *g == source)?;
    if anchors.len() + 1 != order.len() {
        return None;
    }

    // Count anchors (non-source headers) that lie entirely above the pointer.
    let mut target = anchors.len();
    for (i, &cy) in anchors.iter().enumerate() {
        if pointer_y < cy {
            target = i;
            break;
        }
    }

    if target == source_idx {
        return None;
    }

    let mut new_order = order.to_vec();
    new_order.remove(source_idx);
    new_order.insert(target, source);
    Some(new_order)
}

/// Render one tool-family row (§2.2 flyouts). Click activates the family
/// (cycling forward if already active, else restoring the last-used sibling
/// or the primary). Right-click opens a flyout of siblings for multi-variant
/// families. The displayed icon/name is whichever sibling is currently active
/// for the family, or the last-used/primary when the family is inactive.
fn family_row(ui: &mut egui::Ui, state: &mut AppState, family: ToolFamily) {
    let active = state.tool_manager.active();
    let siblings = family.siblings();
    let display = if active.family() == family {
        active
    } else {
        *state
            .tool_family_last
            .get(&family)
            .unwrap_or(&family.primary())
    };
    let is_active_family = active.family() == family;
    let multi = siblings.len() > 1;
    let palette = crate::theme::resolve(state.preferences.theme, ui.ctx());
    let fill = if is_active_family {
        Some(palette.accent_mute)
    } else {
        None
    };

    let (rect, response) =
        ui.allocate_at_least(egui::vec2(ui.available_width(), 32.0), egui::Sense::click());

    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    if response.clicked() {
        // Equivalent to pressing the family's bare-key shortcut: cycles forward
        // when the family is already active, otherwise activates the last-used
        // sibling (or the primary).
        crate::shell::activate_tool_family(state, family, true);
    }

    if let Some(fill) = fill {
        ui.painter().rect_filled(rect, CornerRadius::same(4), fill);
    }

    ui.scope_builder(egui::UiBuilder::new().max_rect(rect.shrink(8.0)), |ui| {
        ui.horizontal_centered(|ui| {
            // Icon and label are purely visual: `Sense::empty()` keeps them from
            // stealing the row's click/cursor (see the original `tool_row` note).
            ui.add(
                crate::icons::tool_image(display, 20.0, ui.visuals().text_color())
                    .sense(egui::Sense::empty()),
            );
            if !state.preferences.tools_collapsed {
                ui.add(
                    egui::Label::new(display.name())
                        .selectable(false)
                        .sense(egui::Sense::empty()),
                );
                if multi {
                    // Visual hint that this slot has more than one tool. Painted
                    // as a small right-pointing triangle rather than the "▸"
                    // glyph, which the default font lacks (it rendered as a tofu
                    // box).
                    let (tri_rect, _) =
                        ui.allocate_exact_size(egui::vec2(9.0, 14.0), egui::Sense::empty());
                    let c = tri_rect.center();
                    ui.painter().add(egui::Shape::convex_polygon(
                        vec![
                            egui::pos2(c.x - 2.0, c.y - 3.5),
                            egui::pos2(c.x - 2.0, c.y + 3.5),
                            egui::pos2(c.x + 3.0, c.y),
                        ],
                        palette.separator_strong,
                        egui::Stroke::NONE,
                    ));
                }
            }
        });
    });

    // Right-click flyout of siblings (multi-variant families only).
    if multi {
        response.context_menu(|ui| {
            for &sib in siblings {
                let selected = is_active_family && active == sib;
                if ui
                    .add(egui::Button::selectable(selected, sib.name()))
                    .clicked()
                {
                    state.set_tool_and_persist(sib);
                    let doc = state.doc().clone();
                    state.tool_manager.load_active_vector_layer(&doc);
                    ui.close();
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_flips_collapsed() {
        let mut collapsed = false;
        toggle_collapsed(&mut collapsed);
        assert!(collapsed);
        toggle_collapsed(&mut collapsed);
        assert!(!collapsed);
    }

    fn header_rect(y: f32) -> egui::Rect {
        egui::Rect::from_min_max(egui::pos2(0.0, y), egui::pos2(10.0, y + 20.0))
    }

    fn default_order() -> Vec<SidebarSection> {
        crate::tools::default_sidebar_order()
    }

    fn anchors_for<T: Copy + PartialEq>(order: &[T], source: T) -> Vec<f32> {
        let rects: Vec<_> = (0..order.len())
            .map(|i| header_rect(i as f32 * 30.0))
            .collect();
        order
            .iter()
            .zip(rects.iter())
            .filter(|(&g, _)| g != source)
            .map(|(_, r)| r.center().y)
            .collect()
    }

    #[test]
    fn reorder_moves_first_section_to_last() {
        let order = default_order();
        let anchors = anchors_for(&order, SidebarSection::Navigate);
        let new = reorder_at_pointer(&order, SidebarSection::Navigate, &anchors, 250.0).unwrap();
        assert_eq!(new.last(), Some(&SidebarSection::Navigate));
    }

    #[test]
    fn reorder_moves_last_section_to_first() {
        let order = default_order();
        let anchors = anchors_for(&order, SidebarSection::Vector);
        let new = reorder_at_pointer(&order, SidebarSection::Vector, &anchors, -10.0).unwrap();
        assert_eq!(new.first(), Some(&SidebarSection::Vector));
    }

    #[test]
    fn reorder_moves_adjacent_down() {
        let order = default_order();
        let anchors = anchors_for(&order, SidebarSection::Navigate);
        let new = reorder_at_pointer(&order, SidebarSection::Navigate, &anchors, 45.0).unwrap();
        assert_eq!(new[0], SidebarSection::Transform);
        assert_eq!(new[1], SidebarSection::Navigate);
    }

    #[test]
    fn reorder_moves_adjacent_up() {
        let order = default_order();
        let anchors = anchors_for(&order, SidebarSection::Transform);
        let new = reorder_at_pointer(&order, SidebarSection::Transform, &anchors, 5.0).unwrap();
        assert_eq!(new[0], SidebarSection::Transform);
        assert_eq!(new[1], SidebarSection::Navigate);
    }

    #[test]
    fn reorder_noop_when_pointer_stays_on_source() {
        let order = default_order();
        let anchors = anchors_for(&order, SidebarSection::Select);
        assert!(reorder_at_pointer(&order, SidebarSection::Select, &anchors, 50.0).is_none());
    }

    #[test]
    fn reorder_noop_when_order_already_normalized() {
        let order = default_order();
        let anchors = anchors_for(&order, SidebarSection::Vector);
        assert!(reorder_at_pointer(&order, SidebarSection::Vector, &anchors, 250.0).is_none());
    }

    #[test]
    fn reorder_returns_none_for_mismatched_anchors() {
        let order = default_order();
        let anchors = vec![10.0, 40.0]; // wrong length for a 7-section order
        assert!(reorder_at_pointer(&order, SidebarSection::Navigate, &anchors, 100.0).is_none());
    }

    #[test]
    fn reorder_can_move_color_past_swatches() {
        let order = vec![
            SidebarSection::Color,
            SidebarSection::Swatches,
            SidebarSection::Navigate,
        ];
        let anchors = anchors_for(&order, SidebarSection::Color);
        let new = reorder_at_pointer(&order, SidebarSection::Color, &anchors, 100.0).unwrap();
        assert_eq!(new[0], SidebarSection::Swatches);
        assert_eq!(new[1], SidebarSection::Navigate);
        assert_eq!(new[2], SidebarSection::Color);
    }
}
