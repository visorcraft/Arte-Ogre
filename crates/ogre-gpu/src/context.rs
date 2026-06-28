// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! GPU context creation and shared-device wrapping.

/// An owned or borrowed `wgpu` device/queue pair.
///
/// `GpuContext` is the low-level GPU handle used throughout `ogre-gpu`.
/// [`GpuContext::from_shared`] lets the renderer operate on the same
/// `wgpu::Device`/`wgpu::Queue` owned by `eframe`. Tests and CLI tooling use
/// [`GpuContext::headless`] to create a fresh headless device.
#[derive(Clone, Debug)]
pub struct GpuContext {
    /// The `wgpu` device all GPU resources are allocated on.
    pub device: wgpu::Device,
    /// The `wgpu` queue used to submit command buffers.
    pub queue: wgpu::Queue,
    /// Information about the adapter backing this context.
    pub adapter_info: wgpu::AdapterInfo,
}

/// Compute the `wgpu::Features` to request for an adapter.
///
/// Always requests `TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES` so that
/// `Rgba32Float` can be used with adapter-specific capabilities. Only adds
/// `FLOAT32_FILTERABLE` when the adapter advertises it, matching the fallback
/// rule.
fn required_features(adapter: &wgpu::Adapter) -> wgpu::Features {
    let mut features = wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES;
    if adapter
        .features()
        .contains(wgpu::Features::FLOAT32_FILTERABLE)
    {
        features |= wgpu::Features::FLOAT32_FILTERABLE;
    }
    features
}

/// Panic unless the adapter supports `Rgba32Float` as both a storage and
/// render target.
fn assert_rgba32float_capabilities(adapter: &wgpu::Adapter) {
    let needed = wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT;
    let format_features = adapter.get_texture_format_features(wgpu::TextureFormat::Rgba32Float);
    assert!(
        format_features.allowed_usages.contains(needed),
        "adapter '{}' does not support Rgba32Float with {:?}",
        adapter.get_info().name,
        needed
    );
}

impl GpuContext {
    /// Create a headless GPU context for tests and command-line use.
    ///
    /// This method blocks on async `wgpu` initialization. It selects an
    /// adapter, verifies that `Rgba32Float` supports both `STORAGE_BINDING`
    /// and `RENDER_ATTACHMENT`, and then requests a device with
    /// `TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES` plus `FLOAT32_FILTERABLE`
    /// only if the adapter advertises it.
    ///
    /// When no discrete/integrated GPU is present `wgpu` falls back to a
    /// software adapter (e.g. Vulkan SwiftShader or llvmpipe); the method
    /// does not panic as long as the resulting adapter has a non-empty name
    /// and the required format capabilities.
    pub fn headless() -> Self {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .expect("no suitable GPU adapter found");

        assert_rgba32float_capabilities(&adapter);

        let adapter_info = adapter.get_info();
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("ogre-gpu headless"),
            required_features: required_features(&adapter),
            required_limits: wgpu::Limits::default(),
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            memory_hints: wgpu::MemoryHints::default(),
            trace: wgpu::Trace::Off,
        }))
        .expect("failed to request GPU device");

        Self {
            device,
            queue,
            adapter_info,
        }
    }

    /// Wrap a device/queue owned by an external caller (e.g. `eframe`).
    ///
    /// The caller must ensure that the supplied `device` and `queue` belong
    /// to the same adapter and that they remain alive for the lifetime of the
    /// returned `GpuContext`.
    ///
    /// # Panics
    ///
    /// Panics if the device was not created with
    /// [`wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES`], or if the
    /// device cannot create `Rgba32Float` textures with `STORAGE_BINDING` and
    /// `RENDER_ATTACHMENT` usages. The compositor relies on these capabilities.
    pub fn from_shared(device: wgpu::Device, queue: wgpu::Queue, info: wgpu::AdapterInfo) -> Self {
        assert!(
            device
                .features()
                .contains(wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES),
            "shared device must be created with Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES"
        );

        // Probe the exact format capabilities the compositor needs. This turns
        // a late validation error into an early, explicit panic.
        let _probe = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("shared device Rgba32Float capability probe"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba32Float,
            usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });

        Self {
            device,
            queue,
            adapter_info: info,
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn headless_context_supports_rgba32_float_storage_and_render() {
        let ctx = crate::context::GpuContext::headless();

        assert!(
            !ctx.adapter_info.name.is_empty(),
            "adapter must report a name"
        );

        let features = ctx.device.features();
        assert!(
            features.contains(wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES),
            "TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES must be enabled"
        );

        let _texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("format capability probe"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba32Float,
            usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
    }

    #[test]
    fn required_features_requests_filterable_only_when_available() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .expect("no suitable GPU adapter found");

        let features = super::required_features(&adapter);

        let expected = wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES
            | if adapter
                .features()
                .contains(wgpu::Features::FLOAT32_FILTERABLE)
            {
                wgpu::Features::FLOAT32_FILTERABLE
            } else {
                wgpu::Features::empty()
            };
        assert_eq!(features, expected);
    }

    #[test]
    #[should_panic(expected = "TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES")]
    fn from_shared_rejects_device_without_format_feature() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .expect("no suitable GPU adapter found");

        let info = adapter.get_info();
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("device without adapter-specific format features"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            memory_hints: wgpu::MemoryHints::default(),
            trace: wgpu::Trace::Off,
        }))
        .expect("failed to request GPU device");

        let _ctx = super::GpuContext::from_shared(device, queue, info);
    }
}
