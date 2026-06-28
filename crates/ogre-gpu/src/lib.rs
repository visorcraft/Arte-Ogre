// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! GPU-accelerated compositor for Arte Ogre.
//!
//! `ogre-gpu` renders [`ogre_core::Document`]s using `wgpu`. It is designed to
//! operate on a *shared* device/queue (e.g. the one owned by `eframe`)
//! via [`context::GpuContext::from_shared`], but also provides a headless context
//! for tests and CLI usage via [`context::GpuContext::headless`].
//!
//! # Shared-device requirement
//!
//! The public [`canvas_renderer::CanvasRenderer`] type is constructed from a
//! `wgpu::Device`, `wgpu::Queue`, and `wgpu::AdapterInfo` supplied by the caller.
//! This lets the UI pass the device owned by `eframe`, avoiding GPU-device
//! copies. The device must be created with
//! [`wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES`] and must support
//! `Rgba32Float` textures with both `STORAGE_BINDING` and `RENDER_ATTACHMENT`
//! usages; [`context::GpuContext::from_shared`] validates this early.
//!
//! # Golden-test contract
//!
//! Every GPU compositor output is tested against the CPU reference compositor in
//! `ogre_core::compositor::composite_document`. The contract is that every
//! channel must match within `1e-4` absolute error.
//! [`canvas_renderer::CanvasRenderer::read_output`](crate::canvas_renderer::CanvasRenderer::read_output)
//! preserves `Rgba32Float` precision so this contract can be verified through the
//! public API.
//!
//! # Feature flags
//!
//! - `egui` (optional): enables `CanvasRenderer::register_with_egui`, which
//!   registers the renderer's output texture with an `egui_wgpu::Renderer` for
//!   zero-copy display inside an egui/eframe application.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

pub mod blend;
pub mod canvas_renderer;
pub mod compositor;
pub mod context;
pub mod filters;
pub mod tile_cache;
pub mod viewport;

pub use canvas_renderer::CanvasRenderer;
pub use filters::{
    ApplyFilterCmd, BrightnessContrastFilter, CurvesFilter, DesaturateFilter, EdgeDetectFilter,
    EmbossFilter, Filter, FilterPreview, FilterRunner, GaussianBlurFilter, GradientMapFilter,
    HueSaturationFilter, InvertFilter, LevelsFilter, ParamsBuffer, PosterizeFilter,
    ReplaceLayerBufferCmd, SharpenFilter, ThresholdFilter,
};
pub use tile_cache::{CacheBudget, TileTextureCache};
pub use viewport::Viewport;
