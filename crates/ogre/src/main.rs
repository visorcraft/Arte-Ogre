// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Arte Ogre — runnable binary entry point.
//!
//! Launches the `eframe` native application with the `wgpu` renderer.
//! The `ogre-gpu` canvas renderer shares the same wgpu device/queue by
//! reading `frame.wgpu_render_state()` lazily in `OgreApp::ui`.

use std::sync::Arc;

use eframe::{NativeOptions, Renderer};
use egui_wgpu::{WgpuConfiguration, WgpuSetup, WgpuSetupCreateNew};

fn main() -> eframe::Result<()> {
    let device_descriptor = Arc::new(|adapter: &wgpu::Adapter| {
        let base_limits = if adapter.get_info().backend == wgpu::Backend::Gl {
            wgpu::Limits::downlevel_webgl2_defaults()
        } else {
            wgpu::Limits::default()
        };

        wgpu::DeviceDescriptor {
            label: Some("egui wgpu device"),
            required_features: wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES,
            required_limits: wgpu::Limits {
                max_texture_dimension_2d: 8192,
                ..base_limits
            },
            ..Default::default()
        }
    });

    let create_new = WgpuSetupCreateNew {
        device_descriptor,
        ..WgpuSetupCreateNew::without_display_handle()
    };

    // Window / taskbar / title-bar icon: the canonical Ogre Face art in
    // `assets/ArteOgre.png` (the source of truth for all icon reproductions).
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../../../assets/ArteOgre.png"))
        .expect("embedded app icon is a valid PNG");

    let mut options = NativeOptions {
        renderer: Renderer::Wgpu,
        // Accept files dragged in from a file manager (e.g. Double Commander):
        // the dropped paths surface in `RawInput::dropped_files` each frame.
        viewport: eframe::egui::ViewportBuilder::default()
            .with_drag_and_drop(true)
            .with_icon(Arc::new(icon))
            // Wayland has no per-window icon protocol: the title-bar / taskbar
            // icon comes from the compositor matching this app_id to an installed
            // `arte-ogre.desktop` (Icon=arte-ogre). `with_icon` above only covers
            // X11. The AppImage install step installs that desktop file + icon.
            .with_app_id("arte-ogre"),
        wgpu_options: WgpuConfiguration {
            wgpu_setup: WgpuSetup::CreateNew(create_new),
            ..Default::default()
        },
        ..Default::default()
    };

    // Default to winit's native backend (Wayland on a Wayland session), which
    // resizes smoothly. winit has no Wayland file drag-and-drop, so dragging
    // files onto the window from a file manager does not work there — set
    // `ARTE_OGRE_X11=1` to force the X11 backend (via XWayland), where drops
    // work (at the cost of a black window while the resize handle is held, a
    // winit/X11 limitation). File → Open / Add as Layer works on both.
    #[cfg(target_os = "linux")]
    if std::env::var_os("ARTE_OGRE_X11").is_some() {
        options.event_loop_builder = Some(Box::new(|builder| {
            use winit::platform::x11::EventLoopBuilderExtX11;
            builder.with_x11();
        }));
    }

    eframe::run_native(
        "Arte Ogre",
        options,
        Box::new(|_cc| Ok(Box::new(ogre_ui::OgreApp::new()))),
    )
}
